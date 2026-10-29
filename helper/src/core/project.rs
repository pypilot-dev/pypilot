//! Project dependency parsing.
//!
//! Detects and parses the Python signal files in a workspace and normalizes their
//! dependencies to bare PyPI package names (versions/markers/extras stripped),
//! which is all the F3 engine needs. Conda `environment.yml` is detected and
//! flagged (per plan we don't fight conda in v1, only offer migration later).

use std::path::{Path, PathBuf};

/// A dependency as the project declares it: a name plus whatever version
/// constraint came with it.
///
/// The constraint matters. `mediapipe` resolves to the newest release, while
/// `mediapipe==0.10.14` resolves to one that supports a narrower set of Python
/// versions, so dropping the specifier produces a confidently wrong answer.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Requirement {
    /// Normalized per PEP 503.
    pub name: String,
    /// The specifier as written, e.g. `==0.10.14`. Empty when unconstrained.
    pub spec: String,
}

impl Requirement {
    /// A dependency with no version constraint.
    pub fn any(name: impl Into<String>) -> Requirement {
        Requirement {
            name: name.into(),
            spec: String::new(),
        }
    }

    pub fn is_pinned(&self) -> bool {
        !self.spec.is_empty()
    }
}

impl std::fmt::Display for Requirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}{}", self.name, self.spec)
    }
}

/// What we learned from a workspace's project files.
#[derive(Debug, Clone, Default)]
pub struct ProjectDeps {
    /// Signal files that were found and parsed.
    pub sources: Vec<PathBuf>,
    /// Declared dependencies, deduped and normalized.
    pub packages: Vec<Requirement>,
    /// `requires-python` from pyproject, if declared.
    pub declared_requires_python: Option<String>,
    /// True when an `environment.yml` is present.
    pub has_conda_env: bool,
    /// Python pin parsed from a conda `environment.yml`, if any.
    pub conda_python: Option<String>,
    /// The pixi manifest, when this is a pixi project (see
    /// [`crate::core::pixi::detect`]). pixi owns such a project: PyPilot
    /// must manage it through pixi, never through uv or pip.
    pub pixi_manifest: Option<PathBuf>,
    /// The pixi default environment's conda dependencies (`python`
    /// excluded). Kept apart from [`ProjectDeps::packages`]: conda names are
    /// not PyPI names, so they never go to the PyPI compatibility engine.
    pub conda_deps: Vec<crate::core::pixi::CondaDep>,
    /// The pixi default environment's `python` spec as written in the
    /// manifest (`3.12.*`), which decides the interpreter pixi installs.
    pub pixi_python: Option<String>,
}

impl ProjectDeps {
    pub fn is_python_project(&self) -> bool {
        !self.sources.is_empty()
    }

    pub fn is_pixi(&self) -> bool {
        self.pixi_manifest.is_some()
    }

    /// Just the names, for display and for callers that don't care about pins.
    pub fn names(&self) -> Vec<String> {
        self.packages.iter().map(|r| r.name.clone()).collect()
    }
}

/// Scan a workspace directory (non-recursively at the root) for project files.
pub fn scan(workspace: &Path) -> ProjectDeps {
    let mut deps = ProjectDeps::default();
    let mut names = Vec::new();

    let pyproject = workspace.join("pyproject.toml");
    if pyproject.is_file() {
        if let Some(text) = read_manifest(&pyproject) {
            let (pkgs, rp) = parse_pyproject(&text);
            names.extend(pkgs);
            deps.declared_requires_python = rp;
            deps.sources.push(pyproject);
        }
    }

    let requirements = workspace.join("requirements.txt");
    if requirements.is_file() {
        if let Some(text) = read_manifest(&requirements) {
            names.extend(parse_requirements(&text));
            deps.sources.push(requirements);
        }
    }

    let env_yml = workspace.join("environment.yml");
    if env_yml.is_file() {
        if let Some(text) = read_manifest(&env_yml) {
            let (pkgs, py) = parse_environment_yml(&text);
            names.extend(pkgs);
            deps.conda_python = py;
        }
        deps.has_conda_env = true;
        deps.sources.push(env_yml);
    }

    // A pixi project is a Python project: its PyPI dependencies are checked
    // like any other, and its conda ones are recorded but kept off PyPI.
    // pixi's python spec is what decides the interpreter, so it outranks a
    // pyproject's `requires-python`. A pixi workspace that asks for neither
    // Python nor PyPI packages (a C++ or Rust one) is still pixi's, but its
    // manifest alone does not make it a Python project: only Python or
    // notebook files next to it do, as for any folder (checked below).
    //
    // pixi installs what its manifest names and nothing else, so its PyPI
    // dependencies replace the ones read above (pixi reads a pyproject's
    // `[project.dependencies]` itself when that is its manifest). After
    // `pypilot migrate-to-pixi`, requirements.txt stays for CI, but gdal in it
    // now comes from conda-forge; reading it as a PyPI sdist would warn about
    // a compile that never happens.
    if let Some(pixi) = crate::core::pixi::detect(workspace) {
        names = pixi.pypi_deps.clone();
        if let Some(rp) = pixi
            .python
            .as_deref()
            .and_then(crate::core::pixi::python_spec_pep440)
        {
            deps.declared_requires_python = Some(rp);
        }
        deps.conda_deps = pixi.conda_deps.clone();
        deps.pixi_python = pixi.python.clone();
        if pixi.uses_python() && !deps.sources.contains(&pixi.manifest) {
            deps.sources.push(pixi.manifest.clone());
        }
        deps.pixi_manifest = Some(pixi.manifest);
    }

    // A bare `.py` file also makes this a Python project even with no manifest,
    // and so does a notebook: a folder of `.ipynb` files needs an environment
    // with a kernel just as much.
    if deps.sources.is_empty() {
        if has_python_file(workspace) {
            deps.sources.push(workspace.join("*.py"));
        } else if has_notebooks(workspace) {
            deps.sources.push(workspace.join("*.ipynb"));
        }
    }

    deps.packages = dedupe(names);
    deps
}

