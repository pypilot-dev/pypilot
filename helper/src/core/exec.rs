//! The one door every mutating action goes through, so `--dry-run` can watch it.
//!
//! Setup, install, the rebuild and the CUDA re-pin all change the project by
//! running uv, pip or pixi, writing a file, or removing a broken venv. Each of
//! those goes through [`run`], [`write`] or [`remove_dir_all`] here with an
//! [`Effect`]: a step name, the files it writes and why it runs.
//!
//! Normally they simply do the work. Inside [`dry_run`] they record a
//! [`PlannedStep`] instead and pretend the work succeeded, so the very same
//! code path that would execute produces the plan, and the plan cannot drift
//! from what a real run does. Read-only probes (`uv --version`, interpreter
//! discovery, PyPI metadata) keep using [`crate::core::command`] directly and
//! still run, which is what lets the plan name the Python the solver picks.
//!
//! What a planned step would create is remembered for the rest of the dry run
//! ([`exists`], [`is_fresh`]), so a later decision like "now that `uv init`
//! made a pyproject.toml, use `uv add -r`" takes the same branch it would take
//! for real.

use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::future::Future;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::core::command::{self, Output};

/// What a mutating action is, for the plan.
#[derive(Debug, Clone, Default)]
pub struct Effect {
    pub name: String,
    pub writes: Vec<PathBuf>,
    pub why: String,
}

impl Effect {
    pub fn new(name: impl Into<String>, why: impl Into<String>) -> Effect {
        Effect {
            name: name.into(),
            writes: Vec::new(),
            why: why.into(),
        }
    }

    /// Add a file or directory this action creates or changes.
    pub fn writes(mut self, path: impl Into<PathBuf>) -> Effect {
        self.writes.push(path.into());
        self
    }
}

/// One step a dry run would have executed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedStep {
    pub name: String,
    /// The exact argument vector, program first. Empty for a step that is not
    /// a process (writing a file, downloading uv, removing a directory).
    pub command: Vec<String>,
    /// Files and directories it would create or change.
    pub writes: Vec<PathBuf>,
    pub why: String,
}

