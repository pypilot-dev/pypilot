//! pixi failures, explained: which Python would work, and the command to get it.
//!
//! When `pixi add --pypi` or `pixi install` fails, pixi relays uv's resolver
//! verdict ("has no wheels with a matching Python ABI tag (e.g., `cp312`)")
//! without saying which Python the package *does* support or how to switch
//! (prefix-dev/pixi#5230, #5752, #5266). [`assess_pixi`] predicts that failure
//! from PyPI metadata before it happens; [`explain_pixi_error`] turns the stderr
//! of one that did into one paragraph with a next step.

use crate::core::pixi::{self, ManifestFormat, PixiError, PixiProject};
use crate::core::platform::{Os, Platform};
use crate::core::project::normalize_name;
use crate::core::{Finding, FixKind, Severity};
use crate::matrix;
use crate::pypi::{self, CompatReport, MetadataSource, PyVersion, PyVersionSet};

/// Check a pixi project's default-environment PyPI dependencies against the
/// Python pixi gives that environment, so a `pixi install` that would fail on
/// "no wheels with a matching Python ABI tag" is caught, with the fix, first.
///
/// The Python is `env_python` (what `.pixi/envs/default` has installed) when
/// known, else the manifest's pin read by [`pixi::python_minor_hint`]. No
/// finding when everything fits or no Python can be determined.
///
/// Conda dependencies are deliberately not checked here: conda-forge builds
/// each package per Python and pixi's conda solver picks `python` together
/// with them, so PyPI wheel tags say nothing about them, and a clash among
/// them shows up in pixi's own solve ([`explain_pixi_error`] covers that).
/// PyPI names that fail to resolve are left to the generic dependency check,
/// so they are not reported twice.
pub async fn assess_pixi<S: MetadataSource>(
    project: &PixiProject,
    env_python: Option<PyVersion>,
    source: &S,
    platform: Platform,
) -> Vec<Finding> {
    if project.pypi_deps.is_empty() {
        return Vec::new();
    }
    let report = pypi::analyze(source, platform, &project.pypi_deps).await;
    pixi_findings(project, env_python, &report)
}

/// [`assess_pixi`] for the project's installed default environment, if any.
pub async fn assess_default_env<S: MetadataSource>(
    project: &PixiProject,
    source: &S,
) -> Vec<Finding> {
    let env_python = pixi::env(project, pixi::DEFAULT_ENV).and_then(|e| e.python_version);
    assess_pixi(project, env_python, source, Platform::current()).await
}

