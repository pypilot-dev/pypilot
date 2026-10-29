//! Structured local log: one JSON line per CLI command or LSP action.
//!
//! Lives at `<cache>/logs/pypilot.log` (see [`log_dir`]) and rotates at
//! [`MAX_BYTES`], keeping [`KEEP_FILES`] files in total. It exists so a user
//! (or a maintainer helping one) can see what PyPilot actually ran: which
//! command, on which workspace, in which mode, every subprocess with its exit
//! code and duration, and the stderr of the ones that failed.
//!
//! Nothing here leaves the machine. Arguments and stderr pass through
//! [`redact`] (credentials in URLs, `TOKEN=...` pairs, well-known token
//! prefixes), and the environment is never recorded.
//!
//! Every function is best-effort: a log that cannot be written is skipped,
//! never reported, so logging can not break a command. Recording is scoped
//! with a tokio task-local ([`scope`]), which keeps concurrent LSP actions
//! apart and lets [`record_step`] run from any async context without locks.

use std::cell::RefCell;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::core::platform;

/// Rotate once the live file reaches this size.
pub const MAX_BYTES: u64 = 1024 * 1024;
/// Files kept in total: `pypilot.log`, `pypilot.log.1`, `pypilot.log.2`.
pub const KEEP_FILES: usize = 3;
/// Failing stderr is cut to its last this-many characters (errors come last).
const STDERR_MAX: usize = 2000;
/// An entry keeps at most this many steps; the rest are only counted.
const STEPS_MAX: usize = 100;
const FILE_NAME: &str = "pypilot.log";

/// Where the log lives. `PYPILOT_LOG_DIR` overrides it (tests, debugging).
pub fn log_dir() -> PathBuf {
    // Unit tests drive the CLI and LSP in-process; keep them out of the
    // user's real log.
    if cfg!(test) {
        return std::env::temp_dir().join("pypilot-test-logs");
    }
    match std::env::var_os("PYPILOT_LOG_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => platform::cache_dir().join("logs"),
    }
}

/// The live log file.
pub fn log_path() -> PathBuf {
    log_dir().join(FILE_NAME)
}

/// `PYPILOT_NO_LOG=1` turns logging off entirely.
fn disabled() -> bool {
    std::env::var_os("PYPILOT_NO_LOG").is_some_and(|v| !v.is_empty() && v != "0")
}

/// One subprocess PyPilot ran during an action.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StepRecord {
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub ok: bool,
    /// Exit status; `None` when killed by a signal, never spawned or timed out.
    #[serde(default)]
    pub exit: Option<i32>,
    pub duration_ms: u64,
    /// Tail of stderr (or the spawn error), only for failed steps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
}

/// One log line.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub ts: String,
    pub pypilot: String,
    /// `cli` or `lsp`.
    pub source: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    /// The `package_manager` setting.
    #[serde(default)]
    pub mode: Option<String>,
    /// What that setting resolved to for this workspace, when it was needed.
    #[serde(default)]
    pub resolved_mode: Option<String>,
    pub ok: bool,
    pub duration_ms: u64,
    #[serde(default)]
    pub steps: Vec<StepRecord>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub steps_dropped: usize,
    #[serde(default)]
    pub error: Option<String>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// An action being recorded. Build one, then run the work inside [`scope`].
#[derive(Debug)]
pub struct Action {
    entry: Entry,
    started: Instant,
}

impl Action {
    pub fn new(source: &str, command: &str, args: &[String]) -> Action {
        Action {
            entry: Entry {
                ts: rfc3339(SystemTime::now()),
                pypilot: env!("CARGO_PKG_VERSION").to_string(),
                source: source.to_string(),
                command: command.to_string(),
                args: redact_args(args),
                workspace: None,
                mode: None,
                resolved_mode: None,
                ok: true,
                duration_ms: 0,
                steps: Vec::new(),
                steps_dropped: 0,
                error: None,
            },
            started: Instant::now(),
        }
    }

    pub fn workspace(mut self, workspace: Option<&Path>) -> Action {
        self.entry.workspace = workspace.map(display_path);
        self
    }

