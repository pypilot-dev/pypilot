//! Can the project's requirements be installed together, and if not, which
//! one edit fixes that?
//!
//! Wheel-tag analysis ([`crate::pypi::analyze`]) only sees which Pythons each
//! dependency supports. Two pins that exclude each other (`requests==2.31.0`
//! with `urllib3<1.21`), or a transitive clash (`boto3==1.26.0` needs
//! `urllib3<1.27`), pass that check and then fail `uv sync`, after setup has
//! already replaced the venv. So [`diagnose`] resolves the requirements as
//! written with `uv pip compile` for the target Python (cheap: no install, no
//! venv, uv's cache), reads uv's "No solution found" explanation for the
//! requirements that clash, and proposes the smallest change to one of them,
//! re-resolving to check it before offering it as a fix
//! ([`FixKind::EditRequirement`]).
//!
//! It also turns the generic "Could not resolve" finding into "did you mean"
//! or the nearest real releases, and a package that does not support this
//! operating system at all into an environment marker.
//!
//! [`blocking_step`] is the hook setup calls before it changes anything.

use std::path::{Path, PathBuf};

use crate::core::platform::Os;
use crate::core::project::{normalize_name, parse_requirement, ProjectDeps};
use crate::core::req_edit::{self, Declared, DeclaredReq};
use crate::core::setup::Step;
use crate::core::suggest;
use crate::core::{Assessment, Finding, FixKind, RequirementEdit, Severity};
use crate::pypi::metadata::SdistKind;
use crate::pypi::{CompatReport, MetadataSource, PyVersion};

/// Title of the finding for requirements that cannot resolve together.
pub const CONFLICT_TITLE: &str = "Dependencies cannot be installed together";

/// Resolves until the explanation names no more missing packages, then once
/// per candidate edit. Bounded so a pathological project costs seconds.
const MAX_MISSING_ROUNDS: usize = 3;
const MAX_VERIFY: usize = 5;

/// What a resolve said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// It resolved: `(normalized name, version)` for every package picked.
    Resolved(Vec<(String, String)>),
    /// "No solution found", with uv's explanation.
    NoSolution(String),
}

/// Something that resolves a requirements list: uv in practice, a table in
/// tests. `None` means it could not say (no network, a build failure).
#[allow(async_fn_in_trait)]
pub trait Resolver {
    async fn resolve(&self, lines: &[String]) -> Option<Outcome>;
}

/// [`Resolver`] that runs `uv pip compile` in the workspace (so uv's own
/// config, indexes included, applies).
pub struct UvResolver<'a> {
    pub uv: &'a Path,
    pub workspace: &'a Path,
    pub python: Option<PyVersion>,
}

impl Resolver for UvResolver<'_> {
    async fn resolve(&self, lines: &[String]) -> Option<Outcome> {
        use std::collections::HashMap;
        use std::sync::{Mutex, OnceLock};
        use std::time::{Duration, Instant};
        // The LSP assesses on every save; a resolve of the same input within
        // a few minutes gives the same answer.
        type Memo = HashMap<String, (Instant, Option<Outcome>)>;
        static MEMO: OnceLock<Mutex<Memo>> = OnceLock::new();
        const TTL: Duration = Duration::from_secs(600);
        let key = format!(
            "{}|{}|{:?}|{}",
            self.uv.display(),
            self.workspace.display(),
            self.python,
            lines.join("\n")
        );
        let memo = MEMO.get_or_init(Default::default);
        if let Some((at, hit)) = memo.lock().ok().and_then(|m| m.get(&key).cloned()) {
            if at.elapsed() < TTL {
                return hit;
            }
        }

        let dir = scratch_dir();
        std::fs::create_dir_all(&dir).ok()?;
        let file = dir.join("requirements.in");
        let mut text = lines.join("\n");
        text.push('\n');
        std::fs::write(&file, text).ok()?;
        let out = crate::core::uv::pip_compile(self.uv, &file, self.python, self.workspace).await;
        std::fs::remove_dir_all(&dir).ok();
        let outcome = match out {
            Ok(o) if o.success() => Some(Outcome::Resolved(parse_pins(&o.stdout))),
            Ok(o) if o.stderr.contains("No solution found") => {
                Some(Outcome::NoSolution(o.stderr.trim().to_string()))
            }
            _ => None,
        };
        if let Ok(mut m) = memo.lock() {
            m.insert(key, (Instant::now(), outcome.clone()));
        }
        outcome
    }
}

fn scratch_dir() -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    std::env::temp_dir().join(format!(
        "pypilot-resolve-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
}

/// `name==version` lines of `uv pip compile` output.
fn parse_pins(stdout: &str) -> Vec<(String, String)> {
    stdout
        .lines()
        .filter(|l| !l.starts_with(['#', ' ']))
        .filter_map(|l| l.split(';').next()?.trim().split_once("=="))
        .map(|(n, v)| (normalize_name(n), v.trim().to_string()))
        .collect()
}

// --- reading uv's explanation ------------------------------------------------

/// One "A depends on B and C" statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    /// The dependent as uv wrote it (`requests==2.31.0`); `None` for the
    /// project itself ("you require", "your project depends on").
    pub from: Option<String>,
    /// What it needs, as written (`urllib3>=1.21.1,<3`).
    pub to: Vec<String>,
}

/// What a "No solution found" explanation says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Explanation {
    pub edges: Vec<Edge>,
    /// Normalized names the index does not have at all.
    pub missing: Vec<String>,
    /// Normalized names with no release matching what is asked.
    pub no_version: Vec<String>,
    /// The explanation as one line, for the finding's detail.
    pub text: String,
}

struct Word<'a> {
    text: &'a str,
    /// Followed by `,` or `.`: the clause ends here.
    end: bool,
}

