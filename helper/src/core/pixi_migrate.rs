//! Turning a pip/uv or conda project into a pixi project.
//!
//! [`migrate_to_pixi`] reads a project's `requirements.txt` and
//! `[project.dependencies]` (or its `environment.yml`) and writes a
//! `pixi.toml` beside them. It never edits or deletes the files it reads:
//! CI jobs and colleagues may still install from them.
//!
//! **Why a separate `pixi.toml` and not `[tool.pixi]` in `pyproject.toml`:**
//! writing a new file is the only non-destructive option. Editing the
//! pyproject would rewrite a file the user owns (and that pip, uv, build
//! backends and CI read), and removing the new file undoes the migration
//! completely. pixi itself prefers `pixi.toml` when both exist, as does
//! [`crate::core::pixi::detect`], so the result is unambiguous.
//!
//! Each PyPI dependency is classified with prefix.dev's PyPI-to-conda mapping
//! (the parselmouth data behind <https://conda-mapping.prefix.dev>), one
//! small document per package, cached like the PyPI metadata:
//!
//! - packaged on conda-forge, with a build for every target platform, and a
//!   specifier conda can express exactly: `[dependencies]` under its conda
//!   name (`torch` becomes `pytorch`);
//! - anything else: `[pypi-dependencies]`, with the reason as a comment.
//!
//! Platform availability and the Pythons conda-forge builds exist for come
//! from `pixi search`, when pixi is installed. Offline, with no cached
//! mapping, everything goes to `[pypi-dependencies]`, which pixi installs
//! exactly as pip would. Python is the version PyPilot's solver recommends
//! ([`crate::core::solver::synthesize`]), narrowed to what the conda
//! packages are built for.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use crate::core::command;
use crate::core::pixi;
use crate::core::pixi_manage;
use crate::core::platform::{self, Arch, Os, Platform};
use crate::core::project::{normalize_name, ProjectDeps, Requirement};
use crate::core::setup::{SetupSummary, Step};
use crate::core::solver::{self, EnvSummary};
use crate::pypi::pyversion::{parse_requires_python, PyVersion, PyVersionSet};
use crate::pypi::version::{PackageVersion, VersionSpec};
use crate::pypi::{self, MetadataSource, PyPiClient};
use crate::settings::{PackageManager, ResolvedMode, Settings};

/// The manifest this module writes.
pub const MANIFEST: &str = "pixi.toml";

/// prefix.dev's PyPI-to-conda mapping for conda-forge, one JSON document per
/// normalized PyPI name: `{"conda_versions": {"<pypi version>": "<conda name>"}}`.
/// A 404 means conda-forge does not package the project.
pub const MAPPING_URL: &str = "https://conda-mapping.prefix.dev/pypi-to-conda-v1/conda-forge";

/// The conda platforms (`subdir`s) pixi accepts in `[workspace] platforms`.
pub const KNOWN_PLATFORMS: &[&str] = &[
    "linux-64",
    "linux-aarch64",
    "linux-ppc64le",
    "osx-64",
    "osx-arm64",
    "win-64",
    "win-arm64",
];

/// One `pixi search` can take a while on a cold cache.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(120);

/// The conda platform of this machine.
pub fn current_platform() -> &'static str {
    let p = Platform::current();
    match (p.os, p.arch) {
        (Os::Linux, Arch::X86_64) => "linux-64",
        (Os::Linux, Arch::Aarch64) => "linux-aarch64",
        (Os::Macos, Arch::X86_64) => "osx-64",
        (Os::Macos, Arch::Aarch64) => "osx-arm64",
        (Os::Windows, Arch::X86_64) => "win-64",
        (Os::Windows, Arch::Aarch64) => "win-arm64",
    }
}

/// What to migrate from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MigrationSource {
    /// `environment.yml` when there is one, else the PyPI dependencies.
    #[default]
    Auto,
    /// `requirements.txt` and `[project.dependencies]`.
    PythonProject,
    /// `environment.yml`.
    CondaEnvironment,
}

/// How [`migrate_to_pixi`] runs.
#[derive(Debug, Clone, Default)]
pub struct MigrateOptions {
    /// Write `pixi.toml`. Without it nothing on disk changes (a dry run).
    pub write: bool,
    /// After writing, run `pixi install` (needs pixi).
    pub install: bool,
    /// Platforms besides this machine's, e.g. `linux-64`.
    pub extra_platforms: Vec<String>,
    pub source: MigrationSource,
}

/// Which table a dependency went to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepKind {
    /// `[dependencies]`, from conda-forge.
    Conda,
    /// `[pypi-dependencies]`.
    Pypi,
}

/// One dependency as written to the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigratedDep {
    /// The key in the manifest (the conda name for conda packages).
    pub name: String,
    /// The TOML value as written (`"*"`, `">=1.26"`, `{ version = ... }`).
    pub value: String,
    pub kind: DepKind,
    /// The dependency as the source file declared it.
    pub from: String,
    /// Why it went where it did; empty when there is nothing to explain.
    pub reason: String,
}

/// A declaration that was not migrated, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedDep {
    pub line: String,
    pub reason: String,
}

/// What a migration did, or would do on a dry run.
#[derive(Debug, Clone)]
pub struct MigrationSummary {
    pub manifest_path: PathBuf,
    /// The full manifest text.
    pub manifest: String,
    /// Diff-style preview of the manifest (a new file).
    pub preview: String,
    /// True when the manifest was written.
    pub written: bool,
    /// The files the dependencies were read from.
    pub sources: Vec<PathBuf>,
    /// The `python` spec written (`3.12.*`).
    pub python: String,
    pub platforms: Vec<String>,
    pub channels: Vec<String>,
    pub deps: Vec<MigratedDep>,
    pub skipped: Vec<SkippedDep>,
    /// Anything else the user should know (offline mapping, dropped channels).
    pub notes: Vec<String>,
    /// The `pixi install` run, when one was asked for.
    pub install: Option<SetupSummary>,
    /// What to run next, when there is something.
    pub next_command: Option<String>,
}

impl MigrationSummary {
    /// False when an install was asked for and did not succeed.
    pub fn ok(&self) -> bool {
        self.install.as_ref().map_or(true, |s| s.ok)
    }

    pub fn conda_deps(&self) -> impl Iterator<Item = &MigratedDep> {
        self.deps.iter().filter(|d| d.kind == DepKind::Conda)
    }

    pub fn pypi_deps(&self) -> impl Iterator<Item = &MigratedDep> {
        self.deps.iter().filter(|d| d.kind == DepKind::Pypi)
    }

