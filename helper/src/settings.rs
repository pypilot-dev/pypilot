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
//!
//! An editor can send settings too (LSP `initializationOptions` and
//! `workspace/didChangeConfiguration`, see [`set_editor_settings`]). Per key,
//! the precedence is: global file < editor < project file. The project file
//! wins because it is shared with everyone working on the project. An editor
//! value equal to the built-in default counts as unset, because VS Code sends
//! every setting, defaults included, and those must not mask the global file.
//! When the editor sends nothing, nothing changes.

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

/// Resolved operational mode for PyPilot operations. Never read from a
/// settings file; serialized (`"uv_full"`) only for `--json` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

/// Which files get import diagnostics.
///
/// - `openFiles` (default): the buffers open in the editor.
/// - `workspace`: also every `.py` file and notebook in the workspace, scanned
///   in the background and published so they show up in the Problems panel
///   (see `crate::lsp`'s workspace scan). Open buffers still win.
///
/// TOML takes `"openFiles"` or `"open-files"`, the editor `"openFiles"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum DiagnosticsScope {
    #[default]
    #[serde(alias = "open-files")]
    OpenFiles,
    Workspace,
}

/// Raw, all-optional shape as parsed from a TOML file. Merging is "last write
/// wins per present key", which is why every field is an `Option`.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
struct RawSettings {
    package_manager: Option<PackageManager>,
    notifications: Option<Notifications>,
    auto_check_on_open: Option<bool>,
    data_refresh_days: Option<u32>,
    pixi_environment: Option<String>,
    diagnostics_scope: Option<DiagnosticsScope>,
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
    /// Which pixi environment is the project's environment (`default`).
    /// The editor and `--pixi-env` can override it for one process; see
    /// [`crate::core::pixi_envs::selected_name`].
    pub pixi_environment: String,
    /// Which files get import diagnostics (`diagnostics_scope`).
    pub diagnostics_scope: DiagnosticsScope,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            package_manager: PackageManager::default(),
            notifications: Notifications::default(),
            auto_check_on_open: true,
            data_refresh_days: 7,
            dismissed: false,
            pixi_environment: crate::core::pixi::DEFAULT_ENV.to_string(),
            diagnostics_scope: DiagnosticsScope::default(),
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
        merge(&mut merged, editor_settings());
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
            pixi_environment: merged
                .pixi_environment
                .map(|e| e.trim().to_string())
                .filter(|e| !e.is_empty())
                .unwrap_or(d.pixi_environment),
            diagnostics_scope: merged.diagnostics_scope.unwrap_or(d.diagnostics_scope),
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

/// Settings the editor sent, as a layer between the global and the project
/// file (see the module docs). `None` until the editor sends any.
static EDITOR: std::sync::RwLock<Option<RawSettings>> = std::sync::RwLock::new(None);

fn editor_settings() -> Option<RawSettings> {
    EDITOR.read().ok().and_then(|e| e.clone())
}

/// Replace the editor's settings layer with what the client sent: its
/// `initializationOptions`, or the `settings` of a `didChangeConfiguration`.
/// Keys are camelCase (`packageManager`, `notifications`, `autoCheckOnOpen`,
/// `dataRefreshDays`, `pixiEnvironment`, `diagnosticsScope`), either at the top level or under
/// `pypilot`. Values that do not parse are ignored with a log line, never an
/// error; values equal to the built-in default are treated as unset.
pub fn set_editor_settings(value: &serde_json::Value) {
    let raw = parse_editor_settings(value);
    if let Ok(mut slot) = EDITOR.write() {
        *slot = Some(raw);
    }
}

fn parse_editor_settings(value: &serde_json::Value) -> RawSettings {
    let obj = value
        .get("pypilot")
        .filter(|v| v.is_object())
        .unwrap_or(value);
    let d = Settings::default();
    let mut raw = RawSettings::default();

    fn typed<T: serde::de::DeserializeOwned>(obj: &serde_json::Value, key: &str) -> Option<T> {
        let v = obj.get(key).filter(|v| !v.is_null())?;
        match serde_json::from_value::<T>(v.clone()) {
            Ok(t) => Some(t),
            Err(_) => {
                eprintln!("pypilot: ignoring editor setting {key} = {v}");
                None
            }
        }
    }

    raw.package_manager =
        typed::<PackageManager>(obj, "packageManager").filter(|pm| *pm != d.package_manager);
    raw.notifications =
        typed::<Notifications>(obj, "notifications").filter(|n| *n != d.notifications);
    raw.auto_check_on_open =
        typed::<bool>(obj, "autoCheckOnOpen").filter(|b| *b != d.auto_check_on_open);
    raw.data_refresh_days =
        typed::<u32>(obj, "dataRefreshDays").filter(|n| *n != d.data_refresh_days);
    raw.pixi_environment = typed::<String>(obj, "pixiEnvironment")
        .map(|e| e.trim().to_string())
        .filter(|e| !e.is_empty() && *e != d.pixi_environment)
        .filter(|e| {
            let ok = is_pixi_env_name(e);
            if !ok {
                eprintln!("pypilot: ignoring editor setting pixiEnvironment = {e:?}");
            }
            ok
        });
    raw.diagnostics_scope =
        typed::<DiagnosticsScope>(obj, "diagnosticsScope").filter(|s| *s != d.diagnostics_scope);
    raw
}

/// pixi environment names are lowercase letters, digits and dashes.
pub fn is_pixi_env_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
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
    if o.pixi_environment.is_some() {
        base.pixi_environment = o.pixi_environment;
    }
    if o.diagnostics_scope.is_some() {
        base.diagnostics_scope = o.diagnostics_scope;
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
        assert_eq!(s.pixi_environment, "default");
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
            // What VS Code sends as `pypilot.packageManager`.
            let editor = parse_editor_settings(&serde_json::json!({ "packageManager": text }));
            let expected = (pm != PackageManager::Auto).then_some(pm);
            assert_eq!(editor.package_manager, expected, "{text}");
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
    fn editor_settings_are_parsed_tolerantly() {
        // The shape VS Code sends in didChangeConfiguration.
        let raw = parse_editor_settings(&serde_json::json!({
            "pypilot": {
                "packageManager": "uv",
                "notifications": "all",
                "autoCheckOnOpen": false,
                "autoSelectVenv": true,
                "pixiEnvironment": "test",
                "dataRefreshDays": 0
            }
        }));
        assert_eq!(raw.package_manager, Some(PackageManager::Uv));
        assert_eq!(raw.notifications, Some(Notifications::All));
        assert_eq!(raw.auto_check_on_open, Some(false));
        assert_eq!(raw.data_refresh_days, Some(0));
        assert_eq!(raw.pixi_environment.as_deref(), Some("test"));

        // initializationOptions: flat. Defaults count as unset, bad values are
        // dropped (an unknown package manager is not an error).
        let raw = parse_editor_settings(&serde_json::json!({
            "pixiEnvironment": "Bad Name!",
            "packageManager": "conda-someday",
            "notifications": "problems-only",
            "autoCheckOnOpen": "yes",
            "dataRefreshDays": 7
        }));
        assert_eq!(raw.pixi_environment, None);
        assert_eq!(raw.package_manager, None);
        assert_eq!(raw.notifications, None);
        assert_eq!(raw.auto_check_on_open, None);
        assert_eq!(raw.data_refresh_days, None);
        assert_eq!(
            parse_editor_settings(&serde_json::json!({"pixiEnvironment": "default"}))
                .pixi_environment,
            None
        );
        assert_eq!(
            parse_editor_settings(&serde_json::Value::Null).package_manager,
            None
        );
        assert!(is_pixi_env_name("py-312"));
        assert!(!is_pixi_env_name("Py312") && !is_pixi_env_name("a b") && !is_pixi_env_name(""));
    }

    #[test]
    fn project_file_beats_editor_beats_global() {
        let mut base = RawSettings {
            package_manager: Some(PackageManager::Pip),
            data_refresh_days: Some(3),
            ..Default::default()
        };
        let editor = parse_editor_settings(&serde_json::json!({
            "packageManager": "uv", "dataRefreshDays": 1, "notifications": "off"
        }));
        merge(&mut base, Some(editor));
        merge(
            &mut base,
            Some(RawSettings {
                data_refresh_days: Some(9),
                ..Default::default()
            }),
        );
        assert_eq!(base.package_manager, Some(PackageManager::Uv));
        assert_eq!(base.notifications, Some(Notifications::Off));
        assert_eq!(base.data_refresh_days, Some(9));
    }

    #[test]
    fn pixi_environment_comes_from_the_project_file() {
        let dir = std::env::temp_dir().join(format!("pypilot-pixi-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pypilot.toml"), "pixi_environment = \" test \"\n").unwrap();
        assert_eq!(Settings::load(&dir).pixi_environment, "test");
        // Blank means unset.
        std::fs::write(dir.join("pypilot.toml"), "pixi_environment = \"\"\n").unwrap();
        assert_eq!(Settings::load(&dir).pixi_environment, "default");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diagnostics_scope_from_editor_and_toml() {
        // didChangeConfiguration (nested) and initializationOptions (flat).
        let nested = serde_json::json!({ "pypilot": { "diagnosticsScope": "workspace" } });
        assert_eq!(
            parse_editor_settings(&nested).diagnostics_scope,
            Some(DiagnosticsScope::Workspace)
        );
        let flat = serde_json::json!({ "diagnosticsScope": "workspace" });
        assert_eq!(
            parse_editor_settings(&flat).diagnostics_scope,
            Some(DiagnosticsScope::Workspace)
        );
        // The default counts as unset; anything unknown is dropped.
        for v in ["openFiles", "everything", "Workspace"] {
            let raw = parse_editor_settings(&serde_json::json!({ "diagnosticsScope": v }));
            assert_eq!(raw.diagnostics_scope, None, "{v}");
        }

        for (text, expected) in [
            (
                "diagnostics_scope = \"workspace\"
",
                DiagnosticsScope::Workspace,
            ),
            (
                "diagnostics_scope = \"openFiles\"
",
                DiagnosticsScope::OpenFiles,
            ),
            (
                "diagnostics_scope = \"open-files\"
",
                DiagnosticsScope::OpenFiles,
            ),
        ] {
            let raw: RawSettings = toml::from_str(text).unwrap();
            assert_eq!(raw.diagnostics_scope, Some(expected), "{text}");
        }
        assert_eq!(
            Settings::default().diagnostics_scope,
            DiagnosticsScope::OpenFiles
        );
    }

    #[test]
    fn diagnostics_scope_precedence_is_global_editor_project() {
        let toml_scope = |s: &str| -> RawSettings {
            toml::from_str(&format!(
                "diagnostics_scope = \"{s}\"
"
            ))
            .unwrap()
        };
        let editor = |s: &str| parse_editor_settings(&serde_json::json!({ "diagnosticsScope": s }));

        // Editor beats the global file.
        let mut base = toml_scope("openFiles");
        merge(&mut base, Some(editor("workspace")));
        assert_eq!(base.diagnostics_scope, Some(DiagnosticsScope::Workspace));

        // The project file beats the editor.
        merge(&mut base, Some(toml_scope("openFiles")));
        assert_eq!(base.diagnostics_scope, Some(DiagnosticsScope::OpenFiles));

        // An editor sending the default does not mask a global "workspace".
        let mut base = toml_scope("workspace");
        merge(&mut base, Some(editor("openFiles")));
        assert_eq!(base.diagnostics_scope, Some(DiagnosticsScope::Workspace));

        // And through `Settings::load`, from the project file.
        let dir = std::env::temp_dir().join(format!("pypilot-diag-scope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("pypilot.toml"),
            "diagnostics_scope = \"workspace\"
",
        )
        .unwrap();
        assert_eq!(
            Settings::load(&dir).diagnostics_scope,
            DiagnosticsScope::Workspace
        );
        let _ = std::fs::remove_dir_all(&dir);
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
