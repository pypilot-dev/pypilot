//! Record the driver-matched PyTorch index in `pyproject.toml`.
//!
//! In uv project mode, a `--index <url>` on the `uv add` command line only
//! steers that one resolution. The next `uv sync` or `uv run` re-resolves from
//! `pyproject.toml`/`uv.lock`, finds `torch` on PyPI, and quietly swaps the
//! CUDA build for the default one (astral-sh/uv#17186). The fix uv documents
//! (docs.astral.sh/uv/guides/integration/pytorch) is to write the index into
//! the manifest: a named, `explicit` `[[tool.uv.index]]` plus a
//! `[tool.uv.sources]` entry routing each torch-family package to it.
//!
//! `uv add --index name=url` gets close but is not enough on its own. The
//! index it writes is not `explicit`, so uv also consults it for every other
//! package. Its source has no platform marker, so macOS (which has no CUDA
//! wheels and takes the PyPI build) cannot resolve. And a later CUDA change
//! leaves the old index behind. So the manifest is edited here, with
//! `toml_edit` so the user's comments and layout survive, and `uv add` then
//! runs without `--index`, honouring the sources it finds.

use std::path::Path;

use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, TableLike, Value};

use crate::core::project::requirement_name;

/// CUDA (and the CPU-only index) apply to Linux and Windows. macOS gets the
/// PyPI build, which is the right one there (Metal), matching uv's guide.
pub const PLATFORM_MARKER: &str = "sys_platform == 'linux' or sys_platform == 'win32'";

/// Packages published on the PyTorch index that must come from the same build
/// as `torch`. A CUDA torch next to a PyPI torchvision fails at import time.
const TORCH_FAMILY: [&str; 3] = ["torch", "torchvision", "torchaudio"];

/// Every PyTorch wheel index lives under this prefix, one segment per build
/// (`cu128`, `cpu`, `rocm6.2`, ...).
const PYTORCH_INDEX_PREFIX: &str = "download.pytorch.org/whl/";

/// Prefix of the index names PyPilot writes, so a later run can tell which
/// entries it owns and may replace.
const NAME_PREFIX: &str = "pytorch-";

/// What [`pin_text`] did to a manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinText {
    /// Nothing to pin: not a PyTorch index, or no torch-family package
    /// involved. The caller keeps its existing behaviour.
    NotApplicable,
    /// The manifest already routes the packages to the right index. The
    /// caller must still drop `--index` so uv does not add a duplicate.
    Unchanged,
    /// The new manifest text.
    Changed(String),
}

/// The index name for a PyTorch wheel index URL: `pytorch-cu128` for
/// `https://download.pytorch.org/whl/cu128`. `None` for any other URL, which
/// is left to the existing command-line path.
pub fn index_name(url: &str) -> Option<String> {
    let rest = url.trim().trim_end_matches('/').split_once("://")?.1;
    let tag = rest.strip_prefix(PYTORCH_INDEX_PREFIX)?;
    if tag.is_empty() || tag.contains('/') {
        return None;
    }
    Some(format!("{NAME_PREFIX}{tag}"))
}

/// Rewrite `text` so the torch-family packages resolve from the PyTorch index.
///
/// With `url` set to a PyTorch index, that index becomes the managed one: it
/// is added (or its URL updated), any other `download.pytorch.org` index is
/// removed, and sources pointing at a removed index are retargeted, so a
/// driver upgrade from cu126 to cu128 replaces rather than accumulates.
///
/// With `url` unset (a batch on the default index), torch-family packages in
/// `packages` are still routed when the manifest already has a PyTorch index,
/// so `torchvision` added after a CUDA `torch` lands on the matching build.
///
/// A package whose existing source is not a PyTorch index (git, a path, the
/// user's own mirror) is left alone: that was a deliberate choice.
pub fn pin_text(
    text: &str,
    url: Option<&str>,
    packages: &[String],
) -> Result<PinText, toml_edit::TomlError> {
    let mut doc: DocumentMut = text.parse()?;

    let batch_family: Vec<String> = packages
        .iter()
        .filter_map(|p| requirement_name(p))
        .filter(|n| TORCH_FAMILY.contains(&n.as_str()))
        .collect();

    let (name, removed) = match url {
        Some(url) => {
            let Some(name) = index_name(url) else {
                return Ok(PinText::NotApplicable);
            };
            let removed = upsert_index(&mut doc, &name, url.trim());
            (name, removed)
        }
        None => {
            if batch_family.is_empty() {
                return Ok(PinText::NotApplicable);
            }
            match existing_pytorch_index(&doc) {
                Some(name) => (name, Vec::new()),
                None => return Ok(PinText::NotApplicable),
            }
        }
    };

    let mut route = batch_family;
    for declared in declared_dependencies(&doc) {
        if TORCH_FAMILY.contains(&declared.as_str()) && !route.contains(&declared) {
            route.push(declared);
        }
    }
    if route.is_empty() {
        return Ok(PinText::NotApplicable);
    }

    let sources = sources_table(&mut doc);
    for pkg in &route {
        let replace = match sources.get(pkg) {
            None => true,
            Some(item) => source_index_names(item)
                .is_some_and(|names| names.iter().all(|n| n.starts_with(NAME_PREFIX))),
        };
        if replace {
            sources.insert(pkg, Item::Value(Value::Array(source_value(&name))));
        }
    }
    if !removed.is_empty() {
        retarget_sources(sources, &removed, &name);
    }

    let out = doc.to_string();
    if out == text {
        Ok(PinText::Unchanged)
    } else {
        Ok(PinText::Changed(out))
    }
}

