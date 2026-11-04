//! Package-level health of a project's environment, read off disk.
//!
//! The solver judges the interpreter; this judges what is installed in the
//! environment against what the project declares:
//!
//! - `uv.lock` out of date with `pyproject.toml` ([`FixKind::Relock`]),
//! - installed versions that drifted from `uv.lock`, and declared packages
//!   that are not installed ([`FixKind::SyncEnvironment`]),
//! - half-installed distributions: files listed in `RECORD` gone, or a
//!   package's files present without its `.dist-info`
//!   ([`FixKind::ReinstallPackages`]),
//! - a venv that sees the base Python's site-packages, and a venv directory
//!   whose `pyvenv.cfg` is gone ([`FixKind::RepairEnvironment`]),
//! - `VIRTUAL_ENV` pointing at another project's environment (advice only).
//!
//! Everything is file reads (no process, no network), so it is cheap enough
//! for every scan. pixi projects are skipped: pixi owns their environments
//! and locks. Lock checks are skipped when `package_manager` is `pip` or
//! `uv-pip`, which never use `uv.lock`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::core::installed::{self, Installed};
use crate::core::probe::VenvInfo;
use crate::core::project::normalize_name;
use crate::core::{Finding, FixKind, Severity};
use crate::settings::PackageManager;

/// All the package-level findings for `workspace`, whose environment is
/// `venv` (as probed). Empty for a pixi project.
pub fn check(
    workspace: &Path,
    venv: Option<&VenvInfo>,
    package_manager: PackageManager,
    pixi: bool,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    if pixi {
        return findings;
    }
    findings.extend(foreign_virtual_env(workspace, venv));
    let Some(venv) = venv.filter(|v| v.broken.is_none() && !v.kind.is_pixi()) else {
        return findings;
    };
    let name = env_label(workspace, &venv.path);
    let installed = installed::scan(&venv.path);
    let declared = declared_packages(workspace);

    if system_site_packages(&venv.path) {
        let on = venv
            .python
            .map(|p| format!(" on Python {p}"))
            .unwrap_or_default();
        findings.push(Finding {
            severity: Severity::Warning,
            title: format!("{name} can see the system site-packages"),
            detail: format!(
                "{name}/pyvenv.cfg has `include-system-site-packages = true`, so packages installed in the base Python leak into the project: imports can work here and fail everywhere else, and a missing dependency goes unnoticed. Rebuild the environment without it{on} (leave it if you set this on purpose)."
            ),
            fix: FixKind::RepairEnvironment {
                python: venv.python,
            },
        });
    }

    let sites = installed::site_packages_dirs(&venv.path);
    let damaged = damaged_distributions(&sites);
    if !damaged.is_empty() {
        findings.push(Finding {
            severity: Severity::Error,
            title: format!("Damaged packages in {name}: {}", damaged.join(", ")),
            detail: format!(
                "Files these packages installed are gone from site-packages, although their metadata says they are installed, so importing them fails and `uv sync` alone would not notice. Reinstall {}.",
                damaged.join(", ")
            ),
            fix: FixKind::ReinstallPackages(damaged.clone()),
        });
    }

    let (orphaned, missing): (Vec<String>, Vec<String>) = declared
        .names
        .iter()
        .filter(|n| !installed.contains(n))
        .cloned()
        .partition(|n| has_files_without_metadata(&sites, n));
    if !orphaned.is_empty() {
        findings.push(Finding {
            severity: Severity::Warning,
            title: format!("Half-installed packages in {name}: {}", orphaned.join(", ")),
            detail: format!(
                "The files of {} are in site-packages but their .dist-info metadata is gone, so installers think they are missing and tools that read metadata fail. Reinstall them.",
                orphaned.join(", ")
            ),
            fix: FixKind::ReinstallPackages(orphaned),
        });
    }
    if !missing.is_empty() {
        findings.push(Finding {
            severity: Severity::Warning,
            title: format!("Declared packages are not installed: {}", missing.join(", ")),
            detail: format!(
                "{} declares {}, but {name} does not have {}, so importing {} fails. Install the declared dependencies into the existing environment.",
                declared.source,
                missing.join(", "),
                if missing.len() == 1 { "it" } else { "them" },
                if missing.len() == 1 { "it" } else { "them" },
            ),
            fix: FixKind::SyncEnvironment,
        });
    }

    if !uses_lock(package_manager) {
        return findings;
    }
    let Some(lock) = read_lock(workspace) else {
        return findings;
    };
    let changes = lock_changes(workspace, &lock);
    if !changes.is_empty() {
        findings.push(Finding {
            severity: Severity::Warning,
            title: "uv.lock is out of date with pyproject.toml".to_string(),
            detail: format!(
                "pyproject.toml changed after uv.lock was written ({}), so `uv sync --locked` refuses and the environment does not have what pyproject.toml asks for. Update the lock (`uv lock`), then sync.",
                changes.join("; ")
            ),
            fix: FixKind::Relock,
        });
        // Drift is measured against a lock that is about to change.
        return findings;
    }
    let drift = drift(&lock, &installed);
    if !drift.is_empty() {
        findings.push(Finding {
            severity: Severity::Warning,
            title: format!("{name} differs from uv.lock"),
            detail: format!(
                "Installed versions do not match the lock: {}. Something installed over the locked versions (`pip install`, `uv pip install`). Sync the environment back to uv.lock.",
                drift.join(", ")
            ),
            fix: FixKind::SyncEnvironment,
        });
    }
    findings
}

