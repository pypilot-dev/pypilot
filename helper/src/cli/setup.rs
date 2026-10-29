//! `pypilot setup` — full F1 bootstrap. Same engine as the LSP "Fix everything".
//! `--dry-run` prints the plan instead (see [`super::plan`]).

use std::path::Path;

use colored::Colorize;

use crate::core::setup;
use crate::pypi::MetadataSource;
use crate::settings::Settings;

use super::output::{self, OutputFormat};
use crate::core::exec::{self, Plan};

/// The `--json` body of every command that runs steps (`setup`, `install`,
/// `fix python`): the [`setup::SetupSummary`] plus what the terminal adds.
#[derive(serde::Serialize)]
pub(crate) struct StepsReport<'a> {
    pub workspace: &'a Path,
    /// `install`: the package asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<&'a str>,
    /// `fix python`: the Python rebuilt on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_python: Option<crate::pypi::PyVersion>,
    pub summary: &'a setup::SetupSummary,
    /// When a failed build needed a system library conda-forge ships.
    pub pixi_advice: Option<String>,
}

impl<'a> StepsReport<'a> {
    pub fn new(workspace: &'a Path, summary: &'a setup::SetupSummary) -> Self {
        StepsReport {
            workspace,
            package: None,
            target_python: None,
            summary,
            pixi_advice: pixi_build_advice(workspace, summary),
        }
    }

    pub fn print(&self, command: &str) -> crate::Result<()> {
        output::print_json(command, self.summary.ok, self)
    }
}

/// `pypilot setup`, or `doctor --fix` under the name `command`.
pub async fn run<S: MetadataSource>(
    workspace: &Path,
    settings: &Settings,
    source: &S,
    format: OutputFormat,
    command: &str,
    dry_run: bool,
) -> crate::Result<()> {
    if dry_run {
        let (summary, steps) = exec::dry_run(setup::run(workspace, settings, source)).await;
        let plan = Plan::from_summary(&summary?, steps);
        return super::plan::finish(command, workspace, &plan, format);
    }
    if format.is_json() {
        let summary = setup::run(workspace, settings, source).await?;
        crate::core::rescan::notify(workspace, crate::core::rescan::Kind::Setup);
        StepsReport::new(workspace, &summary).print(command)?;
        if summary.ok {
            return Ok(());
        }
        anyhow::bail!("setup did not complete — see the steps above");
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
    if let Some(message) = pixi_build_advice(workspace, summary) {
        println!("  {} {message}", "[PIXI ]".cyan().bold());
    }
}

/// The advice [`print_pixi_build_advice`] prints, for `--json`.
pub(crate) fn pixi_build_advice(workspace: &Path, summary: &setup::SetupSummary) -> Option<String> {
    if crate::core::pixi::detect(workspace).is_some() {
        return None;
    }
    let table = crate::matrix::conda_forge();
    let platform = crate::core::platform::Platform::current();
    summary
        .steps
        .iter()
        .filter(|s| !s.ok)
        .find_map(|step| {
            crate::core::pixi_advice::explain_build_failure(&step.detail, table, platform)
        })
        .map(|advice| advice.message)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{PackageManager, ResolvedMode};

    #[test]
    fn steps_report_shape_is_stable() {
        let summary = setup::SetupSummary {
            package_manager: PackageManager::Auto,
            resolved_mode: ResolvedMode::UvPipCompat,
            python: Some(crate::pypi::PyVersion::py3(12)),
            venv_path: ".venv".into(),
            why: "w".into(),
            steps: vec![setup::Step {
                name: "uv venv".into(),
                ok: true,
                detail: ".venv".into(),
            }],
            ok: true,
        };
        let report = StepsReport::new(Path::new("proj"), &summary);
        let text = output::to_json("setup", summary.ok, &report).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "pypilot": env!("CARGO_PKG_VERSION"),
                "command": "setup",
                "ok": true,
                "workspace": "proj",
                "summary": {
                    "package_manager": "auto",
                    "resolved_mode": "uv_pip_compat",
                    "python": "3.12",
                    "venv_path": ".venv",
                    "why": "w",
                    "steps": [{"name": "uv venv", "ok": true, "detail": ".venv"}],
                    "ok": true,
                },
                "pixi_advice": null,
            })
        );
    }
}