/// Apply [`pin_text`] to the `pyproject.toml` at `path`.
///
/// Returns the original text when the file was rewritten, so the caller can
/// put it back if the install that follows fails; a failed install should not
/// leave the project pointing at an index it never used.
pub fn pin_file(path: &Path, url: Option<&str>, packages: &[String]) -> crate::Result<PinFile> {
    // A dry run that planned `uv init` has no file yet: plan against the
    // empty manifest that step would leave behind.
    let original = match std::fs::read_to_string(path) {
        Err(_) if crate::core::exec::is_fresh(path) => String::new(),
        other => other?,
    };
    Ok(match pin_text(&original, url, packages)? {
        PinText::NotApplicable => PinFile::NotApplicable,
        PinText::Unchanged => PinFile::Unchanged,
        PinText::Changed(new) => {
            let effect = crate::core::exec::Effect::new(
                "record PyTorch index",
                format!(
                    "route {} to {} in [tool.uv], so a later `uv sync` keeps the build matched to this machine",
                    packages.join(", "),
                    url.unwrap_or("the matched index")
                ),
            );
            crate::core::exec::write(path, new, effect)?;
            PinFile::Changed { original }
        }
    })
}

/// What [`pin_file`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinFile {
    NotApplicable,
    Unchanged,
    Changed { original: String },
}

impl PinFile {
    /// Whether the manifest now carries the index, in which case uv must be
    /// run without `--index`: passing it as well adds a second, unnamed entry.
    pub fn is_pinned(&self) -> bool {
        !matches!(self, PinFile::NotApplicable)
    }
}

/// `[{ index = "<name>", marker = "<PLATFORM_MARKER>" }]`
fn source_value(name: &str) -> Array {
    let mut entry = InlineTable::new();
    entry.insert("index", name.into());
    entry.insert("marker", PLATFORM_MARKER.into());
    let mut array = Array::new();
    array.push(Value::InlineTable(entry));
    array
}

/// Is `url` any PyTorch wheel index?
fn is_pytorch_url(url: &str) -> bool {
    index_name(url).is_some()
}

/// Make `name` -> `url` an explicit `[[tool.uv.index]]` entry and drop every
/// other PyTorch index. Returns the names of the dropped entries.
fn upsert_index(doc: &mut DocumentMut, name: &str, url: &str) -> Vec<String> {
    let uv = uv_table(doc);
    let mut removed = Vec::new();

    // Tables are by far the common form, but uv also accepts an inline array
    // (`index = [{ name = ..., url = ... }]`), and rewriting that into tables
    // would move the user's text around for no reason.
    if let Some(array) = uv.get_mut("index").and_then(Item::as_array_mut) {
        array.retain(|v| match v.as_inline_table() {
            Some(t) => keep_index(t, name, &mut removed),
            None => true,
        });
        let existing = array
            .iter_mut()
            .filter_map(Value::as_inline_table_mut)
            .find(|t| entry_name(&**t) == Some(name));
        match existing {
            Some(entry) => set_index_fields(entry, url),
            None => {
                let mut entry = InlineTable::new();
                entry.insert("name", name.into());
                set_index_fields(&mut entry, url);
                array.push(Value::InlineTable(entry));
            }
        }
        return removed;
    }

    if !uv.contains_key("index") {
        uv.insert("index", Item::ArrayOfTables(ArrayOfTables::new()));
    }
    let Some(tables) = uv.get_mut("index").and_then(Item::as_array_of_tables_mut) else {
        // Not a shape uv accepts either; leave it for uv to report.
        return removed;
    };
    tables.retain(|t| keep_index(t, name, &mut removed));
    let existing = tables.iter().position(|t| entry_name(t) == Some(name));
    match existing.and_then(|i| tables.get_mut(i)) {
        Some(entry) => set_index_fields(entry, url),
        None => {
            let mut entry = Table::new();
            entry.insert("name", name.into());
            set_index_fields(&mut entry, url);
            tables.push(entry);
        }
    }
    removed
}