/// Normalize a PyPI project name (PEP 503): lowercase, runs of `-_.` collapse to `-`.
pub fn normalize_name(name: &str) -> String {
    let lowered = name.trim().to_lowercase();
    let mut out = String::with_capacity(lowered.len());
    let mut prev_sep = false;
    for c in lowered.chars() {
        if matches!(c, '-' | '_' | '.') {
            if !prev_sep && !out.is_empty() {
                out.push('-');
            }
            prev_sep = true;
        } else {
            out.push(c);
            prev_sep = false;
        }
    }
    out.trim_end_matches('-').to_string()
}

/// Parse one dependency line into a name and its version specifier.
///
/// Environment markers and extras are dropped: neither changes which release an
/// installer picks for the current machine. The version specifier is kept,
/// because it does.
pub fn parse_requirement(line: &str) -> Option<Requirement> {
    let name = requirement_name(line)?;

    let line = strip_inline_comment(line.trim());
    // Cut the marker first so `; python_version<'3.10'` is not read as a bound.
    let without_marker = line.split(';').next().unwrap_or(line);
    // A direct reference (`name @ url`) names its artifact outright, and an `=`
    // inside the URL is not a version bound.
    if without_marker.contains('@') {
        return Some(Requirement::any(name));
    }
    // Then drop extras, whose brackets can contain commas.
    let without_extras = match (without_marker.find('['), without_marker.find(']')) {
        (Some(open), Some(close)) if close > open => {
            format!(
                "{}{}",
                &without_marker[..open],
                &without_marker[close + 1..]
            )
        }
        _ => without_marker.to_string(),
    };

    // The specifier begins at the first comparison character.
    let spec = match without_extras.find(['=', '<', '>', '!', '~']) {
        Some(i) => without_extras[i..]
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect(),
        None => String::new(),
    };

    Some(Requirement { name, spec })
}

/// Extract the bare package name from a single requirement specifier line.
/// Returns `None` for comments, options, includes, blank lines, and URLs.
pub fn requirement_name(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') || line.starts_with('-') {
        return None;
    }
    let line = strip_inline_comment(line);
    // Direct URL / VCS references: "name @ url" keeps the name; bare URLs are skipped.
    if let Some(idx) = line.find('@') {
        let name = line[..idx].trim();
        if !name.is_empty()
            && name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric())
        {
            return Some(normalize_name(strip_extras(name)));
        }
        return None;
    }
    if line.contains("://") {
        return None;
    }
    // Cut at the first version/marker/extras delimiter.
    let end = line
        .find(['=', '<', '>', '!', '~', ';', '[', ' ', '('])
        .unwrap_or(line.len());
    let name = line[..end].trim();
    if name.is_empty() {
        return None;
    }
    Some(normalize_name(name))
}

/// Cut an inline comment. Per pip's rules a `#` only starts one when whitespace
/// precedes it, so a URL fragment like `#egg=name` survives.
fn strip_inline_comment(line: &str) -> &str {
    match line
        .as_bytes()
        .windows(2)
        .position(|w| w[0].is_ascii_whitespace() && w[1] == b'#')
    {
        Some(i) => line[..i].trim_end(),
        None => line,
    }
}

/// Read a manifest as text, whatever encoding Windows tooling left it in.
///
/// In Windows PowerShell 5, `pip freeze > requirements.txt` writes UTF-16LE with
/// a byte-order mark, and some editors prefix UTF-8 with one. A plain
/// `read_to_string` rejects the first file outright and turns the second one's
/// first package name into one that starts with an invisible character.
fn read_manifest(path: &Path) -> Option<String> {
    decode_text(std::fs::read(path).ok()?)
}

