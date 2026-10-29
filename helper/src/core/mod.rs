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
pub mod modules;
pub mod pip;
pub mod pixi;
pub mod pixi_compat;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixKind {
    /// Run the full F1 bootstrap (install uv → Python → venv → deps).
    SetupEnvironment,
    /// Recreate the venv on a specific Python version.
    RecreateWithPython(PyVersion),
    /// Translate a conda `environment.yml` into a PEP 621 `pyproject.toml`
    /// (`pypilot migrate-conda`). Optional per F5 — conda projects work as-is.
    MigrateConda,
    /// Nothing to automate; the message itself is the guidance.
    Manual,
}

/// One finding: a problem plus the *why* and an optional fix. Every user-facing
/// message states which package constrained the decision.
#[derive(Debug, Clone, PartialEq, Eq)]
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
