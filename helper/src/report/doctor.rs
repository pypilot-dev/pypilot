//! `pypilot doctor --json`: the assessment as one document.
//!
//! Borrows from the [`Assessment`] rather than copying it, so the JSON is
//! the data the text report and the editor are printed from.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::core::interpreter::Interpreter;
use crate::core::pixi::{CondaDep, PixiProject};
use crate::core::pixi_envs::{self, EnvReport, LockStatus};
use crate::core::probe::VenvInfo;
use crate::core::uv::UvInfo;
use crate::core::{solver, Assessment, Finding, Severity};
use crate::pypi::{ConflictPair, PackageAnalysis, PyVersion, PyVersionSet};
use crate::settings::PackageManager;

#[derive(Serialize)]
pub struct DoctorReport<'a> {
    pub workspace: &'a Path,
    pub package_manager: PackageManager,
    pub environment: Environment<'a>,
    pub project: Project<'a>,
    /// `null` unless the project has a pixi manifest.
    pub pixi: Option<&'a PixiSection>,
    /// `null` when the project declares no dependencies.
    pub compatibility: Option<Compatibility<'a>>,
    /// The Python setup would build the environment on.
    pub target_python: Option<PyVersion>,
    pub findings: Vec<&'a Finding>,
}

#[derive(Serialize)]
pub struct Environment<'a> {
    pub uv: Option<&'a UvInfo>,
    /// The project environment (a `.venv`, or the selected pixi prefix).
    pub venv: Option<&'a VenvInfo>,
    pub interpreters: &'a [Interpreter],
}

#[derive(Serialize)]
pub struct Project<'a> {
    /// The project files read.
    pub sources: &'a [PathBuf],
    pub requires_python: Option<&'a str>,
    /// The pixi manifest's python spec, as written.
    pub pixi_python: Option<&'a str>,
    /// PyPI requirements: `numpy>=2`.
    pub dependencies: Vec<String>,
    /// A pixi project's conda-forge dependencies.
    pub conda_dependencies: &'a [CondaDep],
    pub has_conda_env: bool,
}

#[derive(Serialize)]
pub struct Compatibility<'a> {
    /// The platform judged: `Windows x64`.
    pub platform: &'static str,
    /// The Pythons every dependency runs on.
    pub supported: &'a PyVersionSet,
    pub supported_range: String,
    pub recommended: Option<String>,
    pub packages: &'a [PackageAnalysis],
    pub unresolved: Vec<Unresolved<'a>>,
    /// Lookups that failed for another reason (offline, a proxy): not
    /// typos, and left out of `supported`.
    pub unreachable: Vec<Unresolved<'a>>,
    /// Set when `supported` is empty: the packages that share no Python.
    pub conflicts: &'a [ConflictPair],
}

#[derive(Serialize)]
pub struct Unresolved<'a> {
    pub name: &'a str,
    pub error: &'a str,
}

/// What doctor learns about a pixi project beyond the assessment.
#[derive(Debug, Clone, Serialize)]
pub struct PixiSection {
    pub manifest: PathBuf,
    pub platforms: Vec<String>,
    /// `pixi lock --check --dry-run`, which writes nothing.
    pub lock: LockStatus,
    pub environments: Vec<EnvReport>,
    /// The lock finding, which doctor lists with the others.
    #[serde(skip)]
    pub finding: Option<Finding>,
}

impl PixiSection {
    pub async fn gather(project: &PixiProject, workspace: &Path) -> PixiSection {
        let lock = pixi_envs::lock_status(&project.root).await;
        let selected = pixi_envs::selected(project, workspace);
        PixiSection {
            manifest: project.manifest.clone(),
            platforms: project.platforms.clone(),
            environments: pixi_envs::reports(project, &selected),
            finding: pixi_envs::lock_finding(project, &lock),
            lock,
        }
    }
}