const NOT_REQUIREMENTS: &[&str] = &[
    "and",
    "because",
    "we",
    "you",
    "your",
    "there",
    "only",
    "the",
    "all",
    "no",
    "of",
    "is",
    "are",
    "can",
    "conclude",
    "that",
    "depends",
    "on",
    "requirements",
    "unsatisfiable",
];

/// `(normalized name, specifier)` when `word` reads as a requirement.
fn as_requirement(word: &str) -> Option<(String, String)> {
    if !word.starts_with(|c: char| c.is_ascii_alphanumeric())
        || NOT_REQUIREMENTS.contains(&word.to_ascii_lowercase().as_str())
    {
        return None;
    }
    let name_end = word
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '[' | ']')))
        .unwrap_or(word.len());
    let spec = &word[name_end..];
    if !(spec.is_empty() || spec.starts_with(['=', '<', '>', '!', '~'])) {
        return None;
    }
    let name = word[..name_end].split('[').next().unwrap_or("");
    Some((normalize_name(name), spec.to_string()))
}

/// Read uv's explanation. `root` is the project's name, which uv uses for
/// the project when it resolves a pyproject.toml.
pub fn parse_explanation(stderr: &str, root: Option<&str>) -> Explanation {
    let text: String = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("error:") && !l.is_empty())
        .map(|l| l.strip_prefix("cause:").unwrap_or(l).trim())
        .collect::<Vec<_>>()
        .join(" ");
    let words: Vec<Word> = text
        .split_whitespace()
        .map(|w| {
            let t = w.trim_end_matches([',', '.', ';', ':']);
            Word {
                text: t,
                end: t.len() != w.len(),
            }
        })
        .collect();
    let at = |i: usize| words.get(i).map_or("", |w| w.text);
    let root = root.map(normalize_name);
    let mut out = Explanation {
        text: text.clone(),
        ..Default::default()
    };

    for i in 0..words.len() {
        let start = if at(i) == "depends" && at(i + 1) == "on" && i > 0 {
            let subject = at(i - 1);
            let from = if subject == "project" && at(i.saturating_sub(2)) == "your" {
                None
            } else {
                match as_requirement(subject) {
                    Some((name, _)) if Some(&name) == root.as_ref() => None,
                    Some(_) => Some(subject.to_string()),
                    None => continue,
                }
            };
            Some((from, i + 2))
        } else if at(i) == "require" && i > 0 && at(i - 1) == "you" {
            Some((None, i + 1))
        } else {
            None
        };
        if let Some((from, mut j)) = start {
            let mut to = Vec::new();
            while let Some(w) = words.get(j) {
                if as_requirement(w.text).is_none() {
                    break;
                }
                to.push(w.text.to_string());
                let next_is_subject = matches!(at(j + 3), "depends" | "was" | "is");
                if w.end
                    || at(j + 1) != "and"
                    || as_requirement(at(j + 2)).is_none()
                    || next_is_subject
                {
                    break;
                }
                j += 2;
            }
            if !to.is_empty() {
                out.edges.push(Edge { from, to });
            }
        }

        if at(i) == "was" && at(i + 1) == "not" && at(i + 2) == "found" && i > 0 {
            if let Some((name, _)) = as_requirement(at(i - 1)) {
                out.missing.push(name);
            }
        }
        let no_version = (at(i) == "no" && at(i + 1) == "version" && at(i + 2) == "of")
            .then(|| at(i + 3))
            .or_else(|| {
                (at(i) == "only" && at(i + 2) == "is" && at(i + 3) == "available")
                    .then(|| at(i + 1))
            });
        if let Some((name, _)) = no_version.and_then(as_requirement) {
            out.no_version.push(name);
        }
    }
    out
}

// --- proposing an edit -------------------------------------------------------

/// A candidate change to one declared requirement.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    req: usize,
    new: String,
    /// Why this value: "requests==2.31.0 needs urllib3>=1.21.1,<3". `None`
    /// when the edit drops the specifier altogether.
    because: Option<String>,
}

/// How firmly a requirement is meant: an upper cap is the likeliest to be
/// an old workaround, an exact pin the most deliberate. Edits try the
/// loosest first.
fn firmness(spec: &str) -> u8 {
    if spec.starts_with("==") || spec.starts_with("===") {
        2
    } else if spec.starts_with('<') || spec.starts_with("!=") {
        0
    } else {
        1
    }
}

/// The edits worth trying, best first: replace a declared requirement's
/// specifier with what the other side needs, then drop a culprit's
/// specifier altogether.
fn candidates(expl: &Explanation, reqs: &[DeclaredReq]) -> Vec<Candidate> {
    let index_of = |name: &str| reqs.iter().position(|r| r.name == name);
    let mut out: Vec<Candidate> = Vec::new();

    for edge in &expl.edges {
        let from_direct = match &edge.from {
            None => true,
            Some(f) => as_requirement(f).is_some_and(|(n, _)| index_of(&n).is_some()),
        };
        if !from_direct {
            continue;
        }
        for to in &edge.to {
            let Some((name, spec)) = as_requirement(to) else {
                continue;
            };
            let Some(i) = index_of(&name) else {
                continue;
            };
            if spec.is_empty() || spec == reqs[i].spec() {
                continue;
            }
            let who = edge.from.clone().or_else(|| {
                expl.edges
                    .iter()
                    .find(|e| e.from.is_some() && e.to.contains(to))
                    .and_then(|e| e.from.clone())
            });
            let because = match who {
                Some(w) => format!("{w} needs {to}"),
                None => format!("the other requirements need {to}"),
            };
            out.push(Candidate {
                req: i,
                new: reqs[i].with_spec(&spec),
                because: Some(because),
            });
        }
    }

    // Then relax a culprit outright.
    for name in culprits(expl, reqs) {
        let i = index_of(&name).unwrap_or_default();
        let spec = reqs[i].spec();
        if spec.is_empty() {
            continue;
        }
        out.push(Candidate {
            req: i,
            new: reqs[i].with_spec(""),
            because: None,
        });
    }

    let mut seen = std::collections::HashSet::new();
    out.retain(|c| seen.insert((c.req, c.new.clone())));
    // Stable: within one firmness, the order uv explained things in.
    out.sort_by_key(|c| (u8::from(c.because.is_none()), firmness(&reqs[c.req].spec())));
    out
}

