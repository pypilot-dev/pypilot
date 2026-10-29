//! PyPilot Zed extension — the thin WASM shim.
//!
//! Deliberately dumb. Its entire job is:
//!   1. Detect the platform.
//!   2. Reuse a `pypilot` already on `$PATH` (dev convenience), else download the
//!      matching prebuilt helper release asset from GitHub and cache it.
//!   3. Register that binary as a language server (`pypilot lsp`).
//!
//! All real logic lives in the native helper, which is editor-agnostic. If Zed's
//! extension API changes, only this file changes — that's the whole point.

use std::fs;
use zed_extension_api::{self as zed, LanguageServerId, Result};

/// GitHub repo that hosts the helper release assets. Must match the release
/// workflow in `.github/workflows/release.yml`.
const HELPER_REPO: &str = "pypilot-dev/pypilot";

/// Oldest helper version this shim trusts when it finds one on `$PATH`. A
/// helper older than this predates whatever the shim currently assumes about
/// its CLI, so using it silently would surface as a confusing failure deep in
/// the language server instead of a clear one here.
const MIN_HELPER_VERSION: (u32, u32, u32) = (0, 1, 0);

struct PyPilotExtension {
    /// Cached path to a working helper binary, to avoid re-probing every launch.
    cached_binary_path: Option<String>,
}

impl PyPilotExtension {
    /// Resolve the helper binary, downloading it if necessary. Follows the
    /// standard Zed language-server-extension download + version-pinning pattern.
    fn helper_binary_path(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<String> {
        // 1. Respect a user- or system-installed `pypilot` on PATH, once its
        // version checks out. A stale leftover from an old install would
        // otherwise get used silently and misbehave in ways that look like
        // an extension bug rather than an outdated helper.
        if let Some(path) = worktree.which("pypilot") {
            match check_helper_version(&path) {
                Ok(version) => {
                    eprintln!(
                        "pypilot: using {path} on $PATH (version {}.{}.{})",
                        version.0, version.1, version.2
                    );
                    return Ok(path);
                }
                Err(reason) => {
                    eprintln!(
                        "pypilot: ignoring {path} on $PATH ({reason}); downloading a managed copy instead"
                    );
                    zed::set_language_server_installation_status(
                        language_server_id,
                        &zed::LanguageServerInstallationStatus::Failed(format!(
                            "pypilot on $PATH at {path} was rejected ({reason}); downloading a managed copy instead"
                        )),
                    );
                }
            }
        }

        // 2. Reuse a previously downloaded binary if it still exists on disk.
        if let Some(path) = &self.cached_binary_path {
            if fs::metadata(path)
                .map(|stat| stat.is_file())
                .unwrap_or(false)
            {
                return Ok(path.clone());
            }
        }

        // 3. Download the matching release asset from GitHub.
        zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::CheckingForUpdate,
        );

        let (platform, arch) = zed::current_platform();
        let asset = AssetSpec::for_platform(platform, arch)?;

        let release = match zed::latest_github_release(
            HELPER_REPO,
            zed::GithubReleaseOptions {
                require_assets: true,
                pre_release: false,
            },
        ) {
            Ok(release) => release,
            // Offline, or rate-limited by the GitHub API. A helper downloaded on
            // an earlier launch still works, so start that one instead of
            // failing the language server.
            Err(e) => match find_downloaded_binary(asset.binary_name()) {
                Some(path) => {
                    eprintln!("pypilot: could not check for a newer helper ({e}); using {path}");
                    zed::set_language_server_installation_status(
                        language_server_id,
                        &zed::LanguageServerInstallationStatus::None,
                    );
                    self.cached_binary_path = Some(path.clone());
                    return Ok(path);
                }
                None => return Err(e),
            },
        };

        let asset_name = asset.file_name();

        let github_asset = release
            .assets
            .iter()
            .find(|a| a.name == asset_name)
            .ok_or_else(|| {
                format!(
                    "no PyPilot helper asset named `{asset_name}` in release {}",
                    release.version
                )
            })?;

        // Version-pinned install dir; keeps old versions around only until the
        // new one is verified, then prunes them.
        let version_dir = format!("pypilot-{}", release.version);
        let binary_path = format!("{version_dir}/{}", asset.binary_name());

        if !fs::metadata(&binary_path)
            .map(|s| s.is_file())
            .unwrap_or(false)
        {
            zed::set_language_server_installation_status(
                language_server_id,
                &zed::LanguageServerInstallationStatus::Downloading,
            );

            zed::download_file(&github_asset.download_url, &version_dir, asset.file_type())
                .map_err(|e| format!("failed to download PyPilot helper: {e}"))?;

            zed::make_file_executable(&binary_path)
                .map_err(|e| format!("failed to mark helper executable: {e}"))?;

            prune_old_versions(&version_dir);
        }

        self.cached_binary_path = Some(binary_path.clone());
        Ok(binary_path)
    }
}

impl zed::Extension for PyPilotExtension {
    fn new() -> Self {
        Self {
            cached_binary_path: None,
        }
    }

    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        let binary = self.helper_binary_path(language_server_id, worktree)?;
        Ok(zed::Command {
            command: binary,
            // The one and only mode Zed ever launches: the LSP server.
            args: vec!["lsp".to_string()],
            env: Default::default(),
        })
    }
}

