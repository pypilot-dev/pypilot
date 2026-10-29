//! `pypilot logs`: where the local log is, and what it says.
//!
//! * `pypilot logs`             the log file's path
//! * `pypilot logs --tail [N]`  the last N entries (default 10), summarized
//! * `--json`                   the raw JSON lines instead of the summary

use crate::core::logs::{self, Entry};

const DEFAULT_TAIL: usize = 10;
/// Failed steps listed under a failed entry (the last ones; `--json` has all).
const SHOWN_FAILURES: usize = 3;

pub fn run(args: &[String]) -> Result<(), String> {
    let json = args.iter().any(|a| a == "--json");
    let Some(n) = tail_count(args)? else {
        println!("{}", logs::log_path().display());
        return Ok(());
    };
    let entries = logs::tail(&logs::log_dir(), n);
    if json {
        for e in &entries {
            if let Ok(line) = serde_json::to_string(e) {
                println!("{line}");
            }
        }
        return Ok(());
    }
    if entries.is_empty() {
        println!("No log entries yet ({}).", logs::log_path().display());
        return Ok(());
    }
    for e in &entries {
        print!("{}", summarize(e));
    }
    Ok(())
}

/// `None` without `--tail`; otherwise the count, from `--tail N`,
/// `--tail=N`, or the default.
fn tail_count(args: &[String]) -> Result<Option<usize>, String> {
    let mut iter = args.iter().peekable();
    while let Some(a) = iter.next() {
        if a == "--tail" {
            return match iter.peek() {
                Some(n) if !n.starts_with('-') => n
                    .parse()
                    .map(Some)
                    .map_err(|_| format!("--tail takes a number, not `{n}`")),
                _ => Ok(Some(DEFAULT_TAIL)),
            };
        }
        if let Some(n) = a.strip_prefix("--tail=") {
            return n
                .parse()
                .map(Some)
                .map_err(|_| format!("--tail takes a number, not `{n}`"));
        }
    }
    Ok(None)
}

fn seconds(ms: u64) -> String {
    format!("{:.1}s", ms as f64 / 1000.0)
}

/// One entry as a few lines of text: the header, failed steps, the error.
fn summarize(e: &Entry) -> String {
    let ts = e.ts.replacen('T', " ", 1);
    let ts = ts.split('.').next().unwrap_or(&ts);
    let mut out = format!(
        "{ts}Z  {} {:<16} {:<4} {:>7}",
        e.source,
        e.command,
        if e.ok { "ok" } else { "FAIL" },
        seconds(e.duration_ms)
    );
    if let Some(ws) = &e.workspace {
        out.push_str(&format!("  {ws}"));
    }
    match (&e.mode, &e.resolved_mode) {
        (Some(m), Some(r)) => out.push_str(&format!("  [{m} -> {r}]")),
        (Some(m), None) => out.push_str(&format!("  [{m}]")),
        (None, Some(r)) => out.push_str(&format!("  [{r}]")),
        (None, None) => {}
    }
    let failed = e.steps.iter().filter(|s| !s.ok).count();
    let total = e.steps.len() + e.steps_dropped;
    if total > 0 {
        out.push_str(&format!("  {total} steps"));
        if failed > 0 && !e.ok {
            out.push_str(&format!(", {failed} failed"));
        }
    }
    out.push('\n');
    // Probes for interpreters that are not installed fail on every run, so a
    // successful entry lists no steps; a failed one lists its last failures.
    let failures: Vec<_> = e.steps.iter().filter(|s| !s.ok && !e.ok).collect();
    for s in &failures[failures.len().saturating_sub(SHOWN_FAILURES)..] {
        let status = s
            .exit
            .map(|c| format!("exit {c}"))
            .unwrap_or_else(|| "no exit".to_string());
        let mut cmd = s.cmd.clone();
        for a in s.args.iter().take(6) {
            cmd.push(' ');
            cmd.push_str(a);
        }
        if s.args.len() > 6 {
            cmd.push_str(" …");
        }
        out.push_str(&format!(
            "    x {cmd}  ({status}, {})\n",
            seconds(s.duration_ms)
        ));
        if let Some(line) = s
            .stderr
            .as_deref()
            .and_then(|t| t.lines().rev().find(|l| !l.trim().is_empty()))
        {
            out.push_str(&format!("      {}\n", line.trim()));
        }
    }
    if let Some(err) = &e.error {
        out.push_str(&format!(
            "    error: {}\n",
            err.lines().next().unwrap_or_default()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::logs::StepRecord;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn tail_count_forms() {
        assert_eq!(tail_count(&v(&[])).unwrap(), None);
        assert_eq!(tail_count(&v(&["--tail"])).unwrap(), Some(DEFAULT_TAIL));
        assert_eq!(tail_count(&v(&["--tail", "3"])).unwrap(), Some(3));
        assert_eq!(tail_count(&v(&["--tail=4"])).unwrap(), Some(4));
        assert_eq!(
            tail_count(&v(&["--tail", "--json"])).unwrap(),
            Some(DEFAULT_TAIL)
        );
        assert!(tail_count(&v(&["--tail", "x"])).is_err());
    }

    #[test]
    fn summary_shows_failures() {
        let e = Entry {
            ts: "2026-10-29T10:03:12.345Z".into(),
            pypilot: "1.2.2".into(),
            source: "cli".into(),
            command: "setup".into(),
            args: vec![],
            workspace: Some("/proj".into()),
            mode: Some("auto".into()),
            resolved_mode: Some("UvFull".into()),
            ok: false,
            duration_ms: 3200,
            steps: vec![
                StepRecord {
                    cmd: "uv".into(),
                    args: v(&["venv"]),
                    ok: true,
                    exit: Some(0),
                    duration_ms: 400,
                    stderr: None,
                },
                StepRecord {
                    cmd: "uv".into(),
                    args: v(&["sync"]),
                    ok: false,
                    exit: Some(1),
                    duration_ms: 2100,
                    stderr: Some("resolving\nerror: no solution".into()),
                },
            ],
            steps_dropped: 0,
            error: Some("setup did not complete".into()),
        };
        let text = summarize(&e);
        assert!(text.starts_with("2026-10-29 10:03:12Z  cli setup"));
        assert!(text.contains("[auto -> UvFull]  2 steps, 1 failed"));
        assert!(text.contains("x uv sync  (exit 1, 2.1s)"));
        assert!(text.contains("error: no solution"));
        assert!(text.contains("error: setup did not complete"));
    }
}
