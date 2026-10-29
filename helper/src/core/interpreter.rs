//! Interpreter discovery.
//!
//! Probes the machine for CPython interpreters by trying the conventional
//! command names (plus the `py` launcher on Windows) in parallel and asking each
//! for its version. Results are deduped by the interpreter's resolved
//! `sys.executable` so `python` and `python3` pointing at the same binary count
//! once.

use std::collections::HashMap;

use crate::core::command;
use crate::pypi::pyversion::{PyVersion, MAX_MINOR, MIN_MINOR};

/// A Python interpreter found on the machine.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Interpreter {
    /// The command name we found it under (e.g. "python3.12").
    pub command: String,
    /// Its resolved absolute path (`sys.executable`).
    pub path: String,
    pub version: PyVersion,
}

/// One-line Python program that prints "MAJOR.MINOR\t<executable path>".
const PROBE: &str =
    "import sys;print('%d.%d\t%s'%(sys.version_info[0],sys.version_info[1],sys.executable))";

/// Discover interpreters, running every candidate probe concurrently.
pub async fn discover() -> Vec<Interpreter> {
    let mut candidates: Vec<String> = vec!["python3".to_string(), "python".to_string()];
    for minor in MIN_MINOR..=MAX_MINOR {
        candidates.push(format!("python3.{minor}"));
    }

    let on_path = futures::future::join_all(candidates.into_iter().map(|cmd| async move {
        let (version, path) = probe(&cmd, &[]).await?;
        Some(Interpreter {
            command: cmd,
            path,
            version,
        })
    }));

    // The python.org installers on Windows put at most one `python` on PATH and
    // no `python3.X` names at all; every other version is only reachable
    // through the `py` launcher.
    let via_launcher =
        futures::future::join_all(launcher_flags().into_iter().map(|flag| async move {
            let (version, path) = probe("py", &[flag]).await?;
            // The launcher only finds it. Later steps run the interpreter itself.
            Some(Interpreter {
                command: path.clone(),
                path,
                version,
            })
        }));

    let (on_path, via_launcher) = tokio::join!(on_path, via_launcher);

    // Dedupe by resolved executable path, preferring the most specific command.
    let mut by_path: HashMap<String, Interpreter> = HashMap::new();
    for interp in on_path.into_iter().flatten() {
        by_path
            .entry(interp.path.clone())
            .and_modify(|existing| {
                if interp.command.len() > existing.command.len() {
                    *existing = interp.clone();
                }
            })
            .or_insert(interp);
    }
    // Launcher finds only fill gaps, so an interpreter that is also on PATH
    // keeps its short command name.
    for interp in via_launcher.into_iter().flatten() {
        by_path.entry(interp.path.clone()).or_insert(interp);
    }

    let mut list: Vec<Interpreter> = by_path.into_values().collect();
    list.sort_by_key(|i| std::cmp::Reverse(i.version));
    list
}

/// Run the version probe through `program`, with `prefix` ahead of `-c`.
/// Returns the version and the interpreter's `sys.executable`.
async fn probe(program: &str, prefix: &[String]) -> Option<(PyVersion, String)> {
    let mut args: Vec<&str> = prefix.iter().map(String::as_str).collect();
    args.extend(["-c", PROBE]);
    let out = command::run(program, &args, None).await.ok()?;
    if !out.success() {
        return None;
    }
    let line = out.first_stdout_line()?;
    let (ver, path) = line.split_once('\t')?;
    Some((PyVersion::parse(ver)?, path.trim().to_string()))
}

/// `py -3.X` selectors to try. Empty off Windows, where there is no launcher.
fn launcher_flags() -> Vec<String> {
    if cfg!(windows) {
        (MIN_MINOR..=MAX_MINOR)
            .map(|minor| format!("-3.{minor}"))
            .collect()
    } else {
        Vec::new()
    }
}

/// Find an interpreter matching an exact minor version (for pip-mode venv creation).
pub fn find_version(interpreters: &[Interpreter], version: PyVersion) -> Option<&Interpreter> {
    interpreters.iter().find(|i| i.version == version)
}
