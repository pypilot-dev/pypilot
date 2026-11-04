//! Known breakages: what package metadata cannot see.
//!
//! uv refuses pins that contradict declared metadata, so the breakages that
//! survive resolution are the undeclared ones: an old pandas with no numpy cap
//! next to numpy 2, a Flask with no Werkzeug cap next to Werkzeug 3, jwt and
//! PyJWT writing the same `jwt/` directory, an enum34 backport shadowing the
//! standard library, source importing `imghdr` on Python 3.13, `import
//! pkg_resources` with setuptools 82, a pinned release PyPI has yanked.
//!
//! The facts live in `data/known_breakages.json` ([`table`]), which installed
//! engines refresh from `main` like the other tables. [`rules`] turns them and
//! a [`rules::Context`] (declared, resolved and installed versions, the
//! project's imports, the Python) into findings that name the culprit and
//! carry a fix; [`yanked`] asks PyPI about pins. [`assess`] is the one hook
//! the solver calls.
//!
//! # Fixes, for the `doctor --fix` planner
//!
//! A constraint (`numpy<2`, `setuptools<82`, `standard-imghdr`,
//! `pandas>=2.2.2`, `requests==2.32.2`) is a requirement edit, built by
//! [`rules::Context::constraint_fix`]:
//!
//! - the project declares that name: [`FixKind::EditRequirement`] rewrites
//!   that one requirement where it is written (marker and extras kept);
//! - it does not: an `EditRequirement` with an empty `old` adds it to
//!   pyproject.toml's `[project] dependencies` or requirements.txt;
//! - no manifest at all: [`FixKind::AddRequirements`] installs it;
//! - the manifest already says it and only the venv lags:
//!   [`FixKind::SyncEnvironment`].
//!
//! The planner applies edits first, assesses again and syncs, so the venv
//! keeps its Python. The one fix of our own is [`FixKind::ReplaceDist`]:
//! drop strays from the manifest, uninstall them, and (re)install the right
//! distribution (jwt next to PyJWT, enum34, sklearn). It runs through
//! [`apply::apply`]: `apply(workspace, settings, &fix)` returns the steps
//! it ran, every mutation goes through [`crate::core::exec`] so it plans
//! correctly under [`crate::core::exec::dry_run`], and it never changes the
//! venv's Python. Other fix kinds return an error. [`crate::core::repair::plan`] collects
//! them, so `pypilot doctor --fix` runs them after its rebuild or sync.

pub mod apply;
pub mod rules;
pub mod table;
pub mod yanked;

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use serde::Serialize;

use crate::core::installed;
use crate::core::project::{self, normalize_name, ProjectDeps};
use crate::core::resolution::{Outcome, Resolver};
use crate::core::Finding;
use crate::pypi::pyversion::PyVersion;
use crate::pypi::{CompatReport, MetadataSource};

use crate::core::FixKind;

/// [`FixKind::ReplaceDist`]'s payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Replacement {
    /// Installed distributions to uninstall.
    pub uninstall: Vec<String>,
    /// Distributions to install (or reinstall) afterwards.
    pub install: Vec<String>,
    /// Declared dependencies to remove from the manifest.
    pub drop: Vec<String>,
}

/// Source files read for imports, at most.
const MAX_SOURCE_FILES: usize = 400;
/// Directory entries walked for them, at most.
const MAX_SOURCE_ENTRIES: usize = 20_000;

/// Every known-breakage finding for a workspace: the table's rules over the
/// project, the venv and the source, plus yanked pins.
///
/// `venv` is the project venv and its Python; `target` is the Python setup
/// would pick, used when there is no venv. Read-only: file reads, cached
/// PyPI lookups for pinned versions, and with a `resolver` the resolves
/// that check a requirement edit before it is offered
/// ([`verify_alternatives`]).
pub async fn assess<S: MetadataSource, R: Resolver>(
    workspace: &Path,
    project: &ProjectDeps,
    compat: Option<&CompatReport>,
    venv: Option<(&Path, Option<PyVersion>)>,
    target: Option<PyVersion>,
    source: &S,
    resolver: Option<&R>,
) -> Vec<Finding> {
    if !project.is_python_project() {
        return Vec::new();
    }
    let ctx = context(workspace, project, compat, venv, target);
    let mut findings = rules::evaluate(table::table(), &ctx);
    if let Some(resolver) = resolver {
        let alternatives = rules::alternatives(table::table(), &ctx);
        verify_alternatives(workspace, &alternatives, &mut findings, resolver).await;
    }
    findings.extend(yanked::check(workspace, project, source, &ctx).await);
    findings
}