    /// One line for a notification: what was (or would be) written.
    pub fn headline(&self) -> String {
        let verb = if self.written { "Wrote" } else { "Would write" };
        format!(
            "{verb} {}: python {}, {} conda-forge and {} PyPI dependencies{}.",
            self.manifest_path.display(),
            self.python,
            self.conda_deps().count(),
            self.pypi_deps().count(),
            if self.skipped.is_empty() {
                String::new()
            } else {
                format!(", {} left for review", self.skipped.len())
            }
        )
    }
}

/// Migrate the project at `workspace` to pixi.
///
/// A dry run unless `opts.write`: the summary then carries the manifest and
/// its preview and nothing on disk changes. With `opts.write` the manifest
/// is created (never overwritten); with `opts.install` too, `pixi install`
/// runs through [`crate::core::pixi_manage`] when pixi is installed.
///
/// Fails when the project already is a pixi project or a `pixi.toml` exists,
/// and for an unknown platform name.
pub async fn migrate_to_pixi(
    workspace: &Path,
    settings: &Settings,
    opts: MigrateOptions,
) -> crate::Result<MigrationSummary> {
    let pixi_bin = pixi::locate().await;
    let index = LiveCondaIndex::new(settings, pixi_bin.clone());
    let source = PyPiClient::new();
    migrate_with(workspace, &source, &index, pixi_bin.as_deref(), &opts).await
}

/// [`migrate_to_pixi`] with the metadata source, the conda index and the
/// pixi binary given, which is how tests run it offline.
pub async fn migrate_with<S: MetadataSource, I: CondaIndex>(
    workspace: &Path,
    source: &S,
    index: &I,
    pixi_bin: Option<&Path>,
    opts: &MigrateOptions,
) -> crate::Result<MigrationSummary> {
    let mut summary = plan(workspace, source, index, opts).await?;
    if !opts.write {
        let command = if opts.source == MigrationSource::CondaEnvironment {
            "pypilot migrate-conda --to pixi --write"
        } else {
            "pypilot migrate-to-pixi --write"
        };
        summary.next_command = Some(if opts.install {
            format!("{command} --install")
        } else {
            command.to_string()
        });
        return Ok(summary);
    }
    write_new(&summary.manifest_path, &summary.manifest)?;
    summary.written = true;

    if !opts.install {
        summary.next_command = Some(if pixi_bin.is_some() {
            "pixi install".to_string()
        } else {
            "pypilot install-pixi, then pixi install".to_string()
        });
        return Ok(summary);
    }
    let mut setup = SetupSummary {
        package_manager: PackageManager::Pixi,
        resolved_mode: ResolvedMode::Pixi,
        python: None,
        venv_path: pixi_manage::default_env_path(workspace),
        why: "pixi project: the environment is installed by pixi from its manifest.".into(),
        steps: Vec::new(),
        ok: true,
    };
    crate::core::setup::run_pixi(workspace, pixi_bin, &mut setup).await;
    if pixi_bin.is_none() {
        summary.next_command = Some("pypilot install-pixi, then pixi install".to_string());
    } else if setup.ok {
        summary.next_command = Some("pixi run python".to_string());
    }
    summary.install = Some(setup);
    Ok(summary)
}

/// Setup's half of `package_manager = "pixi"` on a project with no pixi
/// manifest: write one from the project's dependencies, so that setup can
/// go on to `pixi install` it. Without pixi nothing is written. Returns
/// whether setup should continue.
pub async fn create_for_setup<S: MetadataSource>(
    workspace: &Path,
    settings: &Settings,
    source: &S,
    pixi_bin: Option<&Path>,
    summary: &mut SetupSummary,
) -> bool {
    if pixi_bin.is_none() {
        summary.ok = false;
        summary.steps.push(Step {
            name: pixi_manage::LOCATE_STEP.to_string(),
            ok: false,
            detail: pixi_manage::MISSING_PIXI_FOR_SETTING.to_string(),
        });
        return false;
    }
    let index = LiveCondaIndex::new(settings, pixi_bin.map(Path::to_path_buf));
    let opts = MigrateOptions {
        write: true,
        ..MigrateOptions::default()
    };
    let step = "create pixi manifest";
    match migrate_with(workspace, source, &index, pixi_bin, &opts).await {
        Ok(m) => {
            summary.steps.push(Step {
                name: step.to_string(),
                ok: true,
                detail: format!(
                    "{} (package_manager = \"pixi\"; the files it was made from are untouched)",
                    m.headline()
                ),
            });
            true
        }
        Err(e) => {
            summary.ok = false;
            summary.steps.push(Step {
                name: step.to_string(),
                ok: false,
                detail: e.to_string(),
            });
            false
        }
    }
}

/// Create `path` with `text`, failing if it exists.
fn write_new(path: &Path, text: &str) -> crate::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", path.display()))?;
    f.write_all(text.as_bytes())?;
    Ok(())
}

/// Work out the manifest, touching nothing on disk.
pub async fn plan<S: MetadataSource, I: CondaIndex>(
    workspace: &Path,
    source: &S,
    index: &I,
    opts: &MigrateOptions,
) -> crate::Result<MigrationSummary> {
    let manifest_path = workspace.join(MANIFEST);
    if let Some(existing) = pixi::detect(workspace) {
        anyhow::bail!(
            "this already is a pixi project ({}); nothing to migrate",
            existing.manifest.display()
        );
    }
    if manifest_path.exists() {
        anyhow::bail!(
            "{} already exists; PyPilot never overwrites it",
            manifest_path.display()
        );
    }
    let platforms = platforms(&opts.extra_platforms)?;

    let env_yml = workspace.join("environment.yml");
    let from_conda = match opts.source {
        MigrationSource::CondaEnvironment => true,
        MigrationSource::PythonProject => false,
        MigrationSource::Auto => env_yml.is_file(),
    };
    let name = std::fs::canonicalize(workspace)
        .unwrap_or_else(|_| workspace.to_path_buf())
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".to_string());

    let mut draft = if from_conda {
        let Some(text) = read(&env_yml) else {
            anyhow::bail!("no environment.yml found in {}", workspace.display());
        };
        from_environment_yml(&text, &name, env_yml.clone())
    } else {
        from_python_project(workspace, &name, source, index, &platforms).await
    };
    draft.platforms = platforms;
    let manifest = render(&draft);
    Ok(MigrationSummary {
        preview: preview(&manifest),
        manifest,
        manifest_path,
        written: false,
        sources: draft.sources,
        python: draft.python,
        platforms: draft.platforms,
        channels: draft.channels,
        deps: draft.deps,
        skipped: draft.skipped,
        notes: draft.notes,
        install: None,
        next_command: None,
    })
}

