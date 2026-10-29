//! "pixi can help here": walls pip and uv cannot climb, and conda-forge can.
//!
//! pip and uv install Python packages; they cannot install the C libraries,
//! compilers or CUDA toolkit a source build needs. conda-forge (which pixi
//! installs from) ships those as packages, so for a dependency with no wheel
//! here whose build needs, say, libgdal, pixi is a real way out. This module
//! recognises those walls and says so. It only suggests: the fix it carries
//! is [`FixKind::MigrateToPixi`], which the user runs or doesn't.
//!
//! Signals, all from data the engine already has:
//!
//! 1. A dependency with no installable wheel here (sdist only, no files for
//!    this platform, or no wheel for the venv's Python) that conda-forge
//!    builds for this platform ([`crate::matrix::conda_forge`]). The table
//!    is curated to packages whose source build needs more than a compiler:
//!    system libraries, Fortran, nvcc, or a very long compile.
//! 2. A failed pip/uv build whose output names a missing header, `*-config`
//!    tool, pkg-config module or shared library ([`explain_build_failure`]).
//! 3. CUDA sources (`.cu`) in the repo and no nvcc on the machine.
//! 4. A repo that mixes Python with R, Rust or CMake/Meson C++ (Info only).
//!
//! Nothing here runs in a pixi project: its own support covers it.

use std::path::{Path, PathBuf};

use crate::core::platform::{Os, Platform};
use crate::core::project::{normalize_name, ProjectDeps};
use crate::core::{Finding, FixKind, Severity};
use crate::matrix::conda_forge::{conda_subdir, Availability, CondaForgeTable, CondaPackage};
use crate::pypi::metadata::PackageAnalysis;
use crate::pypi::{CompatReport, PyVersion, PyVersionSet};

/// The CLI command every advisory points at.
pub const MIGRATE_COMMAND: &str = "pypilot migrate-to-pixi";

/// What the advice depends on about this machine.
#[derive(Debug, Clone, Copy)]
pub struct Machine {
    pub platform: Platform,
    /// The venv's Python, when there is one.
    pub venv_python: Option<PyVersion>,
    /// The Python the project would use (venv's, else the recommended one).
    pub python: Option<PyVersion>,
    /// nvcc is on PATH or under `CUDA_HOME` / `CUDA_PATH`.
    pub nvcc: bool,
}

/// Non-Python things found in the repo.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoScan {
    /// A few CUDA source files (`.cu`, `.cuh`), relative to the root.
    pub cuda_sources: Vec<PathBuf>,
    /// Other languages with their own build: `R`, `Rust`, `C/C++`.
    pub languages: Vec<(&'static str, String)>,
}

/// Add the advisory to an assessment's findings. Skips pixi projects.
pub fn apply(
    findings: &mut Vec<Finding>,
    workspace: &Path,
    project: &ProjectDeps,
    compat: Option<&CompatReport>,
    venv_python: Option<PyVersion>,
    target_python: Option<PyVersion>,
) {
    if project.is_pixi() || crate::core::pixi::detect(workspace).is_some() {
        return;
    }
    if !project.is_python_project() {
        return;
    }
    let machine = Machine {
        platform: compat.map(|c| c.platform).unwrap_or_else(Platform::current),
        venv_python,
        python: venv_python.or(target_python),
        nvcc: nvcc_available(),
    };
    let repo = scan_repo(workspace);
    advise(
        findings,
        crate::matrix::conda_forge(),
        compat,
        &machine,
        &repo,
    );
}

/// The pure part of [`apply`].
pub fn advise(
    findings: &mut Vec<Finding>,
    table: &CondaForgeTable,
    compat: Option<&CompatReport>,
    machine: &Machine,
    repo: &RepoScan,
) {
    let mut nvcc_said = false;
    if let Some(compat) = compat {
        for p in &compat.per_package {
            let Some(row) = table.get(&p.name) else {
                continue;
            };
            let Some(wall) = wall(p, machine.venv_python) else {
                continue;
            };
            let Some(pixi) = pixi_sentence(row, machine.platform, machine.python) else {
                continue;
            };
            let nvcc = nvcc_sentence(row, machine);
            nvcc_said |= !nvcc.is_empty();
            attach(findings, p, row, wall, &nvcc, &pixi, machine.platform);
        }
    }

    // The project's own CUDA code: nothing on PyPI provides nvcc.
    if !machine.nvcc && !nvcc_said && machine.platform.os != Os::Macos {
        if let Some(first) = repo.cuda_sources.first() {
            findings.push(Finding {
                severity: Severity::Info,
                title: "CUDA sources found, but nvcc is not installed".to_string(),
                detail: format!(
                    "This project has CUDA source files ({}), and building them needs nvcc, the CUDA compiler. pip wheels bundle the CUDA runtime but not nvcc. pixi can install `cuda-nvcc` (or the whole `cuda-toolkit`) from conda-forge into the project environment, matched to the CUDA version you pick. Run `{MIGRATE_COMMAND}` to set the project up with pixi.",
                    first.display()
                ),
                fix: FixKind::MigrateToPixi,
            });
        }
    }

    if !repo.languages.is_empty() {
        let names: Vec<&str> = repo.languages.iter().map(|(l, _)| *l).collect();
        let evidence: Vec<&str> = repo.languages.iter().map(|(_, e)| e.as_str()).collect();
        findings.push(Finding {
            severity: Severity::Info,
            title: format!("This project mixes Python with {}", join_and(&names)),
            detail: format!(
                "Found {}. pip and uv manage only the Python side. pixi manages {} and Python in one manifest from conda-forge (r-base, cxx-compiler, cmake, rust), so one environment builds and runs all of it. Run `{MIGRATE_COMMAND}` if you want that.",
                evidence.join(", "),
                join_and(&names)
            ),
            fix: FixKind::MigrateToPixi,
        });
    }
}

