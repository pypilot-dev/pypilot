//! The actions behind F4's code actions.
//!
//! Two operations: add packages to the current environment, and rebuild the
//! environment on a different Python before adding them. Both record the package
//! in the project manifest, because an install that vanishes on the next clone
//! has not really solved the user's problem.
//!
//! Installing `torch` or `tensorflow` here also consults F2's solver (see
//! [`crate::matrix::solve`]) for an index URL matched to the machine's GPU
//! driver, so the quick fix that follows a bare `import torch` pulls the CUDA
//! build the driver can actually run rather than whatever the default index
//! happens to hand back.

use std::path::Path;

use anyhow::bail;

use crate::core::exec::{self, Effect};
use crate::core::setup::{self, SetupSummary, Step};
use crate::core::{pip, pixi, pixi_manage, torch_index, uv};
use crate::matrix;
use crate::pypi::pyversion::PyVersion;
use crate::settings::{ResolvedMode, Settings};

/// Install `package` into the project's existing environment and record it.
pub async fn install_package(
    workspace: &Path,
    settings: &Settings,
    package: &str,
) -> crate::Result<SetupSummary> {
    install_packages(workspace, settings, &[package.to_string()]).await
}

/// Packages that can share one installer invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    pub index_url: Option<String>,
    pub packages: Vec<String>,
}

/// Split packages into as few installer invocations as possible.
///
/// An index URL applies to every package in the command that carries it, so
/// torch pinned to a CUDA index must not drag numpy onto that index, where it
/// may not exist at all. Packages on the default index go first, together, so
/// a `requirements.txt` migration never runs against a custom index; each
/// distinct index URL then gets its own invocation.
pub fn batch_by_index(packages: Vec<(String, Option<String>)>) -> Vec<Batch> {
    let mut batches: Vec<Batch> = Vec::new();
    let mut default = Vec::new();
    for (package, index_url) in packages {
        let Some(url) = index_url else {
            default.push(package);
            continue;
        };
        match batches
            .iter_mut()
            .find(|b| b.index_url.as_ref() == Some(&url))
        {
            Some(batch) => batch.packages.push(package),
            None => batches.push(Batch {
                index_url: Some(url),
                packages: vec![package],
            }),
        }
    }
    if !default.is_empty() {
        batches.insert(
            0,
            Batch {
                index_url: None,
                packages: default,
            },
        );
    }
    batches
}

