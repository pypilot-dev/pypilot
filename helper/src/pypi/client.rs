//! PyPI metadata source — the network client and its test-friendly abstraction.
//!
//! Everything downstream (the F3 engine, the solver) is generic over
//! [`MetadataSource`], so tests inject recorded fixtures and never touch the
//! network. The production [`PyPiClient`] layers the disk [`Cache`] over reqwest.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Context;

use crate::pypi::cache::Cache;
use crate::pypi::metadata::PackageMetadata;

/// PyPI answered that it has no project by this name (HTTP 404).
///
/// Kept apart from every other lookup failure: "no such project" is worth
/// telling the user (a typo, or a private index), while a network error says
/// nothing about the package and must not be reported as if it did.
#[derive(Debug)]
pub struct NotOnPyPi(pub String);

impl std::fmt::Display for NotOnPyPi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "`{}` is not on PyPI; check the spelling", self.0)
    }
}

impl std::error::Error for NotOnPyPi {}

/// True when `err` is PyPI saying the project does not exist.
pub fn is_not_on_pypi(err: &anyhow::Error) -> bool {
    err.downcast_ref::<NotOnPyPi>().is_some()
}

/// PyPI answered with a server error (5xx).
#[derive(Debug)]
pub struct PyPiUnavailable(pub String);

impl std::fmt::Display for PyPiUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PyPiUnavailable {}

/// True when the lookup never got an answer about the package: no
/// connection, a timeout, a proxy refusing, or a PyPI server error.
pub fn is_unreachable(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause.downcast_ref::<reqwest::Error>().is_some()
            || cause.downcast_ref::<PyPiUnavailable>().is_some()
    })
}

/// Anything that can resolve a package name to its latest-release metadata.
///
/// Uses native `async fn` in traits (stable), so the engine is generic over the
/// source rather than boxing — zero-cost and easy to fake in tests. The engine
/// only ever uses this through concrete generic parameters (never `dyn`), so the
/// missing `Send` bound the lint warns about doesn't apply here.
#[allow(async_fn_in_trait)]
pub trait MetadataSource {
    /// Metadata for the newest release, plus the list of every published version.
    async fn fetch(&self, name: &str) -> crate::Result<PackageMetadata>;

    /// Metadata for one specific release. Needed when a dependency is pinned or
    /// capped, because the files that decide Python support belong to that
    /// release rather than the newest one.
    async fn fetch_version(&self, name: &str, version: &str) -> crate::Result<PackageMetadata>;
}

/// Live PyPI client: disk cache first, then `pypi.org/pypi/<name>/json`.
pub struct PyPiClient {
    http: reqwest::Client,
    cache: Cache,
    base_url: String,
}

impl PyPiClient {
    pub fn new() -> PyPiClient {
        PyPiClient::with_cache(Cache::open())
    }

