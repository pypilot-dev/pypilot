//! The data behind `pypilot explain`: why a package, or a whole project,
//! supports the Python versions it does.
//!
//! Nothing here decides anything new. A package's answer is what
//! [`PackageMetadata::analyze`] already computes (`requires_python`
//! intersected with the wheel tags for a platform); this module only keeps
//! the intermediate sets that `analyze` folds away, so the user can see which
//! of the two narrowed the range. A project's answer is the [`CompatReport`]
//! the solver already built, read one Python version at a time.
//!
//! Everything is a plain serializable value: the CLI renders it as a table or
//! as JSON (a bug-report artifact), and tests check it without a terminal.

use serde::Serialize;

use crate::core::platform::{Arch, Os, Platform};
use crate::core::project::Requirement;
use crate::pypi::metadata::{PackageAnalysis, PackageMetadata};
use crate::pypi::pyversion::{PyVersion, PyVersionSet, MAX_MINOR, MIN_MINOR};
use crate::pypi::wheel::WheelTags;
use crate::pypi::CompatReport;

/// Every platform PyPilot can reason about, in display order.
pub const PLATFORMS: [Platform; 6] = [
    Platform {
        os: Os::Linux,
        arch: Arch::X86_64,
    },
    Platform {
        os: Os::Linux,
        arch: Arch::Aarch64,
    },
    Platform {
        os: Os::Macos,
        arch: Arch::X86_64,
    },
    Platform {
        os: Os::Macos,
        arch: Arch::Aarch64,
    },
    Platform {
        os: Os::Windows,
        arch: Arch::X86_64,
    },
    Platform {
        os: Os::Windows,
        arch: Arch::Aarch64,
    },
];

/// Stable machine id for a platform: `linux-x64`, `macos-arm64`.
pub fn platform_id(p: &Platform) -> &'static str {
    match (p.os, p.arch) {
        (Os::Linux, Arch::X86_64) => "linux-x64",
        (Os::Linux, Arch::Aarch64) => "linux-arm64",
        (Os::Macos, Arch::X86_64) => "macos-x64",
        (Os::Macos, Arch::Aarch64) => "macos-arm64",
        (Os::Windows, Arch::X86_64) => "windows-x64",
        (Os::Windows, Arch::Aarch64) => "windows-arm64",
    }
}

/// Every Python minor in the tracked universe, oldest first.
fn universe() -> impl Iterator<Item = PyVersion> {
    (MIN_MINOR..=MAX_MINOR).map(PyVersion::py3)
}

fn versions(set: &PyVersionSet) -> Vec<String> {
    set.iter().map(|v| v.to_string()).collect()
}

/// Which input limits a range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Constraint {
    /// The package's declared `requires_python`.
    RequiresPython,
    /// The CPython tags of the wheels published for the platform.
    Wheels,
    /// Both stop at the same version.
    Both,
    /// Nothing installs on this platform at all.
    NoFiles,
    /// No limit inside the versions PyPilot tracks.
    None,
}

/// What happens when installing on one Python version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// A wheel exists: a plain binary install.
    Wheel,
    /// `requires_python` allows it but no wheel matches: pip builds the sdist.
    SourceBuild,
    /// `requires_python` rules it out.
    RequiresPython,
    /// `requires_python` allows it, but there is neither a wheel nor an sdist.
    NoFile,
}

/// The wheels one platform gets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlatformWheels {
    pub id: &'static str,
    pub label: &'static str,
    /// The platform this engine is running on.
    pub current: bool,
    /// The interpreter tags of the matching wheels: `cp39`, `cp314t`,
    /// `cp39-abi3` (3.9 and newer), `py3-none-any`.
    pub tags: Vec<String>,
    /// The Pythons those wheels cover, before `requires_python`.
    pub wheel_pythons: Vec<String>,
    /// What [`PackageMetadata::analyze`] concludes for this platform.
    pub supported: Vec<String>,
}

/// One end of the supported range and what set it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Bound {
    pub constraint: Constraint,
    /// The last supported Python at this end.
    pub python: Option<String>,
    pub detail: String,
}

/// One Python version on the current platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PythonRow {
    pub python: String,
    pub outcome: Outcome,
    /// Whether the engine counts it as supported (what `check` and `doctor`
    /// use). A source build is not, when other Pythons get wheels.
    pub supported: bool,
}

