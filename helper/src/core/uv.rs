//! uv driver — detection, managed download, and the install operations.
//!
//! uv is never installed system-wide. If it isn't already on `$PATH`, PyPilot
//! fetches the correct standalone archive for the OS/arch from astral-sh/uv's
//! GitHub releases into a managed data dir, extracts it, and verifies it with
//! `uv --version` before ever using it.
//!
//! In `package_manager = "pip"` mode this module is inert: [`detect`] returns
//! `None` and [`ensure`] refuses — nothing here downloads or invokes uv.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use serde::Deserialize;

use crate::core::command::{self, Output};
use crate::core::exec::{self, Effect};
use crate::core::platform::{self, Platform};
use crate::pypi::pyversion::PyVersion;
use crate::settings::{PackageManager, Settings};

/// A usable uv binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UvInfo {
    pub path: PathBuf,
    pub version: String,
    /// True when this is the PyPilot-managed copy (vs. one found on `$PATH`).
    pub managed: bool,
}

/// Where the managed uv binary lives.
fn managed_uv_path() -> PathBuf {
    let name = if cfg!(windows) { "uv.exe" } else { "uv" };
    platform::data_dir().join("uv").join(name)
}

/// Check if uv is installed on the system PATH or standard install paths (ignoring any managed copy).
pub async fn is_system_installed() -> Option<UvInfo> {
    if let Some(version) = command::probe_version("uv", &["--version"]).await {
        return Some(UvInfo {
            path: PathBuf::from("uv"),
            version: clean_version(&version),
            managed: false,
        });
    }

    // Check standard global install locations in case the current process environment has a stale PATH.
    if let Some(base) = directories::BaseDirs::new() {
        let bin_name = if cfg!(windows) { "uv.exe" } else { "uv" };
        let candidates = [
            base.home_dir().join(".local").join("bin").join(bin_name),
            base.home_dir().join(".cargo").join("bin").join(bin_name),
        ];
        for candidate in candidates {
            if candidate.is_file() {
                if let Some(version) = command::probe_version(&candidate, &["--version"]).await {
                    return Some(UvInfo {
                        path: candidate,
                        version: clean_version(&version),
                        managed: false,
                    });
                }
            }
        }
    }

    None
}

/// Install uv globally using the official astral-sh installer.
pub async fn install_globally() -> crate::Result<UvInfo> {
    if let Some(info) = is_system_installed().await {
        return Ok(info);
    }

    let out = if cfg!(windows) {
        command::run(
            "powershell",
            &[
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                "irm https://astral.sh/uv/install.ps1 | iex",
            ],
            None,
        )
        .await?
    } else {
        command::run(
            "sh",
            &["-c", "curl -LsSf https://astral.sh/uv/install.sh | sh"],
            None,
        )
        .await?
    };

    if !out.success() {
        bail!("uv global installer failed: {}", out.stderr.trim());
    }

    // Prepend ~/.local/bin to current process PATH so that subsequent command calls find uv.
    if let Some(base) = directories::BaseDirs::new() {
        let local_bin = base.home_dir().join(".local").join("bin");
        if local_bin.is_dir() {
            if let Ok(current_path) = std::env::var("PATH") {
                let sep = if cfg!(windows) { ";" } else { ":" };
                std::env::set_var(
                    "PATH",
                    format!("{}{sep}{current_path}", local_bin.display()),
                );
            }
        }
    }

    if let Some(info) = is_system_installed().await {
        Ok(info)
    } else {
        bail!(
            "uv was installed globally, but could not be verified on PATH or standard directories"
        );
    }
}

/// Command to refresh PATH in an existing shell session.
pub fn refresh_path_command() -> &'static str {
    if cfg!(windows) {
        "$env:Path = [System.Environment]::GetEnvironmentVariable(\"Path\",\"Machine\") + \";\" + [System.Environment]::GetEnvironmentVariable(\"Path\",\"User\")"
    } else {
        "export PATH=\"$HOME/.local/bin:$PATH\""
    }
}