/// Install `packages` into the project's existing environment and record them.
///
/// One installer run per [`Batch`], and the manifest is frozen once at the end,
/// so a single package behaves exactly like [`install_package`] always has.
pub async fn install_packages(
    workspace: &Path,
    settings: &Settings,
    packages: &[String],
) -> crate::Result<SetupSummary> {
    let mode = settings.resolve_mode(workspace).await;
    let mut summary = SetupSummary {
        package_manager: settings.package_manager,
        resolved_mode: mode,
        python: None,
        venv_path: env_path(workspace, mode),
        why: format!("Installing {}.", packages.join(", ")),
        steps: Vec::new(),
        ok: true,
    };

    // Unpinned here — this path installs whatever "the package" resolves to
    // right now, so F2 solves against the newest curated release.
    let mut solved = Vec::with_capacity(packages.len());
    for package in packages {
        let hardware = matrix::solve::solve_framework(package, None, settings).await;
        if let Some(finding) = hardware.as_ref().and_then(|h| h.finding.as_ref()) {
            push(&mut summary, "GPU/CUDA check", true, &finding.detail);
        }
        solved.push((package.clone(), hardware.and_then(|h| h.index_url)));
    }
    let batches = batch_by_index(solved);

    match mode {
        ResolvedMode::Pixi => {
            // pixi takes no per-command index, and PyPilot never writes uv's
            // `[tool.uv]` index table into a pixi project. Say so rather than
            // silently installing a different torch build than F2 matched.
            if batches.iter().any(|b| b.index_url.is_some()) {
                push(
                    &mut summary,
                    "GPU/CUDA index",
                    true,
                    "pixi project: installing from PyPI's default index. For a CUDA build, add the matched index under [pypi-options] in the pixi manifest, or take torch from conda-forge (pytorch-gpu).",
                );
            }
            let all: Vec<String> = batches.into_iter().flat_map(|b| b.packages).collect();
            let pixi = pixi::locate().await;
            pixi_manage::add_pypi(pixi.as_deref(), workspace, &all, &mut summary).await;
            if !summary.ok {
                return Ok(summary);
            }
        }
        ResolvedMode::UvFull => {
            let uv_info = match uv::ensure(settings).await {
                Ok(info) => info,
                Err(e) => {
                    push(&mut summary, "ensure uv", false, &e.to_string());
                    return Ok(summary);
                }
            };

            // Ensure a pyproject.toml exists so `uv add` has somewhere to
            // record the dependency. `uv init --no-workspace` is idempotent-ish:
            // if pyproject.toml is already present it exits without touching it.
            let has_pyproject = workspace.join("pyproject.toml").is_file();
            if !has_pyproject {
                // Pin the new manifest to the environment's Python, or the
                // `uv add` below rebuilds .venv on uv's default instead.
                let python = crate::core::env::project_env(workspace).and_then(|e| e.python);
                match uv::init(&uv_info, python, workspace).await {
                    Ok(out) if out.success() => {
                        push(&mut summary, "uv init", true, "pyproject.toml created");
                    }
                    Ok(out) => {
                        // Non-fatal: may already exist or be a git-tracked dir.
                        push(
                            &mut summary,
                            "uv init",
                            true,
                            &format!("(warning) {}", out.stderr.trim()),
                        );
                    }
                    Err(e) => {
                        push(&mut summary, "uv init", false, &e.to_string());
                        return Ok(summary);
                    }
                }
            }

            // `uv add` records the dep in pyproject.toml AND installs it, so
            // this is always the right call — whether pyproject existed before
            // or was just created by `uv init`. If a requirements.txt exists
            // alongside, the first batch goes through the migration helper,
            // which pulls existing entries into the new pyproject before adding
            // the requested packages.
            let pyproject_was_new = !has_pyproject;
            let pyproject = workspace.join("pyproject.toml");
            for (i, batch) in batches.iter().enumerate() {
                let mut index_url = batch.index_url.as_deref();
                // Record a PyTorch index in the manifest instead of passing it
                // on the command line, so a later `uv sync` keeps the CUDA
                // build. If the manifest cannot be edited, the command-line
                // index still gets this install right.
                let pinned = match torch_index::pin_file(&pyproject, index_url, &batch.packages) {
                    Ok(pinned) => pinned,
                    // Only worth mentioning for a batch that has an index to
                    // record; a plain numpy add would warn about nothing.
                    Err(e) if index_url.is_some() => {
                        let detail = format!(
                            "(warning) pyproject.toml left as is ({e}), so a later `uv sync` may swap the build"
                        );
                        push(&mut summary, "record PyTorch index", true, &detail);
                        torch_index::PinFile::NotApplicable
                    }
                    Err(_) => torch_index::PinFile::NotApplicable,
                };
                if pinned.is_pinned() {
                    index_url = None;
                }
                if matches!(pinned, torch_index::PinFile::Changed { .. }) {
                    push(
                        &mut summary,
                        "record PyTorch index",
                        true,
                        "pyproject.toml routes torch to the matched build, so `uv sync` keeps it",
                    );
                }
                let step = format!("uv add {}", batch.packages.join(" "));
                let result = if pyproject_was_new && i == 0 {
                    uv::add_with_requirements_migration(
                        &uv_info,
                        &batch.packages,
                        index_url,
                        workspace,
                    )
                    .await
                } else {
                    uv::add(&uv_info, &batch.packages, index_url, workspace).await
                };

                let ok = matches!(&result, Ok(out) if out.success());
                match result {
                    Ok(out) if out.success() => push(
                        &mut summary,
                        &step,
                        true,
                        "installed and recorded in pyproject.toml",
                    ),
                    Ok(out) => push(&mut summary, &step, false, out.stderr.trim()),
                    Err(e) => push(&mut summary, &step, false, &e.to_string()),
                }
                // A failed add must not leave the project routed to an index
                // it never installed from.
                if let (false, torch_index::PinFile::Changed { original }) = (ok, &pinned) {
                    if let Err(e) = std::fs::write(&pyproject, original) {
                        push(
                            &mut summary,
                            "restore pyproject.toml",
                            false,
                            &e.to_string(),
                        );
                    }
                }
            }
        }
        ResolvedMode::UvPipCompat => {
            let uv_info = match uv::ensure(settings).await {
                Ok(info) => info,
                Err(e) => {
                    push(&mut summary, "ensure uv", false, &e.to_string());
                    return Ok(summary);
                }
            };
            let venv = workspace.join(".venv");
            if !exec::exists(&venv) {
                push(
                    &mut summary,
                    "locate venv",
                    false,
                    "no .venv in this project; run setup first",
                );
                return Ok(summary);
            }
            let mut any_installed = false;
            for batch in &batches {
                let index_url = batch.index_url.as_deref();
                let step = format!("uv pip install {}", batch.packages.join(" "));
                match uv::pip_install(&uv_info, &batch.packages, index_url, workspace).await {
                    Ok(out) if out.success() => {
                        push(&mut summary, &step, true, "installed");
                        any_installed = true;
                    }
                    Ok(out) => push(&mut summary, &step, false, out.stderr.trim()),
                    Err(e) => push(&mut summary, &step, false, &e.to_string()),
                }
            }
            if any_installed {
                freeze_into_requirements_uv(&uv_info, workspace, &mut summary).await;
            }
        }
        ResolvedMode::PurePip => {
            let venv = workspace.join(".venv");
            if !exec::exists(&venv) {
                push(
                    &mut summary,
                    "locate venv",
                    false,
                    "no .venv in this project; run setup first",
                );
                return Ok(summary);
            }
            let mut any_installed = false;
            for batch in &batches {
                let index_url = batch.index_url.as_deref();
                let step = format!("pip install {}", batch.packages.join(" "));
                match pip::install_packages(&venv, &batch.packages, index_url, workspace).await {
                    Ok(out) if out.success() => {
                        push(&mut summary, &step, true, "installed");
                        any_installed = true;
                    }
                    Ok(out) => push(&mut summary, &step, false, out.stderr.trim()),
                    Err(e) => push(&mut summary, &step, false, &e.to_string()),
                }
            }
            // Freeze the full venv state into requirements.txt so the file
            // tracks exact pinned versions rather than bare names.
            if any_installed {
                freeze_into_requirements(&venv, workspace, &mut summary).await;
            }
        }
    }

    // Notebooks need ipykernel in the venv. This is also the tail of
    // `recreate_with_python`, which ends here and so never needs its own call.
    // When the user asked for ipykernel themselves, the install above already
    // did the job.
    let asked_for_ipykernel = packages
        .iter()
        .any(|p| crate::core::project::requirement_name(p).as_deref() == Some("ipykernel"));
    if !asked_for_ipykernel {
        setup::ensure_ipykernel(workspace, settings, mode, &mut summary).await;
    }
    let installed_torch = packages
        .iter()
        .any(|p| crate::core::project::requirement_name(p).as_deref() == Some("torch"));
    if installed_torch {
        setup::verify_gpu(workspace, &mut summary).await;
    }

    Ok(summary)
}

