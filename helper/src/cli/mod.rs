//! CLI front ends, all thin wrappers over the engine.
//!
//! `doctor` and `check` only read. `setup`, `fix` and `install` change the
//! environment, and each shares its code path with the matching editor action so
//! the terminal and the editor cannot disagree about what a command does.

mod check;
pub mod completions;
mod doctor;
mod explain;
mod fix;
pub mod logs;
mod migrate_conda;
pub mod output;
mod pixi;
mod plan;
mod setup;
mod update_data;
pub mod version;

use std::path::PathBuf;

use crate::pypi::PyPiClient;
use crate::settings::Settings;

use output::OutputFormat;

/// Dispatch a CLI mode. With `--json`, a failure that left no report on
/// stdout still prints one, carrying the error (see [`output`]).
pub async fn dispatch(mode: &str, args: &[String]) -> crate::Result<()> {
    let result = dispatch_mode(mode, args).await;
    if let Err(e) = &result {
        if OutputFormat::from_args(args).is_json() {
            // `fix python`, `fix cuda`: the name its report carries.
            let command = match (mode, positional(args)) {
                ("fix", what) => format!("fix {}", what.as_deref().unwrap_or("python")),
                ("doctor", _) if args.iter().any(|a| a == "--fix") => "doctor --fix".to_string(),
                _ => mode.to_string(),
            };
            output::print_error(&command, e);
        }
    }
    result
}

async fn dispatch_mode(mode: &str, args: &[String]) -> crate::Result<()> {
    let format = OutputFormat::from_args(args);
    // The same project the editor works on, so a task run from a worktree whose
    // Python code lives in one subfolder sets up that subfolder's `.venv`.
    let workspace = crate::core::project::project_root(&workspace_from_args(args));
    // `--pixi-env <name>`: which pixi environment is the project's, for this
    // run only (doctor, setup and the rest all read it the same way).
    if let Some(env) = flag_value(args, "--pixi-env") {
        crate::core::pixi_envs::set_override(Some(&env));
    }
    let settings = Settings::load(&workspace);
    crate::core::logs::note_context(&workspace, &format!("{:?}", settings.package_manager));
    let source = PyPiClient::new();
    // setup, fix, install and `doctor --fix`: print the plan, change nothing.
    let dry_run = plan::dry_run(args);

    match mode {
        // `--fix` runs what the editor's "Fix environment" runs: setup.
        "doctor" if args.iter().any(|a| a == "--fix") => {
            if !format.is_json() {
                doctor::run(&workspace, &settings, &source, format).await?;
                println!();
            }
            setup::run(
                &workspace,
                &settings,
                &source,
                format,
                "doctor --fix",
                dry_run,
            )
            .await
        }
        "doctor" => doctor::run(&workspace, &settings, &source, format).await,
        "setup" => setup::run(&workspace, &settings, &source, format, "setup", dry_run).await,

        "check" => {
            let Some(package) = positional(args) else {
                anyhow::bail!("usage: pypilot check <package> [--suggest-alternatives]");
            };
            let suggest = args.iter().any(|a| a == "--suggest-alternatives");
            check::run(&workspace, &settings, &source, &package, suggest, format).await
        }

        "fix" => {
            let what = positional(args).unwrap_or_else(|| "python".to_string());
            let result = fix::run(&workspace, &settings, &source, &what, format, dry_run).await;
            if !dry_run {
                crate::core::rescan::notify(&workspace, crate::core::rescan::Kind::Setup);
            }
            result
        }

        "install" => {
            let Some(package) = positional(args) else {
                anyhow::bail!("usage: pypilot install <package>");
            };
            let result = fix::install_one(&workspace, &settings, &package, format, dry_run).await;
            if !dry_run {
                crate::core::rescan::notify(&workspace, crate::core::rescan::Kind::Setup);
            }
            result
        }

        "explain" => {
            let package = positional(args);
            explain::run(&workspace, &source, package.as_deref(), format.is_json()).await
        }

        "update-data" if args.iter().any(|a| a == "--check") => {
            update_data::check(&settings, format.is_json()).await
        }
        "update-data" => update_data::run(format).await,
        "migrate-conda" => match flag_value(args, "--to").as_deref() {
            None | Some("pyproject") => migrate_conda::run(&workspace, format).await,
            Some("pixi") => {
                let opts = pixi::options(
                    args,
                    crate::core::pixi_migrate::MigrationSource::CondaEnvironment,
                );
                pixi::migrate(&workspace, &settings, opts, format).await
            }
            Some(other) => {
                anyhow::bail!(
                    "unknown --to `{other}`; use `--to pyproject` (default) or `--to pixi`"
                )
            }
        },
        "migrate-to-pixi" => {
            let opts = pixi::options(args, crate::core::pixi_migrate::MigrationSource::Auto);
            pixi::migrate(&workspace, &settings, opts, format).await
        }
        "install-pixi" => pixi::install_pixi(format).await,

        other => anyhow::bail!("unknown CLI mode `{other}`"),
    }
}

/// A command finished and wants this exit code, having already printed its
/// result (`update-data --check`'s 1 = stale, 2 = network error).
#[derive(Debug)]
pub struct Exit(pub u8);

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exit code {}", self.0)
    }
}

impl std::error::Error for Exit {}

/// First argument that is not a flag or a flag's value.
fn positional(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if a == "--path" || a == "-p" || a == "--pixi-env" || a == "--format" {
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
    fn flag_values() {
        assert_eq!(
            flag_value(&v(&["--to", "pixi"]), "--to").as_deref(),
            Some("pixi")
        );
        assert_eq!(
            flag_value(&v(&["--to=pixi"]), "--to").as_deref(),
            Some("pixi")
        );
        assert_eq!(flag_value(&v(&["--write"]), "--to"), None);
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