/// Detect an available uv, without downloading.
///
/// In `Pip` and `Pixi` mode, returns `None`.
/// In `Auto` mode, checks system PATH / standard locations only (never uses managed copy).
/// In `Uv` or `UvPip` mode, checks system first, then falls back to the managed copy.
pub async fn detect(settings: &Settings) -> Option<UvInfo> {
    match settings.package_manager {
        PackageManager::Pip | PackageManager::Pixi => None,
        PackageManager::Auto => is_system_installed().await,
        PackageManager::Uv | PackageManager::UvPip => {
            if let Some(info) = is_system_installed().await {
                return Some(info);
            }
            let managed = managed_uv_path();
            if managed.is_file() {
                if let Some(version) = command::probe_version(&managed, &["--version"]).await {
                    return Some(UvInfo {
                        path: managed,
                        version: clean_version(&version),
                        managed: true,
                    });
                }
            }
            None
        }
    }
}

/// Ensure a usable uv exists.
///
/// In `Pip` mode: errors because uv is disabled.
/// In `Auto` mode: returns system uv if installed; errors if missing (never auto-downloads).
/// In `Uv` / `UvPip` mode: uses existing uv if detected, otherwise downloads the managed copy.
pub async fn ensure(settings: &Settings) -> crate::Result<UvInfo> {
    match settings.package_manager {
        PackageManager::Pip => bail!("uv is disabled in pip mode (package_manager = \"pip\")"),
        PackageManager::Pixi => {
            bail!("uv is disabled in pixi mode (package_manager = \"pixi\"); pixi manages the environment")
        }
        PackageManager::Auto => {
            if let Some(info) = is_system_installed().await {
                Ok(info)
            } else {
                bail!("uv is not installed on system PATH");
            }
        }
        PackageManager::Uv | PackageManager::UvPip => {
            if let Some(info) = detect(settings).await {
                return Ok(info);
            }
            let managed = managed_uv_path();
            let effect = Effect::new(
                "download uv",
                format!(
                    "uv is not installed; fetch {} from astral-sh/uv's latest GitHub release",
                    uv_asset_name(Platform::current())
                ),
            )
            .writes(&managed);
            if exec::note(effect) {
                return Ok(UvInfo {
                    path: managed,
                    version: "(not yet downloaded)".into(),
                    managed: true,
                });
            }
            download_managed(Platform::current()).await
        }
    }
}

/// Download, extract, and verify the standalone uv for `platform`.
async fn download_managed(platform: Platform) -> crate::Result<UvInfo> {
    let asset_name = uv_asset_name(platform);
    let url = github_asset_url("astral-sh/uv", &asset_name)
        .await
        .with_context(|| format!("locating uv release asset `{asset_name}`"))?;

    let bytes = http_get_bytes(&url)
        .await
        .with_context(|| format!("downloading uv from {url}"))?;

    let dest = platform::data_dir().join("uv");
    tokio::fs::create_dir_all(&dest).await.ok();

    if asset_name.ends_with(".zip") {
        extract_zip(&bytes, &dest)?;
    } else {
        extract_tar_gz(&bytes, &dest)?;
    }

    let binary = locate_binary(&dest).context("uv binary not found in extracted archive")?;
    #[cfg(unix)]
    make_executable(&binary)?;

    // Verify before trusting it.
    let version = command::probe_version(&binary, &["--version"])
        .await
        .context("downloaded uv failed `uv --version` verification")?;

    // Normalize into the canonical managed path.
    let canonical = managed_uv_path();
    if binary != canonical {
        if let Some(parent) = canonical.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        tokio::fs::copy(&binary, &canonical)
            .await
            .with_context(|| format!("copying uv to {}", canonical.display()))?;
        #[cfg(unix)]
        make_executable(&canonical)?;
    }

    Ok(UvInfo {
        path: canonical,
        version: clean_version(&version),
        managed: true,
    })
}

