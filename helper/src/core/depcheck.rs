//! A light `pip check`, read off disk: installed packages whose declared
//! requirements exclude the version of a dependency that is actually installed.
//!
//! The case that motivates it is NumPy 2. Extensions compiled against NumPy 1.x
//! install without complaint next to numpy 2.x and then fail at import time
//! with "A module that was compiled using NumPy 1.x cannot be run in NumPy 2.x"
//! or "numpy.dtype size changed, may indicate binary incompatibility". Those
//! packages say so in their metadata (`Requires-Dist: numpy<2`), and the
//! installed numpy version is in its `.dist-info` directory name, so the clash
//! is visible statically before anything runs.
//!
//! Everything here is local: one directory listing per `site-packages` and the
//! header of each `METADATA` file. No interpreter, no network.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use crate::core::installed::{self, Installed};
use crate::core::project::normalize_name;
use crate::core::{Finding, FixKind, Severity};
use crate::pypi::pyversion::PyVersion;
use crate::pypi::version::{PackageVersion, VersionSpec};

/// Stop after this many distributions, so a pathological venv cannot stall a scan.
const MAX_DISTS: usize = 2000;
/// Read at most this much of one `METADATA` file. The headers come first and
/// are small; the long description after them is never needed.
const MAX_METADATA_BYTES: u64 = 256 * 1024;
/// Generic conflicts reported at most; NumPy's is always one grouped finding.
const MAX_GENERIC_FINDINGS: usize = 8;

/// One installed package whose requirement excludes an installed dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// The package that declares the requirement, normalized.
    pub package: String,
    pub package_version: Option<String>,
    /// The dependency it constrains, normalized.
    pub dependency: String,
    /// The specifier as written, e.g. `<2,>=1.21`.
    pub spec: String,
    /// The dependency version that is installed and falls outside `spec`.
    pub installed: String,
}

/// Check a venv and turn any conflicts into findings.
pub fn check(venv: &Path, python: Option<PyVersion>) -> Vec<Finding> {
    let installed = installed::scan(venv);
    if installed.count() == 0 {
        return Vec::new();
    }
    findings(&conflicts(venv, &installed, python))
}

/// Every installed distribution whose `Requires-Dist` excludes an installed
/// dependency version.
///
/// Requirements are skipped rather than guessed at whenever they cannot be
/// judged: an environment marker this module does not evaluate, a URL
/// requirement, a dependency that is not installed, or a version that does not
/// parse. A false alarm about a working environment is worse than silence.
pub fn conflicts(venv: &Path, installed: &Installed, python: Option<PyVersion>) -> Vec<Conflict> {
    conflicts_in(&requirements(venv, python), installed)
}

/// One installed distribution's requirement on another, whose marker holds
/// on this Python.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requires {
    /// The package that declares the requirement, normalized.
    pub package: String,
    pub package_version: Option<String>,
    /// The dependency it constrains, normalized.
    pub dependency: String,
    /// The specifier as written, e.g. `<2,>=1.21`; never empty.
    pub spec: String,
}

/// Every versioned `Requires-Dist` of the venv's distributions that applies
/// on `python`. Markers this module cannot evaluate are skipped.
pub fn requirements(venv: &Path, python: Option<PyVersion>) -> Vec<Requires> {
    let mut out = Vec::new();
    for (package, package_version, metadata) in dist_infos(venv) {
        for line in requires_dist(&metadata) {
            let Some(req) = parse_requirement(&line) else {
                continue;
            };
            if req.name == package || req.spec.is_empty() {
                continue;
            }
            if let Some(marker) = &req.marker {
                if evaluate_marker(marker, python) != Some(true) {
                    continue;
                }
            }
            out.push(Requires {
                package: package.clone(),
                package_version: package_version.clone(),
                dependency: req.name,
                spec: req.spec,
            });
        }
    }
    out
}

