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
    // `--pixi-env <name>`: which pixi environment is the project's, for this
    // run only (doctor, setup and the rest all read it the same way).
    if let Some(env) = flag_value(args, "--pixi-env") {
        crate::core::pixi_envs::set_override(Some(&env));
    }
    let settings = Settings::load(&workspace);
    let source = PyPiClient::new();

    match mode {
        "doctor" => doctor::run(&workspace, &settings, &source).await,
        "setup" => setup::run(&workspace, &settings, &source).await,

        "check" => {
            let Some(package) = positional(args) else {
                anyhow::bail!("usage: pypilot check <package> [--suggest-alternatives]");
            };
            let suggest = args.iter().any(|a| a == "--suggest-alternatives");
            check::run(&workspace, &settings, &source, &package, suggest).await
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
        if a == "--path" || a == "-p" || a == "--pixi-env" {
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

/// The value of `--<flag> <value>` or `--<flag>=<value>`.
fn flag_value(args: &[String], flag: &str) -> Option<String> {
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if a == flag {
            return iter.next().cloned();
        }
        if let Some(v) = a.strip_prefix(flag).and_then(|r| r.strip_prefix('=')) {
            return Some(v.to_string());
        }
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
    fn pixi_env_flag() {
        let args = v(&["--pixi-env", "test", "numpy"]);
        assert_eq!(flag_value(&args, "--pixi-env").as_deref(), Some("test"));
        assert_eq!(positional(&args).as_deref(), Some("numpy"));
        assert_eq!(
            flag_value(&v(&["--pixi-env=gpu"]), "--pixi-env").as_deref(),
            Some("gpu")
        );
        assert_eq!(flag_value(&v(&["--path", "x"]), "--pixi-env"), None);
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