impl<'a> DoctorReport<'a> {
    pub fn new(
        workspace: &'a Path,
        package_manager: PackageManager,
        a: &'a Assessment,
        pixi: Option<&'a PixiSection>,
    ) -> Self {
        let p = &a.project;
        DoctorReport {
            workspace,
            package_manager,
            environment: Environment {
                uv: a.probes.uv.as_ref(),
                venv: a.probes.venv.as_ref(),
                interpreters: &a.probes.interpreters,
            },
            project: Project {
                sources: &p.sources,
                requires_python: p.declared_requires_python.as_deref(),
                pixi_python: p.pixi_python.as_deref().filter(|_| p.is_pixi()),
                dependencies: p.packages.iter().map(ToString::to_string).collect(),
                conda_dependencies: &p.conda_deps,
                has_conda_env: p.has_conda_env,
            },
            pixi,
            compatibility: a.compat.as_ref().map(|c| Compatibility {
                platform: c.platform.label(),
                supported: &c.intersection,
                supported_range: c.intersection.to_range_string(),
                recommended: solver::recommendation(p, c, a.target_python),
                packages: &c.per_package,
                unresolved: c
                    .unresolved
                    .iter()
                    .map(|(name, error)| Unresolved { name, error })
                    .collect(),
                unreachable: c
                    .unreachable
                    .iter()
                    .map(|(name, error)| Unresolved { name, error })
                    .collect(),
                conflicts: &c.conflicts,
            }),
            target_python: a.target_python,
            findings: a
                .findings
                .iter()
                .chain(pixi.and_then(|s| s.finding.as_ref()))
                .collect(),
        }
    }

    /// No finding is an error. Warnings (a source build, no environment yet)
    /// still count as ok.
    pub fn ok(&self) -> bool {
        !self.findings.iter().any(|f| f.severity == Severity::Error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::platform::Platform;
    use crate::core::FixKind;
    use crate::pypi::CompatReport;
    use crate::report::check::tests::analysis;

    #[test]
    fn json_shape_is_stable() {
        let legacy = analysis("legacy", 8..=10);
        let a = Assessment {
            probes: Default::default(),
            project: crate::core::project::ProjectDeps {
                sources: vec!["requirements.txt".into()],
                packages: vec![crate::core::project::Requirement {
                    name: "legacy".into(),
                    spec: ">=1".into(),
                }],
                ..Default::default()
            },
            compat: Some(CompatReport {
                platform: Platform::current(),
                intersection: legacy.supported.clone(),
                per_package: vec![legacy],
                unresolved: vec![("typo".into(), "not found".into())],
                unreachable: Vec::new(),
                conflicts: Vec::new(),
                sdist_warnings: Vec::new(),
            }),
            target_python: Some(PyVersion::py3(10)),
            findings: vec![Finding {
                severity: Severity::Warning,
                title: "No virtual environment".into(),
                detail: "d".into(),
                fix: FixKind::SetupEnvironment,
            }],
        };
        let r = DoctorReport::new(Path::new("proj"), PackageManager::Auto, &a, None);
        assert!(r.ok());
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["package_manager"], "auto");
        assert_eq!(
            v["environment"],
            serde_json::json!({"uv": null, "venv": null, "interpreters": []})
        );
        assert_eq!(
            v["project"]["dependencies"],
            serde_json::json!(["legacy>=1"])
        );
        assert_eq!(
            v["project"]["sources"],
            serde_json::json!(["requirements.txt"])
        );
        let c = &v["compatibility"];
        assert_eq!(c["supported"], serde_json::json!(["3.8", "3.9", "3.10"]));
        assert_eq!(c["supported_range"], "3.8–3.10");
        assert_eq!(c["packages"][0]["name"], "legacy");
        assert_eq!(
            c["unresolved"],
            serde_json::json!([{"name": "typo", "error": "not found"}])
        );
        assert_eq!(v["target_python"], "3.10");
        assert_eq!(v["pixi"], serde_json::Value::Null);
        assert_eq!(
            v["findings"][0],
            serde_json::json!({
                "severity": "warning",
                "title": "No virtual environment",
                "detail": "d",
                "fix": {"kind": "setup_environment"},
            })
        );
    }
}