/// `pypilot explain <package>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PackageExplanation {
    /// The requirement as given: `mediapipe==0.10.14`.
    pub requirement: String,
    pub name: String,
    /// The release that was analysed.
    pub version: String,
    /// The newest release on PyPI.
    pub latest: String,
    /// Why this release: "newest release matching `==0.10.14` (latest is 0.10.35)".
    pub selection: String,
    pub requires_python: Option<String>,
    /// The Pythons `requires_python` admits.
    pub requires_python_allows: Vec<String>,
    /// The release has a source distribution.
    pub sdist: bool,
    pub platforms: Vec<PlatformWheels>,
    /// The platform this explanation is for.
    pub platform: &'static str,
    /// Supported Pythons here: the intersection the engine uses.
    pub supported: Vec<String>,
    pub supported_range: String,
    /// What limits the newest supported Python: the answer to "why not 3.13?".
    pub binding: Constraint,
    pub upper: Bound,
    pub lower: Bound,
    /// One sentence covering the above.
    pub summary: String,
    pub pythons: Vec<PythonRow>,
}

/// Why the analysed release was chosen.
fn selection(req: &Requirement, chosen: &str, latest: &str) -> String {
    if !req.is_pinned() {
        "newest release (no version constraint)".to_string()
    } else if chosen == latest {
        format!("newest release, and it satisfies `{}`", req.spec)
    } else {
        format!(
            "newest release matching `{}` (latest is {latest})",
            req.spec
        )
    }
}

/// Wheel tags for one platform, and the Pythons they cover.
fn platform_wheels(meta: &PackageMetadata, platform: &Platform) -> (Vec<String>, PyVersionSet) {
    let mut tags: Vec<(u8, String)> = Vec::new();
    let mut covered = PyVersionSet::empty();
    for file in meta.urls.iter().filter(|f| !f.yanked && f.is_wheel()) {
        let Some(wheel) = WheelTags::parse(&file.filename) else {
            continue;
        };
        let set = wheel.supported_on(platform);
        // Other platforms, or another interpreter (PyPy): not counted.
        if set.is_empty() {
            continue;
        }
        covered = covered.union(&set);
        for tag in wheel_labels(&wheel) {
            if !tags.iter().any(|(_, t)| *t == tag.1) {
                tags.push(tag);
            }
        }
    }
    tags.sort();
    (tags.into_iter().map(|(_, t)| t).collect(), covered)
}

/// Display labels for one wheel's interpreter tags, each with a sort key
/// (its CPython minor; generic tags sort last).
fn wheel_labels(wheel: &WheelTags) -> Vec<(u8, String)> {
    let abi3 = wheel.abi.iter().any(|a| a == "abi3");
    let generic = || {
        let platform = if wheel.platform.iter().any(|p| p == "any") {
            "any".to_string()
        } else {
            wheel.platform.join(".")
        };
        (
            u8::MAX,
            format!("{}-{}-{platform}", wheel.py.join("."), wheel.abi.join(".")),
        )
    };
    let mut out = Vec::new();
    for py in &wheel.py {
        let Some(minor) = py.strip_prefix("cp3").and_then(|m| m.parse::<u8>().ok()) else {
            if py.starts_with("py") {
                let g = generic();
                if !out.contains(&g) {
                    out.push(g);
                }
            }
            continue;
        };
        let free_threaded = wheel.abi.iter().any(|a| *a == format!("{py}t"));
        let label = if abi3 {
            format!("{py}-abi3")
        } else if free_threaded {
            format!("{py}t")
        } else {
            py.clone()
        };
        out.push((minor, label));
    }
    out
}

fn tag(v: PyVersion) -> String {
    v.cp_tag()
}

