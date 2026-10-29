//! Workspace-wide import diagnostics (`diagnosticsScope = "workspace"`).
//!
//! By default only open buffers get diagnostics. In workspace mode a
//! background pass also reads every `.py` file and notebook on disk and
//! publishes the same diagnostics for the ones that are not open, so they show
//! up in the editor's Problems panel.
//!
//! - The walk skips environments, VCS and build output (by name, hidden
//!   directories, and any directory with a `pyvenv.cfg`) and honours
//!   `.gitignore` files (the common subset: globs, `**`, `!`, trailing `/`,
//!   anchoring; not `.git/info/exclude` or the global excludes file).
//! - It is bounded: at most [`MAX_FILES`] files, [`MAX_ENTRIES`] directory
//!   entries, and files above [`MAX_PY_BYTES`] / [`MAX_NOTEBOOK_BYTES`] are
//!   skipped.
//! - Scans run on their own task, debounced, and a newer scan request
//!   abandons the one in flight.
//! - An open document always wins: a file (or notebook) the editor has open is
//!   never published from disk, and opening one clears what the scan said.
//! - Changed files are re-checked from `workspace/didChangeWatchedFiles`, and
//!   everything after an environment change (install, rebuild, setup, a
//!   `pypilot setup` from a task, new editor settings).
//! - Switching back to `openFiles` clears everything the scan published.
//!
//! A notebook on disk is one file, so its diagnostics are published on the
//! `.ipynb` itself: each message names the cell and line, and the range is
//! the top of the file (a position inside the notebook's JSON would mean
//! nothing in a notebook editor).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tower_lsp::lsp_types::*;

use crate::settings::{DiagnosticsScope, Notifications, Settings};

use super::analysis::{self, EnvSnapshot, Memo};
use super::{notebook_url, Shared, NOTEBOOK_CELL_SCHEME};

/// Most files one scan analyzes. A repo with more Python than this is better
/// served by opening the folder that matters.
pub const MAX_FILES: usize = 5_000;
/// Most directory entries one walk reads, so a huge data folder that is not
/// ignored cannot stall it.
pub const MAX_ENTRIES: usize = 200_000;
/// Larger `.py` files are generated code, not something to fix imports in.
pub const MAX_PY_BYTES: u64 = 1024 * 1024;
/// Notebooks carry their outputs (plots, tables), so they may be bigger.
pub const MAX_NOTEBOOK_BYTES: u64 = 16 * 1024 * 1024;

/// Wait this long after the last request before a full scan, so the
/// `initialized` scan and VS Code's first `didChangeConfiguration` coalesce.
const FULL_DEBOUNCE: Duration = Duration::from_millis(750);
/// And this long after the last watched-file event, so a branch switch or a
/// formatter run over many files costs one pass.
const FILE_DEBOUNCE: Duration = Duration::from_millis(500);

const WATCH_REGISTRATION_ID: &str = "pypilot-workspace-files";

/// Directories that never hold the user's own code.
const SKIP_DIRS: &[&str] = &[
    "venv",
    "env",
    "node_modules",
    "__pycache__",
    "target",
    "build",
    "dist",
    "site-packages",
    "conda-meta",
];

/// The scan's state, in [`Shared`].
#[derive(Default)]
pub struct State {
    /// What the scan has published and not cleared, by [`path_key`].
    published: HashMap<String, Url>,
    /// Bumped by every full-scan request; a scan whose number is stale stops.
    generation: u64,
    /// Files waiting for the next debounced re-check.
    pending: HashSet<PathBuf>,
    pending_generation: u64,
    /// The client can register file watchers dynamically.
    pub can_watch: bool,
    watching: bool,
}

// --- scheduling ---------------------------------------------------------------

fn scope_of(root: &Path) -> (DiagnosticsScope, bool) {
    let settings = Settings::load(root);
    (
        settings.diagnostics_scope,
        settings.notifications != Notifications::Off,
    )
}

