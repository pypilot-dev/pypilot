//! pixi projects with more than one environment, more than one platform, and
//! a lock file: what doctor reports about them. Report only, never manage.
//!
//! A pixi environment is a list of features (`[environments] test =
//! ["test"]`), plus the default feature (the top-level tables) unless it says
//! `no-default-feature = true`. pixi merges their dependencies, and a package
//! named by several features gets every spec at once: `python = ">=3.11"` at
//! the top and `python = "3.11.*"` in a feature give 3.11, while `3.12.*` and
//! `3.11.*` together cannot be solved. [`effective`] does the same merge, so
//! one environment can be checked like the default one.
//!
//! Which environment is *the* project's (what the import diagnostics, the
//! installed-package scan and the GPU checks look at) is
//! [`selected_name`]: the `pixi_environment` setting (which the editor can
//! send as `pixiEnvironment`), unless `--pixi-env` chose another one.
//!
//! pixi solves `pixi.lock` for every platform in `[workspace] platforms`, so a
//! PyPI dependency without a wheel for one of them breaks `pixi lock` and
//! `pixi install` on every machine, the host included. [`platform_findings`]
//! catches that from PyPI metadata. [`lock_status`] asks pixi whether the
//! lock still matches the manifest with `pixi lock --check --dry-run`, which
//! neither writes `pixi.lock` nor installs anything.

use std::path::Path;
use std::sync::RwLock;
use std::time::Duration;

use crate::core::command;
use crate::core::pixi::{self, CondaDep, ManifestFormat, PixiProject};
use crate::core::pixi_compat;
use crate::core::platform::{Arch, Os, Platform};
use crate::core::project::Requirement;
use crate::core::{Finding, FixKind, Severity};
use crate::pypi::{self, CompatReport, MetadataSource, PyVersion, PyVersionSet};
use crate::settings::Settings;

/// The environment chosen for this process (`--pixi-env`), overriding every
/// setting. The editor's `pixiEnvironment` is a settings layer instead
/// ([`crate::settings::set_editor_settings`]).
static OVERRIDE: RwLock<Option<String>> = RwLock::new(None);

/// Choose the pixi environment for this process; `None` or a blank name
/// returns to the `pixi_environment` setting.
pub fn set_override(name: Option<&str>) {
    let name = name
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(String::from);
    if let Ok(mut slot) = OVERRIDE.write() {
        *slot = name;
    }
}

/// The pixi environment the user asked for: the process override, else the
/// `pixi_environment` setting (config files and editor, `default`). It may
/// not exist in the project; [`selected`] checks that.
pub fn selected_name(workspace: &Path) -> String {
    if let Some(name) = OVERRIDE.read().ok().and_then(|o| o.clone()) {
        return name;
    }
    Settings::load(workspace).pixi_environment
}

/// The project's environment: the requested one when the manifest defines
/// it, else `default`.
pub fn selected(project: &PixiProject, workspace: &Path) -> String {
    existing_or_default(project, selected_name(workspace))
}

fn existing_or_default(project: &PixiProject, wanted: String) -> String {
    if project.environments.contains(&wanted) {
        wanted
    } else {
        pixi::DEFAULT_ENV.to_string()
    }
}

/// Which features make up one environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvComposition {
    pub name: String,
    /// Non-default features, in the order the manifest lists them.
    pub features: Vec<String>,
    /// `no-default-feature = true`: the top-level tables are left out.
    pub no_default_feature: bool,
    pub solve_group: Option<String>,
}

impl EnvComposition {
    fn plain(name: &str) -> EnvComposition {
        EnvComposition {
            name: name.to_string(),
            features: Vec::new(),
            no_default_feature: false,
            solve_group: None,
        }
    }
}

/// Every environment of the project, `default` first, read from its manifest.
/// An unreadable manifest gives each environment the default feature alone.
pub fn compositions(project: &PixiProject) -> Vec<EnvComposition> {
    let parsed = std::fs::read_to_string(&project.manifest)
        .map(|t| parse_compositions(&t, project.format))
        .unwrap_or_default();
    project
        .environments
        .iter()
        .map(|name| {
            parsed
                .iter()
                .find(|c| &c.name == name)
                .cloned()
                .unwrap_or_else(|| EnvComposition::plain(name))
        })
        .collect()
}