fn platforms(extra: &[String]) -> crate::Result<Vec<String>> {
    let mut out = vec![current_platform().to_string()];
    for p in extra.iter().flat_map(|p| p.split(',')) {
        let p = p.trim();
        if p.is_empty() {
            continue;
        }
        if !KNOWN_PLATFORMS.contains(&p) {
            anyhow::bail!(
                "unknown platform `{p}`; pixi platforms are {}",
                KNOWN_PLATFORMS.join(", ")
            );
        }
        if !out.iter().any(|o| o == p) {
            out.push(p.to_string());
        }
    }
    Ok(out)
}

fn read(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Some(text.trim_start_matches('\u{feff}').to_string())
}

/// The manifest before rendering.
#[derive(Debug, Clone, Default)]
struct Draft {
    name: String,
    header: String,
    sources: Vec<PathBuf>,
    channels: Vec<String>,
    platforms: Vec<String>,
    python: String,
    deps: Vec<MigratedDep>,
    skipped: Vec<SkippedDep>,
    notes: Vec<String>,
    index_url: Option<String>,
    extra_index_urls: Vec<String>,
    activation_env: Vec<(String, String)>,
}

// --- PyPI projects -----------------------------------------------------------

/// A PEP 508 requirement, as much of it as the migration needs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PyDep {
    /// Normalized per PEP 503.
    pub name: String,
    pub extras: Vec<String>,
    /// PEP 440 specifier without whitespace; empty when unconstrained.
    pub spec: String,
    pub marker: Option<String>,
    /// `name @ <url>`.
    pub url: Option<String>,
    /// The declaration as written.
    pub line: String,
}

/// Parse a PEP 508 requirement. `Err` explains a line that is not one.
pub fn parse_pep508(line: &str) -> Result<PyDep, String> {
    let original = line.trim().to_string();
    let line = strip_comment(&original);
    let (body, marker) = match line.split_once(';') {
        Some((b, m)) => (
            b.trim(),
            Some(m.trim().to_string()).filter(|m| !m.is_empty()),
        ),
        None => (line.trim(), None),
    };
    let name_end = body
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
        .unwrap_or(body.len());
    let raw_name = &body[..name_end];
    if raw_name.is_empty() || !raw_name.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return Err("not a package requirement".to_string());
    }
    let mut rest = body[name_end..].trim_start();
    let mut extras = Vec::new();
    if let Some(after) = rest.strip_prefix('[') {
        let Some(close) = after.find(']') else {
            return Err("unclosed extras bracket".to_string());
        };
        extras = after[..close]
            .split(',')
            .map(normalize_name)
            .filter(|e| !e.is_empty())
            .collect();
        rest = after[close + 1..].trim_start();
    }
    let mut url = None;
    let mut spec = String::new();
    if let Some(u) = rest.strip_prefix('@') {
        let u = u.trim();
        if u.is_empty() {
            return Err("`@` without a URL".to_string());
        }
        url = Some(u.to_string());
    } else {
        let s: String = rest.chars().filter(|c| !c.is_whitespace()).collect();
        let s = s.trim_start_matches('(').trim_end_matches(')');
        if !s.is_empty() && !s.starts_with(['=', '<', '>', '!', '~']) {
            return Err(format!("cannot read the version specifier `{s}`"));
        }
        spec = s.to_string();
    }
    Ok(PyDep {
        name: normalize_name(raw_name),
        extras,
        spec,
        marker,
        url,
        line: original,
    })
}

fn strip_comment(line: &str) -> &str {
    if line.starts_with('#') {
        return "";
    }
    match line
        .as_bytes()
        .windows(2)
        .position(|w| w[0].is_ascii_whitespace() && w[1] == b'#')
    {
        Some(i) => line[..i].trim_end(),
        None => line,
    }
}

/// What a `requirements.txt` says, beyond its requirements.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Requirements {
    deps: Vec<PyDep>,
    skipped: Vec<SkippedDep>,
    index_url: Option<String>,
    extra_index_urls: Vec<String>,
}

fn parse_requirements_txt(text: &str) -> Requirements {
    let mut out = Requirements::default();
    // Join `\` continuations first.
    let mut logical = Vec::new();
    let mut current = String::new();
    for raw in text.lines() {
        let line = raw.trim_end();
        if let Some(stripped) = line.strip_suffix('\\') {
            current.push_str(stripped);
            current.push(' ');
            continue;
        }
        current.push_str(line);
        logical.push(std::mem::take(&mut current));
    }
    if !current.is_empty() {
        logical.push(current);
    }

    for line in logical {
        let line = strip_comment(line.trim()).trim().to_string();
        if line.is_empty() {
            continue;
        }
        if let Some(opt) = line.strip_prefix('-') {
            let (flag, value) = match opt.split_once([' ', '=']) {
                Some((f, v)) => (f.trim(), v.trim()),
                None => (opt.trim(), ""),
            };
            match flag {
                "i" | "-index-url" => out.index_url = Some(value.to_string()),
                "-extra-index-url" => out.extra_index_urls.push(value.to_string()),
                "e" | "-editable" => out.skipped.push(SkippedDep {
                    line: line.clone(),
                    reason: "editable install: add it as `{ path = \"...\", editable = true }` under [pypi-dependencies]".into(),
                }),
                "r" | "-requirement" | "c" | "-constraint" => out.skipped.push(SkippedDep {
                    line: line.clone(),
                    reason: "includes another file, which is not followed".into(),
                }),
                _ => out.skipped.push(SkippedDep {
                    line: line.clone(),
                    reason: "pip option with no pixi equivalent here".into(),
                }),
            }
            continue;
        }
        // Per-requirement options (`--hash=...`) are pip's, not part of PEP 508.
        let requirement = match line.find(" --") {
            Some(i) => line[..i].trim(),
            None => line.as_str(),
        };
        if requirement.starts_with(['.', '/', '\\']) || requirement.contains("://") {
            out.skipped.push(SkippedDep {
                line: line.clone(),
                reason: "a path or URL with no package name".into(),
            });
            continue;
        }
        match parse_pep508(requirement) {
            Ok(dep) => out.deps.push(dep),
            Err(reason) => out.skipped.push(SkippedDep {
                line: line.clone(),
                reason,
            }),
        }
    }
    out
}