/// A finding's requirement edit must leave the project resolvable: an
/// upgrade of a pinned package next to other pins often does not (flask
/// 1.1.2 to 2.x with jinja2==2.11.3 pinned). When uv finds no solution
/// with the edit, try the finding's [`rules::Alternative`]s in order and
/// take the first that resolves. When uv cannot say (offline), or nothing
/// resolves, the finding keeps its fix; the planner checks again after
/// applying it and rolls back an edit that leaves the project unresolvable.
pub async fn verify_alternatives<R: Resolver>(
    workspace: &Path,
    alternatives: &[rules::Alternative],
    findings: &mut [Finding],
    resolver: &R,
) {
    if alternatives.is_empty() {
        return;
    }
    let declared = crate::core::req_edit::declared(workspace);
    if declared.custom_resolution {
        return;
    }
    let lines: Vec<String> = declared.reqs.iter().map(|r| r.raw.clone()).collect();
    for f in findings.iter_mut() {
        let FixKind::EditRequirement(edit) = &f.fix else {
            continue;
        };
        let mine: Vec<&rules::Alternative> =
            alternatives.iter().filter(|a| a.title == f.title).collect();
        if mine.is_empty()
            || resolves_with(resolver, &declared.options, &lines, edit).await != Some(false)
        {
            continue;
        }
        for alt in mine {
            let FixKind::EditRequirement(e) = &alt.fix else {
                continue;
            };
            if resolves_with(resolver, &declared.options, &lines, e).await == Some(true) {
                f.detail = f.detail.replace(&alt.replaces, &alt.sentence);
                f.fix = alt.fix.clone();
                break;
            }
        }
    }
}

/// Does the project resolve with `edit` applied? `None` when uv cannot say.
async fn resolves_with<R: Resolver>(
    resolver: &R,
    options: &[String],
    lines: &[String],
    edit: &crate::core::RequirementEdit,
) -> Option<bool> {
    let mut input = options.to_vec();
    if edit.old.trim().is_empty() {
        input.extend(lines.iter().cloned());
        input.push(edit.new.clone());
    } else {
        input.extend(lines.iter().map(|l| {
            if *l == edit.old {
                edit.new.clone()
            } else {
                l.clone()
            }
        }));
    }
    match resolver.resolve(&input).await? {
        Outcome::Resolved(_) => Some(true),
        Outcome::NoSolution(_) => Some(false),
    }
}

/// Gather the [`rules::Context`] for a workspace.
pub fn context(
    workspace: &Path,
    project: &ProjectDeps,
    compat: Option<&CompatReport>,
    venv: Option<(&Path, Option<PyVersion>)>,
    target: Option<PyVersion>,
) -> rules::Context {
    let resolved = compat
        .map(|c| {
            c.per_package
                .iter()
                .map(|a| (normalize_name(&a.name), a.version.clone()))
                .collect()
        })
        .unwrap_or_default();
    let (installed, site_modules, declared_conflicts) = match venv {
        Some((path, python)) => {
            let found = installed::scan(path);
            let conflicts = crate::core::depcheck::conflicts(path, &found, python)
                .into_iter()
                .map(|c| (c.package, c.dependency))
                .collect();
            (Some(found.versions), site_modules(path), conflicts)
        }
        None => (None, HashSet::new(), HashSet::new()),
    };
    let venv_python = venv.and_then(|(_, p)| p);
    let source = scan_source(workspace, &table::table().symbol_targets());
    rules::Context {
        declared_raw: crate::core::req_edit::declared(workspace).reqs,
        new_requirement_file: new_requirement_file(workspace),
        has_lock: workspace.join("uv.lock").is_file(),
        declared: project.packages.clone(),
        with_marker: declared_with_marker(workspace),
        resolved,
        installed,
        site_modules,
        python: venv_python.or(target),
        python_is_venv: venv_python.is_some(),
        imports: source.imports,
        symbol_uses: source.symbol_uses,
        requires_python: project.declared_requires_python.clone(),
        python_pin: crate::core::probe::read_python_pin(workspace),
        declared_conflicts,
        pixi: project.is_pixi(),
    }
}

