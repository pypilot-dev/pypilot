//! The planner's decisions ([`plan`]) and its dry runs on small on-disk
//! projects, offline.

use std::path::{Path, PathBuf};

use super::*;
use crate::core::exec::{dry_run, PlannedStep};
use crate::pypi::FixtureSource;
use crate::settings::PackageManager;

fn finding(severity: Severity, title: &str, fix: FixKind) -> Finding {
    Finding {
        severity,
        title: title.into(),
        detail: String::new(),
        fix,
    }
}

#[test]
fn no_findings_plan_nothing() {
    let p = plan(&[]);
    assert!(p.is_empty());
    assert_eq!(
        describe(&p),
        "Nothing to fix: the environment matches the project."
    );
}

#[test]
fn info_findings_are_never_acted_on() {
    let p = plan(&[finding(Severity::Info, "fyi", FixKind::SyncEnvironment)]);
    assert!(p.is_empty());
}

#[test]
fn conda_and_poetry_projects_go_to_their_own_tools() {
    for fix in [
        FixKind::MigrateCondaToPixi,
        FixKind::MigrateConda,
        FixKind::PoetryInstall,
    ] {
        let p = plan(&[finding(Severity::Warning, "project", fix.clone())]);
        assert!(p.setup, "{fix:?}");
        assert!(p.manual.is_empty());
        assert_eq!(p.fixes, ["project"]);
    }
    // The optional migration offer stays a suggestion.
    let p = plan(&[finding(Severity::Info, "conda", FixKind::MigrateConda)]);
    assert!(p.is_empty());
    let p = plan(&[finding(Severity::Warning, "pixi", FixKind::MigrateToPixi)]);
    assert!(p.is_empty() && p.manual.len() == 1);
}

#[test]
fn manual_errors_are_kept_for_the_report() {
    let p = plan(&[finding(Severity::Error, "conflict", FixKind::Manual)]);
    assert!(p.is_empty());
    assert_eq!(p.manual.len(), 1);
    assert!(describe(&p).starts_with("Nothing PyPilot can fix"));
}

#[test]
fn missing_packages_only_sync() {
    let p = plan(&[finding(
        Severity::Warning,
        "missing",
        FixKind::SyncEnvironment,
    )]);
    assert!(p.sync && !p.relock && !p.setup && p.rebuild.is_none());
    assert_eq!(p.fixes, ["missing"]);
}

#[test]
fn relock_and_reinstall_imply_a_sync() {
    let p = plan(&[
        finding(Severity::Warning, "stale", FixKind::Relock),
        finding(
            Severity::Error,
            "damaged",
            FixKind::ReinstallPackages(vec!["requests".into()]),
        ),
        finding(
            Severity::Warning,
            "orphan",
            FixKind::ReinstallPackages(vec!["click".into(), "requests".into()]),
        ),
    ]);
    assert!(p.relock && p.sync && p.rebuild.is_none());
    assert_eq!(p.reinstall, ["requests", "click"]);
}

#[test]
fn a_required_python_outranks_a_repair_and_a_rebuild_drops_reinstalls() {
    let p = plan(&[
        finding(
            Severity::Error,
            "broken",
            FixKind::RepairEnvironment {
                python: Some(PyVersion::py3(13)),
            },
        ),
        finding(
            Severity::Error,
            "too new",
            FixKind::RecreateWithPython(PyVersion::py3(12)),
        ),
        finding(
            Severity::Error,
            "damaged",
            FixKind::ReinstallPackages(vec!["six".into()]),
        ),
    ]);
    let rebuild = p.rebuild.clone().unwrap();
    assert_eq!(rebuild.python, Some(PyVersion::py3(12)));
    assert!(rebuild.required);
    assert_eq!(rebuild.reason, "too new");
    assert!(p.reinstall.is_empty() && p.sync);

    // A later repair does not replace the required version.
    let p = plan(&[
        finding(
            Severity::Error,
            "too new",
            FixKind::RecreateWithPython(PyVersion::py3(12)),
        ),
        finding(
            Severity::Error,
            "leak",
            FixKind::RepairEnvironment { python: None },
        ),
    ]);
    assert_eq!(p.rebuild.unwrap().python, Some(PyVersion::py3(12)));
}