/// Rebuild the environment on `version` once, then install `packages` into it.
///
/// The caller has already told the user how many packages this reinstalls, so
/// this does the work without asking again.
pub async fn recreate_with_python(
    workspace: &Path,
    settings: &Settings,
    version: PyVersion,
    packages: &[String],
) -> crate::Result<SetupSummary> {
    let mode = settings.resolve_mode(workspace).await;
    let venv_path = env_path(workspace, mode);
    let mut summary = SetupSummary {
        package_manager: settings.package_manager,
        resolved_mode: mode,
        python: Some(version),
        venv_path: venv_path.clone(),
        why: format!(
            "Rebuilding the environment on Python {version} so {} can install.",
            packages.join(", ")
        ),
        steps: Vec::new(),
        ok: true,
    };

    match mode {
        ResolvedMode::Pixi => {
            // The manifest's python spec decides the interpreter; pixi
            // rebuilds the environment on it.
            let pixi = pixi::locate().await;
            pixi_manage::set_python(pixi.as_deref(), workspace, version, &mut summary).await;
            if !summary.ok {
                return Ok(summary);
            }
        }
        ResolvedMode::UvFull => {
            let uv_info = match uv::ensure(settings).await {
                Ok(info) => info,
                Err(e) => {
                    push(&mut summary, "ensure uv", false, &e.to_string());
                    return Ok(summary);
                }
            };

            match uv::python_install(&uv_info, version, workspace).await {
                Ok(out) if out.success() => push(
                    &mut summary,
                    &format!("uv python install {version}"),
                    true,
                    "ready",
                ),
                Ok(out) => push(
                    &mut summary,
                    &format!("uv python install {version}"),
                    false,
                    out.stderr.trim(),
                ),
                Err(e) => push(
                    &mut summary,
                    &format!("uv python install {version}"),
                    false,
                    &e.to_string(),
                ),
            }
            if !summary.ok {
                return Ok(summary);
            }

            // create_venv passes --clear, so an existing environment is replaced.
            match uv::create_venv(&uv_info, version, &venv_path, workspace).await {
                Ok(out) if out.success() => push(
                    &mut summary,
                    "uv venv",
                    true,
                    &venv_path.display().to_string(),
                ),
                Ok(out) => push(&mut summary, "uv venv", false, out.stderr.trim()),
                Err(e) => push(&mut summary, "uv venv", false, &e.to_string()),
            }
            if !summary.ok {
                return Ok(summary);
            }

            // Restore the declared dependencies, then add the new one.
            let requirements = workspace.join("requirements.txt");
            if workspace.join("pyproject.toml").is_file() {
                // Without this, `uv sync` follows the project's old pin and
                // rebuilds .venv on the Python it was just moved away from.
                setup::pin_python(&mut summary, &uv_info, version, workspace).await;
                match uv::sync(&uv_info, workspace).await {
                    Ok(out) if out.success() => push(
                        &mut summary,
                        "uv sync",
                        true,
                        "existing dependencies restored",
                    ),
                    Ok(out) => push(&mut summary, "uv sync", false, out.stderr.trim()),
                    Err(e) => push(&mut summary, "uv sync", false, &e.to_string()),
                }
            } else if requirements.is_file() {
                match uv::pip_install_requirements(&uv_info, &requirements, workspace).await {
                    Ok(out) if out.success() => push(
                        &mut summary,
                        "reinstall requirements.txt",
                        true,
                        "existing dependencies restored",
                    ),
                    Ok(out) => push(
                        &mut summary,
                        "reinstall requirements.txt",
                        false,
                        out.stderr.trim(),
                    ),
                    Err(e) => push(
                        &mut summary,
                        "reinstall requirements.txt",
                        false,
                        &e.to_string(),
                    ),
                }
            }
            if !summary.ok {
                return Ok(summary);
            }
        }
        ResolvedMode::UvPipCompat => {
            let uv_info = match uv::ensure(settings).await {
                Ok(info) => info,
                Err(e) => {
                    push(&mut summary, "ensure uv", false, &e.to_string());
                    return Ok(summary);
                }
            };

            match uv::python_install(&uv_info, version, workspace).await {
                Ok(out) if out.success() => push(
                    &mut summary,
                    &format!("uv python install {version}"),
                    true,
                    "ready",
                ),
                Ok(out) => push(
                    &mut summary,
                    &format!("uv python install {version}"),
                    false,
                    out.stderr.trim(),
                ),
                Err(e) => push(
                    &mut summary,
                    &format!("uv python install {version}"),
                    false,
                    &e.to_string(),
                ),
            }
            if !summary.ok {
                return Ok(summary);
            }

            match uv::create_venv(&uv_info, version, &venv_path, workspace).await {
                Ok(out) if out.success() => push(
                    &mut summary,
                    "uv venv",
                    true,
                    &venv_path.display().to_string(),
                ),
                Ok(out) => push(&mut summary, "uv venv", false, out.stderr.trim()),
                Err(e) => push(&mut summary, "uv venv", false, &e.to_string()),
            }
            if !summary.ok {
                return Ok(summary);
            }

            // In pip-compatible mode, restore dependencies from requirements.txt only
            let requirements = workspace.join("requirements.txt");
            if requirements.is_file() {
                match uv::pip_install_requirements(&uv_info, &requirements, workspace).await {
                    Ok(out) if out.success() => {
                        push(
                            &mut summary,
                            "reinstall requirements.txt",
                            true,
                            "existing dependencies restored",
                        );
                        freeze_into_requirements_uv(&uv_info, workspace, &mut summary).await;
                    }
                    Ok(out) => push(
                        &mut summary,
                        "reinstall requirements.txt",
                        false,
                        out.stderr.trim(),
                    ),
                    Err(e) => push(
                        &mut summary,
                        "reinstall requirements.txt",
                        false,
                        &e.to_string(),
                    ),
                }
            }
            if !summary.ok {
                return Ok(summary);
            }
        }
        ResolvedMode::PurePip => {
            // pip mode cannot fetch interpreters, so the version must be present.
            let interpreters = crate::core::interpreter::discover().await;
            let Some(interp) = pip::select_interpreter(&interpreters, version) else {
                push(
                    &mut summary,
                    "select interpreter",
                    false,
                    &format!("Python {version} is not installed. Install it, then run this again. pip mode cannot fetch interpreters."),
                );
                return Ok(summary);
            };

            if exec::exists(&venv_path) {
                let effect = Effect::new(
                    "remove old venv",
                    format!("`python -m venv` cannot replace an environment; start Python {version}'s from scratch"),
                );
                if let Err(e) = exec::remove_dir_all(&venv_path, effect) {
                    push(&mut summary, "remove old venv", false, &e.to_string());
                    return Ok(summary);
                }
            }
            match pip::create_venv(interp, &venv_path, workspace).await {
                Ok(out) if out.success() => push(
                    &mut summary,
                    "python -m venv",
                    true,
                    &venv_path.display().to_string(),
                ),
                Ok(out) => push(&mut summary, "python -m venv", false, out.stderr.trim()),
                Err(e) => push(&mut summary, "python -m venv", false, &e.to_string()),
            }
            if !summary.ok {
                return Ok(summary);
            }

            let requirements = workspace.join("requirements.txt");
            if requirements.is_file() {
                match pip::install_requirements(&venv_path, &requirements, workspace).await {
                    Ok(out) if out.success() => {
                        push(
                            &mut summary,
                            "reinstall requirements.txt",
                            true,
                            "existing dependencies restored",
                        );
                        // Freeze immediately so the file tracks the exact
                        // versions installed into the new venv.
                        freeze_into_requirements(&venv_path, workspace, &mut summary).await;
                    }
                    Ok(out) => push(
                        &mut summary,
                        "reinstall requirements.txt",
                        false,
                        out.stderr.trim(),
                    ),
                    Err(e) => push(
                        &mut summary,
                        "reinstall requirements.txt",
                        false,
                        &e.to_string(),
                    ),
                }
            }
            if !summary.ok {
                return Ok(summary);
            }
        }
    }

    // Now the packages that started all this.
    let add = install_packages(workspace, settings, packages).await?;
    summary.steps.extend(add.steps);
    summary.ok = summary.ok && add.ok;
    Ok(summary)
}

