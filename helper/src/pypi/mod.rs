//! F3 — the PyPI metadata engine (the generic "mediapipe problem" solver).
//!
//! Given a set of dependency names and the current platform, compute the
//! intersection of Python versions every dependency can run on. When that
//! intersection is empty, name the conflicting pair explicitly — that's exactly
//! where beginners give up ("A needs ≥3.11, B tops out at 3.10").
//!
//! No per-library hardcoding: everything is derived from PyPI metadata + wheel
//! tags. Self-contained — knows nothing of uv, LSP, or the editor.

pub mod cache;
pub mod client;
pub mod explain;
pub mod metadata;
pub mod pyversion;
pub mod version;
pub mod wheel;

use futures::future::join_all;

pub use client::{FixtureSource, MetadataSource, PyPiClient};
pub use metadata::PackageAnalysis;
pub use pyversion::{PyVersion, PyVersionSet};

use crate::core::platform::Platform;
use crate::core::project::Requirement;
use crate::pypi::version::VersionSpec;

/// A pair of packages whose supported-Python sets don't overlap.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ConflictPair {
    pub low: PackageAnalysis,
    pub high: PackageAnalysis,
    /// Human explanation, e.g. "opencv tops out at 3.10; ml-lib needs ≥3.11".
    pub explanation: String,
}

/// The full compatibility picture across a project's dependencies.
#[derive(Debug, Clone)]
pub struct CompatReport {
    pub platform: Platform,
    pub per_package: Vec<PackageAnalysis>,
    /// Packages PyPI has no project for (name → error string): a typo, or
    /// a package from a private index.
    pub unresolved: Vec<(String, String)>,
    /// Packages whose lookup failed for another reason (offline, a proxy,
    /// PyPI down). Nothing is known about them, so they are neither typos
    /// nor part of the intersection.
    pub unreachable: Vec<(String, String)>,
    /// Intersection of every resolved package's supported set.
    pub intersection: PyVersionSet,
    /// Populated only when `intersection` is empty, and only from packages
    /// that do have files for this platform.
    pub conflicts: Vec<ConflictPair>,
    /// One-line, OS-neutral summaries of packages that will build from source
    /// (sdist-only) on this platform. The solver turns `per_package` into the
    /// full, OS-specific findings.
    pub sdist_warnings: Vec<String>,
}

impl CompatReport {
    /// The recommended interpreter: the newest Python in the intersection.
    pub fn suggested_python(&self) -> Option<PyVersion> {
        self.intersection.max()
    }

    /// Is a given interpreter compatible with the whole dependency set?
    pub fn is_compatible(&self, py: PyVersion) -> bool {
        !self.intersection.is_empty() && self.intersection.contains(py)
    }

    /// Releases that publish nothing installable on this platform.
    pub fn uninstallable(&self) -> impl Iterator<Item = &PackageAnalysis> {
        self.per_package.iter().filter(|p| p.no_files_for_platform)
    }

    /// Which packages exclude a given interpreter (for "why 3.13 won't work").
    pub fn blockers_for(&self, py: PyVersion) -> Vec<&PackageAnalysis> {
        self.per_package
            .iter()
            .filter(|p| !p.supported.contains(py))
            .collect()
    }
}

/// Fetch the metadata for the release an installer would actually choose.
///
/// An unconstrained dependency gets the newest release, which is what the plain
/// lookup already returns. A pinned or capped one gets the newest release that
/// satisfies its specifier, because that is the release whose wheels decide
/// which Python versions are usable. Reading the newest release for a pinned
/// dependency is how you end up recommending an interpreter the pinned version
/// cannot install on.
pub async fn resolve<S: MetadataSource>(
    source: &S,
    req: &Requirement,
) -> crate::Result<metadata::PackageMetadata> {
    let latest = source.fetch(&req.name).await?;

    if !req.is_pinned() {
        return Ok(latest);
    }

    let spec = VersionSpec::parse(&req.spec);
    if spec.is_unconstrained() {
        return Ok(latest);
    }

    let Some(chosen) = spec.select_newest(latest.versions.iter().map(|s| s.as_str())) else {
        anyhow::bail!(
            "no release of `{}` satisfies `{}`",
            req.name,
            req.spec.trim()
        );
    };

    // Already holding it: the newest release satisfies the specifier.
    if chosen.raw == latest.info.version {
        return Ok(latest);
    }

    source.fetch_version(&req.name, &chosen.raw).await
}