/// The pure half of [`assess_pixi`]: findings from a finished analysis.
pub fn pixi_findings(
    project: &PixiProject,
    env_python: Option<PyVersion>,
    report: &CompatReport,
) -> Vec<Finding> {
    let skipped = skipped_note(project.skipped_pypi_deps);

    if report.intersection.is_empty() {
        let why = report.conflicts.first().map_or_else(
            || "The PyPI dependencies have no common supported Python version.".to_string(),
            |c| c.explanation.clone(),
        );
        return vec![Finding {
            severity: Severity::Error,
            title: CONFLICT_TITLE.to_string(),
            detail: format!(
                "{why} pixi cannot pick one Python for the environment, so `pixi add python=…` will not help; change or drop one of these PyPI dependencies.{skipped}"
            ),
            fix: FixKind::Manual,
        }];
    }

    let (python, from) = match env_python {
        Some(py) => (py, format!("The pixi environment runs Python {py}")),
        None => {
            let Some(py) = pixi::python_minor_hint(project) else {
                return Vec::new();
            };
            let file = project
                .manifest
                .file_name()
                .map_or_else(|| "pixi.toml".into(), |f| f.to_string_lossy());
            let spec = project.python.as_deref().unwrap_or_default();
            (py, format!("{file} sets python to `{spec}` (Python {py})"))
        }
    };
    // A Python outside the tracked range can't be judged from wheel tags.
    if !PyVersionSet::universe().contains(python) || report.is_compatible(python) {
        return Vec::new();
    }
    let blockers = report.blockers_for(python);
    let Some(target) = report.suggested_python() else {
        return Vec::new();
    };

    let os = os_name(report.platform.os);
    let reasons: Vec<String> = blockers
        .iter()
        .map(|p| {
            if p.lacks_wheel_for(python) {
                format!(
                    "{} {} ships no Python {python} wheel for {os} (its wheels cover {})",
                    p.name,
                    p.version,
                    p.supported.to_range_string()
                )
            } else {
                let declared = p
                    .requires_python
                    .as_deref()
                    .map(|rp| format!(", requires_python `{rp}`"))
                    .unwrap_or_default();
                format!(
                    "{} {} supports Python {}{declared}",
                    p.name,
                    p.version,
                    p.supported.to_range_string()
                )
            }
        })
        .collect();
    let consequence = if blockers.iter().any(|p| p.lacks_wheel_for(python)) {
        "`pixi install` fails with \"no wheels with a matching Python ABI tag\" unless it can build from source."
    } else {
        "`pixi install` cannot install it on this Python."
    };
    let names: Vec<&str> = blockers.iter().map(|p| p.name.as_str()).collect();
    let verb = if names.len() == 1 { "does" } else { "do" };

    vec![Finding {
        severity: Severity::Error,
        title: format!("{} {verb}{}", names.join(", "), mismatch_suffix(python)),
        detail: format!(
            "{from}, but {}. {consequence} Run `pixi add python={target}` to move the environment to Python {target}, the newest version every PyPI dependency supports.{skipped}",
            reasons.join("; ")
        ),
        fix: FixKind::Manual,
    }]
}

/// Title of the finding for PyPI dependencies with no common Python.
pub const CONFLICT_TITLE: &str = "pixi's PyPI dependencies conflict on Python version";

/// How a mismatch title ends: "tensorflow does[ not support Python 3.12 in
/// this pixi environment]".
fn mismatch_suffix(python: impl std::fmt::Display) -> String {
    format!(" not support Python {python} in this pixi environment")
}

/// Is `title` this module's finding for dependencies that exclude Python
/// `python` (as printed, `3.12`)?
pub fn is_mismatch_title(title: &str, python: &str) -> bool {
    title.ends_with(&mismatch_suffix(python))
}

/// " 2 PyPI dependencies given as git, path or url were not checked."
fn skipped_note(count: usize) -> String {
    match count {
        0 => String::new(),
        1 => " 1 PyPI dependency given as git, path or url was not checked.".to_string(),
        n => format!(" {n} PyPI dependencies given as git, path or url were not checked."),
    }
}

fn os_name(os: Os) -> &'static str {
    match os {
        Os::Windows => "Windows",
        Os::Macos => "macOS",
        Os::Linux => "Linux",
    }
}

/// Explain the stderr of a failed `pixi add` / `pixi install` in one paragraph
/// that ends with what to do next.
pub fn explain_pixi_error(stderr: &str, project: Option<&PixiProject>) -> String {
    explain_pixi_error_with(stderr, project, None)
}

/// [`explain_pixi_error`] with a PyPI analysis of the project's dependencies
/// at hand, which supplies the supported Python range when pixi's own hint
/// does not list one.
pub fn explain_pixi_error_with(
    stderr: &str,
    project: Option<&PixiProject>,
    compat: Option<&CompatReport>,
) -> String {
    let text = pixi::unwrap_report(stderr);
    let platform = target_platform(&text);
    match pixi::classify_pixi_error(stderr) {
        PixiError::NoMatchingWheel { package, abi } => explain_no_wheel(
            &text,
            &package,
            abi.as_deref(),
            platform.as_deref(),
            project,
            compat,
        ),
        PixiError::NotFound { package } => explain_not_found(&package, project),
        PixiError::Unsolvable => explain_unsolvable(&text, platform.as_deref(), project),
        PixiError::Other => explain_other(&text),
    }
}