/// The environment a flow in `mode` works on: pixi's default prefix in a
/// pixi project, `.venv` otherwise.
fn env_path(workspace: &Path, mode: ResolvedMode) -> std::path::PathBuf {
    match mode {
        ResolvedMode::Pixi => pixi_manage::default_env_path(workspace),
        _ => workspace.join(".venv"),
    }
}

/// Append a package to requirements.txt, creating it if needed, skipping it if
/// the name is already listed.
///
/// Retained for reference and potential future use (e.g. uv-less fallbacks).
/// The live install paths now use [`freeze_into_requirements`] instead, which
/// produces a fully-pinned snapshot from `pip freeze`.
#[allow(dead_code)]
fn record_in_requirements(workspace: &Path, package: &str, summary: &mut SetupSummary) {
    let path = workspace.join("requirements.txt");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();

    let already = existing
        .lines()
        .filter_map(crate::core::project::requirement_name)
        .any(|n| n == crate::core::project::normalize_name(package));
    if already {
        return;
    }

    let mut text = existing;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(package);
    text.push('\n');

    match std::fs::write(&path, text) {
        Ok(()) => push(summary, "record in requirements.txt", true, package),
        Err(e) => push(summary, "record in requirements.txt", false, &e.to_string()),
    }
}

fn push(summary: &mut SetupSummary, name: &str, ok: bool, detail: &str) {
    if !ok {
        summary.ok = false;
    }
    summary.steps.push(Step {
        name: name.to_string(),
        ok,
        detail: detail.to_string(),
    });
}