/// astral-sh/uv standalone asset name for a platform.
fn uv_asset_name(platform: Platform) -> String {
    let triple = platform.uv_triple();
    if platform.os == platform::Os::Windows {
        format!("uv-{triple}.zip")
    } else {
        format!("uv-{triple}.tar.gz")
    }
}

// --- install operations -----------------------------------------------------

/// `uv python install <X.Y>` — fetch a CPython if the machine lacks it.
pub async fn python_install(uv: &UvInfo, version: PyVersion, cwd: &Path) -> crate::Result<Output> {
    let effect = Effect::new(
        format!("uv python install {version}"),
        format!(
            "make CPython {version} available (uv downloads it into its own Python store if missing)"
        ),
    );
    exec::run(
        &uv.path,
        &["python", "install", &version.to_string()],
        Some(cwd),
        effect,
    )
    .await
}

/// `uv venv --clear --python <X.Y> <path>`.
///
/// `--clear` is not optional for us. Since uv 0.9 the command refuses to touch a
/// directory that already holds an environment, which would make both the
/// bootstrap and the rebuild action fail on every project that has been set up
/// once already. Replacing is what the caller means in both cases.
pub async fn create_venv(
    uv: &UvInfo,
    version: PyVersion,
    venv_path: &Path,
    cwd: &Path,
) -> crate::Result<Output> {
    let effect = Effect::new(
        "uv venv",
        format!("create the project environment on Python {version}, replacing any existing one"),
    )
    .writes(venv_path);
    exec::run(&uv.path, &venv_args(version, venv_path), Some(cwd), effect).await
}

/// The argument vector for [`create_venv`], split out so a test can assert on it
/// without needing a uv binary present.
fn venv_args(version: PyVersion, venv_path: &Path) -> Vec<String> {
    vec![
        "venv".to_string(),
        "--clear".to_string(),
        "--python".to_string(),
        version.to_string(),
        venv_path.to_string_lossy().into_owned(),
    ]
}

/// `uv sync` (pyproject/lockfile projects).
pub async fn sync(uv: &UvInfo, cwd: &Path) -> crate::Result<Output> {
    let effect = Effect::new(
        "uv sync",
        "install the dependencies pyproject.toml (and uv.lock) declare",
    )
    .writes(cwd.join("uv.lock"))
    .writes(cwd.join(".venv"));
    exec::run(&uv.path, &["sync"], Some(cwd), effect).await
}

/// `uv init` — scaffold a new pyproject.toml in the current directory.
///
/// Only called when no `pyproject.toml` exists yet (fresh project or a project
/// that was previously requirements.txt-only). It lets subsequent `uv add`
/// calls record dependencies properly rather than operating in a stateless mode.
///
/// - `--bare` writes only `pyproject.toml`. A plain `uv init` also adds a git
///   repository, a README, sample code and a build system to a folder that
///   already holds the user's project.
/// - `--python` sets `requires-python` from the interpreter PyPilot chose,
///   and `--pin-python` writes it to `.python-version`. Without them uv takes
///   its own default (often the newest Python, or a global `uv python pin`),
///   and the next `uv add` silently rebuilds `.venv` on that version, undoing
///   the choice the dependencies needed.
/// - `--no-workspace` keeps uv from attaching the project to a parent
///   workspace further up the directory tree.
///
/// uv releases older than `--bare` reject it; they get the old invocation.
pub async fn init(uv: &UvInfo, python: Option<PyVersion>, cwd: &Path) -> crate::Result<Output> {
    let effect = || {
        let why = match python {
            Some(py) => format!(
                "the project has no pyproject.toml; create a bare one pinned to Python {py} so `uv add` has a manifest to record dependencies in"
            ),
            None => "the project has no pyproject.toml; create a bare one so `uv add` has a manifest to record dependencies in".to_string(),
        };
        let mut e = Effect::new("uv init", why).writes(cwd.join("pyproject.toml"));
        if python.is_some() {
            e = e.writes(cwd.join(".python-version"));
        }
        e
    };
    let out = exec::run(&uv.path, &init_args(python, true), Some(cwd), effect()).await?;
    if !out.success() && out.stderr.contains("unexpected argument") {
        return exec::run(&uv.path, &init_args(python, false), Some(cwd), effect()).await;
    }
    Ok(out)
}

