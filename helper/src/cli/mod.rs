//! CLI front ends, all thin wrappers over the engine.
//!
//! `doctor` and `check` only read. `setup`, `fix` and `install` change the
//! environment, and each shares its code path with the matching editor action so
//! the terminal and the editor cannot disagree about what a command does.

mod check;
mod doctor;
mod explain;
mod fix;
mod migrate_conda;
mod setup;
mod update_data;

use std::path::PathBuf;

use crate::pypi::PyPiClient;
use crate::settings::Settings;

/// Dispatch a CLI mode.
pub async fn dispatch(mode: &str, args: &[String]) -> crate::Result<()> {
    // The same project the editor works on, so a task run from a worktree whose
    // Python code lives in one subfolder sets up that subfolder's `.venv`.
    let workspace = crate::core::project::project_root(&workspace_from_args(args));
    let settings = Settings::load(&workspace);
    let source = PyPiClient::new();

    match mode {
        "doctor" => doctor::run(&workspace, &settings, &source).await,
        "setup" => setup::run(&workspace, &settings, &source).await,

        "check" => {
            let Some(package) = positional(args) else {
                anyhow::bail!("usage: pypilot check <package>");
            };
            check::run(&workspace, &settings, &source, &package).await
        }

        "fix" => {
            let what = positional(args).unwrap_or_else(|| "python".to_string());
            let result = fix::run(&workspace, &settings, &source, &what).await;
            crate::core::rescan::notify(&workspace, crate::core::rescan::Kind::Setup);
            result
        }

        "install" => {
            let Some(package) = positional(args) else {
                anyhow::bail!("usage: pypilot install <package>");
            };
            let result = fix::install_one(&workspace, &settings, &package).await;
            crate::core::rescan::notify(&workspace, crate::core::rescan::Kind::Setup);
            result
        }

        "explain" => {
            let package = positional(args);
            let json = args.iter().any(|a| a == "--json");
            explain::run(&workspace, &source, package.as_deref(), json).await
        }

        "update-data" => update_data::run().await,
        "migrate-conda" => migrate_conda::run(&workspace).await,

        other => anyhow::bail!("unknown CLI mode `{other}`"),
    }
}

/// First argument that is not a flag or a flag's value.
fn positional(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if a == "--path" || a == "-p" {
            iter.next();
            continue;
        }
        if a.starts_with('-') {
            continue;
        }
        return Some(a.clone());
    }
    None
}

/// Workspace directory from `--path <dir>`, defaulting to the current directory.
fn workspace_from_args(args: &[String]) -> PathBuf {
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if a == "--path" || a == "-p" {
            if let Some(dir) = iter.next() {
                return PathBuf::from(dir);
            }
        } else if let Some(dir) = a.strip_prefix("--path=") {
            return PathBuf::from(dir);
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn positional_skips_the_path_flag_and_its_value() {
        assert_eq!(
            positional(&v(&["--path", "/tmp/proj", "mediapipe"])).as_deref(),
            Some("mediapipe")
        );
        assert_eq!(
            positional(&v(&["mediapipe", "--path", "/tmp/proj"])).as_deref(),
            Some("mediapipe")
        );
        assert_eq!(positional(&v(&["--path", "/tmp/proj"])), None);
    }

    #[test]
    fn workspace_accepts_both_flag_forms() {
        assert_eq!(
            workspace_from_args(&v(&["--path", "/tmp/proj"])),
            PathBuf::from("/tmp/proj")
        );
        assert_eq!(
            workspace_from_args(&v(&["--path=/tmp/proj"])),
            PathBuf::from("/tmp/proj")
        );
    }
}
