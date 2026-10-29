//! F2 — hardware / ML compatibility.
//!
//! For pip-installed frameworks the NVIDIA *driver* is what matters, not the
//! system CUDA toolkit: wheels bundle their own CUDA runtime, and drivers are
//! backwards compatible. The broken case is a driver too old for the wheel's
//! runtime; a system CUDA "ahead" of the framework is not a problem.
//!
//! [`nvidia`] holds the driver -> max-CUDA lookup, [`frameworks`] holds which
//! CUDA build variants a torch/tensorflow release ships, and [`solve`] ties
//! them to the machine's actual GPU (via [`crate::core::gpu`]) into a finding
//! plus, for torch, the index URL an install should use. [`check`] is the
//! read-only entry point the solver already calls into every [`crate::core::Assessment`];
//! [`solve::solve_framework`] is also called directly by the install path so
//! a real `torch` install pulls the build matched to the driver.
//!
//! [`cuda_ext`] covers the CUDA extension packages (flash-attn, xformers,
//! bitsandbytes, ...) whose install trouble is platform wheels and torch
//! coupling rather than the driver.
//!
//! The bundled datasets (`data/nvidia.json`, `data/frameworks.json`,
//! `data/import_map.json`, `data/cuda_extensions.json`,
//! `data/conda_forge_packages.json`) are embedded at compile time so an offline machine
//! always has a correct snapshot; [`refresh`] overlays newer copies fetched
//! from the project repo, TTL-gated by F7's `data_refresh_days`, and falls
//! back to the bundled snapshot silently on any failure.

pub mod arch;
pub mod conda_forge;
pub mod cuda_ext;
pub mod frameworks;
pub mod nvidia;
pub mod refresh;
pub mod solve;
pub mod status;

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

use serde::Deserialize;

use crate::core::gpu::{self, Accelerator};
use crate::core::platform::{Os, Platform};
use crate::core::project::{normalize_name, Requirement};
use crate::core::Finding;
use crate::settings::Settings;
use cuda_ext::ExtensionTable;
use frameworks::{Framework, FrameworkTable};

/// F7's documented default TTL, used where a caller has no `Settings` in
/// scope to read the user's configured value from (see [`import_map`]).
const DEFAULT_TTL_DAYS: u32 = 7;

/// Bundled snapshots, embedded so they ship inside the binary.
pub const NVIDIA_JSON: &str = include_str!("../../data/nvidia.json");
pub const FRAMEWORKS_JSON: &str = include_str!("../../data/frameworks.json");
pub const IMPORT_MAP_JSON: &str = include_str!("../../data/import_map.json");
pub const CUDA_EXTENSIONS_JSON: &str = include_str!("../../data/cuda_extensions.json");
pub const CONDA_FORGE_JSON: &str = include_str!("../../data/conda_forge_packages.json");

/// GPU / driver facts, for the report.
#[derive(Debug, Clone, Default)]
pub struct GpuInfo {
    pub name: Option<String>,
    pub driver_version: Option<String>,
    /// e.g. "12.0"; `None` when the driver is too old to report it.
    pub compute_cap: Option<String>,
    pub apple_silicon: bool,
}

/// The hardware side of an assessment.
#[derive(Debug, Clone, Default)]
pub struct HardwareReport {
    pub gpu: Option<GpuInfo>,
    pub findings: Vec<Finding>,
    /// Normalized names of CUDA extensions whose missing wheel a finding above
    /// already explains, so the assessment skips the generic source-build one.
    pub explained_source_builds: Vec<String>,
}