/// `[project]` dependencies and `requires-python`, plus notes on the
/// dependency tables that are not migrated.
fn parse_pyproject(text: &str) -> (Vec<Result<PyDep, SkippedDep>>, Option<String>, Vec<String>) {
    let mut deps = Vec::new();
    let mut notes = Vec::new();
    let Ok(doc) = toml::from_str::<toml::Value>(text) else {
        notes.push("pyproject.toml could not be parsed, so its dependencies were not read".into());
        return (deps, None, notes);
    };
    let project = doc.get("project");
    let requires_python = project
        .and_then(|p| p.get("requires-python"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    if let Some(list) = project
        .and_then(|p| p.get("dependencies"))
        .and_then(|v| v.as_array())
    {
        for line in list.iter().filter_map(|v| v.as_str()) {
            deps.push(parse_pep508(line).map_err(|reason| SkippedDep {
                line: line.to_string(),
                reason,
            }));
        }
    }
    if project
        .and_then(|p| p.get("optional-dependencies"))
        .is_some()
    {
        notes.push("[project.optional-dependencies] was not migrated: pixi models extras as features (`[feature.<name>.pypi-dependencies]` plus an environment)".into());
    }
    if doc.get("dependency-groups").is_some() {
        notes.push("[dependency-groups] was not migrated: add the groups as pixi features if you need them".into());
    }
    if doc
        .get("tool")
        .and_then(|t| t.get("poetry"))
        .and_then(|p| p.get("dependencies"))
        .is_some()
    {
        notes.push(
            "[tool.poetry.dependencies] was not migrated; only PEP 621 dependencies are read"
                .into(),
        );
    }
    (deps, requires_python, notes)
}

/// Merge declarations of the same package: every specifier must hold.
fn merge_deps(deps: Vec<PyDep>) -> Vec<PyDep> {
    let mut out: Vec<PyDep> = Vec::new();
    for dep in deps {
        match out
            .iter_mut()
            .find(|d| d.name == dep.name && d.marker == dep.marker)
        {
            Some(existing) => {
                if existing.url.is_none() && dep.url.is_some() {
                    *existing = dep;
                    continue;
                }
                if !dep.spec.is_empty() && existing.spec != dep.spec {
                    existing.spec = if existing.spec.is_empty() {
                        dep.spec.clone()
                    } else {
                        format!("{},{}", existing.spec, dep.spec)
                    };
                }
                for e in dep.extras {
                    if !existing.extras.contains(&e) {
                        existing.extras.push(e);
                    }
                }
            }
            None => out.push(dep),
        }
    }
    out
}

async fn from_python_project<S: MetadataSource, I: CondaIndex>(
    workspace: &Path,
    name: &str,
    source: &S,
    index: &I,
    platforms: &[String],
) -> Draft {
    let mut draft = Draft {
        name: name.to_string(),
        channels: vec!["conda-forge".to_string()],
        ..Draft::default()
    };
    let mut declared = Vec::new();
    let mut requires_python = None;

    let req_path = workspace.join("requirements.txt");
    if let Some(text) = read(&req_path) {
        let parsed = parse_requirements_txt(&text);
        declared.extend(parsed.deps);
        draft.skipped.extend(parsed.skipped);
        draft.index_url = parsed.index_url;
        draft.extra_index_urls = parsed.extra_index_urls;
        draft.sources.push(req_path);
    }
    let pyproject = workspace.join("pyproject.toml");
    if let Some(text) = read(&pyproject) {
        let (deps, rp, notes) = parse_pyproject(&text);
        for d in deps {
            match d {
                Ok(dep) => declared.push(dep),
                Err(skipped) => draft.skipped.push(skipped),
            }
        }
        requires_python = rp;
        draft.notes.extend(notes);
        draft.sources.push(pyproject);
    }
    let declared = merge_deps(declared);

    // Classify every dependency at once: each is a mapping lookup plus one
    // `pixi search` per platform.
    let classified =
        futures::future::join_all(declared.iter().map(|dep| classify(dep, index, platforms))).await;

    let mut conda_pythons = Vec::new();
    let mut offline = 0usize;
    let mut unverified = 0usize;
    for (dep, outcome) in declared.iter().zip(classified) {
        match outcome {
            Classified::Skip(reason) => draft.skipped.push(SkippedDep {
                line: dep.line.clone(),
                reason,
            }),
            Classified::Dep {
                dep: migrated,
                pythons,
                lookup_failed,
                verified,
            } => {
                if lookup_failed {
                    offline += 1;
                }
                if migrated.kind == DepKind::Conda && !verified {
                    unverified += 1;
                }
                if let Some(p) = pythons {
                    conda_pythons.push((migrated.name.clone(), p));
                }
                draft.deps.push(migrated);
            }
        }
    }
    if offline > 0 {
        draft.notes.push(format!(
            "The conda-forge mapping ({MAPPING_URL}) could not be reached for {offline} dependencies, so they stay on PyPI. Run this again online to move what conda-forge packages into [dependencies]."
        ));
    }
    if unverified > 0 {
        draft.notes.push(format!(
            "pixi is not installed, so {unverified} conda-forge dependencies were not checked against every target platform. `pixi install` reports any that are missing."
        ));
    }

    let (python, python_note) = pick_python(
        source,
        &declared,
        requires_python.as_deref(),
        &conda_pythons,
        draft.sources.clone(),
    )
    .await;
    draft.python = python;
    draft.notes.extend(python_note);
    let from: Vec<String> = draft
        .sources
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    draft.header = if from.is_empty() {
        "# Written by `pypilot migrate-to-pixi`.".to_string()
    } else {
        format!(
            "# Written by `pypilot migrate-to-pixi` from {} (left untouched).",
            from.join(" and ")
        )
    };
    draft
}

enum Classified {
    Skip(String),
    Dep {
        dep: MigratedDep,
        /// Pythons conda-forge builds this conda package for, on every
        /// target platform; `None` when it does not constrain Python.
        pythons: Option<PyVersionSet>,
        lookup_failed: bool,
        /// Checked against every target platform.
        verified: bool,
    },
}

async fn classify<I: CondaIndex>(dep: &PyDep, index: &I, platforms: &[String]) -> Classified {
    if let Some(marker) = &dep.marker {
        return Classified::Skip(format!(
            "environment marker `{marker}`: pixi expresses it with a [target.<platform>.pypi-dependencies] table"
        ));
    }
    if dep.name == "python" {
        return Classified::Skip("python is set by the `python` dependency".into());
    }
    let pypi = |reason: String, lookup_failed: bool| Classified::Dep {
        dep: MigratedDep {
            name: dep.name.clone(),
            value: pypi_value(dep),
            kind: DepKind::Pypi,
            from: dep.line.clone(),
            reason,
        },
        pythons: None,
        lookup_failed,
        verified: false,
    };
    if dep.url.is_some() {
        return pypi("a direct URL reference".into(), false);
    }
    if !dep.extras.is_empty() {
        return pypi(
            "extras are a PyPI feature; conda packages have none".into(),
            false,
        );
    }
    let Some(conda_spec) = pep440_to_conda(&dep.spec) else {
        return pypi(
            format!("the specifier `{}` has no exact conda equivalent", dep.spec),
            false,
        );
    };
    let conda_name = match index.conda_name(&dep.name, &dep.spec).await {
        CondaName::Found(n) => n,
        CondaName::NotPackaged => return pypi("not on conda-forge".into(), false),
        CondaName::NoMatchingVersion => {
            return pypi(
                format!("conda-forge has no release matching `{}`", dep.spec),
                false,
            )
        }
        CondaName::Unknown(_) => return pypi("conda-forge mapping unavailable".into(), true),
    };

    let mut pythons: Option<PyVersionSet> = None;
    let mut verified = true;
    for platform in platforms {
        match index.availability(&conda_name, platform, &dep.spec).await {
            Availability::Available(p) => {
                if let Some(p) = p {
                    pythons = Some(match pythons {
                        Some(acc) => acc.intersect(&p),
                        None => p,
                    });
                }
            }
            Availability::Missing => {
                return pypi(
                    format!("conda-forge has no `{conda_name}` build for {platform}"),
                    false,
                )
            }
            Availability::Unknown => verified = false,
        }
    }
    let reason = if conda_name != dep.name {
        format!("PyPI: {}", dep.name)
    } else {
        String::new()
    };
    Classified::Dep {
        dep: MigratedDep {
            name: conda_name,
            value: toml_str(&conda_spec),
            kind: DepKind::Conda,
            from: dep.line.clone(),
            reason,
        },
        pythons,
        lookup_failed: false,
        verified,
    }
}

/// The `[pypi-dependencies]` value for `dep`.
fn pypi_value(dep: &PyDep) -> String {
    let version = if dep.spec.is_empty() {
        "*".to_string()
    } else {
        dep.spec.clone()
    };
    let mut fields = Vec::new();
    if let Some(url) = &dep.url {
        match url.strip_prefix("git+") {
            Some(git) => {
                // `git+https://host/repo.git@rev`: the `@` after the path.
                let (repo, rev) = match git.rfind('@') {
                    Some(i) if git[..i].contains("://") && !git[i..].contains('/') => {
                        (&git[..i], Some(&git[i + 1..]))
                    }
                    _ => (git, None),
                };
                fields.push(format!("git = {}", toml_str(repo)));
                if let Some(rev) = rev {
                    fields.push(format!("rev = {}", toml_str(rev)));
                }
            }
            None => fields.push(format!("url = {}", toml_str(url))),
        }
    } else if dep.extras.is_empty() {
        return toml_str(&version);
    } else {
        fields.push(format!("version = {}", toml_str(&version)));
    }
    if !dep.extras.is_empty() {
        let extras: Vec<String> = dep.extras.iter().map(|e| toml_str(e)).collect();
        fields.push(format!("extras = [{}]", extras.join(", ")));
    }
    format!("{{ {} }}", fields.join(", "))
}

/// Translate a PEP 440 specifier into a conda match spec, exactly or not
/// at all.
///
/// The operators mean the same in both (`==1.2` matches 1.2 and 1.2.0 but
/// not 1.2.1 in each; `~=` and `!=1.2.*` are understood by pixi), and
/// `==1.2.*` is conda's `1.2.*`. What does not carry over is refused:
/// `===`, and versions with pre/post/dev/local parts or an epoch, which
/// conda orders differently. Those dependencies stay on PyPI.
pub fn pep440_to_conda(spec: &str) -> Option<String> {
    let spec: String = spec.chars().filter(|c| !c.is_whitespace()).collect();
    if spec.is_empty() {
        return Some("*".to_string());
    }
    let mut out = Vec::new();
    for clause in spec.split(',') {
        if clause.starts_with("===") {
            return None;
        }
        let op = ["~=", "==", "!=", "<=", ">=", "<", ">"]
            .into_iter()
            .find(|op| clause.starts_with(op))?;
        let version = &clause[op.len()..];
        let (base, wildcard) = match version.strip_suffix(".*") {
            Some(b) => (b, true),
            None => (version, false),
        };
        let plain = !base.is_empty()
            && base
                .split('.')
                .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
        if !plain || (wildcard && !matches!(op, "==" | "!=")) {
            return None;
        }
        out.push(match (op, wildcard) {
            ("==", true) => format!("{base}.*"),
            _ => format!("{op}{version}"),
        });
    }
    Some(out.join(","))
}

/// The `python` spec: the solver's recommendation over the PyPI metadata,
/// within `requires-python` and the Pythons the conda packages are built for.
async fn pick_python<S: MetadataSource>(
    source: &S,
    deps: &[PyDep],
    requires_python: Option<&str>,
    conda_pythons: &[(String, PyVersionSet)],
    sources: Vec<PathBuf>,
) -> (String, Option<String>) {
    let reqs: Vec<Requirement> = deps
        .iter()
        .filter(|d| d.url.is_none() && d.marker.is_none() && d.name != "python")
        .map(|d| Requirement {
            name: d.name.clone(),
            spec: d.spec.clone(),
        })
        .collect();
    let compat = if reqs.is_empty() {
        None
    } else {
        Some(pypi::analyze(source, Platform::current(), &reqs).await)
    };
    let resolved = compat.as_ref().filter(|c| !c.per_package.is_empty());

    let mut allowed = parse_requires_python(requires_python.unwrap_or(""));
    for (_, set) in conda_pythons {
        allowed = allowed.intersect(set);
    }
    let project = ProjectDeps {
        sources,
        packages: reqs,
        declared_requires_python: requires_python.map(str::to_string),
        ..ProjectDeps::default()
    };
    let target =
        resolved.and_then(|c| solver::synthesize(&EnvSummary::default(), &project, Some(c)).0);

    let chosen: Option<PyVersion> = match (target, resolved) {
        (Some(t), _) if allowed.contains(t) => Some(t),
        (_, Some(c)) => c.intersection.intersect(&allowed).max().or(target),
        (None, None) if !conda_pythons.is_empty() => allowed.max(),
        _ => None,
    };
    match chosen {
        Some(v) => {
            let note = if !allowed.contains(v) {
                let names: Vec<&str> = conda_pythons.iter().map(|(n, _)| n.as_str()).collect();
                Some(format!(
                    "No Python satisfies every dependency (requires-python, conda-forge builds of {} and the PyPI metadata disagree); `pixi install` will name the conflict",
                    names.join(", ")
                ))
            } else {
                target.filter(|t| *t != v).map(|t| {
                    format!(
                        "Python {v} rather than {t}: requires-python or the conda-forge builds do not cover Python {t}"
                    )
                })
            };
            (format!("{v}.*"), note)
        }
        None => {
            let spec = requires_python
                .and_then(pep440_to_conda)
                .unwrap_or_else(|| "*".to_string());
            (
                spec,
                Some("PyPI metadata was unavailable, so Python is left to pixi's solver; run `pypilot doctor` afterwards to check it".to_string()),
            )
        }
    }
}

// --- conda environment.yml --------------------------------------------------

/// What an `environment.yml` declares.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvironmentYml {
    pub name: Option<String>,
    pub channels: Vec<String>,
    /// Conda match specs as written.
    pub conda: Vec<String>,
    /// `pip:` requirements.
    pub pip: Vec<String>,
    pub variables: Vec<(String, String)>,
}

/// Parse the subset of YAML that `environment.yml` files use: top-level
/// keys, block and flow lists, the nested `pip:` list and `variables:` map.
pub fn parse_environment_yml(text: &str) -> EnvironmentYml {
    #[derive(PartialEq)]
    enum Section {
        None,
        Channels,
        Deps,
        Pip,
        Variables,
    }
    let mut env = EnvironmentYml::default();
    let mut section = Section::None;
    let mut pip_indent = 0usize;
    for raw in text.lines() {
        let line = strip_comment(raw.trim_end());
        if line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let item = line.trim();
        if indent == 0 && !item.starts_with('-') {
            let (key, value) = item.split_once(':').unwrap_or((item, ""));
            let value = value.trim();
            section = match key.trim() {
                "name" => {
                    env.name = Some(unquote(value).to_string()).filter(|n| !n.is_empty());
                    Section::None
                }
                "channels" => {
                    env.channels.extend(flow_list(value));
                    Section::Channels
                }
                "dependencies" => {
                    env.conda.extend(flow_list(value));
                    Section::Deps
                }
                "variables" => Section::Variables,
                _ => Section::None,
            };
            continue;
        }
        if section == Section::Pip && indent <= pip_indent {
            section = Section::Deps;
        }
        match section {
            Section::Channels => {
                if let Some(c) = item.strip_prefix('-') {
                    env.channels.push(unquote(c.trim()).to_string());
                }
            }
            Section::Deps => {
                if let Some(d) = item.strip_prefix('-') {
                    let d = unquote(d.trim());
                    if let Some(rest) = d.strip_prefix("pip:") {
                        pip_indent = indent;
                        env.pip.extend(flow_list(rest.trim()));
                        section = Section::Pip;
                    } else if !d.is_empty() {
                        env.conda.push(d.to_string());
                    }
                }
            }
            Section::Pip => {
                if let Some(d) = item.strip_prefix('-') {
                    env.pip.push(unquote(d.trim()).to_string());
                }
            }
            Section::Variables => {
                if let Some((k, v)) = item.split_once(':') {
                    env.variables
                        .push((k.trim().to_string(), unquote(v.trim()).to_string()));
                }
            }
            Section::None => {}
        }
    }
    env
}

fn unquote(s: &str) -> &str {
    let s = s.trim();
    for q in ['"', '\''] {
        if let Some(inner) = s.strip_prefix(q).and_then(|r| r.strip_suffix(q)) {
            return inner;
        }
    }
    s
}

/// `[a, b]` as a list; anything else is not a flow list.
fn flow_list(value: &str) -> Vec<String> {
    let Some(inner) = value.strip_prefix('[').and_then(|v| v.strip_suffix(']')) else {
        return Vec::new();
    };
    inner
        .split(',')
        .map(|s| unquote(s).to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// A conda dependency from `environment.yml` as a pixi entry: `(name,
/// value)`. conda's `pkg=1.2` (a prefix match) is pixi's `1.2.*`, a third
/// field is a build string, and `channel::pkg` pins the channel.
pub fn conda_line_to_pixi(item: &str) -> Result<(String, String, Option<String>), String> {
    let item = item.trim();
    let (channel, spec) = match item.split_once("::") {
        Some((c, s)) => (Some(c.trim().to_string()), s.trim()),
        None => (None, item),
    };
    let name_end = spec
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
        .unwrap_or(spec.len());
    let name = &spec[..name_end];
    if name.is_empty() {
        return Err("not a package name".into());
    }
    let rest = &spec[name_end..];
    if rest.trim_start().starts_with('[') {
        return Err("bracketed match spec: translate it by hand".into());
    }
    let (version, build) = if rest.trim().is_empty() {
        ("*".to_string(), None)
    } else if rest.starts_with(char::is_whitespace) {
        // `numpy 1.24.* py311_0`: the MatchSpec string form, as pixi reads it.
        let mut parts = rest.split_whitespace();
        (
            parts.next().unwrap_or("*").to_string(),
            parts.next().map(str::to_string),
        )
    } else if let Some(v) = rest.strip_prefix('=').filter(|v| !v.starts_with('=')) {
        let (v, build) = match v.split_once('=') {
            Some((v, b)) => (v, Some(b.to_string())),
            None => (v, None),
        };
        // conda's `=1.24` is a prefix match.
        let v = if v.ends_with('*') || v.is_empty() {
            v.to_string()
        } else {
            format!("{v}.*")
        };
        (if v.is_empty() { "*".to_string() } else { v }, build)
    } else {
        // `==1.24.1`, `>=1.2,<2`, optionally followed by `=build`: the
        // build separator is an `=` right after a version character.
        let rest = rest.trim();
        let bytes = rest.as_bytes();
        let split = (1..bytes.len()).find(|&i| {
            bytes[i] == b'='
                && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'*')
                && bytes.get(i + 1).is_some_and(|&n| n != b'=')
        });
        match split {
            Some(i) => (rest[..i].to_string(), Some(rest[i + 1..].to_string())),
            None => (rest.to_string(), None),
        }
    };
    let mut fields = Vec::new();
    if build.is_none() && channel.is_none() {
        return Ok((name.to_string(), toml_str(&version), None));
    }
    fields.push(format!("version = {}", toml_str(&version)));
    if let Some(b) = &build {
        fields.push(format!("build = {}", toml_str(b)));
    }
    if let Some(c) = &channel {
        fields.push(format!("channel = {}", toml_str(c)));
    }
    Ok((
        name.to_string(),
        format!("{{ {} }}", fields.join(", ")),
        channel,
    ))
}

fn from_environment_yml(text: &str, dir_name: &str, path: PathBuf) -> Draft {
    let env = parse_environment_yml(text);
    let mut draft = Draft {
        name: env.name.clone().unwrap_or_else(|| dir_name.to_string()),
        header:
            "# Written by `pypilot migrate-conda --to pixi` from environment.yml (left untouched)."
                .to_string(),
        sources: vec![path],
        python: "*".to_string(),
        activation_env: env.variables.clone(),
        ..Draft::default()
    };
    for c in &env.channels {
        match c.as_str() {
            "defaults" => draft.notes.push("The `defaults` channel was dropped: pixi does not resolve that alias, and Anaconda's repository has its own license terms. Add `https://repo.anaconda.com/pkgs/main` to channels if you need it.".into()),
            "nodefaults" => {}
            _ if !draft.channels.contains(c) => draft.channels.push(c.clone()),
            _ => {}
        }
    }
    if draft.channels.is_empty() {
        draft.channels.push("conda-forge".to_string());
    }
    let mut has_python = false;
    for item in &env.conda {
        match conda_line_to_pixi(item) {
            Ok((name, value, channel)) => {
                if let Some(c) = channel {
                    if !draft.channels.contains(&c) {
                        draft.channels.push(c);
                    }
                }
                if name.eq_ignore_ascii_case("python") {
                    has_python = true;
                    draft.python = value.trim_matches('"').to_string();
                    if value.starts_with('{') {
                        // A build or channel pin on python: keep it verbatim.
                        draft.deps.push(MigratedDep {
                            name: "python".into(),
                            value,
                            kind: DepKind::Conda,
                            from: item.clone(),
                            reason: String::new(),
                        });
                    }
                    continue;
                }
                draft.deps.push(MigratedDep {
                    name,
                    value,
                    kind: DepKind::Conda,
                    from: item.clone(),
                    reason: String::new(),
                });
            }
            Err(reason) => draft.skipped.push(SkippedDep {
                line: item.clone(),
                reason,
            }),
        }
    }
    for line in &env.pip {
        match parse_pep508(line) {
            Ok(dep) if dep.marker.is_some() => draft.skipped.push(SkippedDep {
                line: line.clone(),
                reason: "environment marker: pixi expresses it with a [target.<platform>.pypi-dependencies] table".into(),
            }),
            Ok(dep) => draft.deps.push(MigratedDep {
                name: dep.name.clone(),
                value: pypi_value(&dep),
                kind: DepKind::Pypi,
                from: line.clone(),
                reason: String::new(),
            }),
            Err(reason) => draft.skipped.push(SkippedDep {
                line: line.clone(),
                reason,
            }),
        }
    }
    if !has_python {
        draft.notes.push(
            "environment.yml does not pin python, so `python = \"*\"` lets pixi choose".into(),
        );
    }
    draft
}

// --- rendering ----------------------------------------------------------------

fn toml_str(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

fn toml_key(k: &str) -> String {
    if !k.is_empty()
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        k.to_string()
    } else {
        toml_str(k)
    }
}

fn toml_list(items: &[String]) -> String {
    let items: Vec<String> = items.iter().map(|s| toml_str(s)).collect();
    format!("[{}]", items.join(", "))
}

fn render(d: &Draft) -> String {
    let mut out = String::new();
    out.push_str(&d.header);
    out.push_str("\n# Review it, then run `pixi install`.\n\n[workspace]\n");
    out.push_str(&format!("name = {}\n", toml_str(&d.name)));
    out.push_str(&format!("channels = {}\n", toml_list(&d.channels)));
    out.push_str(&format!("platforms = {}\n", toml_list(&d.platforms)));

    let python_pinned = d.deps.iter().any(|x| x.name == "python");
    out.push_str("\n[dependencies]\n");
    if !python_pinned {
        out.push_str(&format!("python = {}\n", toml_str(&d.python)));
    }
    let line = |dep: &MigratedDep| {
        let mut l = format!("{} = {}", toml_key(&dep.name), dep.value);
        if !dep.reason.is_empty() {
            l.push_str(&format!("  # {}", dep.reason));
        }
        l.push('\n');
        l
    };
    for dep in d.deps.iter().filter(|x| x.kind == DepKind::Conda) {
        out.push_str(&line(dep));
    }
    let pypi: Vec<&MigratedDep> = d.deps.iter().filter(|x| x.kind == DepKind::Pypi).collect();
    if !pypi.is_empty() {
        out.push_str("\n[pypi-dependencies]\n");
        for dep in pypi {
            out.push_str(&line(dep));
        }
    }
    if d.index_url.is_some() || !d.extra_index_urls.is_empty() {
        out.push_str("\n[pypi-options]\n");
        if let Some(url) = &d.index_url {
            out.push_str(&format!("index-url = {}\n", toml_str(url)));
        }
        if !d.extra_index_urls.is_empty() {
            out.push_str(&format!(
                "extra-index-urls = {}\n",
                toml_list(&d.extra_index_urls)
            ));
        }
    }
    if !d.activation_env.is_empty() {
        out.push_str("\n[activation.env]\n");
        for (k, v) in &d.activation_env {
            out.push_str(&format!("{} = {}\n", toml_key(k), toml_str(v)));
        }
    }
    if !d.skipped.is_empty() {
        out.push_str("\n# Not migrated; add these by hand if you need them:\n");
        for s in &d.skipped {
            out.push_str(&format!("#   {}  ({})\n", s.line, s.reason));
        }
    }
    out
}

/// A unified-diff-style preview of a new file.
fn preview(manifest: &str) -> String {
    let lines: Vec<&str> = manifest.lines().collect();
    let mut out = format!(
        "--- /dev/null\n+++ b/{MANIFEST}\n@@ -0,0 +1,{} @@\n",
        lines.len()
    );
    for l in lines {
        out.push('+');
        out.push_str(l);
        out.push('\n');
    }
    out
}

// --- the conda-forge index ------------------------------------------------------

/// What the PyPI-to-conda mapping says about a PyPI project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CondaName {
    /// conda-forge packages it under this name.
    Found(String),
    /// conda-forge does not package it.
    NotPackaged,
    /// Packaged, but no packaged release satisfies the specifier.
    NoMatchingVersion,
    /// The mapping could not be read (offline); the reason.
    Unknown(String),
}

/// Whether conda-forge has a package for one platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// It does, built for these Pythons (`None`: not tied to a Python).
    Available(Option<PyVersionSet>),
    Missing,
    /// Could not check (pixi is not installed, or `pixi search` failed).
    Unknown,
}

