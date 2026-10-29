//! pip-mode driver (F1 fallback).
//!
//! Same detection and compatibility logic as uv mode — only the install engine
//! differs. pip mode never downloads or invokes uv: it uses an interpreter that
//! already exists on the machine, `python -m venv`, and `pip install`. If the
//! required Python isn't installed, we can't fetch it (that's uv's job), so we
//! surface a clear "install Python X.Y manually" message instead.

use std::path::{Path, PathBuf};

use crate::core::command::Output;
use crate::core::exec::{self, Effect};
use crate::core::interpreter::{self, Interpreter};
use crate::pypi::pyversion::PyVersion;

/// The venv's interpreter path, given the venv root.
pub fn venv_python(venv: &Path) -> PathBuf {
    if cfg!(windows) {
        venv.join("Scripts").join("python.exe")
    } else {
        venv.join("bin").join("python")
    }
}

/// Create a venv at `venv_path` using an already-installed `interpreter`.
pub async fn create_venv(
    interpreter: &Interpreter,
    venv_path: &Path,
    cwd: &Path,
) -> crate::Result<Output> {
    let venv = venv_path.to_string_lossy().into_owned();
    let effect = Effect::new(
        "python -m venv",
        format!(
            "create the project environment with the installed Python {} ({})",
            interpreter.version, interpreter.command
        ),
    )
    .writes(venv_path);
    exec::run(
        &interpreter.command,
        &["-m", "venv", &venv],
        Some(cwd),
        effect,
    )
    .await
}

/// `python -m pip install -r requirements.txt` inside the venv.
pub async fn install_requirements(
    venv: &Path,
    requirements: &Path,
    cwd: &Path,
) -> crate::Result<Output> {
    let py = venv_python(venv);
    let req = requirements.to_string_lossy().into_owned();
    let effect = Effect::new(
        "pip install -r requirements.txt",
        "install requirements.txt into the venv",
    )
    .writes(venv);
    exec::run(
        &py,
        &["-m", "pip", "install", "-r", &req],
        Some(cwd),
        effect,
    )
    .await
}

/// `python -m pip install <pkgs...>` inside the venv.
///
/// `index_url` overrides the default index for this call only — used for a
/// torch/tensorflow install matched to the machine's GPU driver (see
/// [`crate::matrix::solve`]).
pub async fn install_packages(
    venv: &Path,
    packages: &[String],
    index_url: Option<&str>,
    cwd: &Path,
) -> crate::Result<Output> {
    let py = venv_python(venv);
    let mut args: Vec<String> = vec!["-m".into(), "pip".into(), "install".into()];
    if let Some(url) = index_url {
        args.push("--index-url".into());
        args.push(url.to_string());
    }
    args.extend(packages.iter().cloned());
    let why = match index_url {
        Some(url) => format!("install into the venv from {url}, the build matched to this machine"),
        None => "install into the venv".to_string(),
    };
    let effect = Effect::new(format!("pip install {}", packages.join(" ")), why).writes(venv);
    exec::run(&py, &args, Some(cwd), effect).await
}

/// Pick an installed interpreter for the target version, preferring an exact
/// minor match. Returns `None` when the required Python isn't on the machine —
/// the caller then tells the user to install it (pip mode can't fetch Pythons).
pub fn select_interpreter(interpreters: &[Interpreter], target: PyVersion) -> Option<&Interpreter> {
    interpreter::find_version(interpreters, target)
}

/// `pip freeze > requirements.txt` — snapshot the full, pinned venv state.
///
/// Called after every successful `pip install` so `requirements.txt` always
/// reflects the exact installed versions (e.g. `numpy==1.26.4` rather than
/// the bare name that was requested). The entire file is replaced because the
/// freeze output is already the authoritative list of everything in the venv.
///
/// Never fails silently: callers record this as a step in [`SetupSummary`] so
/// the user sees if the file write failed.
pub async fn freeze_requirements(
    venv: &Path,
    workspace: &Path,
) -> crate::Result<crate::core::command::Output> {
    let py = venv_python(venv);
    let req_path = workspace.join("requirements.txt");
    let effect = Effect::new(
        "pip freeze > requirements.txt",
        "replace requirements.txt with the exact versions now installed",
    )
    .writes(&req_path);
    let out = exec::run(&py, &["-m", "pip", "freeze"], Some(workspace), effect).await?;
    if out.success() && !exec::is_dry_run() {
        std::fs::write(&req_path, &out.stdout)
            .map_err(|e| anyhow::anyhow!("writing requirements.txt: {e}"))?;
    }
    Ok(out)
}
