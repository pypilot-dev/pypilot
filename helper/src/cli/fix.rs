//! `pypilot fix python`: recompute the target interpreter and rebuild the venv.
//! `pypilot fix cuda`: re-pin torch/tensorflow to the build matching the driver.
//!
//! Both, and `pypilot install`, take `--dry-run` to print the plan instead
//! (see [`super::plan`]).

use std::path::Path;

use colored::Colorize;

use crate::core::Finding;
use crate::core::{install, project, solver, torch_index, uv};
use crate::matrix::frameworks::Framework;
use crate::matrix::solve::solve_framework;
use crate::pypi::MetadataSource;
use crate::settings::{ResolvedMode, Settings};

use super::output::{self, OutputFormat};
use super::setup::StepsReport;
use crate::core::exec::{self, Plan, PlanNote};

pub async fn run<S: MetadataSource>(
    workspace: &Path,
    settings: &Settings,
    source: &S,
    what: &str,
    format: OutputFormat,
    dry_run: bool,
) -> crate::Result<()> {
    match what {
        "python" => fix_python(workspace, settings, source, format, dry_run).await,
        "cuda" if dry_run => plan_cuda(workspace, settings, format).await,
        "cuda" => fix_cuda(workspace, settings, format).await,
        other => anyhow::bail!("unknown fix target `{other}` (expected `python` or `cuda`)"),
    }
}

/// A plan with nothing to do yet, for a dry run that stops at a decision.
fn empty_plan(mode: ResolvedMode, python: Option<crate::pypi::PyVersion>, why: String) -> Plan {
    Plan {
        mode,
        python,
        why,
        steps: Vec::new(),
        notes: Vec::new(),
        blocked: None,
    }
}

/// `fix cuda --dry-run`: the re-pin's own code path, recorded. Its progress
/// lines are silenced (JSON format sends them to stderr) and its findings
/// become the plan's notes.
async fn plan_cuda(
    workspace: &Path,
    settings: &Settings,
    format: OutputFormat,
) -> crate::Result<()> {
    let mode = settings.resolve_mode(workspace).await;
    let mut report = CudaReport::default();
    let (result, steps) = exec::dry_run(fix_cuda_into(
        workspace,
        settings,
        OutputFormat::Json,
        &mut report,
    ))
    .await;
    let mut plan = empty_plan(
        mode,
        None,
        "Re-pin torch/tensorflow to the CUDA build matching this machine's GPU driver.".into(),
    );
    plan.steps = steps;
    if let Some(message) = report.message {
        plan.notes.push(PlanNote {
            name: "fix cuda".into(),
            ok: true,
            detail: message,
        });
    }
    for pin in &report.frameworks {
        if let Some(finding) = &pin.finding {
            plan.notes.push(PlanNote {
                name: finding.title.clone(),
                ok: true,
                detail: finding.detail.clone(),
            });
        }
        if !pin.ok {
            plan.notes.push(PlanNote {
                name: pin.package.clone(),
                ok: false,
                detail: pin.detail.clone(),
            });
        }
    }
    if let Err(e) = result {
        plan.blocked = Some(format!("{e:#}"));
    } else if let Some(pin) = report.frameworks.iter().find(|p| !p.ok) {
        plan.blocked = Some(format!("{}: {}", pin.package, pin.detail));
    }
    super::plan::finish("fix cuda", workspace, &plan, format)
}