/// Bring the scan in line with the current settings: start (or redo) it in
/// workspace mode, clear what it published otherwise. Called after startup,
/// after new editor settings and after every environment change.
pub async fn apply_scope(shared: Arc<Shared>) {
    let Some(root) = shared.root.lock().await.clone() else {
        return;
    };
    match scope_of(&root) {
        (DiagnosticsScope::Workspace, true) => {
            ensure_watching(&shared).await;
            schedule_full_scan(shared).await;
        }
        _ => {
            // Abandon a scan in flight before clearing behind it.
            shared.workspace.lock().await.generation += 1;
            clear_all(&shared).await;
        }
    }
}

/// Ask the client to send `didChangeWatchedFiles` for Python files, once.
async fn ensure_watching(shared: &Arc<Shared>) {
    {
        let mut state = shared.workspace.lock().await;
        if !state.can_watch || state.watching {
            return;
        }
        state.watching = true;
    }
    let watchers = ["**/*.py", "**/*.ipynb"]
        .iter()
        .map(|g| FileSystemWatcher {
            glob_pattern: GlobPattern::String(g.to_string()),
            kind: None,
        })
        .collect();
    let options = DidChangeWatchedFilesRegistrationOptions { watchers };
    let registration = Registration {
        id: WATCH_REGISTRATION_ID.to_string(),
        method: "workspace/didChangeWatchedFiles".to_string(),
        register_options: serde_json::to_value(options).ok(),
    };
    // A request to the client: answer it off this task.
    let client = shared.client.clone();
    tokio::spawn(async move {
        if let Err(e) = client.register_capability(vec![registration]).await {
            client
                .log_message(
                    MessageType::WARNING,
                    format!("could not watch Python files: {e}"),
                )
                .await;
        }
    });
}

/// Re-scan the whole workspace after [`FULL_DEBOUNCE`].
pub async fn schedule_full_scan(shared: Arc<Shared>) {
    let generation = {
        let mut state = shared.workspace.lock().await;
        state.generation += 1;
        state.generation
    };
    tokio::spawn(async move {
        tokio::time::sleep(FULL_DEBOUNCE).await;
        if shared.workspace.lock().await.generation == generation {
            full_scan(shared, generation).await;
        }
    });
}

/// Re-check these files after [`FILE_DEBOUNCE`]: changed, created, deleted,
/// or just closed in the editor. Ignored outside workspace mode.
pub async fn schedule_paths(shared: Arc<Shared>, paths: Vec<PathBuf>) {
    if paths.is_empty() {
        return;
    }
    let generation = {
        let mut state = shared.workspace.lock().await;
        state.pending.extend(paths);
        state.pending_generation += 1;
        state.pending_generation
    };
    tokio::spawn(async move {
        tokio::time::sleep(FILE_DEBOUNCE).await;
        let paths: Vec<PathBuf> = {
            let mut state = shared.workspace.lock().await;
            if state.pending_generation != generation {
                return;
            }
            state.pending.drain().collect()
        };
        recheck(shared, paths).await;
    });
}

async fn full_scan(shared: Arc<Shared>, generation: u64) {
    let Some(root) = shared.root.lock().await.clone() else {
        return;
    };
    if scope_of(&root) != (DiagnosticsScope::Workspace, true) {
        return;
    }
    let walk_root = root.clone();
    let Ok(scan) = tokio::task::spawn_blocking(move || collect_files(&walk_root)).await else {
        return;
    };
    if scan.truncated {
        shared
            .client
            .log_message(
                MessageType::INFO,
                format!(
                    "PyPilot: workspace diagnostics cover the first {} Python files only.",
                    scan.files.len()
                ),
            )
            .await;
    }
    let snap_root = root.clone();
    let Ok(snapshot) = tokio::task::spawn_blocking(move || EnvSnapshot::read(&snap_root)).await
    else {
        return;
    };
    let memo = Memo::new(&shared.source);
    let mut seen = HashSet::new();
    for path in &scan.files {
        if shared.workspace.lock().await.generation != generation {
            return;
        }
        seen.insert(path_key(path));
        check_file(&shared, &snapshot, &memo, path).await;
    }
    // Deleted, now ignored, or past the cap: whatever the last scan said
    // about them no longer holds.
    let stale: Vec<Url> = {
        let mut state = shared.workspace.lock().await;
        if state.generation != generation {
            return;
        }
        let keys: Vec<String> = state
            .published
            .keys()
            .filter(|k| !seen.contains(*k))
            .cloned()
            .collect();
        keys.iter()
            .filter_map(|k| state.published.remove(k))
            .collect()
    };
    for uri in stale {
        shared.client.publish_diagnostics(uri, vec![], None).await;
    }
}