/// The [`requirements`] that exclude the installed version of their
/// dependency.
pub fn conflicts_in(reqs: &[Requires], installed: &Installed) -> Vec<Conflict> {
    let mut out = Vec::new();
    for req in reqs {
        let Some(have) = installed.version_of(&req.dependency) else {
            continue;
        };
        let Some(parsed) = PackageVersion::parse(&have) else {
            continue;
        };
        if VersionSpec::parse(&req.spec).admits(&parsed) {
            continue;
        }
        out.push(Conflict {
            package: req.package.clone(),
            package_version: req.package_version.clone(),
            dependency: req.dependency.clone(),
            spec: req.spec.clone(),
            installed: have,
        });
    }
    out
}

/// Findings for a list of conflicts. Packages built for NumPy 1.x under an
/// installed NumPy 2 get one grouped warning with the ABI explanation; anything
/// else gets the generic `pip check` style message.
pub fn findings(conflicts: &[Conflict]) -> Vec<Finding> {
    let (abi, generic): (Vec<&Conflict>, Vec<&Conflict>) =
        conflicts.iter().partition(|c| is_numpy_abi_break(c));

    let mut out = Vec::new();
    if let Some(f) = numpy_finding(&abi) {
        out.push(f);
    }
    out.extend(
        generic
            .into_iter()
            .take(MAX_GENERIC_FINDINGS)
            .map(generic_finding),
    );
    out
}

/// numpy 2.x is installed and the package wants a 1.x release, which for a
/// compiled extension means it was built against the NumPy 1 ABI.
fn is_numpy_abi_break(c: &Conflict) -> bool {
    if c.dependency != "numpy" {
        return false;
    }
    let (Some(installed), Some(two), Some(numpy1)) = (
        PackageVersion::parse(&c.installed),
        PackageVersion::parse("2"),
        PackageVersion::parse("1.26.4"),
    ) else {
        return false;
    };
    installed >= two && VersionSpec::parse(&c.spec).admits(&numpy1)
}

fn numpy_finding(abi: &[&Conflict]) -> Option<Finding> {
    let first = abi.first()?;
    let numpy = &first.installed;
    let title = if abi.len() == 1 {
        format!(
            "{} needs numpy{}, but numpy {numpy} is installed",
            label(first),
            first.spec
        )
    } else {
        format!(
            "{} packages need NumPy 1.x, but numpy {numpy} is installed",
            abi.len()
        )
    };
    let list = abi
        .iter()
        .map(|c| format!("{} (numpy{})", label(c), c.spec))
        .collect::<Vec<_>>()
        .join(", ");
    let names = abi
        .iter()
        .map(|c| c.package.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let detail = format!(
        "{list} declared a requirement on NumPy 1.x. Extensions built against NumPy 1.x \
         fail to import under NumPy 2 with \"A module that was compiled using NumPy 1.x \
         cannot be run in NumPy 2.x\" or \"numpy.dtype size changed, may indicate binary \
         incompatibility\". Fix: pin `numpy<2` in your dependencies and reinstall, or \
         upgrade {names} to a release built for NumPy 2."
    );
    Some(Finding {
        severity: Severity::Warning,
        title,
        detail,
        fix: FixKind::Manual,
    })
}

fn generic_finding(c: &Conflict) -> Finding {
    Finding {
        severity: Severity::Warning,
        title: format!(
            "{} needs {}{}, but {} {} is installed",
            label(c),
            c.dependency,
            c.spec,
            c.dependency,
            c.installed
        ),
        detail: format!(
            "{}'s metadata excludes the installed {} {}, so importing it may fail or \
             misbehave. Fix: install a {} matching `{}`, or upgrade {} to a release that \
             supports {} {}.",
            c.package,
            c.dependency,
            c.installed,
            c.dependency,
            c.spec,
            c.package,
            c.dependency,
            c.installed
        ),
        fix: FixKind::Manual,
    }
}

/// `opencv-python 4.8.0.76`, or just the name when the version is unknown.
fn label(c: &Conflict) -> String {
    match &c.package_version {
        Some(v) => format!("{} {v}", c.package),
        None => c.package.clone(),
    }
}

/// `(name, version, METADATA path)` for each `.dist-info` in the venv, sorted
/// so findings come out in a stable order. `.egg-info` has no `METADATA`
/// requirements to read, so it is left out.
fn dist_infos(venv: &Path) -> Vec<(String, Option<String>, PathBuf)> {
    let mut out = Vec::new();
    for dir in installed::site_packages_dirs(venv) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            if !file_name.ends_with(".dist-info") {
                continue;
            }
            let Some(name) = installed::distribution_name(&file_name) else {
                continue;
            };
            let version = installed::distribution_version(&file_name);
            out.push((name, version, entry.path().join("METADATA")));
            if out.len() >= MAX_DISTS {
                break;
            }
        }
    }
    out.sort();
    out
}

