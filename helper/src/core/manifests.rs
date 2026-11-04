//! Findings about the manifests themselves, before any environment is built.
//!
//! A project can declare its dependencies in ways the uv flow cannot use as
//! they stand: a `requirements.txt` pip cannot read, a pyproject.toml and a
//! requirements.txt that pin the same package differently, two lock files
//! from different tools, a Poetry project uv does not read, an
//! `environment.yml` only conda reads, or a `requires-python` floor that a
//! dependency's own floor rules out. Each gets a finding that names the file,
//! the package and what to run, and [`crate::core::foreign_setup`] makes
//! `setup` act on them instead of running a uv setup that would install
//! nothing or overwrite the user's pins.
//!
//! Everything here reads files (and PyPI metadata, cached); nothing changes.

use std::path::Path;

use crate::core::probe::VenvInfo;
use crate::core::project::{self, ProjectDeps, Requirement};
use crate::core::{Finding, FixKind, Severity};
use crate::pypi::pyversion::{parse_requires_python, PyVersion};
use crate::pypi::version::{PackageVersion, VersionSpec};
use crate::pypi::{CompatReport, MetadataSource};
use crate::settings::{PackageManager, Settings};

/// Add the manifest findings for the project at `workspace` to `findings`
/// (see the module docs). Called by [`crate::core::solver::assess`].
pub async fn apply<S: MetadataSource>(
    findings: &mut Vec<Finding>,
    workspace: &Path,
    settings: &Settings,
    project: &ProjectDeps,
    compat: Option<&CompatReport>,
    venv: Option<&VenvInfo>,
    source: &S,
) {
    if project.is_pixi() {
        // pixi reads its own manifest; a requirements.txt or lock beside it
        // is not what it installs from.
        return;
    }
    let pyproject = read(&workspace.join("pyproject.toml"));
    let requirements = read(&workspace.join("requirements.txt"));

    if let Some(text) = &requirements {
        if let Some(f) = invalid_requirements_finding(text) {
            findings.push(f);
        }
    }
    if let (Some(py), Some(req)) = (&pyproject, &requirements) {
        if let Some(f) = pin_conflict_finding(&pin_conflicts(py, req)) {
            findings.push(f);
        }
    }
    let poetry = pyproject.as_deref().is_some_and(is_poetry_only);
    if let Some(f) = lockfile_finding(&lockfiles(workspace), poetry, settings) {
        findings.push(f);
    }
    if poetry {
        let poetry_bin = poetry_available().await;
        apply_poetry(findings, project, venv, poetry_bin);
    }
    if let Some(text) = &pyproject {
        if let Some(f) = python_floor_finding(text, project, compat, source).await {
            findings.push(f);
        }
    }
    if let Some(f) = imports_finding(workspace, project, venv) {
        findings.push(f);
    }
    if is_conda_only(workspace, project) {
        let pixi = conda_goes_to_pixi(settings).await;
        apply_conda(findings, project, venv.is_some(), pixi);
    }
}

fn read(path: &Path) -> Option<String> {
    path.is_file()
        .then(|| project::read_manifest(path))
        .flatten()
}

// --- requirements.txt -------------------------------------------------------

/// A requirements.txt line pip cannot read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidLine {
    /// 1-based, the first physical line of a `\`-continued one.
    pub line: usize,
    pub text: String,
    pub reason: String,
}

/// Every requirement line of a requirements.txt that pip would reject.
/// Options (`-r`, `-e`, `--index-url`), URLs and local paths are left to pip.
pub fn invalid_requirements(text: &str) -> Vec<InvalidLine> {
    let mut out = Vec::new();
    let mut pending: Option<(usize, String)> = None;
    for (i, raw) in text.lines().enumerate() {
        let (start, mut joined) = pending.take().unwrap_or((i + 1, String::new()));
        let line = raw.trim_end();
        if let Some(head) = line.strip_suffix('\\') {
            joined.push_str(head);
            joined.push(' ');
            pending = Some((start, joined));
            continue;
        }
        joined.push_str(line);
        if let Some(reason) = invalid_requirement(&joined) {
            out.push(InvalidLine {
                line: start,
                text: joined.trim().to_string(),
                reason,
            });
        }
    }
    out
}

/// Why one logical requirements.txt line is not a requirement, or `None`
/// when pip can read it.
fn invalid_requirement(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') || line.starts_with('-') {
        return None;
    }
    let line = match line
        .as_bytes()
        .windows(2)
        .position(|w| w[0].is_ascii_whitespace() && w[1] == b'#')
    {
        Some(i) => line[..i].trim_end(),
        None => line,
    };
    // A URL, an archive or a local path is pip's to judge.
    if line.contains("://")
        || line.starts_with(['.', '/', '\\', '~'])
        || line.ends_with(".whl")
        || line.ends_with(".tar.gz")
        || line.ends_with(".zip")
    {
        return None;
    }
    if line.starts_with(['=', '<', '>', '!', '~', ';', '[', '@']) {
        return Some("it has no package name".to_string());
    }
    let dep = match crate::core::pixi_migrate::parse_pep508(line) {
        Ok(dep) => dep,
        Err(e) => return Some(e),
    };
    if dep.url.is_some() || dep.spec.is_empty() {
        return None;
    }
    for clause in dep.spec.split(',') {
        if let Some(reason) = invalid_clause(clause) {
            return Some(reason);
        }
    }
    None
}