/// The conda-forge lookups a migration needs. [`LiveCondaIndex`] asks
/// prefix.dev and `pixi search`; tests use recorded answers.
#[allow(async_fn_in_trait)]
pub trait CondaIndex {
    /// The conda-forge name of PyPI project `pypi_name` (normalized), for a
    /// release matching the PEP 440 `spec`.
    async fn conda_name(&self, pypi_name: &str, spec: &str) -> CondaName;
    /// Does conda-forge build `conda_name` (in a release matching the PEP 440
    /// `spec`) for `platform`?
    async fn availability(&self, conda_name: &str, platform: &str, spec: &str) -> Availability;
}

#[derive(Deserialize)]
struct MappingDoc {
    #[serde(default)]
    conda_versions: BTreeMap<String, String>,
}

/// Read a `pypi-to-conda-v1` mapping document. The conda name is the one
/// that packages the newest PyPI release the specifier admits: conda-forge
/// renames packages over time (`pytorch-cpu` before `pytorch`, `libopencv`
/// before `py-opencv`), and the newest name is the one to depend on.
pub fn conda_name_from_mapping(json: &str, spec: &str) -> CondaName {
    let Ok(doc) = serde_json::from_str::<MappingDoc>(json) else {
        return CondaName::Unknown("unreadable mapping document".into());
    };
    if doc.conda_versions.is_empty() {
        return CondaName::NotPackaged;
    }
    let wanted = VersionSpec::parse(spec);
    let chosen = wanted
        .select_newest(doc.conda_versions.keys().map(String::as_str))
        .or_else(|| {
            // Only pre-releases packaged: fine when nothing was asked for.
            wanted.is_unconstrained().then(|| {
                doc.conda_versions
                    .keys()
                    .filter_map(|k| PackageVersion::parse(k))
                    .max()
            })?
        });
    match chosen.and_then(|v| doc.conda_versions.get(&v.raw)) {
        Some(name) => CondaName::Found(name.clone()),
        None => CondaName::NoMatchingVersion,
    }
}

