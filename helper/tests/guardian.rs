//! F4 end to end: buffer text in, editor diagnostics out, using recorded PyPI
//! fixtures so nothing here touches the network.
//!
//! The platform is pinned to Linux x86-64 throughout. Wheel availability is
//! per platform, and the mediapipe fixture ships a narrower set of Windows
//! wheels than Linux ones, so an unpinned test would assert different ranges
//! depending on which machine ran it.

use std::path::PathBuf;

use pypilot::core::guardian::{analyze_buffer, Problem};
use pypilot::core::installed::Installed;
use pypilot::core::platform::{Arch, Os, Platform};
use pypilot::pypi::pyversion::PyVersion;
use pypilot::pypi::FixtureSource;

fn linux() -> Platform {
    Platform {
        os: Os::Linux,
        arch: Arch::X86_64,
    }
}

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn source() -> FixtureSource {
    let mut src = FixtureSource::new();
    src.load_json("mediapipe", fixture_path("mediapipe.json"))
        .unwrap();
    src.load_json("numpy", fixture_path("numpy.json")).unwrap();
    src
}

/// A directory with no Python files, so nothing is mistaken for a local module.
fn empty_workspace() -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "pypilot-guardian-empty-{}-{id}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The headline case. mediapipe ships no cp313 wheel, so importing it on a 3.13
/// venv must be reported against the interpreter, not as a missing package.
#[tokio::test]
async fn incompatible_import_names_the_version_and_the_fix() {
    let buffer = "import os\nimport mediapipe\n";
    let installed = Installed::default();

    let findings = analyze_buffer(
        buffer,
        &empty_workspace(),
        linux(),
        Some(PyVersion::py3(13)),
        &installed,
        &source(),
    )
    .await;

    // `os` is stdlib and must never reach PyPI or the editor.
    assert_eq!(findings.len(), 1, "only mediapipe should be reported");
    let f = &findings[0];
    assert_eq!(f.package, "mediapipe");
    assert_eq!(f.import.line, 1);

    match &f.problem {
        Problem::IncompatibleInterpreter {
            current,
            target,
            supported,
            ..
        } => {
            assert_eq!(*current, PyVersion::py3(13));
            assert_eq!(*target, Some(PyVersion::py3(12)));
            assert_eq!(supported.to_range_string(), "3.9–3.12");
        }
        other => panic!("expected an interpreter conflict, got {other:?}"),
    }
}

/// On a supported interpreter the same import is just missing, and the fix is a
/// plain install rather than a rebuild.
#[tokio::test]
async fn compatible_interpreter_gives_a_plain_install() {
    let findings = analyze_buffer(
        "import mediapipe\n",
        &empty_workspace(),
        linux(),
        Some(PyVersion::py3(12)),
        &Installed::default(),
        &source(),
    )
    .await;

    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].problem, Problem::NotInstalled);
}

/// Installed packages produce no diagnostics at all.
#[tokio::test]
async fn installed_packages_are_silent() {
    let installed = Installed {
        packages: ["mediapipe".to_string()].into_iter().collect(),
        ..Default::default()
    };
    let findings = analyze_buffer(
        "import mediapipe\n",
        &empty_workspace(),
        linux(),
        Some(PyVersion::py3(12)),
        &installed,
        &source(),
    )
    .await;
    assert!(findings.is_empty(), "an installed package needs no fix");
}

/// With no venv, the suggested interpreter comes from every import in the file,
/// so the first environment already suits all of them. numpy alone would allow
/// 3.14; mediapipe pulls the answer down to 3.12.
#[tokio::test]
async fn fresh_project_sizes_the_venv_for_every_import() {
    let buffer = "import numpy\nimport mediapipe\n";
    let findings = analyze_buffer(
        buffer,
        &empty_workspace(),
        linux(),
        None,
        &Installed::default(),
        &source(),
    )
    .await;

    assert_eq!(findings.len(), 2);
    for f in &findings {
        assert_eq!(
            f.problem,
            Problem::NoEnvironment {
                target: Some(PyVersion::py3(12))
            },
            "{} should target the version the whole file agrees on",
            f.package
        );
    }
}

/// A module that lives next to the file is the project's own, not a package.
#[tokio::test]
async fn local_modules_are_never_reported() {
    let dir = std::env::temp_dir().join("pypilot-guardian-local");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("preprocess.py"), "").unwrap();

    let findings = analyze_buffer(
        "import preprocess\nimport mediapipe\n",
        &dir,
        linux(),
        Some(PyVersion::py3(12)),
        &Installed::default(),
        &source(),
    )
    .await;

    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].package, "mediapipe");
}

/// An unknown name gets flagged but never gets an install offer, which is the
/// typosquatting guard.
#[tokio::test]
async fn unknown_package_is_flagged_without_an_install() {
    let findings = analyze_buffer(
        "import mediapip\n",
        &empty_workspace(),
        linux(),
        Some(PyVersion::py3(12)),
        &Installed::default(),
        &source(),
    )
    .await;

    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].problem, Problem::NotOnPyPi);
}

// --- Attribute checks against the installed package -------------------------
//
// The case metadata cannot answer. mediapipe 0.10.35 installs on any CPython
// 3.x and then raises `module 'mediapipe' has no attribute 'solutions'`,
// because the release dropped the legacy API. Reading the package on disk
// catches it while the file is being edited.