/// The `[environments]` table of manifest text. Each value is a feature
/// list or a table with `features`, `solve-group` and `no-default-feature`.
pub fn parse_compositions(text: &str, format: ManifestFormat) -> Vec<EnvComposition> {
    let Ok(doc) = toml::from_str::<toml::Value>(text.trim_start_matches('\u{feff}')) else {
        return Vec::new();
    };
    let level = match format {
        ManifestFormat::PixiToml => Some(&doc),
        ManifestFormat::Pyproject => doc.get("tool").and_then(|t| t.get("pixi")),
    };
    let Some(envs) = level
        .and_then(|l| l.get("environments"))
        .and_then(|e| e.as_table())
    else {
        return Vec::new();
    };
    let names = |v: Option<&toml::Value>| -> Vec<String> {
        v.and_then(|f| f.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|f| f.as_str())
                    .filter(|f| *f != pixi::DEFAULT_ENV)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    };
    envs.iter()
        .map(|(name, value)| match value {
            toml::Value::Table(t) => EnvComposition {
                name: name.clone(),
                features: names(t.get("features")),
                no_default_feature: t
                    .get("no-default-feature")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                solve_group: t
                    .get("solve-group")
                    .and_then(|v| v.as_str())
                    .map(String::from),
            },
            other => EnvComposition {
                name: name.clone(),
                features: names(Some(other)),
                no_default_feature: false,
                solve_group: None,
            },
        })
        .collect()
}

/// The project as one environment sees it: its features' dependencies merged
/// over (or, with `no-default-feature`, instead of) the default ones, and
/// every `python` spec combined. Unknown feature names are ignored.
pub fn effective(project: &PixiProject, env: &EnvComposition) -> PixiProject {
    let mut out = project.clone();
    let mut pythons: Vec<String> = Vec::new();
    if env.no_default_feature {
        out.conda_deps.clear();
        out.pypi_deps.clear();
        out.skipped_pypi_deps = 0;
        out.cuda = None;
    } else if let Some(p) = &project.python {
        pythons.push(p.clone());
    }
    for name in &env.features {
        let Some(feature) = project.features.iter().find(|f| &f.name == name) else {
            continue;
        };
        if let Some(p) = &feature.python {
            pythons.push(p.clone());
        }
        for dep in &feature.conda_deps {
            merge_conda(&mut out.conda_deps, dep);
        }
        for req in &feature.pypi_deps {
            merge_pypi(&mut out.pypi_deps, req);
        }
        if feature.cuda.is_some() {
            out.cuda = feature.cuda.clone();
        }
    }
    out.python = combine_python_specs(&pythons);
    out
}

fn merge_conda(deps: &mut Vec<CondaDep>, dep: &CondaDep) {
    match deps.iter_mut().find(|d| d.name == dep.name) {
        Some(existing) => *existing = dep.clone(),
        None => deps.push(dep.clone()),
    }
}

fn merge_pypi(deps: &mut Vec<Requirement>, req: &Requirement) {
    match deps.iter_mut().find(|d| d.name == req.name) {
        Some(existing) if req.is_pinned() || !existing.is_pinned() => *existing = req.clone(),
        Some(_) => {}
        None => deps.push(req.clone()),
    }
}

/// Several conda python specs that must all hold, as one spec: joined with
/// `,` (AND). A spec with alternatives (`|`) cannot be joined that way, so
/// the last one stands alone.
fn combine_python_specs(specs: &[String]) -> Option<String> {
    let specs: Vec<&str> = specs
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && *s != "*")
        .collect();
    match specs.as_slice() {
        [] => None,
        [one] => Some(one.to_string()),
        many if many.iter().any(|s| s.contains('|') || s.contains(' ')) => {
            many.last().map(|s| s.to_string())
        }
        many => Some(many.join(",")),
    }
}

/// One environment, as doctor lists it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EnvReport {
    pub name: String,
    pub features: Vec<String>,
    /// The project's environment ([`selected`]).
    pub selected: bool,
    /// Its prefix exists under `.pixi/envs`.
    pub installed: bool,
    /// The installed Python, else the one the manifest asks for.
    pub python: Option<PyVersion>,
    /// The combined conda python spec.
    pub python_spec: Option<String>,
    pub conda: Vec<String>,
    pub pypi: Vec<String>,
}

/// Describe every environment of the project.
pub fn reports(project: &PixiProject, selected_env: &str) -> Vec<EnvReport> {
    compositions(project)
        .iter()
        .map(|c| {
            let view = effective(project, c);
            let env = pixi::env(project, &c.name);
            let installed_python = env.as_ref().and_then(|e| e.python_version);
            EnvReport {
                name: c.name.clone(),
                features: c.features.clone(),
                selected: c.name == selected_env,
                installed: env.is_some(),
                python: installed_python.or_else(|| pixi::python_minor_hint(&view)),
                python_spec: view.python.clone(),
                conda: view.conda_deps.iter().map(|d| d.name.clone()).collect(),
                pypi: view.pypi_deps.iter().map(|r| r.name.clone()).collect(),
            }
        })
        .collect()
}

