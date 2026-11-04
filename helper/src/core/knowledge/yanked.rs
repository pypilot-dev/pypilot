//! Pinned or locked releases PyPI has yanked.
//!
//! An installer still installs a yanked release when it is pinned exactly
//! (PEP 592), so `requests==2.32.0` keeps coming back after every rebuild.
//! PyPI says why it was yanked; the finding repeats that and names the
//! nearest release that is not yanked.
//!
//! Checked: exact pins (`==X`) among the declared dependencies, and the
//! `uv.lock` version of each other declared dependency. Each lookup is one
//! per-release document, which the PyPI cache keeps for good.

use std::path::Path;

use futures::future::join_all;

use crate::core::project::{normalize_name, ProjectDeps};
use crate::core::{Finding, FixKind, Severity};
use crate::pypi::version::PackageVersion;
use crate::pypi::MetadataSource;

use super::rules::Context;

/// Candidate releases tried when looking for one that is not yanked.
const MAX_TRIES: usize = 6;

/// Where a checked version came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    Pin,
    Lock,
}

/// `name -> version` from `uv.lock`'s `[[package]]` entries.
pub fn locked_versions(workspace: &Path) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(workspace.join("uv.lock")) else {
        return Vec::new();
    };
    let Ok(doc) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    doc.get("package")
        .and_then(|p| p.as_array())
        .map(|pkgs| {
            pkgs.iter()
                .filter_map(|p| {
                    let name = p.get("name")?.as_str()?;
                    let version = p.get("version")?.as_str()?;
                    Some((normalize_name(name), version.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The exact version a `==X` specifier pins, if it pins one.
fn exact(spec: &str) -> Option<&str> {
    let v = spec.trim().strip_prefix("==")?;
    (!v.contains('*') && !v.contains(',') && !v.is_empty()).then_some(v)
}

/// Releases to try, nearest first: newer ones in the same `major.minor`
/// series, then newer ones, then older ones. Pre-releases are skipped.
pub fn nearest_candidates(versions: &[String], yanked: &str) -> Vec<String> {
    let Some(base) = PackageVersion::parse(yanked) else {
        return Vec::new();
    };
    let series = |v: &PackageVersion| {
        let r = v.release();
        (r.first().copied(), r.get(1).copied())
    };
    let mut all: Vec<PackageVersion> = versions
        .iter()
        .filter_map(|v| PackageVersion::parse(v))
        .filter(|v| !v.is_prerelease() && v.raw != base.raw)
        .collect();
    all.sort();
    let (newer, older): (Vec<_>, Vec<_>) = all.into_iter().partition(|v| *v > base);
    let (same, rest): (Vec<_>, Vec<_>) =
        newer.into_iter().partition(|v| series(v) == series(&base));
    same.into_iter()
        .chain(rest)
        .chain(older.into_iter().rev())
        .map(|v| v.raw)
        .take(MAX_TRIES)
        .collect()
}

async fn is_yanked<S: MetadataSource>(source: &S, name: &str, version: &str) -> Option<bool> {
    let meta = source.fetch_version(name, version).await.ok()?;
    Some(!meta.urls.is_empty() && meta.urls.iter().all(|f| f.yanked))
}

/// Yanked findings for the project's pins and locked versions.
pub async fn check<S: MetadataSource>(
    workspace: &Path,
    project: &ProjectDeps,
    source: &S,
    ctx: &Context,
) -> Vec<Finding> {
    let locked = locked_versions(workspace);
    let mut targets: Vec<(String, String, Origin)> = Vec::new();
    for req in &project.packages {
        if let Some(v) = exact(&req.spec) {
            targets.push((req.name.clone(), v.to_string(), Origin::Pin));
        } else if let Some((_, v)) = locked.iter().find(|(n, _)| *n == normalize_name(&req.name)) {
            targets.push((req.name.clone(), v.clone(), Origin::Lock));
        }
    }
    let results = join_all(
        targets
            .into_iter()
            .map(|(name, version, origin)| async move {
                if is_yanked(source, &name, &version).await != Some(true) {
                    return None;
                }
                let reason = source.yanked_reason(&name, &version).await;
                let mut nearest = None;
                if let Ok(latest) = source.fetch(&name).await {
                    for candidate in nearest_candidates(&latest.versions, &version) {
                        if is_yanked(source, &name, &candidate).await == Some(false) {
                            nearest = Some(candidate);
                            break;
                        }
                    }
                }
                Some(finding(&name, &version, origin, reason, nearest, ctx))
            }),
    )
    .await;
    results.into_iter().flatten().collect()
}

fn finding(
    name: &str,
    version: &str,
    origin: Origin,
    reason: Option<String>,
    nearest: Option<String>,
    ctx: &Context,
) -> Finding {
    let reason = reason
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .map(|r| format!("PyPI's reason: \"{r}\"."))
        .unwrap_or_else(|| "PyPI gives no reason.".to_string());
    let how = match origin {
        Origin::Pin => {
            format!("The project pins {name}=={version}, so every install still picks it.")
        }
        Origin::Lock => format!("uv.lock locks {name} {version}, so every sync installs it again."),
    };
    let (advice, fix) = match (&nearest, origin) {
        (Some(n), Origin::Pin) => (
            format!(" The nearest release that is not yanked is {n}. Fix: pin `{name}=={n}`."),
            ctx.constraint_fix(&format!("{name}=={n}")),
        ),
        (Some(n), Origin::Lock) => (
            format!(
                " The nearest release that is not yanked is {n}. Fix: `uv lock --upgrade-package {name}`, then sync."
            ),
            FixKind::Manual,
        ),
        (None, _) => (
            " No release near it could be checked; choose another release.".to_string(),
            FixKind::Manual,
        ),
    };
    Finding {
        severity: Severity::Warning,
        title: format!("{name} {version} is yanked on PyPI"),
        detail: format!("{reason} {how}{advice}"),
        fix,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::project::Requirement;
    use crate::pypi::metadata::{DistFile, Info, PackageMetadata};
    use crate::pypi::FixtureSource;

    fn release(version: &str, yanked: bool) -> PackageMetadata {
        PackageMetadata {
            info: Info {
                name: "requests".into(),
                version: version.into(),
                requires_python: None,
            },
            urls: vec![DistFile {
                filename: format!("requests-{version}-py3-none-any.whl"),
                packagetype: "bdist_wheel".into(),
                requires_python: None,
                yanked,
            }],
            versions: ["2.31.0", "2.32.0", "2.32.1", "2.32.2", "2.32.3"]
                .map(String::from)
                .to_vec(),
        }
    }

    fn source() -> FixtureSource {
        let mut src = FixtureSource::new();
        src.insert("requests", release("2.32.3", false));
        src.insert("requests==2.32.0", release("2.32.0", true));
        src.insert("requests==2.32.1", release("2.32.1", true));
        src.insert("requests==2.32.2", release("2.32.2", false));
        src.insert("requests==2.31.0", release("2.31.0", false));
        src
    }

    fn project(spec: &str) -> ProjectDeps {
        ProjectDeps {
            sources: vec!["pyproject.toml".into()],
            packages: vec![Requirement {
                name: "requests".into(),
                spec: spec.into(),
            }],
            ..ProjectDeps::default()
        }
    }

    #[test]
    fn nearest_prefers_the_same_series_upwards() {
        let v = ["2.31.0", "2.32.0", "2.32.1", "2.32.2", "2.33.0rc1", "3.0.0"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            nearest_candidates(&v, "2.32.0"),
            ["2.32.1", "2.32.2", "3.0.0", "2.31.0"]
        );
        assert!(nearest_candidates(&v, "not a version").is_empty());
    }

    #[test]
    fn only_exact_pins_count() {
        assert_eq!(exact("==2.32.0"), Some("2.32.0"));
        assert_eq!(exact("==2.*"), None);
        assert_eq!(exact(">=2"), None);
    }

    fn ctx() -> Context {
        Context {
            declared_raw: vec![crate::core::req_edit::DeclaredReq {
                file: "pyproject.toml".into(),
                raw: "requests==2.32.0".into(),
                name: "requests".into(),
            }],
            ..Context::default()
        }
    }

    #[tokio::test]
    async fn a_yanked_pin_names_the_reason_and_the_nearest_release() {
        let dir = std::env::temp_dir();
        let found = check(&dir, &project("==2.32.0"), &source(), &ctx()).await;
        assert_eq!(found.len(), 1, "{found:#?}");
        let f = &found[0];
        assert_eq!(f.title, "requests 2.32.0 is yanked on PyPI");
        assert!(f.detail.contains("2.32.2"), "{}", f.detail);
        assert_eq!(
            f.fix,
            FixKind::EditRequirement(crate::core::RequirementEdit {
                file: "pyproject.toml".into(),
                old: "requests==2.32.0".into(),
                new: "requests==2.32.2".into(),
            })
        );
        // Fine pins and ranges say nothing.
        assert!(check(&dir, &project("==2.32.2"), &source(), &ctx())
            .await
            .is_empty());
        assert!(check(&dir, &project(">=2.31"), &source(), &ctx())
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn a_yanked_lock_entry_is_reported_with_the_lock_fix() {
        let dir = std::env::temp_dir().join(format!("pp-kb-yanked-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("uv.lock"),
            "version = 1\n\n[[package]]\nname = \"requests\"\nversion = \"2.32.0\"\n",
        )
        .unwrap();
        assert_eq!(
            locked_versions(&dir),
            [("requests".to_string(), "2.32.0".to_string())]
        );
        let found = check(&dir, &project(">=2.31"), &source(), &ctx()).await;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].fix, FixKind::Manual);
        assert!(found[0]
            .detail
            .contains("uv lock --upgrade-package requests"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_reason_is_quoted_when_pypi_gives_one() {
        let f = finding(
            "requests",
            "2.32.0",
            Origin::Pin,
            Some("Yanked due to conflicts with CVE-2024-35195 mitigation ".into()),
            Some("2.32.2".into()),
            &ctx(),
        );
        assert!(f.detail.starts_with(
            "PyPI's reason: \"Yanked due to conflicts with CVE-2024-35195 mitigation\"."
        ));
        let pixi = finding(
            "requests",
            "2.32.0",
            Origin::Pin,
            None,
            Some("2.32.2".into()),
            &Context {
                pixi: true,
                ..ctx()
            },
        );
        assert_eq!(pixi.fix, FixKind::Manual);
        assert!(pixi.detail.starts_with("PyPI gives no reason."));
    }
}