fn decode_text(bytes: Vec<u8>) -> Option<String> {
    let utf16 = |rest: &[u8], unit: fn([u8; 2]) -> u16| {
        let units: Vec<u16> = rest.chunks_exact(2).map(|c| unit([c[0], c[1]])).collect();
        String::from_utf16(&units).ok()
    };
    match bytes.as_slice() {
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8(rest.to_vec()).ok(),
        [0xFF, 0xFE, rest @ ..] => utf16(rest, u16::from_le_bytes),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, u16::from_be_bytes),
        _ => String::from_utf8(bytes).ok(),
    }
}

fn strip_extras(name: &str) -> &str {
    match name.find('[') {
        Some(i) => &name[..i],
        None => name,
    }
}

fn parse_requirements(text: &str) -> Vec<Requirement> {
    text.lines().filter_map(parse_requirement).collect()
}

/// Parse dependencies + requires-python from pyproject.toml (PEP 621 and Poetry).
fn parse_pyproject(text: &str) -> (Vec<Requirement>, Option<String>) {
    let mut names = Vec::new();
    let mut requires_python = None;

    let Ok(doc) = toml::from_str::<toml::Value>(text) else {
        return (names, requires_python);
    };

    // PEP 621: [project]
    if let Some(project) = doc.get("project").and_then(|v| v.as_table()) {
        if let Some(rp) = project.get("requires-python").and_then(|v| v.as_str()) {
            requires_python = Some(rp.to_string());
        }
        if let Some(deps) = project.get("dependencies").and_then(|v| v.as_array()) {
            for d in deps.iter().filter_map(|v| v.as_str()) {
                if let Some(r) = parse_requirement(d) {
                    names.push(r);
                }
            }
        }
        // Optional-dependencies groups.
        if let Some(opt) = project
            .get("optional-dependencies")
            .and_then(|v| v.as_table())
        {
            for group in opt.values().filter_map(|v| v.as_array()) {
                for d in group.iter().filter_map(|v| v.as_str()) {
                    if let Some(r) = parse_requirement(d) {
                        names.push(r);
                    }
                }
            }
        }
    }

    // Poetry: [tool.poetry.dependencies] — keys are names; a "python" key is the
    // interpreter constraint, not a package.
    if let Some(poetry) = doc
        .get("tool")
        .and_then(|t| t.get("poetry"))
        .and_then(|p| p.get("dependencies"))
        .and_then(|d| d.as_table())
    {
        for key in poetry.keys() {
            if key.eq_ignore_ascii_case("python") {
                if requires_python.is_none() {
                    if let Some(rp) = poetry.get(key).and_then(|v| v.as_str()) {
                        requires_python = Some(caret_to_pep440(rp));
                    }
                }
                continue;
            }
            let spec = poetry
                .get(key)
                .and_then(|v| v.as_str())
                .map(caret_to_pep440)
                .filter(|s| !s.is_empty() && s != "*")
                .unwrap_or_default();
            names.push(Requirement {
                name: normalize_name(key),
                spec,
            });
        }
    }

    (names, requires_python)
}

/// Best-effort conda environment.yml: dependency names + the `python=` pin.
fn parse_environment_yml(text: &str) -> (Vec<Requirement>, Option<String>) {
    let mut names = Vec::new();
    let mut python = None;
    let mut in_deps = false;

    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with("dependencies:") {
            in_deps = true;
            continue;
        }
        if in_deps {
            // A new top-level key ends the deps block.
            if !raw.starts_with(' ') && !line.starts_with('-') && line.ends_with(':') {
                in_deps = false;
                continue;
            }
            if let Some(item) = line.strip_prefix('-') {
                let item = item.trim();
                if item.ends_with(':') {
                    // e.g. "pip:" sub-block; keep scanning its children as deps.
                    continue;
                }
                // "python=3.10" or "numpy>=1.20".
                let name_part = item
                    .split(['=', '<', '>', '!', '~', ' '])
                    .next()
                    .unwrap_or("")
                    .trim();
                if name_part.eq_ignore_ascii_case("python") {
                    python = item
                        .split('=')
                        .nth(1)
                        .map(|v| v.trim().trim_matches('=').to_string())
                        .filter(|s| !s.is_empty());
                } else if !name_part.is_empty() {
                    names.push(Requirement {
                        name: normalize_name(name_part),
                        spec: conda_spec_to_pep440(&item[name_part.len()..]),
                    });
                }
            }
        }
    }
    (names, python)
}

