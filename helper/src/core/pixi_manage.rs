//! Changing a pixi project, only ever through pixi.
//!
//! pixi owns its project: the manifest, `pixi.lock` and the prefixes under
//! `.pixi/envs`. A `.venv`, `uv.lock`, a `[tool.uv]` table or a frozen
//! `requirements.txt` next to it would be a second, competing environment
//! that pixi neither knows about nor keeps in sync. So in a pixi project
//! every mutating flow (setup, install, rebuild, the notebook kernel) lands
//! here and runs the pixi command instead, and when pixi itself is not
//! installed nothing is changed at all: the step says so and the run fails.
//!
//! PyPI is the default source for packages PyPilot adds (`pixi add --pypi`):
//! the names PyPilot offers come from imports and PyPI metadata, and the same
//! name on conda-forge can be a different package or not exist. The Python
//! interpreter and ipykernel are the exceptions, added as conda packages
//! because that is where pixi expects them.

use std::path::{Path, PathBuf};

use crate::core::command;
use crate::core::installed;
use crate::core::pixi;
use crate::core::pixi_compat;
use crate::core::setup::{SetupSummary, Step};
use crate::pypi::pyversion::PyVersion;

/// What a step says when pixi is not installed. Nothing is changed then:
/// falling back to uv or pip would build an environment pixi does not know.
pub const MISSING_PIXI: &str =
    "This is a pixi project; install pixi (https://pixi.sh) to let PyPilot manage it. Nothing was changed: PyPilot does not set up pixi projects with uv or pip.";

/// Step name shared by every pixi flow when pixi is missing.
pub const LOCATE_STEP: &str = "locate pixi";

/// `pixi install --manifest-path <root>`, plus `--environment <name>` when
/// the project's environment is not `default` (see [`selected_env`]).
pub fn install_args(root: &Path) -> Vec<String> {
    let mut args = vec!["install".to_string()];
    args.extend(manifest_args(root));
    let env = selected_env(root);
    if env != pixi::DEFAULT_ENV {
        args.extend(["--environment".to_string(), env]);
    }
    args
}

/// The pixi environment PyPilot treats as the project's
/// ([`crate::core::pixi_envs::selected`]); `default` outside a pixi project.
pub fn selected_env(root: &Path) -> String {
    pixi::detect(root).map_or_else(
        || pixi::DEFAULT_ENV.to_string(),
        |p| crate::core::pixi_envs::selected(&p, root),
    )
}

/// `pixi add --manifest-path <root> --pypi <packages...>`.
pub fn add_pypi_args(root: &Path, packages: &[String]) -> Vec<String> {
    let mut args = vec!["add".to_string()];
    args.extend(manifest_args(root));
    args.push("--pypi".to_string());
    args.extend(packages.iter().cloned());
    args
}

/// `pixi add --manifest-path <root> <conda specs...>`.
pub fn add_conda_args(root: &Path, specs: &[String]) -> Vec<String> {
    let mut args = vec!["add".to_string()];
    args.extend(manifest_args(root));
    args.extend(specs.iter().cloned());
    args
}

/// The conda spec that moves a project to Python `version`: `python=3.12`,
/// which pixi records as `python = "3.12.*"`.
pub fn python_spec(version: PyVersion) -> String {
    format!("python={version}")
}

fn manifest_args(root: &Path) -> [String; 2] {
    [
        "--manifest-path".to_string(),
        root.to_string_lossy().into_owned(),
    ]
}

/// Where `pixi install` puts the project's environment: `default`, unless
/// another one was selected.
pub fn default_env_path(root: &Path) -> PathBuf {
    root.join(".pixi").join("envs").join(selected_env(root))
}

/// Install the project's default environment (`pixi install`).
pub async fn install(pixi: Option<&Path>, root: &Path, summary: &mut SetupSummary) {
    let Some(pixi) = located(pixi, summary) else {
        return;
    };
    let result = run(pixi, &install_args(root), root).await;
    record(
        summary,
        "pixi install",
        result.map(|_| "environment installed".into()),
    );
}

/// Add PyPI packages (`pixi add --pypi`), which records them in the manifest
/// and installs them. Nothing to do for an empty list.
pub async fn add_pypi(
    pixi: Option<&Path>,
    root: &Path,
    packages: &[String],
    summary: &mut SetupSummary,
) {
    if packages.is_empty() {
        return;
    }
    let Some(pixi) = located(pixi, summary) else {
        return;
    };
    let step = format!("pixi add --pypi {}", packages.join(" "));
    let result = run(pixi, &add_pypi_args(root, packages), root).await;
    record(
        summary,
        &step,
        result.map(|_| "added to the pixi manifest and installed".into()),
    );
}

/// Move the project to Python `version`: `pixi add python=X.Y`, then
/// `pixi install` so the environment is rebuilt on it.
pub async fn set_python(
    pixi: Option<&Path>,
    root: &Path,
    version: PyVersion,
    summary: &mut SetupSummary,
) {
    let Some(pixi) = located(pixi, summary) else {
        return;
    };
    let spec = python_spec(version);
    let result = run(
        pixi,
        &add_conda_args(root, std::slice::from_ref(&spec)),
        root,
    )
    .await;
    record(
        summary,
        &format!("pixi add {spec}"),
        result.map(|_| format!("the manifest now asks for Python {version}")),
    );
    if summary.ok {
        install(Some(pixi), root, summary).await;
    }
}

