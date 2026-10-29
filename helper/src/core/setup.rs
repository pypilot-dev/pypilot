//! F1 — environment bootstrap. The one flow behind both `pypilot setup` and the
//! LSP "Fix everything" command, so the two can never drift.
//!
//! uv mode (default): ensure uv → `uv python install X.Y` → `uv venv --python X.Y`
//! → `uv sync` (pyproject) or `uv pip install -r requirements.txt`.
//!
//! pip mode: pick an already-installed interpreter for X.Y → `python -m venv` →
//! `pip install`. If the required Python isn't installed, we stop with a clear
//! "install Python X.Y" message (pip mode can't fetch interpreters).
//!
//! pixi projects: `pixi install`, and nothing else (see
//! [`crate::core::pixi_manage`]). No `.venv`, no uv, no pip.

use std::path::{Path, PathBuf};

use crate::core::command::Output;
use crate::core::exec::{self, Effect};
use crate::core::solver;
use crate::core::{env, installed, pip, pixi, pixi_manage, project, uv};
use crate::pypi::pyversion::PyVersion;
use crate::pypi::MetadataSource;
use crate::settings::{PackageManager, ResolvedMode, Settings};

/// One executed step, for a human-readable summary.
///
/// A step with `ok == false` usually means the whole run failed, but not
/// always: optional extras (see [`ensure_ipykernel`]) record their failure here
/// so the user sees it, while leaving [`SetupSummary::ok`] alone because what
/// the user actually asked for succeeded.
#[derive(Debug, Clone)]
pub struct Step {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

/// Result of a bootstrap run.
#[derive(Debug, Clone)]
pub struct SetupSummary {
    pub package_manager: PackageManager,
    pub resolved_mode: ResolvedMode,
    pub python: Option<PyVersion>,
    pub venv_path: PathBuf,
    pub why: String,
    pub steps: Vec<Step>,
    /// Overall success (all critical steps ok). Non-fatal steps may fail
    /// without clearing this.
    pub ok: bool,
}

impl SetupSummary {
    fn failed(&self) -> bool {
        !self.ok
    }
}

/// Run the full bootstrap for `workspace`, honoring `package_manager` and resolved mode.
pub async fn run<S: MetadataSource>(
    workspace: &Path,
    settings: &Settings,
    source: &S,
) -> crate::Result<SetupSummary> {
    let mode = settings.resolve_mode(workspace).await;
    run_with_mode(workspace, settings, source, mode).await
}

/// Run bootstrap for `workspace` with an explicit resolved mode (e.g. from user prompt).
pub async fn run_with_mode<S: MetadataSource>(
    workspace: &Path,
    settings: &Settings,
    source: &S,
    mode: ResolvedMode,
) -> crate::Result<SetupSummary> {
    // Whatever mode the caller picked, a pixi project is set up by pixi.
    let mode = if pixi::detect(workspace).is_some() {
        ResolvedMode::Pixi
    } else {
        mode
    };
    let assessment = solver::assess(workspace, settings, source).await;

    if mode == ResolvedMode::Pixi {
        let mut summary = SetupSummary {
            package_manager: settings.package_manager,
            resolved_mode: mode,
            python: None,
            venv_path: pixi_manage::default_env_path(workspace),
            why: "pixi project: the environment is installed by pixi from its manifest."
                .to_string(),
            steps: Vec::new(),
            ok: true,
        };
        let pixi_bin = pixi::locate().await;
        // `package_manager = "pixi"` on a project pixi does not manage yet:
        // create its manifest from the declared dependencies first.
        if pixi::detect(workspace).is_none()
            && !crate::core::pixi_migrate::create_for_setup(
                workspace,
                settings,
                source,
                pixi_bin.as_deref(),
                &mut summary,
            )
            .await
        {
            return Ok(summary);
        }
        run_pixi(workspace, pixi_bin.as_deref(), &mut summary).await;
        if assessment
            .project
            .packages
            .iter()
            .any(|r| r.name == "torch")
        {
            verify_gpu(workspace, &mut summary).await;
        }
        return Ok(summary);
    }

    let venv_path = workspace.join(".venv");
    let mut summary = SetupSummary {
        package_manager: settings.package_manager,
        resolved_mode: mode,
        python: assessment.target_python,
        venv_path: venv_path.clone(),
        why: derive_why(&assessment),
        steps: Vec::new(),
        ok: true,
    };

    // Refuse when dependencies fundamentally conflict — there's no interpreter to
    // pick, so installing anything would just fail confusingly.
    if let Some(compat) = &assessment.compat {
        if compat.intersection.is_empty() && !compat.per_package.is_empty() {
            summary.ok = false;
            summary.steps.push(Step {
                name: "resolve Python version".into(),
                ok: false,
                detail: compat
                    .conflicts
                    .first()
                    .map(|c| c.explanation.clone())
                    .unwrap_or_else(|| "dependencies have no common supported Python".into()),
            });
            return Ok(summary);
        }
    }

    // uv mode only builds a venv once it knows which Python to use, so a
    // broken venv in a project with no version constraints would otherwise be
    // left in place for `uv sync` to trip over. Rebuild it on the version it
    // was made with (uv fetches it if it is gone), else the newest one here.
    if summary.python.is_none() && mode != ResolvedMode::PurePip {
        if let Some(venv) = broken_venv(&assessment, &venv_path) {
            summary.python = venv
                .python
                .or_else(|| assessment.probes.interpreters.first().map(|i| i.version));
        }
    }

    match mode {
        ResolvedMode::UvFull => {
            run_uv_full(workspace, settings, &assessment, &venv_path, &mut summary).await
        }
        ResolvedMode::UvPipCompat => {
            run_uv_pip_compat(workspace, settings, &assessment, &venv_path, &mut summary).await
        }
        ResolvedMode::PurePip => run_pip(workspace, &assessment, &venv_path, &mut summary).await,
        // Returned early above.
        ResolvedMode::Pixi => {}
    }

    ensure_ipykernel(workspace, settings, mode, &mut summary).await;
    if assessment
        .project
        .packages
        .iter()
        .any(|r| r.name == "torch")
    {
        verify_gpu(workspace, &mut summary).await;
    }
    Ok(summary)
}

/// Set up a pixi project with the pixi binary at `pixi`: `pixi install`, then
/// ipykernel when notebooks need it. With no pixi, a failed step that says so,
/// and nothing on disk changes.
pub(crate) async fn run_pixi(workspace: &Path, pixi: Option<&Path>, summary: &mut SetupSummary) {
    pixi_manage::install(pixi, workspace, summary).await;
    if let Some(env) = env::project_env(workspace) {
        summary.python = env.python;
        summary.venv_path = env.path;
    }
    ensure_pixi_ipykernel(workspace, pixi, summary).await;
}

/// [`ensure_ipykernel`] for a pixi project, with the pixi binary at `pixi`.
async fn ensure_pixi_ipykernel(workspace: &Path, pixi: Option<&Path>, summary: &mut SetupSummary) {
    if summary.failed() || !project::has_notebooks(workspace) {
        return;
    }
    // A dry run that planned `pixi install` has no environment to look in
    // yet; plan the kernel as a fresh environment would need it.
    if !exec::is_fresh(&pixi_manage::default_env_path(workspace)) {
        let Some(env) = env::project_env(workspace) else {
            return;
        };
        if installed::has_usable_ipykernel(&installed::scan(&env.path), env.python) {
            return;
        }
    }
    pixi_manage::add_ipykernel(pixi, workspace, summary).await;
}

/// Step name for the post-install torch GPU self-test.
const GPU_CHECK_STEP: &str = "GPU check";

/// Confirm the torch PyPilot just installed can actually use the NVIDIA GPU.
///
/// Called right after an install that included torch, which is the moment a
/// CPU build or an unsupported CUDA build is cheapest to notice. Non-fatal:
/// torch did install, so a mismatch is recorded as a failed step that says
/// why, without clearing [`SetupSummary::ok`]. Skipped when the run already
/// failed, when there is no environment or torch in it, and on machines without
/// an NVIDIA GPU.
pub(crate) async fn verify_gpu(workspace: &Path, summary: &mut SetupSummary) {
    if summary.failed() {
        return;
    }
    // The self-test imports torch from the environment, which a dry run has
    // not built. It only reads, so the plan just says it will happen.
    if exec::note(Effect::new(
        GPU_CHECK_STEP,
        "after the install, import torch in the environment and confirm it can use the NVIDIA GPU (skipped on machines without one)",
    )) {
        return;
    }
    let Some(env) = crate::core::env::project_env(workspace) else {
        return;
    };
    if !installed::scan(&env.path).contains("torch") {
        return;
    }
    match crate::core::torch_gpu::self_test(&env.python_exe, workspace).await {
        Some(Ok(detail)) => step_ok(summary, GPU_CHECK_STEP, &detail),
        Some(Err(why)) => soft_fail(summary, GPU_CHECK_STEP, &why),
        None => {}
    }
}

/// Step name for the notebook kernel install, shared by setup and install.
pub(crate) const IPYKERNEL_STEP: &str = "ipykernel (for notebooks)";

/// Make the project's `.venv` usable as a Jupyter kernel when the project has
/// notebooks.
///
/// VS Code and Jupyter only offer an environment as a kernel when `ipykernel`
/// is installed in it; otherwise the notebook silently runs on some other
/// Python, where none of the packages PyPilot just installed exist. Called at
/// the end of every flow that builds or changes the environment.
///
/// Non-fatal by design: the user's own install already succeeded, so a failure
/// here is recorded as a failed step without clearing [`SetupSummary::ok`].
/// Skipped when the run already failed, when there is no `.venv`, when the
/// project has no notebooks, or when a usable `ipykernel` is already installed
/// (see [`installed::has_usable_ipykernel`]).
pub(crate) async fn ensure_ipykernel(
    workspace: &Path,
    settings: &Settings,
    mode: ResolvedMode,
    summary: &mut SetupSummary,
) {
    if mode == ResolvedMode::Pixi {
        let pixi = pixi::locate().await;
        ensure_pixi_ipykernel(workspace, pixi.as_deref(), summary).await;
        return;
    }
    let venv = workspace.join(".venv");
    // In a dry run, a venv the plan (re)creates is treated as empty.
    let has_ipykernel = !exec::is_fresh(&venv)
        && installed::has_usable_ipykernel(&installed::scan(&venv), venv_python(&venv));
    if summary.failed()
        || !exec::exists(&venv)
        || has_ipykernel
        || !project::has_notebooks(workspace)
    {
        return;
    }
    // One argv element; the range also replaces an ipykernel that cannot
    // connect on this Python (see `installed::IPYKERNEL_REQUIREMENT`).
    let packages = vec![installed::IPYKERNEL_REQUIREMENT.to_string()];

    let result = match mode {
        ResolvedMode::UvFull | ResolvedMode::UvPipCompat => {
            let uv_info = match uv::ensure(settings).await {
                Ok(info) => info,
                Err(e) => {
                    soft_fail(summary, IPYKERNEL_STEP, &e.to_string());
                    return;
                }
            };
            // A dev dependency keeps it out of the runtime requirements. Without
            // a pyproject.toml (uv init was skipped), there is nowhere to record
            // a group, so fall back to a plain install into the venv.
            if mode == ResolvedMode::UvFull && exec::exists(&workspace.join("pyproject.toml")) {
                outcome(uv::add_dev(&uv_info, &packages, workspace).await)
                    .map(|_| "added to the dev dependency group".to_string())
            } else {
                match outcome(uv::pip_install(&uv_info, &packages, None, workspace).await) {
                    Ok(_) if mode == ResolvedMode::UvPipCompat => {
                        match outcome(uv::freeze_into_requirements(&uv_info, workspace).await)
                            .map(|_| ())
                        {
                            Ok(()) => Ok("installed; requirements.txt updated".to_string()),
                            Err(e) => Ok(format!(
                                "installed, but updating requirements.txt failed: {e}"
                            )),
                        }
                    }
                    Ok(_) => Ok("installed".to_string()),
                    Err(e) => Err(e),
                }
            }
        }
        // Handled above; never reached.
        ResolvedMode::Pixi => return,
        ResolvedMode::PurePip => {
            match outcome(pip::install_packages(&venv, &packages, None, workspace).await) {
                Ok(_) => match outcome(pip::freeze_requirements(&venv, workspace).await) {
                    Ok(_) => Ok("installed; requirements.txt updated".to_string()),
                    Err(e) => Ok(format!(
                        "installed, but updating requirements.txt failed: {e}"
                    )),
                },
                Err(e) => Err(e),
            }
        }
    };

    match result {
        Ok(detail) => step_ok(summary, IPYKERNEL_STEP, &detail),
        Err(e) => soft_fail(
            summary,
            IPYKERNEL_STEP,
            &format!("{e} (notebooks will not see this environment until ipykernel is installed)"),
        ),
    }
}

/// The probed venv when it is the one at `venv_path` and its Python is gone.
fn broken_venv<'a>(
    assessment: &'a crate::core::Assessment,
    venv_path: &Path,
) -> Option<&'a crate::core::probe::VenvInfo> {
    assessment
        .probes
        .venv
        .as_ref()
        .filter(|v| v.path == venv_path && v.broken.is_some())
}