/// Why a dependency will not install from a wheel here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wall {
    /// No files for this platform at all.
    NoFiles,
    /// No wheel for this platform: pip builds the sdist.
    SourceBuild,
    /// Wheels for other Pythons, not this one.
    NoWheelFor(PyVersion),
}

fn wall(p: &PackageAnalysis, venv_python: Option<PyVersion>) -> Option<Wall> {
    if p.no_files_for_platform {
        Some(Wall::NoFiles)
    } else if p.sdist_only {
        Some(Wall::SourceBuild)
    } else {
        venv_python
            .filter(|py| p.lacks_wheel_for(*py))
            .map(Wall::NoWheelFor)
    }
}

/// "gdal" or "tables (conda-forge `pytables`)".
fn conda_label(row: &CondaPackage, pypi_name: &str) -> String {
    if normalize_name(&row.conda) == normalize_name(pypi_name) {
        pypi_name.to_string()
    } else {
        format!("{pypi_name} (conda-forge `{}`)", row.conda)
    }
}

/// "pixi installs gdal 3.13.3 from conda-forge ..." or `None` when
/// conda-forge has no build for this platform (then there is nothing to
/// promise).
fn pixi_sentence(
    row: &CondaPackage,
    platform: Platform,
    python: Option<PyVersion>,
) -> Option<String> {
    let subdir = conda_subdir(platform)?;
    let what = conda_label(row, &row.pypi);
    let libs = if row.system_libs.is_empty() {
        "prebuilt".to_string()
    } else {
        format!("with its native libraries ({})", row.system_libs.join(", "))
    };
    let sentence = match row.availability(platform, python) {
        Availability::No => return None,
        Availability::Yes => {
            let on_py = python
                .filter(|_| !row.pythons(platform).is_empty())
                .map(|py| format!(" and Python {py}"))
                .unwrap_or_default();
            format!(
                "pixi installs {what} {} from conda-forge {libs}, built for {subdir}{on_py}.",
                row.version
            )
        }
        Availability::OtherPythons(pys) => format!(
            "pixi installs {what} {} from conda-forge {libs}, built for {subdir} with Python {}.",
            row.version,
            PyVersionSet::from_versions(pys).to_range_string()
        ),
    };
    Some(format!(
        "{sentence} Run `{MIGRATE_COMMAND}` to set the project up with pixi."
    ))
}

fn nvcc_sentence(row: &CondaPackage, machine: &Machine) -> String {
    if !row.needs_nvcc || machine.nvcc || machine.platform.os == Os::Macos {
        return String::new();
    }
    "pip wheels bundle the CUDA runtime but not nvcc, and nvcc is not installed here; pixi can install `cuda-nvcc` from conda-forge.".to_string()
}

/// Put the advice on the finding that already reports this package's
/// problem, or add one.
fn attach(
    findings: &mut Vec<Finding>,
    p: &PackageAnalysis,
    row: &CondaPackage,
    wall: Wall,
    nvcc: &str,
    pixi: &str,
    platform: Platform,
) {
    let here = platform.label();
    let why = if row.why.is_empty() {
        String::new()
    } else {
        format!(" {}", capitalize_it(&row.why, &p.name))
    };
    let extra = [why.trim(), nvcc, pixi]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" ");
    let prefix = format!("{} {} ", p.name, p.version);

    match wall {
        Wall::NoFiles => {
            let title = format!("{} {} cannot be installed on {here}", p.name, p.version);
            if let Some(f) = findings.iter_mut().find(|f| f.title == title) {
                f.detail.push_str(&format!(" {extra}"));
                f.fix = FixKind::MigrateToPixi;
                return;
            }
        }
        Wall::SourceBuild => {
            // The generic "Will compile from source" finding (or a CUDA
            // extension's own) promises a compiler is enough; for these
            // packages it is not, so this one replaces it.
            if let Some(f) = findings.iter_mut().find(|f| {
                (f.title == "Will compile from source" && f.detail.starts_with(&prefix))
                    || first_word_is(&f.title, &p.name)
            }) {
                if f.title == "Will compile from source" {
                    f.title = source_build_title(row, p);
                    f.detail = format!(
                        "{} {} has no wheel for {here}, so pip and uv build it from source. {extra}",
                        p.name, p.version
                    );
                    f.severity = Severity::Warning;
                } else {
                    f.detail.push_str(&format!(" {extra}"));
                }
                f.fix = FixKind::MigrateToPixi;
                return;
            }
        }
        Wall::NoWheelFor(py) => {
            // Rebuilding on a Python with wheels stays the fix; pixi is the
            // way to keep this one.
            let title = format!("Python {py} is not supported by this project");
            if let Some(f) = findings.iter_mut().find(|f| f.title == title) {
                f.detail.push_str(&format!(" To keep Python {py}: {pixi}"));
            }
            return;
        }
    }

    let detail = match wall {
        Wall::NoFiles => format!(
            "{} {} publishes nothing installable on {here}. {extra}",
            p.name, p.version
        ),
        _ => format!(
            "{} {} has no wheel for {here}, so pip and uv build it from source. {extra}",
            p.name, p.version
        ),
    };
    findings.push(Finding {
        severity: if wall == Wall::NoFiles {
            Severity::Error
        } else {
            Severity::Warning
        },
        title: source_build_title(row, p),
        detail,
        fix: FixKind::MigrateToPixi,
    });
}

