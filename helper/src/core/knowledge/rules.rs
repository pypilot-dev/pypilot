//! The decision table: [`Context`] in, findings out. No I/O, so every rule is
//! testable on a hand-built context.

use std::collections::{BTreeMap, HashSet};

use crate::core::project::{normalize_name, Requirement};
use crate::core::{Finding, FixKind, Severity};
use crate::pypi::pyversion::PyVersion;
use crate::pypi::version::{PackageVersion, VersionSpec};

use super::table::{
    ApiRemoval, ModuleRemoval, PythonRemoval, Shadowing, StdlibRemoval, SymbolRemoval, Table,
};
use super::Replacement;
use crate::core::req_edit::DeclaredReq;
use crate::core::RequirementEdit;

/// What the rules look at, gathered by [`super::assess`].
#[derive(Debug, Clone, Default)]
pub struct Context {
    /// Declared dependencies, names normalized.
    pub declared: Vec<Requirement>,
    /// Declared names written with an environment marker
    /// (`dataclasses; python_version < "3.7"`), whose spec says nothing here.
    pub with_marker: HashSet<String>,
    /// The release the compatibility report resolved each declared
    /// dependency to, by normalized name.
    pub resolved: BTreeMap<String, String>,
    /// Installed distributions (normalized name to version), `None` with no venv.
    pub installed: Option<BTreeMap<String, String>>,
    /// Top-level names in site-packages (`pkg_resources`, `imghdr`).
    pub site_modules: HashSet<String>,
    /// The Python the venv runs, else the one setup would pick.
    pub python: Option<PyVersion>,
    /// Whether `python` is the venv's (true) or only a target (false).
    pub python_is_venv: bool,
    /// Top-level modules the project's own source imports, with the first
    /// file (relative path) that does.
    pub imports: BTreeMap<String, String>,
    /// Pairs `(package, dependency)` the installed metadata already rules
    /// out ([`crate::core::depcheck`] reports those), so they are not
    /// reported twice.
    pub declared_conflicts: HashSet<(String, String)>,
    /// A pixi project: findings stay, fixes are manual (pixi owns it).
    pub pixi: bool,
    /// The requirements exactly as the manifests write them, for edits.
    pub declared_raw: Vec<DeclaredReq>,
    /// Where an undeclared requirement is added (see
    /// [`super::new_requirement_file`]); `None` with no manifest.
    pub new_requirement_file: Option<std::path::PathBuf>,
    /// The project has a `uv.lock`, whose drift the health check reports.
    pub has_lock: bool,
    /// Where the source uses a symbol `symbol_removals` names.
    pub symbol_uses: Vec<CodeUse>,
    /// `requires-python` as declared, when it is.
    pub requires_python: Option<String>,
    /// The Python `.python-version` pins, when it names one.
    pub python_pin: Option<PyVersion>,
}

/// One use of a removed symbol in the project's source.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodeUse {
    pub module: String,
    pub symbol: String,
    /// Relative, `/`-separated.
    pub file: String,
    /// One-based.
    pub line: u32,
    /// The code as written: `np.float`.
    pub written: String,
    pub from_import: bool,
}

/// Another fix for a finding, tried when the first one does not resolve
/// (see [`super::verify_alternatives`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alternative {
    /// The finding it belongs to.
    pub title: String,
    /// The fix sentence in the finding's detail that it replaces.
    pub replaces: String,
    /// The sentence that says what this fix does instead.
    pub sentence: String,
    pub fix: FixKind,
}

impl Context {
    fn declared(&self, name: &str) -> Option<&Requirement> {
        let key = normalize_name(name);
        self.declared
            .iter()
            .find(|r| normalize_name(&r.name) == key)
    }

    fn installed_version(&self, name: &str) -> Option<&str> {
        self.installed
            .as_ref()?
            .get(&normalize_name(name))
            .map(String::as_str)
    }

    fn is_installed(&self, name: &str) -> bool {
        self.installed
            .as_ref()
            .is_some_and(|i| i.contains_key(&normalize_name(name)))
    }

    /// Declared without a marker: the spec applies here.
    fn declared_plain(&self, name: &str) -> Option<&Requirement> {
        self.declared(name)
            .filter(|r| !self.with_marker.contains(&normalize_name(&r.name)))
    }

    fn resolved_version(&self, name: &str) -> Option<&str> {
        self.resolved.get(&normalize_name(name)).map(String::as_str)
    }

    fn fix(&self, fix: FixKind) -> FixKind {
        if self.pixi {
            FixKind::Manual
        } else {
            fix
        }
    }

    /// The fix that makes the project require `requirement` (`numpy<2`):
    /// rewrite the declared requirement on that name, add it when there is
    /// none, or just sync when the manifest already says exactly that.
    pub fn constraint_fix(&self, requirement: &str) -> FixKind {
        let requirement = requirement.trim();
        let (name, spec) = split_requirement(requirement);
        if self.pixi || name.is_empty() {
            return FixKind::Manual;
        }
        let key = normalize_name(name);
        if let Some(d) = self.declared_raw.iter().find(|d| d.name == key) {
            let spec = keep_lower_bounds(&d.spec(), &spec);
            if d.spec() == spec {
                return FixKind::SyncEnvironment;
            }
            return FixKind::EditRequirement(RequirementEdit {
                file: d.file.clone(),
                old: d.raw.clone(),
                new: d.with_spec(&spec),
            });
        }
        match &self.new_requirement_file {
            Some(file) => FixKind::EditRequirement(RequirementEdit {
                file: file.clone(),
                old: String::new(),
                new: requirement.to_string(),
            }),
            None => FixKind::AddRequirements(vec![requirement.to_string()]),
        }
    }
}