/// Run the hardware/ML compatibility check over a project's declared
/// dependencies. Read-only from the caller's point of view: the GPU probe has
/// its own disk cache, and any data refresh happens in the background without
/// this call waiting on it.
///
/// Silent by construction for everything except torch/tensorflow and the
/// CUDA extensions: packages this module does not recognize simply produce no
/// findings, matching the "stay silent" contract rather than emitting a
/// Info-per-package no-op.
pub async fn check(
    workspace: &Path,
    settings: &Settings,
    packages: &[Requirement],
) -> HardwareReport {
    let relevant: Vec<&Requirement> = packages
        .iter()
        .filter(|r| Framework::from_package_name(&r.name).is_some())
        .collect();
    let extensions = cuda_extension_table(settings.data_refresh_days);
    let has_extensions = packages.iter().any(|r| extensions.get(&r.name).is_some());

    if relevant.is_empty() && !has_extensions {
        return HardwareReport::default();
    }

    // Best-effort, non-blocking: refreshes the on-disk cache for the *next*
    // scan without making this one wait on a network round trip. A stale
    // cache always falls back to the bundled snapshot, never to an error.
    tokio::spawn(refresh::refresh_if_stale(settings.clone()));

    // Platform and pins only, no GPU: a missing wheel or a torch mismatch is
    // a problem whatever card is installed.
    let issues = if has_extensions {
        let frameworks = refresh::read(
            "frameworks.json",
            FRAMEWORKS_JSON,
            settings.data_refresh_days,
        );
        let torch_versions = FrameworkTable::load(&frameworks)
            .map(|t| t.all_versions(Framework::Torch))
            .unwrap_or_default();
        cuda_ext::assess_project(&extensions, packages, Platform::current(), &torch_versions)
    } else {
        Vec::new()
    };
    let mut findings: Vec<Finding> = issues.iter().map(cuda_ext::Issue::to_finding).collect();
    let explained_source_builds: Vec<String> = issues
        .iter()
        .filter(|i| i.no_wheel)
        .map(|i| normalize_name(&i.package))
        .collect();

    if relevant.is_empty() {
        return HardwareReport {
            gpu: None,
            findings,
            explained_source_builds,
        };
    }

    let accelerator = gpu::detect().await;
    let gpu_info = Some(match &accelerator {
        Accelerator::Nvidia {
            name,
            driver_version,
            compute_cap,
        } => GpuInfo {
            name: Some(name.clone()),
            driver_version: Some(driver_version.clone()),
            compute_cap: compute_cap.clone(),
            apple_silicon: false,
        },
        Accelerator::AppleSilicon => GpuInfo {
            apple_silicon: true,
            ..Default::default()
        },
        Accelerator::None => GpuInfo::default(),
    });

    for req in relevant {
        if let Some(solved) = solve::solve_framework(&req.name, Some(req), settings).await {
            // A torch already in the venv can be the wrong build for this
            // GPU even when the declared requirement solves cleanly, e.g. a
            // cu121 wheel carried over from before an RTX 50-series upgrade.
            if Framework::from_package_name(&req.name) == Some(Framework::Torch) {
                if let Some(env) = crate::core::env::project_env(workspace) {
                    findings.extend(solve::check_installed_torch(&env, &solved, settings).await);
                }
            }
            findings.extend(solved.finding);
        }
    }

    HardwareReport {
        gpu: gpu_info,
        findings,
        explained_source_builds,
    }
}

#[derive(Deserialize)]
struct ImportMapFile {
    map: HashMap<String, String>,
}

/// Import name to PyPI package name, parsed once on first use.
///
/// Cached for the process lifetime, which is fine for a short-lived CLI
/// invocation or a single LSP session: the disk-cache check inside
/// [`refresh::read`] already picks up whatever the last background refresh
/// wrote, so a freshly started process always sees the newest cached data.
fn import_map() -> &'static HashMap<String, String> {
    static MAP: OnceLock<HashMap<String, String>> = OnceLock::new();
    MAP.get_or_init(|| {
        let json = refresh::read("import_map.json", IMPORT_MAP_JSON, DEFAULT_TTL_DAYS);
        serde_json::from_str::<ImportMapFile>(&json)
            .map(|f| f.map)
            .unwrap_or_default()
    })
}

/// The CUDA extension table for the assessment, read fresh so a configured
/// `data_refresh_days` is honoured. A broken refreshed copy cannot reach here
/// (refresh validates JSON), and a bundled one that fails to parse is caught
/// by the tests, so the empty fallback only guards a schema change.
fn cuda_extension_table(ttl_days: u32) -> ExtensionTable {
    let json = refresh::read("cuda_extensions.json", CUDA_EXTENSIONS_JSON, ttl_days);
    ExtensionTable::load(&json)
        .or_else(|| ExtensionTable::load(CUDA_EXTENSIONS_JSON))
        .unwrap_or_else(ExtensionTable::empty)
}

/// The CUDA extension table for the import guardian, parsed once per process
/// like [`import_map`].
pub fn cuda_extensions() -> &'static ExtensionTable {
    static TABLE: OnceLock<ExtensionTable> = OnceLock::new();
    TABLE.get_or_init(|| cuda_extension_table(DEFAULT_TTL_DAYS))
}

