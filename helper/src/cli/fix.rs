//! `pypilot fix python`: recompute the target interpreter and rebuild the venv.
//! `pypilot fix cuda`: re-pin torch/tensorflow to the build matching the driver.
//!
//! Both, and `pypilot install`, take `--dry-run` to print the plan instead
//! (see [`super::plan`]).

use std::path::Path;

use colored::Colorize;

use super::plan::{self, RunOpts};
use crate::core::exec::{self, Plan, PlanNote};
use crate::core::{install, project, solver, torch_index, uv};
use crate::matrix::frameworks::Framework;
use crate::matrix::solve::solve_framework;
use crate::pypi::MetadataSource;
use crate::settings::{ResolvedMode, Settings};

pub async fn run<S: MetadataSource>(
    workspace: &Path,
    settings: &Settings,
    source: &S,
    what: &str,
    opts: RunOpts,
) -> crate::Result<()> {
    match what {
        "python" => fix_python(workspace, settings, source, opts).await,
        "cuda" => fix_cuda(workspace, settings, opts).await,
        other => anyhow::bail!("unknown fix target `{other}` (expected `python` or `cuda`)"),
    }
}

/// A plan with nothing to do, for a dry run that stops at a decision.
fn empty_plan(mode: ResolvedMode, python: Option<String>, why: String) -> Plan {
    Plan {
        mode: exec::mode_name(mode).to_string(),
        python,
        why,
        steps: Vec::new(),
        notes: Vec::new(),
        blocked: None,
    }
}

const CUDA_WHY: &str =
    "Re-pin torch/tensorflow to the CUDA build matching this machine's GPU driver.";

async fn fix_cuda(workspace: &Path, settings: &Settings, opts: RunOpts) -> crate::Result<()> {
    let mode = settings.resolve_mode(workspace).await;
    if !opts.dry_run {
        return repin_frameworks(workspace, settings, mode, opts)
            .await
            .map(|_| ());
    }
    let (result, steps) = exec::dry_run(repin_frameworks(workspace, settings, mode, opts)).await;
    let notes = result?;
    let mut plan = empty_plan(mode, None, CUDA_WHY.to_string());
    plan.steps = steps;
    plan.notes = notes;
    plan::finish("fix cuda", workspace, &plan, opts)
}

/// The body of `fix cuda`. Returns the GPU findings, which a dry run lists
/// with its plan.
async fn repin_frameworks(
    workspace: &Path,
    settings: &Settings,
    mode: ResolvedMode,
    opts: RunOpts,
) -> crate::Result<Vec<PlanNote>> {
    let mut notes = Vec::new();
    let info = |notes: &mut Vec<PlanNote>, text: &str| {
        if !opts.dry_run {
            println!("  {} {text}", "[INFO ]".cyan().bold());
        }
        notes.push(PlanNote {
            name: "fix cuda".into(),
            ok: true,
            detail: text.to_string(),
        });
    };
    let project = project::scan(workspace);
    let frameworks: Vec<_> = project
        .packages
        .iter()
        .filter(|p| Framework::from_package_name(&p.name).is_some())
        .collect();

    if frameworks.is_empty() {
        info(
            &mut notes,
            "no framework packages (torch, tensorflow) found in project.",
        );
        return Ok(notes);
    }

    if mode == ResolvedMode::Pixi {
        // A `uv pip install` or a `[tool.uv]` index would fight pixi, which
        // rebuilds the environment from its own manifest and lock.
        info(
            &mut notes,
            "pixi project: PyPilot does not re-pin torch/tensorflow here. Take a CUDA build from conda-forge (pytorch-gpu, with `cuda` under [system-requirements]), or add the matched index under [pypi-options], then run `pixi install`.",
        );
        return Ok(notes);
    }

    // In a dry run the installs below are only recorded, so there is no
    // outcome to report until the plan is printed.
    let report = !exec::is_dry_run();
    let mut any_failed = false;
    for req in frameworks {
        let Some(solved) = solve_framework(&req.name, Some(req), settings).await else {
            continue;
        };
        if let Some(finding) = &solved.finding {
            if !opts.dry_run {
                println!("{}\n  {}\n", finding.title.bold(), finding.detail);
            }
            notes.push(PlanNote {
                name: finding.title.clone(),
                ok: true,
                detail: finding.detail.clone(),
            });
        }

        let Some(version) = &solved.resolved_version else {
            continue;
        };
        let spec = format!("{}=={version}", req.name);

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
                    if report {
                        if out.success() {
                            println!(
                                "  {} {} now resolves from the matched index in pyproject.toml",
                                "✓".green().bold(),
                                req.name.bold()
                            );
                        } else {
                            any_failed = true;
                            if let torch_index::PinFile::Changed { original } = &pinned {
                                std::fs::write(&pyproject, original)?;
                            }
                            println!("  {} failed {}", "✗".red().bold(), out.stderr.trim().red());
                        }
                    }
                    continue;
                }
                Ok(_) => {}
                Err(e) => {
                    let text = format!(
                        "pyproject.toml left as is ({e}); a later `uv sync` may swap the build"
                    );
                    if !opts.dry_run {
                        println!("  {} {text}", "[WARN ]".yellow().bold());
                    }
                    notes.push(PlanNote {
                        name: "record PyTorch index".into(),
                        ok: false,
                        detail: text,
                    });
                }
            }
        }

        let out = match mode {
            ResolvedMode::UvFull | ResolvedMode::UvPipCompat => {
                let uv_info = uv::ensure(settings).await?;
                uv::pip_install(
                    &uv_info,
                    std::slice::from_ref(&spec),
                    solved.index_url.as_deref(),
                    workspace,
                )
                .await?
            }
            // Returned above: pixi projects are never re-pinned here.
            ResolvedMode::Pixi => continue,
            ResolvedMode::PurePip => {
                let venv = workspace.join(".venv");
                if !venv.is_dir() {
                    any_failed = true;
                    println!(
                        "  {} no .venv in this project; run setup first",
                        "✗ failed".red().bold()
                    );
                    continue;
                }
                crate::core::pip::install_packages(
                    &venv,
                    std::slice::from_ref(&spec),
                    solved.index_url.as_deref(),
                    workspace,
                )
                .await?
            }
        };
        if report {
            if out.success() {
                println!("  {} installed {}", "✓".green().bold(), spec.bold());
            } else {
                any_failed = true;
                println!("  {} failed {}", "✗".red().bold(), out.stderr.trim().red());
            }
        }
    }

    if any_failed {
        anyhow::bail!("one or more installs did not finish, see above");
    }
    Ok(notes)
}

