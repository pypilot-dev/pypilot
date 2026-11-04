//! Setup for projects the uv flow would get wrong as they stand.
//!
//! [`crate::core::setup`] builds a `.venv` with uv or pip from pyproject.toml
//! or requirements.txt. Three kinds of project need something else first,
//! and [`crate::core::manifests`] reports each:
//!
//! - an `environment.yml` that is the only manifest: `uv init` would make an
//!   empty pyproject.toml on the newest Python and install nothing, ignoring
//!   the yml's `python=` pin and packages. It is migrated instead, to pixi
//!   when pixi is installed (keeping conda packages), else to a
//!   pyproject.toml whose environment is built on the yml's Python.
//! - a Poetry project, whose `[tool.poetry]` dependencies uv and pip do not
//!   read: `poetry install`, or, without Poetry, a failed step that says how
//!   to get it and changes nothing (as a pixi project without pixi does).
//! - a requirements.txt with lines pip cannot read: stop before a venv is
//!   built that the install then leaves half-finished.

use std::path::Path;

use crate::core::exec::{self, Effect};
use crate::core::manifests;
use crate::core::pixi_migrate::{self, MigrateOptions, MigrationSource};
use crate::core::project;
use crate::core::setup::{SetupSummary, Step};
use crate::core::Assessment;
use crate::pypi::pyversion::PyVersion;
use crate::pypi::MetadataSource;
use crate::settings::{ResolvedMode, Settings};

/// Step name for the requirements.txt check.
pub const REQUIREMENTS_STEP: &str = "check requirements.txt";

/// Step name when Poetry is not installed.
pub const LOCATE_POETRY_STEP: &str = "locate poetry";

/// What the locate step says without Poetry.
pub const MISSING_POETRY: &str = "This is a Poetry project: its dependencies are in [tool.poetry], which uv and pip do not read. Install Poetry (`uv tool install poetry`, or see https://python-poetry.org/docs/#installation) and run setup again, or convert the project to PEP 621 with `uvx migrate-to-uv` so uv can set it up. Nothing was changed.";

/// Set the project up when it is one of the kinds in the module docs, or
/// `None` to let the uv or pip flow run. `mode` is the mode setup resolved;
/// `package_manager = "pixi"` keeps its own flow, which migrates the project
/// to pixi itself.
pub async fn run<S: MetadataSource>(
    workspace: &Path,
    settings: &Settings,
    source: &S,
    assessment: &Assessment,
    mode: ResolvedMode,
) -> crate::Result<Option<SetupSummary>> {
    if mode == ResolvedMode::Pixi || assessment.project.is_pixi() {
        return Ok(None);
    }
    let mut summary = SetupSummary {
        package_manager: settings.package_manager,
        resolved_mode: mode,
        python: None,
        venv_path: workspace.join(".venv"),
        why: String::new(),
        steps: Vec::new(),
        ok: true,
    };

    if manifests::is_conda_only(workspace, &assessment.project) {
        return conda(workspace, settings, source, assessment, summary)
            .await
            .map(Some);
    }

    let pyproject = workspace.join("pyproject.toml");
    let poetry = project::read_manifest(&pyproject)
        .as_deref()
        .is_some_and(manifests::is_poetry_only);
    if poetry {
        summary.why = "Poetry project: its dependencies are declared in [tool.poetry], so Poetry installs them.".to_string();
        poetry_install(workspace, &mut summary).await;
        return Ok(Some(summary));
    }

    if let Some(text) = project::read_manifest(&workspace.join("requirements.txt")) {
        let bad = manifests::invalid_requirements_at(&text, workspace);
        if !bad.is_empty() {
            let each: Vec<String> = bad
                .iter()
                .map(|b| format!("line {} `{}`: {}", b.line, b.text, b.reason))
                .collect();
            summary.ok = false;
            summary.steps.push(Step {
                name: REQUIREMENTS_STEP.to_string(),
                ok: false,
                detail: format!(
                    "requirements.txt has lines pip cannot read ({}). Fix or remove them and run setup again; nothing was changed, so no half-built environment is left behind.",
                    each.join("; ")
                ),
            });
            return Ok(Some(summary));
        }
    }
    Ok(None)
}

/// The Python a conda `python=` pin names (`3.11`, `3.11.*`, `>=3.10`), as
/// the minor to build on. A range gives its lower bound.
pub fn conda_python(pin: &str) -> Option<PyVersion> {
    let digits = pin.trim().trim_start_matches(['=', '>', '<', '~', '!']);
    PyVersion::parse(digits.trim_end_matches(".*")).filter(|v| v.major == 3)
}