/// Run `<path> --version` and check the result against [`MIN_HELPER_VERSION`].
/// Returns the parsed version on success, or a human-readable reason to
/// reject the binary.
fn check_helper_version(path: &str) -> Result<(u32, u32, u32), String> {
    let output = zed::Command::new(path)
        .arg("--version")
        .output()
        .map_err(|e| format!("failed to run `{path} --version`: {e}"))?;

    if output.status != Some(0) {
        return Err(format!(
            "`{path} --version` exited with status {:?}",
            output.status
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let version = parse_helper_version(stdout.trim())
        .ok_or_else(|| format!("could not parse a version from `{}`", stdout.trim()))?;

    if version < MIN_HELPER_VERSION {
        return Err(format!(
            "version {}.{}.{} is older than the {}.{}.{} this shim requires",
            version.0,
            version.1,
            version.2,
            MIN_HELPER_VERSION.0,
            MIN_HELPER_VERSION.1,
            MIN_HELPER_VERSION.2
        ));
    }

    Ok(version)
}

/// Parses the `pypilot --version` output (`"pypilot X.Y.Z"`) into its numeric
/// parts. Only the first line counts, so a helper that prints more details
/// after it still parses.
fn parse_helper_version(stdout: &str) -> Option<(u32, u32, u32)> {
    let first = stdout.lines().next()?.trim();
    let version = first.strip_prefix("pypilot ")?;
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

/// Per-platform release asset description. Asset names MUST match the artifacts
/// produced by `.github/workflows/release.yml`.
struct AssetSpec {
    /// e.g. "linux-x64", "darwin-arm64", "windows-x64".
    slug: &'static str,
    windows: bool,
}

impl AssetSpec {
    fn for_platform(os: zed::Os, arch: zed::Architecture) -> Result<Self> {
        let spec = match (os, arch) {
            (zed::Os::Linux, zed::Architecture::X8664) => AssetSpec {
                slug: "linux-x64",
                windows: false,
            },
            (zed::Os::Mac, zed::Architecture::X8664) => AssetSpec {
                slug: "darwin-x64",
                windows: false,
            },
            (zed::Os::Mac, zed::Architecture::Aarch64) => AssetSpec {
                slug: "darwin-arm64",
                windows: false,
            },
            (zed::Os::Windows, zed::Architecture::X8664) => AssetSpec {
                slug: "windows-x64",
                windows: true,
            },
            (os, arch) => {
                return Err(format!(
                    "unsupported platform for PyPilot helper: {os:?}/{arch:?}"
                ))
            }
        };
        Ok(spec)
    }

    /// Downloaded archive name, e.g. `pypilot-linux-x64.tar.gz`.
    fn file_name(&self) -> String {
        let ext = if self.windows { "zip" } else { "tar.gz" };
        format!("pypilot-{}.{ext}", self.slug)
    }

    /// Name of the binary inside the extracted archive dir.
    fn binary_name(&self) -> &'static str {
        if self.windows {
            "pypilot.exe"
        } else {
            "pypilot"
        }
    }

    fn file_type(&self) -> zed::DownloadedFileType {
        if self.windows {
            zed::DownloadedFileType::Zip
        } else {
            zed::DownloadedFileType::GzipTar
        }
    }
}

/// A helper left in a `pypilot-*` install dir by an earlier download, if any.
/// [`prune_old_versions`] keeps only one such dir around.
fn find_downloaded_binary(binary_name: &str) -> Option<String> {
    let entries = fs::read_dir(".").ok()?;
    entries.flatten().find_map(|entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("pypilot-") {
            return None;
        }
        let path = format!("{name}/{binary_name}");
        fs::metadata(&path)
            .map(|stat| stat.is_file())
            .unwrap_or(false)
            .then_some(path)
    })
}

/// Remove any `pypilot-*` install dirs other than the one we just verified, so
/// the extension work dir doesn't accumulate stale helper versions.
fn prune_old_versions(keep_dir: &str) {
    let Ok(entries) = fs::read_dir(".") else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("pypilot-") && name != keep_dir {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

zed::register_extension!(PyPilotExtension);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_helpers_own_version_output() {
        assert_eq!(parse_helper_version("pypilot 0.1.0"), Some((0, 1, 0)));
        assert_eq!(parse_helper_version("pypilot 1.12.3"), Some((1, 12, 3)));
        assert_eq!(
            parse_helper_version("pypilot 1.3.0\ncommit   abc123\ntarget   x86_64"),
            Some((1, 3, 0))
        );
    }

    #[test]
    fn rejects_output_that_is_not_a_version() {
        assert_eq!(parse_helper_version(""), None);
        assert_eq!(parse_helper_version("pypilot"), None);
        assert_eq!(parse_helper_version("not pypilot at all"), None);
    }

    #[test]
    fn min_helper_version_orders_as_expected() {
        assert!((0, 0, 9) < MIN_HELPER_VERSION);
        assert!(MIN_HELPER_VERSION <= (0, 1, 0));
    }
}