    fn finish(mut self, ok: bool, error: Option<String>) -> Entry {
        self.entry.ok = ok;
        self.entry.error = error.map(|e| truncate_tail(&redact(&e), STDERR_MAX));
        self.entry.duration_ms = millis(self.started.elapsed());
        self.entry
    }
}

tokio::task_local! {
    static CURRENT: RefCell<Option<Action>>;
}

/// Run `fut` as `action`, then append its entry to the log. `outcome` maps the
/// result to `(ok, error)`. The result is returned untouched.
pub async fn scope<F, T>(
    action: Action,
    fut: F,
    outcome: impl FnOnce(&T) -> (bool, Option<String>),
) -> T
where
    F: Future<Output = T>,
{
    scope_in(&log_dir(), action, fut, outcome).await
}

async fn scope_in<F, T>(
    dir: &Path,
    action: Action,
    fut: F,
    outcome: impl FnOnce(&T) -> (bool, Option<String>),
) -> T
where
    F: Future<Output = T>,
{
    let (result, action) = CURRENT
        .scope(RefCell::new(Some(action)), async {
            let result = fut.await;
            let action = CURRENT.with(|c| c.borrow_mut().take());
            (result, action)
        })
        .await;
    if let Some(action) = action {
        let (ok, error) = outcome(&result);
        append(dir, &action.finish(ok, error));
    }
    result
}

fn with_current(f: impl FnOnce(&mut Entry)) {
    let _ = CURRENT.try_with(|c| {
        if let Ok(mut slot) = c.try_borrow_mut() {
            if let Some(action) = slot.as_mut() {
                f(&mut action.entry);
            }
        }
    });
}

/// Record the workspace and `package_manager` setting of the current action.
pub fn note_context(workspace: &Path, mode: &str) {
    with_current(|e| {
        e.workspace = Some(display_path(workspace));
        e.mode = Some(mode.to_string());
    });
}

/// Record what the mode resolved to.
pub fn note_resolved_mode(mode: &str) {
    with_current(|e| e.resolved_mode = Some(mode.to_string()));
}

/// Record one subprocess. `result` is the exit status and stderr, or the
/// reason it never produced one (spawn failure, timeout).
pub fn record_step(
    program: &str,
    args: &[String],
    result: Result<(Option<i32>, &str), &str>,
    elapsed: Duration,
) {
    with_current(|e| {
        if e.steps.len() >= STEPS_MAX {
            e.steps_dropped += 1;
            return;
        }
        let (ok, exit, stderr) = match result {
            Ok((status, stderr)) => (status == Some(0), status, stderr),
            Err(reason) => (false, None, reason),
        };
        e.steps.push(StepRecord {
            cmd: redact(program),
            args: redact_args(args),
            ok,
            exit,
            duration_ms: millis(elapsed),
            stderr: (!ok)
                .then(|| truncate_tail(&redact(stderr.trim()), STDERR_MAX))
                .filter(|s| !s.is_empty()),
        });
    });
}

/// Absolute form of `path` (so `--path .` still means something later),
/// without the `\\?\` prefix Windows canonicalization adds.
fn display_path(path: &Path) -> String {
    let full = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = full.display().to_string();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => rest.to_string(),
        _ => text,
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

// --- Writing and rotation ----------------------------------------------------

/// Append one entry to the log in `dir`, rotating first if it is full.
/// Silent on every failure.
pub fn append(dir: &Path, entry: &Entry) {
    if disabled() {
        return;
    }
    let Ok(mut line) = serde_json::to_string(entry) else {
        return;
    };
    line.push('\n');
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let path = dir.join(FILE_NAME);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() >= MAX_BYTES) {
        rotate(dir);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        // One write per line, so concurrent writers (the LSP and a CLI task)
        // append whole lines.
        let _ = file.write_all(line.as_bytes());
    }
}