#[derive(Deserialize)]
struct SearchEntry {
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    depends: Vec<String>,
}

/// Read `pixi search --json --platform <p> <name>` output. pixi matches
/// fuzzily (`uvloop` also finds `hypercorn-uvloop`), so only entries named
/// exactly `conda_name`, in a release the PEP 440 `spec` admits, count.
pub fn availability_from_search(json: &str, conda_name: &str, spec: &str) -> Availability {
    let Ok(doc) = serde_json::from_str::<BTreeMap<String, Vec<SearchEntry>>>(json) else {
        return Availability::Unknown;
    };
    let wanted = VersionSpec::parse(spec);
    let entries: Vec<&SearchEntry> = doc
        .values()
        .flatten()
        .filter(|e| e.name == conda_name)
        .filter(|e| PackageVersion::parse(&e.version).map_or(true, |v| wanted.admits(&v)))
        .collect();
    if entries.is_empty() {
        return Availability::Missing;
    }
    let mut pythons = PyVersionSet::empty();
    for e in entries {
        match entry_pythons(e) {
            Some(set) => pythons = pythons.union(&set),
            None => return Availability::Available(None),
        }
    }
    Availability::Available(Some(pythons))
}

/// The Pythons one build runs on: its `python_abi 3.12.*` pin, else its
/// `python` range; `None` when it depends on neither.
fn entry_pythons(e: &SearchEntry) -> Option<PyVersionSet> {
    let mut abi = PyVersionSet::empty();
    let mut range = None;
    for d in &e.depends {
        let mut parts = d.split_whitespace();
        match (parts.next(), parts.next()) {
            (Some("python_abi"), Some(v)) => {
                if let Some(py) = PyVersion::parse(v.trim_end_matches(".*")) {
                    abi.insert(py);
                }
            }
            (Some("python"), Some(v)) => {
                // `>=3.12,<3.13.0a0`: drop conda's `.0a0` upper-bound idiom.
                let pep440 = v.replace(".0a0", "").replace("a0", "");
                range = Some(parse_requires_python(&pep440));
            }
            (Some("python"), None) => {
                range.get_or_insert_with(PyVersionSet::universe);
            }
            _ => {}
        }
    }
    if !abi.is_empty() {
        return Some(abi);
    }
    range
}

