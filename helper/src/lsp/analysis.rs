//! One import-diagnostics pass over one piece of Python text.
//!
//! Open buffers and the workspace scan share this, so a file shows the same
//! diagnostics whether it is open or only on disk. What the text is judged
//! against (the environment and what it can import) is read once into an
//! [`EnvSnapshot`]: per keystroke that is one read, and per workspace scan it
//! is one read for thousands of files instead of one each.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tower_lsp::lsp_types::Diagnostic;

use crate::core::installed::Installed;
use crate::core::platform::Platform;
use crate::core::{env, guardian};
use crate::pypi::metadata::PackageMetadata;
use crate::pypi::pyversion::PyVersion;
use crate::pypi::MetadataSource;

use super::diagnostics;

/// The project environment, as the import diagnostics see it.
pub struct EnvSnapshot {
    root: PathBuf,
    /// The environment's Python is gone. Per-import verdicts against it
    /// would be wrong either way; the onboarding scan reports it once, with
    /// the rebuild fix, so the diagnostics stay quiet until then.
    broken: bool,
    installed: Installed,
    venv_python: Option<PyVersion>,
    venv: PathBuf,
    pixi: bool,
}

impl EnvSnapshot {
    pub fn read(root: &Path) -> EnvSnapshot {
        // The project's environment: its `.venv`, or its pixi prefix.
        let env = env::project_env(root);
        let broken = env.as_ref().is_some_and(|e| e.broken.is_some());
        let installed = if broken {
            Installed::default()
        } else {
            env::importable(root, env.as_ref())
        };
        let venv_python = env.as_ref().and_then(|e| e.python);
        let venv = env.map(|e| e.path).unwrap_or_else(|| root.join(".venv"));
        EnvSnapshot {
            root: root.to_path_buf(),
            broken,
            installed,
            venv_python,
            venv,
            pixi: crate::core::pixi::detect(root).is_some(),
        }
    }
}

/// The diagnostics for `text`, a file living in `dir` (its own directory is
/// on `sys.path`, so modules next to it are local).
///
/// Everything here is local except the PyPI lookups, which are cached and
/// only happen for imports that are neither stdlib, local, nor installed.
pub async fn diagnose<S: MetadataSource>(
    snapshot: &EnvSnapshot,
    text: &str,
    dir: Option<&Path>,
    source: &S,
) -> Vec<Diagnostic> {
    if snapshot.broken {
        return Vec::new();
    }
    let mut findings = guardian::analyze_buffer_in(
        text,
        &snapshot.root,
        dir,
        Platform::current(),
        snapshot.venv_python,
        &snapshot.installed,
        source,
    )
    .await;
    // Packages that are installed still have to expose what the file uses.
    // Purely local, so this costs a directory listing rather than a request.
    findings.extend(guardian::check_attributes(
        text,
        &snapshot.venv,
        &snapshot.installed,
    ));
    findings
        .iter()
        .map(|f| diagnostics::to_diagnostic_in(f, snapshot.pixi))
        .collect()
}

/// A [`MetadataSource`] that answers each name once per workspace scan.
///
/// The disk cache already covers packages PyPI knows. It does not remember a
/// 404, and a module that only looks third-party (a sibling package the
/// local-module check cannot see) would otherwise cost one request per file
/// that imports it.
pub struct Memo<'a, S> {
    inner: &'a S,
    latest: Mutex<HashMap<String, Option<PackageMetadata>>>,
}

impl<'a, S: MetadataSource> Memo<'a, S> {
    pub fn new(inner: &'a S) -> Self {
        Memo {
            inner,
            latest: Mutex::new(HashMap::new()),
        }
    }
}

impl<S: MetadataSource> MetadataSource for Memo<'_, S> {
    async fn fetch(&self, name: &str) -> crate::Result<PackageMetadata> {
        let known = self.latest.lock().ok().and_then(|m| m.get(name).cloned());
        let answer = match known {
            Some(answer) => answer,
            None => {
                let answer = self.inner.fetch(name).await.ok();
                if let Ok(mut m) = self.latest.lock() {
                    m.insert(name.to_string(), answer.clone());
                }
                answer
            }
        };
        answer.ok_or_else(|| anyhow::anyhow!("no metadata for {name}"))
    }

    async fn fetch_version(&self, name: &str, version: &str) -> crate::Result<PackageMetadata> {
        self.inner.fetch_version(name, version).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counting(AtomicUsize);

    impl MetadataSource for Counting {
        async fn fetch(&self, name: &str) -> crate::Result<PackageMetadata> {
            self.0.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("{name}: 404")
        }
        async fn fetch_version(&self, name: &str, _: &str) -> crate::Result<PackageMetadata> {
            anyhow::bail!("{name}: 404")
        }
    }

    #[tokio::test]
    async fn a_missing_package_is_asked_for_once_per_scan() {
        let inner = Counting(AtomicUsize::new(0));
        let memo = Memo::new(&inner);
        for _ in 0..3 {
            assert!(memo.fetch("not-a-real-package").await.is_err());
        }
        assert!(memo.fetch("another").await.is_err());
        assert_eq!(inner.0.load(Ordering::SeqCst), 2);
    }
}
