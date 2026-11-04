//! The engine: probing, project parsing, constraint solving, and bootstrap.
//!
//! Everything here is editor-agnostic. The LSP and CLI frontends are thin callers
//! over [`solver::assess`] (read-only) and [`setup::run`] (mutating). F2 (hardware)
//! slots into [`solver`] via [`crate::matrix`]; F4 (buffer diagnostics) attaches in
//! the LSP layer and reuses the same [`Assessment`].

pub mod command;
pub mod depcheck;
pub mod env;
pub mod exec;
pub mod foreign_setup;
pub mod gpu;
pub mod guardian;
pub mod health;
pub mod imports;
pub mod install;
pub mod installed;
pub mod interpreter;
pub mod knowledge;
pub mod logs;
pub mod manifests;
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
pub mod repair;
pub mod req_edit;
pub mod rescan;
pub mod resolution;
pub mod setup;
pub mod solver;
pub mod stdlib;
pub mod suggest;
pub mod toolchain;
pub mod torch_gpu;
pub mod torch_index;
pub mod undeclared;
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
/// Serialized as `{"kind": "setup_environment"}`, with the variant's data
/// beside the kind: `{"kind": "recreate_with_python", "python": "3.12"}`,
/// `{"kind": "reinstall_packages", "packages": ["requests"]}`,
/// `{"kind": "repair_environment", "python": "3.12"}` (`python` is `null`
/// when the environment's version is unknown). Clients must treat a kind they
/// do not know as "Fix environment" (`pypilot doctor --fix`), which handles
/// every kind.
///
/// `pypilot doctor --fix` turns the findings' kinds into one targeted plan
/// ([`repair::plan`]). To add a kind: add the variant here (with its
/// serialization in the `Serialize` impl below), then give it one arm in
/// [`repair::plan`], which is the only place that decides what a kind runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixKind {
    /// Nothing usable exists yet: run the full F1 bootstrap (install uv,
    /// Python, venv, deps), as `pypilot setup` does.
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
    /// The environment is sound but its packages are not what the project
    /// declares (missing, or drifted from `uv.lock`): install into the
    /// existing environment (`uv sync`, `--locked` when the lock is current;
    /// `uv pip install -r` / `pip install` in the pip modes).
    SyncEnvironment,
    /// `uv.lock` no longer matches `pyproject.toml`: `uv lock`, then sync.
    Relock,
    /// These installed distributions are damaged (files or metadata
    /// missing): reinstall just them (`uv sync --reinstall-package X`).
    ReinstallPackages(Vec<String>),
    /// The environment itself is damaged (its interpreter or `pyvenv.cfg`
    /// is gone, or it leaks the system site-packages): rebuild it in place,
    /// on the Python it had (or the pinned one), then sync.
    RepairEnvironment { python: Option<PyVersion> },
    /// Move a project whose only manifest is a conda `environment.yml` to
    /// pixi (`pypilot migrate-conda --to pixi --write --install`), which
    /// keeps its Python pin and conda packages. Offered when pixi is
    /// installed; [`FixKind::MigrateConda`] otherwise.
    MigrateCondaToPixi,
    /// Install a Poetry project with `poetry install`: its dependencies live
    /// in `[tool.poetry]`, which uv and pip do not read.
    PoetryInstall,

    // --- Requirement edits (resolution diagnosis) ----------------------------
    // Fixes that change what the manifest asks for rather than the
    // environment. Each is applied by `crate::core::req_edit::apply`, which
    // edits only the requirements named and goes through `core::exec`, so a
    // dry run records the write instead of making it. Serialized as
    // `{"kind": "edit_requirement", "edit": {"file", "old", "new"}}` and
    // `{"kind": "add_requirements", "packages": [...]}`.
    /// Replace one requirement in one manifest (pyproject.toml's
    /// `[project] dependencies`, or a requirements.txt line) with another:
    /// relax a pin uv cannot satisfy, correct a typo'd name, or add an
    /// environment marker.
    EditRequirement(RequirementEdit),
    /// Add packages the code imports but the manifest never declares, by
    /// installing them the way `pypilot install` does (which records them).
    AddRequirements(Vec<String>),

    // ---- Known breakages ([`knowledge`]), run by [`knowledge::apply::apply`].
    // Their constraint fixes (`numpy<2`, `setuptools<82`, ...) are
    // `EditRequirement` / `AddRequirements` above; only this has no
    // equivalent there.
    /// Remove stray distributions from the manifest and the environment and
    /// (re)install the right one: jwt next to PyJWT, enum34, sklearn.
    /// Serialized with `uninstall`, `install` and `drop` lists.
    ReplaceDist(knowledge::Replacement),
}

