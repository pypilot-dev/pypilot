//! Installing pixi itself, with pixi's official installer.
//!
//! The pixi equivalent of [`crate::core::uv::install_globally`]: it runs the
//! script pixi documents (<https://pixi.sh/install.ps1> through PowerShell on
//! Windows, <https://pixi.sh/install.sh> through `sh` elsewhere), which puts
//! the binary in `$PIXI_HOME/bin` (default `~/.pixi/bin`) and adds that to
//! PATH. The installer's own variables are honoured because they are simply
//! inherited: `PIXI_HOME`, `PIXI_VERSION`, and `PIXI_NO_PATH_UPDATE` to leave
//! PATH and the shell profile alone.
//!
//! The script is downloaded first and run from a file, rather than piped from
//! the network into a shell, so no shell string is involved here either (see
//! [`crate::core::command`]).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context};

use crate::core::command;
use crate::core::pixi;

/// pixi's official Windows installer.
pub const INSTALL_PS1_URL: &str = "https://pixi.sh/install.ps1";
/// pixi's official Linux and macOS installer.
pub const INSTALL_SH_URL: &str = "https://pixi.sh/install.sh";

/// The installer downloads a ~20 MB binary; give it room on slow links.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(600);

/// A working pixi after [`install`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PixiInstall {
    pub path: PathBuf,
    pub version: String,
    /// True when pixi was already there and nothing was run.
    pub already_installed: bool,
}

/// The installer script for this OS.
pub fn installer_url() -> &'static str {
    if cfg!(windows) {
        INSTALL_PS1_URL
    } else {
        INSTALL_SH_URL
    }
}

/// File name the downloaded script is saved under.
fn script_name() -> &'static str {
    if cfg!(windows) {
        "pixi-install.ps1"
    } else {
        "pixi-install.sh"
    }
}

/// The program and arguments that run the installer saved at `script`.
pub fn installer_command(script: &Path) -> (String, Vec<String>) {
    let script = script.to_string_lossy().into_owned();
    if cfg!(windows) {
        (
            "powershell".to_string(),
            vec![
                "-NoProfile".to_string(),
                "-ExecutionPolicy".to_string(),
                "Bypass".to_string(),
                "-File".to_string(),
                script,
            ],
        )
    } else {
        ("sh".to_string(), vec![script])
    }
}

/// The command a user would run by hand, as pixi's docs give it.
pub fn manual_command() -> &'static str {
    if cfg!(windows) {
        "powershell -ExecutionPolicy Bypass -c \"irm -useb https://pixi.sh/install.ps1 | iex\""
    } else {
        "curl -fsSL https://pixi.sh/install.sh | sh"
    }
}

/// Command that puts pixi on PATH in a shell opened before the install.
pub fn refresh_path_command() -> &'static str {
    // The installer writes PATH (user registry, shell profile) for new
    // shells only.
    if cfg!(windows) {
        "$env:Path = [System.Environment]::GetEnvironmentVariable(\"Path\",\"Machine\") + \";\" + [System.Environment]::GetEnvironmentVariable(\"Path\",\"User\")"
    } else {
        "export PATH=\"$HOME/.pixi/bin:$PATH\""
    }
}

/// Install pixi with the official installer, unless a working pixi is
/// already found (PATH, `$PIXI_HOME/bin`, `~/.pixi/bin` or `PYPILOT_PIXI`).
pub async fn install() -> crate::Result<PixiInstall> {
    if let Some((path, version)) = located().await {
        return Ok(PixiInstall {
            path,
            version,
            already_installed: true,
        });
    }

    let dir = std::env::temp_dir().join(format!("pypilot-pixi-install-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let script = dir.join(script_name());
    let url = installer_url();
    let text = download(url)
        .await
        .with_context(|| format!("downloading the pixi installer from {url}"))?;
    std::fs::write(&script, text)?;

    let (program, args) = installer_command(&script);
    let out = command::run_with_timeout(&program, &args, Some(&dir), INSTALL_TIMEOUT).await;
    let _ = std::fs::remove_dir_all(&dir);
    let out = out?;
    if !out.success() {
        let detail = if out.stderr.trim().is_empty() {
            out.stdout.trim()
        } else {
            out.stderr.trim()
        };
        bail!("the pixi installer failed: {detail}");
    }

    match located().await {
        Some((path, version)) => Ok(PixiInstall {
            path,
            version,
            already_installed: false,
        }),
        None => bail!(
            "the pixi installer finished, but no working pixi was found on PATH or in ~/.pixi/bin"
        ),
    }
}

async fn located() -> Option<(PathBuf, String)> {
    let path = pixi::locate().await?;
    let version = pixi::pixi_version().await.unwrap_or_default();
    Some((path, version))
}

async fn download(url: &str) -> crate::Result<String> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("pypilot/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()?;
    let text = client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_the_official_script_from_a_file() {
        let script = Path::new("dir").join(script_name());
        let (program, args) = installer_command(&script);
        if cfg!(windows) {
            assert_eq!(program, "powershell");
            assert_eq!(
                args[..4],
                ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]
            );
            assert!(args[4].ends_with("pixi-install.ps1"));
            assert_eq!(installer_url(), "https://pixi.sh/install.ps1");
        } else {
            assert_eq!(program, "sh");
            assert!(args[0].ends_with("pixi-install.sh"));
            assert_eq!(installer_url(), "https://pixi.sh/install.sh");
        }
        assert!(manual_command().contains(installer_url()));
    }
}
