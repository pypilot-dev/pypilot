//! `pypilot check <pkg>`: can this package run on the current environment?
//!
//! Read-only, and useful before adding a dependency rather than after the
//! install fails. Takes anything a requirements file would: `numpy`,
//! `mediapipe==0.10.14`, `torch<2.6`. A specifier is judged on the release
//! it selects, whose wheels can support different Pythons than the newest.
//!
//! When the verdict is a failure (no files here, no wheel for this Python,
//! a source build), it also says why and names the conda-forge build pixi
//! could install instead. That costs nothing (the table is bundled), and a
//! failure is exactly when the user needs it, so it is not hidden behind a
//! flag; `--suggest-alternatives` shows the section for a passing verdict
//! too.

use std::path::Path;

use colored::Colorize;

use crate::core::platform::Platform;
use crate::core::{pixi_advice, probe, project};
use crate::pypi::MetadataSource;
use crate::settings::Settings;

pub async fn run<S: MetadataSource>(
    workspace: &Path,
    settings: &Settings,
    source: &S,
    package: &str,
    suggest_alternatives: bool,
) -> crate::Result<()> {
    let Some(req) = project::parse_requirement(package) else {
        anyhow::bail!("`{package}` is not a package name or requirement");
    };
    let meta = crate::pypi::resolve(source, &req).await?;

    let analysis = meta.analyze(&Platform::current());
    let probes = probe::run(workspace, settings).await;

    println!(
        "{} {} ({})",
        "PyPilot check —".bold().cyan(),
        analysis.name.bold(),
        format!("v{}", analysis.version).cyan()
    );
    println!(
        "{}",
        "── Package Details ──────────────────────────────────────"
            .bold()
            .cyan()
    );
    if req.is_pinned() {
        println!("  {:<18}: {}", "requirement", req.to_string().cyan());
    }
    println!(
        "  {:<18}: {}",
        "supported Python",
        analysis.supported.to_range_string().green().bold()
    );
    if let Some(rp) = &analysis.requires_python {
        println!("  {:<18}: {}", "requires-python", rp.to_string().green());
    }
    if analysis.sdist_only {
        println!(
            "  {:<18}: {}",
            "wheels",
            "none for this platform, will compile from source".yellow()
        );
    }
    let here = Platform::current().label();
    let no_files = analysis.no_files_message(here);
    if no_files.is_some() {
        println!(
            "  {:<18}: {}",
            "files",
            format!("none for {here}, cannot be installed").red().bold()
        );
    }

    let env = crate::core::env::project_env(workspace);
    let present = crate::core::env::importable(workspace, env.as_ref()).contains(&analysis.name);
    println!(
        "  {:<18}: {}",
        "installed",
        if present {
            "yes".green().bold().to_string()
        } else {
            "no".yellow().to_string()
        }
    );

    println!(
        "{}",
        "── Verdict ──────────────────────────────────────────────"
            .bold()
            .cyan()
    );
    let venv_python = probes.venv.as_ref().and_then(|v| v.python);
    let failed = no_files.is_some()
        || analysis.sdist_only
        || venv_python.is_some_and(|py| !analysis.supported.contains(py));
    match venv_python {
        // No Python helps, so don't suggest one.
        _ if no_files.is_some() => {
            println!(
                "  {} {}",
                "[ERROR]".red().bold(),
                no_files.unwrap_or_default()
            );
        }
        Some(current) if analysis.supported.contains(current) => {
            println!(
                "  {} This project's Python {} can run {}.",
                "✓".green().bold(),
                current.to_string().cyan().bold(),
                analysis.name.bold()
            );
        }
        Some(current) => {
            let suggestion = analysis
                .supported
                .max()
                .map(|t| format!(" Use Python {t} instead."))
                .unwrap_or_default();
            println!(
                "  {} {} does not support this project's Python {}.{}",
                "[ERROR]".red().bold(),
                analysis.name.bold(),
                current.to_string().yellow().bold(),
                suggestion.cyan()
            );
        }
        None => {
            let suggestion = analysis
                .supported
                .max()
                .map(|t| format!(" Python {t} would suit it."))
                .unwrap_or_default();
            let pixi = crate::core::pixi::detect(workspace).is_some();
            println!(
                "  {} {}{}",
                "[INFO ]".cyan().bold(),
                crate::core::env::no_env_sentence(pixi),
                suggestion.cyan()
            );
        }
    }

    if failed || suggest_alternatives {
        print_alternatives(workspace, &analysis, venv_python);
    }

    Ok(())
}

/// The "why" and "alternatives" lines (see [`pixi_advice::check_advice`]).
fn print_alternatives(
    workspace: &Path,
    analysis: &crate::pypi::PackageAnalysis,
    venv_python: Option<crate::pypi::PyVersion>,
) {
    let machine = pixi_advice::Machine {
        platform: Platform::current(),
        venv_python,
        python: venv_python.or_else(|| analysis.supported.max()),
        nvcc: pixi_advice::nvcc_available(),
    };
    let advice = pixi_advice::check_advice(analysis, crate::matrix::conda_forge(), &machine);
    for reason in &advice.reasons {
        println!("  {:<18}: {}", "why", reason);
    }
    // A pixi project already installs from conda-forge.
    if crate::core::pixi::detect(workspace).is_some() {
        return;
    }
    match advice.alternative {
        Some(alt) => println!("  {:<18}: {}", "alternatives", alt.cyan()),
        None => println!(
            "  {:<18}: {}",
            "alternatives",
            "none known (PyPilot's conda-forge table has no build of it for this platform)"
                .yellow()
        ),
    }
}