/// Declared requirements uv's explanation names, in order of appearance.
fn culprits(expl: &Explanation, reqs: &[DeclaredReq]) -> Vec<String> {
    let mut names = Vec::new();
    for edge in &expl.edges {
        let mut mentioned: Vec<&String> = edge.to.iter().collect();
        if let Some(f) = &edge.from {
            mentioned.insert(0, f);
        }
        for w in mentioned {
            if let Some((name, _)) = as_requirement(w) {
                if reqs.iter().any(|r| r.name == name) && !names.contains(&name) {
                    names.push(name);
                }
            }
        }
    }
    names
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn replaced(lines: &[String], old: &str, new: &str) -> Vec<String> {
    lines
        .iter()
        .map(|l| if l == old { new.to_string() } else { l.clone() })
        .collect()
}

/// The conflict finding for one "No solution found", with a verified edit
/// when one of the candidates resolves.
async fn conflict_finding<R: Resolver>(
    resolver: &R,
    expl: &Explanation,
    reqs: &[DeclaredReq],
    options: &[String],
    lines: &[String],
    python: Option<PyVersion>,
) -> Finding {
    let names = culprits(expl, reqs);
    let culprit_raws: Vec<String> = names
        .iter()
        .filter_map(|n| reqs.iter().find(|r| &r.name == n))
        .map(|r| format!("`{}`", r.raw))
        .collect();
    let title = if culprit_raws.is_empty() {
        CONFLICT_TITLE.to_string()
    } else {
        format!("{CONFLICT_TITLE}: {}", culprit_raws.join(" and "))
    };
    let for_python = python
        .map(|p| format!(" on Python {p}"))
        .unwrap_or_default();

    let mut verified: Vec<(Candidate, Vec<(String, String)>)> = Vec::new();
    for c in candidates(expl, reqs).into_iter().take(MAX_VERIFY) {
        // One working edit, plus at most one alternative on another requirement.
        if verified.iter().any(|(v, _)| v.req == c.req) {
            continue;
        }
        let mut input = options.to_vec();
        input.extend(replaced(lines, &reqs[c.req].raw, &c.new));
        if let Some(Outcome::Resolved(pins)) = resolver.resolve(&input).await {
            verified.push((c, pins));
            if verified.len() == 2 {
                break;
            }
        }
    }

    let mut detail =
        format!("uv finds no set of versions that satisfies every requirement{for_python}.");
    let fix = match verified.first() {
        Some((c, pins)) => {
            let r = &reqs[c.req];
            let picked = pins
                .iter()
                .find(|(n, _)| *n == r.name)
                .map(|(n, v)| format!(", installing {n} {v}"))
                .unwrap_or_default();
            let file = file_label(&r.file);
            if let Some(because) = &c.because {
                detail.push_str(&format!(" {because}, but {file} asks for `{}`.", r.raw));
            }
            detail.push_str(&format!(
                " Fix: change `{}` to `{}` in {file} (checked: uv then resolves every requirement{picked}).",
                r.raw, c.new,
            ));
            if let Some((alt, _)) = verified.get(1) {
                let a = &reqs[alt.req];
                detail.push_str(&format!(
                    " Or change `{}` to `{}` instead, which also resolves.",
                    a.raw, alt.new
                ));
            }
            FixKind::EditRequirement(RequirementEdit {
                file: r.file.clone(),
                old: r.raw.clone(),
                new: c.new.clone(),
            })
        }
        None => {
            if !culprit_raws.is_empty() {
                detail.push_str(&format!(" Relax one of {}.", culprit_raws.join(", ")));
            }
            FixKind::Manual
        }
    };
    detail.push_str(&format!(" uv's explanation: {}", expl.text));
    Finding {
        severity: Severity::Error,
        title,
        detail,
        fix,
    }
}

// --- unknown names and versions ----------------------------------------------

/// Title of the generic finding [`crate::core::solver::synthesize`] makes for
/// a dependency PyPI could not resolve; [`diagnose`] replaces it.
fn generic_unresolved_title(label: &str) -> String {
    format!("Could not resolve `{label}`")
}

fn missing_title(name: &str) -> String {
    format!("`{name}` is not on PyPI")
}

fn no_release_title(name: &str, spec: &str) -> String {
    format!("No release of {name} matches `{spec}`")
}

/// The finding for a declared requirement PyPI has no project or no
/// matching release for, with the edit that fixes it when there is a clear
/// one. `confirmed` when uv's own resolve failed on it too.
async fn unknown_finding<S: MetadataSource>(
    source: &S,
    req: &DeclaredReq,
    confirmed: bool,
) -> Finding {
    let severity = if confirmed {
        Severity::Error
    } else {
        Severity::Warning
    };
    let file = file_label(&req.file);
    let spec = req.spec();
    let edit = |new: String| {
        FixKind::EditRequirement(RequirementEdit {
            file: req.file.clone(),
            old: req.raw.clone(),
            new,
        })
    };
    if let Ok(meta) = source.fetch(&req.name).await {
        // The project exists, so the version is what is wrong.
        let near = suggest::nearest_versions(&spec, &meta.versions);
        let (detail, fix) = match near {
            Some(n) => {
                let new = req.with_spec(&n.spec);
                (
                    format!(
                        "{} has no release matching `{spec}` (asked for in {file}). Nearest releases: {}. Fix: change `{}` to `{new}`.",
                        meta.info.name,
                        n.nearby.join(", "),
                        req.raw
                    ),
                    edit(new),
                )
            }
            None => (
                format!(
                    "{} has no release matching `{spec}` (asked for in {file}); the newest is {}.",
                    meta.info.name, meta.info.version
                ),
                FixKind::Manual,
            ),
        };
        return Finding {
            severity,
            title: no_release_title(&meta.info.name, &spec),
            detail,
            fix,
        };
    }

    let guess = suggest::did_you_mean(&req.name, crate::matrix::import_map_pairs());
    let guess = match guess {
        Some(g) if source.fetch(&g).await.is_ok() => Some(g),
        _ => None,
    };
    let (detail, fix) = match guess {
        Some(g) => {
            let new = req.raw.replacen(req.head().split('[').next().unwrap_or(""), &g, 1);
            (
                format!(
                    "PyPI has no project named `{}` (listed in {file}). Did you mean `{g}`? Fix: change `{}` to `{new}`. If it comes from a private index, configure that index for uv.",
                    req.name, req.raw
                ),
                edit(new),
            )
        }
        None => (
            format!(
                "PyPI has no project named `{}` (listed in {file}). Check the spelling, or configure the private index it comes from.",
                req.name
            ),
            FixKind::Manual,
        ),
    };
    Finding {
        severity,
        title: missing_title(&req.name),
        detail,
        fix,
    }
}

// --- packages that do not support this OS ------------------------------------

fn os_name(os: Os) -> &'static str {
    match os {
        Os::Windows => "Windows",
        Os::Macos => "macOS",
        Os::Linux => "Linux",
    }
}