/// The argument vector for [`init`], split out so a test can assert on it.
fn init_args(python: Option<PyVersion>, bare: bool) -> Vec<String> {
    let mut args = vec!["init".to_string()];
    if bare {
        args.push("--bare".to_string());
    }
    args.push("--no-workspace".to_string());
    if let Some(py) = python {
        // A plain `uv init` pins by default; only the bare form needs asking.
        if bare {
            args.push("--pin-python".to_string());
        }
        args.push("--python".to_string());
        args.push(py.to_string());
    }
    args
}

/// `uv python pin <version>`: record the project's Python in `.python-version`.
///
/// `uv sync` and `uv add` rebuild `.venv` on whatever Python the project's
/// pin file, or failing that a global `uv python pin`, asks for. Pinning the
/// version PyPilot just built the environment on keeps them from replacing it.
pub async fn pin(uv: &UvInfo, version: PyVersion, cwd: &Path) -> crate::Result<Output> {
    let effect = Effect::new(
        format!("uv python pin {version}"),
        format!(
            "record Python {version} in .python-version so `uv sync` and `uv add` keep the environment on it"
        ),
    )
    .writes(cwd.join(".python-version"));
    exec::run(
        &uv.path,
        &["python", "pin", &version.to_string()],
        Some(cwd),
        effect,
    )
    .await
}

/// `uv add <pkgs...>` and migrate any existing `requirements.txt` entries.
///
/// When a `requirements.txt` exists alongside the newly-created `pyproject.toml`,
/// we read its package names and pass them all to a single `uv add` invocation
/// so they land in `[project.dependencies]` before the caller's `package` is
/// added. This avoids leaving a stale `requirements.txt` that pip-based tools
/// would re-read incorrectly.
///
/// `uv add -r <file>`: records dependencies from a requirements file into pyproject.toml and installs them.
pub async fn add_requirements(
    uv: &UvInfo,
    requirements_file: &Path,
    index_url: Option<&str>,
    cwd: &Path,
) -> crate::Result<Output> {
    let mut args: Vec<String> = vec!["add".into(), "-r".into()];
    let file_arg = if let Ok(rel) = requirements_file.strip_prefix(cwd) {
        rel.display().to_string()
    } else {
        requirements_file.display().to_string()
    };
    args.push(file_arg);
    if let Some(url) = index_url {
        args.push("--index".into());
        args.push(url.to_string());
    }
    let effect = Effect::new(
        "uv add -r requirements.txt",
        "move requirements.txt's dependencies into pyproject.toml and install them",
    );
    exec::run(&uv.path, &args, Some(cwd), add_writes(effect, cwd)).await
}

/// What every `uv add` changes: the manifest, the lock and the environment.
fn add_writes(effect: Effect, cwd: &Path) -> Effect {
    effect
        .writes(cwd.join("pyproject.toml"))
        .writes(cwd.join("uv.lock"))
        .writes(cwd.join(".venv"))
}

/// `uv add -r requirements.txt <pkgs...>`: records dependencies from requirements.txt
/// and any new packages into pyproject.toml.
pub async fn add_with_requirements_migration(
    uv: &UvInfo,
    packages: &[String],
    index_url: Option<&str>,
    cwd: &Path,
) -> crate::Result<Output> {
    let req_path = cwd.join("requirements.txt");
    if req_path.is_file() {
        let mut args: Vec<String> = vec!["add".into(), "-r".into(), "requirements.txt".into()];
        if let Some(url) = index_url {
            args.push("--index".into());
            args.push(url.to_string());
        }
        args.extend(packages.iter().cloned());
        let effect = Effect::new(
            format!("uv add -r requirements.txt {}", packages.join(" ")),
            "record requirements.txt's dependencies and the new packages in pyproject.toml, and install them",
        );
        exec::run(&uv.path, &args, Some(cwd), add_writes(effect, cwd)).await
    } else {
        add(uv, packages, index_url, cwd).await
    }
}