/// Run `pip freeze > requirements.txt` inside `venv` and record the step.
/// Called after every successful pip install in pip mode so `requirements.txt`
/// always reflects the exact, pinned state of the venv.
async fn freeze_into_requirements(venv: &Path, workspace: &Path, summary: &mut SetupSummary) {
    match pip::freeze_requirements(venv, workspace).await {
        Ok(out) if out.success() => push(
            summary,
            "pip freeze > requirements.txt",
            true,
            "requirements.txt updated with exact pins",
        ),
        Ok(out) => push(
            summary,
            "pip freeze > requirements.txt",
            false,
            out.stderr.trim(),
        ),
        Err(e) => push(
            summary,
            "pip freeze > requirements.txt",
            false,
            &e.to_string(),
        ),
    }
}

/// Run `uv pip freeze > requirements.txt` and record the step.
async fn freeze_into_requirements_uv(
    uv: &uv::UvInfo,
    workspace: &Path,
    summary: &mut SetupSummary,
) {
    match uv::freeze_into_requirements(uv, workspace).await {
        Ok(out) if out.success() => push(
            summary,
            "uv pip freeze > requirements.txt",
            true,
            "requirements.txt updated with exact pins",
        ),
        Ok(out) => push(
            summary,
            "uv pip freeze > requirements.txt",
            false,
            out.stderr.trim(),
        ),
        Err(e) => push(
            summary,
            "uv pip freeze > requirements.txt",
            false,
            &e.to_string(),
        ),
    }
}