/// Convert the version part of a conda dependency line into PEP 440.
///
/// conda's grammar overlaps with PEP 440 but is not identical:
///   * a single `=` is a *prefix* match (`numpy=1.24` allows 1.24.1), which
///     PEP 440 spells `==1.24.*`;
///   * a third field is a build string (`numpy=1.24=py311h5a2b...`), which has
///     no PyPI equivalent and is dropped;
///   * `>=`, `<=`, `!=`, `<`, `>` and `==` already mean the same thing.
///
/// Anything unrecognized yields an empty spec rather than a guess, since a
/// wrong constraint is worse than an absent one.
fn conda_spec_to_pep440(rest: &str) -> String {
    let rest = rest.trim();
    if rest.is_empty() {
        return String::new();
    }

    // Two-character operators first, so `>=` is not read as a bare `>`.
    for op in ["==", ">=", "<=", "!=", "~="] {
        if let Some(v) = rest.strip_prefix(op) {
            let v = version_field(v);
            return if v.is_empty() {
                String::new()
            } else {
                format!("{op}{v}")
            };
        }
    }
    for op in ['>', '<'] {
        if let Some(v) = rest.strip_prefix(op) {
            let v = version_field(v);
            return if v.is_empty() {
                String::new()
            } else {
                format!("{op}{v}")
            };
        }
    }

    // A lone `=` is conda's prefix match.
    if let Some(v) = rest.strip_prefix('=') {
        let v = version_field(v);
        if v.is_empty() {
            return String::new();
        }
        // An already-exact version stays exact; otherwise widen to the prefix
        // form so `numpy=1.24` keeps matching 1.24.x as conda intends.
        return if v.ends_with('*') {
            format!("=={v}")
        } else {
            format!("=={v}.*")
        };
    }

    String::new()
}

/// Take the version out of a conda spec, discarding any build-string field.
fn version_field(s: &str) -> &str {
    s.split('=').next().unwrap_or("").trim()
}

/// Convert a Poetry caret/tilde constraint to an approximate PEP 440 form.
fn caret_to_pep440(spec: &str) -> String {
    let s = spec.trim();
    if let Some(v) = s.strip_prefix('^') {
        // ^3.9 → >=3.9,<4 (major-locked); good enough for interpreter reasoning.
        let major = v.split('.').next().unwrap_or("3");
        format!(
            ">={v},<{}",
            major.parse::<u32>().map(|m| m + 1).unwrap_or(4)
        )
    } else if let Some(v) = s.strip_prefix('~') {
        format!(">={v}")
    } else {
        s.to_string()
    }
}

fn has_python_file(dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return false;
    };
    rd.flatten()
        .any(|e| e.path().extension().is_some_and(|x| x == "py"))
}

/// How deep [`has_notebooks`] looks below the workspace root. Notebooks live in
/// `notebooks/`, `examples/`, `docs/tutorials/` and the like, rarely deeper, and
/// the walk runs on every assessment, so it has to stay cheap on huge repos.
const NOTEBOOK_SEARCH_DEPTH: usize = 4;

/// Directories that never hold the user's own notebooks: environments, VCS and
/// build output, and Jupyter's own autosave copies.
const NOTEBOOK_SKIP_DIRS: &[&str] = &[
    ".venv",
    ".pixi",
    "venv",
    "env",
    ".git",
    "node_modules",
    ".ipynb_checkpoints",
    "__pycache__",
    "target",
];

/// Does the workspace contain any Jupyter notebook?
///
/// A notebook can only run on an environment that has `ipykernel`; without it
/// VS Code and Jupyter quietly pick some other Python. This decides whether
/// PyPilot needs to care. Bounded to [`NOTEBOOK_SEARCH_DEPTH`] levels, skips
/// environments (by name and by `pyvenv.cfg`), and stops at the first hit.
pub fn has_notebooks(workspace: &Path) -> bool {
    fn walk(dir: &Path, depth: usize) -> bool {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return false;
        };
        let mut subdirs = Vec::new();
        for entry in rd.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_file() {
                if path.extension().is_some_and(|x| x == "ipynb") {
                    return true;
                }
            } else if kind.is_dir() && depth > 0 {
                let name = entry.file_name();
                let skip = NOTEBOOK_SKIP_DIRS.iter().any(|s| name == *s)
                    || path.join("pyvenv.cfg").is_file();
                if !skip {
                    subdirs.push(path);
                }
            }
        }
        // Files at this level first, so a root-level notebook never pays for
        // the subdirectory walk.
        subdirs.into_iter().any(|d| walk(&d, depth - 1))
    }
    walk(workspace, NOTEBOOK_SEARCH_DEPTH)
}

/// Manifests that mark a directory as the root of a Python project, the place
/// its environment belongs. `environment.yml` counts at the workspace root, where
/// [`scan`] reads it, but is too common in non-Python subfolders to count below.
const PROJECT_MANIFESTS: &[&str] = &["pyproject.toml", "requirements.txt", "pixi.toml"];

/// How deep [`project_root`] looks for a nested project when the workspace root
/// has none: `backend/` and `services/api/` are covered, deeper layouts are rare
/// enough to leave to opening that folder directly.
const NESTED_PROJECT_DEPTH: usize = 2;

/// Files that make the workspace root a Python project without making a
/// subfolder one: a root `setup.py` next to `docs/requirements.txt` is still a
/// root project.
const ROOT_ONLY_MARKERS: &[&str] = &["environment.yml", "setup.py", "setup.cfg", "Pipfile"];

fn has_manifest(dir: &Path) -> bool {
    PROJECT_MANIFESTS.iter().any(|m| dir.join(m).is_file())
}

