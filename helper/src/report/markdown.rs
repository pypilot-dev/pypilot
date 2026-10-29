//! The Markdown reports: the editor's "Show details" document and the
//! `--markdown` output of `doctor` and `check`, so what users paste into an
//! issue is what they saw in the editor.

use std::path::Path;

use crate::core::project;
use crate::core::{solver, Assessment, FixKind, Severity};
use crate::pypi::MetadataSource;

use super::check::{self, CheckContext, CheckReport};

/// The report behind "Show details", and `pypilot doctor --markdown`.
pub fn details_markdown(a: &Assessment) -> String {
    let mut s = String::new();
    s.push_str("# PyPilot report\n\n");

    s.push_str("## Environment\n");
    s.push_str(&format!(
        "- uv: {}\n",
        a.probes
            .uv
            .as_ref()
            .map(|u| format!("v{}", u.version))
            .unwrap_or_else(|| "not detected".into())
    ));
    s.push_str(&format!(
        "- {}: {}\n",
        if a.project.is_pixi() {
            "pixi environment"
        } else {
            "virtualenv"
        },
        a.probes
            .venv
            .as_ref()
            .map(|v| v.path.display().to_string())
            .unwrap_or_else(|| "none".into())
    ));
    s.push_str(&format!(
        "- interpreters: {}\n\n",
        if a.probes.interpreters.is_empty() {
            "none".to_string()
        } else {
            a.probes
                .interpreters
                .iter()
                .map(|i| i.version.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        }
    ));

    if let Some(compat) = &a.compat {
        s.push_str("## Compatibility\n");
        s.push_str(&format!(
            "- Supported Python across dependencies: **{}**\n",
            compat.intersection.to_range_string()
        ));
        if let Some(text) = solver::recommendation(&a.project, compat, a.target_python) {
            s.push_str(&format!("- Recommended interpreter: **{text}**\n"));
        }
        s.push_str("\n| Package | Supported Python |\n|---|---|\n");
        for p in &compat.per_package {
            s.push_str(&format!(
                "| {} | {} |\n",
                p.name,
                p.supported.to_range_string()
            ));
        }
        s.push('\n');
    }

    s.push_str("## Findings\n");
    if a.findings.is_empty() {
        s.push_str("Everything looks good.\n");
    } else {
        for f in &a.findings {
            let sev = match f.severity {
                Severity::Error => "Error",
                Severity::Warning => "Warning",
                Severity::Info => "Note",
            };
            s.push_str(&format!("### {sev}: {}\n{}\n", f.title, f.detail));
            if let FixKind::RecreateWithPython(v) = &f.fix {
                s.push_str(&format!(
                    "\n_Fix: rebuild the environment on Python {v}._\n"
                ));
            } else if f.fix == FixKind::SetupEnvironment {
                s.push_str("\n_Fix: run \"Fix environment\", or `pypilot setup`._\n");
            } else if f.fix == FixKind::MigrateConda {
                s.push_str("\n_Fix: run `pypilot migrate-conda`._\n");
            } else if f.fix == FixKind::MigrateToPixi {
                s.push_str(
                    "\n_Fix: run \"Initialize pixi project\", or `pypilot migrate-to-pixi`._\n",
                );
            }
            s.push('\n');
        }
    }
    s
}

/// The `pypilot check <package>` answer as a report section, for the
/// editor's report: judged against the assessment's environment and its
/// dependencies.
pub async fn package_check_markdown<S: MetadataSource>(
    source: &S,
    workspace: &Path,
    a: &Assessment,
    package: &str,
) -> String {
    // Accept what a user would type into requirements.txt, e.g. `numpy>=2`,
    // and judge it on the release that specifier selects.
    let Some(req) = project::parse_requirement(package) else {
        return format!("\n## Check: {package}\n`{package}` is not a package name.\n");
    };
    let ctx = CheckContext {
        workspace,
        venv_python: a.probes.venv.as_ref().and_then(|v| v.python),
        pixi: a.project.is_pixi(),
        project: a
            .compat
            .as_ref()
            .map(|c| c.per_package.as_slice())
            .unwrap_or_default(),
    };
    match check::evaluate(source, &req, &ctx).await {
        Ok(report) => check_markdown(package, &report),
        Err(e) => format!("\n## Check: {package}\n{e}\n"),
    }
}

/// One check, as a report section headed `## Check: <package>`.
pub fn check_markdown(package: &str, r: &CheckReport) -> String {
    let mut s = format!("\n## Check: {package}\n");
    match &r.spec {
        Some(spec) => s.push_str(&format!(
            "- Release matching `{spec}`: **{}**\n",
            r.analysis.version
        )),
        None => s.push_str(&format!("- Latest release: **{}**\n", r.analysis.version)),
    }
    s.push_str(&format!("- Supported Python: **{}**\n", r.supported_range));
    if r.analysis.sdist_only {
        s.push_str("- No wheel for this platform, so it will compile from source\n");
    }
    s.push_str(&format!("\n{}\n", r.verdict.message));

    // Why it fails here, and the conda-forge build pixi could use instead.
    if !r.why.is_empty() {
        s.push('\n');
        for reason in &r.why {
            s.push_str(&format!("- Why: {reason}\n"));
        }
        if let Some(alt) = &r.alternative {
            s.push_str(&format!("- Alternative: {alt}\n"));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::project::Requirement;
    use crate::pypi::PyVersion;
    use crate::report::check::tests::{analysis, ctx, linux};

    fn assessment_with(findings: Vec<crate::core::Finding>) -> Assessment {
        Assessment {
            probes: Default::default(),
            project: Default::default(),
            compat: None,
            target_python: None,
            findings,
        }
    }

    #[test]
    fn pixi_findings_name_the_migration_in_the_report() {
        let report = details_markdown(&assessment_with(vec![crate::core::Finding {
            severity: Severity::Warning,
            title: "gdal 3.8.0 needs native libraries pip cannot install".into(),
            detail: String::new(),
            fix: FixKind::MigrateToPixi,
        }]));
        assert!(report.contains("`pypilot migrate-to-pixi`"));
        assert!(report.starts_with("# PyPilot report\n\n## Environment\n- uv: not detected\n"));
    }

    #[test]
    fn check_section_reads_like_the_editor_report() {
        let r = check::build(
            &Requirement {
                name: "tidy".into(),
                spec: "==1.0".into(),
            },
            analysis("tidy", 10..=12),
            linux(),
            false,
            &ctx(Some(PyVersion::py3(12)), &[]),
        );
        assert_eq!(
            check_markdown("tidy==1.0", &r),
            "\n## Check: tidy==1.0\n- Release matching `==1.0`: **1.0**\n- Supported Python: **3.10–3.12**\n\nThis project's Python 3.12 can run tidy.\n"
        );
    }
}
