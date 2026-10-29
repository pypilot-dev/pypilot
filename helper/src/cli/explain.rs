//! `pypilot explain [<pkg>] [--json]`: the resolution table behind a verdict.
//!
//! `check` and `doctor` say which Python to use; this says why. With a
//! package it shows the release chosen, its `requires_python`, the wheels per
//! platform and which of the two stops the range ("requires_python has no
//! upper bound, but wheels stop at cp312"). Without one it walks the
//! project's dependencies one Python version at a time and names what blocks
//! each. Read-only; the data is what the resolver already computes
//! ([`crate::pypi::explain`]), so the answers match `check` and `doctor`.
//!
//! `--json` prints the same data as a stable document, for bug reports.

use std::path::Path;

use colored::Colorize;

use crate::core::platform::Platform;
use crate::core::project;
use crate::core::solver::{self, EnvSummary};
use crate::pypi::explain::{
    explain_package, explain_project, Constraint, Outcome, PackageExplanation, ProjectExplanation,
};
use crate::pypi::{self, MetadataSource};

pub async fn run<S: MetadataSource>(
    workspace: &Path,
    source: &S,
    package: Option<&str>,
    json: bool,
) -> crate::Result<()> {
    let platform = Platform::current();
    match package {
        Some(package) => {
            let e = package_explanation(source, package, &platform).await?;
            if json {
                println!("{}", to_json("package", &e)?);
            } else {
                print!("{}", render_package(&e));
            }
        }
        None => {
            let e = project_explanation(workspace, source, platform).await?;
            if json {
                println!("{}", to_json("project", &e)?);
            } else {
                print!("{}", render_project(workspace, &e));
            }
        }
    }
    Ok(())
}

/// Resolve `package` the way `check` does and explain the release it picks.
pub async fn package_explanation<S: MetadataSource>(
    source: &S,
    package: &str,
    platform: &Platform,
) -> crate::Result<PackageExplanation> {
    let Some(req) = project::parse_requirement(package) else {
        anyhow::bail!("`{package}` is not a package name or requirement");
    };
    let meta = pypi::resolve(source, &req).await?;
    // Already fetched (and cached) by `resolve`.
    let latest = source.fetch(&req.name).await?.info.version;
    Ok(explain_package(&req, &meta, &latest, platform))
}

/// Explain the workspace's dependencies through the solver's own report.
pub async fn project_explanation<S: MetadataSource>(
    workspace: &Path,
    source: &S,
    platform: Platform,
) -> crate::Result<ProjectExplanation> {
    let deps = project::scan(workspace);
    if deps.packages.is_empty() {
        anyhow::bail!(
            "no dependencies found in {}; name one: pypilot explain <package>",
            workspace.display()
        );
    }
    let compat = pypi::analyze(source, platform, &deps.packages).await;
    // `synthesize` is pure; with no environment it still picks the target
    // `doctor` would, pixi's python spec included.
    let (target, _) = solver::synthesize(&EnvSummary::default(), &deps, Some(&compat));
    let recommendation = solver::recommendation(&deps, &compat, target);
    Ok(explain_project(
        &compat,
        deps.declared_requires_python.as_deref(),
        target,
        recommendation,
    ))
}

/// The `--json` document. A struct rather than `json!` so the fields keep
/// their declared order (serde_json's map sorts keys).
#[derive(serde::Serialize)]
struct Document<'a, T> {
    pypilot: &'static str,
    kind: &'a str,
    explanation: &'a T,
}

fn to_json<T: serde::Serialize>(kind: &str, value: &T) -> crate::Result<String> {
    Ok(serde_json::to_string_pretty(&Document {
        pypilot: env!("CARGO_PKG_VERSION"),
        kind,
        explanation: value,
    })?)
}

fn rule(title: &str) -> String {
    let dashes = 56usize.saturating_sub(title.chars().count() + 4);
    format!("── {title} {}", "─".repeat(dashes))
        .bold()
        .cyan()
        .to_string()
}

fn range(list: &[String]) -> String {
    match (list.first(), list.last()) {
        (None, _) => "none".to_string(),
        (Some(a), Some(b)) if a == b => a.clone(),
        (Some(a), Some(b)) => {
            // Gaps are rare; list them rather than hide them.
            let contiguous = list.windows(2).all(|w| {
                let minor = |s: &str| s.split('.').nth(1).and_then(|m| m.parse::<u8>().ok());
                matches!((minor(&w[0]), minor(&w[1])), (Some(x), Some(y)) if y == x + 1)
            });
            if contiguous {
                format!("{a}–{b}")
            } else {
                list.join(", ")
            }
        }
        _ => unreachable!(),
    }
}