/// Guard used by the CLI surface: refuse an empty package name rather than
/// shelling out with one.
pub fn validate_package_name(name: &str) -> crate::Result<()> {
    if name.trim().is_empty() {
        bail!("no package name given");
    }
    if name.starts_with('-') {
        bail!("`{name}` looks like a flag, not a package name");
    }
    Ok(())
}

/// Validate and dedupe the package names one command asked for.
///
/// Duplicates are dropped by their PEP 503 name, keeping the first spelling, so
/// `PyYAML` and `pyyaml` from two diagnostics become one install. On failure the
/// error carries the first rejected name, for the caller to show.
pub fn validate_package_names(names: &[String]) -> Result<Vec<String>, String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        if validate_package_name(name).is_err() {
            return Err(name.clone());
        }
        if seen.insert(crate::core::project::normalize_name(name)) {
            out.push(name.clone());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::PackageManager;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn package_lists_are_validated_and_deduped() {
        assert_eq!(
            validate_package_names(&names(&["numpy", "PyYAML", "pyyaml", "numpy"])),
            Ok(names(&["numpy", "PyYAML"]))
        );
        assert_eq!(
            validate_package_names(&names(&["numpy", "--index-url", "pandas"])),
            Err("--index-url".to_string())
        );
        assert_eq!(
            validate_package_names(&names(&["numpy", " "])),
            Err(" ".to_string())
        );
    }

    #[test]
    fn gpu_index_packages_get_their_own_invocation() {
        let cuda = "https://download.pytorch.org/whl/cu124".to_string();
        let batches = batch_by_index(vec![
            ("torch".into(), Some(cuda.clone())),
            ("numpy".into(), None),
            ("torchaudio".into(), Some(cuda.clone())),
            ("pandas".into(), None),
        ]);
        assert_eq!(
            batches,
            vec![
                Batch {
                    index_url: None,
                    packages: names(&["numpy", "pandas"]),
                },
                Batch {
                    index_url: Some(cuda),
                    packages: names(&["torch", "torchaudio"]),
                },
            ]
        );
    }

    #[test]
    fn distinct_indexes_never_share_an_invocation() {
        let batches = batch_by_index(vec![
            ("torch".into(), Some("https://a.example/cu124".into())),
            ("tensorflow".into(), Some("https://b.example/cpu".into())),
        ]);
        assert_eq!(batches.len(), 2);
        assert!(batches.iter().all(|b| b.packages.len() == 1));
    }

    #[test]
    fn a_single_package_is_a_single_invocation() {
        assert_eq!(
            batch_by_index(vec![("numpy".into(), None)]),
            vec![Batch {
                index_url: None,
                packages: names(&["numpy"]),
            }]
        );
        assert!(batch_by_index(Vec::new()).is_empty());
    }

    #[test]
    fn rejects_names_that_would_become_flags() {
        assert!(validate_package_name("mediapipe").is_ok());
        assert!(validate_package_name("").is_err());
        assert!(validate_package_name("   ").is_err());
        assert!(validate_package_name("--upgrade").is_err());
    }

    #[test]
    fn requirements_recording_is_idempotent() {
        let dir = std::env::temp_dir().join("pypilot-req-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("requirements.txt"), "numpy\n").unwrap();

        let mut summary = SetupSummary {
            package_manager: PackageManager::Uv,
            resolved_mode: ResolvedMode::UvFull,
            python: None,
            venv_path: dir.join(".venv"),
            why: String::new(),
            steps: Vec::new(),
            ok: true,
        };

        record_in_requirements(&dir, "mediapipe", &mut summary);
        record_in_requirements(&dir, "mediapipe", &mut summary);

        let text = std::fs::read_to_string(dir.join("requirements.txt")).unwrap();
        assert_eq!(text.matches("mediapipe").count(), 1);
        assert!(text.contains("numpy"));
    }
}