async fn recheck(shared: Arc<Shared>, paths: Vec<PathBuf>) {
    let Some(root) = shared.root.lock().await.clone() else {
        return;
    };
    if scope_of(&root) != (DiagnosticsScope::Workspace, true) {
        return;
    }
    let snap_root = root.clone();
    let Ok(snapshot) = tokio::task::spawn_blocking(move || EnvSnapshot::read(&snap_root)).await
    else {
        return;
    };
    let memo = Memo::new(&shared.source);
    for path in paths {
        if path.is_file() && is_candidate(&root, &path) {
            check_file(&shared, &snapshot, &memo, &path).await;
        } else {
            clear_path(&shared, &path).await;
        }
    }
}

/// Analyze one file from disk and publish the result, unless it is open.
async fn check_file<S: crate::pypi::MetadataSource>(
    shared: &Arc<Shared>,
    snapshot: &EnvSnapshot,
    source: &S,
    path: &Path,
) {
    let notebook = path.extension().is_some_and(|e| e == "ipynb");
    let limit = if notebook {
        MAX_NOTEBOOK_BYTES
    } else {
        MAX_PY_BYTES
    };
    if std::fs::metadata(path).map_or(true, |m| m.len() > limit) {
        return clear_path(shared, path).await;
    }
    let Ok(text) = tokio::fs::read_to_string(path).await else {
        return clear_path(shared, path).await;
    };
    // Open documents are analyzed from the editor's text; skip the work.
    if is_open(shared, path).await {
        return;
    }
    let dir = path.parent();
    let diagnostics = if notebook {
        match notebook_python(&text) {
            Some(nb) => {
                let found = analysis::diagnose(snapshot, &nb.text, dir, source).await;
                found.into_iter().map(|d| nb.relocate(d)).collect()
            }
            None => Vec::new(),
        }
    } else {
        analysis::diagnose(snapshot, &text, dir, source).await
    };
    publish(shared, path, diagnostics).await;
}

/// Publish for a file on disk. Holds the document table while it does, so a
/// `didOpen` either lands first (and this publishes nothing) or after (and
/// its own, newer result replaces this one).
async fn publish(shared: &Arc<Shared>, path: &Path, diagnostics: Vec<Diagnostic>) {
    let Ok(uri) = Url::from_file_path(path) else {
        return;
    };
    let key = path_key(path);
    let documents = shared.documents.lock().await;
    if documents
        .keys()
        .any(|u| document_path_key(u).as_ref() == Some(&key))
    {
        return;
    }
    {
        let mut state = shared.workspace.lock().await;
        if diagnostics.is_empty() {
            // Nothing to say, and nothing said before: no traffic.
            if state.published.remove(&key).is_none() {
                return;
            }
        } else {
            state.published.insert(key, uri.clone());
        }
    }
    shared
        .client
        .publish_diagnostics(uri, diagnostics, None)
        .await;
    drop(documents);
}

async fn clear_path(shared: &Arc<Shared>, path: &Path) {
    let removed = shared
        .workspace
        .lock()
        .await
        .published
        .remove(&path_key(path));
    if let Some(uri) = removed {
        shared.client.publish_diagnostics(uri, vec![], None).await;
    }
}

async fn clear_all(shared: &Arc<Shared>) {
    let all: Vec<Url> = shared
        .workspace
        .lock()
        .await
        .published
        .drain()
        .map(|(_, u)| u)
        .collect();
    for uri in all {
        shared.client.publish_diagnostics(uri, vec![], None).await;
    }
}

async fn is_open(shared: &Arc<Shared>, path: &Path) -> bool {
    let key = path_key(path);
    shared
        .documents
        .lock()
        .await
        .keys()
        .any(|u| document_path_key(u).as_ref() == Some(&key))
}

/// The editor opened `uri`: the open document speaks for that file now, so
/// drop what the scan published for it (for a notebook cell, the notebook).
pub async fn on_open(shared: &Arc<Shared>, uri: &Url) {
    if let Some(path) = document_path(uri) {
        clear_path(shared, &path).await;
    }
}