/// The Python version a venv was created with, from its `pyvenv.cfg`.
fn venv_python(venv: &Path) -> Option<PyVersion> {
    let text = std::fs::read_to_string(venv.join("pyvenv.cfg")).ok()?;
    crate::core::probe::parse_pyvenv_version(&text)
}

/// Collapse a process result into its stdout on success or a one-line reason.
fn outcome(result: crate::Result<Output>) -> Result<String, String> {
    match result {
        Ok(out) if out.success() => Ok(out.stdout),
        Ok(out) => Err(out.stderr.trim().to_string()),
        Err(e) => Err(e.to_string()),
    }
}

async fn run_uv_full(
    workspace: &Path,
    settings: &Settings,
    assessment: &crate::core::Assessment,
    venv_path: &Path,
    summary: &mut SetupSummary,
) {
    let uv_info = match uv::ensure(settings).await {
        Ok(info) => {
            summary.steps.push(Step {
                name: "ensure uv".into(),
                ok: true,
                detail: format!(
                    "uv {} ({})",
                    info.version,
                    if info.managed { "managed" } else { "on PATH" }
                ),
            });
            info
        }
        Err(e) => {
            fail(summary, "ensure uv", &e.to_string());
            return;
        }
    };

    // Scaffold pyproject.toml when the project has none yet so that subsequent
    // `uv add` calls have a manifest to write into. Its `requires-python`
    // follows the Python chosen below, so uv keeps the venv built on it.
    // Existing pyproject.toml files are never touched.
    let has_pyproject = workspace.join("pyproject.toml").is_file();
    if !has_pyproject {
        match uv::init(&uv_info, summary.python, workspace).await {
            Ok(out) if out.success() => step_ok(summary, "uv init", "pyproject.toml created"),
            Ok(out) => {
                // Non-zero but not fatal: the directory may already have a
                // hello.py that uv init refuses to overwrite. Proceed and let
                // the downstream sync/add surface a clearer error if needed.
                step_ok(
                    summary,
                    "uv init",
                    &format!("(warning) {}", out.stderr.trim()),
                );
            }
            Err(e) => fail(summary, "uv init", &e.to_string()),
        }
        if summary.failed() {
            return;
        }
    }

    // Install the target Python if one was determined.
    if let Some(py) = summary.python {
        match uv::python_install(&uv_info, py, workspace).await {
            Ok(out) if out.success() => {
                step_ok(summary, &format!("uv python install {py}"), "ready")
            }
            Ok(out) => fail(
                summary,
                &format!("uv python install {py}"),
                out.stderr.trim(),
            ),
            Err(e) => fail(summary, &format!("uv python install {py}"), &e.to_string()),
        }
        if summary.failed() {
            return;
        }

        match uv::create_venv(&uv_info, py, venv_path, workspace).await {
            Ok(out) if out.success() => {
                step_ok(summary, "uv venv", &venv_path.display().to_string())
            }
            Ok(out) => fail(summary, "uv venv", out.stderr.trim()),
            Err(e) => fail(summary, "uv venv", &e.to_string()),
        }
        if summary.failed() {
            return;
        }
        // `uv init` above pinned a new project already.
        if has_pyproject {
            pin_python(summary, &uv_info, py, workspace).await;
        }
    }

    install_deps_uv(workspace, &uv_info, assessment, summary).await;
}