/// Every pixi check that reads PyPI metadata, for [`crate::core::solver::assess`]:
/// the selected environment's PyPI dependencies against its Python (the
/// [`pixi_compat`] check), an unknown `pixi_environment`, Python drift between
/// environments, other environments whose PyPI dependencies exclude their
/// Python, and wheel gaps on the other declared platforms.
pub async fn assess<S: MetadataSource>(
    project: &PixiProject,
    workspace: &Path,
    source: &S,
) -> Vec<Finding> {
    let wanted = selected_name(workspace);
    let selected_env = selected(project, workspace);
    let comps = compositions(project);
    let reports = reports(project, &selected_env);
    let host = Platform::current();

    let mut findings = Vec::new();
    if wanted != selected_env {
        findings.push(unknown_env_finding(project, &wanted));
    }

    let selected_comp = comps
        .iter()
        .find(|c| c.name == selected_env)
        .cloned()
        .unwrap_or_else(|| EnvComposition::plain(pixi::DEFAULT_ENV));
    let view = effective(project, &selected_comp);
    let env_python = pixi::env(project, &selected_env).and_then(|e| e.python_version);
    findings.extend(pixi_compat::assess_pixi(&view, env_python, source, host).await);

    findings.extend(drift_finding(&reports));

    for (comp, report) in comps.iter().zip(&reports) {
        if comp.name == selected_env {
            continue;
        }
        let (Some(python), false) = (report.python, report.pypi.is_empty()) else {
            continue;
        };
        let other = effective(project, comp);
        let compat = pypi::analyze(source, host, &other.pypi_deps).await;
        findings.extend(other_env_finding(&comp.name, python, &compat));
    }

    let python = env_python.or_else(|| pixi::python_minor_hint(&view));
    if let (Some(python), false) = (python, view.pypi_deps.is_empty()) {
        let mut per_platform = Vec::new();
        for name in &project.platforms {
            let Some(platform) = platform_of(name).filter(|p| *p != host) else {
                continue;
            };
            let compat = pypi::analyze(source, platform, &view.pypi_deps).await;
            per_platform.push((name.clone(), compat));
        }
        findings.extend(platform_findings(project, python, &per_platform));
    }
    findings
}

fn manifest_file(project: &PixiProject) -> String {
    project.manifest.file_name().map_or_else(
        || "pixi.toml".to_string(),
        |f| f.to_string_lossy().into_owned(),
    )
}

fn unknown_env_finding(project: &PixiProject, wanted: &str) -> Finding {
    Finding {
        severity: Severity::Warning,
        title: format!("pixi environment `{wanted}` does not exist"),
        detail: format!(
            "PyPilot was asked to use the pixi environment `{wanted}`, but {} defines only {}. It uses `default` instead; change the `pixi_environment` setting (or `--pixi-env`) to one of those.",
            manifest_file(project),
            quoted_list(&project.environments)
        ),
        fix: FixKind::Manual,
    }
}

/// Info when the environments run different Python minors: often a
/// deliberate test matrix, sometimes a feature pin nobody updated.
pub fn drift_finding(reports: &[EnvReport]) -> Option<Finding> {
    let known: Vec<(&str, PyVersion)> = reports
        .iter()
        .filter_map(|r| r.python.map(|p| (r.name.as_str(), p)))
        .collect();
    let first = known.first()?.1;
    if known.iter().all(|(_, p)| *p == first) {
        return None;
    }
    let list: Vec<String> = known
        .iter()
        .map(|(name, py)| format!("`{name}` Python {py}"))
        .collect();
    Some(Finding {
        severity: Severity::Info,
        title: "pixi environments run different Python versions".to_string(),
        detail: format!(
            "{}. Code that works in one can fail in another; if that is not a deliberate test matrix, align the `python` pins in the features.",
            list.join(", ")
        ),
        fix: FixKind::Manual,
    })
}

/// A non-selected environment whose PyPI dependencies exclude its Python.
/// PyPilot does not change that environment, so the advice names the feature
/// pin rather than running anything.
pub fn other_env_finding(env: &str, python: PyVersion, compat: &CompatReport) -> Option<Finding> {
    if !PyVersionSet::universe().contains(python) || compat.is_compatible(python) {
        return None;
    }
    let blockers = compat.blockers_for(python);
    if blockers.is_empty() {
        return None;
    }
    let reasons: Vec<String> = blockers
        .iter()
        .map(|p| {
            format!(
                "{} {} supports Python {}",
                p.name,
                p.version,
                p.supported.to_range_string()
            )
        })
        .collect();
    let target = compat
        .suggested_python()
        .map(|t| format!(" Pin `python = \"{t}.*\"` in its feature to use Python {t}."))
        .unwrap_or_default();
    Some(Finding {
        severity: Severity::Warning,
        title: format!("pixi environment `{env}` runs Python {python}, which its PyPI dependencies do not support"),
        detail: format!(
            "{}, so `pixi install -e {env}` fails.{target}",
            reasons.join("; ")
        ),
        fix: FixKind::Manual,
    })
}

