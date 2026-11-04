//! `pypilot fix requirements`: apply the requirement fixes doctor proposes.
//!
//! Each fix edits one requirement in the manifest (a pin uv cannot
//! satisfy, a typo'd name, a missing environment marker) or adds imports the
//! code uses but the manifest never declares. They run through
//! [`req_edit::apply`], the function the doctor `--fix` planner calls too.
//! The environment itself is left alone: run `pypilot setup` afterwards.

use std::path::Path;

use colored::Colorize;

use crate::core::exec::{self, Plan};
use crate::core::setup::Step;
use crate::core::{req_edit, solver, undeclared, Finding};
use crate::pypi::MetadataSource;
use crate::settings::Settings;

use super::output::{self, OutputFormat};

/// The findings whose fix [`req_edit::apply`] runs, from a fresh assessment.
pub async fn requirement_fixes<S: MetadataSource>(
    workspace: &Path,
    settings: &Settings,
    source: &S,
) -> Vec<Finding> {
    let a = solver::assess(workspace, settings, source).await;
    let mut fixes: Vec<Finding> = a
        .findings
        .iter()
        .filter(|f| req_edit::is_requirement_fix(&f.fix))
        .cloned()
        .collect();
    if !a.project.is_pixi() {
        let venv = a.probes.venv.as_ref().map(|v| (v.path.as_path(), v.python));
        let missing = undeclared::find(workspace, &a.project, venv, source).await;
        fixes.extend(undeclared::finding(&missing));
    }
    fixes
}

async fn apply_all(workspace: &Path, settings: &Settings, fixes: &[Finding]) -> Vec<Step> {
    let mut steps = Vec::new();
    for f in fixes {
        match req_edit::apply(workspace, settings, &f.fix).await {
            Ok(s) => steps.extend(s),
            Err(e) => steps.push(Step {
                name: f.title.clone(),
                ok: false,
                detail: format!("{e:#}"),
            }),
        }
    }
    steps
}

pub async fn run<S: MetadataSource>(
    workspace: &Path,
    settings: &Settings,
    source: &S,
    format: OutputFormat,
    dry_run: bool,
) -> crate::Result<()> {
    let fixes = requirement_fixes(workspace, settings, source).await;
    if dry_run {
        let (_, planned) = exec::dry_run(apply_all(workspace, settings, &fixes)).await;
        let plan = Plan {
            mode: settings.resolve_mode(workspace).await,
            python: None,
            why: if fixes.is_empty() {
                "No requirement needs changing.".into()
            } else {
                fixes
                    .iter()
                    .map(|f| f.title.clone())
                    .collect::<Vec<_>>()
                    .join("; ")
            },
            steps: planned,
            notes: Vec::new(),
            blocked: None,
        };
        return super::plan::finish("fix requirements", workspace, &plan, format);
    }

    let steps = apply_all(workspace, settings, &fixes).await;
    let ok = steps.iter().all(|s| s.ok);
    if format.is_json() {
        #[derive(serde::Serialize)]
        struct Report<'a> {
            workspace: &'a Path,
            findings: &'a [Finding],
            steps: &'a [Step],
        }
        output::print_json(
            "fix requirements",
            ok,
            &Report {
                workspace,
                findings: &fixes,
                steps: &steps,
            },
        )?;
    } else if fixes.is_empty() {
        println!(
            "{} no requirement needs changing.",
            "PyPilot fix requirements:".bold().cyan()
        );
    } else {
        println!("{}", "PyPilot fix requirements:".bold().cyan());
        for f in &fixes {
            println!("  {} {}", "•".cyan(), f.title.bold());
        }
        for s in &steps {
            let mark = if s.ok {
                "✓".green().bold()
            } else {
                "✗".red().bold()
            };
            println!(
                "  {mark} {:<25} {}",
                format!("{}:", s.name).bold(),
                s.detail
            );
        }
        if ok {
            println!("\n  Run `pypilot setup` to install the environment.");
        }
    }
    if ok {
        Ok(())
    } else {
        anyhow::bail!("some requirement fixes did not apply")
    }
}
