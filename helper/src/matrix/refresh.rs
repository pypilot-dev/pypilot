//! Runtime refresh of the bundled datasets, per F7's `data_refresh_days`.
//!
//! The bundled `include_str!` snapshots are the permanent, always-correct
//! fallback: nothing here can make an offline machine wrong, only stale.
//! Every reader in this module reads its dataset through [`read`], which is a
//! synchronous disk-cache check with no network involved — the network side
//! only runs as an explicit refresh (`pypilot update-data`, or an opportunistic
//! background fetch after a scan), and on any failure it simply leaves the
//! cache as it was, so the next `read` falls back to the bundled copy exactly
//! as if refresh had never been attempted.

use std::path::PathBuf;
use std::time::SystemTime;

use crate::core::platform;
use crate::settings::Settings;

const BASE_URL: &str = "https://raw.githubusercontent.com/pypilot-dev/pypilot/main/helper/data";

/// The datasets, paired with the bundled fallback and the refresh URL.
pub const DATASETS: &[(&str, &str)] = &[
    ("nvidia.json", crate::matrix::NVIDIA_JSON),
    ("frameworks.json", crate::matrix::FRAMEWORKS_JSON),
    ("import_map.json", crate::matrix::IMPORT_MAP_JSON),
    ("cuda_extensions.json", crate::matrix::CUDA_EXTENSIONS_JSON),
    ("conda_forge_packages.json", crate::matrix::CONDA_FORGE_JSON),
];

fn cache_path(name: &str) -> PathBuf {
    platform::cache_dir().join("data").join(name)
}

/// The content to use for `name` right now: a fresh cached refresh if one
/// exists within `ttl_days`, otherwise the bundled snapshot. Pure disk I/O,
/// no network — safe to call from any hot path.
pub fn read(name: &str, bundled: &'static str, ttl_days: u32) -> String {
    if ttl_days == 0 {
        return bundled.to_string(); // `0` means fully offline, per F7.
    }
    match read_cache(name, ttl_days) {
        Some(text) => text,
        None => bundled.to_string(),
    }
}

fn read_cache(name: &str, ttl_days: u32) -> Option<String> {
    let path = cache_path(name);
    let meta = std::fs::metadata(&path).ok()?;
    let modified = meta.modified().ok()?;
    let age = SystemTime::now().duration_since(modified).ok()?;
    if age.as_secs() > u64::from(ttl_days) * 24 * 60 * 60 {
        return None;
    }
    let text = std::fs::read_to_string(&path).ok()?;
    // A cache entry that no longer parses as JSON must not shadow the bundled
    // snapshot; treat it as absent rather than serving broken data.
    serde_json::from_str::<serde_json::Value>(&text).ok()?;
    Some(text)
}

/// Fetch every dataset and refresh the disk cache, ignoring the TTL. Used by
/// `pypilot update-data`. Each dataset fails independently; one bad fetch
/// never blocks the others or disturbs an already-good cache entry.
pub async fn refresh_all() -> Vec<(&'static str, Result<(), String>)> {
    let client = client();
    let mut results = Vec::new();
    for (name, _bundled) in DATASETS {
        let outcome = refresh_one(&client, name).await;
        results.push((*name, outcome));
    }
    results
}

/// Opportunistic background refresh: silent, best-effort, never awaited by a
/// caller that needs its result now. Skips entirely when `0` (fully offline).
/// Takes an owned `Settings` so it can run inside `tokio::spawn`, which
/// requires the future to be `'static`.
pub async fn refresh_if_stale(settings: Settings) {
    if settings.data_refresh_days == 0 {
        return;
    }
    let client = client();
    for (name, _bundled) in DATASETS {
        if read_cache(name, settings.data_refresh_days).is_some() {
            continue; // Already fresh; do not fetch on every scan.
        }
        let _ = refresh_one(&client, name).await;
    }
}

async fn refresh_one(client: &reqwest::Client, name: &str) -> Result<(), String> {
    let url = format!("{BASE_URL}/{name}");
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    let text = resp.text().await.map_err(|e| e.to_string())?;

    // Never let a corrupt or truncated download poison the cache.
    serde_json::from_str::<serde_json::Value>(&text).map_err(|e| format!("invalid JSON: {e}"))?;

    let path = cache_path(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, &text).map_err(|e| e.to_string())?;
    Ok(())
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("pypilot/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("reqwest client with rustls should build")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_settings(ttl: u32) -> Settings {
        Settings {
            data_refresh_days: ttl,
            ..Settings::default()
        }
    }

    #[test]
    fn zero_ttl_never_touches_the_cache() {
        // `0` means fully offline: bundled content, no disk read attempted.
        let text = read("nvidia.json", crate::matrix::NVIDIA_JSON, 0);
        assert_eq!(text, crate::matrix::NVIDIA_JSON);
    }

    #[test]
    fn missing_cache_falls_back_to_bundled() {
        let name = "pypilot-test-missing-nvidia.json";
        let text = read(name, crate::matrix::NVIDIA_JSON, 7);
        assert_eq!(text, crate::matrix::NVIDIA_JSON);
    }

    #[test]
    fn fresh_cache_is_preferred_over_bundled() {
        let name = "pypilot-test-fresh.json";
        let path = cache_path(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"schema_version":99}"#).unwrap();

        let text = read(name, crate::matrix::NVIDIA_JSON, 7);
        assert_eq!(text, r#"{"schema_version":99}"#);
    }

    #[test]
    fn corrupt_cache_falls_back_to_bundled_rather_than_serving_garbage() {
        let name = "pypilot-test-corrupt.json";
        let path = cache_path(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not json at all").unwrap();

        let text = read(name, crate::matrix::NVIDIA_JSON, 7);
        assert_eq!(text, crate::matrix::NVIDIA_JSON);
    }

    #[tokio::test]
    async fn refresh_if_stale_is_a_noop_when_fully_offline() {
        // Must not attempt any network call; nothing to assert on the network
        // side, but this must return promptly and not panic.
        refresh_if_stale(scratch_settings(0)).await;
    }
}