/// The upper end of the range on a platform with wheels.
fn upper_bound(declared: &PyVersionSet, wheels: &PyVersionSet, sdist: bool) -> Bound {
    let (Some(d), Some(w)) = (declared.max(), wheels.max()) else {
        return Bound {
            constraint: Constraint::None,
            python: None,
            detail: String::new(),
        };
    };
    let fallback = if sdist {
        "pip would try to build the sdist"
    } else {
        "there is no sdist to fall back on"
    };
    let declared_top = if d.minor == MAX_MINOR {
        "has no upper bound".to_string()
    } else {
        format!("allows up to {d}")
    };
    if w < d {
        let next = PyVersion::py3(w.minor + 1);
        Bound {
            constraint: Constraint::Wheels,
            python: Some(w.to_string()),
            detail: format!(
                "requires_python {declared_top}, but wheels stop at {}, so {next}+ has no wheel and {fallback}",
                tag(w)
            ),
        }
    } else if d < w {
        Bound {
            constraint: Constraint::RequiresPython,
            python: Some(d.to_string()),
            detail: format!(
                "wheels go up to {}, but requires_python stops at {d}",
                tag(w)
            ),
        }
    } else if d.minor == MAX_MINOR {
        Bound {
            constraint: Constraint::None,
            python: Some(d.to_string()),
            detail: format!("wheels reach {}, the newest Python PyPilot tracks", tag(w)),
        }
    } else {
        Bound {
            constraint: Constraint::Both,
            python: Some(d.to_string()),
            detail: format!("requires_python and the wheels both stop at {d}"),
        }
    }
}

/// The lower end of the range on a platform with wheels.
fn lower_bound(declared: &PyVersionSet, wheels: &PyVersionSet) -> Bound {
    let (Some(d), Some(w)) = (declared.min(), wheels.min()) else {
        return Bound {
            constraint: Constraint::None,
            python: None,
            detail: String::new(),
        };
    };
    if d < w {
        Bound {
            constraint: Constraint::Wheels,
            python: Some(w.to_string()),
            detail: format!(
                "requires_python allows {d}, but the oldest wheel is {}",
                tag(w)
            ),
        }
    } else if w < d {
        Bound {
            constraint: Constraint::RequiresPython,
            python: Some(d.to_string()),
            detail: format!("wheels start at {}, but requires_python needs {d}", tag(w)),
        }
    } else if d.minor == MIN_MINOR {
        Bound {
            constraint: Constraint::None,
            python: Some(d.to_string()),
            detail: format!("wheels reach back to {}", tag(w)),
        }
    } else {
        Bound {
            constraint: Constraint::Both,
            python: Some(d.to_string()),
            detail: format!("requires_python and the wheels both start at {d}"),
        }
    }
}

/// A bound set by `requires_python` alone (sdist-only, or no files at all).
fn declared_bound(declared: &PyVersionSet, upper: bool) -> Bound {
    let end = if upper {
        declared.max()
    } else {
        declared.min()
    };
    let open = if upper { MAX_MINOR } else { MIN_MINOR };
    match end {
        Some(v) if v.minor != open => Bound {
            constraint: Constraint::RequiresPython,
            python: Some(v.to_string()),
            detail: format!(
                "requires_python {} at {v}",
                if upper { "stops" } else { "starts" }
            ),
        },
        Some(v) => Bound {
            constraint: Constraint::None,
            python: Some(v.to_string()),
            detail: String::new(),
        },
        None => Bound {
            constraint: Constraint::RequiresPython,
            python: None,
            detail: "requires_python admits no Python PyPilot tracks".to_string(),
        },
    }
}

/// Explain one resolved release on `platform`.
///
/// `meta` is what [`crate::pypi::resolve`] returned for `req`; `latest` the
/// newest version on PyPI, for saying why an older release was picked.
pub fn explain_package(
    req: &Requirement,
    meta: &PackageMetadata,
    latest: &str,
    platform: &Platform,
) -> PackageExplanation {
    let analysis = meta.analyze(platform);
    let declared = analysis.declared();
    let sdist = meta.urls.iter().any(|f| !f.yanked && f.is_sdist());

    let platforms = PLATFORMS
        .iter()
        .map(|p| {
            let (tags, covered) = platform_wheels(meta, p);
            PlatformWheels {
                id: platform_id(p),
                label: p.label(),
                current: p == platform,
                tags,
                wheel_pythons: versions(&covered),
                supported: versions(&meta.analyze(p).supported),
            }
        })
        .collect();

    let (_, here_wheels) = platform_wheels(meta, platform);
    let here = platform.label();

    let (upper, lower) = if analysis.no_files_for_platform {
        let detail = analysis.no_files_message(here).unwrap_or_default();
        let none = Bound {
            constraint: Constraint::NoFiles,
            python: None,
            detail,
        };
        (none.clone(), none)
    } else if analysis.has_platform_wheels {
        (
            upper_bound(&declared, &here_wheels, sdist),
            lower_bound(&declared, &here_wheels),
        )
    } else {
        (
            declared_bound(&declared, true),
            declared_bound(&declared, false),
        )
    };

    let pythons = universe()
        .map(|py| {
            let outcome = if !declared.contains(py) {
                Outcome::RequiresPython
            } else if here_wheels.contains(py) {
                Outcome::Wheel
            } else if sdist {
                Outcome::SourceBuild
            } else {
                Outcome::NoFile
            };
            PythonRow {
                python: py.to_string(),
                outcome,
                supported: analysis.supported.contains(py),
            }
        })
        .collect();

    let summary = package_summary(&analysis, &upper, &lower, here, &declared);

    PackageExplanation {
        requirement: req.to_string(),
        name: analysis.name.clone(),
        version: analysis.version.clone(),
        latest: latest.to_string(),
        selection: selection(req, &analysis.version, latest),
        requires_python: analysis.requires_python.clone(),
        requires_python_allows: versions(&declared),
        sdist,
        platforms,
        platform: here,
        supported: versions(&analysis.supported),
        supported_range: analysis.supported.to_range_string(),
        binding: upper.constraint,
        upper,
        lower,
        summary,
        pythons,
    }
}