/// Retain predicate for index entries: drop PyTorch indexes other than
/// `keep_name`, recording their names.
fn keep_index(entry: &dyn TableLike, keep_name: &str, removed: &mut Vec<String>) -> bool {
    let url = entry.get("url").and_then(Item::as_str).unwrap_or("");
    let entry_name = entry_name(entry).unwrap_or("");
    if entry_name == keep_name || !is_pytorch_url(url) {
        return true;
    }
    if !entry_name.is_empty() {
        removed.push(entry_name.to_string());
    }
    false
}

fn entry_name(entry: &dyn TableLike) -> Option<&str> {
    entry.get("name").and_then(Item::as_str)
}

/// Set `url` and `explicit = true`, touching each only when it differs so an
/// already-correct entry keeps its exact formatting.
fn set_index_fields(entry: &mut dyn TableLike, url: &str) {
    if entry.get("url").and_then(Item::as_str) != Some(url) {
        entry.insert("url", url.into());
    }
    if entry.get("explicit").and_then(Item::as_bool) != Some(true) {
        entry.insert("explicit", true.into());
    }
}

/// The name of the first named PyTorch index already in the manifest.
fn existing_pytorch_index(doc: &DocumentMut) -> Option<String> {
    let index = doc.get("tool")?.get("uv")?.get("index")?;
    let found = |t: &dyn TableLike| {
        let url = t.get("url").and_then(Item::as_str)?;
        is_pytorch_url(url).then(|| entry_name(t).map(str::to_string))?
    };
    if let Some(tables) = index.as_array_of_tables() {
        return tables.iter().find_map(|t| found(t));
    }
    index
        .as_array()?
        .iter()
        .filter_map(Value::as_inline_table)
        .find_map(|t| found(t))
}

/// The index names a source entry points at, or `None` when any part of it is
/// not an index source (git, path, url, editable...).
fn source_index_names(item: &Item) -> Option<Vec<String>> {
    let one = |t: &dyn TableLike| t.get("index").and_then(Item::as_str).map(str::to_string);
    if let Some(t) = item.as_table_like() {
        return one(t).map(|n| vec![n]);
    }
    item.as_array()?
        .iter()
        .map(|v| v.as_inline_table().and_then(|t| one(t)))
        .collect()
}

/// Point every source that referenced a removed index at `name` instead, so
/// a package the user routed to `pytorch-cu126` by hand follows the upgrade
/// rather than referencing an index that no longer exists.
fn retarget_sources(sources: &mut dyn TableLike, removed: &[String], name: &str) {
    let retarget = |t: &mut dyn TableLike| {
        let stale = t
            .get("index")
            .and_then(Item::as_str)
            .is_some_and(|i| removed.iter().any(|r| r == i));
        if stale {
            t.insert("index", name.into());
        }
    };
    for (_, item) in sources.iter_mut() {
        if let Some(array) = item.as_array_mut() {
            for v in array.iter_mut() {
                if let Some(t) = v.as_inline_table_mut() {
                    retarget(t);
                }
            }
        } else if let Some(t) = item.as_table_like_mut() {
            retarget(t);
        }
    }
}

/// Normalised names from `[project.dependencies]`,
/// `[project.optional-dependencies]` and `[dependency-groups]`, so torchvision
/// already in the manifest follows torch onto the new index.
fn declared_dependencies(doc: &DocumentMut) -> Vec<String> {
    let mut lists: Vec<&Array> = Vec::new();
    if let Some(project) = doc.get("project") {
        if let Some(deps) = project.get("dependencies").and_then(Item::as_array) {
            lists.push(deps);
        }
        if let Some(extras) = project
            .get("optional-dependencies")
            .and_then(Item::as_table_like)
        {
            lists.extend(extras.iter().filter_map(|(_, i)| i.as_array()));
        }
    }
    if let Some(groups) = doc.get("dependency-groups").and_then(Item::as_table_like) {
        lists.extend(groups.iter().filter_map(|(_, i)| i.as_array()));
    }
    lists
        .into_iter()
        .flat_map(|a| a.iter())
        .filter_map(Value::as_str)
        .filter_map(requirement_name)
        .collect()
}