async fn install_deps_uv(
    workspace: &Path,
    uv_info: &uv::UvInfo,
    assessment: &crate::core::Assessment,
    summary: &mut SetupSummary,
) {
    // After run_uv may have called `uv init`, re-check for pyproject.toml.
    let has_pyproject = exec::exists(&workspace.join("pyproject.toml"));
    let requirements = workspace.join("requirements.txt");

    if has_pyproject && requirements.is_file() {
        // If requirements.txt exists and pyproject.toml exists (whether newly created by
        // `uv init` or pre-existing), add and resolve dependencies using `uv add -r requirements.txt`.
        match uv::add_requirements(uv_info, &requirements, None, workspace).await {
            Ok(out) if out.success() => step_ok(
                summary,
                "uv add -r requirements.txt",
                "dependencies added to pyproject.toml and installed",
            ),
            Ok(out) => fail(summary, "uv add -r requirements.txt", out.stderr.trim()),
            Err(e) => fail(summary, "uv add -r requirements.txt", &e.to_string()),
        }
    } else if has_pyproject {
        // Use `uv sync` which reads pyproject.toml / uv.lock and installs the
        // declared dependencies.
        match uv::sync(uv_info, workspace).await {
            Ok(out) if out.success() => step_ok(summary, "uv sync", "dependencies installed"),
            Ok(out) => fail(summary, "uv sync", out.stderr.trim()),
            Err(e) => fail(summary, "uv sync", &e.to_string()),
        }
    } else if requirements.is_file() {
        // No pyproject.toml (init must have been skipped or failed): fall back
        // to the plain requirements install.
        match uv::pip_install_requirements(uv_info, &requirements, workspace).await {
            Ok(out) if out.success() => {
                step_ok(summary, "uv pip install -r requirements.txt", "installed")
            }
            Ok(out) => fail(
                summary,
                "uv pip install -r requirements.txt",
                out.stderr.trim(),
            ),
            Err(e) => fail(
                summary,
                "uv pip install -r requirements.txt",
                &e.to_string(),
            ),
        }
    } else {
        step_ok(
            summary,
            "install dependencies",
            "no manifest to install from",
        );
    }
    let _ = assessment;
}