/// The live [`CondaIndex`]: prefix.dev's mapping (cached under the PyPilot
/// cache dir for `data_refresh_days`) and `pixi search`.
pub struct LiveCondaIndex {
    http: reqwest::Client,
    ttl_days: u32,
    pixi: Option<PathBuf>,
    cache_dir: PathBuf,
}

impl LiveCondaIndex {
    pub fn new(settings: &Settings, pixi: Option<PathBuf>) -> LiveCondaIndex {
        LiveCondaIndex {
            http: reqwest::Client::builder()
                .user_agent(concat!("pypilot/", env!("CARGO_PKG_VERSION")))
                .timeout(Duration::from_secs(15))
                .build()
                .expect("reqwest client with rustls should build"),
            ttl_days: settings.data_refresh_days,
            pixi,
            cache_dir: platform::cache_dir()
                .join("conda-mapping")
                .join("conda-forge"),
        }
    }

    fn cached(&self, path: &Path, any_age: bool) -> Option<String> {
        let meta = std::fs::metadata(path).ok()?;
        if !any_age {
            let age = SystemTime::now()
                .duration_since(meta.modified().ok()?)
                .ok()?;
            if age.as_secs() > u64::from(self.ttl_days) * 24 * 60 * 60 {
                return None;
            }
        }
        std::fs::read_to_string(path).ok()
    }