/// Where a requirement the project does not declare yet goes: pyproject.toml
/// when it has a `[project]` table, else requirements.txt when there is one.
pub fn new_requirement_file(workspace: &Path) -> Option<std::path::PathBuf> {
    let pyproject = workspace.join("pyproject.toml");
    let has_project = crate::core::project::read_manifest(&pyproject)
        .and_then(|t| t.parse::<toml::Table>().ok())
        .is_some_and(|d| d.get("project").is_some_and(|p| p.is_table()));
    if has_project {
        return Some(pyproject);
    }
    let requirements = workspace.join("requirements.txt");
    requirements.is_file().then_some(requirements)
}

/// Top-level importable names in a venv's site-packages: package
/// directories and single-file modules.
pub fn site_modules(venv: &Path) -> HashSet<String> {
    let mut out = HashSet::new();
    for dir in installed::site_packages_dirs(venv) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            if path.is_dir() {
                if !name.contains('.') && !name.starts_with('_') || name == "_distutils_hack" {
                    out.insert(name);
                }
            } else if let Some(stem) = name.strip_suffix(".py") {
                out.insert(stem.to_string());
            }
        }
    }
    out
}

/// Normalized names of dependencies written with an environment marker,
/// read off the manifests' text. Their specifier may not apply here, so the
/// rules judge them only by what is installed.
pub fn declared_with_marker(workspace: &Path) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut consider = |req: &str| {
        if req.contains(';') {
            if let Some(name) = project::requirement_name(req) {
                out.insert(normalize_name(&name));
            }
        }
    };
    if let Some(text) = crate::core::project::read_manifest(&workspace.join("requirements.txt")) {
        text.lines().for_each(&mut consider);
    }
    if let Some(text) = crate::core::project::read_manifest(&workspace.join("pyproject.toml")) {
        if let Ok(doc) = text.parse::<toml::Table>() {
            for req in pyproject_dependencies(&doc) {
                consider(&req);
            }
        }
    }
    out
}

/// `[project] dependencies` and every `[project.optional-dependencies]` list.
pub fn pyproject_dependencies(doc: &toml::Table) -> Vec<String> {
    let Some(project) = doc.get("project").and_then(|p| p.as_table()) else {
        return Vec::new();
    };
    let strings = |v: &toml::Value| -> Vec<String> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut out = project.get("dependencies").map(strings).unwrap_or_default();
    if let Some(extras) = project
        .get("optional-dependencies")
        .and_then(|e| e.as_table())
    {
        for list in extras.values() {
            out.extend(strings(list));
        }
    }
    out
}

/// Top-level modules the project's own `.py` files and notebooks import,
/// each with the first file (relative, `/`-separated) that imports it. The
/// project's own modules are left out.
pub fn project_imports(workspace: &Path) -> BTreeMap<String, String> {
    scan_source(workspace, &[]).imports
}

/// What [`scan_source`] found in the project's source.
#[derive(Debug, Default)]
pub struct SourceScan {
    /// See [`project_imports`].
    pub imports: BTreeMap<String, String>,
    /// Uses of the `(module, symbol)` targets, in file order.
    pub symbol_uses: Vec<rules::CodeUse>,
}