async fn run_uv_pip_compat(
    workspace: &Path,
    settings: &Settings,
    assessment: &crate::core::Assessment,
    venv_path: &Path,
    summary: &mut SetupSummary,
) {
    let uv_info = match uv::ensure(settings).await {
        Ok(info) => {
            summary.steps.push(Step {
                name: "ensure uv".into(),
                ok: true,
                detail: format!(
                    "uv {} ({})",
                    info.version,
                    if info.managed { "managed" } else { "on PATH" }
                ),
            });
            info
        }
        Err(e) => {
            fail(summary, "ensure uv", &e.to_string());
            return;
        }
    };

    // In pip-compatible mode: NEVER call uv init, NEVER create pyproject.toml / uv.lock.
    // Use uv venv + uv pip install + uv pip freeze into requirements.txt.
    let target_py = summary
        .python
        .or_else(|| assessment.probes.interpreters.first().map(|i| i.version));

    if let Some(py) = target_py {
        match uv::python_install(&uv_info, py, workspace).await {
            Ok(out) if out.success() => {
                step_ok(summary, &format!("uv python install {py}"), "ready")
            }
            Ok(out) => fail(
                summary,
                &format!("uv python install {py}"),
                out.stderr.trim(),
            ),
            Err(e) => fail(summary, &format!("uv python install {py}"), &e.to_string()),
        }
        if summary.failed() {
            return;
        }

        match uv::create_venv(&uv_info, py, venv_path, workspace).await {
            Ok(out) if out.success() => {
                step_ok(summary, "uv venv", &venv_path.display().to_string())
            }
            Ok(out) => fail(summary, "uv venv", out.stderr.trim()),
            Err(e) => fail(summary, "uv venv", &e.to_string()),
        }
        if summary.failed() {
            return;
        }
    }

    let requirements = workspace.join("requirements.txt");
    if requirements.is_file() {
        match uv::pip_install_requirements(&uv_info, &requirements, workspace).await {
            Ok(out) if out.success() => {
                step_ok(summary, "uv pip install -r requirements.txt", "installed");
                freeze_into_requirements_uv(&uv_info, workspace, summary).await;
            }
            Ok(out) => fail(
                summary,
                "uv pip install -r requirements.txt",
                out.stderr.trim(),
            ),
            Err(e) => fail(
                summary,
                "uv pip install -r requirements.txt",
                &e.to_string(),
            ),
        }
    } else if !assessment.project.packages.is_empty() {
        match uv::pip_install(&uv_info, &assessment.project.names(), None, workspace).await {
            Ok(out) if out.success() => {
                step_ok(summary, "uv pip install", "installed");
                freeze_into_requirements_uv(&uv_info, workspace, summary).await;
            }
            Ok(out) => fail(summary, "uv pip install", out.stderr.trim()),
            Err(e) => fail(summary, "uv pip install", &e.to_string()),
        }
    } else {
        step_ok(
            summary,
            "install dependencies",
            "no manifest to install from",
        );
    }
}

