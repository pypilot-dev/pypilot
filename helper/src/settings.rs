//! F7 — user settings.
//!
//! A global config file (in the platform config dir) provides defaults; a
//! per-project file overrides individual keys. `package_manager` is respected
//! everywhere in the codebase — the uv driver refuses to run in pip mode, and the
//! setup flow branches on it.
//!
//! Per-project files are looked up, in order:
//!   1. `<workspace>/.zed/pypilot.toml`
//!   2. `<workspace>/pypilot.toml`
//!
//! The same per-project file also stores F5 dismissal state (`[state] never = true`)
//! so "Never for this project" is remembered.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Which package manager drives installs.
///
/// - `auto` (default): Checks whether uv is installed on the system PATH. If so, uses full uv project workflow.
///   If not, falls back to the standard pip ecosystem rather than silently using a managed uv binary.
/// - `uv`: Full uv project mode (pyproject.toml + uv.lock).
/// - `uv-pip`: uv-accelerated pip-compatible mode (requirements.txt only, using uv venv and uv pip install).
/// - `pip`: Standard python -m venv + pip.
/// - `pixi`: manage the project with pixi. A pixi project is pixi's whatever
///   this says; on a project without a pixi manifest, setup first writes a
///   `pixi.toml` from its dependencies ([`crate::core::pixi_migrate`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[derive(Default)]
pub enum PackageManager {
    #[default]
    Auto,
    Uv,
    #[serde(rename = "uv-pip")]
    UvPip,
    Pip,
    Pixi,
}

/// Resolved operational mode for PyPilot operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResolvedMode {
    /// Full uv project mode (pyproject.toml + uv.lock + uv init/add/sync).
    UvFull,
    /// uv-accelerated pip-compatible mode (requirements.txt only, uv venv + uv pip install + uv pip freeze).
    UvPipCompat,
    /// Pure pip ecosystem (python -m venv + pip install + pip freeze).
    PurePip,
    /// A pixi project, managed with pixi alone (`pixi install`, `pixi add`).
    ///
    /// Chosen whenever the project has a pixi manifest, whatever
    /// `package_manager` says, and for `package_manager = "pixi"` on a project
    /// without one (setup then creates it): that setting picks how PyPilot builds an
    /// environment it owns, and a pixi project's environment is pixi's. A
    /// `.venv`, `uv.lock` or `[tool.uv]` beside it would be a second,
    /// competing environment (see [`crate::core::pixi_manage`]).
    Pixi,
}

/// F5 toast verbosity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[derive(Default)]
pub enum Notifications {
    All,
    #[default]
    ProblemsOnly,
    Off,
}

/// Raw, all-optional shape as parsed from a TOML file. Merging is "last write
/// wins per present key", which is why every field is an `Option`.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
struct RawSettings {
    package_manager: Option<PackageManager>,
    notifications: Option<Notifications>,
    auto_check_on_open: Option<bool>,
    data_refresh_days: Option<u32>,
    #[serde(default)]
    state: RawState,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
struct RawState {
    /// "Never for this project" — F5 dismissal.
    never: Option<bool>,
}

/// Fully-resolved settings after merging global defaults with a project override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub package_manager: PackageManager,
    pub notifications: Notifications,
    pub auto_check_on_open: bool,
    /// TTL in days for bundled matrix/mapping refresh; `0` = fully offline.
    pub data_refresh_days: u32,
    /// True when the user chose "Never for this project".
    pub dismissed: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            package_manager: PackageManager::default(),
            notifications: Notifications::default(),
            auto_check_on_open: true,
            data_refresh_days: 7,
            dismissed: false,
        }
    }
}