/// The editor closed `uri`: in workspace mode, the file on disk is checked
/// again once nothing of it is open (a notebook closes cell by cell).
pub async fn on_close(shared: Arc<Shared>, uri: &Url) {
    let Some(path) = document_path(uri) else {
        return;
    };
    if is_open(&shared, &path).await {
        return;
    }
    let Some(root) = shared.root.lock().await.clone() else {
        return;
    };
    if scope_of(&root) == (DiagnosticsScope::Workspace, true) && is_candidate(&root, &path) {
        schedule_paths(shared, vec![path]).await;
    }
}

/// The file on disk a document stands for: itself, or a cell's notebook.
fn document_path(uri: &Url) -> Option<PathBuf> {
    if uri.scheme() == NOTEBOOK_CELL_SCHEME {
        notebook_url(uri)?.to_file_path().ok()
    } else {
        uri.to_file_path().ok()
    }
}

fn document_path_key(uri: &Url) -> Option<String> {
    document_path(uri).map(|p| path_key(&p))
}

/// One spelling per file. Clients send `file:///c%3A/...`, the walk produces
/// `C:\...`; Windows paths compare case-insensitively.
pub fn path_key(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

// --- the walk ----------------------------------------------------------------

/// What [`collect_files`] found.
#[derive(Debug, Default)]
pub struct Scan {
    pub files: Vec<PathBuf>,
    /// A cap was hit, so some files were left out.
    pub truncated: bool,
}

/// Every `.py` file and notebook under `root` worth diagnosing, in a stable
/// order, at most [`MAX_FILES`].
pub fn collect_files(root: &Path) -> Scan {
    collect_files_capped(root, MAX_FILES, MAX_ENTRIES)
}

pub fn collect_files_capped(root: &Path, max_files: usize, max_entries: usize) -> Scan {
    let mut scan = Scan::default();
    let mut budget = max_entries;
    let mut rules = Vec::new();
    walk(root, "", &mut rules, max_files, &mut budget, &mut scan);
    scan
}

fn walk(
    dir: &Path,
    rel: &str,
    rules: &mut Vec<Rule>,
    max_files: usize,
    budget: &mut usize,
    scan: &mut Scan,
) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let added = load_gitignore(dir, rel, rules);
    let mut entries: Vec<std::fs::DirEntry> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    let mut subdirs = Vec::new();
    for entry in entries {
        if scan.files.len() >= max_files || *budget == 0 {
            scan.truncated = true;
            break;
        }
        *budget -= 1;
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().to_string();
        let child_rel = join_rel(rel, &name);
        let path = entry.path();
        if kind.is_dir() {
            if !skip_dir(&path, &name) && !ignored(rules, &child_rel, &name, true) {
                subdirs.push((path, child_rel));
            }
        } else if kind.is_file()
            && is_python_file(&name)
            && !ignored(rules, &child_rel, &name, false)
        {
            scan.files.push(path);
        }
    }
    // Files first, so a capped scan keeps the shallow ones.
    for (path, child_rel) in subdirs {
        if scan.truncated {
            break;
        }
        walk(&path, &child_rel, rules, max_files, budget, scan);
    }
    rules.truncate(rules.len() - added);
}

fn is_python_file(name: &str) -> bool {
    name.ends_with(".py") || name.ends_with(".ipynb")
}

fn skip_dir(path: &Path, name: &str) -> bool {
    name.starts_with('.') || SKIP_DIRS.contains(&name) || path.join("pyvenv.cfg").is_file()
}

fn join_rel(rel: &str, name: &str) -> String {
    if rel.is_empty() {
        name.to_string()
    } else {
        format!("{rel}/{name}")
    }
}

/// Would the walk include `path`? For watched-file events, which arrive for
/// `.venv/**/*.py` too while a package installs.
pub fn is_candidate(root: &Path, path: &Path) -> bool {
    let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
        return false;
    };
    if !is_python_file(&name) {
        return false;
    }
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    let mut rules = Vec::new();
    let mut dir = root.to_path_buf();
    let mut rel_so_far = String::new();
    for (i, part) in parts.iter().enumerate() {
        load_gitignore(&dir, &rel_so_far, &mut rules);
        let child_rel = join_rel(&rel_so_far, part);
        let is_dir = i + 1 < parts.len();
        dir = dir.join(part);
        if is_dir && skip_dir(&dir, part) {
            return false;
        }
        if ignored(&rules, &child_rel, part, is_dir) {
            return false;
        }
        rel_so_far = child_rel;
    }
    true
}