fn platform_title(name: &str, os: Os) -> String {
    format!("{name} does not support {}", os_name(os))
}

/// Declared requirements whose package does not support this OS, with the
/// marker edit. Replaces the "Will compile from source" / "cannot be
/// installed" finding for the same package.
async fn platform_findings<S: MetadataSource>(
    source: &S,
    compat: &CompatReport,
    reqs: &[DeclaredReq],
    findings: &mut Vec<Finding>,
) -> Vec<(usize, String)> {
    let os = compat.platform.os;
    let here = compat.platform.label();
    let mut edits = Vec::new();
    for p in &compat.per_package {
        let no_wheel_here = p.no_files_for_platform || p.sdist_kind() == Some(SdistKind::Native);
        if !no_wheel_here {
            continue;
        }
        let name = normalize_name(&p.name);
        let Some(i) = reqs.iter().position(|r| r.name == name) else {
            continue;
        };
        let marker = reqs[i].marker();
        if marker.contains("sys_platform") || marker.contains("platform_system") {
            continue;
        }
        let unsupported = suggest::unsupported_on(&name, os, &[])
            || suggest::unsupported_on(&name, os, &source.classifiers(&p.name).await);
        if !unsupported {
            continue;
        }
        let new = suggest::with_platform_marker(&reqs[i].raw, os);
        let prefix = format!("{} {} ", p.name, p.version);
        let blocked = format!("{} {} cannot be installed on {here}", p.name, p.version);
        findings.retain(|f| {
            !((f.title == "Will compile from source" && f.detail.starts_with(&prefix))
                || f.title == blocked)
        });
        findings.push(Finding {
            severity: Severity::Error,
            title: platform_title(&p.name, os),
            detail: format!(
                "{} {} publishes no wheel for {} and its metadata lists only other operating systems, so building it here fails whatever build tools are installed. Fix: change `{}` to `{new}` in {} so it installs only where it works (code importing it should fall back without it; for uvloop, plain asyncio).",
                p.name,
                p.version,
                os_name(os),
                reqs[i].raw,
                file_label(&reqs[i].file),
            ),
            fix: FixKind::EditRequirement(RequirementEdit {
                file: reqs[i].file.clone(),
                old: reqs[i].raw.clone(),
                new: new.clone(),
            }),
        });
        edits.push((i, new));
    }
    edits
}

// --- entry points ------------------------------------------------------------

/// Everything [`diagnose`] needs besides the sources.
pub struct Context<'a> {
    pub declared: &'a Declared,
    pub compat: Option<&'a CompatReport>,
    /// The project's name, which uv calls the project by.
    pub project_name: Option<String>,
    /// The Python to resolve for.
    pub python: Option<PyVersion>,
}

/// Add the resolution findings to `findings` (see the module docs).
/// `resolver` is `None` when uv is not available; the PyPI-based findings
/// are still made then, but nothing is confirmed.
pub async fn diagnose<S: MetadataSource, R: Resolver>(
    ctx: &Context<'_>,
    source: &S,
    resolver: Option<&R>,
    findings: &mut Vec<Finding>,
) {
    let reqs = &ctx.declared.reqs;
    if reqs.is_empty() {
        return;
    }
    let mut lines: Vec<String> = reqs.iter().map(|r| r.raw.clone()).collect();

    if let Some(compat) = ctx.compat {
        for (i, new) in platform_findings(source, compat, reqs, findings).await {
            lines[i] = new;
        }
    }

    // Names PyPI does not resolve: say what they probably should be.
    for (label, _) in ctx
        .compat
        .map(|c| c.unresolved.as_slice())
        .unwrap_or_default()
    {
        let Some(name) = parse_requirement(label).map(|r| normalize_name(&r.name)) else {
            continue;
        };
        if let Some(i) = reqs.iter().position(|r| r.name == name) {
            let finding = unknown_finding(source, &reqs[i], false).await;
            let generic = generic_unresolved_title(label);
            match findings.iter_mut().find(|f| f.title == generic) {
                Some(f) => *f = finding,
                None => findings.push(finding),
            }
        }
    }

    let Some(resolver) = resolver else {
        return;
    };
    if ctx.declared.custom_resolution {
        return;
    }
    let options = &ctx.declared.options;
    for _ in 0..MAX_MISSING_ROUNDS {
        let mut input = options.clone();
        input.extend(lines.iter().cloned());
        let Some(Outcome::NoSolution(stderr)) = resolver.resolve(&input).await else {
            return;
        };
        let expl = parse_explanation(&stderr, ctx.project_name.as_deref());
        let missing: Vec<usize> = expl
            .missing
            .iter()
            .chain(&expl.no_version)
            .filter_map(|n| reqs.iter().position(|r| &r.name == n))
            .filter(|i| lines.contains(&reqs[*i].raw))
            .collect();
        if missing.is_empty() {
            let f = conflict_finding(resolver, &expl, reqs, options, &lines, ctx.python).await;
            findings.push(f);
            return;
        }
        // uv agrees these do not exist: the warning becomes an error. Then
        // resolve the rest, which may clash on their own.
        for i in missing {
            let confirmed = unknown_finding(source, &reqs[i], true).await;
            let generic = generic_unresolved_title(&reqs[i].raw);
            let existing = findings
                .iter_mut()
                .find(|f| f.title == confirmed.title || f.title == generic);
            match existing {
                Some(f) => *f = confirmed,
                None => findings.push(confirmed),
            }
            let raw = reqs[i].raw.clone();
            lines.retain(|l| *l != raw);
        }
        if lines.is_empty() {
            return;
        }
    }
}