/// `pypilot.log` -> `.1` -> `.2`; the oldest drops off.
fn rotate(dir: &Path) {
    let name = |i: usize| {
        if i == 0 {
            dir.join(FILE_NAME)
        } else {
            dir.join(format!("{FILE_NAME}.{i}"))
        }
    };
    let _ = std::fs::remove_file(name(KEEP_FILES - 1));
    for i in (0..KEEP_FILES - 1).rev() {
        let _ = std::fs::rename(name(i), name(i + 1));
    }
}

/// The last `n` entries in `dir`, oldest first, reading into the rotated
/// files when the live one has fewer. Lines that do not parse are skipped.
pub fn tail(dir: &Path, n: usize) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    for i in 0..KEEP_FILES {
        if entries.len() >= n {
            break;
        }
        let path = if i == 0 {
            dir.join(FILE_NAME)
        } else {
            dir.join(format!("{FILE_NAME}.{i}"))
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut older: Vec<Entry> = text
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        older.append(&mut entries);
        entries = older;
    }
    let skip = entries.len().saturating_sub(n);
    entries.split_off(skip)
}

// --- Redaction ---------------------------------------------------------------

fn redact_args(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut hide_next = false;
    for a in args {
        if hide_next {
            out.push("***".to_string());
            hide_next = false;
            continue;
        }
        let lower = a.to_ascii_lowercase();
        if lower.starts_with("--") && !lower.contains('=') && is_secret_name(&lower) {
            hide_next = true;
        }
        out.push(redact(a));
    }
    out
}

fn is_secret_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    [
        "token",
        "secret",
        "password",
        "passwd",
        "auth",
        "credential",
        "api-key",
        "api_key",
        "apikey",
    ]
    .iter()
    .any(|k| n.contains(k))
}

/// Mask credentials in free text: `scheme://user:pass@host`, `NAME=value`
/// where NAME looks secret, and well-known token formats.
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for piece in text.split_inclusive(char::is_whitespace) {
        let word = piece.trim_end_matches(char::is_whitespace);
        let space = &piece[word.len()..];
        out.push_str(&redact_word(word));
        out.push_str(space);
    }
    out
}

fn redact_word(word: &str) -> String {
    // URL userinfo.
    if let Some(scheme_end) = word.find("://") {
        let rest = &word[scheme_end + 3..];
        let host_end = rest.find('/').unwrap_or(rest.len());
        if let Some(at) = rest[..host_end].rfind('@') {
            return format!("{}://***@{}", &word[..scheme_end], &rest[at + 1..]);
        }
    }
    // NAME=value with a secret-looking NAME (also `--token=value`).
    if let Some(eq) = word.find('=') {
        let name = &word[..eq];
        if is_secret_name(name) && eq + 1 < word.len() {
            return format!("{name}=***");
        }
    }
    // Token formats: GitHub, PyPI, GitLab, Slack, OpenAI-style keys.
    const PREFIXES: &[&str] = &[
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "github_pat_",
        "pypi-",
        "glpat-",
        "xoxb-",
        "xoxp-",
        "sk-",
    ];
    let bare = word.trim_matches(|c: char| c == '"' || c == '\'');
    for p in PREFIXES {
        if let Some(rest) = bare.strip_prefix(p) {
            let tokenish = rest.len() >= 20
                && rest
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            if tokenish {
                return "***".to_string();
            }
        }
    }
    word.to_string()
}

/// The last `max` characters of `s`, prefixed with `…` when cut.
fn truncate_tail(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let tail: String = s.chars().skip(count - max).collect();
    format!("…{tail}")
}

// --- Time --------------------------------------------------------------------

/// `2026-10-29T10:03:12.345Z` (UTC), without a date crate.
pub fn rfc3339(t: SystemTime) -> String {
    let d = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    let secs = d.as_secs();
    let (y, m, day) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        d.subsec_millis()
    )
}