/// `fix cuda --json`: what happened to each framework.
#[derive(Debug, Default, serde::Serialize)]
struct CudaReport {
    /// Why nothing was attempted: no framework, or a pixi project.
    message: Option<String>,
    frameworks: Vec<CudaPin>,
    /// Set when the run stopped early (uv could not be fetched, a write
    /// failed).
    error: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct CudaPin {
    package: String,
    /// The solver's guidance (driver too old, no build for this GPU).
    finding: Option<Finding>,
    /// `torch==2.9.1`; `null` when no release was resolved.
    spec: Option<String>,
    index_url: Option<String>,
    /// `"pinned_in_pyproject"`, `"installed"` or `"skipped"`.
    action: &'static str,
    ok: bool,
    detail: String,
}

impl CudaReport {
    fn ok(&self) -> bool {
        self.frameworks.iter().all(|f| f.ok)
    }
}

async fn fix_cuda(
    workspace: &Path,
    settings: &Settings,
    format: OutputFormat,
) -> crate::Result<()> {
    let mut report = CudaReport::default();
    let result = fix_cuda_into(workspace, settings, format, &mut report).await;
    if format.is_json() {
        report.error = result.as_ref().err().map(|e| format!("{e:#}"));
        output::print_json("fix cuda", report.ok() && result.is_ok(), &report)?;
    }
    result
}

/// Re-pin each framework, printing as it goes (text) and recording each
/// outcome in `report` (for `--json`).
async fn fix_cuda_into(
    workspace: &Path,
    settings: &Settings,
    format: OutputFormat,
    report: &mut CudaReport,
) -> crate::Result<()> {
    let text = !format.is_json();
    let project = project::scan(workspace);
    let frameworks: Vec<_> = project
        .packages
        .iter()
        .filter(|p| Framework::from_package_name(&p.name).is_some())
        .collect();

    if frameworks.is_empty() {
        let message = "no framework packages (torch, tensorflow) found in project.";
        if text {
            println!("  {} {message}", "[INFO ]".cyan().bold());
        }
        report.message = Some(message.to_string());
        return Ok(());
    }

    let mode = settings.resolve_mode(workspace).await;
    if mode == ResolvedMode::Pixi {
        // A `uv pip install` or a `[tool.uv]` index would fight pixi, which
        // rebuilds the environment from its own manifest and lock.
        let message = "pixi project: PyPilot does not re-pin torch/tensorflow here. Take a CUDA build from conda-forge (pytorch-gpu, with `cuda` under [system-requirements]), or add the matched index under [pypi-options], then run `pixi install`.";
        if text {
            println!("  {} {message}", "[INFO ]".cyan().bold());
        }
        report.message = Some(message.to_string());
        return Ok(());
    }

    let mut any_failed = false;
    for req in frameworks {
        let Some(solved) = solve_framework(&req.name, Some(req), settings).await else {
            continue;
        };
        if let Some(finding) = solved.finding.as_ref().filter(|_| text) {
            println!("{}\n  {}\n", finding.title.bold(), finding.detail);
        }
        let mut pin = CudaPin {
            package: req.name.clone(),
            finding: solved.finding.clone(),
            spec: None,
            index_url: solved.index_url.clone(),
            action: "skipped",
            ok: true,
            detail: String::new(),
        };

        let Some(version) = &solved.resolved_version else {
            pin.detail = "no release in the bundled tables to pin".to_string();
            report.frameworks.push(pin);
            continue;
        };
        let spec = format!("{}=={version}", req.name);
        pin.spec = Some(spec.clone());

        let pyproject = workspace.join("pyproject.toml");
        if mode == ResolvedMode::UvFull && pyproject.is_file() {
            // A `uv pip install` here would be undone by the next `uv sync`,
            // which re-resolves from the manifest. Record the index there and
            // sync instead, so the fix survives.
            match torch_index::pin_file(
                &pyproject,
                solved.index_url.as_deref(),
                std::slice::from_ref(&req.name),
            ) {
                Ok(pinned) if pinned.is_pinned() => {
                    let uv_info = uv::ensure(settings).await?;
                    let out = uv::sync(&uv_info, workspace).await?;
                    pin.action = "pinned_in_pyproject";
                    if out.success() {
                        pin.detail = format!(
                            "{} now resolves from the matched index in pyproject.toml",
                            req.name
                        );
                        if text {
                            println!(
                                "  {} {} now resolves from the matched index in pyproject.toml",
                                "✓".green().bold(),
                                req.name.bold()
                            );
                        }
                    } else {
                        any_failed = true;
                        if let torch_index::PinFile::Changed { original } = &pinned {
                            std::fs::write(&pyproject, original)?;
                        }
                        pin.ok = false;
                        pin.detail = out.stderr.trim().to_string();
                        if text {
                            println!("  {} failed {}", "✗".red().bold(), out.stderr.trim().red());
                        }
                    }
                    report.frameworks.push(pin);
                    continue;
                }
                Ok(_) => {}
                Err(e) => output::progress(
                    format,
                    format!(
                        "  {} pyproject.toml left as is ({e}); a later `uv sync` may swap the build",
                        "[WARN ]".yellow().bold()
                    ),
                ),
            }
        }

        let out = match mode {
            ResolvedMode::UvFull | ResolvedMode::UvPipCompat => {
                let uv_info = uv::ensure(settings).await?;
                Some(
                    uv::pip_install(
                        &uv_info,
                        std::slice::from_ref(&spec),
                        solved.index_url.as_deref(),
                        workspace,
                    )
                    .await?,
                )
            }
            // Returned above: pixi projects are never re-pinned here.
            ResolvedMode::Pixi => None,
            ResolvedMode::PurePip => {
                let venv = workspace.join(".venv");
                if !venv.is_dir() {
                    any_failed = true;
                    pin.ok = false;
                    pin.detail = "no .venv in this project; run setup first".to_string();
                    if text {
                        println!(
                            "  {} no .venv in this project; run setup first",
                            "✗ failed".red().bold()
                        );
                    }
                    report.frameworks.push(pin);
                    continue;
                }
                Some(
                    crate::core::pip::install_packages(
                        &venv,
                        std::slice::from_ref(&spec),
                        solved.index_url.as_deref(),
                        workspace,
                    )
                    .await?,
                )
            }
        };
        if let Some(out) = out {
            pin.action = "installed";
            if out.success() {
                pin.detail = format!("installed {spec}");
                if text {
                    println!("  {} installed {}", "✓".green().bold(), spec.bold());
                }
            } else {
                any_failed = true;
                pin.ok = false;
                pin.detail = out.stderr.trim().to_string();
                if text {
                    println!("  {} failed {}", "✗".red().bold(), out.stderr.trim().red());
                }
            }
        }
        report.frameworks.push(pin);
    }

    if any_failed {
        anyhow::bail!("one or more installs did not finish, see above");
    }
    Ok(())
}

/// `fix python --json` when nothing needed rebuilding.
#[derive(serde::Serialize)]
struct AlreadyThere<'a> {
    workspace: &'a Path,
    target_python: crate::pypi::PyVersion,
    /// `null`: nothing ran.
    summary: Option<()>,
    message: String,
}