fn source_build_title(row: &CondaPackage, p: &PackageAnalysis) -> String {
    if row.needs_nvcc {
        format!("{} {} needs nvcc to build", p.name, p.version)
    } else if row.system_libs.is_empty() {
        format!(
            "{} {} has no wheel here and builds from source",
            p.name, p.version
        )
    } else {
        format!(
            "{} {} needs native libraries pip cannot install",
            p.name, p.version
        )
    }
}

/// The table's "why" clauses start with "it"; name the package instead.
fn capitalize_it(why: &str, name: &str) -> String {
    let body = why.strip_prefix("it ").map(|rest| format!("{name} {rest}"));
    let mut s = body.unwrap_or_else(|| format!("{name}: {why}"));
    if !s.ends_with('.') {
        s.push('.');
    }
    s
}

fn first_word_is(text: &str, name: &str) -> bool {
    text.split_whitespace()
        .next()
        .is_some_and(|w| normalize_name(w) == normalize_name(name))
}

fn join_and(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => one.to_string(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

// --- `pypilot check` -------------------------------------------------------

/// Why one package will not install from a wheel here, and the conda-forge
/// way around it, for `pypilot check`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckAdvice {
    /// One line per reason: no wheel for cpXY, misleading requires-python,
    /// what the source build needs.
    pub reasons: Vec<String>,
    /// "conda-forge: gdal 3.13.3 available for linux-64/python 3.13 →
    /// pypilot migrate-to-pixi", when conda-forge builds it here.
    pub alternative: Option<String>,
}

/// [`CheckAdvice`] for `p` on `machine.platform`, judged against
/// `machine.venv_python` when there is one.
pub fn check_advice(
    p: &PackageAnalysis,
    table: &CondaForgeTable,
    machine: &Machine,
) -> CheckAdvice {
    let here = machine.platform.label();
    let row = table.get(&p.name);
    let mut reasons = Vec::new();
    if let Some(msg) = p.no_files_message(here) {
        reasons.push(msg);
    } else if p.sdist_only {
        reasons.push(format!(
            "no wheel for {here}: pip and uv build it from source"
        ));
    }
    if let Some(py) = machine.venv_python {
        let rp = p.requires_python.as_deref().unwrap_or("(none)");
        if p.lacks_wheel_for(py) {
            reasons.push(format!(
                "no {} wheel for {here}: requires-python {rp} admits Python {py}, but the release ships no wheel for it, so pip and uv would build it from source",
                py.cp_tag()
            ));
        } else if !p.declared().contains(py) {
            reasons.push(format!("requires-python {rp} excludes Python {py}"));
        }
    }
    let wall = !reasons.is_empty();
    if let Some(row) = row.filter(|_| wall) {
        if !row.why.is_empty() {
            reasons.push(format!(
                "source build: {}",
                capitalize_it(&row.why, &p.name)
            ));
        }
        let nvcc = nvcc_sentence(row, machine);
        if !nvcc.is_empty() {
            reasons.push(nvcc);
        }
    }

    let alternative = row.and_then(|row| {
        let subdir = conda_subdir(machine.platform)?;
        let pythons = match row.availability(machine.platform, machine.python) {
            Availability::No => return None,
            Availability::Yes => match machine.python {
                Some(py) if !row.pythons(machine.platform).is_empty() => format!("/python {py}"),
                _ => String::new(),
            },
            Availability::OtherPythons(pys) => format!(
                "/python {}",
                PyVersionSet::from_versions(pys).to_range_string()
            ),
        };
        Some(format!(
            "conda-forge: {} {} available for {subdir}{pythons} → {MIGRATE_COMMAND}",
            row.conda, row.version
        ))
    });
    CheckAdvice {
        reasons,
        alternative,
    }
}

// --- the machine and the repo ----------------------------------------------

/// Is nvcc installed? Looks under `CUDA_HOME` / `CUDA_PATH` and on PATH.
pub fn nvcc_available() -> bool {
    let exe = if cfg!(windows) { "nvcc.exe" } else { "nvcc" };
    for var in ["CUDA_HOME", "CUDA_PATH"] {
        if let Some(dir) = std::env::var_os(var) {
            if Path::new(&dir).join("bin").join(exe).is_file() {
                return true;
            }
        }
    }
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(exe).is_file()))
}

/// How deep [`scan_repo`] looks, and how many entries it reads at most:
/// it runs on every assessment, so it must stay cheap on huge repos.
const SCAN_DEPTH: usize = 4;
const SCAN_BUDGET: usize = 5000;

const SKIP_DIRS: &[&str] = &[
    ".venv",
    ".pixi",
    "venv",
    "env",
    ".git",
    "node_modules",
    "__pycache__",
    "target",
    "build",
    "dist",
    ".tox",
    ".nox",
    "site-packages",
    "third_party",
    "vendor",
    "renv",
];

/// Look for CUDA sources and other languages' build files.
pub fn scan_repo(root: &Path) -> RepoScan {
    let mut scan = RepoScan::default();
    let mut budget = SCAN_BUDGET;
    let mut r_files = None;
    walk(root, root, SCAN_DEPTH, &mut budget, &mut |rel, name| {
        let lower = name.to_ascii_lowercase();
        if (lower.ends_with(".cu") || lower.ends_with(".cuh")) && scan.cuda_sources.len() < 3 {
            scan.cuda_sources.push(rel.to_path_buf());
        }
        if r_files.is_none() && (lower.ends_with(".r") || lower.ends_with(".rmd")) {
            r_files = Some(rel.display().to_string());
        }
    });

    let root_file = |n: &str| root.join(n).is_file();
    let r_package = std::fs::read_to_string(root.join("DESCRIPTION"))
        .is_ok_and(|t| t.lines().any(|l| l.starts_with("Package:")));
    if r_package {
        scan.languages
            .push(("R", "an R package DESCRIPTION".into()));
    } else if let Some(f) = r_files {
        scan.languages.push(("R", format!("R code ({f})")));
    }
    if root_file("Cargo.toml") {
        scan.languages.push(("Rust", "Cargo.toml".into()));
    }
    if let Some(f) = ["CMakeLists.txt", "meson.build"]
        .iter()
        .find(|f| root_file(f))
    {
        scan.languages.push(("C/C++", (*f).to_string()));
    }
    scan
}