async fn fix_python<S: MetadataSource>(
    workspace: &Path,
    settings: &Settings,
    source: &S,
    opts: RunOpts,
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
    let mode = settings.resolve_mode(workspace).await;
    let why = crate::core::setup::derive_why(&assessment);

    let current = assessment.probes.venv.as_ref().and_then(|v| v.python);
    if current == Some(target) {
        if opts.dry_run {
            let plan = empty_plan(
                mode,
                Some(target.to_string()),
                format!("{why} Already on Python {target}. Nothing to do."),
            );
            return plan::finish("fix python", workspace, &plan, opts);
        }
        println!(
            "  {} Already on Python {target}. Nothing to do.",
            "✓".green().bold()
        );
        return Ok(());
    }

    // Rebuild and restore the declared dependencies. There is no single new
    // package here, so reuse the bootstrap rather than the add path. A pixi
    // project changes its manifest's python spec instead (`pixi add
    // python=X.Y` + `pixi install`).
    let rebuild = async {
        if mode == ResolvedMode::Pixi {
            install::recreate_with_python(workspace, settings, target, &[]).await
        } else {
            crate::core::setup::run(workspace, settings, source).await
        }
    };

    if opts.dry_run {
        let (summary, steps) = exec::dry_run(rebuild).await;
        let mut plan = Plan::from_summary(&summary?, steps);
        plan.python = Some(target.to_string());
        plan.why = match current {
            Some(old) => format!("{why} The environment is on Python {old}; rebuild it."),
            None => why,
        };
        return plan::finish("fix python", workspace, &plan, opts);
    }

    let installed_count = assessment
        .probes
        .venv
        .as_ref()
        .map(|v| crate::core::installed::scan(&v.path).count())
        .unwrap_or(0);

    println!(
        "{} — Rebuilding on Python {}. Dependencies support {}.",
        "PyPilot fix".bold().cyan(),
        format!("{target}").cyan().bold(),
        compat.intersection.to_range_string().green().bold()
    );
    if installed_count > 0 {
        println!(
            "  {} {installed_count} installed packages will be reinstalled.",
            "•".cyan()
        );
    }
    println!(
        "{}",
        "── Steps ────────────────────────────────────────────────"
            .bold()
            .cyan()
    );

    let summary = rebuild.await?;
    for step in &summary.steps {
        let mark = if step.ok {
            "✓".green().bold()
        } else {
            "✗".red().bold()
        };
        println!("  {mark} {:<24} — {}", step.name.bold(), step.detail);
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

/// Install one package from the terminal, sharing the code action's path.
pub async fn install_one(
    workspace: &Path,
    settings: &Settings,
    package: &str,
    opts: RunOpts,
) -> crate::Result<()> {
    install::validate_package_name(package)?;
    if opts.dry_run {
        let (summary, steps) =
            exec::dry_run(install::install_package(workspace, settings, package)).await;
        let plan = Plan::from_summary(&summary?, steps);
        return plan::finish("install", workspace, &plan, opts);
    }
    println!(
        "{} Installing {}...",
        "PyPilot install —".bold().cyan(),
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
        println!("  {mark} {:<24} — {}", step.name.bold(), step.detail);
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