/// Analyze a project's dependencies against a platform (F3 entry point).
///
/// Package lookups fan out concurrently; each is cache-first so the hot path is
/// off-network. Unresolvable names are collected rather than failing the whole
/// analysis (a typo'd dep shouldn't blank the report).
pub async fn analyze<S: MetadataSource>(
    source: &S,
    platform: Platform,
    packages: &[Requirement],
) -> CompatReport {
    let results = join_all(
        packages
            .iter()
            .map(|req| async move { (req.to_string(), resolve(source, req).await) }),
    )
    .await;

    let mut per_package = Vec::new();
    let mut unresolved = Vec::new();
    let mut unreachable = Vec::new();
    let mut sdist_warnings = Vec::new();

    for (name, res) in results {
        match res {
            Ok(meta) => {
                let analysis = meta.analyze(&platform);
                match analysis.sdist_kind() {
                    Some(metadata::SdistKind::Native) => sdist_warnings.push(format!(
                        "{} {} publishes wheels for other platforms but not yours — it will compile native code from source and needs a compiler toolchain.",
                        analysis.name, analysis.version
                    )),
                    Some(metadata::SdistKind::Unknown) => sdist_warnings.push(format!(
                        "{} {} publishes only a source distribution — it will build from source and may need a compiler if it has native extensions.",
                        analysis.name, analysis.version
                    )),
                    None => {}
                }
                per_package.push(analysis);
            }
            Err(e) if crate::pypi::client::is_unreachable(&e) => {
                unreachable.push((name, format!("{e:#}")))
            }
            Err(e) => unresolved.push((name, e.to_string())),
        }
    }

    let intersection = per_package
        .iter()
        .map(|p| p.supported.clone())
        .reduce(|acc, s| acc.intersect(&s))
        .unwrap_or_else(PyVersionSet::universe);

    // A release with no files for this platform empties the intersection on
    // its own; that is its own finding (see `PackageAnalysis::no_files_message`),
    // not a version conflict. Look for conflicts among the rest only.
    let installable: Vec<PackageAnalysis> = per_package
        .iter()
        .filter(|p| !p.no_files_for_platform)
        .cloned()
        .collect();
    let rest_empty = installable
        .iter()
        .map(|p| p.supported.clone())
        .reduce(|acc, s| acc.intersect(&s))
        .is_some_and(|s| s.is_empty());
    let conflicts = if rest_empty && installable.len() >= 2 {
        find_conflicts(&installable)
    } else {
        Vec::new()
    };

    CompatReport {
        platform,
        per_package,
        unresolved,
        unreachable,
        intersection,
        conflicts,
        sdist_warnings,
    }
}

/// Why `candidate` cannot join a project whose dependencies are `project`:
/// the pair [`find_conflicts`] names once the candidate is added (the same
/// sentence `doctor` and `setup` print for a conflicting project).
///
/// `None` when the candidate fits, when the project already shares no Python
/// on its own (that is the project's conflict, which `doctor` reports), or
/// when the candidate installs nowhere here (its own, more precise message).
/// A project dependency with the candidate's name is replaced by it: `check
/// numpy==1.26` judges the pin being considered, not the one declared.
pub fn conflict_with(
    candidate: &PackageAnalysis,
    project: &[PackageAnalysis],
) -> Option<ConflictPair> {
    if candidate.no_files_for_platform {
        return None;
    }
    let key = crate::core::project::normalize_name(&candidate.name);
    let mut packages: Vec<PackageAnalysis> = project
        .iter()
        .filter(|p| {
            !p.no_files_for_platform && crate::core::project::normalize_name(&p.name) != key
        })
        .cloned()
        .collect();
    let shared = packages
        .iter()
        .map(|p| p.supported.clone())
        .reduce(|acc, s| acc.intersect(&s))?;
    if shared.is_empty() || !shared.intersect(&candidate.supported).is_empty() {
        return None;
    }
    packages.push(candidate.clone());
    let pairs = find_conflicts(&packages);
    let names_candidate =
        |c: &ConflictPair| c.low.name == candidate.name || c.high.name == candidate.name;
    pairs
        .iter()
        .find(|c| names_candidate(c))
        .or(pairs.first())
        .cloned()
}