fn walk(
    root: &Path,
    dir: &Path,
    depth: usize,
    budget: &mut usize,
    visit: &mut dyn FnMut(&Path, &str),
) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut subdirs = Vec::new();
    for entry in rd.flatten() {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if kind.is_file() {
            visit(path.strip_prefix(root).unwrap_or(&path), &name);
        } else if kind.is_dir() && depth > 0 {
            let skip = SKIP_DIRS.contains(&name.as_str())
                || name.starts_with('.')
                || path.join("pyvenv.cfg").is_file();
            if !skip {
                subdirs.push(path);
            }
        }
    }
    for d in subdirs {
        walk(root, &d, depth - 1, budget, visit);
    }
}

// --- failed builds ----------------------------------------------------------

/// What a failed build said was missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missing {
    /// `header`, `tool`, `pkg-config` or `library`.
    pub kind: &'static str,
    pub name: String,
    /// The conda-forge package that provides it.
    pub conda: String,
}

/// A failed pip/uv build, explained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildAdvice {
    /// The package whose build failed, when the output names it.
    pub package: Option<String>,
    pub missing: Option<Missing>,
    /// The explanation, ending with the `pypilot migrate-to-pixi` pointer.
    pub message: String,
}

/// Explain a failed install from the installer's stderr, when the failure is
/// one pixi can get past: the build names a header, tool, pkg-config module
/// or shared library that conda-forge ships, or the failing package is one
/// the table knows. `None` for everything else (network errors, conflicts).
pub fn explain_build_failure(
    stderr: &str,
    table: &CondaForgeTable,
    platform: Platform,
) -> Option<BuildAdvice> {
    let package = failed_package(stderr);
    let missing = find_missing(stderr, table);
    let row = package.as_deref().and_then(|p| table.get(p));
    if missing.is_none() && (row.is_none() || !looks_like_build_failure(stderr)) {
        return None;
    }
    let who = match (&package, &missing) {
        (Some(p), _) => format!("{p} failed to build"),
        (None, Some(m)) if m.kind == "library" => "A dependency failed to load".to_string(),
        (None, _) => "The build failed".to_string(),
    };

    let mut message = match &missing {
        Some(m) if m.conda == "cuda-nvcc" || m.conda == "cuda-toolkit" => format!(
            "{who} because nvcc, the CUDA compiler, is missing. pip wheels bundle the CUDA runtime but not nvcc, and pip and uv cannot install it."
        ),
        Some(m) => format!(
            "{who} because {} is missing. It comes from a system library (conda-forge `{}`) that pip and uv cannot install.",
            describe(m),
            m.conda
        ),
        None => format!(
            "{who}: {}",
            capitalize_it(
                &row.map(|r| r.why.clone()).unwrap_or_default(),
                package.as_deref().unwrap_or("It")
            )
        ),
    };
    let pixi = row.and_then(|r| {
        let label = conda_label(r, package.as_deref().unwrap_or(&r.pypi));
        match r.availability(platform, None) {
            Availability::No => None,
            _ if r.system_libs.is_empty() => {
                Some(format!(" pixi installs {label} prebuilt from conda-forge."))
            }
            _ => Some(format!(
                " pixi installs {label} from conda-forge with its native libraries ({}).",
                r.system_libs.join(", ")
            )),
        }
    });
    match (pixi, &missing) {
        (Some(p), _) => message.push_str(&p),
        (None, Some(m)) => message.push_str(&format!(
            " pixi can install `{}` from conda-forge into the project environment.",
            m.conda
        )),
        (None, None) => return None,
    }
    message.push_str(&format!(
        " Run `{MIGRATE_COMMAND}` to set the project up with pixi."
    ));
    Some(BuildAdvice {
        package,
        missing,
        message,
    })
}

/// The [`Finding`] form of [`explain_build_failure`], for reports.
pub fn build_failure_finding(advice: &BuildAdvice) -> Finding {
    Finding {
        severity: Severity::Error,
        title: match &advice.package {
            Some(p) => format!("{p} failed to build"),
            None => "A dependency failed to build".to_string(),
        },
        detail: advice.message.clone(),
        fix: FixKind::MigrateToPixi,
    }
}

fn describe(m: &Missing) -> String {
    match m.kind {
        "header" => format!("the header `{}`", m.name),
        "tool" => format!("the `{}` tool", m.name),
        "pkg-config" => format!("the pkg-config module `{}`", m.name),
        _ => format!("the shared library `{}`", m.name),
    }
}

fn looks_like_build_failure(stderr: &str) -> bool {
    [
        "subprocess-exited-with-error",
        "Failed building wheel",
        "Failed to build",
        "error: command",
        "Building wheel",
        "Could not build wheels",
        "failed to build",
    ]
    .iter()
    .any(|s| stderr.contains(s))
}

