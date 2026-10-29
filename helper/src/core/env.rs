//! The one place that decides which environment belongs to a project.
//!
//! A pixi project's environment is the conda prefix pixi built for it
//! (`.pixi/envs/default`), never a `.venv` that happens to sit next to the
//! manifest: pixi owns that project, and its prefix is what `pixi run` and
//! the editor's toolchain use. Everything else keeps the venv rules PyPilot
//! always had (`.venv`, `venv` or `env` with a `pyvenv.cfg`).
//!
//! Every caller that used to look at `<root>/.venv` asks [`project_env`]
//! instead, so the installed-package scan, the import diagnostics, the GPU and
//! dependency checks and the CLI all agree on the environment.

use std::path::{Path, PathBuf};

use crate::core::installed::{self, Installed};
use crate::core::project::normalize_name;
use crate::core::{pip, pixi, pixi_envs, probe};
use crate::pypi::pyversion::PyVersion;

/// What kind of environment a project uses.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum EnvKind {
    /// A virtual environment with a `pyvenv.cfg`.
    #[default]
    Venv,
    /// A pixi (conda) prefix, built from the named pixi environment.
    Pixi { name: String },
}

impl EnvKind {
    pub fn is_pixi(&self) -> bool {
        matches!(self, EnvKind::Pixi { .. })
    }

    /// How user-facing text names this kind of environment (see [`env_name`]).
    pub fn name(&self) -> &'static str {
        env_name(self.is_pixi())
    }
}

/// How user-facing text names a project's environment: `.venv`, or `the
/// pixi environment` for a pixi prefix, which has no directory the user
/// would recognise by name.
pub fn env_name(pixi: bool) -> &'static str {
    if pixi {
        "the pixi environment"
    } else {
        ".venv"
    }
}

/// A project's environment as found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectEnv {
    pub kind: EnvKind,
    /// The environment root: the venv directory or the conda prefix.
    pub path: PathBuf,
    pub python_exe: PathBuf,
    /// From `pyvenv.cfg` (venv) or `conda-meta/` (pixi).
    pub python: Option<PyVersion>,
    /// The interpreter the environment needs that is missing from disk, when
    /// it can no longer run.
    pub broken: Option<PathBuf>,
}

/// The sentence for a project with no environment yet.
pub fn no_env_sentence(pixi: bool) -> &'static str {
    if pixi {
        "This pixi project's environment is not installed yet; `pixi install` creates it."
    } else {
        "This project has no virtual environment yet."
    }
}

/// The venv directory names looked for, in order.
const VENV_DIRS: [&str; 3] = [".venv", "venv", "env"];

/// The environment of the project at `workspace`, read off disk only.
///
/// A pixi project gets its selected pixi environment (`default` unless the
/// `pixi_environment` setting or `--pixi-env` names another one; see
/// [`pixi_envs::selected`]), or `None` when pixi has
/// not installed it yet (a `.venv` beside it is not the project's environment).
/// Other projects get the first venv with a `pyvenv.cfg`.
pub fn project_env(workspace: &Path) -> Option<ProjectEnv> {
    match pixi::detect(workspace) {
        Some(project) => {
            let name = pixi_envs::selected(&project, workspace);
            pixi::env(&project, &name).map(|e| from_pixi(&project, e))
        }
        None => venv_env(workspace),
    }
}

/// [`project_env`], plus one fallback for pixi projects whose environments
/// live outside the project (pixi's `detached-environments` setting): when
/// the prefix is missing but a `pixi.lock` exists, ask `pixi info` where it
/// went. Costs one ~100 ms process, and only in that case.
pub async fn project_env_resolved(workspace: &Path) -> Option<ProjectEnv> {
    let Some(project) = pixi::detect(workspace) else {
        return venv_env(workspace);
    };
    let selected = pixi_envs::selected(&project, workspace);
    if let Some(env) = pixi::env(&project, &selected) {
        return Some(from_pixi(&project, env));
    }
    if !project.root.join("pixi.lock").is_file() {
        return None;
    }
    pixi::info_envs(&project.root)
        .await
        .into_iter()
        .find(|(name, prefix)| *name == selected && prefix.is_dir())
        .map(|(name, prefix)| from_pixi(&project, pixi::env_at(&name, &prefix)))
}

