//! Dry runs of the real setup, install and rebuild flows, offline: the plan
//! each mode produces, and that the project on disk is left byte-identical.

use std::path::{Path, PathBuf};

use super::*;
use crate::core::setup::{self, SetupSummary};
use crate::core::{installed, pixi_manage};
use crate::pypi::pyversion::PyVersion;
use crate::pypi::FixtureSource;
use crate::settings::{PackageManager, ResolvedMode, Settings};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

/// numpy 1.26.4 has cp39-cp312 wheels on every platform CI runs on, so the
/// solver picks Python 3.12 wherever the test runs.
fn source() -> FixtureSource {
    let mut src = FixtureSource::new();
    for key in ["numpy", "numpy==1.26.4"] {
        src.load_json(key, fixture("numpy-1.26.4.json")).unwrap();
    }
    src
}

fn settings(pm: PackageManager) -> Settings {
    Settings {
        package_manager: pm,
        data_refresh_days: 0,
        ..Settings::default()
    }
}

/// A fresh project directory holding `files`.
fn project(tag: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = std::env::temp_dir().join(format!("pypilot-dry-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    for (name, text) in files {
        std::fs::write(root.join(name), text).unwrap();
    }
    root
}

/// Every file and directory under `root`, relative and sorted, with contents.
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

/// Step names, without the uv download a machine without uv adds first.
fn names(steps: &[PlannedStep]) -> Vec<String> {
    steps
        .iter()
        .filter(|s| s.name != "download uv")
        .map(|s| s.name.clone())
        .collect()
}

fn step<'a>(steps: &'a [PlannedStep], name: &str) -> &'a PlannedStep {
    steps
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no step `{name}` in {steps:#?}"))
}

/// The arguments after the program, which differs from machine to machine.
fn args(step: &PlannedStep) -> Vec<&str> {
    step.command[1..].iter().map(String::as_str).collect()
}

fn assert_untouched(root: &Path, before: &[(PathBuf, Vec<u8>)]) {
    assert_eq!(tree(root), before, "the dry run changed the project");
    for absent in [
        ".venv",
        "pyproject.toml",
        "uv.lock",
        ".python-version",
        ".pixi",
    ] {
        let existed = before.iter().any(|(p, _)| p == Path::new(absent));
        assert_eq!(root.join(absent).exists(), existed, "{absent}");
    }
}

const REQUIREMENTS: &str = "numpy==1.26.4\n";

