//! Parallel environment probes.
//!
//! The whole workspace scan (excluding network) targets <200ms, so every probe
//! runs concurrently: uv presence, venv presence + its interpreter, and the set
//! of interpreters on the machine. Hardware detection (F2) will add one more
//! concurrent probe here without disturbing the shape.

use std::path::{Path, PathBuf};

use crate::core::env::{self, EnvKind, ProjectEnv};
use crate::core::interpreter::{self, Interpreter};
use crate::core::uv::{self, UvInfo};
use crate::pypi::pyversion::PyVersion;
use crate::settings::Settings;

/// The project's environment: a virtual environment, or the pixi prefix of a
/// pixi project (see [`crate::core::env`]).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct VenvInfo {
    pub path: PathBuf,
    /// Interpreter version recorded in `pyvenv.cfg` (or a pixi prefix's
    /// `conda-meta/`), if parseable.
    pub python: Option<PyVersion>,
    /// The interpreter the environment depends on that is gone from disk,
    /// when it can no longer run (see [`missing_interpreter`]).
    pub broken: Option<PathBuf>,
    pub kind: EnvKind,
}

impl VenvInfo {
    /// The environment's own interpreter.
    pub fn python_exe(&self) -> PathBuf {
        env::python_in(&self.kind, &self.path)
    }
}

impl From<ProjectEnv> for VenvInfo {
    fn from(e: ProjectEnv) -> VenvInfo {
        VenvInfo {
            path: e.path,
            python: e.python,
            broken: e.broken,
            kind: e.kind,
        }
    }
}

/// Everything the fast probe phase learned about the environment.
#[derive(Debug, Clone, Default)]
pub struct Probes {
    pub uv: Option<UvInfo>,
    pub venv: Option<VenvInfo>,
    pub interpreters: Vec<Interpreter>,
}

/// Run all probes concurrently for `workspace`.
pub async fn run(workspace: &Path, settings: &Settings) -> Probes {
    let uv_fut = uv::detect(settings);
    let venv_fut = detect_venv(workspace);
    let interp_fut = interpreter::discover();

    let (uv, venv, interpreters) = tokio::join!(uv_fut, venv_fut, interp_fut);

    Probes {
        uv,
        venv,
        interpreters,
    }
}

/// The project's environment: its pixi prefix in a pixi project, else a
/// `.venv` (or `venv`, `env`) directory with a `pyvenv.cfg`.
async fn detect_venv(workspace: &Path) -> Option<VenvInfo> {
    env::project_env_resolved(workspace)
        .await
        .map(VenvInfo::from)
}

/// [`missing_interpreter`] for `venv`, reading its `pyvenv.cfg`. `None` when
/// there is no venv there at all.
pub fn broken_venv(venv: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(venv.join("pyvenv.cfg")).ok()?;
    missing_interpreter(venv, &text)
}

/// The interpreter a venv needs that no longer exists, or `None` when it can
/// still run.
///
/// A venv is a thin shell around its base Python: when that Python is moved or
/// removed (a uv-managed Python upgraded or uninstalled, a system Python
/// removed, the project copied from another machine), every command run
/// through the venv fails with "No Python at ..." or "os error 2". Checked
/// from disk alone, without spawning the interpreter, so it is cheap enough
/// for every scan.
///
/// The base interpreter is reported first, since that is the root cause: the
/// `base-executable` (virtualenv) or `executable` (CPython 3.11+) key, then the
/// `home` directory. Then the venv's own `python`, including a symlink whose
/// target is gone.
pub fn missing_interpreter(venv: &Path, cfg: &str) -> Option<PathBuf> {
    for key in ["base-executable", "executable", "home"] {
        if let Some(path) = pyvenv_value(cfg, key).map(PathBuf::from) {
            if is_rooted(&path) && is_missing(&path) {
                return Some(path);
            }
        }
    }
    venv_interpreter_missing(venv)
}

/// The venv's own interpreter, when none of its candidates exists.
fn venv_interpreter_missing(venv: &Path) -> Option<PathBuf> {
    let primary = crate::core::pip::venv_python(venv);
    let mut candidates = vec![primary.clone()];
    if !cfg!(windows) {
        candidates.push(venv.join("bin").join("python3"));
    }
    if candidates.iter().any(|c| !is_missing(c)) {
        return None;
    }
    // A dangling symlink: name the interpreter it pointed at, which is the
    // one the user actually lost.
    for c in &candidates {
        if let Ok(target) = std::fs::read_link(c) {
            return Some(match c.parent() {
                Some(dir) if target.is_relative() => dir.join(target),
                _ => target,
            });
        }
    }
    Some(primary)
}

/// The value of `key` in a `pyvenv.cfg`, trimmed. Values can hold spaces and
/// backslashes (`home = C:\Program Files\Python312`), so only the first `=`
/// splits.
fn pyvenv_value<'a>(cfg: &'a str, key: &str) -> Option<&'a str> {
    cfg.lines().find_map(|line| {
        let (k, v) = line.split_once('=')?;
        let v = v.trim();
        (k.trim() == key && !v.is_empty()).then_some(v)
    })
}