/// The directory PyPilot should treat as the project for a workspace.
///
/// Normally that is the workspace itself. When the workspace root has no Python
/// of its own (no manifest, no legacy `setup.py`/`setup.cfg`/`Pipfile`, no `.py`
/// file or notebook) but exactly one subfolder has a `pyproject.toml` or
/// `requirements.txt`, as when a repo keeps its Python code in `backend/`, that
/// subfolder is the project: its `.venv` is the one to set up, install into and
/// check imports against. Several nested projects are ambiguous, so the
/// workspace root stays in charge then.
pub fn project_root(workspace: &Path) -> PathBuf {
    let own_python = has_manifest(workspace)
        || ROOT_ONLY_MARKERS
            .iter()
            .any(|m| workspace.join(m).is_file())
        || std::fs::read_dir(workspace).is_ok_and(|rd| {
            rd.flatten().any(|e| {
                e.path()
                    .extension()
                    .is_some_and(|x| x == "py" || x == "ipynb")
            })
        });
    if own_python {
        return workspace.to_path_buf();
    }

    fn walk(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        let mut subdirs: Vec<PathBuf> = rd
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                !name.starts_with('.')
                    && !NOTEBOOK_SKIP_DIRS.iter().any(|s| name == *s)
                    && !e.path().join("pyvenv.cfg").is_file()
            })
            .map(|e| e.path())
            .collect();
        subdirs.sort();
        for sub in subdirs {
            if has_manifest(&sub) {
                // A project's own subfolders belong to it, not to a sibling.
                found.push(sub);
            } else if depth > 1 {
                walk(&sub, depth - 1, found);
            }
            // Two is already ambiguous; no need to look further.
            if found.len() > 1 {
                return;
            }
        }
    }

    let mut found = Vec::new();
    walk(workspace, NESTED_PROJECT_DEPTH, &mut found);
    match found.as_slice() {
        [only] => only.clone(),
        _ => workspace.to_path_buf(),
    }
}

fn dedupe(mut reqs: Vec<Requirement>) -> Vec<Requirement> {
    reqs.retain(|r| !r.name.is_empty());
    reqs.sort();
    // Same name declared twice: keep the constrained one, since an installer
    // has to satisfy every mention.
    reqs.dedup_by(|a, b| {
        if a.name != b.name {
            return false;
        }
        if b.spec.is_empty() && !a.spec.is_empty() {
            b.spec = a.spec.clone();
        }
        true
    });
    reqs
}