/// [`diagnose`] for a workspace, with uv when it is available. Skipped for
/// pixi and conda projects, whose packages pixi or conda resolve.
#[allow(clippy::too_many_arguments)]
pub async fn diagnose_workspace<S: MetadataSource>(
    workspace: &Path,
    project: &ProjectDeps,
    compat: Option<&CompatReport>,
    uv: Option<&Path>,
    python: Option<PyVersion>,
    source: &S,
    findings: &mut Vec<Finding>,
) {
    if project.is_pixi() || project.has_conda_env || project.packages.is_empty() {
        return;
    }
    let declared = req_edit::declared(workspace);
    let ctx = Context {
        declared: &declared,
        compat,
        project_name: project_name(workspace),
        python,
    };
    let resolver = uv.map(|uv| UvResolver {
        uv,
        workspace,
        python,
    });
    diagnose(&ctx, source, resolver.as_ref(), findings).await;
}

fn project_name(workspace: &Path) -> Option<String> {
    let text = std::fs::read_to_string(workspace.join("pyproject.toml")).ok()?;
    let doc = toml::from_str::<toml::Value>(&text).ok()?;
    Some(doc.get("project")?.get("name")?.as_str()?.to_string())
}

/// Does this finding mean installing the project will fail, so setup must
/// stop before it changes anything?
pub fn blocks_install(f: &Finding) -> bool {
    if f.severity != Severity::Error {
        return false;
    }
    let t = f.title.as_str();
    t.starts_with(CONFLICT_TITLE)
        || (t.starts_with('`') && t.ends_with("` is not on PyPI"))
        || t.starts_with("No release of ")
        || [Os::Windows, Os::Macos, Os::Linux]
            .iter()
            .any(|&os| t.ends_with(&format!(" does not support {}", os_name(os))))
}

/// The hook setup runs first: when the assessment found requirements that
/// cannot install, the failed step to stop on, before any venv is touched.
/// Its detail names every problem and its fix.
pub fn blocking_step(a: &Assessment) -> Option<Step> {
    blocking_step_in(&a.findings)
}