fn invalid_clause(clause: &str) -> Option<String> {
    const OPS: [&str; 8] = ["===", "==", "!=", "<=", ">=", "~=", "<", ">"];
    let clause = clause.trim();
    if clause.is_empty() {
        return Some("it has an empty version clause".to_string());
    }
    let Some(op) = OPS.iter().find(|op| clause.starts_with(**op)) else {
        return Some(format!("`{clause}` is not a version comparison"));
    };
    let version = &clause[op.len()..];
    if version.is_empty() {
        return Some(format!("`{op}` has no version after it"));
    }
    let valid = version.starts_with(|c: char| c.is_ascii_digit() || c == 'v')
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '*' | '+' | '!' | '-' | '_'));
    (!valid).then(|| format!("`{version}` is not a version"))
}

fn invalid_requirements_finding(text: &str) -> Option<Finding> {
    let bad = invalid_requirements(text);
    if bad.is_empty() {
        return None;
    }
    let lines: Vec<String> = bad.iter().map(|b| b.line.to_string()).collect();
    let title = match lines.as_slice() {
        [one] => format!("requirements.txt line {one} is not a valid requirement"),
        [init @ .., last] => format!(
            "requirements.txt lines {} and {last} are not valid requirements",
            init.join(", ")
        ),
        [] => unreachable!(),
    };
    let each: Vec<String> = bad
        .iter()
        .map(|b| format!("line {} `{}`: {}", b.line, b.text, b.reason))
        .collect();
    Some(Finding {
        severity: Severity::Error,
        title,
        detail: format!(
            "{}. pip and uv stop at the first line they cannot read, so nothing in requirements.txt installs until {} fixed or removed.",
            each.join("; "),
            if bad.len() == 1 { "it is" } else { "they are" }
        ),
        fix: FixKind::Manual,
    })
}

// --- pyproject.toml against requirements.txt ----------------------------------

/// One package pinned in both files in ways that cannot both hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinConflict {
    pub name: String,
    pub pyproject: String,
    pub requirements: String,
}

/// The packages pyproject.toml and requirements.txt constrain in ways no
/// single version satisfies: one side pins an exact version the other side's
/// constraint rules out. Different but overlapping ranges are not a conflict.
pub fn pin_conflicts(pyproject: &str, requirements: &str) -> Vec<PinConflict> {
    let (declared, _) = project::parse_pyproject(pyproject);
    let listed = project::parse_requirements(requirements);
    let mut out: Vec<PinConflict> = Vec::new();
    for r in &listed {
        let Some(d) = declared.iter().find(|d| d.name == r.name) else {
            continue;
        };
        if d.spec.is_empty() || r.spec.is_empty() || d.spec == r.spec {
            continue;
        }
        if disagree(&d.spec, &r.spec) && !out.iter().any(|c| c.name == r.name) {
            out.push(PinConflict {
                name: r.name.clone(),
                pyproject: d.spec.clone(),
                requirements: r.spec.clone(),
            });
        }
    }
    out
}

/// The version an `==X` spec pins, when it pins exactly one.
fn exact_pin(spec: &str) -> Option<PackageVersion> {
    let v = spec.strip_prefix("==")?;
    if v.contains([',', '*']) || v.starts_with('=') {
        return None;
    }
    PackageVersion::parse(v)
}

fn disagree(a: &str, b: &str) -> bool {
    let rules_out = |pin: &str, other: &str| {
        exact_pin(pin).is_some_and(|v| !VersionSpec::parse(other).admits(&v))
    };
    rules_out(a, b) || rules_out(b, a)
}

fn pin_conflict_finding(conflicts: &[PinConflict]) -> Option<Finding> {
    if conflicts.is_empty() {
        return None;
    }
    let names: Vec<&str> = conflicts.iter().map(|c| c.name.as_str()).collect();
    let each: Vec<String> = conflicts
        .iter()
        .map(|c| {
            format!(
                "{} is `{}` in pyproject.toml but `{}` in requirements.txt",
                c.name, c.pyproject, c.requirements
            )
        })
        .collect();
    Some(Finding {
        severity: Severity::Warning,
        title: format!(
            "pyproject.toml and requirements.txt disagree on {}",
            names.join(", ")
        ),
        detail: format!(
            "{}. PyPilot follows pyproject.toml, the project's declaration, and setup will not copy requirements.txt's pins over it. If requirements.txt is the one that is right, change pyproject.toml; otherwise regenerate it with `uv export --format requirements-txt --no-hashes -o requirements.txt`.",
            each.join("; ")
        ),
        fix: FixKind::Manual,
    })
}

/// How requirements.txt relates to pyproject.toml when setup installs both.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequirementsMerge {
    /// Some requirements.txt package is already declared in pyproject.toml.
    pub overlaps: bool,
    /// requirements.txt lines for packages pyproject.toml does not declare,
    /// as `uv add` arguments.
    pub extra: Vec<String>,
    /// Pins that disagree (see [`pin_conflicts`]).
    pub conflicts: Vec<PinConflict>,
}