impl Settings {
    /// Load global settings, then apply a per-project override if present.
    pub fn load(workspace: &Path) -> Settings {
        let mut merged = RawSettings::default();

        if let Some(global) = global_config_path() {
            merge(&mut merged, read_raw(&global));
        }
        if let Some(project) = project_config_path(workspace) {
            merge(&mut merged, read_raw(&project));
        }

        let d = Settings::default();
        Settings {
            package_manager: merged.package_manager.unwrap_or(d.package_manager),
            notifications: merged.notifications.unwrap_or(d.notifications),
            auto_check_on_open: merged.auto_check_on_open.unwrap_or(d.auto_check_on_open),
            data_refresh_days: merged.data_refresh_days.unwrap_or(d.data_refresh_days),
            dismissed: merged.state.never.unwrap_or(false),
        }
    }

    /// Persist "Never for this project" into `<workspace>/.zed/pypilot.toml`,
    /// preserving any settings already there.
    pub fn mark_dismissed(workspace: &Path) -> crate::Result<()> {
        let dir = workspace.join(".zed");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("pypilot.toml");

        let mut raw = read_raw(&path).unwrap_or_default();
        raw.state.never = Some(true);
        let text = toml::to_string_pretty(&raw)?;
        std::fs::write(&path, text)?;
        Ok(())
    }

    /// Resolve the operational mode for the project at `workspace`.
    pub async fn resolve_mode(&self, workspace: &Path) -> ResolvedMode {
        resolve_mode(self, workspace).await
    }

    /// Persist the chosen package manager into `<workspace>/.zed/pypilot.toml`,
    /// preserving any settings already there.
    pub fn persist_package_manager(workspace: &Path, pm: PackageManager) -> crate::Result<()> {
        let dir = workspace.join(".zed");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("pypilot.toml");

        let mut raw = read_raw(&path).unwrap_or_default();
        raw.package_manager = Some(pm);
        let text = toml::to_string_pretty(&raw)?;
        std::fs::write(&path, text)?;
        Ok(())
    }
}

/// Resolve the operational mode from settings, checking system uv for Auto
/// mode. A pixi project is always [`ResolvedMode::Pixi`].
pub async fn resolve_mode(settings: &Settings, workspace: &Path) -> ResolvedMode {
    if crate::core::pixi::detect(workspace).is_some() {
        return ResolvedMode::Pixi;
    }
    match settings.package_manager {
        PackageManager::Uv => ResolvedMode::UvFull,
        PackageManager::UvPip => ResolvedMode::UvPipCompat,
        PackageManager::Pip => ResolvedMode::PurePip,
        PackageManager::Pixi => ResolvedMode::Pixi,
        PackageManager::Auto => {
            if crate::core::uv::is_system_installed().await.is_some() {
                ResolvedMode::UvFull
            } else {
                ResolvedMode::PurePip
            }
        }
    }
}

/// Path to the global config file, e.g. `~/.config/pypilot/config.toml`.
pub fn global_config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("dev", "PyPilot", "pypilot")
        .map(|d| d.config_dir().join("config.toml"))
}

/// Per-project config path, preferring `.zed/pypilot.toml`.
pub fn project_config_path(workspace: &Path) -> Option<PathBuf> {
    let zed = workspace.join(".zed").join("pypilot.toml");
    if zed.exists() {
        return Some(zed);
    }
    let root = workspace.join("pypilot.toml");
    if root.exists() {
        return Some(root);
    }
    // Return the preferred location even if absent, so callers can describe where
    // config *would* live; `read_raw` tolerates a missing file.
    Some(zed)
}

fn read_raw(path: &Path) -> Option<RawSettings> {
    let text = std::fs::read_to_string(path).ok()?;
    match toml::from_str::<RawSettings>(&text) {
        Ok(raw) => Some(raw),
        Err(e) => {
            eprintln!("pypilot: ignoring malformed config {}: {e}", path.display());
            None
        }
    }
}