/// Whether this package manager setting works from `uv.lock`.
pub fn uses_lock(pm: PackageManager) -> bool {
    matches!(pm, PackageManager::Auto | PackageManager::Uv)
}

/// How text names the environment at `path`: `.venv`, `venv`, `env`.
pub fn env_label(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

/// A venv directory (`.venv`, `venv`, `env`) that is no longer one: its
/// `pyvenv.cfg` is gone while its `Scripts`/`bin` or `Lib`/`lib` remain.
/// Python then runs without the venv's site-packages, and `uv venv --clear`
/// refuses the directory, so it has to be removed and rebuilt.
pub fn damaged_venv(workspace: &Path) -> Option<PathBuf> {
    crate::core::env::VENV_DIRS.iter().find_map(|name| {
        let dir = workspace.join(name);
        if !dir.is_dir() || dir.join("pyvenv.cfg").exists() {
            return None;
        }
        ["Scripts", "bin", "Lib", "lib"]
            .iter()
            .any(|sub| dir.join(sub).is_dir())
            .then_some(dir)
    })
}

/// `include-system-site-packages = true` in the venv's `pyvenv.cfg`.
pub fn system_site_packages(venv: &Path) -> bool {
    std::fs::read_to_string(venv.join("pyvenv.cfg"))
        .ok()
        .is_some_and(|text| {
            text.lines().any(|line| {
                line.split_once('=').is_some_and(|(k, v)| {
                    k.trim() == "include-system-site-packages"
                        && v.trim().eq_ignore_ascii_case("true")
                })
            })
        })
}

/// `VIRTUAL_ENV` naming an environment that is not this project's, as an
/// Info finding: a terminal activated on another project runs its Python.
fn foreign_virtual_env(workspace: &Path, venv: Option<&VenvInfo>) -> Option<Finding> {
    let active = std::env::var_os("VIRTUAL_ENV").filter(|v| !v.is_empty())?;
    let active = PathBuf::from(active);
    let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    };
    if venv.is_some_and(|v| same(&v.path, &active)) {
        return None;
    }
    let inside = match (active.canonicalize(), workspace.canonicalize()) {
        (Ok(a), Ok(w)) => a.starts_with(w),
        _ => active.starts_with(workspace),
    };
    if inside {
        return None;
    }
    Some(Finding {
        severity: Severity::Info,
        title: "VIRTUAL_ENV points outside this project".to_string(),
        detail: format!(
            "The shell has {} activated, which is not this project's environment, so `python` and `pip` typed here use it. Run `deactivate`, use `uv run`, or open a new terminal. PyPilot itself checks the project's own environment.",
            active.display()
        ),
        fix: FixKind::Manual,
    })
}