/// An absolute path on either platform. A venv copied from Windows to POSIX
/// (or back) records the other system's paths, which are just as missing.
fn is_rooted(path: &Path) -> bool {
    if path.has_root() || path.is_absolute() {
        return true;
    }
    let s = path.to_string_lossy();
    let b = s.as_bytes();
    s.starts_with(r"\\") || (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':')
}

/// Only a definite "not found" counts. Anything else (permissions, Windows
/// app execution aliases that refuse `metadata`) is given the benefit of the
/// doubt, so a working venv is never flagged.
fn is_missing(path: &Path) -> bool {
    matches!(std::fs::metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}

/// Parse `version = 3.12.1` (or `version_info`) out of a `pyvenv.cfg`.
pub(crate) fn parse_pyvenv_version(text: &str) -> Option<PyVersion> {
    for line in text.lines() {
        let line = line.trim();
        let key = line.split('=').next().map(str::trim).unwrap_or("");
        if key == "version" || key == "version_info" {
            if let Some(val) = line.split('=').nth(1) {
                if let Some(v) = PyVersion::parse(val.trim()) {
                    return Some(v);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pyvenv_cfg_version() {
        let cfg = "home = /usr/bin\nversion = 3.12.1\ninclude-system-site-packages = false\n";
        assert_eq!(parse_pyvenv_version(cfg), Some(PyVersion::py3(12)));
    }

    /// A fresh directory standing in for a base Python install, and a venv
    /// next to it whose own interpreter exists.
    fn fixture(name: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("pypilot-broken-venv-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("base python");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("python.exe"), "").unwrap();
        let venv = root.join(".venv");
        let py = crate::core::pip::venv_python(&venv);
        std::fs::create_dir_all(py.parent().unwrap()).unwrap();
        std::fs::write(&py, "").unwrap();
        (home, venv)
    }

    fn cfg(home: &Path) -> String {
        format!(
            "home = {}\ninclude-system-site-packages = false\nversion = 3.12.1\n",
            home.display()
        )
    }

    #[test]
    fn healthy_venv_is_not_broken() {
        let (home, venv) = fixture("healthy");
        let text = format!(
            "{}executable = {}\n",
            cfg(&home),
            home.join("python.exe").display()
        );
        assert_eq!(missing_interpreter(&venv, &text), None);
        std::fs::write(venv.join("pyvenv.cfg"), &text).unwrap();
        assert_eq!(broken_venv(&venv), None);
    }

    #[test]
    fn missing_home_is_reported() {
        let (home, venv) = fixture("home");
        let gone = home.with_file_name("cpython-3.12.1-removed");
        assert_eq!(missing_interpreter(&venv, &cfg(&gone)), Some(gone));
    }

    #[test]
    fn missing_base_executable_wins_over_home() {
        let (home, venv) = fixture("exe");
        let gone = home.join("python3.11.exe");
        let text = format!("{}base-executable = {}\n", cfg(&home), gone.display());
        assert_eq!(missing_interpreter(&venv, &text), Some(gone));
    }

    #[test]
    fn missing_venv_python_is_reported() {
        let (home, venv) = fixture("venv-python");
        let py = crate::core::pip::venv_python(&venv);
        std::fs::remove_file(&py).unwrap();
        assert_eq!(missing_interpreter(&venv, &cfg(&home)), Some(py));
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_names_its_target() {
        let (home, venv) = fixture("symlink");
        let py = venv.join("bin").join("python");
        std::fs::remove_file(&py).unwrap();
        let gone = home.join("python3.12");
        std::os::unix::fs::symlink(&gone, &py).unwrap();
        assert_eq!(missing_interpreter(&venv, &cfg(&home)), Some(gone));
    }

    #[test]
    fn windows_style_paths_parse_and_are_checked() {
        let (_home, venv) = fixture("windows");
        let text = "home = C:\\Users\\me\\AppData\\Roaming\\uv\\python\\cpython-3.12.1-windows-x86_64-none\nimplementation = CPython\nuv = 0.5.0\nversion_info = 3.12.1\n";
        assert_eq!(
            pyvenv_value(text, "home"),
            Some(r"C:\Users\me\AppData\Roaming\uv\python\cpython-3.12.1-windows-x86_64-none")
        );
        assert_eq!(
            missing_interpreter(&venv, text),
            Some(PathBuf::from(
                r"C:\Users\me\AppData\Roaming\uv\python\cpython-3.12.1-windows-x86_64-none"
            ))
        );
        assert!(is_rooted(Path::new(r"D:\Python312")));
        assert!(is_rooted(Path::new(r"\\server\share\python")));
        assert!(is_rooted(Path::new("/usr/bin")));
        assert!(!is_rooted(Path::new("python")));
    }

    #[test]
    fn no_pyvenv_cfg_is_not_broken() {
        let (_home, venv) = fixture("no-cfg");
        assert_eq!(broken_venv(&venv), None);
    }
}