impl PlannedStep {
    /// The command as one line, double-quoting arguments a shell would split
    /// or redirect (`ipykernel>=6.31.0,<7`, a path with spaces), so it can be
    /// pasted into bash or PowerShell. PyPilot itself never runs a shell.
    pub fn command_line(&self) -> String {
        const SPECIAL: &[char] = &[
            '<', '>', '|', '&', ';', '\'', '"', '$', '`', '*', '?', '(', ')', '[', ']', '{', '}',
            '#', '~', '!',
        ];
        self.command
            .iter()
            .map(|a| {
                if a.is_empty() || a.contains(char::is_whitespace) || a.contains(SPECIAL) {
                    format!("\"{a}\"")
                } else {
                    a.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// A finding the flow made on the way that is not a step of its own: which
/// uv it found, which interpreter it picked, a GPU/CUDA decision, or the
/// reason it would stop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanNote {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

/// What a dry run of setup, install or a rebuild would do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Plan {
    /// `uv`, `uv-pip`, `pip` or `pixi`.
    pub mode: String,
    /// The Python the environment would run on, when the flow decides one.
    pub python: Option<String>,
    /// Why that Python (the solver's wording), or what the flow is for.
    pub why: String,
    pub steps: Vec<PlannedStep>,
    pub notes: Vec<PlanNote>,
    /// Set when the real run would stop before finishing, with the reason.
    pub blocked: Option<String>,
}

/// Summary steps that report a decision rather than echo a planned command,
/// and so belong in [`Plan::notes`]. Failed steps always do.
const NOTE_STEPS: &[&str] = &[
    "ensure uv",
    "select interpreter",
    "GPU/CUDA check",
    "GPU/CUDA index",
];

impl Plan {
    /// The plan for a flow that ran under [`dry_run`] and returned `summary`.
    pub fn from_summary(
        summary: &crate::core::setup::SetupSummary,
        steps: Vec<PlannedStep>,
    ) -> Plan {
        let notes = summary
            .steps
            .iter()
            .filter(|s| !s.ok || NOTE_STEPS.contains(&s.name.as_str()))
            .map(|s| PlanNote {
                name: s.name.clone(),
                ok: s.ok,
                detail: s.detail.clone(),
            })
            .collect();
        let blocked = if summary.ok {
            None
        } else {
            summary
                .steps
                .iter()
                .rev()
                .find(|s| !s.ok)
                .map(|s| format!("{}: {}", s.name, s.detail))
                .or_else(|| Some("the run would not finish".to_string()))
        };
        Plan {
            mode: mode_name(summary.resolved_mode).to_string(),
            python: summary.python.map(|p| p.to_string()),
            why: summary.why.clone(),
            steps,
            notes,
            blocked,
        }
    }

    /// Whether the real run is expected to finish.
    pub fn ok(&self) -> bool {
        self.blocked.is_none()
    }
}

/// The name a [`ResolvedMode`](crate::settings::ResolvedMode) goes by in
/// settings and output.
pub fn mode_name(mode: crate::settings::ResolvedMode) -> &'static str {
    use crate::settings::ResolvedMode;
    match mode {
        ResolvedMode::UvFull => "uv",
        ResolvedMode::UvPipCompat => "uv-pip",
        ResolvedMode::PurePip => "pip",
        ResolvedMode::Pixi => "pixi",
    }
}

#[derive(Default)]
struct Recorder {
    steps: Vec<PlannedStep>,
    created: HashSet<PathBuf>,
    removed: HashSet<PathBuf>,
}

tokio::task_local! {
    static RECORDER: RefCell<Recorder>;
}

/// Run `fut` as a dry run: every mutating action inside it is recorded, not
/// executed. Returns its output and the steps it would have run, in order.
///
/// Scoped to the current task, so tests and the LSP running real setups in
/// parallel are unaffected. Code under a dry run must not hand mutating work
/// to a `tokio::spawn`ed task, which would run outside the recorder.
pub async fn dry_run<F: Future>(fut: F) -> (F::Output, Vec<PlannedStep>) {
    RECORDER
        .scope(RefCell::new(Recorder::default()), async move {
            let out = fut.await;
            let steps = RECORDER.with(|r| std::mem::take(&mut r.borrow_mut().steps));
            (out, steps)
        })
        .await
}

/// Whether the current task is inside [`dry_run`].
pub fn is_dry_run() -> bool {
    RECORDER.try_with(|_| ()).is_ok()
}

/// Record `step` when dry-running. Returns whether it was recorded.
fn record(step: PlannedStep) -> bool {
    RECORDER
        .try_with(|r| {
            let mut r = r.borrow_mut();
            for path in &step.writes {
                r.removed.remove(path);
                r.created.insert(path.clone());
            }
            r.steps.push(step);
        })
        .is_ok()
}

/// What a recorded command returns: success, no output.
fn planned_output() -> Output {
    Output {
        status: Some(0),
        stdout: String::new(),
        stderr: String::new(),
    }
}

/// Run a mutating command, or record it in a dry run.
pub async fn run<S, A>(
    program: S,
    args: &[A],
    cwd: Option<&Path>,
    effect: Effect,
) -> crate::Result<Output>
where
    S: AsRef<OsStr>,
    A: AsRef<OsStr>,
{
    if is_dry_run() {
        let mut command = vec![program.as_ref().to_string_lossy().into_owned()];
        command.extend(
            args.iter()
                .map(|a| a.as_ref().to_string_lossy().into_owned()),
        );
        record(PlannedStep {
            name: effect.name,
            command,
            writes: effect.writes,
            why: effect.why,
        });
        return Ok(planned_output());
    }
    command::run(program, args, cwd).await
}

/// Write `contents` to `path`, or record it in a dry run. `path` is added to
/// the effect's writes.
pub fn write(path: &Path, contents: impl AsRef<[u8]>, effect: Effect) -> std::io::Result<()> {
    let effect = effect.writes(path);
    if note(effect) {
        return Ok(());
    }
    std::fs::write(path, contents)
}

/// [`write`] that fails when `path` already exists.
pub fn write_new(path: &Path, contents: impl AsRef<[u8]>, effect: Effect) -> std::io::Result<()> {
    use std::io::Write;
    if is_dry_run() {
        if exists(path) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("{} already exists", path.display()),
            ));
        }
        note(effect.writes(path));
        return Ok(());
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    f.write_all(contents.as_ref())
}

/// Remove the directory `path` and everything in it, or record it.
pub fn remove_dir_all(path: &Path, effect: Effect) -> std::io::Result<()> {
    if is_dry_run() {
        let mut effect = effect;
        effect.why = format!("{} (deletes {})", effect.why, path.display());
        let step = PlannedStep {
            name: effect.name,
            command: Vec::new(),
            writes: vec![path.to_path_buf()],
            why: effect.why,
        };
        let _ = RECORDER.try_with(|r| {
            let mut r = r.borrow_mut();
            r.created.remove(path);
            r.removed.insert(path.to_path_buf());
            r.steps.push(step);
        });
        return Ok(());
    }
    std::fs::remove_dir_all(path)
}

/// Record a step that is not a process (a download, a check that only makes
/// sense once the environment exists). Returns whether it was recorded, i.e.
/// whether the caller is dry-running and must skip the real work.
pub fn note(effect: Effect) -> bool {
    record(PlannedStep {
        name: effect.name,
        command: Vec::new(),
        writes: effect.writes,
        why: effect.why,
    })
}

/// Whether `path` exists, counting what the dry run so far has planned to
/// create or remove.
pub fn exists(path: &Path) -> bool {
    RECORDER
        .try_with(|r| {
            let r = r.borrow();
            if r.removed.contains(path) {
                Some(false)
            } else if r.created.contains(path) {
                Some(true)
            } else {
                None
            }
        })
        .ok()
        .flatten()
        .unwrap_or_else(|| path.exists())
}

/// Whether the dry run so far plans to create or replace `path`, so its
/// current contents on disk say nothing about what it would hold.
pub fn is_fresh(path: &Path) -> bool {
    RECORDER
        .try_with(|r| r.borrow().created.contains(path))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_dry_run_records_and_touches_nothing() {
        let dir = std::env::temp_dir().join(format!("pypilot-exec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("out.txt");

        let ((), steps) = dry_run(async {
            assert!(is_dry_run());
            let out = run(
                "definitely-not-a-real-program",
                &["--flag", "two words", "ipykernel>=6.31.0,<7"],
                Some(&dir),
                Effect::new("fake", "because").writes(dir.join("made")),
            )
            .await
            .unwrap();
            assert!(out.success());
            assert!(exists(&dir.join("made")));
            assert!(is_fresh(&dir.join("made")));
            write(&file, "x", Effect::new("write", "w")).unwrap();
            remove_dir_all(&dir.join("made"), Effect::new("rm", "r")).unwrap();
            assert!(!exists(&dir.join("made")));
        })
        .await;

        assert!(!is_dry_run());
        assert!(!file.exists() && !dir.join("made").exists());
        assert_eq!(steps.len(), 3);
        assert_eq!(
            steps[0].command_line(),
            "definitely-not-a-real-program --flag \"two words\" \"ipykernel>=6.31.0,<7\""
        );
        assert_eq!(steps[1].writes, vec![file.clone()]);
        assert!(steps[2].command.is_empty());

        // Outside a dry run the same calls do the work.
        write(&file, "x", Effect::new("write", "w")).unwrap();
        assert!(file.is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
#[path = "exec_tests.rs"]
mod flow_tests;