/// The project's direct dependencies, normalized, and the file they came
/// from. Requirements with an environment marker are left out: whether they
/// belong here depends on the marker, and a false "missing" is worse than a
/// missed one.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Declared {
    pub source: &'static str,
    pub names: Vec<String>,
}

pub fn declared_packages(workspace: &Path) -> Declared {
    let pyproject = crate::core::project::read_manifest(&workspace.join("pyproject.toml"))
        .and_then(|t| t.parse::<toml::Table>().ok());
    let project_name = pyproject
        .as_ref()
        .and_then(|t| t.get("project")?.get("name")?.as_str().map(normalize_name));
    let from_pyproject: Vec<String> = pyproject
        .as_ref()
        .and_then(|t| t.get("project")?.get("dependencies")?.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|v| v.as_str())
        .filter_map(unconditional_name)
        .collect();
    let (source, names) = if !from_pyproject.is_empty() {
        ("pyproject.toml", from_pyproject)
    } else {
        let text = crate::core::project::read_manifest(&workspace.join("requirements.txt"))
            .unwrap_or_default();
        (
            "requirements.txt",
            text.lines().filter_map(unconditional_name).collect(),
        )
    };
    let mut seen = BTreeSet::new();
    let names = names
        .into_iter()
        .filter(|n| Some(n) != project_name.as_ref() && seen.insert(n.clone()))
        .collect();
    Declared { source, names }
}

/// The normalized name of a requirement line without an environment marker.
fn unconditional_name(line: &str) -> Option<String> {
    let line = line.split(" #").next().unwrap_or(line);
    if line.contains(';') {
        return None;
    }
    crate::core::project::requirement_name(line).map(|n| normalize_name(&n))
}

/// Distributions whose `RECORD` lists top-level files or directories that
/// are gone, by normalized name, sorted.
pub fn damaged_distributions(sites: &[PathBuf]) -> Vec<String> {
    let mut out = BTreeSet::new();
    for site in sites {
        let Ok(entries) = std::fs::read_dir(site) else {
            continue;
        };
        for entry in entries.flatten() {
            let file = entry.file_name();
            let file = file.to_string_lossy();
            if !file.ends_with(".dist-info") {
                continue;
            }
            let Some(name) = installed::distribution_name(&file) else {
                continue;
            };
            let record = entry.path().join("RECORD");
            // A huge RECORD (thousands of files) is read once per scan; skip
            // anything absurd rather than stall the editor.
            if std::fs::metadata(&record).map_or(true, |m| m.len() > 16 << 20) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&record) else {
                continue;
            };
            if record_tops(&text)
                .iter()
                .any(|top| !site.join(top).exists())
            {
                out.insert(name);
            }
        }
    }
    out.into_iter().collect()
}

/// The top-level entries a `RECORD` installs into site-packages: the first
/// path component of each row, leaving out the metadata directory, caches
/// and anything outside site-packages (`../../Scripts/x.exe`).
fn record_tops(record: &str) -> BTreeSet<String> {
    record
        .lines()
        .filter_map(|line| {
            let path = match line.strip_prefix('"') {
                Some(rest) => rest.split('"').next()?,
                None => line.split(',').next()?,
            };
            let path = path.trim();
            if path.is_empty() || path.starts_with("..") || path.starts_with('/') {
                return None;
            }
            if path.len() > 1 && path.as_bytes()[1] == b':' {
                return None;
            }
            let top = path.split(['/', '\\']).next()?;
            let skip = top.ends_with(".dist-info")
                || top.ends_with(".data")
                || top == "__pycache__"
                || top.is_empty();
            (!skip).then(|| top.to_string())
        })
        .collect()
}

