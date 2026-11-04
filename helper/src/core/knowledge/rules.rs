//! The decision table: [`Context`] in, findings out. No I/O, so every rule is
//! testable on a hand-built context.

use std::collections::{BTreeMap, HashSet};

use crate::core::project::{normalize_name, Requirement};
use crate::core::{Finding, FixKind, Severity};
use crate::pypi::pyversion::PyVersion;
use crate::pypi::version::{PackageVersion, VersionSpec};

use super::table::{ApiRemoval, ModuleRemoval, PythonRemoval, Shadowing, StdlibRemoval, Table};
use super::{Constraint, Replacement};

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
    let upgrade_req = format!("{}>={}", rule.package, rule.fixed_in);
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
    let upgrades: Vec<String> = group
        .iter()
        .map(|h| format!("`{}>={}`", h.rule.package, h.rule.fixed_in))
        .collect();
    if first.forced {
        detail.push_str(&format!(
            "The project requires {dep}{}, which rules out the cap. Fix: upgrade to {}.",
            ctx.declared(dep).map(|d| d.spec.as_str()).unwrap_or(""),
            join_and(&upgrades.iter().map(String::as_str).collect::<Vec<_>>())
        ));
    } else if first.upgrade {
        detail.push_str(&format!(
            "Fix: upgrade to {} (or add `{}`, which holds {dep} back).",
            join_and(&upgrades.iter().map(String::as_str).collect::<Vec<_>>()),
            rule.constraint
        ));
    } else {
        detail.push_str(&format!(
            "Fix: add `{}` to the dependencies, or upgrade to {}.",
            rule.constraint,
            join_and(&upgrades.iter().map(String::as_str).collect::<Vec<_>>())
        ));
    }
    detail.push_str(&source_line(&rule.source));

    // Several packages needing an upgrade each cannot share one requirement.
    let fix = if first.upgrade && group.len() > 1 {
        FixKind::Manual
    } else {
        ctx.fix(FixKind::AddConstraint(Constraint {
            requirement: first.requirement.clone(),
        }))
    };
    Finding {
        severity: Severity::Warning,
        title,
        detail,
        fix,
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
        detail.push_str(&format!(
            " {} {fixed} is the first release that works there. Fix: upgrade to `{}>={fixed}`.",
            rule.package, rule.package
        ));
        ctx.fix(FixKind::AddConstraint(Constraint {
            requirement: format!("{}>={fixed}", rule.package),
        }))
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
    } else {
        let instead = rule
            .replacement
            .as_ref()
            .map(|r| format!(" Move the code to `{r}`, or"))
            .unwrap_or_else(|| " Either change the code, or".to_string());
        detail.push_str(&format!(
            " No release of {} fixes it.{instead} keep the project on Python {last} (PyPilot does not change the Python for this on its own).",
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
        fix: ctx.fix(FixKind::AddConstraint(Constraint {
            requirement: rule.constraint.clone(),
        })),
    })
}

// --- stdlib modules a Python removed ------------------------------------------

fn stdlib_finding(rule: &StdlibRemoval, ctx: &Context) -> Option<Finding> {
    let file = ctx.imports.get(&rule.module)?;
    let current = ctx.python?;
    let removed = py(&rule.removed_in)?;
    if current < removed || ctx.site_modules.contains(&rule.module) {
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
    let title = format!("`{module}` was removed in Python {removed}");
    let mut detail = format!(
        "{file} imports `{module}`, which Python {removed} removed from the standard library; Python {last} is the last that has it. {whose} Python {current}, so the import fails with \"ModuleNotFoundError: No module named '{module}'\". Fix: "
    );
    let fix = match &rule.replacement {
        Some(repl) => {
            detail.push_str(&format!("add `{repl}`, which provides `{module}` again"));
            if let Some(m) = &rule.migrate {
                detail.push_str(&format!(", or move the code to {m}"));
            }
            ctx.fix(FixKind::AddConstraint(Constraint {
                requirement: repl.clone(),
            }))
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