fn package_summary(
    a: &PackageAnalysis,
    upper: &Bound,
    lower: &Bound,
    here: &str,
    declared: &PyVersionSet,
) -> String {
    if a.no_files_for_platform {
        return upper.detail.clone();
    }
    let range = a.supported.to_range_string();
    if !a.has_platform_wheels {
        let what = if a.sdist_only {
            format!("{} {} has no wheel for {here}, so pip builds the sdist; requires_python is the only limit", a.name, a.version)
        } else {
            format!(
                "{} {} publishes no files, so only requires_python is known",
                a.name, a.version
            )
        };
        return format!("{what}: Python {}.", declared.to_range_string());
    }
    let mut out = format!("{} {} supports Python {range} on {here}", a.name, a.version);
    let parts: Vec<&str> = [upper, lower]
        .into_iter()
        .filter(|b| {
            matches!(
                b.constraint,
                Constraint::Wheels | Constraint::RequiresPython
            )
        })
        .map(|b| b.detail.as_str())
        .collect();
    if parts.is_empty() {
        out.push('.');
    } else {
        out.push_str(": ");
        out.push_str(&parts.join("; "));
        out.push('.');
    }
    out
}

/// Why one package rules out one Python.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockReason {
    RequiresPython,
    NoWheel,
    NoFiles,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Blocker {
    pub package: String,
    pub version: String,
    pub reason: BlockReason,
    /// "wheels end at cp312", "requires_python <3.13".
    pub detail: String,
}

/// One dependency in the project table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PackageRow {
    pub name: String,
    pub version: String,
    pub requires_python: Option<String>,
    pub supported: Vec<String>,
    pub supported_range: String,
    /// "sdist only", "no files for Windows x64", or empty.
    pub note: String,
}

/// One Python version across the whole project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectPythonRow {
    pub python: String,
    pub ok: bool,
    pub blockers: Vec<Blocker>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Unresolved {
    pub requirement: String,
    pub error: String,
}

/// `pypilot explain` in a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectExplanation {
    pub platform: &'static str,
    /// The project's own `requires-python`, as declared (informational: the
    /// dependencies decide the range).
    pub requires_python: Option<String>,
    /// The intersection of every dependency's supported set.
    pub supported: Vec<String>,
    pub supported_range: String,
    /// The Python the solver picks.
    pub recommended: Option<String>,
    /// The solver's recommendation line (mentions pixi's spec when relevant).
    pub recommendation: Option<String>,
    /// The next Python above the recommended one, and what blocks it: the
    /// binding constraint. Empty when nothing newer is tracked.
    pub binding_python: Option<String>,
    pub binding: Vec<Blocker>,
    pub packages: Vec<PackageRow>,
    pub pythons: Vec<ProjectPythonRow>,
    /// The solver's conflict explanations when no Python fits.
    pub conflicts: Vec<String>,
    pub unresolved: Vec<Unresolved>,
}

