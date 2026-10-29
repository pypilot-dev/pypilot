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
//!
//! It is also judged against the project's other dependencies: when no
//! Python runs it and them, the verdict names the two packages that clash,
//! in the words `doctor` and `setup` use. `--json` prints the
//! [`CheckReport`]; `--markdown` the editor report's section.

use std::path::Path;

use colored::Colorize;

use crate::core::platform::Platform;
use crate::core::{probe, project};
use crate::pypi::MetadataSource;
use crate::report::check::{self, CheckContext, CheckReport, VerdictKind};
use crate::settings::Settings;

use super::output::{self, OutputFormat};

pub async fn run<S: MetadataSource>(
    workspace: &Path,
    settings: &Settings,
    source: &S,
    package: &str,
    suggest_alternatives: bool,
    format: OutputFormat,
) -> crate::Result<()> {
    let Some(req) = project::parse_requirement(package) else {
        anyhow::bail!("`{package}` is not a package name or requirement");
    };
    let probes = probe::run(workspace, settings).await;
    // The project's other dependencies, for the conflict check. Cache-first,
    // and the same lookups `doctor` makes.
    let deps = project::scan(workspace);
    let compat = if deps.packages.is_empty() {
        None
    } else {
        Some(crate::pypi::analyze(source, Platform::current(), &deps.packages).await)
    };
    let ctx = CheckContext {
        workspace,
        venv_python: probes.venv.as_ref().and_then(|v| v.python),
        pixi: crate::core::pixi::detect(workspace).is_some(),
        project: compat
            .as_ref()
            .map(|c| c.per_package.as_slice())
            .unwrap_or_default(),
    };
    let report = check::evaluate(source, &req, &ctx).await?;

    match format {
        OutputFormat::Json => output::print_json("check", !report.failed(), &report),
        OutputFormat::Markdown => {
            print!(
                "{}",
                crate::report::markdown::check_markdown(package, &report).trim_start()
            );
            Ok(())
        }
        OutputFormat::Text => {
            print_text(&report, suggest_alternatives);
            Ok(())
        }
    }
}

fn print_text(r: &CheckReport, suggest_alternatives: bool) {
    let analysis = &r.analysis;
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
    if r.spec.is_some() {
        println!("  {:<18}: {}", "requirement", r.requirement.cyan());
    }
    println!(
        "  {:<18}: {}",
        "supported Python",
        r.supported_range.green().bold()
    );
    if let Some(rp) = &analysis.requires_python {
        println!("  {:<18}: {}", "requires-python", rp.green());
    }
    if analysis.sdist_only {
        println!(
            "  {:<18}: {}",
            "wheels",
            "none for this platform, will compile from source".yellow()
        );
    }
    if r.no_files.is_some() {
        println!(
            "  {:<18}: {}",
            "files",
            format!("none for {}, cannot be installed", r.platform)
                .red()
                .bold()
        );
    }
    println!(
        "  {:<18}: {}",
        "installed",
        if r.installed {
            "yes".green().bold().to_string()
        } else {
            "no".yellow().to_string()
        }
    );
    if let Some(c) = &r.conflict {
        // The other package of the pair; the checked one is above.
        let other = if c.low.name == analysis.name {
            &c.high
        } else {
            &c.low
        };
        println!(
            "  {:<18}: {} {} (Python {})",
            "conflicts with",
            other.name.bold(),
            other.version,
            other.supported.to_range_string().yellow()
        );
    }

    println!(
        "{}",
        "── Verdict ──────────────────────────────────────────────"
            .bold()
            .cyan()
    );
    let mark = match r.verdict.kind {
        VerdictKind::Supported => "✓".green().bold(),
        VerdictKind::NoEnvironment => "[INFO ]".cyan().bold(),
        VerdictKind::NoFiles | VerdictKind::Conflict | VerdictKind::Unsupported => {
            "[ERROR]".red().bold()
        }
    };
    println!("  {mark} {}", r.verdict.message);

    if r.failed() || suggest_alternatives {
        print_alternatives(r);
    }
}

/// The "why" and "alternatives" lines (see
/// [`crate::core::pixi_advice::check_advice`]).
fn print_alternatives(r: &CheckReport) {
    for reason in &r.why {
        println!("  {:<18}: {}", "why", reason);
    }
    // A pixi project already installs from conda-forge.
    if r.pixi_project {
        return;
    }
    match &r.alternative {
        Some(alt) => println!("  {:<18}: {}", "alternatives", alt.cyan()),
        None => println!(
            "  {:<18}: {}",
            "alternatives",
            "none known (PyPilot's conda-forge table has no build of it for this platform)"
                .yellow()
        ),
    }
}
