//! `--dry-run`: print the plan a mutating command would carry out.
//!
//! The plan itself comes from the engine ([`crate::core::exec`]): the command
//! runs its normal code path with every mutating action recorded instead of
//! executed. This module only prints it, as text or, with `--json`, as
//! `{ "pypilot", "command", "ok", "dry_run": true, "plan": {...} }`.
//!
//! The JSON envelope is built here by [`envelope`] until the shared `--json`
//! output helper lands; switching over means replacing that one function.

use std::path::Path;

use colored::Colorize;
use serde::Serialize;

use crate::core::exec::Plan;

/// How a mutating command was asked to run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunOpts {
    /// `--dry-run`: plan, change nothing.
    pub dry_run: bool,
    /// `--json`: machine-readable output.
    pub json: bool,
}

impl RunOpts {
    pub fn from_args(args: &[String]) -> RunOpts {
        RunOpts {
            dry_run: args.iter().any(|a| a == "--dry-run"),
            json: args.iter().any(|a| a == "--json"),
        }
    }
}

#[derive(Serialize)]
struct Envelope<'a, T: Serialize> {
    pypilot: &'static str,
    command: &'a str,
    ok: bool,
    dry_run: bool,
    plan: &'a T,
}

/// The shared JSON shape around a plan.
fn envelope<'a, T: Serialize>(command: &'a str, ok: bool, plan: &'a T) -> Envelope<'a, T> {
    Envelope {
        pypilot: env!("CARGO_PKG_VERSION"),
        command,
        ok,
        dry_run: true,
        plan,
    }
}

/// Print `plan` for `command` (`setup`, `fix python`, ...) and fail when the
/// real run would not finish, so the exit code matches what it would be.
pub fn finish(command: &str, workspace: &Path, plan: &Plan, opts: RunOpts) -> crate::Result<()> {
    if opts.json {
        let text = serde_json::to_string_pretty(&envelope(command, plan.ok(), plan))?;
        println!("{text}");
    } else {
        print_text(command, workspace, plan);
    }
    match &plan.blocked {
        None => Ok(()),
        Some(why) => anyhow::bail!("the plan stops before it finishes: {why}"),
    }
}

fn print_text(command: &str, workspace: &Path, plan: &Plan) {
    println!(
        "{} — {} ({} mode, {})",
        format!("PyPilot {command} --dry-run").bold().cyan(),
        workspace.display(),
        plan.mode.bold(),
        match &plan.python {
            Some(py) => format!("Python {py}").cyan().bold().to_string(),
            None => "no Python chosen".to_string(),
        }
    );
    if !plan.why.is_empty() {
        println!("  {}", plan.why.cyan());
    }
    for note in &plan.notes {
        let mark = if note.ok {
            "•".cyan().bold()
        } else {
            "✗".red().bold()
        };
        println!("  {mark} {}: {}", note.name.bold(), note.detail);
    }
    println!(
        "{}",
        "── Plan (nothing was changed) ───────────────────────────"
            .bold()
            .cyan()
    );
    if plan.steps.is_empty() {
        println!("  Nothing to do.");
    }
    for (i, step) in plan.steps.iter().enumerate() {
        println!("  {}. {}", i + 1, step.name.bold());
        if !step.command.is_empty() {
            println!("     {} {}", "$".dimmed(), step.command_line());
        }
        if !step.writes.is_empty() {
            let writes: Vec<String> = step.writes.iter().map(|p| relative(workspace, p)).collect();
            println!("     {} {}", "writes:".dimmed(), writes.join(", "));
        }
        println!("     {} {}", "why:".dimmed(), step.why);
    }
    if let Some(why) = &plan.blocked {
        println!("\n  {} {why}", "✗ The run would stop here:".red().bold());
    } else {
        println!(
            "\n  {} run without --dry-run to apply.",
            "Dry run only;".green().bold()
        );
    }
}

/// `path` relative to the workspace when it is inside it.
fn relative(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::exec::PlannedStep;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flags() {
        assert_eq!(
            RunOpts::from_args(&v(&["--dry-run", "--json"])),
            RunOpts {
                dry_run: true,
                json: true
            }
        );
        assert_eq!(RunOpts::from_args(&v(&["numpy"])), RunOpts::default());
    }

    #[test]
    fn json_shape() {
        let plan = Plan {
            mode: "uv".into(),
            python: Some("3.12".into()),
            why: "Python 3.12 chosen".into(),
            steps: vec![PlannedStep {
                name: "uv venv".into(),
                command: v(&["uv", "venv"]),
                writes: vec![".venv".into()],
                why: "env".into(),
            }],
            notes: Vec::new(),
            blocked: None,
        };
        let value = serde_json::to_value(envelope("setup", plan.ok(), &plan)).unwrap();
        assert_eq!(value["command"], "setup");
        assert_eq!(value["ok"], true);
        assert_eq!(value["dry_run"], true);
        assert_eq!(value["pypilot"], env!("CARGO_PKG_VERSION"));
        assert_eq!(value["plan"]["python"], "3.12");
        assert_eq!(value["plan"]["steps"][0]["command"][1], "venv");
        assert_eq!(value["plan"]["steps"][0]["writes"][0], ".venv");
    }
}