/// Why `p` excludes `py` (assumes it does).
fn blocker(p: &PackageAnalysis, py: PyVersion, here: &str) -> Blocker {
    let (reason, detail) = if p.no_files_for_platform {
        (BlockReason::NoFiles, format!("no files for {here}"))
    } else if !p.declared().contains(py) {
        (
            BlockReason::RequiresPython,
            format!(
                "requires_python {}",
                p.requires_python.as_deref().unwrap_or("").trim()
            ),
        )
    } else {
        let detail = match (p.supported.min(), p.supported.max()) {
            (_, Some(max)) if py > max => format!("wheels end at {}", tag(max)),
            (Some(min), _) if py < min => format!("wheels start at {}", tag(min)),
            _ => format!("no {} wheel", tag(py)),
        };
        (BlockReason::NoWheel, detail)
    };
    Blocker {
        package: p.name.clone(),
        version: p.version.clone(),
        reason,
        detail,
    }
}

fn blockers(compat: &CompatReport, py: PyVersion) -> Vec<Blocker> {
    let here = compat.platform.label();
    compat
        .blockers_for(py)
        .into_iter()
        .map(|p| blocker(p, py, here))
        .collect()
}

/// Explain a project's [`CompatReport`]. `recommended` and `recommendation`
/// come from the solver ([`crate::core::solver::synthesize`],
/// [`crate::core::solver::recommendation`]) so the two never disagree.
pub fn explain_project(
    compat: &CompatReport,
    requires_python: Option<&str>,
    recommended: Option<PyVersion>,
    recommendation: Option<String>,
) -> ProjectExplanation {
    let here = compat.platform.label();
    let packages = compat
        .per_package
        .iter()
        .map(|p| PackageRow {
            name: p.name.clone(),
            version: p.version.clone(),
            requires_python: p.requires_python.clone(),
            supported: versions(&p.supported),
            supported_range: p.supported.to_range_string(),
            note: if p.no_files_for_platform {
                format!("no files for {here}")
            } else if p.sdist_only {
                "sdist only (builds from source)".to_string()
            } else {
                String::new()
            },
        })
        .collect();

    let pythons = universe()
        .map(|py| {
            let blockers = blockers(compat, py);
            ProjectPythonRow {
                python: py.to_string(),
                ok: blockers.is_empty() && compat.is_compatible(py),
                blockers,
            }
        })
        .collect();

    let next = recommended
        .filter(|t| t.minor < MAX_MINOR)
        .map(|t| PyVersion::py3(t.minor + 1));

    ProjectExplanation {
        platform: here,
        requires_python: requires_python.map(str::to_string),
        supported: versions(&compat.intersection),
        supported_range: compat.intersection.to_range_string(),
        recommended: recommended.map(|v| v.to_string()),
        recommendation,
        binding_python: next.map(|v| v.to_string()),
        binding: next.map(|v| blockers(compat, v)).unwrap_or_default(),
        packages,
        pythons,
        conflicts: compat
            .conflicts
            .iter()
            .map(|c| c.explanation.clone())
            .collect(),
        unresolved: compat
            .unresolved
            .iter()
            .map(|(r, e)| Unresolved {
                requirement: r.clone(),
                error: e.clone(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pypi::metadata::{DistFile, Info};

    fn linux() -> Platform {
        PLATFORMS[0]
    }

    fn file(name: &str) -> DistFile {
        DistFile {
            filename: name.to_string(),
            packagetype: if name.ends_with(".whl") {
                "bdist_wheel".into()
            } else {
                "sdist".into()
            },
            requires_python: None,
            yanked: false,
        }
    }

    fn meta(rp: Option<&str>, files: &[&str]) -> PackageMetadata {
        PackageMetadata {
            info: Info {
                name: "pkg".into(),
                version: "1.0".into(),
                requires_python: rp.map(str::to_string),
            },
            urls: files.iter().map(|f| file(f)).collect(),
            versions: vec!["1.0".into()],
        }
    }

    fn explain(m: &PackageMetadata) -> PackageExplanation {
        explain_package(&Requirement::any("pkg"), m, "1.0", &linux())
    }

    #[test]
    fn wheels_bind_when_requires_python_is_open() {
        let m = meta(
            Some(">=3.8"),
            &[
                "pkg-1.0-cp39-cp39-manylinux_2_17_x86_64.whl",
                "pkg-1.0-cp312-cp312-manylinux_2_17_x86_64.whl",
                "pkg-1.0.tar.gz",
            ],
        );
        let e = explain(&m);
        assert_eq!(e.binding, Constraint::Wheels);
        assert_eq!(e.upper.python.as_deref(), Some("3.12"));
        assert!(e.upper.detail.contains("has no upper bound"));
        assert!(e.upper.detail.contains("wheels stop at cp312"));
        assert!(e.upper.detail.contains("3.13+ has no wheel"));
        assert!(e.upper.detail.contains("sdist"));
        assert_eq!(e.lower.constraint, Constraint::Wheels);
        let row = |v: &str| e.pythons.iter().find(|r| r.python == v).unwrap().clone();
        assert_eq!(row("3.7").outcome, Outcome::RequiresPython);
        assert_eq!(row("3.9").outcome, Outcome::Wheel);
        assert_eq!(row("3.10").outcome, Outcome::SourceBuild);
        assert!(!row("3.10").supported);
        assert_eq!(row("3.13").outcome, Outcome::SourceBuild);
    }

    #[test]
    fn requires_python_binds_when_it_caps_below_the_wheels() {
        let m = meta(
            Some(">=3.9,<3.12"),
            &["pkg-1.0-cp39-abi3-manylinux_2_17_x86_64.whl"],
        );
        let e = explain(&m);
        assert_eq!(e.binding, Constraint::RequiresPython);
        assert_eq!(e.supported_range, "3.9–3.11");
        assert_eq!(e.platforms[0].tags, vec!["cp39-abi3"]);
        assert_eq!(e.lower.constraint, Constraint::Both);
        assert_eq!(
            e.pythons
                .iter()
                .find(|r| r.python == "3.13")
                .unwrap()
                .outcome,
            Outcome::RequiresPython
        );
    }

    #[test]
    fn pure_python_wheels_have_no_binding_limit() {
        let m = meta(None, &["pkg-1.0-py3-none-any.whl"]);
        let e = explain(&m);
        assert_eq!(e.binding, Constraint::None);
        assert!(e.platforms.iter().all(|p| p.tags == ["py3-none-any"]));
        assert_eq!(e.supported.len(), (MAX_MINOR - MIN_MINOR + 1) as usize);
    }

    #[test]
    fn foreign_wheels_without_sdist_are_no_files() {
        let m = meta(Some(">=3.9"), &["pkg-1.0-cp312-cp312-win_amd64.whl"]);
        let e = explain(&m);
        assert_eq!(e.binding, Constraint::NoFiles);
        assert!(e.supported.is_empty());
        assert!(e.summary.contains("cannot be installed"));
        let win = e.platforms.iter().find(|p| p.id == "windows-x64").unwrap();
        assert_eq!(win.tags, vec!["cp312"]);
        assert_eq!(win.supported, vec!["3.12"]);
        assert!(e.pythons.iter().all(|r| !r.supported));
    }

    #[test]
    fn sdist_only_is_limited_by_requires_python() {
        let m = meta(Some(">=3.10"), &["pkg-1.0.tar.gz"]);
        let e = explain(&m);
        assert!(e.sdist);
        assert_eq!(e.binding, Constraint::None);
        assert_eq!(e.lower.constraint, Constraint::RequiresPython);
        assert!(e.summary.contains("pip builds the sdist"));
    }

    #[test]
    fn free_threaded_and_pypy_tags() {
        let m = meta(
            Some(">=3.13"),
            &[
                "pkg-1.0-cp314-cp314t-manylinux_2_28_x86_64.whl",
                "pkg-1.0-cp313-cp313-manylinux_2_28_x86_64.whl",
                "pkg-1.0-pp310-pypy310_pp73-manylinux_2_28_x86_64.whl",
            ],
        );
        let e = explain(&m);
        assert_eq!(e.platforms[0].tags, vec!["cp313", "cp314t"]);
    }

    #[test]
    fn selection_says_why() {
        let pinned = Requirement {
            name: "pkg".into(),
            spec: "==1.0".into(),
        };
        assert_eq!(
            selection(&pinned, "1.0", "2.0"),
            "newest release matching `==1.0` (latest is 2.0)"
        );
        assert!(selection(&pinned, "1.0", "1.0").contains("satisfies"));
        assert!(selection(&Requirement::any("pkg"), "1.0", "1.0").contains("no version"));
    }
}