/// Run `uv pip freeze > requirements.txt` and record the step.
pub(crate) async fn freeze_into_requirements_uv(
    uv: &uv::UvInfo,
    workspace: &Path,
    summary: &mut SetupSummary,
) {
    match uv::freeze_into_requirements(uv, workspace).await {
        Ok(out) if out.success() => step_ok(
            summary,
            "uv pip freeze > requirements.txt",
            "requirements.txt updated with exact pins",
        ),
        Ok(out) => fail(
            summary,
            "uv pip freeze > requirements.txt",
            out.stderr.trim(),
        ),
        Err(e) => fail(summary, "uv pip freeze > requirements.txt", &e.to_string()),
    }
}

async fn run_pip(
    workspace: &Path,
    assessment: &crate::core::Assessment,
    venv_path: &Path,
    summary: &mut SetupSummary,
) {
    // pip mode requires the target Python to already exist on the machine.
    let target = summary.python;
    let interp = match target {
        Some(py) => match pip::select_interpreter(&assessment.probes.interpreters, py) {
            Some(i) => i.clone(),
            None => {
                fail(
                    summary,
                    "select interpreter",
                    &format!(
                        "Python {py} is required but not installed. Install it (e.g. from python.org or your package manager), then run setup again. pip mode cannot fetch interpreters."
                    ),
                );
                return;
            }
        },
        None => {
            // No dep-driven target: use the newest interpreter available.
            match assessment.probes.interpreters.first() {
                Some(i) => i.clone(),
                None => {
                    fail(
                        summary,
                        "select interpreter",
                        "no Python interpreter found on this machine",
                    );
                    return;
                }
            }
        }
    };
    step_ok(
        summary,
        "select interpreter",
        &format!("{} ({})", interp.command, interp.version),
    );

    // Unlike `uv venv --clear`, `python -m venv` reuses an existing directory,
    // keeping the dead interpreter's links and site-packages (and failing on a
    // dangling `bin/python`). Start a broken venv from scratch.
    if broken_venv(assessment, venv_path).is_some() {
        let effect = Effect::new(
            "remove broken venv",
            "its Python is gone, and `python -m venv` would keep the dead links",
        );
        match exec::remove_dir_all(venv_path, effect) {
            Ok(()) => step_ok(
                summary,
                "remove broken venv",
                &venv_path.display().to_string(),
            ),
            Err(e) => {
                fail(summary, "remove broken venv", &e.to_string());
                return;
            }
        }
    }

    match pip::create_venv(&interp, venv_path, workspace).await {
        Ok(out) if out.success() => {
            step_ok(summary, "python -m venv", &venv_path.display().to_string())
        }
        Ok(out) => fail(summary, "python -m venv", out.stderr.trim()),
        Err(e) => fail(summary, "python -m venv", &e.to_string()),
    }
    if summary.failed() {
        return;
    }

    let requirements = workspace.join("requirements.txt");
    if requirements.is_file() {
        match pip::install_requirements(venv_path, &requirements, workspace).await {
            Ok(out) if out.success() => {
                step_ok(summary, "pip install -r requirements.txt", "installed");
                // Freeze immediately: requirements.txt now reflects exact pins.
                freeze_into_requirements(venv_path, workspace, summary).await;
            }
            Ok(out) => fail(
                summary,
                "pip install -r requirements.txt",
                out.stderr.trim(),
            ),
            Err(e) => fail(summary, "pip install -r requirements.txt", &e.to_string()),
        }
    } else if !assessment.project.packages.is_empty() {
        match pip::install_packages(venv_path, &assessment.project.names(), None, workspace).await {
            Ok(out) if out.success() => {
                step_ok(summary, "pip install", "installed");
                // Freeze immediately: requirements.txt now reflects exact pins.
                freeze_into_requirements(venv_path, workspace, summary).await;
            }
            Ok(out) => fail(summary, "pip install", out.stderr.trim()),
            Err(e) => fail(summary, "pip install", &e.to_string()),
        }
    } else {
        step_ok(
            summary,
            "install dependencies",
            "no manifest to install from",
        );
    }
}