/// `uv add <pkgs...>` — records the dependency in pyproject.toml and installs it.
/// Preferred over `uv pip install` for pyproject projects because uv edits the
/// manifest itself, preserving formatting and comments.
pub async fn add(
    uv: &UvInfo,
    packages: &[String],
    index_url: Option<&str>,
    cwd: &Path,
) -> crate::Result<Output> {
    let mut args: Vec<String> = vec!["add".into()];
    if let Some(url) = index_url {
        args.push("--index".into());
        args.push(url.to_string());
    }
    args.extend(packages.iter().cloned());
    let effect = Effect::new(
        format!("uv add {}", packages.join(" ")),
        "record the packages in pyproject.toml and install them",
    );
    exec::run(&uv.path, &args, Some(cwd), add_writes(effect, cwd)).await
}

/// `uv add --dev <pkgs...>` — records the packages in the `dev` dependency group
/// and installs them.
///
/// Used for tooling the editor needs but the program does not, such as
/// `ipykernel` for notebooks: it must never become a runtime dependency that
/// ships with the user's package.
pub async fn add_dev(uv: &UvInfo, packages: &[String], cwd: &Path) -> crate::Result<Output> {
    let effect = Effect::new(
        format!("uv add --dev {}", packages.join(" ")),
        "record the packages in the dev dependency group and install them",
    );
    exec::run(
        &uv.path,
        &add_dev_args(packages),
        Some(cwd),
        add_writes(effect, cwd),
    )
    .await
}

/// The argument vector for [`add_dev`], split out so a test can assert on it
/// without needing a uv binary present.
fn add_dev_args(packages: &[String]) -> Vec<String> {
    let mut args: Vec<String> = vec!["add".into(), "--dev".into()];
    args.extend(packages.iter().cloned());
    args
}

/// `uv pip install <pkgs...>` into the project venv.
///
/// `index_url` overrides the default index for this call only — used to point
/// a torch/tensorflow install at the CUDA build matched to the machine's GPU
/// driver (see [`crate::matrix::solve`]), without affecting any other package.
pub async fn pip_install(
    uv: &UvInfo,
    packages: &[String],
    index_url: Option<&str>,
    cwd: &Path,
) -> crate::Result<Output> {
    let mut args: Vec<String> = vec!["pip".into(), "install".into()];
    if let Some(url) = index_url {
        args.push("--index-url".into());
        args.push(url.to_string());
    }
    args.extend(packages.iter().cloned());
    let why = match index_url {
        Some(url) => format!("install into .venv from {url}, the build matched to this machine"),
        None => "install into .venv".to_string(),
    };
    let effect = Effect::new(format!("uv pip install {}", packages.join(" ")), why)
        .writes(cwd.join(".venv"));
    exec::run(&uv.path, &args, Some(cwd), effect).await
}

/// `uv pip install -r requirements.txt`.
pub async fn pip_install_requirements(
    uv: &UvInfo,
    requirements: &Path,
    cwd: &Path,
) -> crate::Result<Output> {
    let req = requirements.to_string_lossy().into_owned();
    let effect = Effect::new(
        "uv pip install -r requirements.txt",
        "install requirements.txt into .venv",
    )
    .writes(cwd.join(".venv"));
    exec::run(&uv.path, &["pip", "install", "-r", &req], Some(cwd), effect).await
}

/// `uv pip freeze` — snapshot installed packages into requirements format.
pub async fn pip_freeze(uv: &UvInfo, cwd: &Path) -> crate::Result<Output> {
    command::run(&uv.path, &["pip", "freeze"], Some(cwd)).await
}