/// `win-64` from "for platform 'win-64'". miette may wrap inside the
/// quotes (`'win- 64'`), so whitespace in the name is dropped.
fn target_platform(text: &str) -> Option<String> {
    let rest = text.split_once("for platform '")?.1;
    let name: String = rest
        .split_once('\'')?
        .0
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    (!name.is_empty()).then_some(name)
}

fn on_platform(platform: Option<&str>) -> String {
    platform.map(|p| format!(" on {p}")).unwrap_or_default()
}

/// The text between `start` and the next `end` after it.
fn between<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let rest = text.split_once(start)?.1;
    Some(rest.split_once(end).map_or(rest, |(a, _)| a))
}

fn explain_no_wheel(
    text: &str,
    package: &str,
    abi: Option<&str>,
    platform: Option<&str>,
    project: Option<&PixiProject>,
    compat: Option<&CompatReport>,
) -> String {
    // uv's hint: "You require CPython 3.12 (`cp312`), but we only found wheels
    // for `tensorflow` (v2.15.0) with the following Python ABI tags: `cp39`, ..."
    let python = between(text, "You require CPython ", " ")
        .and_then(PyVersion::parse)
        .or_else(|| abi.and_then(PyVersion::parse))
        .or_else(|| project.and_then(pixi::python_minor_hint));
    let version = between(text, "but we only found wheels for", ")")
        .and_then(|s| s.split_once("(v"))
        .map(|(_, v)| v.trim().to_string());
    let hinted: PyVersionSet = text
        .split_once("Python ABI tags:")
        .map(|(_, tags)| {
            PyVersionSet::from_versions(tags.split('`').skip(1).step_by(2).filter_map(abi_python))
        })
        .unwrap_or_default();

    let analysed = compat.and_then(|c| {
        c.per_package
            .iter()
            .find(|p| normalize_name(&p.name) == normalize_name(package))
    });
    let supported = match analysed {
        Some(p) if !p.supported.is_empty() => Some(p.supported.clone()),
        _ => (!hinted.is_empty()).then_some(hinted),
    };
    // The newest Python every dependency supports, when the whole project was
    // analysed; else the newest this package supports.
    let target = compat
        .map(|c| &c.intersection)
        .filter(|i| !i.is_empty())
        .and_then(PyVersionSet::max)
        .or_else(|| supported.as_ref().and_then(PyVersionSet::max));

    let who = match &version {
        Some(v) => format!("{package} {v}"),
        None => package.to_string(),
    };
    let tag = python
        .map(|p| p.cp_tag())
        .or_else(|| abi.map(str::to_string));
    let which = match (python, &tag) {
        (Some(p), Some(t)) => format!("Python {p} (`{t}`)"),
        _ => "this environment's Python".to_string(),
    };
    let mut out = format!("{who} has no wheel for {which}{}", on_platform(platform));
    match (&supported, target) {
        (Some(s), Some(t)) => out.push_str(&format!(
            "; it only ships wheels for Python {}. Run `pixi add python={t}` to move the environment to Python {t}",
            s.to_range_string()
        )),
        _ => out.push_str(&format!(
            ". Check which Python versions its wheels cover at https://pypi.org/project/{package}/#files, then run `pixi add python=3.X` with one of them"
        )),
    }
    match tag {
        Some(t) => out.push_str(&format!(
            ", or require a {package} release that publishes `{t}` wheels."
        )),
        None => out.push('.'),
    }
    out
}

/// `cp311` -> 3.11; `abi3`, `none` and PyPy tags -> none.
fn abi_python(tag: &str) -> Option<PyVersion> {
    tag.strip_prefix("cp")?;
    PyVersion::parse(tag)
}