#[tokio::test]
async fn uv_project_mode_plans_init_python_venv_and_add() {
    let root = project("uv", &[("requirements.txt", REQUIREMENTS)]);
    let before = tree(&root);

    let (summary, steps) =
        dry_run(setup::run(&root, &settings(PackageManager::Uv), &source())).await;
    let summary = summary.unwrap();
    assert_eq!(summary.resolved_mode, ResolvedMode::UvFull);
    let py = summary
        .python
        .unwrap_or_else(|| panic!("no Python decided: {summary:#?}"));
    assert_eq!(py, PyVersion::py3(12));
    let plan = Plan::from_summary(&summary, steps.clone());
    assert!(plan.ok(), "{plan:#?}");
    assert_eq!(plan.mode, "uv");
    assert!(
        plan.why.starts_with(&format!("Python {py} chosen")),
        "{}",
        plan.why
    );

    assert_eq!(
        names(&steps),
        [
            "uv init".to_string(),
            format!("uv python install {py}"),
            "uv venv".to_string(),
            // The pyproject.toml `uv init` would create is what makes this
            // `uv add -r`, not `uv pip install -r`.
            "uv add -r requirements.txt".to_string(),
        ]
    );
    let py = py.to_string();
    assert_eq!(
        args(step(&steps, "uv init")),
        [
            "init",
            "--bare",
            "--no-workspace",
            "--pin-python",
            "--python",
            py.as_str()
        ]
    );
    assert_eq!(
        step(&steps, "uv init").writes,
        [root.join("pyproject.toml"), root.join(".python-version")]
    );
    let venv = root.join(".venv").to_string_lossy().into_owned();
    assert_eq!(
        args(step(&steps, "uv venv")),
        ["venv", "--clear", "--python", py.as_str(), venv.as_str()]
    );
    assert_eq!(
        args(step(&steps, "uv add -r requirements.txt")),
        ["add", "-r", "requirements.txt"]
    );
    assert_untouched(&root, &before);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn uv_pip_mode_plans_a_freeze_into_requirements() {
    let root = project("uvpip", &[("requirements.txt", REQUIREMENTS)]);
    let before = tree(&root);

    let (summary, steps) = dry_run(setup::run(
        &root,
        &settings(PackageManager::UvPip),
        &source(),
    ))
    .await;
    let summary = summary.unwrap();
    assert_eq!(summary.resolved_mode, ResolvedMode::UvPipCompat);
    let py = summary.python.unwrap();
    assert_eq!(
        names(&steps),
        [
            format!("uv python install {py}"),
            "uv venv".to_string(),
            "uv pip install -r requirements.txt".to_string(),
            "uv pip freeze > requirements.txt".to_string(),
        ]
    );
    let freeze = step(&steps, "uv pip freeze > requirements.txt");
    assert_eq!(args(freeze), ["pip", "freeze"]);
    assert_eq!(freeze.writes, [root.join("requirements.txt")]);
    assert_untouched(&root, &before);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn pip_mode_plans_venv_install_freeze_and_the_notebook_kernel() {
    // No constraints, so the newest interpreter on the machine is used.
    let root = project("pip", &[("requirements.txt", ""), ("analysis.ipynb", "{}")]);
    let before = tree(&root);

    let (summary, steps) =
        dry_run(setup::run(&root, &settings(PackageManager::Pip), &source())).await;
    let summary = summary.unwrap();
    assert_eq!(summary.resolved_mode, ResolvedMode::PurePip);
    let plan = Plan::from_summary(&summary, steps.clone());
    if !summary.ok {
        // A machine without any Python: the plan says it would stop there.
        assert!(steps.is_empty());
        assert!(plan.blocked.unwrap().starts_with("select interpreter"));
        return;
    }
    assert!(plan.notes.iter().any(|n| n.name == "select interpreter"));
    let ipykernel = format!("pip install {}", installed::IPYKERNEL_REQUIREMENT);
    assert_eq!(
        names(&steps),
        [
            "python -m venv",
            "pip install -r requirements.txt",
            "pip freeze > requirements.txt",
            // The venv the plan creates is empty, so it needs a kernel.
            ipykernel.as_str(),
            "pip freeze > requirements.txt",
        ]
    );
    let venv = root.join(".venv").to_string_lossy().into_owned();
    assert_eq!(args(&steps[0]), ["-m", "venv", venv.as_str()]);
    assert_eq!(
        steps[1].command[0],
        crate::core::pip::venv_python(&root.join(".venv")).to_string_lossy()
    );
    assert_untouched(&root, &before);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn install_plans_uv_init_then_the_requirements_migration() {
    let root = project("install", &[("requirements.txt", "numpy\n")]);
    let before = tree(&root);

    let (summary, steps) = dry_run(crate::core::install::install_package(
        &root,
        &settings(PackageManager::Uv),
        "mediapipe",
    ))
    .await;
    assert!(summary.unwrap().ok);
    assert_eq!(
        names(&steps),
        ["uv init", "uv add -r requirements.txt mediapipe"]
    );
    assert_eq!(
        args(&steps[steps.len() - 1]),
        ["add", "-r", "requirements.txt", "mediapipe"]
    );
    assert_untouched(&root, &before);
    let _ = std::fs::remove_dir_all(&root);
}

fn pixi_summary(root: &Path) -> SetupSummary {
    SetupSummary {
        package_manager: PackageManager::Auto,
        resolved_mode: ResolvedMode::Pixi,
        python: None,
        venv_path: pixi_manage::default_env_path(root),
        why: String::new(),
        steps: Vec::new(),
        ok: true,
    }
}

#[tokio::test]
async fn pixi_plans_install_and_the_python_change() {
    let root = project(
        "pixi",
        &[
            (
                "pixi.toml",
                include_str!("../../tests/fixtures/pixi/pixi-0.81-init.pixi.toml"),
            ),
            ("analysis.ipynb", "{}"),
        ],
    );
    let before = tree(&root);
    // Passed in, never located or run.
    let pixi = Path::new("pixi-that-is-never-run");
    let manifest = root.to_string_lossy().into_owned();

    let mut summary = pixi_summary(&root);
    let ((), steps) = dry_run(setup::run_pixi(&root, Some(pixi), &mut summary)).await;
    assert!(summary.ok, "{:?}", summary.steps);
    let ipykernel = format!("pixi add {}", installed::IPYKERNEL_REQUIREMENT);
    assert_eq!(names(&steps), ["pixi install", ipykernel.as_str()]);
    assert_eq!(steps[0].command[0], "pixi-that-is-never-run");
    assert_eq!(
        args(&steps[0]),
        ["install", "--manifest-path", manifest.as_str()]
    );
    assert!(steps[0].writes.contains(&root.join("pixi.lock")));

    let mut summary = pixi_summary(&root);
    let ((), steps) = dry_run(pixi_manage::set_python(
        Some(pixi),
        &root,
        PyVersion::py3(11),
        &mut summary,
    ))
    .await;
    assert!(summary.ok);
    assert_eq!(names(&steps), ["pixi add python=3.11", "pixi install"]);
    assert_eq!(
        args(&steps[0]),
        ["add", "--manifest-path", manifest.as_str(), "python=3.11"]
    );
    assert!(steps[0].writes.contains(&root.join("pixi.toml")));

    assert_untouched(&root, &before);
    let _ = std::fs::remove_dir_all(&root);
}