/// Find explicit conflicting pairs when the global intersection is empty.
///
/// Strategy: the tightest conflict is between the package with the *lowest max*
/// supported version and the one with the *highest min* — if those two don't
/// overlap, that pair is the story to tell the user.
fn find_conflicts(packages: &[PackageAnalysis]) -> Vec<ConflictPair> {
    let with_bounds: Vec<&PackageAnalysis> = packages
        .iter()
        .filter(|p| !p.supported.is_empty())
        .collect();

    if with_bounds.len() < 2 {
        // Some package supports nothing at all on this platform; report each such
        // package against the best-supported one so the message is still concrete.
        return degenerate_conflicts(packages);
    }

    let lowest_max = with_bounds
        .iter()
        .min_by_key(|p| p.supported.max().map(|v| v.minor).unwrap_or(u8::MAX))
        .unwrap();
    let highest_min = with_bounds
        .iter()
        .max_by_key(|p| p.supported.min().map(|v| v.minor).unwrap_or(0))
        .unwrap();

    if lowest_max.name == highest_min.name {
        return degenerate_conflicts(packages);
    }

    let low_cap = lowest_max.supported.max();
    let high_floor = highest_min.supported.min();
    let explanation = match (low_cap, high_floor) {
        (Some(cap), Some(floor)) => format!(
            "{} supports up to Python {} but {} requires at least Python {} — no single interpreter satisfies both.",
            lowest_max.name, cap, highest_min.name, floor
        ),
        _ => format!(
            "{} and {} have no overlapping supported Python version.",
            lowest_max.name, highest_min.name
        ),
    };

    vec![ConflictPair {
        low: (*lowest_max).clone(),
        high: (*highest_min).clone(),
        explanation,
    }]
}

