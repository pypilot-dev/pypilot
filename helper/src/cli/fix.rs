//! `pypilot fix python`: recompute the target interpreter and rebuild the venv.
//! `pypilot fix cuda`: re-pin torch/tensorflow to the build matching the driver.

use std::path::Path;

use colored::Colorize;

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
) -> crate::Result<()> {
    match what {
        "python" => fix_python(workspace, settings, source).await,
        "cuda" => fix_cuda(workspace, settings).await,
        other => anyhow::bail!("unknown fix target `{other}` (expected `python` or `cuda`)"),
    }
}

async fn fix_cuda(workspace: &Path, settings: &Settings) -> crate::Result<()> {
    let project = project::scan(workspace);
    let frameworks: Vec<_> = project
        .packages
        .iter()
        .filter(|p| Framework::from_package_name(&p.name).is_some())
        .collect();

    if frameworks.is_empty() {
        println!(
            "  {} no framework packages (torch, tensorflow) found in project.",
            "[INFO ]".cyan().bold()
        );
        return Ok(());
    }

    let mode = settings.resolve_mode(workspace).await;
    if mode == ResolvedMode::Pixi {
        // A `uv pip install` or a `[tool.uv]` index would fight pixi, which
        // rebuilds the environment from its own manifest and lock.
        println!(
            "  {} pixi project: PyPilot does not re-pin torch/tensorflow here. Take a CUDA build from conda-forge (pytorch-gpu, with `cuda` under [system-requirements]), or add the matched index under [pypi-options], then run `pixi install`.",
            "[INFO ]".cyan().bold()
        );
        return Ok(());
    }

    let mut any_failed = false;
    for req in frameworks {
        let Some(solved) = solve_framework(&req.name, Some(req), settings).await else {
            continue;
        };
        if let Some(finding) = &solved.finding {
            println!("{}\n  {}\n", finding.title.bold(), finding.detail);
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
                    continue;
                }
                Ok(_) => {}
                Err(e) => println!(
                    "  {} pyproject.toml left as is ({e}); a later `uv sync` may swap the build",
                    "[WARN ]".yellow().bold()
                ),
            }
        }

        match mode {
            ResolvedMode::UvFull | ResolvedMode::UvPipCompat => {
                let uv_info = uv::ensure(settings).await?;
                let out = uv::pip_install(
                    &uv_info,
                    std::slice::from_ref(&spec),
                    solved.index_url.as_deref(),
                    workspace,
                )
                .await?;
                if out.success() {
                    println!("  {} installed {}", "✓".green().bold(), spec.bold());
                } else {
                    any_failed = true;
                    println!("  {} failed {}", "✗".red().bold(), out.stderr.trim().red());
                }
            }
            // Returned above: pixi projects are never re-pinned here.
            ResolvedMode::Pixi => {}
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
                let out = crate::core::pip::install_packages(
                    &venv,
                    std::slice::from_ref(&spec),
                    solved.index_url.as_deref(),
                    workspace,
                )
                .await?;
                if out.success() {
                    println!("  {} installed {}", "✓".green().bold(), spec.bold());
                } else {
                    any_failed = true;
                    println!("  {} failed {}", "✗".red().bold(), out.stderr.trim().red());
                }
            }
        }
    }

    if any_failed {
        anyhow::bail!("one or more installs did not finish, see above");
    }
    Ok(())
}

async fn fix_python<S: MetadataSource>(
    workspace: &Path,
    settings: &Settings,
    source: &S,
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

    // Rebuild and restore the declared dependencies. There is no single new
    // package here, so reuse the bootstrap rather than the add path. A pixi
    // project changes its manifest's python spec instead (`pixi add
    // python=X.Y` + `pixi install`).
    let summary = if settings.resolve_mode(workspace).await == ResolvedMode::Pixi {
        install::recreate_with_python(workspace, settings, target, &[]).await?
    } else {
        crate::core::setup::run(workspace, settings, source).await?
    };
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
) -> crate::Result<()> {
    install::validate_package_name(package)?;
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