    async fn fetch(&self, name: &str) -> Result<String, String> {
        let url = format!("{MAPPING_URL}/{name}.json");
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(r#"{"conda_versions":{}}"#.to_string());
        }
        let text = resp
            .error_for_status()
            .map_err(|e| e.to_string())?
            .text()
            .await
            .map_err(|e| e.to_string())?;
        serde_json::from_str::<MappingDoc>(&text).map_err(|e| format!("invalid mapping: {e}"))?;
        Ok(text)
    }
}

impl CondaIndex for LiveCondaIndex {
    async fn conda_name(&self, pypi_name: &str, spec: &str) -> CondaName {
        let name = normalize_name(pypi_name);
        let path = self.cache_dir.join(format!("{name}.json"));
        if self.ttl_days == 0 {
            // Fully offline: an old answer beats none.
            return match self.cached(&path, true) {
                Some(text) => conda_name_from_mapping(&text, spec),
                None => CondaName::Unknown("offline (data_refresh_days = 0)".into()),
            };
        }
        if let Some(text) = self.cached(&path, false) {
            return conda_name_from_mapping(&text, spec);
        }
        match self.fetch(&name).await {
            Ok(text) => {
                if std::fs::create_dir_all(&self.cache_dir).is_ok() {
                    let _ = std::fs::write(&path, &text);
                }
                conda_name_from_mapping(&text, spec)
            }
            Err(e) => match self.cached(&path, true) {
                Some(text) => conda_name_from_mapping(&text, spec),
                None => CondaName::Unknown(e),
            },
        }
    }

    async fn availability(&self, conda_name: &str, platform: &str, spec: &str) -> Availability {
        let Some(pixi) = &self.pixi else {
            return Availability::Unknown;
        };
        let args = [
            "search",
            "--json",
            "--platform",
            platform,
            "--channel",
            "conda-forge",
            conda_name,
        ];
        match command::run_with_timeout(pixi, &args, None, SEARCH_TIMEOUT).await {
            Ok(out) if out.success() => availability_from_search(&out.stdout, conda_name, spec),
            Ok(out) if search_found_nothing(&out.stderr) => Availability::Missing,
            _ => Availability::Unknown,
        }
    }
}

/// `pixi search` exits 1 with "No packages found matching" for a name it
/// does not know on that platform.
pub fn search_found_nothing(stderr: &str) -> bool {
    pixi::strip_ansi(stderr).contains("No packages found matching")
}

#[cfg(test)]
#[path = "pixi_migrate_tests.rs"]
mod tests;