/// [`RequirementsMerge`] for the project at `workspace`. A pyproject.toml
/// that does not exist yet (a dry run's `uv init`) declares nothing.
pub fn requirements_merge(workspace: &Path) -> RequirementsMerge {
    let Some(req) = read(&workspace.join("requirements.txt")) else {
        return RequirementsMerge::default();
    };
    let py = read(&workspace.join("pyproject.toml")).unwrap_or_default();
    let (declared, _) = project::parse_pyproject(&py);
    let mut merge = RequirementsMerge {
        conflicts: pin_conflicts(&py, &req),
        ..RequirementsMerge::default()
    };
    for line in req.lines() {
        let Some(r) = project::parse_requirement(line) else {
            continue;
        };
        if declared.iter().any(|d| d.name == r.name) {
            merge.overlaps = true;
            continue;
        }
        let text = match line.find(" #") {
            Some(i) => &line[..i],
            None => line,
        };
        merge.extra.push(text.trim().to_string());
    }
    merge
}

// --- lock files ---------------------------------------------------------------

/// Lock files PyPilot recognizes, and the tool that writes each.
const LOCKFILES: [(&str, &str); 4] = [
    ("uv.lock", "uv"),
    ("poetry.lock", "Poetry"),
    ("pdm.lock", "PDM"),
    ("Pipfile.lock", "Pipenv"),
];

/// The lock files present at the project root.
pub fn lockfiles(workspace: &Path) -> Vec<&'static str> {
    LOCKFILES
        .iter()
        .map(|(f, _)| *f)
        .filter(|f| workspace.join(f).is_file())
        .collect()
}

fn lockfile_finding(present: &[&str], poetry_only: bool, settings: &Settings) -> Option<Finding> {
    if present.len() < 2 {
        return None;
    }
    let followed = if poetry_only {
        Some("poetry.lock")
    } else if matches!(
        settings.package_manager,
        PackageManager::Pip | PackageManager::UvPip
    ) {
        None
    } else {
        Some("uv.lock")
    };
    let list = match present {
        [a, b] => format!("{a} and {b}"),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
        [] => unreachable!(),
    };
    let why = match followed {
        Some("poetry.lock") => "PyPilot follows poetry.lock: the dependencies are declared in [tool.poetry], so `poetry install` is what installs them.".to_string(),
        Some(lock) => format!(
            "PyPilot follows {lock}: this is a uv project, and `uv sync` installs from it."
        ),
        None => "PyPilot follows neither: package_manager is set to pip, so it installs requirements.txt.".to_string(),
    };
    let others: Vec<&str> = present
        .iter()
        .copied()
        .filter(|f| Some(*f) != followed)
        .collect();
    Some(Finding {
        severity: Severity::Info,
        title: format!("{list} are both present"),
        detail: format!(
            "{why} {} left untouched, so the two can drift apart; delete the one the project no longer uses.",
            if others.len() == 1 {
                format!("{} is", others[0])
            } else {
                format!("{} are", others.join(", "))
            }
        ),
        fix: FixKind::Manual,
    })
}

// --- Poetry ---------------------------------------------------------------------

/// A pyproject.toml whose dependencies only Poetry reads: packages in
/// `[tool.poetry.dependencies]` (or a Poetry group) and no PEP 621
/// `[project] dependencies`. uv and pip ignore `[tool.poetry]`.
pub fn is_poetry_only(pyproject: &str) -> bool {
    let Ok(doc) = toml::from_str::<toml::Value>(pyproject) else {
        return false;
    };
    let pep621 = doc
        .get("project")
        .and_then(|p| p.get("dependencies"))
        .is_some();
    let Some(poetry) = doc.get("tool").and_then(|t| t.get("poetry")) else {
        return false;
    };
    let has_packages = |t: Option<&toml::Value>| {
        t.and_then(|d| d.as_table())
            .is_some_and(|d| d.keys().any(|k| !k.eq_ignore_ascii_case("python")))
    };
    let groups = poetry
        .get("group")
        .and_then(|g| g.as_table())
        .is_some_and(|g| g.values().any(|g| has_packages(g.get("dependencies"))));
    !pep621 && (has_packages(poetry.get("dependencies")) || groups)
}

/// Is Poetry on PATH?
pub async fn poetry_available() -> bool {
    crate::core::command::probe_version("poetry", &["--version"])
        .await
        .is_some()
}

/// The titles of the generic "nothing set up yet" findings, which a Poetry
/// or conda finding replaces: their fix, a uv setup, is the wrong one there.
const NO_ENV_TITLES: [&str; 2] = ["No environment set up yet", "No virtual environment"];

fn apply_poetry(
    findings: &mut Vec<Finding>,
    project: &ProjectDeps,
    venv: Option<&VenvInfo>,
    poetry: bool,
) {
    findings.retain(|f| !NO_ENV_TITLES.contains(&f.title.as_str()));
    let missing: Vec<String> = match venv {
        Some(v) => {
            let installed = crate::core::installed::scan(&v.path);
            project
                .packages
                .iter()
                .filter(|r| !installed.contains(&r.name))
                .map(|r| r.name.clone())
                .collect()
        }
        None => project.names(),
    };
    findings.push(poetry_finding(venv.is_some(), &missing, poetry));
}