async fn fix_python<S: MetadataSource>(
    workspace: &Path,
    settings: &Settings,
    source: &S,
    format: OutputFormat,
    dry_run: bool,
) -> crate::Result<()> {
    let assessment = solver::assess(workspace, settings, source).await;

    let Some(compat) = &assessment.compat else {
        anyhow::bail!("no declared dependencies here, so there is no version to solve for");
    };
    if compat.intersection.is_empty() {
        let why = compat
            .conflicts
            .first()
            .map(|c| c.explanation.clone())
            .unwrap_or_else(|| "the dependencies share no supported Python version".to_string());
        anyhow::bail!("{why}");
    }
    let Some(target) = assessment.target_python else {
        anyhow::bail!("could not determine a target Python version");
    };

    let current = assessment.probes.venv.as_ref().and_then(|v| v.python);
    if dry_run {
        let mode = settings.resolve_mode(workspace).await;
        let why = crate::core::setup::derive_why(&assessment);
        let plan = if current == Some(target) {
            empty_plan(
                mode,
                Some(target),
                format!("{why} Already on Python {target}. Nothing to do."),
            )
        } else {
            let (summary, steps) =
                exec::dry_run(rebuild(workspace, settings, source, mode, target)).await;
            let mut plan = Plan::from_summary(&summary?, steps);
            plan.python = Some(target);
            plan.why = match current {
                Some(old) => format!("{why} The environment is on Python {old}; rebuild it."),
                None => why,
            };
            plan
        };
        return super::plan::finish("fix python", workspace, &plan, format);
    }
    if current == Some(target) && format.is_json() {
        return output::print_json(
            "fix python",
            true,
            &AlreadyThere {
                workspace,
                target_python: target,
                summary: None,
                message: format!("Already on Python {target}. Nothing to do."),
            },
        );
    }
    if current == Some(target) {
        println!(
            "  {} Already on Python {target}. Nothing to do.",
            "✓".green().bold()
        );
        return Ok(());
    }

    let installed_count = assessment
        .probes
        .venv
        .as_ref()
        .map(|v| crate::core::installed::scan(&v.path).count())
        .unwrap_or(0);

    output::progress(
        format,
        format!(
            "{}: rebuilding on Python {}. Dependencies support {}.",
            "PyPilot fix".bold().cyan(),
            format!("{target}").cyan().bold(),
            compat.intersection.to_range_string().green().bold()
        ),
    );
    if installed_count > 0 {
        output::progress(
            format,
            format!(
                "  {} {installed_count} installed packages will be reinstalled.",
                "•".cyan()
            ),
        );
    }
    if !format.is_json() {
        println!(
            "{}",
            "── Steps ────────────────────────────────────────────────"
                .bold()
                .cyan()
        );
    }

    let mode = settings.resolve_mode(workspace).await;
    let summary = rebuild(workspace, settings, source, mode, target).await?;
    if format.is_json() {
        let report = StepsReport {
            target_python: Some(target),
            ..StepsReport::new(workspace, &summary)
        };
        report.print("fix python")?;
        if summary.ok {
            return Ok(());
        }
        anyhow::bail!("the rebuild did not finish, see the steps above");
    }
    for step in &summary.steps {
        let mark = if step.ok {
            "✓".green().bold()
        } else {
            "✗".red().bold()
        };
        println!(
            "  {mark} {:<25} {}",
            format!("{}:", step.name).bold(),
            step.detail
        );
    }

    if summary.ok {
        println!(
            "\n  {} Environment now runs Python {}.",
            "✓ Done.".green().bold(),
            target.to_string().cyan().bold()
        );
        Ok(())
    } else {
        super::setup::print_pixi_build_advice(workspace, &summary);
        anyhow::bail!("the rebuild did not finish, see the steps above")
    }
}