/// The module names a distribution most likely installs: its own name with
/// `_` for `-`, plus import names the import map ties to it.
pub fn import_candidates(package: &str) -> Vec<String> {
    let mut out = vec![package.replace(['-', '.'], "_")];
    for name in crate::matrix::import_names_for(package) {
        if !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// Are `package`'s files in site-packages although no `.dist-info` says so?
fn has_files_without_metadata(sites: &[PathBuf], package: &str) -> bool {
    let candidates = import_candidates(package);
    sites.iter().any(|site| {
        candidates.iter().any(|m| {
            site.join(m).join("__init__.py").is_file() || site.join(format!("{m}.py")).is_file()
        })
    })
}

/// What `uv.lock` records, as far as these checks need it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Lock {
    pub requires_python: Option<String>,
    /// Registry packages: normalized name to every locked version (a fork
    /// across Python versions locks several).
    pub versions: BTreeMap<String, BTreeSet<String>>,
    /// The project's own `requires-dist`, when the lock has a root package.
    pub root_requires: Option<BTreeMap<String, BTreeSet<String>>>,
    /// The root's `requires-dev`, merged across groups.
    pub root_dev: BTreeMap<String, BTreeSet<String>>,
}

/// Parse `<workspace>/uv.lock`, or `None` when it is absent or unreadable.
pub fn read_lock(workspace: &Path) -> Option<Lock> {
    let text = std::fs::read_to_string(workspace.join("uv.lock")).ok()?;
    parse_lock(&text)
}

pub fn parse_lock(text: &str) -> Option<Lock> {
    let table: toml::Table = text.parse().ok()?;
    let mut lock = Lock {
        requires_python: table
            .get("requires-python")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        ..Lock::default()
    };
    for pkg in table.get("package")?.as_array()? {
        let Some(name) = pkg.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let name = normalize_name(name);
        let source = pkg.get("source");
        let local = source.is_some_and(|s| {
            ["virtual", "editable"]
                .iter()
                .any(|k| s.get(k).and_then(|v| v.as_str()) == Some("."))
        });
        if local {
            let metadata = pkg.get("metadata");
            lock.root_requires = Some(requirement_table(
                metadata
                    .and_then(|m| m.get("requires-dist"))
                    .and_then(|v| v.as_array()),
            ));
            if let Some(groups) = metadata
                .and_then(|m| m.get("requires-dev"))
                .and_then(|v| v.as_table())
            {
                for list in groups.values() {
                    merge(&mut lock.root_dev, requirement_table(list.as_array()));
                }
            }
            continue;
        }
        if source.is_some_and(|s| s.get("registry").is_some()) {
            if let Some(v) = pkg.get("version").and_then(|v| v.as_str()) {
                lock.versions
                    .entry(name)
                    .or_default()
                    .insert(normalize_version(v));
            }
        }
    }
    Some(lock)
}

fn merge(into: &mut BTreeMap<String, BTreeSet<String>>, from: BTreeMap<String, BTreeSet<String>>) {
    for (k, v) in from {
        into.entry(k).or_default().extend(v);
    }
}

/// `[{name = "x", specifier = ">=1", extras = ["a"]}]` as name to specs.
fn requirement_table(list: Option<&Vec<toml::Value>>) -> BTreeMap<String, BTreeSet<String>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for item in list.into_iter().flatten() {
        let Some(name) = item.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let spec = item.get("specifier").and_then(|v| v.as_str()).unwrap_or("");
        let mut extras: Vec<String> = item
            .get("extras")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|e| e.as_str().map(normalize_name))
            .collect();
        extras.sort();
        out.entry(normalize_name(name))
            .or_default()
            .insert(requirement_key(&extras, spec));
    }
    out
}

/// One comparable form for a requirement: sorted extras, then its
/// specifier with whitespace removed and clauses sorted.
fn requirement_key(extras: &[String], spec: &str) -> String {
    let mut clauses: Vec<String> = spec
        .split(',')
        .map(|c| {
            c.chars()
                .filter(|ch| !ch.is_whitespace())
                .collect::<String>()
        })
        .filter(|c| !c.is_empty())
        .collect();
    clauses.sort();
    format!("[{}]{}", extras.join(","), clauses.join(","))
}