/// The conda-forge table for the pixi advisory
/// ([`crate::core::pixi_advice`]), parsed once per process like
/// [`import_map`]. A refreshed copy that no longer parses falls back to the
/// bundled one.
pub fn conda_forge() -> &'static conda_forge::CondaForgeTable {
    static TABLE: OnceLock<conda_forge::CondaForgeTable> = OnceLock::new();
    TABLE.get_or_init(|| {
        let json = refresh::read(
            "conda_forge_packages.json",
            CONDA_FORGE_JSON,
            DEFAULT_TTL_DAYS,
        );
        conda_forge::CondaForgeTable::load(&json)
            .filter(|t| !t.is_empty())
            .or_else(|| conda_forge::CondaForgeTable::load(CONDA_FORGE_JSON))
            .unwrap_or_else(conda_forge::CondaForgeTable::empty)
    })
}

/// Resolve an import name to the PyPI package that provides it.
///
/// Unlisted names resolve to themselves, which is usually right and occasionally
/// a typo. Callers must show the resolved name in any install prompt and never
/// act on it silently: that display step is the whole typosquatting defence.
pub fn resolve_package(import_name: &str) -> String {
    import_map()
        .get(import_name)
        .cloned()
        .unwrap_or_else(|| import_name.to_string())
}

/// True when the name came from the bundled table rather than falling through.
pub fn is_mapped(import_name: &str) -> bool {
    import_map().contains_key(import_name)
}

/// A short note for an offer to install `package` on `os`, when the install
/// would not give what people install it for. Today that is tensorflow on
/// native Windows, which has had no GPU build since 2.10: an "Install
/// tensorflow" quick fix must not read as a GPU setup there.
pub fn install_caveat(package: &str, os: Os) -> Option<&'static str> {
    match Framework::from_package_name(package) {
        Some(Framework::TensorFlow) if os == Os::Windows => Some("CPU-only on native Windows"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_datasets_are_valid_json() {
        // A malformed bundled dataset would be a build-time footgun for F2.
        serde_json::from_str::<serde_json::Value>(NVIDIA_JSON).unwrap();
        serde_json::from_str::<serde_json::Value>(FRAMEWORKS_JSON).unwrap();
        serde_json::from_str::<serde_json::Value>(IMPORT_MAP_JSON).unwrap();
        ExtensionTable::load(CUDA_EXTENSIONS_JSON)
            .expect("cuda_extensions.json matches its schema");
        conda_forge::CondaForgeTable::load(CONDA_FORGE_JSON)
            .expect("conda_forge_packages.json matches its schema");
    }

    #[test]
    fn maps_the_classic_mismatches() {
        assert_eq!(resolve_package("cv2"), "opencv-python");
        assert_eq!(resolve_package("PIL"), "pillow");
        assert_eq!(resolve_package("sklearn"), "scikit-learn");
        assert_eq!(resolve_package("yaml"), "PyYAML");
    }

    #[test]
    fn unmapped_names_resolve_to_themselves() {
        assert_eq!(resolve_package("mediapipe"), "mediapipe");
        assert_eq!(resolve_package("requests"), "requests");
        assert!(!is_mapped("mediapipe"));
        assert!(is_mapped("cv2"));
    }

    #[test]
    fn only_tensorflow_on_windows_carries_an_install_caveat() {
        assert!(install_caveat("tensorflow", Os::Windows).is_some());
        assert!(install_caveat("TensorFlow", Os::Windows).is_some());
        assert!(install_caveat("tensorflow", Os::Linux).is_none());
        assert!(install_caveat("tensorflow", Os::Macos).is_none());
        assert!(install_caveat("torch", Os::Windows).is_none());
        assert!(install_caveat("numpy", Os::Windows).is_none());
    }

    #[test]
    fn map_is_not_empty() {
        // A parse failure would silently degrade every lookup to identity.
        assert!(import_map().len() > 20);
    }

    #[tokio::test]
    async fn irrelevant_dependencies_produce_no_findings() {
        let packages = vec![Requirement::any("numpy"), Requirement::any("requests")];
        let report = check(Path::new("."), &Settings::default(), &packages).await;
        assert!(report.findings.is_empty());
        assert!(
            report.gpu.is_none(),
            "GPU is not even probed when nothing needs it"
        );
    }

    #[tokio::test]
    async fn cuda_extensions_reach_the_assessment_without_a_gpu_probe() {
        // flash-attn has no binary wheel anywhere, so every OS gets a finding.
        let settings = Settings {
            data_refresh_days: 0,
            ..Settings::default()
        };
        let packages = vec![Requirement::any("flash-attn"), Requirement::any("numpy")];
        let report = check(Path::new("."), &settings, &packages).await;
        assert_eq!(report.findings.len(), 1);
        assert!(report.findings[0].title.starts_with("flash-attn"));
        assert_eq!(report.explained_source_builds, vec!["flash-attn"]);
        assert!(
            report.gpu.is_none(),
            "wheel availability needs no GPU probe"
        );
    }
}