fn explain_not_found(package: &str, project: Option<&PixiProject>) -> String {
    let index_table = match project.map(|p| p.format) {
        Some(ManifestFormat::Pyproject) => "`[tool.pixi.pypi-options]` in pyproject.toml",
        _ => "`[pypi-options]` in pixi.toml",
    };
    let import_hint = if matrix::is_mapped(package) {
        let pypi_name = matrix::resolve_package(package);
        format!(
            " `{package}` is an import name: the PyPI package is `{pypi_name}`, so run `pixi add --pypi {pypi_name}`."
        )
    } else {
        " Check the spelling; PyPI names often differ from import names (`scikit-learn`, not `sklearn`).".to_string()
    };
    format!(
        "`{package}` was not found on PyPI or any configured index.{import_hint} If it is a conda package, add it without `--pypi`; if it lives on a private index, add that index under `extra-index-urls` in {index_table}."
    )
}

/// How many unavailable requirements an Unsolvable explanation lists.
const MAX_LEAVES: usize = 3;

fn explain_unsolvable(text: &str, platform: Option<&str>, project: Option<&PixiProject>) -> String {
    let on = on_platform(platform);
    if let Some(root) = between(text, "Cannot solve the request because of:", ":") {
        // Conda: "tensorflow * cannot be installed because there are no viable options"
        let root = root
            .split(" because there are no viable options")
            .next()
            .unwrap_or(root)
            .trim();
        let package = root.split_whitespace().next().unwrap_or(root);
        let leaves = unavailable_requirements(text);
        let mut out = format!("pixi could not solve the conda dependencies{on}: {root}.");
        if !leaves.is_empty() {
            out.push_str(&format!(
                " Every candidate needs something that is not available: {}.",
                leaves.join("; ")
            ));
        }
        if leaves.iter().any(|l| l.starts_with("python ")) {
            let current = project
                .and_then(pixi::python_minor_hint)
                .map(|p| format!(" (this environment asks for Python {p})"))
                .unwrap_or_default();
            out.push_str(&format!(
                " Its builds need a different Python{current}: change the `python` pin with `pixi add python=3.X`, or pin a {package} version built for your Python; `pixi search {package}` lists the builds for this platform."
            ));
        } else {
            out.push_str(&format!(
                " Loosen one of the pins named above; `pixi search {package}` lists the builds for this platform."
            ));
        }
        return out;
    }
    if let Some(reason) = between(text, "Because ", ", we can conclude") {
        return format!(
            "pixi could not solve the PyPI dependencies{on}: {}. Loosen or remove one of the pins named here.",
            reason.trim()
        );
    }
    format!(
        "pixi could not solve the environment{on}: {}. Loosen the version pins it names.",
        summary(text)
    )
}

/// The requirements a conda solve found no candidates for, in order of
/// appearance, without repeats: `numpy >=1.16.1,<2.0.0a0`, `python 3.6.*`.
fn unavailable_requirements(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some((before, after)) = rest.split_once(", for which no candidates were found") {
        if let Some((_, leaf)) = before.rsplit_once(" would require ") {
            let leaf = leaf.trim().to_string();
            if !out.contains(&leaf) {
                out.push(leaf);
            }
        }
        if out.len() == MAX_LEAVES {
            break;
        }
        rest = after;
    }
    out
}

/// How much of an unrecognised error is quoted back.
const SUMMARY_CHARS: usize = 300;

/// The error text without pixi's `Error: ×` lead-in, cut to [`SUMMARY_CHARS`].
fn summary(text: &str) -> String {
    let text = text.trim();
    let text = text.strip_prefix("Error:").unwrap_or(text);
    let text = text.trim_start_matches(|c: char| c.is_whitespace() || c == '×');
    let mut out: String = text.chars().take(SUMMARY_CHARS).collect();
    if text.chars().count() > SUMMARY_CHARS {
        out.push('…');
    }
    out
}