/// `new` with the lower bounds of `old` that still fit under it, when `new`
/// only caps (`<1.24`): `>=1.20` and `<1.24` give `>=1.20,<1.24`, so a cap
/// does not throw away the floor the user wrote. Anything else is `new`.
fn keep_lower_bounds(old: &str, new: &str) -> String {
    let caps_only = !new.is_empty()
        && new
            .split(',')
            .all(|c| c.starts_with('<') || c.starts_with("!="));
    if !caps_only {
        return new.to_string();
    }
    let cap = VersionSpec::parse(new);
    let kept: Vec<&str> = old
        .split(',')
        .map(str::trim)
        .filter(|c| c.starts_with(">=") || (c.starts_with('>') && !c.starts_with(">=")))
        .filter(|c| {
            let v = c.trim_start_matches(['>', '=']);
            ver(v).is_some_and(|v| cap.admits(&v))
        })
        .collect();
    if kept.is_empty() {
        return new.to_string();
    }
    format!("{},{new}", kept.join(","))
}

/// `pandas>=2.2.2,<3`: the first fixed release, without moving to the next
/// major (a bigger change than the fix needs).
pub fn upgrade_requirement(package: &str, fixed_in: &str) -> String {
    match fixed_in
        .split('.')
        .next()
        .and_then(|m| m.parse::<u64>().ok())
    {
        Some(major) => format!("{package}>={fixed_in},<{}", major + 1),
        None => format!("{package}>={fixed_in}"),
    }
}

/// Does the project's `requires-python` allow `py` or anything newer? False
/// when it declares none: there is no promise to judge.
fn allows_from(ctx: &Context, py: PyVersion) -> bool {
    ctx.requires_python
        .as_deref()
        .map(crate::pypi::pyversion::parse_requires_python)
        .and_then(|set| set.max())
        .is_some_and(|max| max >= py)
}

/// Does the project let the environment run on `py`: `requires-python`
/// admits it and `.python-version` does not pin another Python?
fn may_run_on(ctx: &Context, py: PyVersion) -> bool {
    let rp_ok = ctx.requires_python.as_deref().map_or(true, |rp| {
        crate::pypi::pyversion::parse_requires_python(rp).contains(py)
    });
    rp_ok && ctx.python_pin.map_or(true, |p| p == py)
}

/// `("numpy", "<2")` for `numpy<2`, whitespace dropped from the spec.
fn split_requirement(requirement: &str) -> (&str, String) {
    let req = requirement.split(';').next().unwrap_or(requirement).trim();
    let end = req
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
        .unwrap_or(req.len());
    let spec = req[end..].chars().filter(|c| !c.is_whitespace()).collect();
    (&req[..end], spec)
}

fn ver(s: &str) -> Option<PackageVersion> {
    PackageVersion::parse(s)
}

/// `a < b`, false when either does not parse.
fn below(a: &str, b: &str) -> bool {
    matches!((ver(a), ver(b)), (Some(a), Some(b)) if a < b)
}

/// `a >= b`, false when either does not parse.
fn at_least(a: &str, b: &str) -> bool {
    matches!((ver(a), ver(b)), (Some(a), Some(b)) if a >= b)
}

fn py(s: &str) -> Option<PyVersion> {
    PyVersion::parse(s)
}

/// The highest lower bound a specifier sets (`>=`, `>`, `==`, `~=`, `===`),
/// read off the text. `None` when it sets none.
pub fn lower_bound(spec: &str) -> Option<PackageVersion> {
    spec.split(',')
        .filter_map(|clause| {
            let c = clause.trim();
            let rest = ["===", "==", ">=", "~=", ">"]
                .iter()
                .find_map(|op| c.strip_prefix(op))?;
            ver(rest.trim().trim_end_matches(".*"))
        })
        .max()
}

/// Does `spec` let an installer pick a release at or above `from`? True when
/// it admits `from` itself or something far above it.
fn admits_from(spec: &str, from: &str) -> bool {
    let spec_v = VersionSpec::parse(spec);
    if spec_v.is_unconstrained() {
        return true;
    }
    let probes = [from.to_string(), "999999".to_string()];
    probes
        .iter()
        .filter_map(|p| ver(p))
        .any(|p| spec_v.admits(&p))
        || lower_bound(spec).is_some_and(|lb| ver(from).is_some_and(|f| lb >= f))
}

/// Does `spec` rule out everything below `from` (`numpy>=2`)?
fn forces_from(spec: &str, from: &str) -> bool {
    matches!((lower_bound(spec), ver(from)), (Some(lb), Some(f)) if lb >= f)
}

fn python_in_range(ctx: &Context, min: Option<&str>, below_py: Option<&str>) -> bool {
    let Some(current) = ctx.python else {
        // Unknown Python: judge the row anyway, the common case is affected.
        return true;
    };
    if min.and_then(py).is_some_and(|m| current < m) {
        return false;
    }
    if below_py.and_then(py).is_some_and(|b| current >= b) {
        return false;
    }
    true
}

fn source_line(url: &str) -> String {
    format!(" Source: {url}")
}

/// Every finding the table produces for `ctx`.
pub fn evaluate(table: &Table, ctx: &Context) -> Vec<Finding> {
    let mut out = Vec::new();
    out.extend(api_findings(&table.api_removals, ctx));
    for rule in &table.python_removals {
        out.extend(python_finding(rule, ctx));
    }
    for rule in &table.shadowing {
        out.extend(shadow_finding(rule, ctx));
    }
    for rule in &table.module_removals {
        out.extend(module_finding(rule, ctx));
    }
    for rule in &table.stdlib_removals {
        out.extend(stdlib_finding(rule, ctx));
    }
    out.extend(symbol_findings(&table.symbol_removals, ctx));
    out.extend(drift_findings(ctx));
    out
}