/// A pixi platform name as the wheel-tag [`Platform`] it installs on.
/// `None` for platforms PyPilot does not judge (32-bit, ppc64le, wasm).
pub fn platform_of(name: &str) -> Option<Platform> {
    let (os, arch) = match name {
        "linux-64" => (Os::Linux, Arch::X86_64),
        "linux-aarch64" => (Os::Linux, Arch::Aarch64),
        "osx-64" => (Os::Macos, Arch::X86_64),
        "osx-arm64" => (Os::Macos, Arch::Aarch64),
        "win-64" => (Os::Windows, Arch::X86_64),
        "win-arm64" => (Os::Windows, Arch::Aarch64),
        _ => return None,
    };
    Some(Platform { os, arch })
}

/// PyPI dependencies with no wheel for Python `python` on one of the other
/// declared platforms (and no sdist to build instead). `per_platform` pairs
/// each pixi platform name with the analysis made for it.
pub fn platform_findings(
    project: &PixiProject,
    python: PyVersion,
    per_platform: &[(String, CompatReport)],
) -> Vec<Finding> {
    if !PyVersionSet::universe().contains(python) {
        return Vec::new();
    }
    let file = manifest_file(project);
    let declared = project.platforms.join(" and ");
    let mut findings = Vec::new();
    for (platform, compat) in per_platform {
        // Excluded by wheels alone: requires_python would allow it. A
        // requires_python clash is the same on every platform and is the
        // environment check's to report.
        let gaps: Vec<_> = compat
            .per_package
            .iter()
            .filter(|p| !p.supported.contains(python) && p.declared().contains(python))
            .collect();
        if gaps.is_empty() {
            continue;
        }
        let names: Vec<&str> = gaps.iter().map(|p| p.name.as_str()).collect();
        let what: Vec<String> = gaps
            .iter()
            .map(|p| {
                if p.no_files_for_platform {
                    format!(
                        "{} {} publishes only {} wheels and no source distribution",
                        p.name,
                        p.version,
                        p.wheel_platforms.join(", ")
                    )
                } else {
                    format!(
                        "{} {} has {platform} wheels only for Python {}",
                        p.name,
                        p.version,
                        p.supported.to_range_string()
                    )
                }
            })
            .collect();
        let verb = if names.len() == 1 { "has" } else { "have" };
        findings.push(Finding {
            severity: Severity::Error,
            title: format!(
                "{file} declares {declared}, but {} {verb} no {platform} wheel for Python {python}",
                names.join(", ")
            ),
            detail: format!(
                "{}. pixi solves pixi.lock for every declared platform, so `pixi lock` and `pixi install` fail on every machine, this one included. Remove `{platform}` from `platforms`, move {} under `[target.<platform>.pypi-dependencies]` for the platforms it supports, or pick versions that ship {platform} wheels.",
                what.join("; "),
                if names.len() == 1 { "it" } else { "them" }
            ),
            fix: FixKind::Manual,
        });
    }
    findings
}

/// Whether `pixi.lock` matches the manifest. Serialized as
/// `{"status": "stale", "changes": [...]}`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum LockStatus {
    /// No `pixi.lock` yet.
    Missing,
    UpToDate,
    /// pixi would change the lock; `changes` are its summary lines
    /// (`~ (pypi) requests 2.34.2 -> 2.32.5`), one per package and
    /// environment.
    Stale {
        changes: Vec<String>,
    },
    /// The manifest no longer solves: re-locking fails. `reason` is pixi's
    /// error, on one line.
    Unsolvable {
        reason: String,
    },
    /// pixi is missing, timed out or said something else.
    Unknown,
}

/// The read-only lock check: `--dry-run` computes the lock without writing
/// it (and implies `--no-install`), `--check` makes a difference exit 1.
pub fn lock_check_args(root: &Path) -> Vec<String> {
    vec![
        "lock".to_string(),
        "--check".to_string(),
        "--dry-run".to_string(),
        "--manifest-path".to_string(),
        root.to_string_lossy().into_owned(),
    ]
}

/// Read the outcome of `pixi lock --check --dry-run` (pixi 0.81 prints its
/// summary on stderr).
pub fn classify_lock_check(success: bool, stdout: &str, stderr: &str) -> LockStatus {
    let text = pixi::strip_ansi(&format!("{stdout}\n{stderr}"));
    if success {
        return if text.contains("would not change") || !text.contains("would be updated") {
            LockStatus::UpToDate
        } else {
            LockStatus::Stale {
                changes: change_lines(&text),
            }
        };
    }
    if text.contains("would be updated") || text.contains("lock file not up-to-date") {
        return LockStatus::Stale {
            changes: change_lines(&text),
        };
    }
    let flat = pixi::unwrap_report(&text);
    match flat.split_once('×') {
        Some((_, rest)) if !rest.trim().is_empty() => LockStatus::Unsolvable {
            reason: first_sentence(rest.trim()),
        },
        _ => LockStatus::Unknown,
    }
}