/// The package a pip or uv build failure names.
fn failed_package(stderr: &str) -> Option<String> {
    const MARKERS: &[&str] = &[
        "Failed to build `",
        "Failed to download and build `",
        "Failed building wheel for ",
        "Building wheel for ",
        "Failed to build installable wheels for some pyproject.toml based projects (",
        "Failed to build ",
        "Getting requirements to build wheel for ",
    ];
    for marker in MARKERS {
        if let Some(i) = stderr.find(marker) {
            let rest = &stderr[i + marker.len()..];
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                .collect();
            let name = name.trim_end_matches('.');
            if !name.is_empty() && name != "installable" && name != "wheel" {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// The first thing the build output says is missing, that the table maps.
fn find_missing(stderr: &str, table: &CondaForgeTable) -> Option<Missing> {
    let found = |kind: &'static str, name: &str| {
        table.library(kind, name).map(|l| Missing {
            kind,
            name: name.to_string(),
            conda: l.conda.clone(),
        })
    };

    for line in stderr.lines() {
        // gcc `x.c:1:10: fatal error: gdal.h: No such file or directory`,
        // clang `fatal error: 'gdal.h' file not found`, MSVC `fatal error
        // C1083: Cannot open include file: 'gdal.h': No such file ...`.
        if let Some(i) = line.find("Cannot open include file: '") {
            let rest = &line[i + "Cannot open include file: '".len()..];
            if let Some(m) = rest.split('\'').next().and_then(|h| found("header", h)) {
                return Some(m);
            }
        } else if let Some(i) = line.find("fatal error: ") {
            let rest = line[i + "fatal error: ".len()..].trim_start();
            let header = match rest.strip_prefix('\'') {
                Some(q) => q.split('\'').next(),
                None => rest.split(':').next(),
            };
            if let Some(m) = header.map(str::trim).and_then(|h| found("header", h)) {
                return Some(m);
            }
        }

        // pkg-config: `Package cairo was not found in the pkg-config search
        // path`, `No package 'cairo' found`, meson `Dependency "cairo" not found`.
        for (start, end) in [
            ("Package ", " was not found in the pkg-config search path"),
            ("No package '", "' found"),
            ("Dependency \"", "\" not found"),
            ("Run-time dependency ", " found: NO"),
        ] {
            if let Some(name) = between(line, start, end) {
                if let Some(m) = found("pkg-config", name.trim()) {
                    return Some(m);
                }
            }
        }

        // Shared libraries the loader could not open.
        if let Some(i) = line.find(": cannot open shared object file") {
            let name = line[..i].rsplit([' ', '\'', '"', ':']).next().unwrap_or("");
            if let Some(m) = found("library", name) {
                return Some(m);
            }
        }
        if let Some(i) = line.find("Library not loaded: ") {
            let path = line[i + "Library not loaded: ".len()..].trim();
            let name = path.rsplit('/').next().unwrap_or(path);
            if let Some(m) = found("library", name) {
                return Some(m);
            }
        }
        if line.contains("failed to find libmagic") {
            if let Some(m) = found("library", "libmagic") {
                return Some(m);
            }
        }
    }

    // Missing build tools. Tool names are short (`R`, `dot`), so match them
    // as whole words inside known phrasings only.
    if stderr.contains("CUDA_HOME environment variable is not set")
        || stderr.contains("nvcc was not found")
        || stderr.contains("No CUDA runtime is found")
    {
        if let Some(m) = found("tool", "nvcc") {
            return Some(m);
        }
    }
    if stderr.contains("CMake must be installed") {
        if let Some(m) = found("tool", "cmake") {
            return Some(m);
        }
    }
    let lower = stderr.to_ascii_lowercase();
    let tool_hit = |tool: &str| {
        let t = tool.to_ascii_lowercase();
        [
            format!("{t}: not found"),
            format!("{t}: command not found"),
            format!("could not find {t}"),
            format!("{t} executable not found"),
            format!("{t} not found"),
            format!("'{t}' is not recognized"),
            format!("no such file or directory: '{t}'"),
            format!("unable to find {t}"),
        ]
        .iter()
        .any(|phrase| contains_word(&lower, phrase))
    };
    for line in table_tools(table) {
        if tool_hit(&line) {
            return found("tool", &line);
        }
    }
    None
}

fn table_tools(table: &CondaForgeTable) -> Vec<String> {
    // Longest first, so `mysql_config` is not reported as a bare `config`.
    let mut tools: Vec<String> = TOOL_PROBES
        .iter()
        .filter(|t| table.library("tool", t).is_some())
        .map(|t| t.to_string())
        .collect();
    tools.sort_by_key(|t| std::cmp::Reverse(t.len()));
    tools
}

/// Tools worth looking for in build output. Kept to names that cannot be
/// mistaken for ordinary words in an error message.
const TOOL_PROBES: &[&str] = &[
    "gdal-config",
    "pg_config",
    "mysql_config",
    "mariadb_config",
    "pkg-config",
    "krb5-config",
    "curl-config",
    "xslt-config",
    "icu-config",
    "geos-config",
    "nc-config",
    "ta-lib-config",
    "llvm-config",
    "h5cc",
    "mpicc",
    "swig",
    "cmake",
    "gfortran",
    "nvcc",
];

/// `phrase` occurs in `text` with no word character right before it.
fn contains_word(text: &str, phrase: &str) -> bool {
    let mut from = 0;
    while let Some(i) = text[from..].find(phrase) {
        let at = from + i;
        let before = text[..at].chars().next_back();
        if !before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return true;
        }
        from = at + phrase.len();
    }
    false
}

fn between<'a>(line: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let i = line.find(start)? + start.len();
    let j = line[i..].find(end)? + i;
    Some(&line[i..j])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::platform::Arch;

    fn linux() -> Platform {
        Platform {
            os: Os::Linux,
            arch: Arch::X86_64,
        }
    }

    fn table() -> &'static CondaForgeTable {
        use std::sync::OnceLock;
        static T: OnceLock<CondaForgeTable> = OnceLock::new();
        T.get_or_init(|| CondaForgeTable::load(crate::matrix::CONDA_FORGE_JSON).unwrap())
    }

    fn machine(venv: Option<u8>) -> Machine {
        Machine {
            platform: linux(),
            venv_python: venv.map(PyVersion::py3),
            python: Some(PyVersion::py3(venv.unwrap_or(13))),
            nvcc: false,
        }
    }

    fn analysis(name: &str, version: &str) -> PackageAnalysis {
        PackageAnalysis {
            name: name.into(),
            version: version.into(),
            supported: PyVersionSet::from_versions((10..=14).map(PyVersion::py3)),
            has_platform_wheels: true,
            sdist_only: false,
            has_foreign_wheels: false,
            no_files_for_platform: false,
            wheel_platforms: Vec::new(),
            requires_python: Some(">=3.10".into()),
        }
    }

    fn sdist(name: &str, version: &str) -> PackageAnalysis {
        PackageAnalysis {
            has_platform_wheels: false,
            sdist_only: true,
            has_foreign_wheels: true,
            ..analysis(name, version)
        }
    }

    fn report(per_package: Vec<PackageAnalysis>) -> CompatReport {
        CompatReport {
            platform: linux(),
            per_package,
            unresolved: Vec::new(),
            intersection: PyVersionSet::from_versions((10..=14).map(PyVersion::py3)),
            conflicts: Vec::new(),
            sdist_warnings: Vec::new(),
        }
    }

    fn generic_sdist(name: &str, version: &str) -> Finding {
        Finding {
            severity: Severity::Info,
            title: "Will compile from source".into(),
            detail: format!("{name} {version} ships prebuilt wheels for other platforms but none for Linux, so installing it compiles native code from source. The build tools it needs are installed."),
            fix: FixKind::Manual,
        }
    }

    #[test]
    fn a_native_source_build_becomes_a_pixi_advisory() {
        let compat = report(vec![sdist("GDAL", "3.8.0"), analysis("requests", "2.32")]);
        let mut findings = vec![generic_sdist("GDAL", "3.8.0")];
        advise(
            &mut findings,
            table(),
            Some(&compat),
            &machine(None),
            &RepoScan::default(),
        );
        assert_eq!(findings.len(), 1, "{findings:#?}");
        let f = &findings[0];
        assert_eq!(f.fix, FixKind::MigrateToPixi);
        assert_eq!(f.severity, Severity::Warning);
        assert_eq!(
            f.title,
            "GDAL 3.8.0 needs native libraries pip cannot install"
        );
        assert!(f
            .detail
            .starts_with("GDAL 3.8.0 has no wheel for Linux x64"));
        assert!(f.detail.contains("libgdal"), "{}", f.detail);
        assert!(f.detail.contains("built for linux-64 and Python 3.13"));
        assert!(f
            .detail
            .ends_with("Run `pypilot migrate-to-pixi` to set the project up with pixi."));
    }

    #[test]
    fn packages_with_wheels_or_unknown_to_the_table_get_nothing() {
        let compat = report(vec![
            analysis("gdal", "3.8.0"),
            sdist("some-private-lib", "1.0"),
        ]);
        let mut findings = Vec::new();
        advise(
            &mut findings,
            table(),
            Some(&compat),
            &machine(Some(12)),
            &RepoScan::default(),
        );
        assert!(findings.is_empty(), "{findings:#?}");
    }

    #[test]
    fn a_wheel_gap_for_the_venv_python_keeps_the_rebuild_fix() {
        let mut p = analysis("gdal", "3.8.0");
        p.requires_python = Some(">=3.9".into());
        p.supported = PyVersionSet::from_versions((10..=12).map(PyVersion::py3));
        let compat = report(vec![p]);
        let mut findings = vec![Finding {
            severity: Severity::Error,
            title: "Python 3.13 is not supported by this project".into(),
            detail: "gdal (supports 3.10-3.12) do not support Python 3.13.".into(),
            fix: FixKind::RecreateWithPython(PyVersion::py3(12)),
        }];
        advise(
            &mut findings,
            table(),
            Some(&compat),
            &machine(Some(13)),
            &RepoScan::default(),
        );
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].fix,
            FixKind::RecreateWithPython(PyVersion::py3(12))
        );
        assert!(findings[0]
            .detail
            .contains("To keep Python 3.13: pixi installs gdal"));
    }

    #[test]
    fn no_files_for_the_platform_keeps_its_error_and_gains_the_fix() {
        let mut p = analysis("pymol-open-source", "3.1.0");
        p.has_platform_wheels = false;
        p.no_files_for_platform = true;
        let compat = report(vec![p]);
        let mut findings = vec![Finding {
            severity: Severity::Error,
            title: "pymol-open-source 3.1.0 cannot be installed on Linux x64".into(),
            detail: "pymol-open-source 3.1.0 publishes no files for Linux x64.".into(),
            fix: FixKind::Manual,
        }];
        advise(
            &mut findings,
            table(),
            Some(&compat),
            &machine(None),
            &RepoScan::default(),
        );
        assert_eq!(findings[0].severity, Severity::Error);
        assert_eq!(findings[0].fix, FixKind::MigrateToPixi);
        assert!(findings[0]
            .detail
            .contains("pixi installs pymol-open-source"));
    }

    #[test]
    fn nvcc_packages_say_nvcc_and_extend_the_cuda_finding() {
        let compat = report(vec![sdist("flash-attn", "2.8.3")]);
        let mut findings = vec![Finding {
            severity: Severity::Warning,
            title: "flash-attn has no prebuilt wheel".into(),
            detail: "flash-attn publishes only an sdist.".into(),
            fix: FixKind::Manual,
        }];
        let repo = RepoScan {
            cuda_sources: vec!["kernels/a.cu".into()],
            languages: Vec::new(),
        };
        advise(&mut findings, table(), Some(&compat), &machine(None), &repo);
        assert_eq!(findings.len(), 1, "one nvcc note, not two: {findings:#?}");
        assert_eq!(findings[0].fix, FixKind::MigrateToPixi);
        assert!(findings[0]
            .detail
            .contains("bundle the CUDA runtime but not nvcc"));
    }

    #[test]
    fn cuda_sources_without_nvcc_are_an_info_note() {
        let repo = RepoScan {
            cuda_sources: vec!["src/kernel.cu".into()],
            languages: Vec::new(),
        };
        let mut findings = Vec::new();
        advise(&mut findings, table(), None, &machine(None), &repo);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Info);
        assert!(
            findings[0].detail.contains("src/kernel.cu")
                || findings[0].detail.contains("src\\kernel.cu")
        );

        let mut with_nvcc = Vec::new();
        let m = Machine {
            nvcc: true,
            ..machine(None)
        };
        advise(&mut with_nvcc, table(), None, &m, &repo);
        assert!(with_nvcc.is_empty());
    }

    #[test]
    fn mixed_language_repos_get_one_info_note() {
        let dir = tempdir("mixed");
        std::fs::write(dir.join("DESCRIPTION"), "Package: foo\nVersion: 1.0\n").unwrap();
        std::fs::write(dir.join("CMakeLists.txt"), "project(x)").unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("k.cu"), "").unwrap();
        std::fs::create_dir_all(dir.join(".venv")).unwrap();
        std::fs::write(dir.join(".venv").join("ignored.cu"), "").unwrap();
        let repo = scan_repo(&dir);
        assert_eq!(repo.cuda_sources, vec![PathBuf::from("src").join("k.cu")]);
        let langs: Vec<&str> = repo.languages.iter().map(|(l, _)| *l).collect();
        assert_eq!(langs, vec!["R", "C/C++"]);

        let mut findings = Vec::new();
        let m = Machine {
            nvcc: true,
            ..machine(None)
        };
        advise(&mut findings, table(), None, &m, &repo);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].title,
            "This project mixes Python with R and C/C++"
        );
        assert_eq!(findings[0].severity, Severity::Info);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pixi_project_gets_no_advice() {
        let dir = tempdir("pixi");
        std::fs::write(
            dir.join("pixi.toml"),
            "[workspace]\nname='x'\nchannels=['conda-forge']\nplatforms=['linux-64']\n",
        )
        .unwrap();
        std::fs::write(dir.join("CMakeLists.txt"), "").unwrap();
        std::fs::write(dir.join("main.py"), "").unwrap();
        let project = crate::core::project::scan(&dir);
        let mut findings = Vec::new();
        apply(&mut findings, &dir, &project, None, None, None);
        assert!(findings.is_empty(), "{findings:#?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tempdir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pypilot-pixi-advice-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn check_explains_the_wall_and_names_the_conda_forge_build() {
        let mut p = analysis("gdal", "3.8.0");
        p.supported = PyVersionSet::from_versions((10..=12).map(PyVersion::py3));
        p.requires_python = Some(">=3.9".into());
        let advice = check_advice(&p, table(), &machine(Some(13)));
        assert!(
            advice.reasons[0].starts_with(
                "no cp313 wheel for Linux x64: requires-python >=3.9 admits Python 3.13"
            ),
            "{advice:#?}"
        );
        assert!(
            advice.reasons[1].starts_with("source build: gdal's Python bindings")
                || advice.reasons[1].starts_with("source build: gdal"),
            "{advice:#?}"
        );
        let alt = advice.alternative.unwrap();
        assert!(alt.starts_with("conda-forge: gdal "), "{alt}");
        assert!(
            alt.ends_with(" available for linux-64/python 3.13 → pypilot migrate-to-pixi"),
            "{alt}"
        );

        // A package that installs fine has no reasons, but the table still
        // knows the conda-forge build when asked.
        let ok = check_advice(&analysis("gdal", "3.8.0"), table(), &machine(Some(12)));
        assert!(ok.reasons.is_empty());
        assert!(ok.alternative.is_some());
        let unknown = check_advice(&sdist("left-pad", "1.0"), table(), &machine(None));
        assert_eq!(
            unknown.reasons,
            vec!["no wheel for Linux x64: pip and uv build it from source"]
        );
        assert_eq!(unknown.alternative, None);
    }

    // --- build failures (recorded installer output) --------------------------

    const PIP_GDAL: &str = r#"Collecting gdal
  Using cached gdal-3.8.0.tar.gz (802 kB)
Building wheels for collected packages: gdal
  Building wheel for gdal (pyproject.toml) ... error
  error: subprocess-exited-with-error

  × Building wheel for gdal (pyproject.toml) did not run successfully.
  │ exit code: 1
  ╰─> [120 lines of output]
      running build_ext
      gcc -pthread -fPIC -I/usr/include/python3.12 -c gdal_wrap.cpp -o build/gdal_wrap.o
      extensions/gdal_wrap.cpp:3168:10: fatal error: cpl_port.h: No such file or directory
       3168 | #include "cpl_port.h"
      extensions/gdal_wrap.cpp:3170:10: fatal error: gdal.h: No such file or directory
      compilation terminated.
      error: command '/usr/bin/gcc' failed with exit code 1
      [end of output]
  ERROR: Failed building wheel for gdal
Failed to build gdal
"#;

    const UV_PSYCOPG2: &str = r#"  × Failed to build `psycopg2==2.9.10`
  ├─▶ The build backend returned an error
  ╰─▶ Call to `setuptools.build_meta:__legacy__.build_wheel` failed (exit status: 1)

      [stdout]
      running egg_info

      [stderr]
      Error: pg_config executable not found.

      pg_config is required to build psycopg2 from source.  Please add the directory
      containing pg_config to the $PATH or specify the full executable path with the
      option:
"#;

    const PIP_PYCAIRO: &str = r#"  Building wheel for pycairo (pyproject.toml) ... error
  error: subprocess-exited-with-error
      Run-time dependency cairo found: NO (tried pkgconfig and cmake)
      ../cairo/meson.build:31:12: ERROR: Dependency "cairo" not found, tried pkgconfig and cmake
"#;

    const CLANG_GRAPHVIZ: &str = r#"Building wheel for pygraphviz (pyproject.toml) did not run successfully.
      pygraphviz/graphviz_wrap.c:3020:10: fatal error: 'graphviz/cgraph.h' file not found
"#;

    const MSVC_PYAUDIO: &str = r#"Building wheel for pyaudio (pyproject.toml) ... error
      src/pyaudio/device_api.c(9): fatal error C1083: Cannot open include file: 'portaudio.h': No such file or directory
      error: command 'C:\\Program Files\\Microsoft Visual Studio\\cl.exe' failed with exit code 2
"#;

    const FLASH_ATTN: &str = r#"  × Failed to build `flash-attn==2.8.3`
      OSError: CUDA_HOME environment variable is not set. Please set it to your CUDA install root.
"#;

    const LIBGL: &str = r#"Traceback (most recent call last):
  File "app.py", line 1, in <module>
    import cv2
ImportError: libGL.so.1: cannot open shared object file: No such file or directory
"#;

    #[test]
    fn missing_headers_map_to_their_conda_forge_package() {
        let a = explain_build_failure(PIP_GDAL, table(), linux()).unwrap();
        assert_eq!(a.package.as_deref(), Some("gdal"));
        let m = a.missing.unwrap();
        assert_eq!(
            (m.kind, m.name.as_str(), m.conda.as_str()),
            ("header", "gdal.h", "libgdal")
        );
        assert!(a
            .message
            .starts_with("gdal failed to build because the header `gdal.h` is missing."));
        assert!(a
            .message
            .contains("pixi installs gdal from conda-forge with its native libraries"));
        assert!(a
            .message
            .ends_with("Run `pypilot migrate-to-pixi` to set the project up with pixi."));

        let a = explain_build_failure(CLANG_GRAPHVIZ, table(), linux()).unwrap();
        assert_eq!(a.missing.unwrap().conda, "graphviz");
        let a = explain_build_failure(MSVC_PYAUDIO, table(), linux()).unwrap();
        assert_eq!(a.package.as_deref(), Some("pyaudio"));
        assert_eq!(a.missing.unwrap().conda, "portaudio");
    }

    #[test]
    fn missing_tools_and_pkg_config_modules_are_recognised() {
        let a = explain_build_failure(UV_PSYCOPG2, table(), linux()).unwrap();
        assert_eq!(a.package.as_deref(), Some("psycopg2"));
        let m = a.missing.unwrap();
        assert_eq!(
            (m.kind, m.name.as_str(), m.conda.as_str()),
            ("tool", "pg_config", "libpq")
        );

        let a = explain_build_failure(PIP_PYCAIRO, table(), linux()).unwrap();
        let m = a.missing.unwrap();
        assert_eq!((m.kind, m.conda.as_str()), ("pkg-config", "cairo"));

        let a = explain_build_failure(FLASH_ATTN, table(), linux()).unwrap();
        assert!(
            a.message.contains("nvcc, the CUDA compiler, is missing"),
            "{}",
            a.message
        );
    }

    #[test]
    fn missing_shared_libraries_are_recognised() {
        let a = explain_build_failure(LIBGL, table(), linux()).unwrap();
        assert_eq!(a.missing.unwrap().conda, "libgl");
        assert!(a.message.contains("pixi can install `libgl`"));
    }

    #[test]
    fn unrelated_failures_get_no_advice() {
        for stderr in [
            "error: Failed to fetch: `https://pypi.org/simple/requests/`\n  Caused by: dns error",
            "× No solution found when resolving dependencies:\n  ╰─▶ Because foo==1.0 depends on bar>=2...",
            "ERROR: Could not find a version that satisfies the requirement nosuchpkg",
            "Header not found in the response",
        ] {
            assert_eq!(explain_build_failure(stderr, table(), linux()), None, "{stderr}");
        }
    }

    #[test]
    fn a_known_package_failing_without_a_named_cause_still_gets_advice() {
        let stderr = "  Building wheel for dlib (setup.py) ... error\n  error: subprocess-exited-with-error\n  ERROR: Failed building wheel for dlib\n";
        let a = explain_build_failure(stderr, table(), linux()).unwrap();
        assert_eq!(a.package.as_deref(), Some("dlib"));
        assert!(
            a.message
                .starts_with("dlib failed to build: dlib has no wheels"),
            "{}",
            a.message
        );
        assert!(a.message.contains("pixi installs dlib"));
    }
}