fn venv_env(workspace: &Path) -> Option<ProjectEnv> {
    VENV_DIRS.iter().find_map(|name| {
        let dir = workspace.join(name);
        let text = std::fs::read_to_string(dir.join("pyvenv.cfg")).ok()?;
        Some(ProjectEnv {
            kind: EnvKind::Venv,
            python_exe: pip::venv_python(&dir),
            python: probe::parse_pyvenv_version(&text),
            broken: probe::missing_interpreter(&dir, &text),
            path: dir,
        })
    })
}

fn from_pixi(project: &pixi::PixiProject, env: pixi::PixiEnv) -> ProjectEnv {
    // A pixi prefix has no `pyvenv.cfg`, so the venv check does not apply.
    // It is broken when its interpreter is gone although Python belongs
    // there: conda-meta records one, or the manifest asks for one (a PyPI
    // dependency needs one too). A prefix of a project that never wanted
    // Python simply has none.
    let wants_python =
        env.python_version.is_some() || project.python.is_some() || !project.pypi_deps.is_empty();
    let broken = (wants_python && !env.python_exe.exists()).then(|| env.python_exe.clone());
    ProjectEnv {
        kind: EnvKind::Pixi { name: env.name },
        path: env.prefix,
        python_exe: env.python_exe,
        python: env.python_version,
        broken,
    }
}

/// What the project can import, for the import diagnostics: the
/// distributions in its environment, plus, in a pixi project, every conda
/// package pixi installed or the manifest declares. A conda package is not
/// always a Python distribution with a `.dist-info`, and its name is not a
/// PyPI name, so without this a conda dependency would be reported as missing
/// or as unknown to PyPI.
pub fn importable(workspace: &Path, env: Option<&ProjectEnv>) -> Installed {
    let mut installed = env.map(|e| installed::scan(&e.path)).unwrap_or_default();
    let Some(project) = pixi::detect(workspace) else {
        return installed;
    };
    // The conda packages of the selected environment: its features' too.
    let selected = pixi_envs::selected(&project, workspace);
    let view = pixi_envs::compositions(&project)
        .into_iter()
        .find(|c| c.name == selected)
        .map(|c| pixi_envs::effective(&project, &c))
        .unwrap_or(project);
    let mut names: Vec<String> = view.conda_deps.iter().map(|d| d.name.clone()).collect();
    if let Some(env) = env.filter(|e| e.kind.is_pixi()) {
        names.extend(conda_meta_names(&env.path));
    }
    installed
        .packages
        .extend(names.iter().map(|n| normalize_name(n)));
    installed
}

/// Package names of the conda records in a prefix's `conda-meta/`
/// (`numpy-2.1.0-py312_0.json` -> `numpy`).
fn conda_meta_names(prefix: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(prefix.join("conda-meta")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| conda_record_name(&e.file_name().to_string_lossy()))
        .collect()
}

fn conda_record_name(file: &str) -> Option<String> {
    let stem = file.strip_suffix(".json")?;
    let mut parts = stem.rsplitn(3, '-');
    let (_build, _version, name) = (parts.next()?, parts.next()?, parts.next()?);
    (!name.is_empty()).then(|| name.to_string())
}