fn poetry_finding(has_venv: bool, missing: &[String], poetry: bool) -> Finding {
    let state = if !has_venv {
        "It has no environment yet. ".to_string()
    } else if missing.is_empty() {
        String::new()
    } else {
        format!(
            "{} declared {} not installed in the environment. ",
            missing.join(", "),
            if missing.len() == 1 { "is" } else { "are" }
        )
    };
    let next = if poetry {
        "`poetry install` installs them, and is what setup runs here."
    } else {
        "Poetry is not installed: install it (`uv tool install poetry`, or see https://python-poetry.org/docs/#installation) and run `poetry install`, or convert the project to PEP 621 with `uvx migrate-to-uv` so uv can set it up."
    };
    Finding {
        severity: if state.is_empty() {
            Severity::Info
        } else {
            Severity::Warning
        },
        title: "Poetry manages this project's dependencies".to_string(),
        detail: format!(
            "pyproject.toml declares them in [tool.poetry], which uv and pip do not read, so a uv setup would install none of them. {state}{next}"
        ),
        fix: FixKind::PoetryInstall,
    }
}

// --- requires-python against the dependencies' own floors ----------------------

/// When the project allows Pythons older than a dependency's oldest allowed
/// release supports, Poetry and uv (which both resolve for every Python the
/// project allows) find no solution: `numpy = "^2.2"` with `python = "^3.8"`
/// fails because numpy 2.2.0 needs Python 3.10. One finding naming the
/// dependencies and the floor to raise to.
async fn python_floor_finding<S: MetadataSource>(
    pyproject: &str,
    project: &ProjectDeps,
    compat: Option<&CompatReport>,
    source: &S,
) -> Option<Finding> {
    let compat = compat?;
    let declared = declared_python(pyproject)?;
    let floor = parse_requires_python(&declared.pep440).min()?;
    // (name, requirement, oldest allowed release, its requires_python, floor)
    let mut needs: Vec<(String, String, String, String, PyVersion)> = Vec::new();
    for req in floor_candidates(pyproject, project) {
        // The newest matching release already supports the floor: older
        // ones almost always do too, so skip the extra lookup.
        let Some(analysis) = compat
            .per_package
            .iter()
            .find(|p| project::normalize_name(&p.name) == req.name)
        else {
            continue;
        };
        if analysis.supported.min().map_or(true, |m| m <= floor) {
            continue;
        }
        if let Some((version, rp, min)) = oldest_release_floor(source, &req).await {
            if min > floor {
                needs.push((req.name.clone(), req.to_string(), version, rp, min));
            }
        }
    }
    let required = needs.iter().map(|n| n.4).max()?;
    let each: Vec<String> = needs
        .iter()
        .map(|(_, req, version, rp, _)| {
            format!("`{req}` needs Python `{rp}` (the oldest release it allows, {version}, declares that)")
        })
        .collect();
    let names: Vec<&str> = needs.iter().map(|n| n.0.as_str()).collect();
    let change = if declared.poetry {
        format!("`python = \"^{required}\"` in [tool.poetry.dependencies]")
    } else {
        format!("`requires-python = \">={required}\"`")
    };
    Some(Finding {
        severity: Severity::Error,
        title: format!(
            "{} `{}` allows Pythons {} does not support",
            if declared.poetry {
                "python ="
            } else {
                "requires-python"
            },
            declared.written,
            names.join(", ")
        ),
        detail: format!(
            "The project allows Python {floor}, but {}. Poetry and uv resolve for every Python the project allows, so `poetry lock` and `uv lock` find no solution for Python {floor}. Raise the floor to {required}: {change}.",
            each.join("; ")
        ),
        fix: FixKind::Manual,
    })
}

/// The project's Python constraint as written and as PEP 440.
struct DeclaredPython {
    written: String,
    pep440: String,
    /// From `[tool.poetry.dependencies] python`, not `requires-python`.
    poetry: bool,
}

fn declared_python(pyproject: &str) -> Option<DeclaredPython> {
    let doc = toml::from_str::<toml::Value>(pyproject).ok()?;
    if let Some(rp) = doc
        .get("project")
        .and_then(|p| p.get("requires-python"))
        .and_then(|v| v.as_str())
    {
        return Some(DeclaredPython {
            written: rp.to_string(),
            pep440: rp.to_string(),
            poetry: false,
        });
    }
    let py = doc
        .get("tool")?
        .get("poetry")?
        .get("dependencies")?
        .get("python")?
        .as_str()?;
    Some(DeclaredPython {
        written: py.to_string(),
        pep440: project::poetry_to_pep440(py),
        poetry: true,
    })
}

/// The dependencies with a lower bound whose constraint applies on every
/// Python: no environment marker, and for Poetry no `python`/`markers` key,
/// since either can confine it to newer Pythons.
fn floor_candidates(pyproject: &str, project: &ProjectDeps) -> Vec<Requirement> {
    let Ok(doc) = toml::from_str::<toml::Value>(pyproject) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(deps) = doc
        .get("project")
        .and_then(|p| p.get("dependencies"))
        .and_then(|d| d.as_array())
    {
        out.extend(
            deps.iter()
                .filter_map(|d| d.as_str())
                .filter(|d| !d.contains(';'))
                .filter_map(project::parse_requirement),
        );
    }
    if let Some(table) = doc
        .get("tool")
        .and_then(|t| t.get("poetry"))
        .and_then(|p| p.get("dependencies"))
        .and_then(|d| d.as_table())
    {
        for (key, value) in table {
            if key.eq_ignore_ascii_case("python") {
                continue;
            }
            if value
                .as_table()
                .is_some_and(|t| t.contains_key("python") || t.contains_key("markers"))
            {
                continue;
            }
            if let Some(v) = project::poetry_version(value) {
                out.push(Requirement {
                    name: project::normalize_name(key),
                    spec: project::poetry_to_pep440(&v),
                });
            }
        }
    }
    out.retain(|r| {
        r.spec.contains(">=")
            || r.spec.contains("==")
            || r.spec.contains("~=")
            || (r.spec.contains('>') && !r.spec.contains(">="))
    });
    out.retain(|r| project.packages.iter().any(|p| p.name == r.name));
    out
}