/// `uv pip freeze > requirements.txt`: replace `cwd/requirements.txt` with
/// the environment's exact pins. A failed write comes back as an `Err`.
pub async fn freeze_into_requirements(uv: &UvInfo, cwd: &Path) -> crate::Result<Output> {
    let path = cwd.join("requirements.txt");
    let effect = Effect::new(
        "uv pip freeze > requirements.txt",
        "replace requirements.txt with the exact versions now installed",
    )
    .writes(&path);
    let out = exec::run(&uv.path, &["pip", "freeze"], Some(cwd), effect).await?;
    if out.success() && !exec::is_dry_run() {
        std::fs::write(&path, &out.stdout)
            .map_err(|e| anyhow::anyhow!("writing requirements.txt: {e}"))?;
    }
    Ok(out)
}

/// `uv pip install --dry-run --system --python <X.Y> <pkgs...>` — fast
/// resolvability check against a real interpreter, without needing a venv.
///
/// `--system` matters: without it uv refuses with "No virtual environment
/// found" when no `.venv` exists yet, which is exactly the situation this
/// confirmation is most useful in (before committing to a full setup). It
/// still requires `version` to be an interpreter uv can actually find on the
/// machine — callers should only call this once the target is known to exist,
/// e.g. from [`crate::core::probe::Probes::interpreters`].
pub async fn dry_run_resolve(
    uv: &UvInfo,
    version: PyVersion,
    packages: &[String],
    cwd: &Path,
) -> crate::Result<Output> {
    command::run(&uv.path, &dry_run_args(version, packages), Some(cwd)).await
}

/// The argument vector for [`dry_run_resolve`], split out so a test can assert
/// on it without needing a uv binary present. `--python <version>` pins the
/// resolve to the caller-supplied target explicitly; `--system` only lifts the
/// "must be inside a venv" requirement, it does not select an interpreter on
/// its own, so this can never silently fall back to whatever Python is first
/// on `$PATH`.
fn dry_run_args(version: PyVersion, packages: &[String]) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "pip".into(),
        "install".into(),
        "--dry-run".into(),
        "--system".into(),
        "--python".into(),
        version.to_string(),
    ];
    args.extend(packages.iter().cloned());
    args
}

// --- helpers ----------------------------------------------------------------

fn clean_version(raw: &str) -> String {
    // "uv 0.4.20 (abc 2024-01-01)" -> "0.4.20"
    raw.trim()
        .strip_prefix("uv")
        .unwrap_or(raw)
        .split_whitespace()
        .next()
        .unwrap_or(raw)
        .to_string()
}

#[derive(Deserialize)]
struct GhRelease {
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

/// Resolve a release asset's download URL via the GitHub API (latest release).
async fn github_asset_url(repo: &str, asset_name: &str) -> crate::Result<String> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let client = http_client()?;
    let release: GhRelease = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    release
        .assets
        .into_iter()
        .find(|a| a.name == asset_name)
        .map(|a| a.browser_download_url)
        .ok_or_else(|| anyhow::anyhow!("no asset `{asset_name}` in {repo} latest release"))
}

async fn http_get_bytes(url: &str) -> crate::Result<Vec<u8>> {
    let client = http_client()?;
    let resp = client.get(url).send().await?.error_for_status()?;
    Ok(resp.bytes().await?.to_vec())
}

fn http_client() -> crate::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("pypilot/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(Into::into)
}

fn extract_tar_gz(bytes: &[u8], dest: &Path) -> crate::Result<()> {
    let decoder = flate2::read::GzDecoder::new(Cursor::new(bytes));
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(dest).context("unpacking uv tar.gz")?;
    Ok(())
}

fn extract_zip(bytes: &[u8], dest: &Path) -> crate::Result<()> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).context("opening uv zip")?;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let Some(rel) = file.enclosed_name() else {
            continue;
        };
        let out = dest.join(rel);
        if file.is_dir() {
            std::fs::create_dir_all(&out)?;
        } else {
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut writer = std::fs::File::create(&out)?;
            std::io::copy(&mut file, &mut writer)?;
        }
    }
    Ok(())
}