/// `Environment: test` headers and `+`/`-`/`~` package lines, as
/// `test: ~ (pypi) requests 2.34.2 -> 2.32.5`.
fn change_lines(text: &str) -> Vec<String> {
    let mut env = String::new();
    let mut out = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(name) = line.strip_prefix("Environment:") {
            env = name.trim().to_string();
        } else if line.starts_with(['+', '-', '~']) && line.contains(')') {
            let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
            out.push(if env.is_empty() {
                line
            } else {
                format!("{env}: {line}")
            });
        }
    }
    out
}

/// pixi's headline (`failed to solve the pypi requirements of environment
/// 'test' for platform 'linux-64'`), without the resolver's detail.
fn first_sentence(text: &str) -> String {
    let cut = text.find(" failed to resolve").unwrap_or(text.len());
    let head = text[..cut].trim();
    let head: String = head.chars().take(200).collect();
    head
}

/// How long the lock check may take: an up-to-date lock answers in well
/// under a second, a stale one costs a solve.
const LOCK_CHECK_TIMEOUT: Duration = Duration::from_secs(90);

/// Ask pixi whether the project's `pixi.lock` is up to date. Never writes
/// the lock or installs anything (see [`lock_check_args`]).
pub async fn lock_status(root: &Path) -> LockStatus {
    if !root.join("pixi.lock").is_file() {
        return LockStatus::Missing;
    }
    let Some(pixi) = pixi::locate().await else {
        return LockStatus::Unknown;
    };
    let args = lock_check_args(root);
    match command::run_with_timeout(&pixi, &args, Some(root), LOCK_CHECK_TIMEOUT).await {
        Ok(out) => classify_lock_check(out.success(), &out.stdout, &out.stderr),
        Err(_) => LockStatus::Unknown,
    }
}

/// The doctor finding for a lock status, if it needs one.
pub fn lock_finding(project: &PixiProject, status: &LockStatus) -> Option<Finding> {
    let file = manifest_file(project);
    match status {
        LockStatus::UpToDate | LockStatus::Unknown => None,
        LockStatus::Missing => Some(Finding {
            severity: Severity::Info,
            title: "This pixi project has no pixi.lock yet".to_string(),
            detail: "Without a lock file every install may resolve different versions. `pixi lock` (or `pixi install`) writes one; commit it.".to_string(),
            fix: FixKind::Manual,
        }),
        LockStatus::Stale { changes } => {
            const SHOWN: usize = 5;
            let mut list = changes.iter().take(SHOWN).cloned().collect::<Vec<_>>().join("; ");
            if changes.len() > SHOWN {
                list.push_str(&format!("; and {} more", changes.len() - SHOWN));
            }
            let what = if list.is_empty() {
                String::new()
            } else {
                format!(" Re-locking would change: {list}.")
            };
            Some(Finding {
                severity: Severity::Warning,
                title: format!("pixi.lock is out of date with {file}"),
                detail: format!(
                    "{file} changed since pixi.lock was written, so the installed environments may not match it.{what} Run `pixi lock` to update the lock, or `pixi install` to update it and the environment."
                ),
                fix: FixKind::Manual,
            })
        }
        LockStatus::Unsolvable { reason } => Some(Finding {
            severity: Severity::Error,
            title: format!("pixi.lock is out of date and {file} no longer solves"),
            detail: format!(
                "Re-locking fails: {reason}. Fix the manifest (see the other findings), then run `pixi lock`."
            ),
            fix: FixKind::Manual,
        }),
    }
}