/// A pyproject requirement string as `(name, key)`; `None` for a direct
/// reference, whose lock entry carries a URL rather than a specifier.
fn pyproject_requirement(line: &str) -> Option<(String, String)> {
    let req = line.split(';').next().unwrap_or(line).trim();
    let end = req
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
        .unwrap_or(req.len());
    let (name, rest) = req.split_at(end);
    if name.is_empty() {
        return None;
    }
    let name = normalize_name(name);
    if rest.contains('@') {
        return Some((name, "@".to_string()));
    }
    let rest = rest.trim();
    let (extras, spec) = match rest.strip_prefix('[') {
        Some(r) => r.split_once(']').unwrap_or(("", r)),
        None => ("", rest),
    };
    let mut extras: Vec<String> = extras
        .split(',')
        .map(|e| normalize_name(e.trim()))
        .filter(|e| !e.is_empty())
        .collect();
    extras.sort();
    let spec = spec.trim().trim_start_matches('(').trim_end_matches(')');
    Some((name, requirement_key(&extras, spec)))
}

/// Requirements as declared by `pyproject.toml`: runtime (with optional
/// dependencies, which uv records as `extra == ...` entries) and dev
/// (`[dependency-groups]` and `tool.uv.dev-dependencies`).
fn pyproject_requirements(
    table: &toml::Table,
) -> (
    BTreeMap<String, BTreeSet<String>>,
    BTreeMap<String, BTreeSet<String>>,
) {
    let strings = |v: Option<&toml::Value>| -> Vec<String> {
        v.and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|s| s.as_str().map(str::to_string))
            .collect()
    };
    let collect = |lines: Vec<String>| {
        let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for line in lines {
            if let Some((name, key)) = pyproject_requirement(&line) {
                out.entry(name).or_default().insert(key);
            }
        }
        out
    };
    let project = table.get("project");
    let mut runtime = strings(project.and_then(|p| p.get("dependencies")));
    if let Some(optional) = project
        .and_then(|p| p.get("optional-dependencies"))
        .and_then(|o| o.as_table())
    {
        for list in optional.values() {
            runtime.extend(strings(Some(list)));
        }
    }
    let mut dev = Vec::new();
    if let Some(groups) = table.get("dependency-groups").and_then(|g| g.as_table()) {
        for list in groups.values() {
            dev.extend(strings(Some(list)));
        }
    }
    dev.extend(strings(
        table
            .get("tool")
            .and_then(|t| t.get("uv"))
            .and_then(|u| u.get("dev-dependencies")),
    ));
    (collect(runtime), collect(dev))
}

/// How `pyproject.toml` differs from what `uv.lock` was resolved for, in
/// words (`added pyyaml`, `requires-python >=3.9 -> >=3.10`). Empty when
/// they agree, or when the lock has no root package to compare with (a
/// workspace member, an old lock format).
pub fn lock_changes(workspace: &Path, lock: &Lock) -> Vec<String> {
    let Some(table) = crate::core::project::read_manifest(&workspace.join("pyproject.toml"))
        .and_then(|t| t.parse::<toml::Table>().ok())
    else {
        return Vec::new();
    };
    compare_lock(&table, lock)
}

fn compare_lock(table: &toml::Table, lock: &Lock) -> Vec<String> {
    let Some(locked) = &lock.root_requires else {
        return Vec::new();
    };
    let (runtime, dev) = pyproject_requirements(table);
    let mut changes = diff(&runtime, locked);
    changes.extend(diff(&dev, &lock.root_dev));
    let declared_rp = table
        .get("project")
        .and_then(|p| p.get("requires-python"))
        .and_then(|v| v.as_str());
    if let (Some(declared), Some(locked)) = (declared_rp, lock.requires_python.as_deref()) {
        if requirement_key(&[], declared) != requirement_key(&[], locked) {
            changes.push(format!(
                "requires-python {} -> {}",
                locked.trim(),
                declared.trim()
            ));
        }
    }
    changes
}