/// Run `pip freeze > requirements.txt` inside `venv` and record the step.
async fn freeze_into_requirements(venv: &Path, workspace: &Path, summary: &mut SetupSummary) {
    match pip::freeze_requirements(venv, workspace).await {
        Ok(out) if out.success() => step_ok(
            summary,
            "pip freeze > requirements.txt",
            "requirements.txt updated with exact pins",
        ),
        Ok(out) => fail(summary, "pip freeze > requirements.txt", out.stderr.trim()),
        Err(e) => fail(summary, "pip freeze > requirements.txt", &e.to_string()),
    }
}

pub(crate) fn derive_why(assessment: &crate::core::Assessment) -> String {
    match (&assessment.compat, assessment.target_python) {
        (Some(compat), Some(py)) => {
            let range = compat.intersection.to_range_string();
            let limiter = compat
                .per_package
                .iter()
                .filter(|p| p.supported.max() == Some(py))
                .min_by_key(|p| p.supported.len());
            match limiter {
                Some(p) => format!(
                    "Python {py} chosen: dependencies support {range}; {} caps the upper bound (supports {}).",
                    p.name,
                    p.supported.to_range_string()
                ),
                None => format!("Python {py} chosen: dependencies support {range}."),
            }
        }
        _ => "No dependency constraints; using an available interpreter.".to_string(),
    }
}