/// The oldest release `req` allows, its `requires_python`, and the oldest
/// Python that allows.
async fn oldest_release_floor<S: MetadataSource>(
    source: &S,
    req: &Requirement,
) -> Option<(String, String, PyVersion)> {
    let meta = source.fetch(&req.name).await.ok()?;
    let spec = VersionSpec::parse(&req.spec);
    let oldest = meta
        .versions
        .iter()
        .filter_map(|v| PackageVersion::parse(v))
        .filter(|v| spec.matches(v))
        .min()?;
    let release = source.fetch_version(&req.name, &oldest.raw).await.ok()?;
    let rp = release.info.requires_python?;
    let min = parse_requires_python(&rp).min()?;
    Some((oldest.raw, rp, min))
}

// --- imports ------------------------------------------------------------------

/// What the code imports that no manifest records (a project read from its
/// imports, see [`ProjectDeps::from_imports`]), or, in a project with an
/// environment, what its notebooks import that is not installed there.
fn imports_finding(
    workspace: &Path,
    project: &ProjectDeps,
    venv: Option<&VenvInfo>,
) -> Option<Finding> {
    let installed = venv.map(|v| crate::core::installed::scan(&v.path));
    if project.from_imports {
        let imported = project::imported_packages(workspace, false);
        let missing = installed
            .as_ref()
            .map(|i| imported.iter().filter(|p| !i.contains(&p.package)).count())
            .unwrap_or(imported.len());
        return Some(Finding {
            severity: if venv.is_some() && missing > 0 {
                Severity::Warning
            } else {
                Severity::Info
            },
            title: "Dependencies read from the code's imports".to_string(),
            detail: format!(
                "No pyproject.toml or requirements.txt declares this project's dependencies, so PyPilot read them from its .py files and notebooks: {}. Setup records them in pyproject.toml and installs them; check the names first.",
                import_list(&imported)
            ),
            fix: FixKind::SetupEnvironment,
        });
    }
    let installed = installed?;
    let missing: Vec<project::ImportedPackage> = project::imported_packages(workspace, true)
        .into_iter()
        .filter(|p| !installed.contains(&p.package))
        .collect();
    if missing.is_empty() {
        return None;
    }
    let declared =
        |p: &project::ImportedPackage| project.packages.iter().any(|r| r.name == p.package);
    let (fix, next) = if missing.iter().all(declared) {
        (
            FixKind::SyncEnvironment,
            "They are declared, so syncing the environment installs them.".to_string(),
        )
    } else {
        let names: Vec<&str> = missing.iter().map(|p| p.package.as_str()).collect();
        (
            FixKind::Manual,
            format!("Install them with `pypilot install {}`.", names.join(" ")),
        )
    };
    Some(Finding {
        severity: Severity::Warning,
        title: format!(
            "Notebooks import packages the environment lacks: {}",
            missing
                .iter()
                .map(|p| p.package.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        detail: format!(
            "The notebooks import {}, which the environment does not have, so those cells fail with ModuleNotFoundError. {next}",
            import_list(&missing)
        ),
        fix,
    })
}

/// `requests, pyyaml (imported as yaml), numpy`.
fn import_list(imports: &[project::ImportedPackage]) -> String {
    imports
        .iter()
        .map(|p| {
            if project::normalize_name(&p.module) == p.package {
                p.package.clone()
            } else {
                format!("{} (imported as {})", p.package, p.module)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

// --- environment.yml -------------------------------------------------------------

/// An `environment.yml` is the project's only manifest: no pyproject.toml,
/// no requirements.txt, no pixi manifest. uv and pip read none of it.
pub fn is_conda_only(workspace: &Path, project: &ProjectDeps) -> bool {
    project.has_conda_env
        && !project.is_pixi()
        // `exec::exists`: a dry run that planned the migration's
        // pyproject.toml must see it, or setup would plan it again.
        && !crate::core::exec::exists(&workspace.join("pyproject.toml"))
        && !crate::core::exec::exists(&workspace.join("requirements.txt"))
}

/// Whether a conda-only project is moved to pixi rather than to a
/// pyproject.toml: pixi is installed and the user has not chosen a package
/// manager (an explicit uv or pip choice keeps the pyproject route).
pub async fn conda_goes_to_pixi(settings: &Settings) -> bool {
    settings.package_manager == PackageManager::Auto && crate::core::pixi::locate().await.is_some()
}

fn apply_conda(findings: &mut Vec<Finding>, project: &ProjectDeps, has_venv: bool, pixi: bool) {
    findings.retain(|f| !NO_ENV_TITLES.contains(&f.title.as_str()));
    let conda = conda_finding(project.conda_python.as_deref(), has_venv, pixi);
    match findings.iter_mut().find(|f| f.fix == FixKind::MigrateConda) {
        Some(f) => *f = conda,
        None => findings.push(conda),
    }
}

fn conda_finding(python: Option<&str>, has_venv: bool, pixi: bool) -> Finding {
    let keeps = python
        .map(|p| format!("`python={p}`"))
        .unwrap_or_else(|| "its Python pin".to_string());
    let state = if has_venv {
        ""
    } else {
        " It has no environment yet."
    };
    let (detail, fix) = if pixi {
        (
            format!(
                "environment.yml is this project's only manifest, and only conda reads it: a uv setup would ignore {keeps}, its conda packages and its pip: section.{state} `pypilot migrate-conda --to pixi --write --install` (what setup runs here) writes a pixi.toml that keeps all three and installs it with pixi; environment.yml is left untouched."
            ),
            FixKind::MigrateCondaToPixi,
        )
    } else {
        (
            format!(
                "environment.yml is this project's only manifest, and only conda reads it: a uv setup would ignore {keeps} and its packages.{state} `pypilot migrate-conda` (what setup runs here) writes a pyproject.toml from it, and setup then builds .venv on that Python. Conda package names are carried over as written, so review them; with pixi installed (`pypilot install-pixi`), `pypilot migrate-conda --to pixi` keeps them as conda packages instead."
            ),
            FixKind::MigrateConda,
        )
    };
    Finding {
        severity: if has_venv {
            Severity::Info
        } else {
            Severity::Warning
        },
        title: "Conda environment detected".to_string(),
        detail,
        fix,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pypi::metadata::{Info, PackageMetadata};

    #[test]
    fn invalid_requirement_lines_are_numbered() {
        let text = "requests==2.31.0\nthis-package-does-not-exist-xyz==1.0\n==broken line\nsix>=\n# fine\n-r base.txt\n--index-url https://x/simple\n./local/pkg\nfoo @ https://x/foo.whl\nnumpy>=1.2; python_version<'3.12'\nbar[extra]>=1,<2  # comment\nbaz==1.*\nqux > 2\nbad>=1,\nalso bad\n";
        let bad = invalid_requirements(text);
        let lines: Vec<usize> = bad.iter().map(|b| b.line).collect();
        assert_eq!(lines, [3, 4, 14, 15], "{bad:#?}");
        assert_eq!(bad[0].text, "==broken line");
        assert_eq!(bad[0].reason, "it has no package name");
        assert_eq!(bad[1].reason, "`>=` has no version after it");

        let f = invalid_requirements_finding(text).unwrap();
        assert_eq!(f.severity, Severity::Error);
        assert_eq!(
            f.title,
            "requirements.txt lines 3, 4, 14 and 15 are not valid requirements"
        );
        assert!(f.detail.contains("line 3 `==broken line`"), "{}", f.detail);
        assert!(invalid_requirements_finding("requests\nsix>=1.16\n").is_none());
    }

    #[test]
    fn continued_lines_report_their_first_line() {
        let bad = invalid_requirements("ok==1\nsix>= \\\n  \n");
        assert_eq!(bad.len(), 1);
        assert_eq!(bad[0].line, 2);
        let f = invalid_requirements_finding("x\n==1\n").unwrap();
        assert_eq!(
            f.title,
            "requirements.txt line 2 is not a valid requirement"
        );
    }

    #[test]
    fn disagreeing_pins_are_named_with_both_specs() {
        let pyproject = "[project]\nname = \"x\"\nrequires-python = \">=3.9\"\ndependencies = [\"requests==2.31.0\", \"pyyaml==6.0.1\", \"rich>=13\", \"six>=1.10\"]\n";
        let requirements =
            "requests==2.25.0\npyyaml==5.4.1\nrich==13.7.0\nsix==1.9.0\nattrs==23.1\n";
        let conflicts = pin_conflicts(pyproject, requirements);
        let names: Vec<&str> = conflicts.iter().map(|c| c.name.as_str()).collect();
        // rich==13.7.0 satisfies >=13; six 1.9.0 does not satisfy >=1.10.
        assert_eq!(names, ["requests", "pyyaml", "six"]);
        assert_eq!(conflicts[0].pyproject, "==2.31.0");
        assert_eq!(conflicts[0].requirements, "==2.25.0");

        let f = pin_conflict_finding(&conflicts).unwrap();
        assert_eq!(f.severity, Severity::Warning);
        assert_eq!(
            f.title,
            "pyproject.toml and requirements.txt disagree on requests, pyyaml, six"
        );
        assert!(f.detail.contains(
            "requests is `==2.31.0` in pyproject.toml but `==2.25.0` in requirements.txt"
        ));
        assert!(pin_conflict_finding(&pin_conflicts(pyproject, "requests>=2\n")).is_none());
    }

    #[test]
    fn requirements_never_replace_declared_pins() {
        let dir = std::env::temp_dir().join(format!("pypilot-req-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("requirements.txt"),
            "requests==2.25.0
pyyaml==5.4.1  # old
attrs>=23
-r more.txt
",
        )
        .unwrap();
        // No pyproject.toml yet (uv init in a dry run): everything is new.
        let merge = requirements_merge(&dir);
        assert!(!merge.overlaps);
        assert_eq!(
            merge.extra,
            ["requests==2.25.0", "pyyaml==5.4.1", "attrs>=23"]
        );

        std::fs::write(
            dir.join("pyproject.toml"),
            "[project]
name = \"x\"
dependencies = [\"requests==2.31.0\", \"pyyaml==6.0.1\"]
",
        )
        .unwrap();
        let merge = requirements_merge(&dir);
        assert!(merge.overlaps);
        assert_eq!(merge.extra, ["attrs>=23"]);
        let names: Vec<&str> = merge.conflicts.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["requests", "pyyaml"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_lock_files_say_which_one_is_followed() {
        let uv = Settings::default();
        let f = lockfile_finding(&["uv.lock", "poetry.lock"], false, &uv).unwrap();
        assert_eq!(f.severity, Severity::Info);
        assert_eq!(f.title, "uv.lock and poetry.lock are both present");
        assert!(
            f.detail.starts_with("PyPilot follows uv.lock"),
            "{}",
            f.detail
        );
        assert!(f.detail.contains("poetry.lock is left untouched"));

        let f = lockfile_finding(&["uv.lock", "poetry.lock"], true, &uv).unwrap();
        assert!(f.detail.starts_with("PyPilot follows poetry.lock"));
        assert!(f.detail.contains("uv.lock is left untouched"));

        let pip = Settings {
            package_manager: PackageManager::Pip,
            ..Settings::default()
        };
        let f = lockfile_finding(&["uv.lock", "poetry.lock", "pdm.lock"], false, &pip).unwrap();
        assert_eq!(
            f.title,
            "uv.lock, poetry.lock and pdm.lock are both present"
        );
        assert!(f.detail.starts_with("PyPilot follows neither"));
        assert!(lockfile_finding(&["uv.lock"], false, &uv).is_none());
    }

    #[test]
    fn poetry_only_projects_are_recognized() {
        let poetry = "[tool.poetry]\nname = \"x\"\n\n[tool.poetry.dependencies]\npython = \"^3.8\"\nrequests = \"^2.31\"\n";
        assert!(is_poetry_only(poetry));
        // Only a python key: nothing to install.
        assert!(!is_poetry_only(
            "[tool.poetry.dependencies]\npython = \"^3.8\"\n"
        ));
        // Poetry 2 with PEP 621 dependencies: uv reads those.
        assert!(!is_poetry_only(
            "[project]\nname = \"x\"\ndependencies = [\"requests\"]\n\n[tool.poetry.dependencies]\npython = \"^3.8\"\nrequests = \"*\"\n"
        ));
        assert!(is_poetry_only(
            "[tool.poetry.group.dev.dependencies]\npytest = \"^8\"\n"
        ));
        assert!(!is_poetry_only("[project]\nname = \"x\"\n"));
    }

    #[test]
    fn poetry_finding_names_the_command_and_what_is_missing() {
        let f = poetry_finding(false, &["numpy".into(), "requests".into()], false);
        assert_eq!(f.severity, Severity::Warning);
        assert_eq!(f.fix, FixKind::PoetryInstall);
        assert!(f.detail.contains("[tool.poetry]"));
        assert!(f.detail.contains("Poetry is not installed"));
        assert!(f.detail.contains("uvx migrate-to-uv"));

        let f = poetry_finding(true, &["numpy".into()], true);
        assert!(
            f.detail.contains("numpy declared is not installed"),
            "{}",
            f.detail
        );
        assert!(f.detail.contains("`poetry install` installs them"));
        let f = poetry_finding(true, &[], true);
        assert_eq!(f.severity, Severity::Info);

        let mut findings = vec![Finding {
            severity: Severity::Warning,
            title: "No environment set up yet".into(),
            detail: String::new(),
            fix: FixKind::SetupEnvironment,
        }];
        apply_poetry(&mut findings, &ProjectDeps::default(), None, true);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].fix, FixKind::PoetryInstall);
    }

    fn meta(name: &str, version: &str, rp: Option<&str>, versions: &[&str]) -> PackageMetadata {
        PackageMetadata {
            info: Info {
                name: name.into(),
                version: version.into(),
                requires_python: rp.map(String::from),
            },
            urls: Vec::new(),
            versions: versions.iter().map(|v| v.to_string()).collect(),
        }
    }

    /// A source that answers `fetch` with the newest release and
    /// `fetch_version` with the release asked for.
    struct Releases(Vec<PackageMetadata>);

    impl MetadataSource for Releases {
        async fn fetch(&self, name: &str) -> crate::Result<PackageMetadata> {
            self.0
                .iter()
                .filter(|m| m.info.name == name)
                .max_by_key(|m| PackageVersion::parse(&m.info.version))
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no {name}"))
        }
        async fn fetch_version(&self, name: &str, version: &str) -> crate::Result<PackageMetadata> {
            self.0
                .iter()
                .find(|m| m.info.name == name && m.info.version == version)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no {name} {version}"))
        }
    }

    fn numpy_source() -> Releases {
        let all = ["1.26.4", "2.2.0", "2.3.5"];
        Releases(vec![
            meta("numpy", "1.26.4", Some(">=3.9"), &all),
            meta("numpy", "2.2.0", Some(">=3.10"), &all),
            meta("numpy", "2.3.5", Some(">=3.11"), &all),
        ])
    }

    async fn floor(pyproject: &str) -> Option<Finding> {
        let dir = std::env::temp_dir().join(format!(
            "pypilot-floor-{}-{}",
            std::process::id(),
            pyproject.len()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pyproject.toml"), pyproject).unwrap();
        let project = project::scan(&dir);
        let source = numpy_source();
        let compat = crate::pypi::analyze(
            &source,
            crate::core::platform::Platform::current(),
            &project.packages,
        )
        .await;
        let f = python_floor_finding(pyproject, &project, Some(&compat), &source).await;
        let _ = std::fs::remove_dir_all(&dir);
        f
    }

    #[tokio::test]
    async fn a_python_floor_below_a_dependency_floor_is_an_error() {
        let poetry = "[tool.poetry.dependencies]\npython = \"^3.8\"\nnumpy = \"^2.2\"\n";
        let f = floor(poetry).await.expect("finding");
        assert_eq!(f.severity, Severity::Error);
        assert_eq!(
            f.title,
            "python = `^3.8` allows Pythons numpy does not support"
        );
        assert!(
            f.detail.contains("release it allows, 2.2.0"),
            "{}",
            f.detail
        );
        assert!(f.detail.contains("`>=3.10`"));
        assert!(f.detail.contains("`python = \"^3.10\"`"), "{}", f.detail);

        let pep621 = "[project]\nname = \"x\"\nrequires-python = \">=3.8\"\ndependencies = [\"numpy>=2.2\"]\n";
        let f = floor(pep621).await.expect("finding");
        assert!(f.title.starts_with("requires-python `>=3.8`"));
        assert!(f.detail.contains("`requires-python = \">=3.10\"`"));

        // A floor the oldest allowed release supports, or a marker, is fine.
        assert!(floor("[project]\nname = \"x\"\nrequires-python = \">=3.10\"\ndependencies = [\"numpy>=2.2\"]\n").await.is_none());
        assert!(floor("[project]\nname = \"x\"\nrequires-python = \">=3.8\"\ndependencies = [\"numpy>=2.2; python_version>='3.10'\"]\n").await.is_none());
        assert!(floor("[project]\nname = \"x\"\nrequires-python = \">=3.9\"\ndependencies = [\"numpy>=1.26\"]\n").await.is_none());
    }

    #[test]
    fn conda_only_findings_lead_to_the_migration() {
        let mut findings = vec![
            Finding {
                severity: Severity::Info,
                title: "Conda environment detected".into(),
                detail: String::new(),
                fix: FixKind::MigrateConda,
            },
            Finding {
                severity: Severity::Warning,
                title: "No environment set up yet".into(),
                detail: "Recommended: 3.15".into(),
                fix: FixKind::SetupEnvironment,
            },
        ];
        let project = ProjectDeps {
            has_conda_env: true,
            conda_python: Some("3.11".into()),
            ..Default::default()
        };
        apply_conda(&mut findings, &project, false, true);
        assert_eq!(findings.len(), 1, "{findings:#?}");
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Warning);
        assert_eq!(f.fix, FixKind::MigrateCondaToPixi);
        assert!(f.detail.contains("`python=3.11`"));
        assert!(f
            .detail
            .contains("pypilot migrate-conda --to pixi --write --install"));

        let f = conda_finding(Some("3.11"), true, false);
        assert_eq!(f.severity, Severity::Info);
        assert_eq!(f.fix, FixKind::MigrateConda);
        assert!(f.detail.contains("`pypilot migrate-conda`"));
    }

    #[test]
    fn notebook_imports_are_reported() {
        let dir = std::env::temp_dir().join(format!("pypilot-nb-imports-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("nb.ipynb"),
            r#"{"cells":[{"cell_type":"code","source":["import requests\n","import yaml\n"]}],"metadata":{}}"#,
        )
        .unwrap();

        // No manifest: the imports are the dependencies.
        let project = project::scan(&dir);
        let f = imports_finding(&dir, &project, None).unwrap();
        assert_eq!(f.severity, Severity::Info);
        assert_eq!(f.fix, FixKind::SetupEnvironment);
        assert!(
            f.detail.contains("pyyaml (imported as yaml), requests"),
            "{}",
            f.detail
        );

        // A manifest and a venv without them: a warning naming both.
        std::fs::write(dir.join("requirements.txt"), "requests\n").unwrap();
        let venv = dir.join(".venv");
        std::fs::create_dir_all(venv.join("Lib").join("site-packages")).unwrap();
        std::fs::write(venv.join("pyvenv.cfg"), "version = 3.12.1\n").unwrap();
        let info = VenvInfo {
            path: venv.clone(),
            python: Some(PyVersion::py3(12)),
            broken: None,
            kind: Default::default(),
        };
        let project = project::scan(&dir);
        let f = imports_finding(&dir, &project, Some(&info)).unwrap();
        assert_eq!(f.severity, Severity::Warning);
        assert_eq!(
            f.title,
            "Notebooks import packages the environment lacks: pyyaml, requests"
        );
        assert_eq!(f.fix, FixKind::Manual);
        assert!(f.detail.contains("`pypilot install pyyaml requests`"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn conda_only_means_no_other_manifest() {
        let dir = std::env::temp_dir().join(format!("pypilot-conda-only-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("environment.yml"),
            "dependencies:\n  - python=3.11\n",
        )
        .unwrap();
        assert!(is_conda_only(&dir, &project::scan(&dir)));
        std::fs::write(dir.join("requirements.txt"), "requests\n").unwrap();
        assert!(!is_conda_only(&dir, &project::scan(&dir)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