fn diff(
    declared: &BTreeMap<String, BTreeSet<String>>,
    locked: &BTreeMap<String, BTreeSet<String>>,
) -> Vec<String> {
    let mut out = Vec::new();
    for (name, keys) in declared {
        match locked.get(name) {
            None => out.push(format!("added {name}")),
            // A direct reference's lock entry has no specifier to compare.
            Some(_) if keys.contains("@") => {}
            Some(l) if l != keys => out.push(format!("changed {name}")),
            Some(_) => {}
        }
    }
    for name in locked.keys() {
        if !declared.contains_key(name) {
            out.push(format!("removed {name}"));
        }
    }
    out
}

/// A version as written in a dist-info name or the lock, for comparing:
/// lowercase, `_` (dist-info escaping) as `-`.
fn normalize_version(v: &str) -> String {
    v.trim().to_ascii_lowercase().replace('_', "-")
}

/// Installed registry packages whose version is not one the lock records,
/// as `requests 2.31.0 (locked 2.34.2)`.
pub fn drift(lock: &Lock, installed: &Installed) -> Vec<String> {
    let mut out = Vec::new();
    for (name, locked) in &lock.versions {
        let Some(have) = installed.version_of(name) else {
            continue;
        };
        if !locked.contains(&normalize_version(&have)) {
            let want: Vec<&str> = locked.iter().map(String::as_str).collect();
            out.push(format!("{name} {have} (locked {})", want.join(" or ")));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::env::EnvKind;

    /// A project dir with a Windows- or POSIX-shaped venv whose
    /// site-packages holds the given `(dist-info dir, RECORD, module files)`.
    struct Fixture {
        root: PathBuf,
        site: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Fixture {
            let root =
                std::env::temp_dir().join(format!("pypilot-health-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let venv = root.join(".venv");
            let site = if cfg!(windows) {
                venv.join("Lib").join("site-packages")
            } else {
                venv.join("lib").join("python3.12").join("site-packages")
            };
            std::fs::create_dir_all(&site).unwrap();
            std::fs::write(
                venv.join("pyvenv.cfg"),
                "home = /usr\nversion_info = 3.12.1\ninclude-system-site-packages = false\n",
            )
            .unwrap();
            Fixture { root, site }
        }

        /// Install `name==version` with module dir `module`.
        fn dist(&self, name: &str, version: &str, module: &str) -> &Self {
            let info = self.site.join(format!("{name}-{version}.dist-info"));
            std::fs::create_dir_all(&info).unwrap();
            let m = self.site.join(module);
            std::fs::create_dir_all(&m).unwrap();
            std::fs::write(m.join("__init__.py"), "").unwrap();
            std::fs::write(
                info.join("RECORD"),
                format!("{module}/__init__.py,sha256=x,0\n{name}-{version}.dist-info/RECORD,,\n../../Scripts/tool.exe,sha256=y,1\n"),
            )
            .unwrap();
            self
        }

        fn write(&self, file: &str, text: &str) -> &Self {
            std::fs::write(self.root.join(file), text).unwrap();
            self
        }

        fn venv(&self) -> VenvInfo {
            VenvInfo {
                path: self.root.join(".venv"),
                python: Some(crate::pypi::PyVersion::py3(12)),
                broken: None,
                kind: EnvKind::Venv,
            }
        }

        fn check(&self) -> Vec<Finding> {
            check(&self.root, Some(&self.venv()), PackageManager::Auto, false)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    const PYPROJECT: &str = "[project]\nname = \"demo\"\nversion = \"0.1.0\"\nrequires-python = \">=3.9\"\ndependencies = [\"requests>=2.31\", \"six\", \"pywin32; sys_platform == 'win32'\"]\n";

    const LOCK: &str = r#"version = 1
requires-python = ">=3.9"

[[package]]
name = "demo"
version = "0.1.0"
source = { virtual = "." }
dependencies = [{ name = "requests" }, { name = "six" }]

[package.metadata]
requires-dist = [
    { name = "pywin32", marker = "sys_platform == 'win32'" },
    { name = "requests", specifier = ">=2.31" },
    { name = "six" },
]

[[package]]
name = "requests"
version = "2.34.2"
source = { registry = "https://pypi.org/simple" }

[[package]]
name = "six"
version = "1.17.0"
source = { registry = "https://pypi.org/simple" }
"#;

    fn healthy(tag: &str) -> Fixture {
        let f = Fixture::new(tag);
        f.write("pyproject.toml", PYPROJECT).write("uv.lock", LOCK);
        f.dist("requests", "2.34.2", "requests")
            .dist("six", "1.17.0", "six");
        f
    }

    fn kinds(findings: &[Finding]) -> Vec<&FixKind> {
        findings.iter().map(|f| &f.fix).collect()
    }

    #[test]
    fn healthy_locked_project_is_silent() {
        let f = healthy("ok");
        assert_eq!(f.check(), Vec::<Finding>::new());
    }

    #[test]
    fn pixi_projects_are_never_judged_here() {
        let f = healthy("pixi");
        std::fs::remove_dir_all(f.site.join("six-1.17.0.dist-info")).unwrap();
        assert!(check(&f.root, Some(&f.venv()), PackageManager::Auto, true).is_empty());
    }

    #[test]
    fn added_dependency_makes_the_lock_stale() {
        let f = healthy("stale");
        f.write(
            "pyproject.toml",
            &PYPROJECT.replace("\"six\",", "\"six\", \"pyyaml\","),
        );
        let findings = f.check();
        let stale = findings
            .iter()
            .find(|x| x.fix == FixKind::Relock)
            .unwrap_or_else(|| panic!("{findings:#?}"));
        assert!(stale.detail.contains("added pyyaml"), "{}", stale.detail);
        // pyyaml is missing too; the sync after the relock installs it.
        assert!(kinds(&findings).contains(&&FixKind::SyncEnvironment));
    }

    #[test]
    fn spec_and_requires_python_changes_are_seen_and_order_is_not() {
        let table: toml::Table = PYPROJECT
            .replace("requests>=2.31", "requests >= 2.31, <3")
            .replace(">=3.9\"", ">=3.10\"")
            .parse()
            .unwrap();
        let lock = parse_lock(LOCK).unwrap();
        assert_eq!(
            compare_lock(&table, &lock),
            ["changed requests", "requires-python >=3.9 -> >=3.10"]
        );
        // Whitespace and clause order are not changes.
        let lock2 = parse_lock(&LOCK.replace("\">=2.31\"", "\"<3, >=2.31\"")).unwrap();
        let table2: toml::Table = PYPROJECT
            .replace("requests>=2.31", "requests>=2.31,<3")
            .parse()
            .unwrap();
        assert!(compare_lock(&table2, &lock2).is_empty());
    }

    #[test]
    fn dev_groups_are_compared() {
        let lock = Lock {
            root_dev: [("pytest".to_string(), ["[]".to_string()].into())].into(),
            ..parse_lock(LOCK).unwrap()
        };
        let with: toml::Table = format!("{PYPROJECT}\n[dependency-groups]\ndev = [\"pytest\"]\n")
            .parse()
            .unwrap();
        assert!(compare_lock(&with, &lock).is_empty());
        let without: toml::Table = PYPROJECT.parse().unwrap();
        assert_eq!(compare_lock(&without, &lock), ["removed pytest"]);
    }

    #[test]
    fn downgraded_package_is_drift() {
        let f = Fixture::new("drift");
        f.write("pyproject.toml", PYPROJECT).write("uv.lock", LOCK);
        f.dist("requests", "2.31.0", "requests")
            .dist("six", "1.17.0", "six");
        let findings = f.check();
        assert_eq!(kinds(&findings), [&FixKind::SyncEnvironment]);
        assert!(
            findings[0]
                .detail
                .contains("requests 2.31.0 (locked 2.34.2)"),
            "{}",
            findings[0].detail
        );
    }

    #[test]
    fn uninstalled_package_is_missing() {
        let f = Fixture::new("missing");
        f.write("pyproject.toml", PYPROJECT).write("uv.lock", LOCK);
        f.dist("requests", "2.34.2", "requests");
        let findings = f.check();
        assert_eq!(kinds(&findings), [&FixKind::SyncEnvironment]);
        assert_eq!(
            findings[0].title,
            "Declared packages are not installed: six"
        );
    }

    #[test]
    fn deleted_module_dir_is_a_damaged_package() {
        let f = healthy("damaged");
        std::fs::remove_dir_all(f.site.join("requests")).unwrap();
        let findings = f.check();
        assert_eq!(
            kinds(&findings),
            [&FixKind::ReinstallPackages(vec!["requests".into()])]
        );
        assert_eq!(findings[0].severity, Severity::Error);
    }

    #[test]
    fn deleted_dist_info_is_half_installed_not_missing() {
        let f = healthy("orphan");
        std::fs::remove_dir_all(f.site.join("six-1.17.0.dist-info")).unwrap();
        let findings = f.check();
        assert_eq!(
            kinds(&findings),
            [&FixKind::ReinstallPackages(vec!["six".into()])]
        );
        assert!(findings[0].title.starts_with("Half-installed"));
    }

    #[test]
    fn system_site_packages_asks_for_a_rebuild_on_the_same_python() {
        let f = healthy("site");
        let cfg = f.root.join(".venv").join("pyvenv.cfg");
        let text = std::fs::read_to_string(&cfg)
            .unwrap()
            .replace("= false", "= True");
        std::fs::write(&cfg, text).unwrap();
        assert_eq!(
            kinds(&f.check()),
            [&FixKind::RepairEnvironment {
                python: Some(crate::pypi::PyVersion::py3(12))
            }]
        );
    }

    #[test]
    fn pip_modes_ignore_the_lock() {
        let f = Fixture::new("pipmode");
        f.write("pyproject.toml", PYPROJECT).write("uv.lock", LOCK);
        f.dist("requests", "2.31.0", "requests")
            .dist("six", "1.17.0", "six");
        for pm in [PackageManager::Pip, PackageManager::UvPip] {
            assert!(check(&f.root, Some(&f.venv()), pm, false).is_empty());
        }
    }

    #[test]
    fn requirements_txt_is_used_without_pyproject_dependencies() {
        let f = Fixture::new("reqs");
        f.write("requirements.txt", "# c\nrequests==2.31.0\n-r other.txt\nsix ; python_version < '3'\nattrs  # pinned later\n");
        assert_eq!(
            declared_packages(&f.root),
            Declared {
                source: "requirements.txt",
                names: vec!["requests".into(), "attrs".into()],
            }
        );
    }

    #[test]
    fn venv_dir_without_pyvenv_cfg_is_damaged() {
        let f = Fixture::new("nocfg");
        assert_eq!(damaged_venv(&f.root), None);
        std::fs::remove_file(f.root.join(".venv").join("pyvenv.cfg")).unwrap();
        assert_eq!(damaged_venv(&f.root), Some(f.root.join(".venv")));
        // An empty directory is just "no environment yet".
        let g = Fixture::new("emptydir");
        std::fs::remove_dir_all(g.root.join(".venv")).unwrap();
        std::fs::create_dir_all(g.root.join(".venv")).unwrap();
        assert_eq!(damaged_venv(&g.root), None);
    }

    #[test]
    fn record_tops_skip_metadata_and_outside_paths() {
        let tops = record_tops(
            "requests/__init__.py,sha256=a,1\nrequests/api.py,,\nsix.py,,\n\"odd,name/x.py\",,\nrequests-2.0.dist-info/RECORD,,\n../../bin/x,,\n__pycache__/six.cpython-312.pyc,,\n",
        );
        assert_eq!(
            tops.into_iter().collect::<Vec<_>>(),
            ["odd,name", "requests", "six.py"]
        );
    }
}