    pub fn with_cache(cache: Cache) -> PyPiClient {
        let http = reqwest::Client::builder()
            .user_agent(concat!("pypilot/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("reqwest client with rustls should build");
        PyPiClient {
            http,
            cache,
            base_url: "https://pypi.org/pypi".to_string(),
        }
    }
}

#[cfg(test)]
impl PyPiClient {
    /// A client that never reaches PyPI (a closed local port, no disk cache):
    /// every lookup fails at once, as when offline.
    pub fn unreachable() -> PyPiClient {
        PyPiClient {
            base_url: "http://127.0.0.1:9".to_string(),
            ..PyPiClient::with_cache(Cache::disabled())
        }
    }

    /// A client for a stand-in index at `base_url`, with no disk cache.
    #[cfg(test)]
    pub fn at(base_url: &str) -> PyPiClient {
        PyPiClient {
            base_url: base_url.to_string(),
            ..PyPiClient::with_cache(Cache::disabled())
        }
    }
}

impl Default for PyPiClient {
    fn default() -> Self {
        PyPiClient::new()
    }
}

impl MetadataSource for PyPiClient {
    async fn fetch(&self, name: &str) -> crate::Result<PackageMetadata> {
        // 1. Fresh-enough latest pointer? (24h TTL, handled inside the cache.)
        if let Some(meta) = self.cache.get_latest(name) {
            return Ok(meta);
        }

        // 2. Network. When PyPI is unreachable, an expired entry is still
        // better than calling the package unresolvable.
        let url = format!("{}/{}/json", self.base_url, name);
        let resp = match self.http.get(&url).send().await {
            Ok(resp) => resp,
            Err(e) => {
                if let Some(meta) = self.cache.get_latest_stale(name) {
                    return Ok(meta);
                }
                return Err(e).with_context(|| format!("requesting metadata for `{name}`"));
            }
        };

        if !resp.status().is_success() {
            if resp.status().is_server_error() {
                if let Some(meta) = self.cache.get_latest_stale(name) {
                    return Ok(meta);
                }
            }
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                return Err(NotOnPyPi(name.to_string()).into());
            }
            let message = format!("PyPI returned {} for `{name}`", resp.status());
            if resp.status().is_server_error() {
                return Err(PyPiUnavailable(message).into());
            }
            anyhow::bail!(message);
        }

        let meta: PackageMetadata = resp
            .json()
            .await
            .with_context(|| format!("parsing PyPI JSON for `{name}`"))?;

        // 3. Persist into both cache tiers.
        self.cache.put_latest(name, &meta);
        Ok(meta)
    }

    async fn fetch_version(&self, name: &str, version: &str) -> crate::Result<PackageMetadata> {
        // A released version's file list never changes, so this tier is cached
        // forever and a hit means no request at all.
        if let Some(meta) = self.cache.get_release(name, version) {
            return Ok(meta);
        }

        let url = format!("{}/{}/{}/json", self.base_url, name, version);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("requesting metadata for `{name}=={version}`"))?;

        if !resp.status().is_success() {
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                anyhow::bail!("`{name}` has no release {version} on PyPI");
            }
            let message = format!("PyPI returned {} for `{name}=={version}`", resp.status());
            if resp.status().is_server_error() {
                return Err(PyPiUnavailable(message).into());
            }
            anyhow::bail!(message);
        }

        let meta: PackageMetadata = resp
            .json()
            .await
            .with_context(|| format!("parsing PyPI JSON for `{name}=={version}`"))?;

        // Release tier only: this is an older document and must not become the
        // package's "latest" answer.
        self.cache.put_release(name, &meta);
        Ok(meta)
    }
}

/// Recorded-fixture source used by tests — no network, ever.
#[derive(Default, Clone)]
pub struct FixtureSource {
    by_name: HashMap<String, PackageMetadata>,
}

impl FixtureSource {
    pub fn new() -> FixtureSource {
        FixtureSource::default()
    }

    /// Insert metadata under a name (normalized to lowercase).
    pub fn insert(&mut self, name: &str, meta: PackageMetadata) {
        self.by_name.insert(name.to_lowercase(), meta);
    }

    /// Load a recorded PyPI JSON document from disk and register it under `name`.
    pub fn load_json(&mut self, name: &str, path: impl AsRef<Path>) -> crate::Result<()> {
        let text = std::fs::read_to_string(path.as_ref())
            .with_context(|| format!("reading fixture {}", path.as_ref().display()))?;
        let meta: PackageMetadata = serde_json::from_str(&text)
            .with_context(|| format!("parsing fixture {}", path.as_ref().display()))?;
        self.insert(name, meta);
        Ok(())
    }
}

impl MetadataSource for FixtureSource {
    async fn fetch(&self, name: &str) -> crate::Result<PackageMetadata> {
        self.by_name
            .get(&name.to_lowercase())
            .cloned()
            .ok_or_else(|| NotOnPyPi(name.to_string()).into())
    }

    async fn fetch_version(&self, name: &str, version: &str) -> crate::Result<PackageMetadata> {
        // Registered as "name==version", mirroring how a pin is written.
        let key = format!("{}=={version}", name.to_lowercase());
        self.by_name
            .get(&key)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no fixture registered for `{key}`"))
    }
}