fn constraint_name(c: Constraint) -> &'static str {
    match c {
        Constraint::RequiresPython => "requires_python",
        Constraint::Wheels => "wheels",
        Constraint::Both => "requires_python and wheels",
        Constraint::NoFiles => "no files for this platform",
        Constraint::None => "nothing",
    }
}

/// The terminal table for one package.
pub fn render_package(e: &PackageExplanation) -> String {
    let mut out = String::new();
    let mut line = |s: String| {
        out.push_str(&s);
        out.push('\n');
    };

    line(format!(
        "{} {} ({})",
        "PyPilot explain:".bold().cyan(),
        e.name.bold(),
        format!("v{}", e.version).cyan()
    ));
    line(format!("  {:<16}: {}", "requirement", e.requirement.cyan()));
    line(format!(
        "  {:<16}: {}, {}",
        "release",
        e.version.bold(),
        e.selection
    ));
    let rp = match &e.requires_python {
        Some(rp) => format!(
            "{} (allows {})",
            rp.green(),
            range(&e.requires_python_allows)
        ),
        None => format!("not declared (allows {})", range(&e.requires_python_allows)),
    };
    line(format!("  {:<16}: {rp}", "requires-python"));
    line(format!(
        "  {:<16}: {}",
        "sdist",
        if e.sdist {
            "yes (pip can build from source)".to_string()
        } else {
            "no".yellow().to_string()
        }
    ));

    line(String::new());
    line(rule("Wheels by platform"));
    line(format!(
        "  {:<15} {:<11} {:<11} {}",
        "platform".dimmed(),
        "wheels".dimmed(),
        "supported".dimmed(),
        "tags".dimmed()
    ));
    for p in &e.platforms {
        let marker = if p.current { "▸" } else { " " };
        let label = format!("{:<15}", p.label);
        let wheels = format!("{:<11}", range(&p.wheel_pythons));
        let supported = format!("{:<11}", range(&p.supported));
        let tags = if p.tags.is_empty() && !p.supported.is_empty() {
            "(no wheel: sdist build)".yellow().to_string()
        } else if p.tags.is_empty() {
            "none".dimmed().to_string()
        } else {
            p.tags.join(" ")
        };
        let label = if p.current {
            label.bold().to_string()
        } else {
            label
        };
        let supported = if p.supported.is_empty() {
            supported.red().to_string()
        } else {
            supported.green().to_string()
        };
        line(format!("{marker} {label} {wheels} {supported} {tags}"));
    }

    line(String::new());
    line(rule(&format!("Python versions on {}", e.platform)));
    let header: String = e
        .pythons
        .iter()
        .map(|r| format!("{:<6}", r.python))
        .collect();
    line(format!("  {:<8}{}", "Python".dimmed(), header.trim_end()));
    let cells: String = e
        .pythons
        .iter()
        .map(|r| {
            let cell = format!("{:<6}", outcome_symbol(r.outcome));
            match r.outcome {
                Outcome::Wheel => cell.green().bold().to_string(),
                Outcome::SourceBuild => cell.yellow().to_string(),
                Outcome::RequiresPython => cell.red().to_string(),
                Outcome::NoFile => cell.dimmed().to_string(),
            }
        })
        .collect();
    line(format!("  {:<8}{}", "here".dimmed(), cells.trim_end()));
    line(format!(
        "  {}",
        "✓ wheel   S no wheel, sdist build   ✗ requires_python excludes   – nothing to install"
            .dimmed()
    ));

    line(String::new());
    line(rule("Binding constraint"));
    let supported = if e.supported.is_empty() {
        "none".red().bold().to_string()
    } else {
        e.supported_range.green().bold().to_string()
    };
    line(format!("  {:<16}: {supported}", "supported here"));
    for (name, b) in [("newest", &e.upper), ("oldest", &e.lower)] {
        if b.constraint == Constraint::NoFiles {
            continue;
        }
        let mut s = constraint_name(b.constraint).bold().to_string();
        if !b.detail.is_empty() {
            s.push_str(&format!(" ({})", b.detail));
        }
        line(format!("  {:<16}: {s}", format!("{name} set by")));
    }
    // The bound lines already say it when wheels exist here; otherwise the
    // summary is the explanation (no files, sdist only).
    let wheels_here = e.platforms.iter().any(|p| p.current && !p.tags.is_empty());
    if !wheels_here {
        line(format!("  {}", e.summary));
    }
    out
}