/// Find the `uv`/`uv.exe` binary anywhere under `dir` (archives may nest it).
fn locate_binary(dir: &Path) -> Option<PathBuf> {
    let target = if cfg!(windows) { "uv.exe" } else { "uv" };
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|n| n == target) {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(unix)]
fn make_executable(path: &Path) -> crate::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::platform::{Arch, Os};

    #[test]
    fn asset_names_per_platform() {
        assert_eq!(
            uv_asset_name(Platform {
                os: Os::Linux,
                arch: Arch::X86_64
            }),
            "uv-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            uv_asset_name(Platform {
                os: Os::Macos,
                arch: Arch::Aarch64
            }),
            "uv-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            uv_asset_name(Platform {
                os: Os::Windows,
                arch: Arch::X86_64
            }),
            "uv-x86_64-pc-windows-msvc.zip"
        );
    }

    #[test]
    fn init_is_bare_and_pins_the_chosen_python() {
        assert_eq!(
            init_args(Some(PyVersion::py3(12)), true),
            [
                "init",
                "--bare",
                "--no-workspace",
                "--pin-python",
                "--python",
                "3.12"
            ]
        );
        // Old uv without --bare, and no Python chosen yet.
        assert_eq!(init_args(None, false), ["init", "--no-workspace"]);
    }

    #[tokio::test]
    async fn create_venv_passes_clear() {
        // Regression guard. Without --clear, `uv venv` errors out on any project
        // that already has an environment, which breaks both `setup` on a second
        // run and every rebuild action.
        let uv = UvInfo {
            path: PathBuf::from("uv-does-not-exist-here"),
            version: "0.0.0".into(),
            managed: false,
        };
        // The spawn fails (no such binary), but the arg vector is what matters,
        // so assert on the command we would have run.
        let args = venv_args(PyVersion::py3(12), Path::new(".venv"));
        assert!(
            args.iter().any(|a| a == "--clear"),
            "uv venv must replace an existing environment, got {args:?}"
        );
        assert!(
            create_venv(&uv, PyVersion::py3(12), Path::new(".venv"), Path::new("."))
                .await
                .is_err()
        );
    }

    #[test]
    fn dry_run_pins_the_given_target_not_whatever_system_python_is() {
        // Regression guard for a real risk: if this ever dropped back to bare
        // `--system` with no `--python`, uv would resolve against the first
        // Python it finds on PATH, which is often the *wrong* one — the whole
        // reason PyPilot exists is that the system default often isn't the
        // interpreter the project's dependencies actually support.
        let system_looking_default = PyVersion::py3(13);
        let project_target = PyVersion::py3(9);
        assert_ne!(system_looking_default, project_target);

        let args = dry_run_args(project_target, &["mediapipe".to_string()]);
        let python_idx = args.iter().position(|a| a == "--python").unwrap();
        assert_eq!(args[python_idx + 1], project_target.to_string());
        assert_ne!(args[python_idx + 1], system_looking_default.to_string());
        assert!(args.iter().any(|a| a == "--system"));
    }

    #[test]
    fn dev_add_targets_the_dev_group() {
        // Regression guard: dropping `--dev` would put ipykernel into the
        // project's runtime dependencies.
        let args = add_dev_args(&["ipykernel".to_string()]);
        assert_eq!(args, vec!["add", "--dev", "ipykernel"]);
    }

    #[test]
    fn version_is_cleaned() {
        assert_eq!(clean_version("uv 0.4.20 (abcd 2024-09-01)"), "0.4.20");
        assert_eq!(clean_version("0.5.0"), "0.5.0");
    }

    #[tokio::test]
    async fn pip_mode_disables_uv() {
        let s = Settings {
            package_manager: PackageManager::Pip,
            ..Settings::default()
        };
        assert!(detect(&s).await.is_none());
        assert!(ensure(&s).await.is_err());
    }

    #[test]
    fn refresh_path_command_is_non_empty() {
        let cmd = refresh_path_command();
        assert!(!cmd.is_empty());
        if cfg!(windows) {
            assert!(cmd.contains("GetEnvironmentVariable"));
        } else {
            assert!(cmd.contains("PATH="));
        }
    }
}