// --- .gitignore ----------------------------------------------------------------

/// One `.gitignore` line.
#[derive(Debug, Clone)]
struct Rule {
    /// Directory of the `.gitignore`, relative to the scan root.
    base: String,
    pattern: String,
    negated: bool,
    dir_only: bool,
    /// Has a `/` before its end: matched against the path from `base`, not
    /// against the bare name at any depth.
    anchored: bool,
}

/// Push the rules of `dir/.gitignore`; returns how many.
fn load_gitignore(dir: &Path, rel: &str, rules: &mut Vec<Rule>) -> usize {
    let Ok(text) = std::fs::read_to_string(dir.join(".gitignore")) else {
        return 0;
    };
    let before = rules.len();
    rules.extend(text.lines().filter_map(|l| parse_rule(rel, l)));
    rules.len() - before
}

fn parse_rule(base: &str, line: &str) -> Option<Rule> {
    let line = line.trim_end();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (negated, line) = match line.strip_prefix('!') {
        Some(rest) => (true, rest),
        None => (false, line.strip_prefix('\\').unwrap_or(line)),
    };
    let (dir_only, line) = match line.strip_suffix('/') {
        Some(rest) => (true, rest),
        None => (false, line),
    };
    let anchored = line.contains('/');
    let pattern = line.strip_prefix('/').unwrap_or(line).to_string();
    if pattern.is_empty() {
        return None;
    }
    Some(Rule {
        base: base.to_string(),
        pattern,
        negated,
        dir_only,
        anchored,
    })
}

/// Git's rule: the last matching line decides.
fn ignored(rules: &[Rule], rel: &str, name: &str, is_dir: bool) -> bool {
    let mut verdict = false;
    for rule in rules {
        if rule.dir_only && !is_dir {
            continue;
        }
        let matched = if rule.anchored {
            let from_base = if rule.base.is_empty() {
                Some(rel)
            } else {
                rel.strip_prefix(&rule.base)
                    .and_then(|r| r.strip_prefix('/'))
            };
            from_base.is_some_and(|r| glob(rule.pattern.as_bytes(), r.as_bytes()))
        } else {
            glob(rule.pattern.as_bytes(), name.as_bytes())
        };
        if matched {
            verdict = !rule.negated;
        }
    }
    verdict
}

/// gitignore globbing: `*` and `?` stay within one path segment, `**`
/// crosses them, `[...]` is a character class.
fn glob(p: &[u8], s: &[u8]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some(b'*') if p.get(1) == Some(&b'*') => {
            let rest = &p[2..];
            // `**/` also matches no directory at all.
            if let Some(after) = rest.strip_prefix(b"/") {
                if glob(after, s) {
                    return true;
                }
            }
            (0..=s.len()).any(|i| glob(rest, &s[i..]))
        }
        Some(b'*') => {
            let rest = &p[1..];
            for i in 0..=s.len() {
                if glob(rest, &s[i..]) {
                    return true;
                }
                if s.get(i) == Some(&b'/') {
                    break;
                }
            }
            false
        }
        Some(b'?') => s.first().is_some_and(|c| *c != b'/') && glob(&p[1..], &s[1..]),
        Some(b'[') => {
            let Some(close) = p.iter().skip(2).position(|c| *c == b']').map(|i| i + 2) else {
                return s.first() == Some(&b'[') && glob(&p[1..], &s[1..]);
            };
            let Some(&c) = s.first() else { return false };
            let mut class = &p[1..close];
            let negate = matches!(class.first(), Some(b'!') | Some(b'^'));
            if negate {
                class = &class[1..];
            }
            let mut hit = false;
            let mut i = 0;
            while i < class.len() {
                if i + 2 < class.len() && class[i + 1] == b'-' {
                    hit |= class[i] <= c && c <= class[i + 2];
                    i += 3;
                } else {
                    hit |= class[i] == c;
                    i += 1;
                }
            }
            hit != negate && c != b'/' && glob(&p[close + 1..], &s[1..])
        }
        Some(b'\\') if p.len() > 1 => s.first() == Some(&p[1]) && glob(&p[2..], &s[1..]),
        Some(c) => s.first() == Some(c) && glob(&p[1..], &s[1..]),
    }
}