#[test]
fn requirement_edits_are_planned_first_and_once() {
    let edit = FixKind::EditRequirement(crate::core::RequirementEdit {
        file: "pyproject.toml".into(),
        old: "urllib3<1.21".into(),
        new: "urllib3>=1.21.1,<3".into(),
    });
    let add = FixKind::AddRequirements(vec!["PyYAML".into()]);
    let p = plan(&[
        finding(Severity::Error, "conflict", edit.clone()),
        finding(Severity::Error, "conflict again", edit.clone()),
        finding(Severity::Warning, "undeclared", add.clone()),
        finding(
            Severity::Info,
            "advice",
            FixKind::AddRequirements(vec!["x".into()]),
        ),
    ]);
    assert!(!p.is_empty());
    assert_eq!(p.requirements, [edit, add]);
    // The sync comes from the assessment after the edits, not from here.
    assert!(!p.sync && p.rebuild.is_none() && p.manual.is_empty());
}

#[test]
fn unresolvable_requirements_stop_before_anything_changes() {
    let conflict = finding(
        Severity::Error,
        &format!("{}: `a` and `b`", resolution::CONFLICT_TITLE),
        FixKind::Manual,
    );
    let step = resolution::blocking_step_in(&[conflict]).unwrap();
    assert!(!step.ok && step.detail.contains("Nothing was changed"));
    assert!(resolution::blocking_step_in(&[finding(
        Severity::Warning,
        "missing",
        FixKind::SyncEnvironment
    )])
    .is_none());
}

#[test]
fn nothing_there_runs_the_bootstrap() {
    let p = plan(&[finding(
        Severity::Warning,
        "No environment set up yet",
        FixKind::SetupEnvironment,
    )]);
    assert!(p.setup && !p.is_empty());
}

#[test]
fn uv_resolution_errors_name_the_floor() {
    let split = "error: No solution found when resolving dependencies for split (markers: python_full_version >= '3.8' and python_full_version < '3.10'):\n  cause: Because the requested Python version (>=3.8) does not satisfy Python>=3.11 and all of:\n             numpy>=2.3.0,<=2.4.0rc1\n";
    assert_eq!(raised_floor(split), Some(PyVersion::py3(10)));
    let plain = "Because the requested Python version (>=3.8) does not satisfy Python>=3.11 and numpy==2.3.0 depends on Python>=3.11";
    assert_eq!(raised_floor(plain), Some(PyVersion::py3(11)));
    assert_eq!(raised_floor("error: network unreachable"), None);
    assert!(floor_reason(split).starts_with("numpy>=2.3.0,<=2.4.0rc1"));
}

#[test]
fn raising_requires_python_keeps_the_upper_bound_and_layout() {
    let text = "[project]\nname = \"s3\" # keep\nrequires-python = \">=3.8, <3.14\"\ndependencies = [\"numpy>=2.1\"]\n";
    let (old, new, edited) = raised_requires_python(text, PyVersion::py3(10)).unwrap();
    assert_eq!(old, ">=3.8, <3.14");
    assert_eq!(new, ">=3.10,<3.14");
    assert!(edited.contains("requires-python = \">=3.10,<3.14\""));
    assert!(edited.contains("name = \"s3\" # keep"));
    // Already high enough: nothing to do.
    assert!(raised_requires_python(&edited, PyVersion::py3(10)).is_none());
    // No requires-python at all: one is added.
    let (_, new, _) =
        raised_requires_python("[project]\nname = \"x\"\n", PyVersion::py3(10)).unwrap();
    assert_eq!(new, ">=3.10");
}