/// Pin a uv project to `py`, the Python its `.venv` was just built on, so
/// the `uv sync` / `uv add` that follow keep that environment instead of
/// rebuilding it on an older pin or a global `uv python pin`. A failure is
/// reported but does not stop the run.
pub(crate) async fn pin_python(
    summary: &mut SetupSummary,
    uv_info: &uv::UvInfo,
    py: PyVersion,
    workspace: &Path,
) {
    let name = format!("uv python pin {py}");
    match uv::pin(uv_info, py, workspace).await {
        Ok(out) if out.success() => step_ok(summary, &name, ".python-version updated"),
        Ok(out) => soft_fail(summary, &name, out.stderr.trim()),
        Err(e) => soft_fail(summary, &name, &e.to_string()),
    }
}

fn step_ok(summary: &mut SetupSummary, name: &str, detail: &str) {
    summary.steps.push(Step {
        name: name.to_string(),
        ok: true,
        detail: detail.to_string(),
    });
}

/// Record a failed step that must not fail the run: it is shown with the
/// other steps, but [`SetupSummary::ok`] keeps its value.
fn soft_fail(summary: &mut SetupSummary, name: &str, detail: &str) {
    summary.steps.push(Step {
        name: name.to_string(),
        ok: false,
        detail: detail.to_string(),
    });
}