/// One walk over the project's `.py` files and notebooks: the modules they
/// import, and where they use one of `targets` (`numpy.float`).
pub fn scan_source(workspace: &Path, targets: &[(String, String)]) -> SourceScan {
    let mut files = Vec::new();
    let mut budget = MAX_SOURCE_ENTRIES;
    source_files(workspace, &mut files, &mut budget);
    let local = local_modules(workspace);
    let mut out = BTreeMap::new();
    let mut uses = Vec::new();
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let source = if file.extension().is_some_and(|e| e == "ipynb") {
            match notebook_code(&text) {
                Some(code) => crate::core::imports::mask_ipython(&code),
                None => continue,
            }
        } else {
            text
        };
        let rel = file
            .strip_prefix(workspace)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");
        let imports = crate::core::imports::extract(&source);
        let wanted: Vec<(String, String)> = targets
            .iter()
            .filter(|(m, _)| {
                let top = m.split('.').next().unwrap_or(m);
                !local.contains(top) && imports.iter().any(|i| i.module == top)
            })
            .cloned()
            .collect();
        for u in crate::core::imports::symbol_uses(&source, &wanted) {
            uses.push(rules::CodeUse {
                module: u.module,
                symbol: u.symbol,
                file: rel.clone(),
                line: u.line,
                written: u.written,
                from_import: u.from_import,
            });
        }
        for import in imports {
            if local.contains(&import.module) {
                continue;
            }
            out.entry(import.module).or_insert_with(|| rel.clone());
        }
    }
    SourceScan {
        imports: out,
        symbol_uses: uses,
    }
}

/// Directories never searched for project source.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "__pycache__",
    "site-packages",
    "build",
    "dist",
    "target",
];

/// `.py` files and notebooks under `dir`, files first, skipping hidden
/// directories, virtualenvs and build output, within the caps.
fn source_files(dir: &Path, out: &mut Vec<std::path::PathBuf>, budget: &mut usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<std::fs::DirEntry> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    let mut subdirs = Vec::new();
    for entry in entries {
        if out.len() >= MAX_SOURCE_FILES || *budget == 0 {
            return;
        }
        *budget -= 1;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if !name.starts_with('.')
                && !SKIP_DIRS.contains(&name.as_str())
                && !path.join("pyvenv.cfg").is_file()
            {
                subdirs.push(path);
            }
        } else if name.ends_with(".py") || name.ends_with(".ipynb") {
            out.push(path);
        }
    }
    for sub in subdirs {
        source_files(&sub, out, budget);
    }
}

/// The code cells of a Python notebook, joined. `None` for anything else.
fn notebook_code(json: &str) -> Option<String> {
    let nb: serde_json::Value = serde_json::from_str(json).ok()?;
    let language = nb
        .pointer("/metadata/kernelspec/language")
        .and_then(|l| l.as_str());
    if language.is_some_and(|l| !l.eq_ignore_ascii_case("python")) {
        return None;
    }
    let mut code = String::new();
    for cell in nb.get("cells")?.as_array()? {
        if cell.get("cell_type").and_then(|t| t.as_str()) != Some("code") {
            continue;
        }
        match cell.get("source") {
            Some(serde_json::Value::String(s)) => code.push_str(s),
            Some(serde_json::Value::Array(lines)) => {
                for l in lines.iter().filter_map(|l| l.as_str()) {
                    code.push_str(l);
                }
            }
            _ => {}
        }
        code.push('\n');
    }
    Some(code)
}

/// Module names the project defines at its root or under `src/`.
fn local_modules(workspace: &Path) -> HashSet<String> {
    let mut names = HashSet::new();
    for root in [workspace.to_path_buf(), workspace.join("src")] {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.join("__init__.py").is_file() {
                    names.insert(entry.file_name().to_string_lossy().into_owned());
                }
            } else if path.extension().is_some_and(|e| e == "py") {
                if let Some(stem) = path.file_stem() {
                    names.insert(stem.to_string_lossy().into_owned());
                }
            }
        }
    }
    names
}

/// For the LSP guardian: the requirement to offer for an import of `module`
/// instead of its bare distribution, when the table knows newer releases of
/// that distribution dropped it (`pkg_resources` gives `setuptools<82`).
/// `None` for everything else, and when the installed release still has it.
pub fn install_requirement_for(
    module: &str,
    installed: &crate::core::installed::Installed,
) -> Option<String> {
    rules::install_requirement_for(table::table(), module, &installed.versions)
}

#[cfg(test)]
mod tests;
