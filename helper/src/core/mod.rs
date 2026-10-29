//! The engine: probing, project parsing, constraint solving, and bootstrap.
//!
//! Everything here is editor-agnostic. The LSP and CLI frontends are thin callers
//! over [`solver::assess`] (read-only) and [`setup::run`] (mutating). F2 (hardware)
//! slots into [`solver`] via [`crate::matrix`]; F4 (buffer diagnostics) attaches in
//! the LSP layer and reuses the same [`Assessment`].

pub mod command;
pub mod depcheck;
pub mod env;
pub mod gpu;
pub mod guardian;
pub mod imports;
pub mod install;
pub mod installed;
pub mod interpreter;
pub mod logs;
pub mod modules;
pub mod pip;
pub mod pixi;
pub mod pixi_advice;
pub mod pixi_compat;
pub mod pixi_envs;
pub mod pixi_install;
pub mod pixi_manage;
pub mod pixi_migrate;
pub mod platform;
pub mod probe;
pub mod project;
pub mod pyright;
pub mod rescan;
pub mod setup;
pub mod solver;
pub mod stdlib;
pub mod toolchain;
pub mod torch_gpu;
pub mod torch_index;
pub mod uv;

use crate::pypi::{CompatReport, PyVersion};

/// Severity of a finding — drives toast styling and doctor output ordering.
/// Serialized as `"error"`, `"warning"`, `"info"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Blocks installation entirely (empty intersection, incompatible interpreter).
    Error,
    /// Works but suboptimal / needs attention (sdist compile, missing venv).
    Warning,
    /// Purely informational.
    Info,
}

/// A machine-actionable fix a finding can carry. Both the CLI and the LSP
/// `executeCommand` map onto these — they call the exact same code.
///
/// Serialized as `{"kind": "setup_environment"}`, or
/// `{"kind": "recreate_with_python", "python": "3.12"}`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", content = "python", rename_all = "snake_case")]
pub enum FixKind {
    /// Run the full F1 bootstrap (install uv → Python → venv → deps).
    SetupEnvironment,
    /// Recreate the venv on a specific Python version.
    RecreateWithPython(PyVersion),
    /// Translate a conda `environment.yml` into a PEP 621 `pyproject.toml`
    /// (`pypilot migrate-conda`). Optional per F5 — conda projects work as-is.
    MigrateConda,
    /// Set the project up with pixi (`pypilot migrate-to-pixi`, the LSP
    /// command `pypilot.migrateToPixi`): pip and uv cannot install what a
    /// dependency needs (system libraries, nvcc), and conda-forge ships it.
    /// Only suggested, never run without the user asking.
    MigrateToPixi,
    /// Nothing to automate; the message itself is the guidance.
    Manual,
}

/// One finding: a problem plus the *why* and an optional fix. Every user-facing
/// message states which package constrained the decision.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Finding {
    pub severity: Severity,
    pub title: String,
    /// The "why" — always names the constraining package(s).
    pub detail: String,
    pub fix: FixKind,
}

/// The full read-only assessment of a workspace. Shared by `doctor` (prints it)
/// and F5 (decides whether to toast).
#[derive(Debug, Clone)]
pub struct Assessment {
    pub probes: probe::Probes,
    pub project: project::ProjectDeps,
    /// `None` when there are no declared dependencies to check.
    pub compat: Option<CompatReport>,
    /// Chosen target interpreter and the reason, when one can be determined.
    pub target_python: Option<PyVersion>,
    pub findings: Vec<Finding>,
}

impl Assessment {
    /// Silence contract (F5): true when there is nothing worth a toast.
    pub fn all_good(&self) -> bool {
        !self
            .findings
            .iter()
            .any(|f| matches!(f.severity, Severity::Error | Severity::Warning))
    }

    /// Highest severity present, for toast selection.
    pub fn worst_severity(&self) -> Option<Severity> {
        self.findings
            .iter()
            .map(|f| f.severity)
            .min_by_key(|s| match s {
                Severity::Error => 0,
                Severity::Warning => 1,
                Severity::Info => 2,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `--json` contract: these names are what scripts and bug reports read.
    #[test]
    fn findings_serialize_with_stable_names() {
        let f = Finding {
            severity: Severity::Error,
            title: "t".into(),
            detail: "d".into(),
            fix: FixKind::RecreateWithPython(PyVersion::py3(12)),
        };
        assert_eq!(
            serde_json::to_value(&f).unwrap(),
            serde_json::json!({
                "severity": "error",
                "title": "t",
                "detail": "d",
                "fix": {"kind": "recreate_with_python", "python": "3.12"},
            })
        );
        let kinds = [
            FixKind::SetupEnvironment,
            FixKind::MigrateConda,
            FixKind::MigrateToPixi,
            FixKind::Manual,
        ]
        .map(|k| serde_json::to_value(k).unwrap()["kind"].clone());
        assert_eq!(
            kinds,
            [
                "setup_environment",
                "migrate_conda",
                "migrate_to_pixi",
                "manual"
            ]
        );
        assert_eq!(
            serde_json::to_value(Severity::Warning).unwrap(),
            serde_json::json!("warning")
        );
    }
}