/// `[tool.uv]`, created as implicit tables so a manifest without one gains
/// only the subtables actually written (`[tool.uv.sources]`), not empty
/// `[tool]` and `[tool.uv]` headers.
fn uv_table(doc: &mut DocumentMut) -> &mut dyn TableLike {
    let tool = implicit_child(doc.as_table_mut(), "tool");
    implicit_child(tool, "uv")
}

fn implicit_child<'a>(parent: &'a mut dyn TableLike, key: &str) -> &'a mut dyn TableLike {
    if !parent.get(key).is_some_and(|i| i.is_table_like()) {
        let mut table = Table::new();
        table.set_implicit(true);
        parent.insert(key, Item::Table(table));
    }
    parent
        .get_mut(key)
        .and_then(Item::as_table_like_mut)
        .expect("just ensured a table")
}

fn sources_table(doc: &mut DocumentMut) -> &mut dyn TableLike {
    let uv = uv_table(doc);
    if !uv.get("sources").is_some_and(|i| i.is_table_like()) {
        uv.insert("sources", Item::Table(Table::new()));
    }
    uv.get_mut("sources")
        .and_then(Item::as_table_like_mut)
        .expect("just ensured a table")
}

#[cfg(test)]
mod tests {
    use super::*;

    const CU128: &str = "https://download.pytorch.org/whl/cu128";
    const CU126: &str = "https://download.pytorch.org/whl/cu126";