/// Fallback when a package supports no Python at all on this platform.
fn degenerate_conflicts(packages: &[PackageAnalysis]) -> Vec<ConflictPair> {
    let mut out = Vec::new();
    let anchor = packages.iter().max_by_key(|p| p.supported.len()).cloned();
    for p in packages.iter().filter(|p| p.supported.is_empty()) {
        let explanation = format!(
            "{} {} has no installable distribution for your platform/Python — it constrains the project to nothing.",
            p.name, p.version
        );
        if let Some(anchor) = &anchor {
            out.push(ConflictPair {
                low: p.clone(),
                high: anchor.clone(),
                explanation,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::platform::{Arch, Os};
    use crate::pypi::metadata::{DistFile, Info, PackageMetadata};

    fn linux() -> Platform {
        Platform {
            os: Os::Linux,
            arch: Arch::X86_64,
        }
    }

    fn wheel(name: &str) -> DistFile {
        DistFile {
            filename: name.to_string(),
            packagetype: "bdist_wheel".into(),
            requires_python: None,
            yanked: false,
        }
    }

    fn pkg(name: &str, rp: Option<&str>, files: Vec<DistFile>) -> PackageMetadata {
        PackageMetadata {
            info: Info {
                name: name.into(),
                version: "1.0".into(),
                requires_python: rp.map(|s| s.to_string()),
            },
            urls: files,
            versions: vec!["1.0".into()],
        }
    }

    #[tokio::test]
    async fn a_candidate_that_shares_no_python_with_the_project_names_the_pair() {
        let mut src = FixtureSource::new();
        // legacy: wheels only cp38-cp310; tidy: pure Python, any 3.x.
        src.insert(
            "legacy",
            pkg(
                "legacy",
                Some(">=3.8"),
                vec![
                    wheel("legacy-1.0-cp38-cp38-manylinux_2_17_x86_64.whl"),
                    wheel("legacy-1.0-cp310-cp310-manylinux_2_17_x86_64.whl"),
                ],
            ),
        );
        src.insert(
            "tidy",
            pkg("tidy", None, vec![wheel("tidy-1.0-py3-none-any.whl")]),
        );
        src.insert(
            "shiny",
            pkg(
                "shiny",
                Some(">=3.11"),
                vec![wheel("shiny-1.0-cp311-cp311-manylinux_2_17_x86_64.whl")],
            ),
        );
        let project = analyze(
            &src,
            linux(),
            &[Requirement::any("legacy"), Requirement::any("tidy")],
        )
        .await;
        let shiny = analyze(&src, linux(), &[Requirement::any("shiny")]).await;
        let shiny = &shiny.per_package[0];

        let pair = conflict_with(shiny, &project.per_package).expect("conflict");
        assert_eq!(pair.low.name, "legacy");
        assert_eq!(pair.high.name, "shiny");
        assert!(pair
            .explanation
            .contains("legacy supports up to Python 3.10"));
        assert!(pair
            .explanation
            .contains("shiny requires at least Python 3.11"));

        // It fits next to tidy alone; and replacing legacy with itself is no
        // conflict at all.
        let tidy = &project.per_package[1];
        assert_eq!(conflict_with(shiny, std::slice::from_ref(tidy)), None);
        let legacy = &project.per_package[0];
        assert_eq!(conflict_with(legacy, &project.per_package), None);
        // No project dependencies: nothing to conflict with.
        assert_eq!(conflict_with(shiny, &[]), None);
    }

    #[tokio::test]
    async fn empty_intersection_names_conflicting_pair() {
        let mut src = FixtureSource::new();
        // legacy: wheels only cp38-cp310
        src.insert(
            "legacy",
            pkg(
                "legacy",
                Some(">=3.8"),
                vec![
                    wheel("legacy-1.0-cp38-cp38-manylinux_2_17_x86_64.whl"),
                    wheel("legacy-1.0-cp310-cp310-manylinux_2_17_x86_64.whl"),
                ],
            ),
        );
        // shiny: requires >=3.11
        src.insert(
            "shiny",
            pkg(
                "shiny",
                Some(">=3.11"),
                vec![wheel("shiny-1.0-cp311-cp311-manylinux_2_17_x86_64.whl")],
            ),
        );

        let report = analyze(
            &src,
            linux(),
            &[Requirement::any("legacy"), Requirement::any("shiny")],
        )
        .await;

        assert!(report.intersection.is_empty());
        assert_eq!(report.conflicts.len(), 1);
        let c = &report.conflicts[0];
        assert!(c.explanation.contains("legacy"));
        assert!(c.explanation.contains("shiny"));
    }

    #[tokio::test]
    async fn platformless_release_is_not_a_version_conflict() {
        let mut src = FixtureSource::new();
        // Windows wheels only, no sdist: nothing installs on Linux.
        src.insert(
            "winonly",
            pkg(
                "winonly",
                Some(">=3.9"),
                vec![wheel("winonly-1.0-cp312-cp312-win_amd64.whl")],
            ),
        );
        src.insert(
            "fine",
            pkg(
                "fine",
                Some(">=3.9"),
                vec![wheel("fine-1.0-py3-none-any.whl")],
            ),
        );

        let report = analyze(
            &src,
            linux(),
            &[Requirement::any("winonly"), Requirement::any("fine")],
        )
        .await;

        assert!(report.intersection.is_empty());
        assert!(report.conflicts.is_empty());
        let names: Vec<&str> = report.uninstallable().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["winonly"]);
    }
}