/// Overlay `overlay` onto `base`, key by key (present keys win).
fn merge(base: &mut RawSettings, overlay: Option<RawSettings>) {
    let Some(o) = overlay else { return };
    if o.package_manager.is_some() {
        base.package_manager = o.package_manager;
    }
    if o.notifications.is_some() {
        base.notifications = o.notifications;
    }
    if o.auto_check_on_open.is_some() {
        base.auto_check_on_open = o.auto_check_on_open;
    }
    if o.data_refresh_days.is_some() {
        base.data_refresh_days = o.data_refresh_days;
    }
    if o.state.never.is_some() {
        base.state.never = o.state.never;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_auto() {
        let s = Settings::default();
        assert_eq!(s.package_manager, PackageManager::Auto);
        assert_eq!(s.notifications, Notifications::ProblemsOnly);
        assert!(!s.dismissed);
    }

    #[tokio::test]
    async fn resolve_mode_explicit_pip() {
        let s = Settings {
            package_manager: PackageManager::Pip,
            ..Settings::default()
        };
        assert_eq!(s.resolve_mode(Path::new(".")).await, ResolvedMode::PurePip);
    }

    #[tokio::test]
    async fn resolve_mode_explicit_uv() {
        let s = Settings {
            package_manager: PackageManager::Uv,
            ..Settings::default()
        };
        assert_eq!(s.resolve_mode(Path::new(".")).await, ResolvedMode::UvFull);
    }

    #[tokio::test]
    async fn resolve_mode_explicit_uv_pip() {
        let s = Settings {
            package_manager: PackageManager::UvPip,
            ..Settings::default()
        };
        assert_eq!(
            s.resolve_mode(Path::new(".")).await,
            ResolvedMode::UvPipCompat
        );
    }

    #[tokio::test]
    async fn a_pixi_project_is_always_pixi_mode() {
        let dir = std::env::temp_dir().join(format!("pypilot-mode-pixi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pixi.toml"), "[workspace]\nplatforms = []\n").unwrap();
        for pm in [
            PackageManager::Auto,
            PackageManager::Uv,
            PackageManager::UvPip,
            PackageManager::Pip,
        ] {
            let s = Settings {
                package_manager: pm,
                ..Settings::default()
            };
            assert_eq!(s.resolve_mode(&dir).await, ResolvedMode::Pixi, "{pm:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn explicit_pixi_means_pixi_mode_without_a_manifest() {
        let s = Settings {
            package_manager: PackageManager::Pixi,
            ..Settings::default()
        };
        assert_eq!(s.resolve_mode(Path::new(".")).await, ResolvedMode::Pixi);
    }

    #[test]
    fn package_manager_values_parse() {
        for (text, pm) in [
            ("auto", PackageManager::Auto),
            ("uv", PackageManager::Uv),
            ("uv-pip", PackageManager::UvPip),
            ("pip", PackageManager::Pip),
            ("pixi", PackageManager::Pixi),
        ] {
            let raw: RawSettings =
                toml::from_str(&format!("package_manager = \"{text}\"")).unwrap();
            assert_eq!(raw.package_manager, Some(pm), "{text}");
            let json: PackageManager = serde_json::from_str(&format!("\"{text}\"")).unwrap();
            assert_eq!(json, pm);
        }
    }

    #[test]
    fn project_override_wins_over_global() {
        let mut base = RawSettings {
            package_manager: Some(PackageManager::Uv),
            ..Default::default()
        };
        merge(
            &mut base,
            Some(RawSettings {
                package_manager: Some(PackageManager::Pip),
                ..Default::default()
            }),
        );
        assert_eq!(base.package_manager, Some(PackageManager::Pip));
    }

    #[test]
    fn absent_overlay_keys_do_not_clobber() {
        let mut base = RawSettings {
            package_manager: Some(PackageManager::Pip),
            notifications: Some(Notifications::All),
            ..Default::default()
        };
        merge(&mut base, Some(RawSettings::default()));
        assert_eq!(base.package_manager, Some(PackageManager::Pip));
        assert_eq!(base.notifications, Some(Notifications::All));
    }
}