/// Rebuild on `target` and restore the declared dependencies. There is no
/// single new package here, so reuse the bootstrap rather than the add path.
/// A pixi project changes its manifest's python spec instead (`pixi add
/// python=X.Y` + `pixi install`).
async fn rebuild<S: MetadataSource>(
    workspace: &Path,
    settings: &Settings,
    source: &S,
    mode: ResolvedMode,
    target: crate::pypi::PyVersion,
) -> crate::Result<crate::core::setup::SetupSummary> {
    if mode == ResolvedMode::Pixi {
        install::recreate_with_python(workspace, settings, target, &[]).await
    } else {
        crate::core::setup::run(workspace, settings, source).await
    }
}

/// Install one package from the terminal, sharing the code action's path.
pub async fn install_one(
    workspace: &Path,
    settings: &Settings,
    package: &str,
    format: OutputFormat,
    dry_run: bool,
) -> crate::Result<()> {
    install::validate_package_name(package)?;
    if dry_run {
        let (summary, steps) =
            exec::dry_run(install::install_package(workspace, settings, package)).await;
        let plan = Plan::from_summary(&summary?, steps);
        return super::plan::finish("install", workspace, &plan, format);
    }
    if format.is_json() {
        output::progress(format, format!("Installing {package}..."));
        let summary = install::install_package(workspace, settings, package).await?;
        let report = StepsReport {
            package: Some(package),
            ..StepsReport::new(workspace, &summary)
        };
        report.print("install")?;
        if summary.ok {
            return Ok(());
        }
        anyhow::bail!("install did not finish");
    }
    println!(
        "{} installing {}...",
        "PyPilot install:".bold().cyan(),
        package.bold()
    );
    println!(
        "{}",
        "── Steps ────────────────────────────────────────────────"
            .bold()
            .cyan()
    );
    let summary = install::install_package(workspace, settings, package).await?;
    for step in &summary.steps {
        let mark = if step.ok {
            "✓".green().bold()
        } else {
            "✗".red().bold()
        };
        println!(
            "  {mark} {:<25} {}",
            format!("{}:", step.name).bold(),
            step.detail
        );
    }
    if summary.ok {
        println!(
            "\n  {} Successfully installed {}.",
            "✓ Done.".green().bold(),
            package.bold()
        );
        Ok(())
    } else {
        super::setup::print_pixi_build_advice(workspace, &summary);
        anyhow::bail!("install did not finish")
    }
}