/// The interpreter inside an environment root of the given kind.
pub fn python_in(kind: &EnvKind, path: &Path) -> PathBuf {
    match kind {
        EnvKind::Venv => pip::venv_python(path),
        EnvKind::Pixi { .. } => pixi::interpreter_path(path),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A fresh, empty directory for one test.
    pub(crate) fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pypilot-env-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A pixi project with an installed default environment on Python 3.12,
    /// laid out as pixi 0.81 does it on this OS.
    pub(crate) fn pixi_fixture(name: &str, with_python: bool) -> (PathBuf, PathBuf) {
        let root = scratch(name);
        std::fs::write(
            root.join("pixi.toml"),
            include_str!("../../tests/fixtures/pixi/pixi-0.81-init.pixi.toml"),
        )
        .unwrap();
        let prefix = root.join(".pixi").join("envs").join("default");
        let meta = prefix.join("conda-meta");
        std::fs::create_dir_all(&meta).unwrap();
        std::fs::write(meta.join("python-3.12.15-hb12b558_1_cpython.json"), "{}").unwrap();
        std::fs::create_dir_all(installed_sp(&prefix)).unwrap();
        if with_python {
            let exe = pixi::interpreter_path(&prefix);
            std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
            std::fs::write(exe, "").unwrap();
        }
        (root, prefix)
    }

    pub(crate) fn installed_sp(prefix: &Path) -> PathBuf {
        if cfg!(windows) {
            prefix.join("Lib").join("site-packages")
        } else {
            prefix.join("lib").join("python3.12").join("site-packages")
        }
    }

    #[test]
    fn pixi_project_resolves_to_its_pixi_env() {
        let (root, prefix) = pixi_fixture("pixi", true);
        // A stray `.venv` next to the manifest is not the project's env.
        std::fs::create_dir_all(root.join(".venv")).unwrap();
        std::fs::write(root.join(".venv").join("pyvenv.cfg"), "version = 3.10.1\n").unwrap();

        let env = project_env(&root).unwrap();
        assert_eq!(
            env.kind,
            EnvKind::Pixi {
                name: "default".into()
            }
        );
        assert_eq!(env.path, prefix);
        assert_eq!(env.python_exe, pixi::interpreter_path(&prefix));
        assert_eq!(env.python, Some(PyVersion::py3(12)));
        assert_eq!(env.broken, None);
    }

    #[test]
    fn pixi_env_without_its_python_is_broken() {
        let (root, prefix) = pixi_fixture("pixi-broken", false);
        let env = project_env(&root).unwrap();
        assert_eq!(env.broken, Some(pixi::interpreter_path(&prefix)));
    }

    #[test]
    fn pixi_env_of_a_project_without_python_is_not_broken() {
        let root = scratch("pixi-no-python");
        std::fs::write(root.join("pixi.toml"), "[workspace]\nplatforms = []\n").unwrap();
        std::fs::create_dir_all(root.join(".pixi/envs/default/conda-meta")).unwrap();
        let env = project_env(&root).unwrap();
        assert!(env.kind.is_pixi());
        assert_eq!((env.python, env.broken), (None, None));
    }

    #[test]
    fn uninstalled_pixi_project_has_no_env() {
        let root = scratch("pixi-uninstalled");
        std::fs::write(
            root.join("pixi.toml"),
            include_str!("../../tests/fixtures/pixi/pixi-0.81-init.pixi.toml"),
        )
        .unwrap();
        std::fs::create_dir_all(root.join(".venv")).unwrap();
        std::fs::write(root.join(".venv").join("pyvenv.cfg"), "version = 3.10.1\n").unwrap();
        assert_eq!(project_env(&root), None);
    }

    #[test]
    fn conda_packages_count_as_importable_in_pixi_projects() {
        let (root, prefix) = pixi_fixture("importable", true);
        let meta = prefix.join("conda-meta");
        std::fs::write(meta.join("py-opencv-4.10.0-py312h1_0.json"), "{}").unwrap();
        std::fs::write(meta.join("pixi"), "{}").unwrap();
        std::fs::create_dir_all(installed_sp(&prefix).join("requests-2.34.2.dist-info")).unwrap();

        let env = project_env(&root).unwrap();
        let installed = importable(&root, Some(&env));
        assert!(installed.contains("requests"));
        assert!(installed.contains("py-opencv"));
        assert!(installed.contains("python"));
        // Declared in pixi.toml, even before pixi installed anything.
        assert!(importable(&root, None).contains("numpy"));

        // Outside pixi nothing is added.
        let plain = scratch("importable-plain");
        assert_eq!(importable(&plain, None).count(), 0);
        assert_eq!(conda_record_name("history"), None);
        assert_eq!(conda_record_name("pixi_env_prefix"), None);
    }

    #[test]
    fn plain_project_resolves_to_its_venv() {
        let root = scratch("venv");
        assert_eq!(project_env(&root), None);
        let venv = root.join("venv");
        std::fs::create_dir_all(&venv).unwrap();
        std::fs::write(venv.join("pyvenv.cfg"), "version = 3.11.4\n").unwrap();
        let py = pip::venv_python(&venv);
        std::fs::create_dir_all(py.parent().unwrap()).unwrap();
        std::fs::write(&py, "").unwrap();

        let env = project_env(&root).unwrap();
        assert_eq!(env.kind, EnvKind::Venv);
        assert_eq!(env.path, venv);
        assert_eq!(env.python_exe, py);
        assert_eq!(env.python, Some(PyVersion::py3(11)));
        assert_eq!(env.broken, None);
        assert_eq!(python_in(&env.kind, &env.path), py);
    }
}