impl serde::Serialize for FixKind {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = s.serialize_map(None)?;
        map.serialize_entry("kind", self.kind())?;
        match self {
            FixKind::RecreateWithPython(py) => map.serialize_entry("python", py)?,
            FixKind::ReinstallPackages(packages) => map.serialize_entry("packages", packages)?,
            FixKind::RepairEnvironment { python } => map.serialize_entry("python", python)?,
            FixKind::EditRequirement(edit) => map.serialize_entry("edit", edit)?,
            FixKind::AddRequirements(packages) => map.serialize_entry("packages", packages)?,
            FixKind::ReplaceDist(r) => {
                map.serialize_entry("uninstall", &r.uninstall)?;
                map.serialize_entry("install", &r.install)?;
                map.serialize_entry("drop", &r.drop)?;
            }
            _ => {}
        }
        map.end()
    }
}

impl FixKind {
    /// The `kind` string the JSON output carries.
    pub fn kind(&self) -> &'static str {
        match self {
            FixKind::SetupEnvironment => "setup_environment",
            FixKind::RecreateWithPython(_) => "recreate_with_python",
            FixKind::MigrateConda => "migrate_conda",
            FixKind::MigrateToPixi => "migrate_to_pixi",
            FixKind::Manual => "manual",
            FixKind::SyncEnvironment => "sync_environment",
            FixKind::Relock => "relock",
            FixKind::ReinstallPackages(_) => "reinstall_packages",
            FixKind::RepairEnvironment { .. } => "repair_environment",
            FixKind::MigrateCondaToPixi => "migrate_conda_to_pixi",
            FixKind::PoetryInstall => "poetry_install",
            FixKind::EditRequirement(_) => "edit_requirement",
            FixKind::AddRequirements(_) => "add_requirements",
            FixKind::ReplaceDist(_) => "replace_dist",
        }
    }
}

/// One requirement replaced by another in one manifest file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RequirementEdit {
    /// The manifest, `pyproject.toml` or a requirements file.
    pub file: std::path::PathBuf,
    /// The requirement as written there, e.g. `urllib3<1.21`.
    pub old: String,
    /// What replaces it, e.g. `urllib3>=1.21.1,<3`.
    pub new: String,
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
            FixKind::MigrateCondaToPixi,
            FixKind::PoetryInstall,
        ]
        .map(|k| serde_json::to_value(k).unwrap()["kind"].clone());
        assert_eq!(
            kinds,
            [
                "setup_environment",
                "migrate_conda",
                "migrate_to_pixi",
                "manual",
                "migrate_conda_to_pixi",
                "poetry_install"
            ]
        );
        let new = [
            (
                FixKind::SyncEnvironment,
                serde_json::json!({"kind": "sync_environment"}),
            ),
            (FixKind::Relock, serde_json::json!({"kind": "relock"})),
            (
                FixKind::ReinstallPackages(vec!["requests".into()]),
                serde_json::json!({"kind": "reinstall_packages", "packages": ["requests"]}),
            ),
            (
                FixKind::RepairEnvironment {
                    python: Some(PyVersion::py3(12)),
                },
                serde_json::json!({"kind": "repair_environment", "python": "3.12"}),
            ),
            (
                FixKind::RepairEnvironment { python: None },
                serde_json::json!({"kind": "repair_environment", "python": null}),
            ),
        ];
        let known = [(
            FixKind::ReplaceDist(knowledge::Replacement {
                uninstall: vec!["jwt".into(), "pyjwt".into()],
                install: vec!["PyJWT".into()],
                drop: vec!["jwt".into()],
            }),
            serde_json::json!({
                "kind": "replace_dist",
                "uninstall": ["jwt", "pyjwt"],
                "install": ["PyJWT"],
                "drop": ["jwt"],
            }),
        )];
        for (kind, json) in new.into_iter().chain(known) {
            assert_eq!(serde_json::to_value(&kind).unwrap(), json);
        }
        assert_eq!(
            serde_json::to_value(Severity::Warning).unwrap(),
            serde_json::json!("warning")
        );
    }
}