fn explain_other(text: &str) -> String {
    let summary = summary(text);
    if summary.is_empty() {
        "pixi failed without an error message. Re-run the command with `-v` to see what it was doing.".to_string()
    } else {
        format!("pixi failed: {summary}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::pixi::{parse_manifest, ManifestFormat};
    use crate::pypi::metadata::PackageAnalysis;
    use std::path::Path;

    const NO_WHEEL: &str = include_str!("../../tests/fixtures/pixi/pixi-0.81-err-no-wheel.txt");
    const NOT_FOUND: &str = include_str!("../../tests/fixtures/pixi/pixi-0.81-err-not-found.txt");
    const CONDA_CONFLICT: &str =
        include_str!("../../tests/fixtures/pixi/pixi-0.81-err-conda-conflict.txt");

    fn project(manifest: &str) -> PixiProject {
        parse_manifest(
            manifest,
            ManifestFormat::PixiToml,
            Path::new("/proj"),
            Path::new("/proj/pixi.toml").to_path_buf(),
        )
        .unwrap()
    }

    #[test]
    fn explains_missing_wheel_from_uv_hint() {
        let msg = explain_pixi_error(NO_WHEEL, None);
        assert_eq!(
            msg,
            "tensorflow 2.15.0 has no wheel for Python 3.12 (`cp312`) on win-64; it only ships wheels for Python 3.9–3.11. Run `pixi add python=3.11` to move the environment to Python 3.11, or require a tensorflow release that publishes `cp312` wheels."
        );
        assert!(!msg.contains('\u{1b}'));
    }

    #[test]
    fn missing_wheel_prefers_the_project_analysis() {
        let compat = CompatReport {
            platform: crate::core::platform::Platform::current(),
            per_package: vec![PackageAnalysis {
                name: "tensorflow".into(),
                version: "2.15.0".into(),
                supported: PyVersionSet::from_versions([PyVersion::py3(10), PyVersion::py3(11)]),
                has_platform_wheels: true,
                sdist_only: false,
                has_foreign_wheels: false,
                no_files_for_platform: false,
                wheel_platforms: Vec::new(),
                requires_python: Some(">=3.9".into()),
            }],
            unresolved: Vec::new(),
            unreachable: Vec::new(),
            intersection: PyVersionSet::from_versions([PyVersion::py3(10)]),
            conflicts: Vec::new(),
            sdist_warnings: Vec::new(),
        };
        let msg = explain_pixi_error_with(NO_WHEEL, None, Some(&compat));
        assert!(
            msg.contains("it only ships wheels for Python 3.10–3.11"),
            "{msg}"
        );
        assert!(msg.contains("`pixi add python=3.10`"), "{msg}");
    }

    #[test]
    fn missing_wheel_without_hint_stays_generic() {
        let stderr = "Error:   × failed to solve the pypi requirements of environment 'default' for\n  │ platform 'linux-64'\n  ╰─▶ Because foo==1.0 has no wheels with a matching Python ABI tag (e.g., `cp313`) and you require foo==1.0, we can conclude that your requirements are unsatisfiable.\n";
        assert_eq!(
            explain_pixi_error(stderr, None),
            "foo has no wheel for Python 3.13 (`cp313`) on linux-64. Check which Python versions its wheels cover at https://pypi.org/project/foo/#files, then run `pixi add python=3.X` with one of them, or require a foo release that publishes `cp313` wheels."
        );
    }

    #[test]
    fn explains_unknown_package() {
        let msg = explain_pixi_error(NOT_FOUND, None);
        assert_eq!(
            msg,
            "`definitely-not-a-real-package-xyz` was not found on PyPI or any configured index. Check the spelling; PyPI names often differ from import names (`scikit-learn`, not `sklearn`). If it is a conda package, add it without `--pypi`; if it lives on a private index, add that index under `extra-index-urls` in `[pypi-options]` in pixi.toml."
        );
    }

    #[test]
    fn unknown_import_name_points_at_the_pypi_name() {
        let stderr = NOT_FOUND.replace("definitely-not-a-real-package-xyz", "sklearn");
        let msg = explain_pixi_error(&stderr, None);
        assert!(
            msg.contains(
                "the PyPI package is `scikit-learn`, so run `pixi add --pypi scikit-learn`"
            ),
            "{msg}"
        );
    }

    #[test]
    fn explains_conda_conflict() {
        let proj = project("[workspace]\nname = \"p\"\n[dependencies]\npython = \"3.14.*\"\n");
        let msg = explain_pixi_error(CONDA_CONFLICT, Some(&proj));
        assert_eq!(
            msg,
            "pixi could not solve the conda dependencies on win-64: tensorflow * cannot be installed. Every candidate needs something that is not available: numpy >=1.16.1,<2.0.0a0; python 3.6.*; python >=3.6,<3.7.0a0. Its builds need a different Python (this environment asks for Python 3.14): change the `python` pin with `pixi add python=3.X`, or pin a tensorflow version built for your Python; `pixi search tensorflow` lists the builds for this platform."
        );
    }

    #[test]
    fn explains_pypi_conflict() {
        let stderr = "Error:   × failed to solve the pypi requirements of environment 'default' for\n  │ platform 'win-64'\n  ╰─▶ Because a==1.0 depends on b<2 and you require b>=2, we can conclude that your requirements are unsatisfiable.\n";
        assert_eq!(
            explain_pixi_error(stderr, None),
            "pixi could not solve the PyPI dependencies on win-64: a==1.0 depends on b<2 and you require b>=2. Loosen or remove one of the pins named here."
        );
    }

    #[test]
    fn other_errors_are_quoted() {
        assert_eq!(
            explain_pixi_error(
                "Error:   × could not reach https://conda.anaconda.org\n",
                None
            ),
            "pixi failed: could not reach https://conda.anaconda.org"
        );
        assert!(explain_pixi_error("", None).starts_with("pixi failed without an error message"));
    }

    // --- assess_pixi ---

    use crate::core::platform::Arch;
    use crate::pypi::metadata::{DistFile, Info, PackageMetadata};
    use crate::pypi::FixtureSource;

    fn linux() -> Platform {
        Platform {
            os: Os::Linux,
            arch: Arch::X86_64,
        }
    }

    fn pkg(name: &str, rp: Option<&str>, wheels: &[&str]) -> PackageMetadata {
        PackageMetadata {
            info: Info {
                name: name.into(),
                version: "1.0".into(),
                requires_python: rp.map(str::to_string),
            },
            urls: wheels
                .iter()
                .map(|w| DistFile {
                    filename: w.to_string(),
                    packagetype: "bdist_wheel".into(),
                    requires_python: None,
                    yanked: false,
                })
                .collect(),
            versions: vec!["1.0".into()],
        }
    }

    /// Wheels for linux x86_64 on each listed CPython minor.
    fn cp_wheels(name: &str, minors: &[u8]) -> Vec<String> {
        minors
            .iter()
            .map(|m| format!("{name}-1.0-cp3{m}-cp3{m}-manylinux_2_17_x86_64.whl"))
            .collect()
    }

    fn source() -> FixtureSource {
        let mut src = FixtureSource::new();
        src.insert(
            "requests",
            pkg(
                "requests",
                Some(">=3.8"),
                &["requests-1.0-py3-none-any.whl"],
            ),
        );
        let tf = cp_wheels("tensorflow", &[9, 10, 11]);
        let tf: Vec<&str> = tf.iter().map(String::as_str).collect();
        src.insert("tensorflow", pkg("tensorflow", Some(">=3.9"), &tf));
        let legacy = cp_wheels("legacy", &[8, 9, 10]);
        let legacy: Vec<&str> = legacy.iter().map(String::as_str).collect();
        src.insert("legacy", pkg("legacy", Some(">=3.8"), &legacy));
        let shiny = cp_wheels("shiny", &[11, 12]);
        let shiny: Vec<&str> = shiny.iter().map(String::as_str).collect();
        src.insert("shiny", pkg("shiny", Some(">=3.11"), &shiny));
        src
    }

    fn manifest(python: &str, pypi: &str) -> PixiProject {
        project(&format!(
            "[workspace]\nname = \"p\"\nchannels = [\"conda-forge\"]\nplatforms = [\"linux-64\"]\n\n[dependencies]\npython = \"{python}\"\nnumpy = \">=1.26\"\n\n[pypi-dependencies]\n{pypi}\n"
        ))
    }

    #[tokio::test]
    async fn compatible_project_has_no_finding() {
        // numpy is a conda dependency, so it is not checked against PyPI.
        let proj = manifest("3.12.*", "requests = \"*\"");
        assert!(proj.pypi_deps.iter().all(|r| r.name != "numpy"));
        assert!(assess_pixi(&proj, None, &source(), linux())
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn names_the_package_and_the_python_to_switch_to() {
        let proj = manifest("3.12.*", "requests = \"*\"\ntensorflow = \"*\"");
        let findings = assess_pixi(&proj, None, &source(), linux()).await;
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Error);
        assert_eq!(
            f.title,
            "tensorflow does not support Python 3.12 in this pixi environment"
        );
        assert_eq!(
            f.detail,
            "pixi.toml sets python to `3.12.*` (Python 3.12), but tensorflow 1.0 ships no Python 3.12 wheel for Linux (its wheels cover 3.9–3.11). `pixi install` fails with \"no wheels with a matching Python ABI tag\" unless it can build from source. Run `pixi add python=3.11` to move the environment to Python 3.11, the newest version every PyPI dependency supports."
        );
    }

    #[tokio::test]
    async fn installed_python_wins_over_the_manifest() {
        let proj = manifest("3.11.*", "tensorflow = \"*\"");
        assert!(assess_pixi(&proj, None, &source(), linux())
            .await
            .is_empty());
        let findings = assess_pixi(&proj, Some(PyVersion::py3(12)), &source(), linux()).await;
        assert_eq!(findings.len(), 1);
        assert!(findings[0]
            .detail
            .starts_with("The pixi environment runs Python 3.12, but tensorflow"));
    }

    #[tokio::test]
    async fn requires_python_blocker_is_worded_as_such() {
        let proj = manifest("3.10.*", "shiny = \"*\"");
        let findings = assess_pixi(&proj, None, &source(), linux()).await;
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].detail.contains(
                "shiny 1.0 supports Python 3.11–3.12, requires_python `>=3.11`. `pixi install` cannot install it on this Python. Run `pixi add python=3.12`"
            ),
            "{}",
            findings[0].detail
        );
    }

    #[tokio::test]
    async fn conflicting_pair_is_named() {
        let proj = manifest(
            "3.12.*",
            "legacy = \"*\"\nshiny = \"*\"\nlocal = { path = \"./local\" }",
        );
        let findings = assess_pixi(&proj, None, &source(), linux()).await;
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(
            f.title,
            "pixi's PyPI dependencies conflict on Python version"
        );
        assert_eq!(
            f.detail,
            "legacy supports up to Python 3.10 but shiny requires at least Python 3.11. No single interpreter satisfies both. pixi cannot pick one Python for the environment, so `pixi add python=…` will not help; change or drop one of these PyPI dependencies. 1 PyPI dependency given as git, path or url was not checked."
        );
    }

    #[tokio::test]
    async fn unknown_python_gives_no_finding() {
        let proj = manifest("*", "tensorflow = \"*\"");
        assert!(assess_pixi(&proj, None, &source(), linux())
            .await
            .is_empty());
    }
}