async fn conda<S: MetadataSource>(
    workspace: &Path,
    settings: &Settings,
    source: &S,
    assessment: &Assessment,
    mut summary: SetupSummary,
) -> crate::Result<SetupSummary> {
    if manifests::conda_goes_to_pixi(settings).await {
        summary.resolved_mode = ResolvedMode::Pixi;
        summary.why = "environment.yml is the only manifest: moving it to pixi keeps its Python pin and conda packages (`pypilot migrate-conda --to pixi --write --install`).".to_string();
        let opts = MigrateOptions {
            write: true,
            install: true,
            source: MigrationSource::CondaEnvironment,
            ..MigrateOptions::default()
        };
        let step = "migrate-conda --to pixi";
        match pixi_migrate::migrate_to_pixi(workspace, settings, opts).await {
            Ok(m) => {
                summary.steps.push(Step {
                    name: step.to_string(),
                    ok: true,
                    detail: format!("{} environment.yml was left untouched.", m.headline()),
                });
                if let Some(install) = m.install {
                    summary.ok = install.ok;
                    summary.python = install.python;
                    summary.venv_path = install.venv_path;
                    summary.steps.extend(install.steps);
                }
            }
            Err(e) => {
                summary.ok = false;
                summary.steps.push(Step {
                    name: step.to_string(),
                    ok: false,
                    detail: e.to_string(),
                });
            }
        }
        return Ok(summary);
    }

    // No pixi: a pyproject.toml from the yml, then a .venv on its Python.
    let project_name = std::fs::canonicalize(workspace)
        .unwrap_or_else(|_| workspace.to_path_buf())
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".to_string());
    let toml = project::conda_migration_pyproject(&assessment.project, &project_name);
    let target = workspace.join("pyproject.toml");
    let effect = Effect::new(
        "migrate-conda",
        "environment.yml is the only manifest; write a pyproject.toml from it (environment.yml is left untouched)",
    );
    if let Err(e) = exec::write_new(&target, &toml, effect) {
        summary.ok = false;
        summary.steps.push(Step {
            name: "migrate-conda".to_string(),
            ok: false,
            detail: format!("cannot create {}: {e}", target.display()),
        });
        return Ok(summary);
    }
    summary.steps.push(Step {
        name: "migrate-conda".to_string(),
        ok: true,
        detail: format!(
            "wrote pyproject.toml from environment.yml ({} dependencies, carried over as written; review conda-only names). environment.yml was left untouched.",
            assessment.project.packages.len()
        ),
    });
    let python = assessment
        .project
        .conda_python
        .as_deref()
        .and_then(conda_python);
    let rest = match python {
        Some(py) => {
            crate::core::install::recreate_with_python(workspace, settings, py, &[]).await?
        }
        // No pin: an ordinary setup of the new pyproject.toml.
        None => Box::pin(crate::core::setup::run(workspace, settings, source)).await?,
    };
    summary.why = match python {
        Some(py) => format!(
            "environment.yml is the only manifest and pixi is not installed: migrated to pyproject.toml, environment built on its Python {py}."
        ),
        None => "environment.yml is the only manifest and pixi is not installed: migrated to pyproject.toml.".to_string(),
    };
    summary.resolved_mode = rest.resolved_mode;
    summary.python = rest.python;
    summary.venv_path = rest.venv_path;
    summary.ok = rest.ok;
    summary.steps.extend(rest.steps);
    Ok(summary)
}

/// `poetry install`, keeping the environment in the project's `.venv`
/// (where PyPilot and the editors look) when there is none yet.
async fn poetry_install(workspace: &Path, summary: &mut SetupSummary) {
    if !manifests::poetry_available().await {
        summary.ok = false;
        summary.steps.push(Step {
            name: LOCATE_POETRY_STEP.to_string(),
            ok: false,
            detail: MISSING_POETRY.to_string(),
        });
        return;
    }
    // Poetry uses an existing `.venv`, but otherwise creates the environment
    // in its cache. `--local` records the choice in this project's
    // poetry.toml only; an existing poetry.toml is the user's to keep.
    if !workspace.join(".venv").exists() && !workspace.join("poetry.toml").exists() {
        let effect = Effect::new(
            "poetry config virtualenvs.in-project true --local",
            "keep Poetry's environment in the project's .venv, where editors find it (writes poetry.toml)",
        )
        .writes(workspace.join("poetry.toml"));
        let args = ["config", "virtualenvs.in-project", "true", "--local"];
        record(
            summary,
            "poetry config virtualenvs.in-project",
            exec::run("poetry", &args, Some(workspace), effect).await,
            "the environment goes in .venv (poetry.toml)",
            false,
        );
    }
    let effect = Effect::new(
        "poetry install",
        "install the dependencies declared in [tool.poetry], with their dev groups",
    )
    .writes(workspace.join("poetry.lock"))
    .writes(workspace.join(".venv"));
    record(
        summary,
        "poetry install",
        exec::run("poetry", &["install"], Some(workspace), effect).await,
        "dependencies installed",
        true,
    );
    if let Some(env) = crate::core::env::project_env(workspace) {
        summary.python = env.python;
        summary.venv_path = env.path;
    }
}