fn outcome_symbol(o: Outcome) -> &'static str {
    match o {
        Outcome::Wheel => "✓",
        Outcome::SourceBuild => "S",
        Outcome::RequiresPython => "✗",
        Outcome::NoFile => "–",
    }
}

/// The terminal table for a project.
pub fn render_project(workspace: &Path, e: &ProjectExplanation) -> String {
    let mut out = String::new();
    let mut line = |s: String| {
        out.push_str(&s);
        out.push('\n');
    };

    line(format!(
        "{} {}",
        "PyPilot explain:".bold().cyan(),
        workspace.display()
    ));
    line(format!("  {:<16}: {}", "platform", e.platform));
    if let Some(rp) = &e.requires_python {
        line(format!("  {:<16}: {}", "requires-python", rp.green()));
    }
    let supported = if e.supported.is_empty() {
        "none".red().bold().to_string()
    } else {
        e.supported_range.green().bold().to_string()
    };
    line(format!("  {:<16}: {supported}", "supported"));
    if let Some(r) = &e.recommendation {
        line(format!("  {:<16}: {}", "recommended", r.cyan().bold()));
    }

    line(String::new());
    line(rule("Dependencies"));
    let width = e
        .packages
        .iter()
        .map(|p| p.name.len())
        .max()
        .unwrap_or(0)
        .max(7);
    line(format!(
        "  {:<width$}  {:<12} {:<16} {}",
        "package".dimmed(),
        "version".dimmed(),
        "requires-python".dimmed(),
        "supported".dimmed()
    ));
    for p in &e.packages {
        let supported = format!("{} {}", range(&p.supported), p.note.yellow());
        line(format!(
            "  {:<width$}  {:<12} {:<16} {}",
            p.name.bold(),
            p.version,
            p.requires_python.as_deref().unwrap_or("not declared"),
            supported.trim_end()
        ));
    }
    for u in &e.unresolved {
        line(format!(
            "  {:<width$}  {}",
            u.requirement.bold(),
            format!("could not resolve: {}", u.error).red()
        ));
    }

    line(String::new());
    line(rule("Python versions"));
    for row in &e.pythons {
        let py = format!("{:<6}", row.python);
        if row.ok {
            line(format!("  {} {}", py.green().bold(), "✓".green().bold()));
            continue;
        }
        let who = row
            .blockers
            .iter()
            .map(|b| format!("{} ({})", b.package.bold(), b.detail))
            .collect::<Vec<_>>()
            .join(", ");
        line(format!("  {} {} blocked by {who}", py.dimmed(), "✗".red()));
    }

    line(String::new());
    line(rule("Binding constraint"));
    match (&e.recommended, &e.binding_python) {
        (Some(t), Some(next)) if !e.binding.is_empty() => {
            let who = e
                .binding
                .iter()
                .map(|b| format!("{} {} ({})", b.package.bold(), b.version, b.detail))
                .collect::<Vec<_>>()
                .join(", ");
            line(format!(
                "  Python {} is the newest that works: {next} is blocked by {who}.",
                t.cyan().bold()
            ));
        }
        (Some(t), Some(next)) => line(format!(
            "  Python {} is recommended; {next} is not blocked by any dependency (the project's own Python spec narrows it).",
            t.cyan().bold()
        )),
        (Some(t), None) => line(format!(
            "  Python {} is the newest Python PyPilot tracks; nothing caps it.",
            t.cyan().bold()
        )),
        (None, _) if !e.conflicts.is_empty() => {
            for c in &e.conflicts {
                line(format!("  {} {c}", "[ERROR]".red().bold()));
            }
        }
        (None, _) => line(format!(
            "  {} No Python version satisfies every dependency on {}.",
            "[ERROR]".red().bold(),
            e.platform
        )),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::pixi::strip_ansi;
    use crate::core::platform::{Arch, Os};
    use crate::pypi::FixtureSource;

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    fn source() -> FixtureSource {
        let mut src = FixtureSource::new();
        src.load_json("mediapipe", fixture("mediapipe-0.10.35.json"))
            .unwrap();
        src.load_json("mediapipe==0.10.14", fixture("mediapipe-0.10.14.json"))
            .unwrap();
        src.load_json("numpy", fixture("numpy.json")).unwrap();
        src.load_json("tensorflow", fixture("tensorflow-2.15.0.json"))
            .unwrap();
        src
    }

    const LINUX: Platform = Platform {
        os: Os::Linux,
        arch: Arch::X86_64,
    };

    #[tokio::test]
    async fn pinned_mediapipe_names_the_wheels_as_the_limit() {
        let e = package_explanation(&source(), "mediapipe==0.10.14", &LINUX)
            .await
            .unwrap();
        assert_eq!(e.version, "0.10.14");
        assert_eq!(e.latest, "0.10.35");
        assert!(e.selection.contains("latest is 0.10.35"));
        assert_eq!(e.binding, Constraint::Wheels);
        let text = strip_ansi(&render_package(&e));
        assert!(text.contains("wheels stop at cp312, so 3.13+ has no wheel"));
        assert!(text.contains("▸ Linux x64"));
        assert!(text.contains("cp39 cp310 cp311 cp312"));
    }

    #[tokio::test]
    async fn json_is_a_stable_document() {
        let e = package_explanation(&source(), "tensorflow==2.15.0", &LINUX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&to_json("package", &e).unwrap()).unwrap();
        assert_eq!(v["kind"], "package");
        assert_eq!(v["explanation"]["binding"], "wheels");
        assert_eq!(v["explanation"]["supported_range"], "3.9–3.11");
        assert_eq!(v["explanation"]["platforms"][4]["id"], "windows-x64");
        assert_eq!(
            v["explanation"]["pythons"][6]["outcome"], "no_file",
            "3.13: no wheel and no sdist"
        );
    }

    #[tokio::test]
    async fn project_names_blockers_per_python() {
        let dir = std::env::temp_dir().join(format!("pypilot-explain-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("requirements.txt"),
            "mediapipe==0.10.14\ntensorflow\n",
        )
        .unwrap();
        let e = project_explanation(&dir, &source(), LINUX).await.unwrap();
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(e.recommended.as_deref(), Some("3.11"));
        assert_eq!(e.binding_python.as_deref(), Some("3.12"));
        let names: Vec<&str> = e.binding.iter().map(|b| b.package.as_str()).collect();
        assert_eq!(names, ["tensorflow"]);
        let row = e.pythons.iter().find(|r| r.python == "3.13").unwrap();
        assert_eq!(row.blockers.len(), 2);
        let text = strip_ansi(&render_project(&dir, &e));
        assert!(text.contains("3.12 is blocked by tensorflow 2.15.0 (wheels end at cp311)"));
        assert!(text.contains("mediapipe (wheels end at cp312)"));
    }

    /// numpy 1.26.4 has an sdist: Pythons past its wheels are a source
    /// build, not "nothing to install", and the PyPy wheels are not counted.
    #[tokio::test]
    async fn capped_numpy_falls_back_to_the_sdist() {
        let mut src = FixtureSource::new();
        src.load_json("numpy", fixture("numpy-1.26.4.json"))
            .unwrap();
        let e = package_explanation(&src, "numpy<2", &LINUX).await.unwrap();
        assert_eq!(e.version, "1.26.4");
        assert!(e.sdist);
        assert!(e.upper.detail.contains("pip would try to build the sdist"));
        let py313 = e.pythons.iter().find(|r| r.python == "3.13").unwrap();
        assert_eq!(py313.outcome, Outcome::SourceBuild);
        assert!(!py313.supported);
        assert!(e.platforms[0].tags.iter().all(|t| t.starts_with("cp")));
        let arm = e
            .platforms
            .iter()
            .find(|p| p.id == "windows-arm64")
            .unwrap();
        assert!(arm.tags.is_empty());
        assert!(!arm.supported.is_empty(), "sdist-only there");
        let text = strip_ansi(&render_package(&e));
        assert!(text.contains("no wheel: sdist build"));
    }

    #[test]
    fn range_collapses_runs_and_keeps_gaps() {
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(range(&v(&["3.9", "3.10", "3.11"])), "3.9–3.11");
        assert_eq!(range(&v(&["3.9", "3.12"])), "3.9, 3.12");
        assert_eq!(range(&[]), "none");
    }
}