/// Translate a conda `environment.yml` into a minimal PEP 621 `pyproject.toml`,
/// per F5's "offer translation to pyproject/uv" migration action.
///
/// `deps` must have `has_conda_env` set. `project_name` is typically the
/// workspace directory name. This is deliberately conservative: conda package
/// names do not always match their PyPI equivalent (and some, like compiled
/// system libraries, have no PyPI equivalent at all), and guessing a mapping
/// per name would be exactly the kind of per-library hardcoding F3 refuses to
/// do elsewhere. Every parsed dependency is carried over as-is, and the caller
/// is told plainly to review it — an honest starting point, not a silent one.
pub fn conda_migration_pyproject(deps: &ProjectDeps, project_name: &str) -> String {
    // normalize_name doesn't touch whitespace (it never appears in a parsed
    // requirement name), but a directory name like "My Demo Project" needs it
    // collapsed too to satisfy PEP 621's `[A-Za-z0-9._-]` project name rule.
    let spaced = project_name.replace(char::is_whitespace, "-");
    let name = normalize_name(&spaced);
    let name = if name.is_empty() {
        "project".to_string()
    } else {
        name
    };

    let requires_python = deps.conda_python.as_deref().map(|v| {
        let v = v.trim();
        if v.starts_with(['>', '<', '=', '~', '!']) {
            v.to_string()
        } else {
            format!(">={v}")
        }
    });

    let mut out = String::new();
    out.push_str("[project]\n");
    out.push_str(&format!("name = \"{name}\"\n"));
    out.push_str("version = \"0.1.0\"\n");
    if let Some(rp) = &requires_python {
        out.push_str(&format!("requires-python = \"{rp}\"\n"));
    }

    if deps.packages.is_empty() {
        out.push_str("dependencies = []\n");
    } else {
        out.push_str("dependencies = [\n");
        for req in &deps.packages {
            out.push_str(&format!("    \"{req}\",\n"));
        }
        out.push_str("]\n");
    }

    // No [build-system] section: this describes an application (a venv plus
    // dependencies) for uv to manage, not a redistributable package. Adding a
    // build backend nobody asked for would be an assumption beyond the ask.
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notebooks_are_found_but_not_inside_environments() {
        let ws = std::env::temp_dir().join("pypilot-notebook-detect-test");
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("main.py"), "").unwrap();
        assert!(!has_notebooks(&ws));

        // Notebooks inside environments, checkpoints, or any dir holding a
        // pyvenv.cfg are someone else's, not the project's.
        for skipped in [".venv/share", ".ipynb_checkpoints", "node_modules/x"] {
            std::fs::create_dir_all(ws.join(skipped)).unwrap();
            std::fs::write(ws.join(skipped).join("a.ipynb"), "{}").unwrap();
        }
        std::fs::create_dir_all(ws.join("myenv")).unwrap();
        std::fs::write(ws.join("myenv").join("pyvenv.cfg"), "").unwrap();
        std::fs::write(ws.join("myenv").join("b.ipynb"), "{}").unwrap();
        assert!(!has_notebooks(&ws));

        // Deeper than the bound is not looked at.
        let deep = ws.join("a/b/c/d/e");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("deep.ipynb"), "{}").unwrap();
        assert!(!has_notebooks(&ws));

        // Within the bound is found.
        std::fs::write(ws.join("a/b/c/d/ok.ipynb"), "{}").unwrap();
        assert!(has_notebooks(&ws));

        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn normalize_examples() {
        assert_eq!(normalize_name("Flask"), "flask");
        assert_eq!(normalize_name("opencv_python"), "opencv-python");
        assert_eq!(normalize_name("ruamel.yaml"), "ruamel-yaml");
    }

    #[test]
    fn requirement_name_strips_specifiers() {
        assert_eq!(
            requirement_name("mediapipe==0.10.9").as_deref(),
            Some("mediapipe")
        );
        assert_eq!(requirement_name("torch>=2.0,<3").as_deref(), Some("torch"));
        assert_eq!(
            requirement_name("uvicorn[standard]").as_deref(),
            Some("uvicorn")
        );
        assert_eq!(
            requirement_name("numpy ; python_version<'3.10'").as_deref(),
            Some("numpy")
        );
        assert_eq!(requirement_name("# a comment"), None);
        assert_eq!(requirement_name("-r base.txt"), None);
        assert_eq!(requirement_name(""), None);
    }

    #[test]
    fn inline_comments_stay_out_of_the_specifier() {
        assert_eq!(
            parse_requirement("numpy==1.26.4  # pinned for torch"),
            Some(req("numpy", "==1.26.4"))
        );
        assert_eq!(
            parse_requirement("numpy>=1.24\t# tab-separated"),
            Some(req("numpy", ">=1.24"))
        );
    }

    #[test]
    fn direct_references_carry_no_version_bound() {
        assert_eq!(
            parse_requirement("demo @ https://example.com/demo.whl?sig=abc"),
            Some(req("demo", ""))
        );
        assert_eq!(
            parse_requirement("demo @ git+https://example.com/demo.git#egg=demo"),
            Some(req("demo", ""))
        );
    }

    #[test]
    fn manifests_decode_through_a_byte_order_mark() {
        let utf8_bom = b"\xEF\xBB\xBFnumpy==1.26.4\n".to_vec();
        assert_eq!(decode_text(utf8_bom).as_deref(), Some("numpy==1.26.4\n"));

        // What Windows PowerShell 5 writes for `pip freeze > requirements.txt`.
        let mut utf16le = vec![0xFF, 0xFE];
        utf16le.extend(
            "numpy==1.26.4\r\n"
                .encode_utf16()
                .flat_map(u16::to_le_bytes),
        );
        let text = decode_text(utf16le).unwrap();
        assert_eq!(parse_requirements(&text), vec![req("numpy", "==1.26.4")]);

        assert_eq!(decode_text(b"plain".to_vec()).as_deref(), Some("plain"));
        assert_eq!(decode_text(vec![0xC3]), None);
    }

    #[test]
    fn parse_pep621_pyproject() {
        let text = r#"
[project]
name = "demo"
requires-python = ">=3.9,<3.13"
dependencies = ["mediapipe==0.10.9", "numpy>=1.24"]
"#;
        let (names, rp) = parse_pyproject(text);
        assert_eq!(rp.as_deref(), Some(">=3.9,<3.13"));
        assert!(names.iter().any(|r| r.name == "mediapipe"));
        assert!(names.iter().any(|r| r.name == "numpy"));
    }

    #[test]
    fn parse_poetry_pyproject() {
        let text = r#"
[tool.poetry.dependencies]
python = "^3.10"
requests = "^2.31"
"#;
        let (names, rp) = parse_pyproject(text);
        assert!(names.iter().any(|r| r.name == "requests"));
        assert!(!names.iter().any(|r| r.name == "python"));
        assert!(rp.unwrap().starts_with(">=3.10"));
    }

    #[test]
    fn parse_conda_env() {
        let text = "name: demo\ndependencies:\n  - python=3.10\n  - numpy>=1.20\n";
        let (names, py) = parse_environment_yml(text);
        assert_eq!(py.as_deref(), Some("3.10"));
        assert!(names.iter().any(|r| r.name == "numpy"));
        assert!(!names.iter().any(|r| r.name == "python"));
    }

    #[test]
    fn conda_dependency_pins_are_kept() {
        // Dropping these silently produced a pyproject.toml with no version
        // constraints at all, which is a different project from the one the
        // environment.yml described.
        let text = "dependencies:\n  \
             - numpy>=1.24\n  \
             - scipy=1.11\n  \
             - pandas==2.0.3\n  \
             - requests\n  \
             - pip:\n    \
             - mediapipe==0.10.14\n";
        let (names, _) = parse_environment_yml(text);

        let spec_of = |n: &str| {
            names
                .iter()
                .find(|r| r.name == n)
                .map(|r| r.spec.clone())
                .unwrap_or_else(|| panic!("{n} missing from {names:?}"))
        };
        assert_eq!(spec_of("numpy"), ">=1.24");
        // conda's single `=` is a prefix match, not an exact pin.
        assert_eq!(spec_of("scipy"), "==1.11.*");
        assert_eq!(spec_of("pandas"), "==2.0.3");
        assert_eq!(spec_of("requests"), "");
        assert_eq!(spec_of("mediapipe"), "==0.10.14");
    }

    #[test]
    fn conda_build_strings_are_discarded() {
        // `name=version=build` has no PyPI equivalent for the build field.
        let text = "dependencies:\n  - numpy=1.24=py311h5a2b\n";
        let (names, _) = parse_environment_yml(text);
        assert_eq!(names[0].spec, "==1.24.*");
    }

    #[test]
    fn conda_migration_produces_a_usable_pyproject() {
        let deps = ProjectDeps {
            sources: vec!["environment.yml".into()],
            packages: vec![Requirement::any("numpy"), req("pillow", "==10.0.0")],
            declared_requires_python: None,
            has_conda_env: true,
            conda_python: Some("3.10".to_string()),
            ..Default::default()
        };
        let toml = conda_migration_pyproject(&deps, "My Demo Project");

        assert!(toml.contains("name = \"my-demo-project\""));
        assert!(
            toml.contains("requires-python = \">=3.10\""),
            "a bare conda python pin must gain a >= operator: {toml}"
        );
        assert!(toml.contains("\"numpy\""));
        assert!(toml.contains("\"pillow==10.0.0\""));
        assert!(
            !toml.contains("build-system"),
            "an application migration should not assume a build backend"
        );
    }

    #[test]
    fn conda_migration_handles_no_python_pin_and_empty_name() {
        let deps = ProjectDeps {
            has_conda_env: true,
            ..Default::default()
        };
        let toml = conda_migration_pyproject(&deps, "");
        assert!(!toml.contains("requires-python"));
        assert!(toml.contains("name = \"project\""));
        assert!(toml.contains("dependencies = []"));
    }

    #[test]
    fn a_folder_of_notebooks_is_a_python_project() {
        let dir = std::env::temp_dir().join("pypilot-notebook-only-project");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("analysis.ipynb"), "{}").unwrap();

        let deps = scan(&dir);
        assert!(deps.is_python_project());
        assert!(deps.packages.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pixi_toml_alone_is_a_python_project() {
        let dir = fresh_dir("pypilot-scan-pixi-toml");
        std::fs::write(
            dir.join("pixi.toml"),
            include_str!("../../tests/fixtures/pixi/pixi-0.81-init.pixi.toml"),
        )
        .unwrap();
        let deps = scan(&dir);
        assert!(deps.is_python_project());
        assert!(deps.is_pixi());
        assert_eq!(deps.pixi_manifest, Some(dir.join("pixi.toml")));
        assert_eq!(deps.sources, [dir.join("pixi.toml")]);
        // PyPI deps go to the compatibility engine, conda deps do not.
        assert_eq!(deps.packages, [req("requests", ">=2.34.2,<3")]);
        assert_eq!(deps.conda_deps.len(), 1);
        assert_eq!(deps.conda_deps[0].name, "numpy");
        assert_eq!(deps.declared_requires_python.as_deref(), Some("==3.12.*"));
        assert_eq!(deps.pixi_python.as_deref(), Some("3.12.*"));
    }

    #[test]
    fn a_pixi_project_checks_only_what_pixi_installs() {
        // After `pypilot migrate-to-pixi`: requirements.txt stays for CI, but
        // numpy and gdal now come from conda-forge.
        let dir = fresh_dir("pypilot-scan-pixi-migrated");
        std::fs::write(
            dir.join("pixi.toml"),
            include_str!("../../tests/fixtures/pixi/pixi-0.81-init.pixi.toml"),
        )
        .unwrap();
        std::fs::write(dir.join("requirements.txt"), "numpy\ngdal\nrequests\n").unwrap();
        let deps = scan(&dir);
        assert_eq!(deps.packages, [req("requests", ">=2.34.2,<3")]);
        assert_eq!(deps.conda_deps[0].name, "numpy");
    }

    #[test]
    fn a_pixi_workspace_without_python_is_not_a_python_project() {
        let dir = fresh_dir("pypilot-scan-pixi-cpp");
        std::fs::write(
            dir.join("pixi.toml"),
            "[workspace]\nchannels = [\"conda-forge\"]\nplatforms = [\"linux-64\"]\n\n[dependencies]\ncmake = \"*\"\ncxx-compiler = \"*\"\n",
        )
        .unwrap();
        touch(&dir.join("main.cpp"));
        let deps = scan(&dir);
        assert!(deps.is_pixi());
        assert!(!deps.is_python_project());
        assert!(deps.sources.is_empty());

        // A Python script next to it makes it one again.
        touch(&dir.join("build.py"));
        assert!(scan(&dir).is_python_project());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pixi_pyproject_merges_both_dependency_lists() {
        let dir = fresh_dir("pypilot-scan-pixi-pyproject");
        std::fs::write(
            dir.join("pyproject.toml"),
            "[project]
name = \"x\"
requires-python = \">=3.10\"
dependencies = [\"requests>=2\"]

[tool.pixi.workspace]
platforms = [\"win-64\"]

[tool.pixi.dependencies]
python = \"3.11.*\"
numpy = \"*\"

[tool.pixi.pypi-dependencies]
rich = \">=13\"
",
        )
        .unwrap();
        let deps = scan(&dir);
        assert!(deps.is_pixi());
        assert_eq!(deps.sources, [dir.join("pyproject.toml")]);
        assert_eq!(deps.packages, [req("requests", ">=2"), req("rich", ">=13")]);
        assert_eq!(deps.conda_deps[0].name, "numpy");
        // pixi's python pin decides the interpreter.
        assert_eq!(deps.declared_requires_python.as_deref(), Some("==3.11.*"));

        // A plain pyproject is not pixi.
        let plain = fresh_dir("pypilot-scan-plain-pyproject");
        std::fs::write(
            plain.join("pyproject.toml"),
            "[project]
name = \"x\"
",
        )
        .unwrap();
        assert!(!scan(&plain).is_pixi());
    }

    #[test]
    fn pixi_manifests_mark_project_roots_and_envs_are_skipped() {
        let ws = fresh_dir("pypilot-root-pixi");
        touch(&ws.join("README.md"));
        touch(&ws.join("analysis").join("pixi.toml"));
        assert_eq!(project_root(&ws), ws.join("analysis"));

        // A notebook inside a pixi prefix is not the user's.
        let ws = fresh_dir("pypilot-notebook-pixi-env");
        touch(&ws.join(".pixi/envs/default/share/jupyter/demo.ipynb"));
        assert!(!has_notebooks(&ws));
    }

    /// A fresh, empty directory under the temp dir for one test.
    fn fresh_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "").unwrap();
    }

    #[test]
    fn a_workspace_with_its_own_manifest_is_its_own_project() {
        let ws = fresh_dir("pypilot-root-own-manifest");
        touch(&ws.join("pyproject.toml"));
        // Even with a nested project next to it.
        touch(&ws.join("tools/requirements.txt"));
        assert_eq!(project_root(&ws), ws);

        let ws2 = fresh_dir("pypilot-root-own-requirements");
        touch(&ws2.join("requirements.txt"));
        touch(&ws2.join("backend/pyproject.toml"));
        assert_eq!(project_root(&ws2), ws2);
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&ws2);
    }

    #[test]
    fn a_single_nested_project_becomes_the_project() {
        let ws = fresh_dir("pypilot-root-single-nested");
        touch(&ws.join("README.md"));
        touch(&ws.join("backend/pyproject.toml"));
        // Its own subfolders do not make it ambiguous.
        touch(&ws.join("backend/docs/requirements.txt"));
        assert_eq!(project_root(&ws), ws.join("backend"));

        let deeper = fresh_dir("pypilot-root-depth-two");
        touch(&deeper.join("services/api/requirements.txt"));
        assert_eq!(project_root(&deeper), deeper.join("services/api"));
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&deeper);
    }

    #[test]
    fn several_nested_projects_or_none_keep_the_workspace() {
        let ws = fresh_dir("pypilot-root-ambiguous");
        touch(&ws.join("api/pyproject.toml"));
        touch(&ws.join("worker/pyproject.toml"));
        assert_eq!(project_root(&ws), ws);

        let empty = fresh_dir("pypilot-root-none");
        assert_eq!(project_root(&empty), empty);

        // Python at the root keeps the root, whatever a subfolder declares.
        let scripts = fresh_dir("pypilot-root-scripts");
        touch(&scripts.join("main.py"));
        touch(&scripts.join("backend/pyproject.toml"));
        assert_eq!(project_root(&scripts), scripts);
        let legacy = fresh_dir("pypilot-root-setup-py");
        touch(&legacy.join("setup.py"));
        touch(&legacy.join("docs/requirements.txt"));
        assert_eq!(project_root(&legacy), legacy);

        // Manifests inside environments, hidden or too-deep folders don't count.
        let hidden = fresh_dir("pypilot-root-hidden");
        touch(&hidden.join(".venv/lib/pyproject.toml"));
        touch(&hidden.join("node_modules/x/requirements.txt"));
        touch(&hidden.join("a/b/c/pyproject.toml"));
        assert_eq!(project_root(&hidden), hidden);
        for d in [ws, empty, scripts, legacy, hidden] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    fn req(name: &str, spec: &str) -> Requirement {
        Requirement {
            name: name.to_string(),
            spec: spec.to_string(),
        }
    }
}