use pypilot::core::guardian::check_attributes;

/// Build a venv containing a package laid out like mediapipe 0.10.35.
fn venv_with_mediapipe(tag: &str) -> PathBuf {
    let venv = std::env::temp_dir().join(format!("pypilot-attr-{tag}"));
    let _ = std::fs::remove_dir_all(&venv);
    let site = venv.join("Lib").join("site-packages");
    let pkg = site.join("mediapipe");
    std::fs::create_dir_all(pkg.join("tasks")).unwrap();
    std::fs::write(pkg.join("tasks").join("__init__.py"), "").unwrap();
    std::fs::write(
        pkg.join("__init__.py"),
        "import mediapipe.tasks.python as tasks\n\
         from mediapipe.tasks.python.vision.core.image import Image\n",
    )
    .unwrap();
    std::fs::create_dir_all(site.join("mediapipe-0.10.35.dist-info")).unwrap();
    venv
}

fn installed_mediapipe(venv: &std::path::Path) -> Installed {
    pypilot::core::installed::scan(venv)
}

#[test]
fn removed_api_is_caught_before_running_the_file() {
    let venv = venv_with_mediapipe("removed");
    let installed = installed_mediapipe(&venv);

    let buffer = "import mediapipe as mp\n\nmp_hands = mp.solutions.hands\n";
    let findings = check_attributes(buffer, &venv, &installed);

    assert_eq!(findings.len(), 1, "got {findings:?}");
    match &findings[0].problem {
        Problem::AttributeMissing {
            attribute,
            installed_version,
            available,
        } => {
            assert_eq!(attribute, "solutions");
            assert_eq!(installed_version.as_deref(), Some("0.10.35"));
            assert!(
                available.contains(&"tasks".to_string()),
                "the message should point at the replacement API: {available:?}"
            );
        }
        other => panic!("expected a missing attribute, got {other:?}"),
    }
    // Squiggle lands on the usage, not the import.
    assert_eq!(findings[0].import.line, 2);
}

#[test]
fn attributes_that_exist_are_silent() {
    let venv = venv_with_mediapipe("present");
    let installed = installed_mediapipe(&venv);

    // `tasks` is a submodule, `Image` is re-exported in __init__.py.
    let buffer = "import mediapipe as mp\n\nx = mp.tasks.python\ny = mp.Image\n";
    assert!(
        check_attributes(buffer, &venv, &installed).is_empty(),
        "no false positives on the API that is actually there"
    );
}

#[test]
fn uninstalled_packages_are_not_attribute_checked() {
    // Nothing on disk to read, so nothing can be claimed.
    let venv = venv_with_mediapipe("uninstalled");
    let buffer = "import mediapipe as mp\n\nx = mp.solutions\n";
    assert!(check_attributes(buffer, &venv, &Installed::default()).is_empty());
}

/// A source whose every lookup fails as a PyPI outage would.
struct Offline;

impl pypilot::pypi::MetadataSource for Offline {
    async fn fetch(&self, name: &str) -> pypilot::Result<pypilot::pypi::metadata::PackageMetadata> {
        Err(
            pypilot::pypi::client::PyPiUnavailable(format!("PyPI returned 503 for `{name}`"))
                .into(),
        )
    }

    async fn fetch_version(
        &self,
        name: &str,
        _version: &str,
    ) -> pypilot::Result<pypilot::pypi::metadata::PackageMetadata> {
        Err(
            pypilot::pypi::client::PyPiUnavailable(format!("PyPI returned 503 for `{name}`"))
                .into(),
        )
    }
}

/// Offline, nothing is known about the package, so the editor must not claim
/// that PyPI has no such project (it used to say "check the spelling").
#[tokio::test]
async fn an_unreachable_pypi_is_not_reported_as_a_missing_project() {
    let findings = analyze_buffer(
        "import requests\n",
        &empty_workspace(),
        linux(),
        Some(PyVersion::py3(12)),
        &Installed::default(),
        &Offline,
    )
    .await;
    assert!(findings.is_empty(), "got {findings:?}");
}

/// A name PyPI really does not know still gets the spelling warning.
#[tokio::test]
async fn a_project_pypi_does_not_know_is_still_reported() {
    let findings = analyze_buffer(
        "import nmupy\n",
        &empty_workspace(),
        linux(),
        Some(PyVersion::py3(12)),
        &Installed::default(),
        &source(),
    )
    .await;
    assert_eq!(findings.len(), 1);
    assert!(matches!(findings[0].problem, Problem::NotOnPyPi));
}

/// doctor's side of the same rule: a lookup that never got an answer leaves
/// the package out of the range as "not checked", not as a typo.
#[tokio::test]
async fn doctor_files_an_unreachable_package_as_not_checked() {
    let report = pypilot::pypi::analyze(
        &Offline,
        linux(),
        &[pypilot::core::project::Requirement::any("requests")],
    )
    .await;
    assert!(report.unresolved.is_empty(), "{:?}", report.unresolved);
    assert_eq!(report.unreachable.len(), 1);
    assert_eq!(report.unreachable[0].0, "requests");
}