fn record(
    summary: &mut SetupSummary,
    name: &str,
    result: crate::Result<crate::core::command::Output>,
    ok_detail: &str,
    critical: bool,
) {
    let (ok, detail) = match result {
        Ok(out) if out.success() => (true, ok_detail.to_string()),
        Ok(out) => {
            let err = out.stderr.trim();
            let detail = if err.is_empty() {
                out.stdout.trim()
            } else {
                err
            };
            (false, detail.to_string())
        }
        Err(e) => (false, e.to_string()),
    };
    if !ok && critical {
        summary.ok = false;
    }
    summary.steps.push(Step {
        name: name.to_string(),
        ok,
        detail,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::probe::Probes;

    fn assessment(workspace: &Path) -> Assessment {
        Assessment {
            probes: Probes::default(),
            project: project::scan(workspace),
            compat: None,
            target_python: None,
            findings: Vec::new(),
        }
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pypilot-foreign-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn conda_python_pins() {
        assert_eq!(conda_python("3.11"), Some(PyVersion::py3(11)));
        assert_eq!(conda_python("3.11.*"), Some(PyVersion::py3(11)));
        assert_eq!(conda_python(">=3.10"), Some(PyVersion::py3(10)));
        assert_eq!(conda_python("=3.9"), Some(PyVersion::py3(9)));
        assert_eq!(conda_python("2.7"), None);
        assert_eq!(conda_python("x"), None);
    }

    #[tokio::test]
    async fn unreadable_requirements_stop_before_anything_is_built() {
        let dir = scratch("bad-req");
        std::fs::write(
            dir.join("requirements.txt"),
            "requests\n==broken line\nsix>=\n",
        )
        .unwrap();
        let settings = Settings::default();
        let source = crate::pypi::FixtureSource::new();
        let a = assessment(&dir);
        let summary = run(&dir, &settings, &source, &a, ResolvedMode::PurePip)
            .await
            .unwrap()
            .expect("setup is taken over");
        assert!(!summary.ok);
        assert_eq!(summary.steps.len(), 1);
        assert_eq!(summary.steps[0].name, REQUIREMENTS_STEP);
        assert!(summary.steps[0].detail.contains("line 2 `==broken line`"));
        assert!(summary.steps[0].detail.contains("line 3 `six>=`"));
        assert!(!dir.join(".venv").exists());

        // A readable file is left to the normal flow, and so is pixi mode.
        std::fs::write(dir.join("requirements.txt"), "requests\n").unwrap();
        let a = assessment(&dir);
        assert!(run(&dir, &settings, &source, &a, ResolvedMode::PurePip)
            .await
            .unwrap()
            .is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_poetry_project_is_never_handed_to_uv() {
        let dir = scratch("poetry");
        std::fs::write(
            dir.join("pyproject.toml"),
            "[tool.poetry.dependencies]\npython = \"^3.10\"\nrequests = \"^2.31\"\n",
        )
        .unwrap();
        let settings = Settings::default();
        let source = crate::pypi::FixtureSource::new();
        let a = assessment(&dir);
        let (summary, steps) =
            exec::dry_run(run(&dir, &settings, &source, &a, ResolvedMode::UvFull)).await;
        let summary = summary.unwrap().expect("setup is taken over");
        // With Poetry the plan is poetry's; without it, one failed step.
        if manifests::poetry_available().await {
            assert!(
                steps.iter().any(|s| s.name == "poetry install"),
                "{steps:?}"
            );
        } else {
            assert!(!summary.ok);
            assert_eq!(summary.steps[0].name, LOCATE_POETRY_STEP);
            assert_eq!(summary.steps[0].detail, MISSING_POETRY);
        }
        assert!(steps.iter().all(|s| !s.name.starts_with("uv")), "{steps:?}");
        assert!(!dir.join(".venv").exists() && !dir.join("uv.lock").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