fn fail(summary: &mut SetupSummary, name: &str, detail: &str) {
    summary.ok = false;
    summary.steps.push(Step {
        name: name.to_string(),
        ok: false,
        detail: detail.to_string(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::probe::{Probes, VenvInfo};
    use crate::core::Assessment;
    use crate::settings::PackageManager;

    fn assessment(venv: Option<VenvInfo>) -> Assessment {
        Assessment {
            probes: Probes {
                venv,
                ..Default::default()
            },
            project: Default::default(),
            compat: None,
            target_python: None,
            findings: Vec::new(),
        }
    }

    /// Every file and directory under `root`, relative and sorted.
    fn tree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        fn walk(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                let rel = path.strip_prefix(root).unwrap().to_path_buf();
                if path.is_dir() {
                    out.push((rel, Vec::new()));
                    walk(root, &path, out);
                } else {
                    out.push((rel, std::fs::read(&path).unwrap()));
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out.sort();
        out
    }

    /// The guard: in a pixi project with no pixi to run, setup, install,
    /// rebuild and the notebook kernel all stop with the pixi message and
    /// leave every file as it was. No `.venv`, no uv, no pip, even with the
    /// user's package manager set to uv.
    #[tokio::test]
    async fn pixi_project_without_pixi_is_left_untouched() {
        let root = std::env::temp_dir().join(format!("pypilot-pixi-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("pixi.toml"),
            include_str!("../../tests/fixtures/pixi/pixi-0.81-init.pixi.toml"),
        )
        .unwrap();
        std::fs::write(root.join("main.py"), "import requests\n").unwrap();
        std::fs::write(root.join("analysis.ipynb"), "{}").unwrap();
        std::fs::write(root.join("requirements.txt"), "requests\n").unwrap();
        let before = tree(&root);

        // Only this test asks for pixi, so pointing the override at a missing
        // binary makes "pixi is not installed" deterministic here.
        std::env::set_var(pixi::PIXI_OVERRIDE_ENV, root.join("no-such-pixi"));
        let settings = Settings {
            package_manager: PackageManager::Uv,
            data_refresh_days: 0,
            ..Settings::default()
        };
        let source = crate::pypi::FixtureSource::new();

        let summary = run(&root, &settings, &source).await.unwrap();
        assert_eq!(summary.resolved_mode, ResolvedMode::Pixi);
        assert!(!summary.ok);
        let failed: Vec<&Step> = summary.steps.iter().filter(|s| !s.ok).collect();
        assert_eq!(failed.len(), 1, "{:?}", summary.steps);
        assert_eq!(failed[0].name, pixi_manage::LOCATE_STEP);
        assert_eq!(failed[0].detail, pixi_manage::MISSING_PIXI);
        assert!(summary.steps.iter().all(|s| !s.name.contains("uv")
            && !s.name.contains("pip")
            && !s.name.contains("venv")));

        // An explicit uv mode from the editor's prompt is overridden too.
        let summary = run_with_mode(&root, &settings, &source, ResolvedMode::UvFull)
            .await
            .unwrap();
        assert_eq!(summary.resolved_mode, ResolvedMode::Pixi);

        let summary = crate::core::install::install_packages(&root, &settings, &["numpy".into()])
            .await
            .unwrap();
        assert!(!summary.ok);
        assert_eq!(
            summary.steps.last().unwrap().detail,
            pixi_manage::MISSING_PIXI
        );

        let summary = crate::core::install::recreate_with_python(
            &root,
            &settings,
            PyVersion::py3(11),
            &["numpy".into()],
        )
        .await
        .unwrap();
        assert!(!summary.ok);

        let mut summary = run(&root, &settings, &source).await.unwrap();
        summary.ok = true;
        ensure_ipykernel(&root, &settings, ResolvedMode::Pixi, &mut summary).await;

        for absent in [".venv", "uv.lock", "pyproject.toml", ".pixi"] {
            assert!(!root.join(absent).exists(), "{absent} was created");
        }
        assert_eq!(tree(&root), before, "the project changed on disk");
        std::env::remove_var(pixi::PIXI_OVERRIDE_ENV);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn only_the_rebuilt_venv_counts_as_broken() {
        let path = PathBuf::from("ws").join(".venv");
        let venv = |dir: &str, broken: Option<&str>| VenvInfo {
            path: PathBuf::from("ws").join(dir),
            python: Some(PyVersion::py3(12)),
            broken: broken.map(PathBuf::from),
            kind: Default::default(),
        };

        let a = assessment(Some(venv(".venv", Some("/gone/python3.12"))));
        assert!(broken_venv(&a, &path).is_some());

        // Healthy: setup leaves it to `uv venv --clear` / `python -m venv`.
        let a = assessment(Some(venv(".venv", None)));
        assert!(broken_venv(&a, &path).is_none());

        // A broken `venv/` is not the `.venv` setup builds, so never deleted.
        let a = assessment(Some(venv("venv", Some("/gone/python3.12"))));
        assert!(broken_venv(&a, &path).is_none());

        assert!(broken_venv(&assessment(None), &path).is_none());
    }
}