/// The `Requires-Dist` values from a `METADATA` file's header block.
///
/// Reading stops at the blank line that ends the headers, so a package with a
/// long README costs no more than one with none.
fn requires_dist(path: &Path) -> Vec<String> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let reader = BufReader::new(file.take(MAX_METADATA_BYTES));
    let mut out = Vec::new();
    for line in reader.lines() {
        let Ok(line) = line else {
            break;
        };
        if line.trim().is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("Requires-Dist") {
                out.push(value.trim().to_string());
            }
        }
    }
    out
}

#[derive(Debug, PartialEq, Eq)]
struct Requirement {
    name: String,
    spec: String,
    marker: Option<String>,
}

/// Parse a `Requires-Dist` value such as `numpy<2,>=1.21; python_version>="3.9"`
/// or the older `numpy (<2.0,>=1.21)`. `None` for URL requirements and anything
/// without a name.
fn parse_requirement(line: &str) -> Option<Requirement> {
    let (req, marker) = match line.split_once(';') {
        Some((r, m)) => (r.trim(), Some(m.trim().to_string())),
        None => (line.trim(), None),
    };
    let name_end = req
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
        .unwrap_or(req.len());
    let (name, mut rest) = req.split_at(name_end);
    if name.is_empty() {
        return None;
    }
    rest = rest.trim_start();
    if let Some(after) = rest.strip_prefix('[') {
        rest = after
            .split_once(']')
            .map(|(_, r)| r)
            .unwrap_or("")
            .trim_start();
    }
    if rest.starts_with('@') {
        return None;
    }
    let spec: String = rest
        .trim_start_matches('(')
        .trim_end_matches(')')
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    Some(Requirement {
        name: normalize_name(name),
        spec,
        marker: marker.filter(|m| !m.is_empty()),
    })
}

/// Evaluate a PEP 508 environment marker for this venv.
///
/// Handles `and` / `or` chains over `python_version`, `extra`, `sys_platform`,
/// `platform_system` and `os_name`. `None` means "cannot tell" (parentheses,
/// other variables, an unknown Python), and callers skip the requirement.
fn evaluate_marker(marker: &str, python: Option<PyVersion>) -> Option<bool> {
    if marker.contains('(') {
        return None;
    }
    let mut result = Some(false);
    for disjunct in marker.split(" or ") {
        let mut all = Some(true);
        for atom in disjunct.split(" and ") {
            match evaluate_atom(atom.trim(), python) {
                Some(true) => {}
                Some(false) => {
                    all = Some(false);
                    break;
                }
                None => all = None,
            }
        }
        match all {
            Some(true) => return Some(true),
            None => result = None,
            Some(false) => {}
        }
    }
    result
}

/// One `variable op "value"` comparison (either side may be the literal).
fn evaluate_atom(atom: &str, python: Option<PyVersion>) -> Option<bool> {
    const OPS: [&str; 6] = ["==", "!=", "<=", ">=", "<", ">"];
    let (pos, op) = OPS
        .iter()
        .filter_map(|op| atom.find(op).map(|i| (i, *op)))
        .min_by_key(|(i, op)| (*i, std::cmp::Reverse(op.len())))?;
    let left = atom[..pos].trim();
    let right = atom[pos + op.len()..].trim();
    let unquote = |s: &str| {
        let s = s.trim();
        let quoted = s.len() >= 2
            && ((s.starts_with('"') && s.ends_with('"'))
                || (s.starts_with('\'') && s.ends_with('\'')));
        quoted.then(|| s[1..s.len() - 1].to_string())
    };
    // Normalize to `variable op literal`; a reversed comparison flips the op.
    let (var, value, op) = match (unquote(left), unquote(right)) {
        (None, Some(v)) => (left, v, op),
        (Some(v), None) => (right, v, flip(op)),
        _ => return None,
    };

    match var {
        "python_version" => {
            let py = python?;
            let have = PackageVersion::parse(&py.to_string())?;
            Some(VersionSpec::parse(&format!("{op}{value}")).admits(&have))
        }
        // PyPilot reasons about the base install, so no extra is ever active.
        "extra" => match op {
            "==" => Some(false),
            "!=" => Some(true),
            _ => None,
        },
        "sys_platform" => compare_str(op, current("sys_platform")?, &value),
        "platform_system" => compare_str(op, current("platform_system")?, &value),
        "os_name" => compare_str(op, current("os_name")?, &value),
        _ => None,
    }
}

