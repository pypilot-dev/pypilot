//! `--dry-run`: print the plan a mutating command would carry out.
//!
//! The plan itself comes from the engine ([`crate::core::exec`]): the command
//! runs its normal code path with every mutating action recorded instead of
//! executed. This module only prints it, as text or, with `--json`, as the
//! shared document ([`super::output`]) with `"dry_run": true` and the plan:
//! `{ "pypilot", "command", "ok", "dry_run": true, "plan": {...} }`.

use std::path::Path;

use colored::Colorize;
use serde::Serialize;

use super::output::{self, OutputFormat};
use crate::core::exec::{self, Plan};

/// `--dry-run`: plan, change nothing.
pub fn dry_run(args: &[String]) -> bool {
    args.iter().any(|a| a == "--dry-run")
}

/// The `--json` body of a dry run.
#[derive(Serialize)]
struct DryRunReport<'a> {
    dry_run: bool,
    plan: &'a Plan,
}

/// Print `plan` for `command` (`setup`, `fix python`, ...) and fail when the
/// real run would not finish, so the exit code matches what it would be.
pub fn finish(
    command: &str,
    workspace: &Path,
    plan: &Plan,
    format: OutputFormat,
) -> crate::Result<()> {
    if format.is_json() {
        output::print_json(
            command,
            plan.ok(),
            &DryRunReport {
                dry_run: true,
                plan,
            },
        )?;
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
        exec::mode_name(plan.mode).bold(),
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
    use crate::pypi::PyVersion;
    use crate::settings::ResolvedMode;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flag() {
        assert!(dry_run(&v(&["numpy", "--dry-run", "--json"])));
        assert!(!dry_run(&v(&["numpy"])));
    }

    #[test]
    fn json_shape() {
        let plan = Plan {
            mode: ResolvedMode::UvFull,
            python: Some(PyVersion::py3(12)),
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
        let report = DryRunReport {
            dry_run: true,
            plan: &plan,
        };
        let text = output::to_json("setup", plan.ok(), &report).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "pypilot": env!("CARGO_PKG_VERSION"),
                "command": "setup",
                "ok": true,
                "dry_run": true,
                "plan": {
                    "mode": "uv_full",
                    "python": "3.12",
                    "why": "Python 3.12 chosen",
                    "steps": [{
                        "name": "uv venv",
                        "command": ["uv", "venv"],
                        "writes": [".venv"],
                        "why": "env",
                    }],
                    "notes": [],
                    "blocked": null,
                },
            })
        );
    }
}