/// Days since 1970-01-01 to (year, month, day); Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pypilot-logs-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn entry(command: &str) -> Entry {
        Action::new("cli", command, &[]).finish(true, None)
    }

    #[test]
    fn timestamps_are_utc_rfc3339() {
        assert_eq!(rfc3339(SystemTime::UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        let t = SystemTime::UNIX_EPOCH + Duration::from_millis(1_793_268_192_345);
        assert_eq!(rfc3339(t), "2026-10-29T10:03:12.345Z");
        let leap = SystemTime::UNIX_EPOCH + Duration::from_secs(951_782_400);
        assert_eq!(rfc3339(leap), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn append_then_tail_round_trips() {
        let dir = scratch("roundtrip");
        for c in ["doctor", "setup", "check"] {
            append(&dir, &entry(c));
        }
        let got: Vec<String> = tail(&dir, 2).into_iter().map(|e| e.command).collect();
        assert_eq!(got, ["setup", "check"]);
        assert_eq!(tail(&dir, 10).len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotation_keeps_three_files_and_tail_reads_across_them() {
        let dir = scratch("rotate");
        std::fs::create_dir_all(&dir).unwrap();
        // A full live file forces a rotation on the next append.
        let big = serde_json::to_string(&entry("old")).unwrap() + "\n";
        let full = big.repeat((MAX_BYTES as usize / big.len()) + 1);
        for _ in 0..4 {
            std::fs::write(dir.join(FILE_NAME), &full).unwrap();
            append(&dir, &entry("new"));
        }
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(files.len(), KEEP_FILES);
        assert!(!dir.join(format!("{FILE_NAME}.3")).exists());
        // The live file holds only the newest entry; tail reaches back.
        let last = tail(&dir, 3);
        assert_eq!(last.len(), 3);
        assert_eq!(last[2].command, "new");
        assert_eq!(last[1].command, "old");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unwritable_dir_is_silent() {
        // A file where the directory should be: create_dir_all fails.
        let dir = scratch("blocked");
        std::fs::write(&dir, "x").unwrap();
        append(&dir, &entry("doctor"));
        assert!(tail(&dir, 5).is_empty());
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn redaction_masks_credentials() {
        assert_eq!(
            redact("https://user:hunter2@pypi.example.com/simple"),
            "https://***@pypi.example.com/simple"
        );
        assert_eq!(redact("UV_INDEX_TOKEN=abc123 ok"), "UV_INDEX_TOKEN=*** ok");
        assert_eq!(
            redact("using ghp_abcdefghijklmnopqrstuvwxyz0123"),
            "using ***"
        );
        // Ordinary words and package names are left alone.
        assert_eq!(
            redact("uv pip install pypi-simple torch==2.5"),
            "uv pip install pypi-simple torch==2.5"
        );
        assert_eq!(
            redact_args(&[
                "--token".into(),
                "s3cret".into(),
                "--path".into(),
                "x".into()
            ]),
            ["--token", "***", "--path", "x"]
        );
    }

    #[tokio::test]
    async fn scope_collects_context_and_steps() {
        let dir = scratch("scope");
        let action = Action::new("cli", "setup", &["--path".into(), "/p".into()]);
        let out = scope_in(
            &dir,
            action,
            async {
                note_context(Path::new("/p"), "auto");
                note_resolved_mode("UvFull");
                record_step(
                    "uv",
                    &["sync".into()],
                    Ok((Some(0), "")),
                    Duration::from_millis(5),
                );
                record_step(
                    "uv",
                    &["pip".into()],
                    Ok((Some(2), "boom\n")),
                    Duration::from_millis(7),
                );
                record_step("nope", &[], Err("failed to spawn"), Duration::ZERO);
                42
            },
            |_| (false, Some("setup did not complete".into())),
        )
        .await;
        assert_eq!(out, 42);
        let e = tail(&dir, 1).pop().expect("one entry");
        assert_eq!(e.command, "setup");
        assert_eq!(e.mode.as_deref(), Some("auto"));
        assert_eq!(e.resolved_mode.as_deref(), Some("UvFull"));
        assert!(!e.ok);
        assert_eq!(e.steps.len(), 3);
        assert!(e.steps[0].ok && e.steps[0].stderr.is_none());
        assert_eq!(e.steps[1].stderr.as_deref(), Some("boom"));
        assert_eq!(e.steps[2].exit, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recording_outside_a_scope_is_a_noop() {
        record_step("uv", &[], Ok((Some(0), "")), Duration::ZERO);
        note_resolved_mode("PurePip");
    }
}