/// The other fixes worth trying for [`evaluate`]'s findings, best first, for
/// a finding whose own fix may not resolve: a pinned package upgraded next
/// to other pins (flask 1.1.2 with jinja2 2.11.3) can be fixed by capping
/// the culprit instead (`itsdangerous<2.1`).
pub fn alternatives(table: &Table, ctx: &Context) -> Vec<Alternative> {
    let hits: Vec<ApiHit> = table
        .api_removals
        .iter()
        .filter_map(|r| api_hit(r, ctx))
        .filter(|h| h.upgrade)
        .collect();
    let mut out = Vec::new();
    for hit in &hits {
        let group: Vec<&ApiHit> = hits
            .iter()
            .filter(|h| h.requirement == hit.requirement)
            .collect();
        if group.len() > 1 || ctx.pixi {
            continue;
        }
        let finding = api_finding(&group, ctx);
        let rule = hit.rule;
        let fix = ctx.constraint_fix(&rule.constraint);
        let FixKind::EditRequirement(edit) = &fix else {
            continue;
        };
        let sentence = if edit.old.is_empty() {
            format!(
                "Upgrading to `{}` does not resolve with the project's other requirements (checked with uv), so the fix holds {} back instead: add `{}`.",
                hit.requirement, rule.dependency, edit.new
            )
        } else {
            format!(
                "Upgrading to `{}` does not resolve with the project's other requirements (checked with uv), so the fix holds {} back instead: change `{}` to `{}`.",
                hit.requirement, rule.dependency, edit.old, edit.new
            )
        };
        out.push(Alternative {
            title: finding.title.clone(),
            replaces: api_fix_sentence(&group, ctx),
            sentence,
            fix,
        });
    }
    out
}

/// Installed versions the manifest rules out, in a project with no
/// `uv.lock` (with one, the health check reports drift from the lock). This
/// is what is left after a constraint edit (`requests==2.32.2` while
/// 2.32.0 is installed): the sync that installs the new requirement.
fn drift_findings(ctx: &Context) -> Vec<Finding> {
    if ctx.has_lock || !ctx.python_is_venv {
        return Vec::new();
    }
    let mut out = Vec::new();
    for d in &ctx.declared_raw {
        let spec = d.spec();
        if spec.is_empty() || !d.marker().is_empty() {
            continue;
        }
        let Some(have) = ctx.installed_version(&d.name) else {
            continue;
        };
        let Some(v) = ver(have) else {
            continue;
        };
        if VersionSpec::parse(&spec).admits(&v) {
            continue;
        }
        let file = d
            .file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        out.push(Finding {
            severity: Severity::Warning,
            title: format!("{} {have} is installed, but {file} requires {}", d.name, d.raw),
            detail: format!(
                "The environment has {} {have}, which `{}` rules out. Fix: install the declared dependencies into the existing environment.",
                d.name, d.raw
            ),
            fix: ctx.fix(FixKind::SyncEnvironment),
        });
    }
    out
}

// --- API / ABI removals -------------------------------------------------------

/// One package that a rule says breaks.
struct ApiHit<'a> {
    rule: &'a ApiRemoval,
    /// `pandas 2.0.3`.
    package_label: String,
    /// The dependency version that breaks it, when installed.
    installed_dependency: Option<String>,
    /// The fix requirement: the constraint, or `package>=fixed_in`.
    requirement: String,
    /// Whether that requirement is the upgrade rather than the cap.
    upgrade: bool,
    /// The declared dependency spec rules out the cap.
    forced: bool,
}

fn api_hit<'a>(rule: &'a ApiRemoval, ctx: &Context) -> Option<ApiHit<'a>> {
    if !python_in_range(
        ctx,
        rule.python_min.as_deref(),
        rule.python_below.as_deref(),
    ) {
        return None;
    }
    let dep_decl = ctx.declared_plain(&rule.dependency);
    let forced = dep_decl.is_some_and(|d| forces_from(&d.spec, &rule.breaks_from));
    let upgrade_req = upgrade_requirement(&rule.package, &rule.fixed_in);
    let pick = |forced: bool| {
        if forced || rule.prefer.as_deref() == Some("upgrade") {
            (upgrade_req.clone(), true)
        } else {
            (rule.constraint.clone(), false)
        }
    };

    // What is installed: both sides on disk, the break is real now.
    if let (Some(pv), Some(dv)) = (
        ctx.installed_version(&rule.package),
        ctx.installed_version(&rule.dependency),
    ) {
        let key = (
            normalize_name(&rule.package),
            normalize_name(&rule.dependency),
        );
        if below(pv, &rule.fixed_in)
            && at_least(dv, &rule.breaks_from)
            && !ctx.declared_conflicts.contains(&key)
        {
            let (requirement, upgrade) = pick(forced);
            return Some(ApiHit {
                rule,
                package_label: format!("{} {pv}", rule.package),
                installed_dependency: Some(dv.to_string()),
                requirement,
                upgrade,
                forced,
            });
        }
        return None;
    }
    if ctx.is_installed(&rule.package) && ctx.python_is_venv {
        // Installed, dependency absent: nothing breaks yet.
        return None;
    }

    // What is declared: the release an installer would pick, and whether
    // anything keeps the dependency below the break.
    let decl = ctx.declared_plain(&rule.package)?;
    let picked = ctx
        .resolved_version(&rule.package)
        .map(str::to_string)
        .or_else(|| {
            decl.spec
                .strip_prefix("==")
                .filter(|v| !v.contains('*'))
                .map(str::to_string)
        })?;
    if !below(&picked, &rule.fixed_in) {
        return None;
    }
    if let Some(d) = dep_decl {
        if !admits_from(&d.spec, &rule.breaks_from) {
            return None;
        }
    }
    let (requirement, upgrade) = pick(forced);
    Some(ApiHit {
        rule,
        package_label: format!("{} {picked}", rule.package),
        installed_dependency: None,
        requirement,
        upgrade,
        forced,
    })
}

