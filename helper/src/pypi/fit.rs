//! Judge a version range by a release that fits, not only by its newest one.
//!
//! [`super::resolve`] reads the newest release a specifier admits, because
//! that is what an installer picks when nothing else constrains it. Next to
//! another dependency that tops out at an older Python, though, an installer
//! picks an older release of the range instead: `pandas==2.0.3` (up to
//! Python 3.11) with `numpy>=2` installs numpy 2.2 on 3.11, yet the newest
//! numpy needs 3.12, so judging `numpy>=2` by it reported a Python conflict
//! that does not exist.
//!
//! [`fit_ranges`] runs only when the dependencies already share no Python. For
//! each one that is not an exact pin, it walks back through the newest release
//! of each older series its specifier admits until one shares a Python with
//! the rest, and judges the range by that release. Exact pins are left alone:
//! there is no other release to choose.

use crate::core::platform::Platform;
use crate::core::project::Requirement;
use crate::pypi::metadata::PackageAnalysis;
use crate::pypi::pyversion::PyVersionSet;
use crate::pypi::version::{PackageVersion, VersionSpec};
use crate::pypi::MetadataSource;

/// Older series tried per dependency, so a hopeless conflict costs a bounded
/// number of lookups (each one cached for good once fetched).
const MAX_SERIES: usize = 8;

/// A specifier that admits exactly one release: `==1.2.3` or `===1.2.3`.
fn is_exact_pin(spec: &str) -> bool {
    let spec = spec.trim();
    (spec.starts_with("==") && !spec.contains('*') && !spec.contains(','))
        || spec.starts_with("===")
}

/// Intersection of every installable analysis except the one at `skip`.
/// `None` when there is nothing else to fit against.
fn others_supported(per_package: &[PackageAnalysis], skip: usize) -> Option<PyVersionSet> {
    per_package
        .iter()
        .enumerate()
        .filter(|(j, p)| *j != skip && !p.no_files_for_platform)
        .map(|(_, p)| p.supported.clone())
        .reduce(|acc, s| acc.intersect(&s))
}

/// Releases to try for `spec`, newest first: the newest release of each
/// `major.minor` series below `current`, at most [`MAX_SERIES`] of them.
pub fn candidates(versions: &[String], spec: &str, current: &str) -> Vec<String> {
    let spec = VersionSpec::parse(spec);
    let current = PackageVersion::parse(current);
    let mut parsed: Vec<PackageVersion> = versions
        .iter()
        .filter_map(|v| PackageVersion::parse(v))
        .filter(|v| spec.matches(v))
        .filter(|v| current.as_ref().map_or(true, |c| v < c))
        .collect();
    parsed.sort();
    parsed.reverse();
    let mut seen: Vec<(u64, u64)> = Vec::new();
    let mut out = Vec::new();
    for v in parsed {
        let r = v.release();
        let series = (
            r.first().copied().unwrap_or(0),
            r.get(1).copied().unwrap_or(0),
        );
        let current_series = current.as_ref().map(|c| {
            let cr = c.release();
            (
                cr.first().copied().unwrap_or(0),
                cr.get(1).copied().unwrap_or(0),
            )
        });
        // The current release's own series is already judged by it.
        if Some(series) == current_series || seen.contains(&series) {
            continue;
        }
        seen.push(series);
        out.push(v.raw);
        if out.len() >= MAX_SERIES {
            break;
        }
    }
    out
}

/// Replace, in place, the analysis of each range dependency that shares no
/// Python with the rest by the newest older release of its range that does.
/// `requirements[i]` is the requirement behind `per_package[i]`. Returns
/// whether anything changed.
pub async fn fit_ranges<S: MetadataSource>(
    source: &S,
    platform: Platform,
    requirements: &[Requirement],
    per_package: &mut [PackageAnalysis],
) -> bool {
    let mut changed = false;
    for i in 0..per_package.len() {
        let Some(req) = requirements.get(i) else {
            break;
        };
        if per_package[i].no_files_for_platform || is_exact_pin(&req.spec) {
            continue;
        }
        let Some(others) = others_supported(per_package, i) else {
            continue;
        };
        if others.is_empty() || !per_package[i].supported.intersect(&others).is_empty() {
            continue;
        }
        let Ok(latest) = source.fetch(&req.name).await else {
            continue;
        };
        for version in candidates(&latest.versions, &req.spec, &per_package[i].version) {
            let Ok(meta) = source.fetch_version(&req.name, &version).await else {
                continue;
            };
            let analysis = meta.analyze(&platform);
            if !analysis.no_files_for_platform && !analysis.supported.intersect(&others).is_empty()
            {
                per_package[i] = analysis;
                changed = true;
                break;
            }
        }
    }
    changed
}

/// For an environment that already runs `python`: replace the analysis of
/// each dependency with an explicit, non-exact range that excludes it by
/// the newest older release of the range that supports it. `requirements[i]`
/// is the requirement behind `per_package[i]` (`None` when unknown).
/// Returns whether anything changed.
pub async fn fit_to_python<S: MetadataSource>(
    source: &S,
    platform: Platform,
    requirements: &[Option<Requirement>],
    per_package: &mut [PackageAnalysis],
    python: crate::pypi::pyversion::PyVersion,
) -> bool {
    let mut changed = false;
    for (current, req) in per_package.iter_mut().zip(requirements) {
        let Some(req) = req else {
            continue;
        };
        if req.spec.trim().is_empty()
            || is_exact_pin(&req.spec)
            || current.no_files_for_platform
            || current.supported.contains(python)
        {
            continue;
        }
        let Ok(latest) = source.fetch(&req.name).await else {
            continue;
        };
        for version in candidates(&latest.versions, &req.spec, &current.version) {
            let Ok(meta) = source.fetch_version(&req.name, &version).await else {
                continue;
            };
            let analysis = meta.analyze(&platform);
            if !analysis.no_files_for_platform && analysis.supported.contains(python) {
                *current = analysis;
                changed = true;
                break;
            }
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn versions(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn exact_pins_are_not_ranges() {
        assert!(is_exact_pin("==2.0.3"));
        assert!(is_exact_pin("===2.0.3"));
        assert!(!is_exact_pin("==2.*"));
        assert!(!is_exact_pin(">=2"));
        assert!(!is_exact_pin(""));
        assert!(!is_exact_pin("==1.0,!=1.0"));
    }

    #[test]
    fn candidates_are_the_newest_of_each_older_series() {
        let all = versions(&[
            "1.26.4", "2.0.0", "2.0.2", "2.1.3", "2.2.5", "2.2.6", "2.3.0rc1", "2.3.1", "2.4.0",
        ]);
        assert_eq!(
            candidates(&all, ">=2", "2.4.0"),
            ["2.3.1", "2.2.6", "2.1.3", "2.0.2"]
        );
        // Unconstrained: every older series, pre-releases skipped.
        assert_eq!(
            candidates(&all, "", "2.4.0"),
            ["2.3.1", "2.2.6", "2.1.3", "2.0.2", "1.26.4"]
        );
        // The current series is already judged.
        assert_eq!(candidates(&all, ">=2.3", "2.3.1"), Vec::<String>::new());
    }

    #[test]
    fn candidates_are_bounded() {
        let many: Vec<String> = (0..30).map(|m| format!("1.{m}.0")).collect();
        assert_eq!(candidates(&many, "", "1.29.0").len(), MAX_SERIES);
    }
}