/// Add ipykernel as a conda package, within the range PyPilot trusts (see
/// [`installed::IPYKERNEL_REQUIREMENT`]). Non-fatal, like the venv flavour.
pub async fn add_ipykernel(pixi: Option<&Path>, root: &Path, summary: &mut SetupSummary) {
    let step = crate::core::setup::IPYKERNEL_STEP;
    let Some(pixi) = pixi else {
        summary.steps.push(failed_step(step, MISSING_PIXI));
        return;
    };
    let spec = installed::IPYKERNEL_REQUIREMENT.to_string();
    let detail = match run(pixi, &add_conda_args(root, &[spec]), root).await {
        Ok(_) => Step {
            name: step.to_string(),
            ok: true,
            detail: "added to the pixi manifest".into(),
        },
        Err(e) => failed_step(
            step,
            &format!("{e} Notebooks will not see this environment until ipykernel is installed."),
        ),
    };
    summary.steps.push(detail);
}

/// `pixi` when it was found, else a failed step with [`MISSING_PIXI`].
fn located<'a>(pixi: Option<&'a Path>, summary: &mut SetupSummary) -> Option<&'a Path> {
    if pixi.is_none() {
        summary.ok = false;
        summary.steps.push(failed_step(LOCATE_STEP, MISSING_PIXI));
    }
    pixi
}

async fn run(pixi: &Path, args: &[String], root: &Path) -> Result<(), String> {
    match command::run(pixi, args, Some(root)).await {
        Ok(out) if out.success() => Ok(()),
        Ok(out) => Err(explain_failure(&out.stderr, root)),
        Err(e) => Err(e.to_string()),
    }
}

fn record(summary: &mut SetupSummary, name: &str, result: Result<String, String>) {
    match result {
        Ok(detail) => summary.steps.push(Step {
            name: name.to_string(),
            ok: true,
            detail,
        }),
        Err(detail) => {
            summary.ok = false;
            summary.steps.push(failed_step(name, &detail));
        }
    }
}

fn failed_step(name: &str, detail: &str) -> Step {
    Step {
        name: name.to_string(),
        ok: false,
        detail: detail.to_string(),
    }
}

/// One paragraph for a failed pixi command run on the project at `root`:
/// what went wrong and what to run next, worded as doctor words it
/// ([`pixi_compat::explain_pixi_error`]).
pub fn explain_failure(stderr: &str, root: &Path) -> String {
    pixi_compat::explain_pixi_error(stderr, pixi::detect(root).as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{PackageManager, ResolvedMode};

    fn summary(root: &Path) -> SetupSummary {
        SetupSummary {
            package_manager: PackageManager::Auto,
            resolved_mode: ResolvedMode::Pixi,
            python: None,
            venv_path: default_env_path(root),
            why: String::new(),
            steps: Vec::new(),
            ok: true,
        }
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn command_lines() {
        let root = Path::new("proj");
        assert_eq!(
            install_args(root),
            strings(&["install", "--manifest-path", "proj"])
        );
        assert_eq!(
            add_pypi_args(root, &strings(&["numpy", "mediapipe==0.10.14"])),
            strings(&[
                "add",
                "--manifest-path",
                "proj",
                "--pypi",
                "numpy",
                "mediapipe==0.10.14"
            ])
        );
        assert_eq!(
            add_conda_args(root, &[python_spec(PyVersion::py3(12))]),
            strings(&["add", "--manifest-path", "proj", "python=3.12"])
        );
        assert_eq!(
            add_conda_args(root, &[installed::IPYKERNEL_REQUIREMENT.to_string()]),
            strings(&["add", "--manifest-path", "proj", "ipykernel>=6.31.0,<7"])
        );
    }

    #[tokio::test]
    async fn without_pixi_every_flow_stops_and_says_why() {
        let root = Path::new("never-touched");
        let mut s = summary(root);
        install(None, root, &mut s).await;
        assert!(!s.ok);
        assert_eq!(s.steps.len(), 1);
        assert_eq!(s.steps[0].name, LOCATE_STEP);
        assert_eq!(s.steps[0].detail, MISSING_PIXI);

        let mut s = summary(root);
        add_pypi(None, root, &strings(&["numpy"]), &mut s).await;
        assert!(!s.ok && s.steps[0].detail == MISSING_PIXI);

        let mut s = summary(root);
        set_python(None, root, PyVersion::py3(11), &mut s).await;
        assert!(!s.ok);
        assert_eq!(s.steps.len(), 1);

        // Optional extras fail softly.
        let mut s = summary(root);
        add_ipykernel(None, root, &mut s).await;
        assert!(s.ok && !s.steps[0].ok);

        // Nothing asked, nothing to complain about.
        let mut s = summary(root);
        add_pypi(None, root, &[], &mut s).await;
        assert!(s.ok && s.steps.is_empty());
    }

    #[test]
    fn failures_are_explained_as_doctor_explains_them() {
        let root = Path::new("no-pixi-project-here");
        for stderr in [
            include_str!("../../tests/fixtures/pixi/pixi-0.81-err-no-wheel.txt"),
            include_str!("../../tests/fixtures/pixi/pixi-0.81-err-not-found.txt"),
            include_str!("../../tests/fixtures/pixi/pixi-0.81-err-conda-conflict.txt"),
            "Error: network unreachable",
        ] {
            assert_eq!(
                explain_failure(stderr, root),
                pixi_compat::explain_pixi_error(stderr, None)
            );
        }
        let no_wheel = include_str!("../../tests/fixtures/pixi/pixi-0.81-err-no-wheel.txt");
        assert!(explain_failure(no_wheel, root).contains("`pixi add python=3.11`"));
    }
}