fn quoted_list(items: &[String]) -> String {
    items
        .iter()
        .map(|i| format!("`{i}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::pixi::parse_manifest;
    use crate::pypi::metadata::{DistFile, Info, PackageMetadata};
    use crate::pypi::FixtureSource;
    use std::path::PathBuf;

    const TWO_ENVS: &str = include_str!("../../tests/fixtures/pixi/pixi-0.81-two-envs.pixi.toml");
    const LOCK_FRESH: &str =
        include_str!("../../tests/fixtures/pixi/pixi-0.81-lock-check-fresh.txt");
    const LOCK_STALE: &str =
        include_str!("../../tests/fixtures/pixi/pixi-0.81-lock-check-stale.txt");
    const LOCK_UNSOLVABLE: &str =
        include_str!("../../tests/fixtures/pixi/pixi-0.81-lock-check-unsolvable.txt");

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pypilot-pixi-envs-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn project_in(dir: &Path, manifest: &str) -> PixiProject {
        std::fs::write(dir.join("pixi.toml"), manifest).unwrap();
        pixi::detect(dir).unwrap()
    }

    fn install_env(root: &Path, name: &str, python: &str) {
        let meta = root
            .join(".pixi")
            .join("envs")
            .join(name)
            .join("conda-meta");
        std::fs::create_dir_all(&meta).unwrap();
        std::fs::write(
            meta.join(format!("python-{python}-h1_0_cpython.json")),
            "{}",
        )
        .unwrap();
    }

    fn pkg(name: &str, version: &str, rp: Option<&str>, files: &[&str]) -> PackageMetadata {
        PackageMetadata {
            info: Info {
                name: name.into(),
                version: version.into(),
                requires_python: rp.map(String::from),
            },
            urls: files
                .iter()
                .map(|f| DistFile {
                    filename: f.to_string(),
                    packagetype: if f.ends_with(".whl") {
                        "bdist_wheel".into()
                    } else {
                        "sdist".into()
                    },
                    requires_python: None,
                    yanked: false,
                })
                .collect(),
            versions: vec![version.into()],
        }
    }

    /// pywin32 312: Windows wheels only, no sdist.
    fn pywin32() -> PackageMetadata {
        pkg(
            "pywin32",
            "312",
            None,
            &[
                "pywin32-312-cp311-cp311-win_amd64.whl",
                "pywin32-312-cp312-cp312-win_amd64.whl",
                "pywin32-312-cp312-cp312-win_arm64.whl",
            ],
        )
    }

    fn requests() -> PackageMetadata {
        pkg(
            "requests",
            "2.34.2",
            Some(">=3.9"),
            &["requests-2.34.2-py3-none-any.whl", "requests-2.34.2.tar.gz"],
        )
    }

    #[test]
    fn real_two_env_manifest() {
        let comps = parse_compositions(TWO_ENVS, ManifestFormat::PixiToml);
        assert_eq!(comps.len(), 2);
        let default = comps.iter().find(|c| c.name == "default").unwrap();
        assert_eq!(default.features, ["py312"]);
        let test = comps.iter().find(|c| c.name == "test").unwrap();
        assert_eq!(test.features, ["test"]);
        assert!(!test.no_default_feature);

        let p = parse_manifest(
            TWO_ENVS,
            ManifestFormat::PixiToml,
            Path::new("."),
            "pixi.toml".into(),
        )
        .unwrap();
        assert_eq!(p.platforms, ["win-64", "linux-64"]);
        let d = effective(&p, default);
        assert_eq!(d.python.as_deref(), Some(">=3.11,<3.13,3.12.*"));
        assert_eq!(pixi::python_minor_hint(&d), Some(PyVersion::py3(12)));
        let t = effective(&p, test);
        assert_eq!(pixi::python_minor_hint(&t), Some(PyVersion::py3(11)));
        assert!(t.conda_deps.iter().any(|c| c.name == "pytest"));
        assert_eq!(t.pypi_deps[0].name, "requests");
    }

    #[test]
    fn table_form_and_no_default_feature() {
        let text = r#"
[tool.pixi.workspace]
platforms = ["linux-64"]
[tool.pixi.dependencies]
python = "3.12.*"
numpy = "*"
[tool.pixi.pypi-dependencies]
requests = "*"
[tool.pixi.feature.lint.dependencies]
ruff = "*"
[tool.pixi.feature.lint.pypi-dependencies]
requests = ">=2.32"
[tool.pixi.environments]
lint = { features = ["lint"], no-default-feature = true, solve-group = "default" }
dev = { features = ["lint"] }
"#;
        let comps = parse_compositions(text, ManifestFormat::Pyproject);
        let lint = comps.iter().find(|c| c.name == "lint").unwrap();
        assert!(lint.no_default_feature);
        assert_eq!(lint.solve_group.as_deref(), Some("default"));
        let p = parse_manifest(
            text,
            ManifestFormat::Pyproject,
            Path::new("."),
            "pyproject.toml".into(),
        )
        .unwrap();
        let l = effective(&p, lint);
        assert_eq!(l.python, None);
        assert_eq!(
            l.conda_deps
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["ruff"]
        );
        let dev = effective(&p, comps.iter().find(|c| c.name == "dev").unwrap());
        assert_eq!(dev.python.as_deref(), Some("3.12.*"));
        assert_eq!(dev.conda_deps.len(), 2);
        // The pinned feature spec replaces the bare default one.
        assert_eq!(dev.pypi_deps.len(), 1);
        assert_eq!(dev.pypi_deps[0].spec, ">=2.32");
        assert!(parse_compositions("not toml [", ManifestFormat::PixiToml).is_empty());
    }

    #[test]
    fn python_specs_combine() {
        let s =
            |v: &[&str]| combine_python_specs(&v.iter().map(|x| x.to_string()).collect::<Vec<_>>());
        assert_eq!(s(&[]), None);
        assert_eq!(s(&["*", "3.11.*"]).as_deref(), Some("3.11.*"));
        assert_eq!(s(&[">=3.10", "3.11.*"]).as_deref(), Some(">=3.10,3.11.*"));
        assert_eq!(
            s(&[">=3.10", "3.11.*|3.12.*"]).as_deref(),
            Some("3.11.*|3.12.*")
        );
        // Contradictory pins combine to a spec no Python satisfies.
        assert_eq!(
            s(&["3.12.*", "3.11.*"])
                .as_deref()
                .and_then(pixi::python_spec_minor),
            None
        );
    }

    #[test]
    fn reports_and_drift() {
        let root = scratch("reports");
        let p = project_in(&root, TWO_ENVS);
        install_env(&root, "default", "3.12.15");
        let r = reports(&p, "default");
        assert_eq!(r.len(), 2);
        assert!(r[0].selected && r[0].installed);
        assert_eq!(r[0].python, Some(PyVersion::py3(12)));
        // Not installed: the manifest's pin.
        assert!(!r[1].installed && !r[1].selected);
        assert_eq!(r[1].python, Some(PyVersion::py3(11)));
        assert_eq!(r[1].conda, ["pytest"]);
        let drift = drift_finding(&r).unwrap();
        assert_eq!(drift.severity, Severity::Info);
        assert!(drift
            .detail
            .contains("`default` Python 3.12, `test` Python 3.11"));

        let same: Vec<EnvReport> = r
            .iter()
            .cloned()
            .map(|mut e| {
                e.python = Some(PyVersion::py3(12));
                e
            })
            .collect();
        assert_eq!(drift_finding(&same), None);
    }

    #[test]
    fn selection_falls_back_to_default() {
        let root = scratch("select");
        let p = project_in(&root, TWO_ENVS);
        // The process override is global, so it is not set here (tests share
        // a process under `cargo test`).
        assert_eq!(selected(&p, &root), "default");
        assert_eq!(existing_or_default(&p, "test".into()), "test");
        assert_eq!(existing_or_default(&p, "nope".into()), "default");
        std::fs::write(root.join("pypilot.toml"), "pixi_environment = \"test\"\n").unwrap();
        assert_eq!(selected(&p, &root), "test");
        let f = unknown_env_finding(&p, "nope");
        assert!(f.detail.contains("`default`, `test`"), "{}", f.detail);
    }

    #[test]
    fn platform_without_wheel_is_an_error() {
        let p = parse_manifest(
            TWO_ENVS,
            ManifestFormat::PixiToml,
            Path::new("."),
            "pixi.toml".into(),
        )
        .unwrap();
        let linux = platform_of("linux-64").unwrap();
        let windows = platform_of("win-64").unwrap();
        let report = |m: PackageMetadata, plat: Platform| CompatReport {
            platform: plat,
            per_package: vec![m.analyze(&plat)],
            unresolved: Vec::new(),
            intersection: PyVersionSet::universe(),
            conflicts: Vec::new(),
            sdist_warnings: Vec::new(),
        };
        let py = PyVersion::py3(12);
        let f = platform_findings(&p, py, &[("linux-64".into(), report(pywin32(), linux))]);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::Error);
        assert_eq!(
            f[0].title,
            "pixi.toml declares win-64 and linux-64, but pywin32 has no linux-64 wheel for Python 3.12"
        );
        assert!(
            f[0].detail
                .contains("only Windows arm64, Windows x64 wheels"),
            "{}",
            f[0].detail
        );

        // Wheels for other Pythons on that platform.
        let cp311_only = pkg(
            "fastlib",
            "1.0",
            None,
            &[
                "fastlib-1.0-cp311-cp311-win_amd64.whl",
                "fastlib-1.0-cp312-cp312-manylinux_2_17_x86_64.whl",
            ],
        );
        let f = platform_findings(&p, py, &[("win-64".into(), report(cp311_only, windows))]);
        assert!(
            f[0].detail
                .contains("has win-64 wheels only for Python 3.11"),
            "{}",
            f[0].detail
        );

        // Pure-Python or sdist packages are fine everywhere.
        assert!(
            platform_findings(&p, py, &[("linux-64".into(), report(requests(), linux))]).is_empty()
        );
        let sdist = pkg(
            "native",
            "1.0",
            None,
            &["native-1.0-cp312-cp312-win_amd64.whl", "native-1.0.tar.gz"],
        );
        assert!(platform_findings(&p, py, &[("linux-64".into(), report(sdist, linux))]).is_empty());

        assert_eq!(platform_of("linux-ppc64le"), None);
        assert_eq!(platform_of("osx-arm64").unwrap().label(), "macOS arm64");
    }

    #[tokio::test]
    async fn assess_reports_every_declared_platform() {
        let root = scratch("assess");
        let manifest = TWO_ENVS.replace(
            "requests = \">=2.31\"",
            "requests = \">=2.31\"\npywin32 = \"*\"",
        );
        let p = project_in(&root, &manifest);
        let mut source = FixtureSource::new();
        source.insert("requests", requests());
        source.insert("pywin32", pywin32());
        let findings = assess(&p, &root, &source).await;
        let titles: Vec<&str> = findings.iter().map(|f| f.title.as_str()).collect();
        assert!(
            titles.contains(&"pixi environments run different Python versions"),
            "{titles:?}"
        );
        // The host platform is pixi_compat's to judge; every other declared
        // platform lacking a wheel is reported here.
        let host_is_windows = Platform::current().os == Os::Windows;
        let linux_gap = titles
            .iter()
            .any(|t| t.contains("no linux-64 wheel for Python 3.12"));
        let windows_gap = titles.iter().any(|t| t.contains("no win-64 wheel"));
        assert!(!windows_gap);
        if host_is_windows || Platform::current() != platform_of("linux-64").unwrap() {
            assert!(linux_gap, "{titles:?}");
        }
    }

    #[test]
    fn other_env_with_unsupported_python() {
        let modern = pkg(
            "modern",
            "3.0",
            Some(">=3.12"),
            &["modern-3.0-py3-none-any.whl"],
        );
        let host = Platform::current();
        let compat = CompatReport {
            platform: host,
            per_package: vec![modern.analyze(&host)],
            unresolved: Vec::new(),
            intersection: modern.analyze(&host).supported,
            conflicts: Vec::new(),
            sdist_warnings: Vec::new(),
        };
        let f = other_env_finding("test", PyVersion::py3(11), &compat).unwrap();
        assert_eq!(f.severity, Severity::Warning);
        assert!(f.title.contains("`test` runs Python 3.11"));
        assert!(f.detail.contains("modern 3.0 supports Python 3.12"));
        assert!(f.detail.contains("pixi install -e test"));
        assert_eq!(other_env_finding("test", PyVersion::py3(12), &compat), None);
    }

    /// Output of `pixi lock --check --dry-run` from pixi 0.81.0.
    #[test]
    fn classifies_real_lock_checks() {
        assert_eq!(
            classify_lock_check(true, "", LOCK_FRESH),
            LockStatus::UpToDate
        );
        let LockStatus::Stale { changes } = classify_lock_check(false, "", LOCK_STALE) else {
            panic!("not stale");
        };
        assert_eq!(
            changes,
            [
                "default: ~ (pypi) requests 2.34.2 -> 2.32.5",
                "test: ~ (pypi) requests 2.34.2 -> 2.32.5"
            ]
        );
        assert_eq!(
            classify_lock_check(false, "", LOCK_UNSOLVABLE),
            LockStatus::Unsolvable {
                reason: "failed to solve the pypi requirements of environment 'test' for platform 'linux-64'".into()
            }
        );
        assert_eq!(classify_lock_check(false, "", ""), LockStatus::Unknown);
        assert_eq!(
            lock_check_args(Path::new("proj")),
            ["lock", "--check", "--dry-run", "--manifest-path", "proj"]
        );
    }

    #[test]
    fn lock_findings() {
        let p = parse_manifest(
            TWO_ENVS,
            ManifestFormat::PixiToml,
            Path::new("."),
            "pixi.toml".into(),
        )
        .unwrap();
        assert_eq!(lock_finding(&p, &LockStatus::UpToDate), None);
        assert_eq!(lock_finding(&p, &LockStatus::Unknown), None);
        let stale = classify_lock_check(false, "", LOCK_STALE);
        let f = lock_finding(&p, &stale).unwrap();
        assert_eq!(f.severity, Severity::Warning);
        assert_eq!(f.title, "pixi.lock is out of date with pixi.toml");
        assert!(f
            .detail
            .contains("default: ~ (pypi) requests 2.34.2 -> 2.32.5"));
        assert!(f.detail.contains("`pixi lock`") && f.detail.contains("`pixi install`"));
        let many = LockStatus::Stale {
            changes: (0..8).map(|i| format!("+ (pypi) p{i}")).collect(),
        };
        assert!(lock_finding(&p, &many)
            .unwrap()
            .detail
            .contains("and 3 more"));
        let f = lock_finding(&p, &classify_lock_check(false, "", LOCK_UNSOLVABLE)).unwrap();
        assert_eq!(f.severity, Severity::Error);
        assert!(f.detail.contains("platform 'linux-64'"));
        assert_eq!(
            lock_finding(&p, &LockStatus::Missing).unwrap().severity,
            Severity::Info
        );
    }

    /// `pixi info --json` for the two-environment fixture project, home
    /// directory redacted: every environment and its prefix is found.
    #[test]
    fn real_two_env_info_json() {
        let text = include_str!("../../tests/fixtures/pixi/pixi-0.81-info-two-envs.json");
        let envs = pixi::parse_info_json(text);
        let names: Vec<&str> = envs.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["default", "test"]);
        assert!(envs[1].1.to_string_lossy().ends_with(".pixi\\envs\\test"));
        // The manifest pixi read agrees with ours.
        let comps = parse_compositions(TWO_ENVS, ManifestFormat::PixiToml);
        let mut ours: Vec<&str> = comps.iter().map(|c| c.name.as_str()).collect();
        ours.sort();
        assert_eq!(ours, names);
    }

    #[tokio::test]
    async fn missing_lock_is_not_run() {
        let root = scratch("no-lock");
        assert_eq!(lock_status(&root).await, LockStatus::Missing);
    }
}