/// [`blocking_step`] over a list of findings.
pub fn blocking_step_in(findings: &[Finding]) -> Option<Step> {
    let blocking: Vec<&Finding> = findings.iter().filter(|f| blocks_install(f)).collect();
    if blocking.is_empty() {
        return None;
    }
    let detail = blocking
        .iter()
        .map(|f| format!("{}: {}", f.title, f.detail))
        .collect::<Vec<_>>()
        .join(" | ");
    Some(Step {
        name: "resolve dependencies".into(),
        ok: false,
        detail: format!(
            "{detail} Nothing was changed. Apply the fix (`pypilot fix requirements`), then set up again."
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::platform::Platform;
    use std::collections::HashMap;

    const S1A: &str = "error: No solution found when resolving dependencies\n  cause: Because requests==2.31.0 depends on urllib3>=1.21.1,<3 and you require requests==2.31.0, we can conclude that you require urllib3>=1.21.1,<3.\n         And because you require urllib3<1.21, we can conclude that your requirements are unsatisfiable.\n";
    const S1B_PYPROJECT: &str = "error: No solution found when resolving dependencies\n  cause: Because flask==3.0.0 depends on werkzeug>=3.0.0 and s1b depends on flask==3.0.0, we can conclude that s1b depends on werkzeug>=3.0.0.\n         And because s1b depends on werkzeug<2.0, we can conclude that your requirements are unsatisfiable.\n";
    const S9A: &str = "error: No solution found when resolving dependencies\n  cause: Because botocore>=1.29.0,<=1.29.165 depends on urllib3>=1.25.4,<1.27 and boto3==1.26.0 depends on botocore>=1.29.0,<1.30.0, we can conclude that boto3==1.26.0 depends on urllib3>=1.25.4,<1.27.\n         And because you require boto3==1.26.0 and urllib3>=2, we can conclude that your requirements are unsatisfiable.\n";
    const S9B: &str = "error: No solution found when resolving dependencies\n  cause: Because pydantic-settings==2.0.0 depends on pydantic>=2.0b3 and pydantic-settings>=2.0.1,<=2.0.3 depends on pydantic>=2.0.1, we can conclude that pydantic-settings>=2.0.0,<=2.0.3 depends on pydantic>=2.0b3.\n         And because pydantic-settings>=2.1.0,<=2.2.1 depends on pydantic>=2.3.0 and pydantic-settings>=2.3.0 depends on pydantic>=2.7.0, we can conclude that pydantic-settings>=2.0.0 depends on pydantic>=2.0b3.\n         And because you require pydantic<2 and pydantic-settings>=2, we can conclude that your requirements are unsatisfiable.\n";
    const S2A: &str = "error: No solution found when resolving dependencies\n  cause: Because reqests was not found in the package registry and you require reqests, we can conclude that your requirements are unsatisfiable.\n";
    const S2B: &str = "error: No solution found when resolving dependencies\n  cause: Because there is no version of requests==9.9.9 and you require requests==9.9.9, we can conclude that your requirements are unsatisfiable.\n";
    const ONLY: &str = "error: No solution found when resolving dependencies\n  cause: Because only requests<=2.34.2 is available and you require requests>=99, we can conclude that your requirements are unsatisfiable.\n";

    fn edge(from: Option<&str>, to: &[&str]) -> Edge {
        Edge {
            from: from.map(String::from),
            to: to.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn explanation_edges_are_read() {
        let e = parse_explanation(S1A, None);
        assert_eq!(
            e.edges,
            [
                edge(Some("requests==2.31.0"), &["urllib3>=1.21.1,<3"]),
                edge(None, &["requests==2.31.0"]),
                edge(None, &["urllib3>=1.21.1,<3"]),
                edge(None, &["urllib3<1.21"]),
            ]
        );
        assert!(e.text.starts_with("Because requests==2.31.0"));
        assert!(!e.text.contains("error:"));

        // A pyproject resolve names the project instead of "you".
        let e = parse_explanation(S1B_PYPROJECT, Some("s1b"));
        assert!(e.edges.contains(&edge(None, &["werkzeug<2.0"])));
        assert!(e
            .edges
            .contains(&edge(Some("flask==3.0.0"), &["werkzeug>=3.0.0"])));

        let e = parse_explanation(S9A, None);
        assert!(e
            .edges
            .contains(&edge(None, &["boto3==1.26.0", "urllib3>=2"])));
        assert!(e
            .edges
            .contains(&edge(Some("boto3==1.26.0"), &["urllib3>=1.25.4,<1.27"])));
        assert!(e.missing.is_empty() && e.no_version.is_empty());
    }

    #[test]
    fn missing_names_and_versions_are_read() {
        assert_eq!(parse_explanation(S2A, None).missing, ["reqests"]);
        assert_eq!(parse_explanation(S2B, None).no_version, ["requests"]);
        assert_eq!(parse_explanation(ONLY, None).no_version, ["requests"]);
    }

    fn reqs(file: &str, raws: &[&str]) -> Vec<DeclaredReq> {
        raws.iter()
            .map(|r| DeclaredReq {
                file: PathBuf::from(file),
                raw: r.to_string(),
                name: normalize_name(&crate::core::project::requirement_name(r).unwrap()),
            })
            .collect()
    }

    /// Resolves the inputs listed as working, fails the rest with `fail`.
    struct Table {
        ok: HashMap<String, Vec<(String, String)>>,
        fail: String,
    }

    impl Resolver for Table {
        async fn resolve(&self, lines: &[String]) -> Option<Outcome> {
            Some(match self.ok.get(&lines.join("|")) {
                Some(pins) => Outcome::Resolved(pins.clone()),
                None => Outcome::NoSolution(self.fail.clone()),
            })
        }
    }

    fn table(fail: &str, ok: &[(&str, &[(&str, &str)])]) -> Table {
        Table {
            ok: ok
                .iter()
                .map(|(k, pins)| {
                    (
                        k.to_string(),
                        pins.iter()
                            .map(|(n, v)| (n.to_string(), v.to_string()))
                            .collect(),
                    )
                })
                .collect(),
            fail: fail.to_string(),
        }
    }

    async fn run(declared: Declared, resolver: &Table, name: Option<&str>) -> Vec<Finding> {
        let ctx = Context {
            declared: &declared,
            compat: None,
            project_name: name.map(String::from),
            python: Some(PyVersion::py3(12)),
        };
        let mut findings = Vec::new();
        diagnose(
            &ctx,
            &crate::pypi::FixtureSource::new(),
            Some(resolver),
            &mut findings,
        )
        .await;
        findings
    }

    #[tokio::test]
    async fn unsatisfiable_pins_get_a_verified_relaxation() {
        let declared = Declared {
            reqs: reqs("pyproject.toml", &["requests==2.31.0", "urllib3<1.21"]),
            ..Default::default()
        };
        let resolver = table(
            S1A,
            &[(
                "requests==2.31.0|urllib3>=1.21.1,<3",
                &[("requests", "2.31.0"), ("urllib3", "2.8.0")],
            )],
        );
        let findings = run(declared, &resolver, None).await;
        let [f] = &findings[..] else {
            panic!("{findings:#?}")
        };
        assert_eq!(f.severity, Severity::Error);
        assert_eq!(
            f.title,
            "Dependencies cannot be installed together: `requests==2.31.0` and `urllib3<1.21`"
        );
        assert_eq!(
            f.fix,
            FixKind::EditRequirement(RequirementEdit {
                file: "pyproject.toml".into(),
                old: "urllib3<1.21".into(),
                new: "urllib3>=1.21.1,<3".into(),
            })
        );
        assert!(
            f.detail.contains(
                "requests==2.31.0 needs urllib3>=1.21.1,<3, but pyproject.toml asks for `urllib3<1.21`. Fix: change `urllib3<1.21` to `urllib3>=1.21.1,<3` in pyproject.toml"
            ),
            "{}",
            f.detail
        );
        assert!(
            f.detail.contains("installing urllib3 2.8.0"),
            "{}",
            f.detail
        );
        assert!(f.detail.contains("on Python 3.12"));
        assert!(!f.detail.contains("..."));
        assert!(blocks_install(f));
    }

    #[tokio::test]
    async fn transitive_conflict_keeps_the_exact_pin_and_offers_the_other_way() {
        let declared = Declared {
            reqs: reqs("requirements.txt", &["boto3==1.26.0", "urllib3>=2"]),
            ..Default::default()
        };
        let resolver = table(
            S9A,
            &[
                (
                    "boto3==1.26.0|urllib3>=1.25.4,<1.27",
                    &[("urllib3", "1.26.20")],
                ),
                ("boto3|urllib3>=2", &[("boto3", "1.40.0")]),
            ],
        );
        let findings = run(declared, &resolver, None).await;
        let f = &findings[0];
        let FixKind::EditRequirement(edit) = &f.fix else {
            panic!("{f:#?}")
        };
        assert_eq!(edit.old, "urllib3>=2");
        assert_eq!(edit.new, "urllib3>=1.25.4,<1.27");
        assert!(
            f.detail
                .contains("Or change `boto3==1.26.0` to `boto3` instead"),
            "{}",
            f.detail
        );
    }

    #[tokio::test]
    async fn upper_caps_are_relaxed_first() {
        let declared = Declared {
            reqs: reqs("pyproject.toml", &["pydantic<2", "pydantic-settings>=2"]),
            ..Default::default()
        };
        let resolver = table(S9B, &[("pydantic>=2.0b3|pydantic-settings>=2", &[])]);
        let f = &run(declared, &resolver, None).await[0];
        let FixKind::EditRequirement(edit) = &f.fix else {
            panic!("{f:#?}")
        };
        assert_eq!(
            (edit.old.as_str(), edit.new.as_str()),
            ("pydantic<2", "pydantic>=2.0b3")
        );
    }

    #[tokio::test]
    async fn no_verified_edit_means_a_manual_finding() {
        let declared = Declared {
            reqs: reqs("pyproject.toml", &["werkzeug<2.0", "flask==3.0.0"]),
            ..Default::default()
        };
        let resolver = table(S1B_PYPROJECT, &[]);
        let f = &run(declared, &resolver, Some("s1b")).await[0];
        assert_eq!(f.fix, FixKind::Manual);
        assert!(f.detail.contains("Relax one of"), "{}", f.detail);
        assert!(blocks_install(f));
    }

    #[tokio::test]
    async fn custom_resolution_and_success_are_silent() {
        let resolver = table(S1A, &[("a|b", &[])]);
        let declared = Declared {
            reqs: reqs("pyproject.toml", &["a", "b"]),
            ..Default::default()
        };
        assert!(run(declared.clone(), &resolver, None).await.is_empty());
        let custom = Declared {
            reqs: reqs("pyproject.toml", &["requests==2.31.0", "urllib3<1.21"]),
            custom_resolution: true,
            ..Default::default()
        };
        assert!(run(custom, &resolver, None).await.is_empty());
    }

    fn meta(name: &str, versions: &[&str]) -> crate::pypi::metadata::PackageMetadata {
        serde_json::from_value(serde_json::json!({
            "info": {"name": name, "version": versions.last().unwrap()},
            "urls": [],
            "releases": versions,
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn typo_and_missing_version_get_concrete_edits() {
        let mut source = crate::pypi::FixtureSource::new();
        source.insert(
            "requests",
            meta("requests", &["2.31.0", "2.32.3", "2.34.2"]),
        );
        let declared = Declared {
            reqs: reqs("pyproject.toml", &["reqests>=2", "requests==9.9.9"]),
            ..Default::default()
        };
        let ctx = Context {
            declared: &declared,
            compat: None,
            project_name: None,
            python: None,
        };
        // uv reports one at a time; both are confirmed, round by round.
        struct Seq;
        impl Resolver for Seq {
            async fn resolve(&self, lines: &[String]) -> Option<Outcome> {
                Some(if lines.iter().any(|l| l.starts_with("reqests")) {
                    Outcome::NoSolution(
                        S2A.replace("you require reqests", "you require reqests>=2"),
                    )
                } else {
                    Outcome::NoSolution(S2B.to_string())
                })
            }
        }
        let mut findings = Vec::new();
        diagnose(&ctx, &source, Some(&Seq), &mut findings).await;
        assert_eq!(findings.len(), 2, "{findings:#?}");
        let typo = &findings[0];
        assert_eq!(typo.title, "`reqests` is not on PyPI");
        assert_eq!(typo.severity, Severity::Error);
        assert!(typo.detail.contains("Did you mean `requests`?"));
        assert_eq!(
            typo.fix,
            FixKind::EditRequirement(RequirementEdit {
                file: "pyproject.toml".into(),
                old: "reqests>=2".into(),
                new: "requests>=2".into(),
            })
        );
        let version = &findings[1];
        assert_eq!(version.title, "No release of requests matches `==9.9.9`");
        assert!(version
            .detail
            .contains("Nearest releases: 2.31.0, 2.32.3, 2.34.2"));
        let FixKind::EditRequirement(edit) = &version.fix else {
            panic!()
        };
        assert_eq!(edit.new, "requests==2.34.2");
        assert!(findings.iter().all(blocks_install));
    }

    #[tokio::test]
    async fn unconfirmed_unknowns_replace_the_generic_warning() {
        let declared = Declared {
            reqs: reqs("pyproject.toml", &["reqests"]),
            ..Default::default()
        };
        let mut compat = crate::pypi::CompatReport {
            platform: Platform::current(),
            per_package: vec![],
            unresolved: vec![("reqests".into(), "`reqests` is not on PyPI".into())],
            unreachable: vec![],
            intersection: crate::pypi::PyVersionSet::universe(),
            conflicts: vec![],
            sdist_warnings: vec![],
        };
        let ctx = Context {
            declared: &declared,
            compat: Some(&compat),
            project_name: None,
            python: None,
        };
        let mut source = crate::pypi::FixtureSource::new();
        source.insert("requests", meta("requests", &["2.34.2"]));
        let mut findings = vec![Finding {
            severity: Severity::Warning,
            title: "Could not resolve `reqests`".into(),
            detail: "generic".into(),
            fix: FixKind::Manual,
        }];
        diagnose::<_, Table>(&ctx, &source, None, &mut findings).await;
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Warning);
        assert!(findings[0].detail.contains("Did you mean `requests`?"));
        assert!(
            !blocks_install(&findings[0]),
            "unconfirmed: a private index may have it"
        );
        compat.unresolved.clear();
    }

    #[tokio::test]
    async fn platform_only_package_gets_a_marker() {
        use crate::pypi::metadata::PackageAnalysis;
        let declared = Declared {
            reqs: reqs("pyproject.toml", &["uvloop>=0.19", "requests"]),
            ..Default::default()
        };
        let platform = Platform {
            os: Os::Windows,
            ..Platform::current()
        };
        let uvloop = PackageAnalysis {
            name: "uvloop".into(),
            version: "0.23.0".into(),
            supported: crate::pypi::PyVersionSet::universe(),
            has_platform_wheels: false,
            sdist_only: true,
            has_foreign_wheels: true,
            no_files_for_platform: false,
            wheel_platforms: Vec::new(),
            requires_python: None,
        };
        assert_eq!(uvloop.sdist_kind(), Some(SdistKind::Native));
        let compat = crate::pypi::CompatReport {
            platform,
            per_package: vec![uvloop],
            unresolved: vec![],
            unreachable: vec![],
            intersection: crate::pypi::PyVersionSet::universe(),
            conflicts: vec![],
            sdist_warnings: vec![],
        };
        let ctx = Context {
            declared: &declared,
            compat: Some(&compat),
            project_name: None,
            python: None,
        };
        let mut findings = vec![Finding {
            severity: Severity::Info,
            title: "Will compile from source".into(),
            detail:
                "uvloop 0.23.0 ships prebuilt wheels ... The build tools it needs are installed."
                    .into(),
            fix: FixKind::Manual,
        }];
        // The marker version is what gets resolved.
        let resolver = table(
            S1A,
            &[("uvloop>=0.19; sys_platform != 'win32'|requests", &[])],
        );
        diagnose(
            &ctx,
            &crate::pypi::FixtureSource::new(),
            Some(&resolver),
            &mut findings,
        )
        .await;
        let [f] = &findings[..] else {
            panic!("{findings:#?}")
        };
        assert_eq!(f.title, "uvloop does not support Windows");
        assert_eq!(
            f.fix,
            FixKind::EditRequirement(RequirementEdit {
                file: "pyproject.toml".into(),
                old: "uvloop>=0.19".into(),
                new: "uvloop>=0.19; sys_platform != 'win32'".into(),
            })
        );
        assert!(blocks_install(f));
    }

    #[tokio::test]
    async fn classifiers_decide_for_unlisted_packages() {
        use crate::pypi::metadata::PackageAnalysis;
        let declared = Declared {
            reqs: reqs("pyproject.toml", &["posixonly"]),
            ..Default::default()
        };
        let compat = crate::pypi::CompatReport {
            platform: Platform {
                os: Os::Windows,
                ..Platform::current()
            },
            per_package: vec![PackageAnalysis {
                name: "posixonly".into(),
                version: "1.0".into(),
                supported: crate::pypi::PyVersionSet::universe(),
                has_platform_wheels: false,
                sdist_only: true,
                has_foreign_wheels: true,
                no_files_for_platform: false,
                wheel_platforms: Vec::new(),
                requires_python: None,
            }],
            unresolved: vec![],
            unreachable: vec![],
            intersection: crate::pypi::PyVersionSet::universe(),
            conflicts: vec![],
            sdist_warnings: vec![],
        };
        let ctx = Context {
            declared: &declared,
            compat: Some(&compat),
            project_name: None,
            python: None,
        };
        let mut findings = Vec::new();
        let none: Option<&Table> = None;
        diagnose(
            &ctx,
            &crate::pypi::FixtureSource::new(),
            none,
            &mut findings,
        )
        .await;
        assert!(findings.is_empty(), "no classifiers, no claim");

        let mut source = crate::pypi::FixtureSource::new();
        source.set_classifiers("posixonly", &["Operating System :: POSIX :: Linux"]);
        diagnose(&ctx, &source, none, &mut findings).await;
        assert_eq!(findings[0].title, "posixonly does not support Windows");
    }

    #[test]
    fn setup_stops_on_blocking_findings_only() {
        let mut a = Assessment {
            probes: crate::core::probe::Probes::default(),
            project: ProjectDeps::default(),
            compat: None,
            target_python: None,
            findings: vec![Finding {
                severity: Severity::Warning,
                title: "No environment set up yet".into(),
                detail: String::new(),
                fix: FixKind::SetupEnvironment,
            }],
        };
        assert!(blocking_step(&a).is_none());
        a.findings.push(Finding {
            severity: Severity::Error,
            title: format!("{CONFLICT_TITLE}: `a` and `b`"),
            detail: "why.".into(),
            fix: FixKind::Manual,
        });
        let step = blocking_step(&a).unwrap();
        assert!(!step.ok);
        assert!(step.detail.contains("Nothing was changed"));
    }

    #[test]
    fn compile_output_pins_are_read() {
        let out = "certifi==2026.10.23\nrequests==2.34.2\n    # via x\nPyYAML==6.0.2 ; sys_platform == 'linux'\n";
        assert_eq!(
            parse_pins(out),
            [
                ("certifi".to_string(), "2026.10.23".to_string()),
                ("requests".into(), "2.34.2".into()),
                ("pyyaml".into(), "6.0.2".into()),
            ]
        );
    }
}