/// API findings, one per fix requirement, so pandas and matplotlib under
/// numpy 2 become one "add `numpy<2`".
fn api_findings(rules: &[ApiRemoval], ctx: &Context) -> Vec<Finding> {
    let hits: Vec<ApiHit> = rules.iter().filter_map(|r| api_hit(r, ctx)).collect();
    let mut groups: BTreeMap<String, Vec<&ApiHit>> = BTreeMap::new();
    for hit in &hits {
        groups.entry(hit.requirement.clone()).or_default().push(hit);
    }
    groups
        .into_values()
        .map(|group| api_finding(&group, ctx))
        .collect()
}

fn api_finding(group: &[&ApiHit], ctx: &Context) -> Finding {
    let first = group[0];
    let rule = first.rule;
    let labels: Vec<&str> = group.iter().map(|h| h.package_label.as_str()).collect();
    let joined = join_and(&labels);
    let dep = &rule.dependency;
    let major = rule.breaks_from.as_str();

    let title = match &first.installed_dependency {
        Some(dv) => format!("{joined} cannot run with the installed {dep} {dv}"),
        None => format!("{joined} will break with {dep} {major}"),
    };

    let mut detail = String::new();
    for h in group {
        let r = h.rule;
        let why = if r.kind == "abi" {
            format!(
                "{} was built against {dep} {} and older, and its metadata does not say so",
                h.package_label,
                previous_major(major)
            )
        } else {
            format!(
                "{} uses an API that {dep} {major} removed, and its metadata does not cap {dep}",
                h.package_label
            )
        };
        detail.push_str(&format!(
            "{why}; it fails with \"{}\". {} {} is the first release that works with {dep} {major}. ",
            r.symptom, r.package, r.fixed_in
        ));
    }
    match &first.installed_dependency {
        Some(dv) => detail.push_str(&format!("{dep} {dv} is installed. ")),
        None => detail.push_str(&format!(
            "Nothing in the project keeps {dep} below {major}, so an install picks {dep} {major} or newer. "
        )),
    }
    detail.push_str(&api_fix_sentence(group, ctx));
    detail.push_str(&source_line(&rule.source));

    // Several packages needing an upgrade each cannot share one requirement.
    let fix = if first.upgrade && group.len() > 1 {
        FixKind::Manual
    } else {
        ctx.constraint_fix(&first.requirement)
    };
    Finding {
        severity: Severity::Warning,
        title,
        detail,
        fix,
    }
}

/// The "Fix: ..." sentence of an API finding.
fn api_fix_sentence(group: &[&ApiHit], ctx: &Context) -> String {
    let first = group[0];
    let rule = first.rule;
    let dep = &rule.dependency;
    let upgrades: Vec<String> = group
        .iter()
        .map(|h| {
            format!(
                "`{}`",
                upgrade_requirement(&h.rule.package, &h.rule.fixed_in)
            )
        })
        .collect();
    let upgrades = join_and(&upgrades.iter().map(String::as_str).collect::<Vec<_>>());
    if first.forced {
        format!(
            "The project requires {dep}{}, which rules out the cap. Fix: upgrade to {upgrades}.",
            ctx.declared(dep).map(|d| d.spec.as_str()).unwrap_or("")
        )
    } else if first.upgrade {
        format!(
            "Fix: upgrade to {upgrades} (or add `{}`, which holds {dep} back).",
            rule.constraint
        )
    } else {
        format!(
            "Fix: add `{}` to the dependencies, or upgrade to {upgrades}.",
            rule.constraint
        )
    }
}

/// `1.x` for `2`, `2.0.x` for `2.1`: how to name the releases before a break.
fn previous_major(from: &str) -> String {
    let parts: Vec<u64> = from.split('.').filter_map(|p| p.parse().ok()).collect();
    match parts.as_slice() {
        [major] if *major > 0 => format!("{}.x", major - 1),
        [major, minor, ..] if *minor > 0 => format!("{major}.{}.x", minor - 1),
        _ => format!("releases before {from}"),
    }
}