// --- notebooks -----------------------------------------------------------------

/// A notebook's Python code cells, as one text for the analysis.
#[derive(Debug)]
pub struct NotebookText {
    pub text: String,
    /// `(cell number, first line in text)`, numbered from 1 like Jupyter's
    /// own cell list (markdown cells count).
    cells: Vec<(usize, u32)>,
}

impl NotebookText {
    /// Move a diagnostic found in [`Self::text`] to the top of the notebook
    /// file, naming the cell and line in the message instead.
    fn relocate(&self, mut d: Diagnostic) -> Diagnostic {
        let line = d.range.start.line;
        if let Some((cell, start)) = self.cells.iter().rev().find(|(_, s)| *s <= line) {
            d.message = format!("{} (cell {cell}, line {})", d.message, line - start + 1);
        }
        d.range = Range::default();
        d
    }
}

/// The code cells of an `.ipynb`, with IPython syntax masked. `None` for
/// what is not a notebook, or one whose kernel is not Python.
pub fn notebook_python(json: &str) -> Option<NotebookText> {
    let nb: serde_json::Value = serde_json::from_str(json).ok()?;
    let language = nb
        .pointer("/metadata/kernelspec/language")
        .or_else(|| nb.pointer("/metadata/language_info/name"))
        .and_then(|l| l.as_str());
    if language.is_some_and(|l| !l.eq_ignore_ascii_case("python")) {
        return None;
    }
    let mut text = String::new();
    let mut cells = Vec::new();
    let mut line = 0u32;
    for (i, cell) in nb.get("cells")?.as_array()?.iter().enumerate() {
        if cell.get("cell_type").and_then(|t| t.as_str()) != Some("code") {
            continue;
        }
        let source = match cell.get("source") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Array(parts)) => {
                parts.iter().filter_map(|p| p.as_str()).collect::<String>()
            }
            _ => continue,
        };
        let count = source.lines().count().max(1);
        let masked = crate::core::imports::mask_ipython(&source);
        let mut lines: Vec<&str> = masked.lines().collect();
        lines.resize(count, "");
        cells.push((i + 1, line));
        for l in lines {
            text.push_str(l);
            text.push('\n');
        }
        line += count as u32;
    }
    Some(NotebookText { text, cells })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pypilot-wscan-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "import os\n").unwrap();
    }

    fn rels(root: &Path, scan: &Scan) -> Vec<String> {
        scan.files
            .iter()
            .map(|p| {
                p.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }

    #[test]
    fn the_walk_skips_environments_vcs_build_output_and_gitignored_paths() {
        let root = scratch("skip");
        for f in [
            "main.py",
            "pkg/mod.py",
            "notebooks/a.ipynb",
            "README.md",
            ".venv/lib/site.py",
            ".pixi/envs/default/x.py",
            "node_modules/x/y.py",
            ".git/hooks/h.py",
            "build/lib/b.py",
            "dist/d.py",
            "myenv/lib/e.py",
            "generated/g.py",
            "pkg/secret_gen.py",
            "pkg/keep_gen.py",
            "data/raw/r.py",
            "data/raw/keep/k.py",
        ] {
            touch(&root.join(f));
        }
        std::fs::write(root.join("myenv/pyvenv.cfg"), "").unwrap();
        std::fs::write(
            root.join(".gitignore"),
            "# comment\n/generated/\n*_gen.py\n!keep_gen.py\n",
        )
        .unwrap();
        std::fs::write(root.join("data/.gitignore"), "raw/\n").unwrap();

        let scan = collect_files(&root);
        assert!(!scan.truncated);
        assert_eq!(
            rels(&root, &scan),
            [
                "main.py",
                "notebooks/a.ipynb",
                "pkg/keep_gen.py",
                "pkg/mod.py"
            ]
        );

        // Watched-file events go through the same rules.
        assert!(is_candidate(&root, &root.join("pkg/mod.py")));
        assert!(is_candidate(&root, &root.join("pkg/keep_gen.py")));
        for no in [
            ".venv/lib/site.py",
            "myenv/lib/e.py",
            "generated/g.py",
            "pkg/secret_gen.py",
            "data/raw/r.py",
            "README.md",
        ] {
            assert!(!is_candidate(&root, &root.join(no)), "{no}");
        }
        assert!(!is_candidate(
            &root,
            &std::env::temp_dir().join("elsewhere.py")
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_walk_stops_at_the_cap_and_says_so() {
        let root = scratch("cap");
        for i in 0..10 {
            touch(&root.join(format!("f{i}.py")));
            touch(&root.join(format!("sub/s{i}.py")));
        }
        let scan = collect_files_capped(&root, 4, MAX_ENTRIES);
        assert!(scan.truncated);
        assert_eq!(scan.files.len(), 4);
        // Shallow files first.
        assert!(rels(&root, &scan).iter().all(|f| !f.starts_with("sub/")));

        let scan = collect_files_capped(&root, MAX_FILES, 5);
        assert!(scan.truncated);
        assert!(scan.files.len() <= 5);

        let scan = collect_files_capped(&root, 20, MAX_ENTRIES);
        assert!(!scan.truncated);
        assert_eq!(scan.files.len(), 20);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn gitignore_globs() {
        assert!(glob(b"*.py", b"a.py"));
        assert!(!glob(b"*.py", b"a/b.py"));
        assert!(glob(b"**/gen", b"gen"));
        assert!(glob(b"**/gen", b"a/b/gen"));
        assert!(glob(b"a/**/b", b"a/b"));
        assert!(glob(b"a/**/b", b"a/x/y/b"));
        assert!(glob(b"out/**", b"out/x/y.py"));
        assert!(glob(b"f?o.py", b"foo.py"));
        assert!(glob(b"[abc]x", b"bx") && !glob(b"[!abc]x", b"bx"));
        assert!(glob(b"v[0-9]", b"v7") && !glob(b"v[0-9]", b"vx"));
        assert!(!glob(b"build", b"builder"));
    }

    #[test]
    fn notebooks_read_their_code_cells_with_magics_masked() {
        let nb = serde_json::json!({
            "metadata": {"kernelspec": {"language": "python"}},
            "cells": [
                {"cell_type": "markdown", "source": ["# title\n"]},
                {"cell_type": "code", "source": ["%pip install x\n", "import numpy\n"]},
                {"cell_type": "code", "source": "import os\nimport polars"}
            ]
        });
        let text = notebook_python(&nb.to_string()).unwrap();
        assert_eq!(text.text, "\nimport numpy\nimport os\nimport polars\n");

        let at = |line| Diagnostic {
            range: Range::new(Position::new(line, 7), Position::new(line, 13)),
            message: "`polars` is not installed.".into(),
            ..Default::default()
        };
        let moved = text.relocate(at(3));
        assert_eq!(moved.message, "`polars` is not installed. (cell 3, line 2)");
        assert_eq!(moved.range, Range::default());
        assert!(text.relocate(at(1)).message.ends_with("(cell 2, line 2)"));

        let r = serde_json::json!({
            "metadata": {"kernelspec": {"language": "R"}},
            "cells": [{"cell_type": "code", "source": "library(x)"}]
        });
        assert!(notebook_python(&r.to_string()).is_none());
        assert!(notebook_python("not json").is_none());
    }

    #[test]
    fn client_and_walk_spellings_of_a_path_agree() {
        let uri = Url::parse("file:///c%3A/Users/me/p/a.py").unwrap();
        #[cfg(windows)]
        assert_eq!(
            document_path_key(&uri),
            Some(path_key(Path::new(r"C:\Users\me\p\a.py")))
        );
        let cell = Url::parse("vscode-notebook-cell:/c%3A/Users/me/p/n.ipynb#W1").unwrap();
        #[cfg(windows)]
        assert_eq!(
            document_path_key(&cell),
            Some(path_key(Path::new(r"c:\users\me\p\n.ipynb")))
        );
        let _ = (uri, cell);
    }
}