    fn pkgs(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn changed(text: &str, url: Option<&str>, packages: &[&str]) -> String {
        match pin_text(text, url, &pkgs(packages)).unwrap() {
            PinText::Changed(out) => out,
            other => panic!("expected a change, got {other:?}"),
        }
    }

    const BASIC: &str = r#"# Project manifest, hand-written.
[project]
name = "demo"
version = "0.1.0"
dependencies = [
    "numpy>=2", # keep me
]

[tool.ruff]
line-length = 100 # house style
"#;

    #[test]
    fn index_names_come_from_the_build_segment() {
        assert_eq!(index_name(CU128).as_deref(), Some("pytorch-cu128"));
        assert_eq!(
            index_name("https://download.pytorch.org/whl/cpu/").as_deref(),
            Some("pytorch-cpu")
        );
        assert_eq!(index_name("https://pypi.org/simple"), None);
        assert_eq!(index_name("https://download.pytorch.org/whl/"), None);
    }

    #[test]
    fn writes_explicit_index_and_marked_source_keeping_comments() {
        let out = changed(BASIC, Some(CU128), &["torch"]);
        assert_eq!(
            out,
            r#"# Project manifest, hand-written.
[project]
name = "demo"
version = "0.1.0"
dependencies = [
    "numpy>=2", # keep me
]

[tool.ruff]
line-length = 100 # house style

[[tool.uv.index]]
name = "pytorch-cu128"
url = "https://download.pytorch.org/whl/cu128"
explicit = true

[tool.uv.sources]
torch = [{ index = "pytorch-cu128", marker = "sys_platform == 'linux' or sys_platform == 'win32'" }]
"#
        );
        // The result is a manifest `toml` (and so uv) can still read.
        let parsed: toml::Value = toml::from_str(&out).unwrap();
        assert_eq!(
            parsed["tool"]["uv"]["index"][0]["explicit"].as_bool(),
            Some(true)
        );
    }

    #[test]
    fn rerunning_is_a_no_op() {
        let once = changed(BASIC, Some(CU128), &["torch"]);
        assert_eq!(
            pin_text(&once, Some(CU128), &pkgs(&["torch"])).unwrap(),
            PinText::Unchanged
        );
    }

    #[test]
    fn a_new_cuda_version_replaces_the_old_index() {
        let old = changed(BASIC, Some(CU126), &["torch", "torchvision"]);
        let new = changed(&old, Some(CU128), &["torch"]);
        assert!(!new.contains("cu126"), "{new}");
        assert_eq!(new.matches("[[tool.uv.index]]").count(), 1, "{new}");
        // torchvision was routed before, so it follows torch.
        assert!(
            new.contains(r#"torchvision = [{ index = "pytorch-cu128""#),
            "{new}"
        );
        assert!(new.contains("# keep me") && new.contains("# house style"));
    }

    #[test]
    fn declared_companions_are_routed_too() {
        let text = BASIC.replace(
            r#""numpy>=2", # keep me"#,
            r#""numpy>=2", # keep me
    "torchaudio","#,
        );
        let out = changed(&text, Some(CU128), &["torch==2.7.1"]);
        assert!(
            out.contains(r#"torch = [{ index = "pytorch-cu128""#),
            "{out}"
        );
        assert!(
            out.contains(r#"torchaudio = [{ index = "pytorch-cu128""#),
            "{out}"
        );
        assert!(!out.contains("numpy ="), "{out}");
    }

    #[test]
    fn a_companion_added_later_follows_the_existing_index() {
        let pinned = changed(BASIC, Some(CU128), &["torch"]);
        let out = changed(&pinned, None, &["torchvision", "pillow"]);
        assert!(
            out.contains(r#"torchvision = [{ index = "pytorch-cu128""#),
            "{out}"
        );
        assert!(!out.contains("pillow ="), "{out}");
    }

    #[test]
    fn nothing_to_do_without_torch_or_a_pytorch_index() {
        assert_eq!(
            pin_text(BASIC, None, &pkgs(&["torch"])).unwrap(),
            PinText::NotApplicable
        );
        assert_eq!(
            pin_text(BASIC, None, &pkgs(&["numpy"])).unwrap(),
            PinText::NotApplicable
        );
        assert_eq!(
            pin_text(BASIC, Some("https://example.com/simple"), &pkgs(&["torch"])).unwrap(),
            PinText::NotApplicable
        );
    }

    #[test]
    fn user_chosen_sources_are_respected() {
        let text = format!(
            "{BASIC}\n[tool.uv.sources]\ntorch = {{ git = \"https://github.com/pytorch/pytorch\" }} # mine\n"
        );
        let out = changed(&text, Some(CU128), &["torch"]);
        assert!(
            out.contains(r#"torch = { git = "https://github.com/pytorch/pytorch" } # mine"#),
            "{out}"
        );
    }

    #[test]
    fn other_indexes_and_their_sources_are_kept() {
        let text = format!(
            r#"{BASIC}
[tool.uv.sources]
internal-lib = {{ index = "corp" }}
xformers = {{ index = "pytorch-cu126" }}

[[tool.uv.index]]
name = "corp" # our mirror
url = "https://pypi.corp.example/simple"

[[tool.uv.index]]
name = "pytorch-cu126"
url = "https://download.pytorch.org/whl/cu126"
explicit = true
"#
        );
        let out = changed(&text, Some(CU128), &["torch"]);
        assert!(out.contains(r#"name = "corp" # our mirror"#), "{out}");
        assert!(
            out.contains(r#"internal-lib = { index = "corp" }"#),
            "{out}"
        );
        // Hand-routed to the old PyTorch index: retargeted, not dangling.
        assert!(
            out.contains(r#"xformers = { index = "pytorch-cu128" }"#),
            "{out}"
        );
        assert!(!out.contains("cu126"), "{out}");
    }

    #[test]
    fn an_existing_entry_gains_explicit_in_place() {
        let text =
            format!("{BASIC}\n[[tool.uv.index]]\nname = \"pytorch-cu128\"\nurl = \"{CU128}\"\n");
        let out = changed(&text, Some(CU128), &["torch"]);
        assert_eq!(out.matches("[[tool.uv.index]]").count(), 1, "{out}");
        assert!(out.contains("explicit = true"), "{out}");
    }

    #[test]
    fn inline_index_arrays_are_edited_in_place() {
        let text = format!(
            "{BASIC}\n[tool.uv]\nindex = [{{ name = \"pytorch-cu126\", url = \"{CU126}\", explicit = true }}]\n"
        );
        let out = changed(&text, Some(CU128), &["torch"]);
        let parsed: toml::Value = toml::from_str(&out).unwrap();
        let index = parsed["tool"]["uv"]["index"].as_array().unwrap();
        assert_eq!(index.len(), 1, "{out}");
        assert_eq!(index[0]["name"].as_str(), Some("pytorch-cu128"));
        assert_eq!(index[0]["url"].as_str(), Some(CU128));
    }

    #[test]
    fn unparsable_manifests_are_reported_not_clobbered() {
        assert!(pin_text("[project\nname=", Some(CU128), &pkgs(&["torch"])).is_err());
    }
}