fn flip(op: &str) -> &str {
    match op {
        "<" => ">",
        ">" => "<",
        "<=" => ">=",
        ">=" => "<=",
        other => other,
    }
}

fn compare_str(op: &str, have: &str, want: &str) -> Option<bool> {
    match op {
        "==" => Some(have == want),
        "!=" => Some(have != want),
        _ => None,
    }
}

/// The marker value of a platform variable on the machine running PyPilot,
/// which is the machine the venv runs on.
fn current(var: &str) -> Option<&'static str> {
    let (sys, system, os) = if cfg!(windows) {
        ("win32", "Windows", "nt")
    } else if cfg!(target_os = "macos") {
        ("darwin", "Darwin", "posix")
    } else if cfg!(target_os = "linux") {
        ("linux", "Linux", "posix")
    } else {
        return None;
    };
    match var {
        "sys_platform" => Some(sys),
        "platform_system" => Some(system),
        "os_name" => Some(os),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway venv with a Windows-style `Lib/site-packages`, which
    /// `site_packages_dirs` finds on every platform.
    struct Venv(PathBuf);

    impl Venv {
        fn new(tag: &str) -> Venv {
            let root =
                std::env::temp_dir().join(format!("pypilot-depcheck-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("Lib").join("site-packages")).unwrap();
            Venv(root)
        }

        fn dist(&self, dir: &str, requires: &[&str]) -> &Venv {
            let path = self.0.join("Lib").join("site-packages").join(dir);
            std::fs::create_dir_all(&path).unwrap();
            let mut text = String::from("Metadata-Version: 2.1\nName: x\n");
            for r in requires {
                text.push_str(&format!("Requires-Dist: {r}\n"));
            }
            // The body must never be read as headers.
            text.push_str("\nRequires-Dist: numpy<1\n");
            std::fs::write(path.join("METADATA"), text).unwrap();
            self
        }
    }

    impl Drop for Venv {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const PY312: Option<PyVersion> = Some(PyVersion::py3(12));

    #[test]
    fn numpy_1_package_under_numpy_2_is_one_abi_warning() {
        let venv = Venv::new("abi");
        venv.dist("numpy-2.1.0.dist-info", &[])
            .dist(
                "opencv_python-4.8.0.76.dist-info",
                &["numpy (<2.0,>=1.21.2) ; python_version >= \"3.10\""],
            )
            .dist("requests-2.32.3.dist-info", &["urllib3<3,>=1.21.1"])
            .dist("urllib3-2.2.3.dist-info", &[]);

        let found = check(&venv.0, PY312);
        assert_eq!(found.len(), 1, "{found:?}");
        let f = &found[0];
        assert_eq!(f.severity, Severity::Warning);
        assert_eq!(
            f.title,
            "opencv-python 4.8.0.76 needs numpy<2.0,>=1.21.2, but numpy 2.1.0 is installed"
        );
        assert!(f.detail.contains("cannot be run in NumPy 2.x"));
        assert!(f.detail.contains("pin `numpy<2`"));
        assert!(f.detail.contains("upgrade opencv-python"));
    }

    #[test]
    fn several_numpy_1_packages_are_grouped() {
        let venv = Venv::new("group");
        venv.dist("numpy-2.0.2.dist-info", &[])
            .dist("opencv_python-4.8.0.76.dist-info", &["numpy<2"])
            .dist("scikit_image-0.21.0.dist-info", &["numpy>=1.21.1,<2"]);

        let found = check(&venv.0, PY312);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            found[0].title,
            "2 packages need NumPy 1.x, but numpy 2.0.2 is installed"
        );
        assert!(found[0].detail.contains("opencv-python, scikit-image"));
    }

    #[test]
    fn satisfied_requirements_are_silent() {
        let venv = Venv::new("ok");
        venv.dist("numpy-1.26.4.dist-info", &[])
            .dist("opencv_python-4.8.0.76.dist-info", &["numpy<2,>=1.21"])
            .dist(
                "pandas-2.2.3.dist-info",
                &["numpy>=1.22.4; python_version < \"3.11\""],
            );
        assert!(check(&venv.0, PY312).is_empty());
    }

    #[test]
    fn other_conflicts_get_the_generic_message() {
        let venv = Venv::new("generic");
        venv.dist("numpy-1.24.0.dist-info", &[])
            .dist(
                "pandas-2.2.3.dist-info",
                &["numpy>=1.26.0; python_version >= \"3.12\""],
            )
            .dist("urllib3-1.26.20.dist-info", &[])
            .dist("botocore-1.20.0.dist-info", &["urllib3<1.26,>=1.25.4"]);

        let found = check(&venv.0, PY312);
        let titles: Vec<&str> = found.iter().map(|f| f.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "botocore 1.20.0 needs urllib3<1.26,>=1.25.4, but urllib3 1.26.20 is installed",
                "pandas 2.2.3 needs numpy>=1.26.0, but numpy 1.24.0 is installed",
            ]
        );
    }

    #[test]
    fn unjudgeable_requirements_are_skipped() {
        let venv = Venv::new("skip");
        venv.dist("numpy-2.1.0.dist-info", &[]).dist(
            "lib-1.0.dist-info",
            &[
                "numpy<2; extra == \"legacy\"",
                "numpy<2; platform_machine == \"arm64\"",
                "numpy<2; (python_version < \"3.9\")",
                "numpy @ https://example.com/numpy.whl",
                "missing-dep<1",
            ],
        );
        assert!(check(&venv.0, PY312).is_empty());
        // No Python version known: a python_version marker cannot be judged.
        let venv = Venv::new("nopy");
        venv.dist("numpy-2.1.0.dist-info", &[])
            .dist("lib-1.0.dist-info", &["numpy<2; python_version >= \"3.9\""]);
        assert!(check(&venv.0, None).is_empty());
        assert_eq!(check(&venv.0, PY312).len(), 1);
    }

    #[test]
    fn parses_requirement_forms() {
        assert_eq!(
            parse_requirement("numpy (<2.0,>=1.21)"),
            Some(Requirement {
                name: "numpy".into(),
                spec: "<2.0,>=1.21".into(),
                marker: None,
            })
        );
        let r =
            parse_requirement("Opencv_Python[contrib] >= 4.0 ; sys_platform == 'win32'").unwrap();
        assert_eq!(r.name, "opencv-python");
        assert_eq!(r.spec, ">=4.0");
        assert_eq!(r.marker.as_deref(), Some("sys_platform == 'win32'"));
        assert!(parse_requirement("pkg @ file:///x").is_none());
    }

    #[test]
    fn evaluates_markers() {
        let py = PY312;
        assert_eq!(
            evaluate_marker("python_version >= \"3.10\"", py),
            Some(true)
        );
        assert_eq!(evaluate_marker("python_version < '3.10'", py), Some(false));
        assert_eq!(
            evaluate_marker("\"3.10\" <= python_version", py),
            Some(true)
        );
        assert_eq!(
            evaluate_marker(
                "python_version <= \"3.9\" and platform_machine == \"arm64\"",
                py
            ),
            Some(false)
        );
        assert_eq!(
            evaluate_marker(
                "python_version >= \"3.9\" and platform_machine == \"arm64\"",
                py
            ),
            None
        );
        assert_eq!(
            evaluate_marker("extra == \"dev\" or python_version >= \"3.8\"", py),
            Some(true)
        );
        assert_eq!(evaluate_marker("extra == \"dev\"", py), Some(false));
        let os = if cfg!(windows) { "win32" } else { "nope" };
        assert_eq!(
            evaluate_marker(&format!("sys_platform == \"{os}\""), py),
            Some(cfg!(windows))
        );
    }
}
