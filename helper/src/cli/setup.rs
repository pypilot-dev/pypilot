//! `pypilot setup` — full F1 bootstrap. Same engine as the LSP "Fix everything".
//! `--dry-run` prints the plan instead (see [`super::plan`]).

use std::path::Path;

use colored::Colorize;

use super::plan::{self, RunOpts};
use crate::core::exec::{self, Plan};
use crate::core::setup;
use crate::pypi::MetadataSource;
use crate::settings::Settings;

/// `pypilot setup`, or `doctor --fix` under the name `command`.
pub async fn run<S: MetadataSource>(
    workspace: &Path,
    settings: &Settings,
    source: &S,
    command: &str,
    opts: RunOpts,
) -> crate::Result<()> {
    if opts.dry_run {
        let (summary, steps) = exec::dry_run(setup::run(workspace, settings, source)).await;
        let plan = Plan::from_summary(&summary?, steps);
        return plan::finish(command, workspace, &plan, opts);
    }
    println!(
        "{} — {} ({})",
        "PyPilot setup".bold().cyan(),
        workspace.display(),
        if crate::core::pixi::detect(workspace).is_some() {
            "pixi project".bold()
        } else {
            format!("{:?} mode", settings.package_manager).bold()
        }
    );

    let summary = setup::run(workspace, settings, source).await?;

    if !summary.why.is_empty() {
        println!("  {}", summary.why.cyan());
    }
    println!(
        "{}",
        "── Steps ────────────────────────────────────────────────"
            .bold()
            .cyan()
    );

    for step in &summary.steps {
        let mark = if step.ok {
            "✓".green().bold()
        } else {
            "✗".red().bold()
        };
        println!(
            "  {mark} {:<24} — {}",
            step.name.bold(),
            truncate(&step.detail, 300)
        );
    }
    println!();

    // If an LSP instance is watching this workspace (e.g. the command was run
    // from a Zed task), have it re-scan and toast the fresh state.
    crate::core::rescan::notify(workspace, crate::core::rescan::Kind::Setup);

    if summary.ok {
        println!(
            "  {} Environment at {} ({}).",
            "✓ Done.".green().bold(),
            summary.venv_path.display(),
            summary
                .python
                .map(|p| format!("Python {p}").cyan().bold().to_string())
                .unwrap_or_else(|| "system interpreter".to_string())
        );
        Ok(())
    } else {
        print_pixi_build_advice(workspace, &summary);
        anyhow::bail!("setup did not complete — see the steps above");
    }
}

/// When a failed step is a build that needed a system library conda-forge
/// ships, say so (see [`crate::core::pixi_advice::explain_build_failure`]).
/// Silent in a pixi project, whose failures pixi_compat explains.
pub(crate) fn print_pixi_build_advice(workspace: &Path, summary: &setup::SetupSummary) {
    if crate::core::pixi::detect(workspace).is_some() {
        return;
    }
    let table = crate::matrix::conda_forge();
    let platform = crate::core::platform::Platform::current();
    for step in summary.steps.iter().filter(|s| !s.ok) {
        if let Some(advice) =
            crate::core::pixi_advice::explain_build_failure(&step.detail, table, platform)
        {
            println!("  {} {}", "[PIXI ]".cyan().bold(), advice.message);
            return;
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}