#[tokio::test]
async fn a_snapshot_puts_the_edited_manifests_back() {
    // s05: an edit that leaves the project unresolvable is undone.
    let dir = std::env::temp_dir().join(format!("pp-snapshot-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let pyproject = dir.join("pyproject.toml");
    std::fs::write(&pyproject, "flask==1.1.2\n").unwrap();
    std::fs::write(dir.join("uv.lock"), "lock\n").unwrap();
    let snapshot = ManifestSnapshot::take(&dir);

    // A dry run restores the planned write, and the disk is untouched.
    let (restored, steps) = dry_run(async {
        exec::write(&pyproject, "flask>=2.0.0\n", Effect::new("edit", "test")).unwrap();
        let restored = snapshot.restore().unwrap();
        (restored, exec::read(&pyproject).unwrap())
    })
    .await;
    assert_eq!(restored.0, ["pyproject.toml"]);
    assert_eq!(restored.1, b"flask==1.1.2\n");
    assert_eq!(names(&steps), ["edit", "roll back requirement edits"]);

    // For real: only the changed file is written back.
    std::fs::write(&pyproject, "flask>=2.0.0\n").unwrap();
    assert_eq!(snapshot.restore().unwrap(), ["pyproject.toml"]);
    assert_eq!(
        std::fs::read_to_string(&pyproject).unwrap(),
        "flask==1.1.2\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("uv.lock")).unwrap(),
        "lock\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn additions_skip_declared_names_and_wait_for_the_rebuild() {
    // c12b: the typo fix already declares `requests`, and requires-python
    // was raised past the venv's 3.9, where `uv add` refuses to run.
    let dir = std::env::temp_dir().join(format!("pp-additions-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let pyproject = dir.join("pyproject.toml");
    std::fs::write(
        &pyproject,
        "[project]\nname = \"c12b\"\nversion = \"0.1.0\"\nrequires-python = \">=3.10\"\ndependencies = [\"requests\"]\n",
    )
    .unwrap();
    let add = FixKind::AddRequirements(vec!["requests".into(), "PyYAML".into()]);
    assert_eq!(
        additions_for(&dir, &add, Some(PyVersion::py3(9))),
        [FixKind::EditRequirement(crate::core::RequirementEdit {
            file: pyproject.clone(),
            old: String::new(),
            new: "PyYAML".into(),
        })]
    );
    // On a Python the project allows, `uv add` runs, for what is left.
    assert_eq!(
        additions_for(&dir, &add, Some(PyVersion::py3(12))),
        [FixKind::AddRequirements(vec!["PyYAML".into()])]
    );
    let only_requests = FixKind::AddRequirements(vec!["requests".into()]);
    assert!(additions_for(&dir, &only_requests, Some(PyVersion::py3(12))).is_empty());
    // Other fixes pass through.
    assert_eq!(
        additions_for(&dir, &FixKind::Relock, None),
        [FixKind::Relock]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- dry runs ----------------------------------------------------------------

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

/// numpy 1.26.4 has cp39-cp312 wheels on every platform CI runs on.
fn source() -> FixtureSource {
    let mut src = FixtureSource::new();
    for key in ["numpy", "numpy==1.26.4"] {
        src.load_json(key, fixture("numpy-1.26.4.json")).unwrap();
    }
    src
}

fn settings() -> Settings {
    Settings {
        package_manager: PackageManager::Uv,
        data_refresh_days: 0,
        ..Settings::default()
    }
}

const PYPROJECT: &str = "[project]\nname = \"demo\"\nversion = \"0.1.0\"\nrequires-python = \">=3.9\"\ndependencies = [\"numpy==1.26.4\"]\n";

/// A project with a fake but healthy venv in `dir` on Python 3.`minor`,
/// numpy installed in it, and `.python-version` 3.12.
fn project(tag: &str, dir: &str, minor: u8) -> PathBuf {
    let root = std::env::temp_dir().join(format!("pypilot-repair-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("pyproject.toml"), PYPROJECT).unwrap();
    std::fs::write(root.join(".python-version"), "3.12\n").unwrap();
    let venv = root.join(dir);
    let py = pip::venv_python(&venv);
    std::fs::create_dir_all(py.parent().unwrap()).unwrap();
    std::fs::write(&py, "").unwrap();
    std::fs::write(
        venv.join("pyvenv.cfg"),
        format!(
            "home = {}\ninclude-system-site-packages = false\nversion_info = 3.{minor}.1\n",
            root.display()
        ),
    )
    .unwrap();
    let site = if cfg!(windows) {
        venv.join("Lib").join("site-packages")
    } else {
        venv.join("lib")
            .join(format!("python3.{minor}"))
            .join("site-packages")
    };
    let info = site.join("numpy-1.26.4.dist-info");
    std::fs::create_dir_all(&info).unwrap();
    std::fs::create_dir_all(site.join("numpy")).unwrap();
    std::fs::write(site.join("numpy").join("__init__.py"), "").unwrap();
    std::fs::write(
        info.join("RECORD"),
        "numpy/__init__.py,sha256=x,0\nnumpy-1.26.4.dist-info/RECORD,,\n",
    )
    .unwrap();
    root
}

fn site(root: &Path, dir: &str) -> PathBuf {
    crate::core::installed::site_packages_dirs(&root.join(dir))
        .into_iter()
        .next()
        .unwrap()
}

fn names(steps: &[PlannedStep]) -> Vec<String> {
    steps
        .iter()
        .filter(|s| s.name != "download uv")
        .map(|s| s.name.clone())
        .collect()
}

async fn plan_for(root: &Path) -> (SetupSummary, Vec<PlannedStep>) {
    let (summary, steps) = dry_run(run_with_mode(
        root,
        &settings(),
        &source(),
        ResolvedMode::UvFull,
    ))
    .await;
    (summary.unwrap(), steps)
}

#[tokio::test]
async fn healthy_project_gets_no_steps() {
    let root = project("healthy", ".venv", 12);
    let (summary, steps) = plan_for(&root).await;
    assert!(summary.ok, "{summary:#?}");
    assert!(names(&steps).is_empty(), "{steps:#?}");
    assert!(summary.why.starts_with("Nothing to fix"), "{}", summary.why);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn damaged_package_is_reinstalled_in_place() {
    let root = project("reinstall", ".venv", 12);
    std::fs::remove_dir_all(site(&root, ".venv").join("numpy")).unwrap();
    let (summary, steps) = plan_for(&root).await;
    assert!(summary.ok, "{summary:#?}");
    assert_eq!(
        names(&steps),
        ["uv sync --reinstall-package numpy", "verify"]
    );
    let sync = &steps
        .iter()
        .find(|s| s.name.starts_with("uv sync"))
        .unwrap();
    assert_eq!(
        sync.command[1..],
        ["sync", "--python", "3.12", "--reinstall-package", "numpy"]
    );
    assert!(sync.env.is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn an_env_in_venv_dir_is_synced_where_it_is() {
    let root = project("venvdir", "venv", 12);
    std::fs::remove_dir_all(site(&root, "venv").join("numpy-1.26.4.dist-info")).unwrap();
    std::fs::remove_dir_all(site(&root, "venv").join("numpy")).unwrap();
    let (summary, steps) = plan_for(&root).await;
    assert!(summary.ok, "{summary:#?}");
    assert_eq!(names(&steps), ["uv sync", "verify"]);
    assert_eq!(
        steps[steps.len() - 2].env.get("UV_PROJECT_ENVIRONMENT"),
        Some(&root.join("venv").to_string_lossy().into_owned())
    );
    assert_eq!(summary.venv_path, root.join("venv"));
    assert!(!steps.iter().any(|s| s.writes.contains(&root.join(".venv"))));
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn venv_without_pyvenv_cfg_is_removed_and_rebuilt_on_the_pin() {
    let root = project("nocfg", ".venv", 12);
    std::fs::remove_file(root.join(".venv").join("pyvenv.cfg")).unwrap();
    let (summary, steps) = plan_for(&root).await;
    assert!(summary.ok, "{summary:#?}");
    assert_eq!(
        names(&steps),
        [
            "remove the old environment",
            "uv python install 3.12",
            "uv venv",
            "uv sync",
            "verify"
        ]
    );
    assert_eq!(summary.python, Some(PyVersion::py3(12)));
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_venv_off_the_pin_is_rebuilt_on_it_and_the_pin_stays() {
    // numpy 1.26.4 has no 3.13 wheels and the pin says 3.12.
    let root = project("pin", ".venv", 13);
    let (summary, steps) = plan_for(&root).await;
    assert!(summary.ok, "{summary:#?}");
    let names = names(&steps);
    assert_eq!(
        names,
        ["uv python install 3.12", "uv venv", "uv sync", "verify"]
    );
    assert!(!names.iter().any(|n| n.starts_with("uv python pin")));
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn an_unsatisfiable_requires_python_builds_nothing() {
    let root = project("rp", ".venv", 12);
    std::fs::write(
        root.join("pyproject.toml"),
        PYPROJECT.replace(">=3.9", ">=3.10,<=3.9"),
    )
    .unwrap();
    let (summary, steps) = plan_for(&root).await;
    assert!(!summary.ok);
    assert!(steps.is_empty(), "{steps:#?}");
    assert!(summary.steps[0].detail.contains("matches no Python"));
    let _ = std::fs::remove_dir_all(&root);
}

/// D 5a: the requirements.txt edit runs before the bootstrap, and the
/// bootstrap's report still lists it.
#[tokio::test]
async fn requirement_edits_before_a_setup_are_reported() {
    let root = project("edit-then-setup", ".venv", 12);
    std::fs::remove_dir_all(root.join(".venv")).unwrap();
    std::fs::write(root.join("requirements.txt"), "numpy==1.25.0\n").unwrap();
    let (summary, steps) = plan_for(&root).await;
    assert_eq!(names(&steps)[0], "edit requirements.txt", "{steps:#?}");
    assert_eq!(
        summary.steps[0].name, "edit requirements.txt",
        "{summary:#?}"
    );
    assert_eq!(
        summary.steps[0].detail,
        "`numpy==1.25.0` -> `numpy==1.26.4`"
    );
    assert!(
        summary
            .why
            .contains("requirements.txt disagree on numpy. Then: "),
        "{}",
        summary.why
    );
    assert!(!summary.why.ends_with(' '));
    // A dry run: the file is untouched.
    assert_eq!(
        std::fs::read_to_string(root.join("requirements.txt")).unwrap(),
        "numpy==1.25.0\n"
    );
    let _ = std::fs::remove_dir_all(&root);
}