fn join_and(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

// --- packages dead on a newer Python ------------------------------------------

fn python_finding(rule: &PythonRemoval, ctx: &Context) -> Option<Finding> {
    let current = ctx.python?;
    let min = py(&rule.python_min)?;
    if current < min {
        return None;
    }
    let version = match ctx.installed_version(&rule.package) {
        Some(v) => v.to_string(),
        None => {
            if ctx.is_installed(&rule.package) {
                return None;
            }
            let decl = ctx.declared_plain(&rule.package)?;
            ctx.resolved_version(&rule.package)
                .map(str::to_string)
                .or_else(|| decl.spec.strip_prefix("==").map(str::to_string))
                .unwrap_or_default()
        }
    };
    if let Some(fixed) = &rule.fixed_in {
        if version.is_empty() || !below(&version, fixed) {
            return None;
        }
    }
    let label = if version.is_empty() {
        rule.package.clone()
    } else {
        format!("{} {version}", rule.package)
    };
    let last = PyVersion::new(min.major, min.minor.saturating_sub(1));
    let title = format!("{label} does not work on Python {current}");
    let mut detail = format!(
        "{label} fails on Python {min} and newer with \"{}\". This project runs on Python {current}.",
        rule.symptom
    );
    let fix = if let Some(fixed) = &rule.fixed_in {
        let upgrade = upgrade_requirement(&rule.package, fixed);
        detail.push_str(&format!(
            " {} {fixed} is the first release that works there. Fix: upgrade to `{upgrade}`.",
            rule.package
        ));
        ctx.constraint_fix(&upgrade)
    } else if let (Some(repl), true) = (&rule.replacement, rule.drop_in) {
        detail.push_str(&format!(
            " No release fixes it. `{repl}` is a drop-in replacement with the same import. Fix: replace {} with {repl}.",
            rule.package
        ));
        ctx.fix(FixKind::ReplaceDist(Replacement {
            uninstall: installed_only(ctx, &[&rule.package]),
            install: vec![repl.clone()],
            drop: declared_only(ctx, &[&rule.package]),
        }))
    } else if ctx.python_is_venv && may_run_on(ctx, last) {
        // Moving to another package means changing every `import` of this
        // one; Python {last} keeps the code as it is.
        let other = rule
            .replacement
            .as_ref()
            .map(|r| {
                format!(
                    " Moving to `{r}` instead would mean changing the code that imports {}.",
                    rule.package
                )
            })
            .unwrap_or_default();
        detail.push_str(&format!(
            " No release of {} fixes it. Fix: rebuild the environment on Python {last}, the last it works on, {}; this is the smaller change, since the code stays as it is.{other}",
            rule.package,
            allowed_by(ctx)
        ));
        ctx.fix(FixKind::RecreateWithPython(last))
    } else {
        let instead = rule
            .replacement
            .as_ref()
            .map(|r| format!(" Move the code to `{r}`, or"))
            .unwrap_or_else(|| " Either change the code, or".to_string());
        let blocked = if ctx.python_pin.is_some_and(|p| p != last) {
            format!(
                " (.python-version pins Python {}, so PyPilot does not move it on its own)",
                ctx.python_pin.map(|p| p.to_string()).unwrap_or_default()
            )
        } else if ctx.requires_python.is_some() && !may_run_on(ctx, last) {
            format!(
                " (requires-python `{}` does not allow it, so PyPilot does not move it on its own)",
                ctx.requires_python.as_deref().unwrap_or_default()
            )
        } else {
            String::new()
        };
        detail.push_str(&format!(
            " No release of {} fixes it.{instead} keep the project on Python {last}{blocked}.",
            rule.package
        ));
        FixKind::Manual
    };
    detail.push_str(&source_line(&rule.source));
    Some(Finding {
        severity: Severity::Warning,
        title,
        detail,
        fix,
    })
}

/// What lets the project move to another Python, naming only the files that
/// exist: "which the project's requires-python allows" with no
/// `.python-version`, and so on.
fn allowed_by(ctx: &Context) -> String {
    match (ctx.requires_python.is_some(), ctx.python_pin.is_some()) {
        (true, true) => "which the project's requires-python and .python-version allow".into(),
        (true, false) => "which the project's requires-python allows".into(),
        (false, true) => "which the project's .python-version allows".into(),
        (false, false) => "which nothing in the project rules out".into(),
    }
}

fn installed_only(ctx: &Context, names: &[&str]) -> Vec<String> {
    names
        .iter()
        .filter(|n| ctx.is_installed(n))
        .map(|n| n.to_string())
        .collect()
}

fn declared_only(ctx: &Context, names: &[&str]) -> Vec<String> {
    names
        .iter()
        .filter(|n| ctx.declared(n).is_some())
        .map(|n| n.to_string())
        .collect()
}

// --- shadowing ----------------------------------------------------------------

fn present(ctx: &Context, name: &str) -> bool {
    ctx.is_installed(name) || ctx.declared_plain(name).is_some()
}

fn shadow_finding(rule: &Shadowing, ctx: &Context) -> Option<Finding> {
    let severity = match rule.severity.as_deref() {
        Some("info") => Severity::Info,
        Some("error") => Severity::Error,
        _ => Severity::Warning,
    };
    let module = &rule.module;
    let (title, mut detail, fix) = match rule.kind.as_str() {
        "clash" => {
            let other = rule.other.as_ref()?;
            if !(present(ctx, &rule.dist) && present(ctx, other)) {
                return None;
            }
            let keep = rule.keep.clone().unwrap_or_else(|| other.clone());
            let stray = if normalize_name(&keep) == normalize_name(&rule.dist) {
                other.clone()
            } else {
                rule.dist.clone()
            };
            let title = format!(
                "{} and {} both provide the `{module}` module",
                rule.dist, other
            );
            let detail = format!(
                "Both write into the same `{module}` directory, so whichever installed last has overwritten the other's files and the result is a mix: \"{}\". Keep one. Fix: remove {stray} from the dependencies, uninstall both, and reinstall {keep}.",
                rule.symptom
            );
            let fix = ctx.fix(FixKind::ReplaceDist(Replacement {
                uninstall: installed_only(ctx, &[&rule.dist, other]),
                install: vec![keep.clone()],
                drop: declared_only(ctx, &[&stray]),
            }));
            (title, detail, fix)
        }
        "backport" => {
            let since = py(rule.stdlib_since.as_deref()?)?;
            if ctx.python.is_some_and(|p| p < since) || !present(ctx, &rule.dist) {
                return None;
            }
            let title = format!(
                "{} is an obsolete backport that shadows the standard library `{module}`",
                rule.dist
            );
            let mut detail = format!(
                "Python {since} and newer ship `{module}` in the standard library. The backport in site-packages is imported instead whenever site-packages comes first on sys.path (PYTHONPATH, some test runners and freezers), and then fails with \"{}\". Fix: remove {} from the dependencies and uninstall it.",
                rule.symptom, rule.dist
            );
            if ctx.declared(&rule.dist).is_none() {
                detail.push_str(
                    " It is not declared, so another package pulls it in; if it comes back after a sync, that package is too old for this Python.",
                );
            }
            let fix = ctx.fix(FixKind::ReplaceDist(Replacement {
                uninstall: installed_only(ctx, &[&rule.dist]),
                install: Vec::new(),
                drop: declared_only(ctx, &[&rule.dist]),
            }));
            (title, detail, fix)
        }
        "deprecated" => {
            let keep = rule.keep.as_ref()?;
            if !present(ctx, &rule.dist) {
                return None;
            }
            let title = format!("{} is a dead name: install {keep} instead", rule.dist);
            let detail = format!(
                "The `{}` distribution is not the package people mean by `import {module}`: \"{}\". Fix: replace {} with {keep} in the dependencies and the environment.",
                rule.dist, rule.symptom, rule.dist
            );
            let fix = ctx.fix(FixKind::ReplaceDist(Replacement {
                uninstall: installed_only(ctx, &[&rule.dist]),
                install: vec![keep.clone()],
                drop: declared_only(ctx, &[&rule.dist]),
            }));
            (title, detail, fix)
        }
        _ => return None,
    };
    detail.push_str(&source_line(&rule.source));
    Some(Finding {
        severity,
        title,
        detail,
        fix,
    })
}

// --- modules a dependency stopped shipping ------------------------------------

fn module_finding(rule: &ModuleRemoval, ctx: &Context) -> Option<Finding> {
    let file = ctx.imports.get(&rule.module)?;
    let module = &rule.module;
    let dep = &rule.dependency;
    if ctx.site_modules.contains(module) {
        return None; // It is on disk, so the import works.
    }
    let decl = ctx.declared_plain(dep);
    let title;
    let mut detail;
    match ctx.installed_version(dep) {
        Some(v) => {
            title = format!("`import {module}` fails: {dep} {v} no longer ships it");
            detail = format!(
                "{file} imports `{module}`, which {dep} removed in {}; {dep} {v} is installed.",
                rule.breaks_from
            );
        }
        None if ctx.is_installed(dep) => {
            title = format!("`import {module}` fails: the installed {dep} does not ship it");
            detail = format!(
                "{file} imports `{module}`, which {dep} removed in {}.",
                rule.breaks_from
            );
        }
        None => {
            if decl.is_some_and(|d| !admits_from(&d.spec, &rule.breaks_from)) {
                return None; // Declared below the removal; setup installs it.
            }
            let note = if decl.is_some() {
                ""
            } else {
                ", and it is not installed"
            };
            title = format!("`import {module}` will fail: newer {dep} does not ship it");
            detail = format!(
                "{file} imports `{module}`, which {dep} removed in {}; nothing keeps {dep} below that{note}.",
                rule.breaks_from
            );
        }
    }
    detail.push_str(&format!(
        " It fails with \"{}\". Fix: add `{}` to the dependencies",
        rule.symptom, rule.constraint
    ));
    if let Some(m) = &rule.migrate {
        detail.push_str(&format!(", or move the code to {m}"));
    }
    detail.push('.');
    detail.push_str(&source_line(&rule.source));
    Some(Finding {
        severity: Severity::Warning,
        title,
        detail,
        fix: ctx.constraint_fix(&rule.constraint),
    })
}

// --- stdlib modules a Python removed ------------------------------------------

fn stdlib_finding(rule: &StdlibRemoval, ctx: &Context) -> Option<Finding> {
    let file = ctx.imports.get(&rule.module)?;
    let current = ctx.python?;
    let removed = py(&rule.removed_in)?;
    if ctx.site_modules.contains(&rule.module) {
        return None;
    }
    // Works on this Python, but requires-python lets the project move to
    // one without it (c12c: distutils on 3.11 with `>=3.9`). Only worth a
    // warning when there is a distribution that brings the module back.
    let ahead = current < removed;
    if ahead && !(allows_from(ctx, removed) && rule.replacement.is_some()) {
        return None;
    }
    if let Some(repl) = &rule.replacement {
        if ctx.is_installed(repl) || ctx.declared(repl).is_some() {
            return None;
        }
    }
    let module = &rule.module;
    let last = PyVersion::new(removed.major, removed.minor.saturating_sub(1));
    let whose = if ctx.python_is_venv {
        "The venv runs"
    } else {
        "The environment would run"
    };
    let (title, now) = if ahead {
        (
            format!("`{module}` fails on Python {removed} and newer, which the project allows"),
            format!(
                "{whose} Python {current}, where it still works, but requires-python `{}` allows Python {removed} and newer, where the import fails with \"ModuleNotFoundError: No module named '{module}'\".",
                ctx.requires_python.as_deref().unwrap_or_default()
            ),
        )
    } else {
        (
            format!("`{module}` was removed in Python {removed}"),
            format!(
                "{whose} Python {current}, so the import fails with \"ModuleNotFoundError: No module named '{module}'\"."
            ),
        )
    };
    let mut detail = format!(
        "{file} imports `{module}`, which Python {removed} removed from the standard library; Python {last} is the last that has it. {now} Fix: "
    );
    let fix = match &rule.replacement {
        Some(repl) => {
            detail.push_str(&format!("add `{repl}`, which provides `{module}` again"));
            if let Some(m) = &rule.migrate {
                detail.push_str(&format!(", or move the code to {m}"));
            }
            ctx.constraint_fix(repl)
        }
        None => {
            match &rule.migrate {
                Some(m) => detail.push_str(&format!("move the code to {m}")),
                None => detail.push_str("remove the dependency on it"),
            }
            FixKind::Manual
        }
    };
    detail.push_str(&format!(
        ", or keep the project on Python {last}. PyPilot does not change the Python for this on its own.{}",
        source_line(&rule.source)
    ));
    Some(Finding {
        severity: Severity::Warning,
        title,
        detail,
        fix,
    })
}

/// Modules a source file reaches that `table` knows as removed: used by the
/// LSP guardian hook for `pkg_resources`. Returns the requirement to install
/// instead of the bare distribution, when the installed one lacks it.
pub fn install_requirement_for(
    table: &Table,
    module: &str,
    installed: &BTreeMap<String, String>,
) -> Option<String> {
    let rule = table.module_removal(module)?;
    match installed.get(&normalize_name(&rule.dependency)) {
        Some(v) if !at_least(v, &rule.breaks_from) => None,
        _ => Some(rule.constraint.clone()),
    }
}

// --- module symbols a dependency or Python removed ----------------------------

/// How many uses a finding lists before "and N more".
const MAX_LISTED_USES: usize = 5;

/// Findings for removed symbols the source uses: one per distribution
/// release that removed some (`numpy 1.24`), one per Python release.
fn symbol_findings(rows: &[SymbolRemoval], ctx: &Context) -> Vec<Finding> {
    symbol_groups(rows, ctx)
        .into_iter()
        .filter_map(|(f, _)| f)
        .collect()
}

/// Each finding [`symbol_findings`] makes, next to the uses it is about.
type Hits<'a> = Vec<(&'a SymbolRemoval, &'a CodeUse)>;

fn symbol_groups<'a>(
    rows: &'a [SymbolRemoval],
    ctx: &'a Context,
) -> Vec<(Option<Finding>, Hits<'a>)> {
    let mut by_dist: BTreeMap<(String, String), Hits<'a>> = BTreeMap::new();
    let mut by_python: BTreeMap<String, Hits<'a>> = BTreeMap::new();
    for u in &ctx.symbol_uses {
        let Some(row) = rows
            .iter()
            .find(|r| r.module == u.module && r.symbol == u.symbol)
        else {
            continue;
        };
        match (&row.dependency, &row.breaks_from, &row.removed_in) {
            (Some(dep), Some(from), _) => by_dist
                .entry((normalize_name(dep), from.clone()))
                .or_default()
                .push((row, u)),
            (_, _, Some(py)) => by_python.entry(py.clone()).or_default().push((row, u)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    for hits in by_dist.into_values() {
        out.push((dist_symbol_finding(&hits, ctx), hits));
    }
    for hits in by_python.into_values() {
        out.push((python_symbol_finding(&hits, ctx), hits));
    }
    out
}

/// The error a use fails with.
fn symbol_symptom(row: &SymbolRemoval, u: &CodeUse) -> String {
    if let Some(s) = &row.symptom {
        return s.clone();
    }
    if u.from_import {
        format!(
            "ImportError: cannot import name '{}' from '{}'",
            row.symbol, row.module
        )
    } else {
        format!(
            "AttributeError: module '{}' has no attribute '{}'",
            row.module, row.symbol
        )
    }
}

/// "`np.float`" or "`np.float` and 2 more", for a title.
fn uses_label(hits: &[(&SymbolRemoval, &CodeUse)]) -> String {
    let mut written: Vec<&str> = Vec::new();
    for (_, u) in hits {
        if !written.contains(&u.written.as_str()) {
            written.push(&u.written);
        }
    }
    match written.len() {
        1 => format!("`{}`", written[0]),
        n => format!("`{}` and {} more", written[0], n - 1),
    }
}

/// "app.py:2 uses `np.float`: write `float` instead." per use, capped.
fn uses_detail(hits: &[(&SymbolRemoval, &CodeUse)]) -> String {
    let mut parts: Vec<String> = hits
        .iter()
        .take(MAX_LISTED_USES)
        .map(|(r, u)| {
            format!(
                "{}:{} uses `{}`: write {} instead.",
                u.file, u.line, u.written, r.replacement
            )
        })
        .collect();
    if hits.len() > MAX_LISTED_USES {
        parts.push(format!(
            "{} more use(s) like these.",
            hits.len() - MAX_LISTED_USES
        ));
    }
    parts.join(" ")
}

/// The fix sentence for code that uses removed symbols: "change app.py:1
/// to `from pydantic_settings import BaseSettings` (...)" for one use,
/// "change the code as above (app.py:2, app.py:5)" for several.
fn code_fix(hits: &[(&SymbolRemoval, &CodeUse)]) -> String {
    match hits {
        [(row, u)] => format!("change {}:{} to {}", u.file, u.line, row.replacement),
        _ => {
            let mut at: Vec<String> = Vec::new();
            for (_, u) in hits.iter().take(MAX_LISTED_USES) {
                let loc = format!("{}:{}", u.file, u.line);
                if !at.contains(&loc) {
                    at.push(loc);
                }
            }
            let more = if hits.len() > MAX_LISTED_USES {
                ", ..."
            } else {
                ""
            };
            format!("change the code as above ({}{more})", at.join(", "))
        }
    }
}

/// The code changes still needed: for each removed-symbol finding PyPilot
/// cannot fix itself (the project's pins rule out the old API, or Python
/// removed it), its title and where the code has to change. The planner
/// lists these after a fix (E s09), since the environment can be healthy
/// while the code still fails.
pub fn code_changes(table: &Table, ctx: &Context) -> Vec<String> {
    symbol_groups(&table.symbol_removals, ctx)
        .into_iter()
        .filter_map(|(f, hits)| {
            let f = f?;
            (f.severity != Severity::Info && f.fix == FixKind::Manual)
                .then(|| format!("{}: {}", f.title, code_fix(&hits)))
        })
        .collect()
}

fn dist_symbol_finding(hits: &[(&SymbolRemoval, &CodeUse)], ctx: &Context) -> Option<Finding> {
    let row = hits[0].0;
    let dep = row.dependency.as_deref()?;
    let from = row.breaks_from.as_deref()?;
    let constraint = row.constraint.as_deref()?;
    let decl = ctx.declared_plain(dep);
    let have = ctx
        .installed_version(dep)
        .map(str::to_string)
        .or_else(|| {
            if ctx.is_installed(dep) {
                return None;
            }
            ctx.resolved_version(dep).map(str::to_string)
        })
        .or_else(|| {
            decl.and_then(|d| d.spec.strip_prefix("=="))
                .filter(|v| !v.contains('*'))
                .map(str::to_string)
        });
    let now = match &have {
        Some(v) if below(v, from) => return None,
        Some(_) => true,
        None => {
            let d = decl?;
            if !admits_from(&d.spec, from) {
                return None;
            }
            false
        }
    };
    let label = uses_label(hits);
    let title = if now {
        format!("{label} fails: {dep} {from} removed it")
    } else {
        format!("{label} will fail: {dep} {from} removed it")
    };
    let mut detail = format!(
        "{dep} {from} removed APIs the code uses. {} ",
        uses_detail(hits)
    );
    match &have {
        Some(v) if ctx.installed_version(dep).is_some() => detail.push_str(&format!(
            "{dep} {v} is installed, so the code fails with \"{}\". ",
            symbol_symptom(row, hits[0].1)
        )),
        _ => detail.push_str(&format!(
            "Nothing in the project keeps {dep} below {from}, so the code fails with \"{}\". ",
            symbol_symptom(row, hits[0].1)
        )),
    }
    let forced = decl.is_some_and(|d| forces_from(&d.spec, from));
    let add = hits
        .iter()
        .find_map(|(r, _)| r.add.clone())
        .filter(|a| ctx.declared(a).is_none() && !ctx.is_installed(a));
    let fix = if forced {
        let spec = decl.map(|d| d.spec.as_str()).unwrap_or("");
        match &add {
            Some(a) => {
                detail.push_str(&format!(
                    "The project requires {dep}{spec}, which rules out `{constraint}`. Fix: {}; PyPilot adds `{a}`, which the new code needs, but does not edit source, so this stays reported until the code changes.",
                    code_fix(hits)
                ));
                ctx.constraint_fix(a)
            }
            None => {
                detail.push_str(&format!(
                    "The project requires {dep}{spec}, which rules out `{constraint}`, so the code has to change. Fix: {}. PyPilot does not edit source, so this stays reported until the code changes.",
                    code_fix(hits)
                ));
                FixKind::Manual
            }
        }
    } else {
        let and_add = add
            .as_ref()
            .map(|a| format!(" and add `{a}`"))
            .unwrap_or_default();
        detail.push_str(&format!(
            "Fix: add `{constraint}` to keep the old API until the code is changed, or change the code as above{and_add}."
        ));
        ctx.constraint_fix(constraint)
    };
    detail.push_str(&source_line(&row.source));
    Some(Finding {
        severity: Severity::Warning,
        title,
        detail,
        fix,
    })
}

fn python_symbol_finding(hits: &[(&SymbolRemoval, &CodeUse)], ctx: &Context) -> Option<Finding> {
    let row = hits[0].0;
    let removed = py(row.removed_in.as_deref()?)?;
    let current = ctx.python?;
    let ahead = current < removed;
    if ahead && !allows_from(ctx, removed) {
        return None;
    }
    let last = PyVersion::new(removed.major, removed.minor.saturating_sub(1));
    let label = uses_label(hits);
    let whose = if ctx.python_is_venv {
        "The venv runs"
    } else {
        "The environment would run"
    };
    let (severity, title, now) = if ahead {
        (
            Severity::Info,
            format!("{label} fails on Python {removed} and newer, which the project allows"),
            format!(
                "{whose} Python {current}, where it still works, but requires-python `{}` allows Python {removed} and newer, where it fails with \"{}\".",
                ctx.requires_python.as_deref().unwrap_or_default(),
                symbol_symptom(row, hits[0].1)
            ),
        )
    } else {
        (
            Severity::Warning,
            format!("{label} was removed in Python {removed}"),
            format!(
                "{whose} Python {current}, so it fails with \"{}\".",
                symbol_symptom(row, hits[0].1)
            ),
        )
    };
    let detail = format!(
        "Python {removed} removed these from the standard library; Python {last} is the last that has them. {now} {} Fix: {}. PyPilot does not move the project to an older Python for this.{}",
        uses_detail(hits),
        code_fix(hits),
        source_line(&row.source)
    );
    Some(Finding {
        severity,
        title,
        detail,
        fix: FixKind::Manual,
    })
}
