//! The answer to `pypilot check <package>`, as data.
//!
//! Built once ([`evaluate`]) and rendered three ways: the terminal text
//! (`cli/check.rs`), `--json` (this struct, serialized) and the Markdown
//! section of the editor's report ([`super::markdown::check_markdown`]).

use std::path::Path;

use serde::Serialize;

use crate::core::platform::Platform;
use crate::core::project::Requirement;
use crate::core::{env, pixi_advice};
use crate::pypi::{self, ConflictPair, MetadataSource, PackageAnalysis, PyVersion};

/// What the verdict rests on, besides the package itself.
pub struct CheckContext<'a> {
    pub workspace: &'a Path,
    /// The project environment's Python, when there is one.
    pub venv_python: Option<PyVersion>,
    /// A pixi project: it already installs from conda-forge, so no
    /// alternative is suggested.
    pub pixi: bool,
    /// The project's dependencies, analysed, for the conflict check. Empty
    /// when it declares none.
    pub project: &'a [PackageAnalysis],
}

/// Which verdict `check` reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictKind {
    /// The release publishes nothing installable on this platform.
    NoFiles,
    /// No Python runs both it and the project's other dependencies.
    Conflict,
    /// The environment's Python runs it.
    Supported,
    /// The environment's Python does not.
    Unsupported,
    /// No environment yet, so there is no Python to judge.
    NoEnvironment,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Verdict {
    pub kind: VerdictKind,
    /// `"ok"`, `"error"` or `"info"`.
    pub status: &'static str,
    /// The sentence the terminal and the editor show.
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckReport {
    /// The requirement as given: `mediapipe==0.10.14`.
    pub requirement: String,
    /// The release analysed: the newest one matching the requirement.
    pub analysis: PackageAnalysis,
    /// `analysis.supported` as text: `3.9–3.12`.
    pub supported_range: String,
    /// The platform judged: `Windows x64`.
    pub platform: &'static str,
    /// Installed in the project environment already.
    pub installed: bool,
    /// The project environment's Python, `null` without one.
    pub environment_python: Option<PyVersion>,
    pub pixi_project: bool,
    pub verdict: Verdict,
    /// The two packages that share no Python, when the verdict is
    /// `conflict` (also set alongside `unsupported`).
    pub conflict: Option<ConflictPair>,
    /// Why it fails here: no wheel for cpXY, what a source build needs.
    pub why: Vec<String>,
    /// The conda-forge build pixi could install instead; `null` in a pixi
    /// project or when none is known.
    pub alternative: Option<String>,
    /// The specifier, when the requirement had one (`==0.10.14`).
    #[serde(skip)]
    pub spec: Option<String>,
    /// The message when nothing installs on this platform.
    #[serde(skip)]
    pub no_files: Option<String>,
}

impl CheckReport {
    /// The verdict is a failure: nothing installs, a conflict, a source
    /// build, or the environment's Python is unsupported.
    pub fn failed(&self) -> bool {
        self.no_files.is_some()
            || self.conflict.is_some()
            || self.analysis.sdist_only
            || self.verdict.kind == VerdictKind::Unsupported
    }
}

/// Resolve `req` the way an installer would and judge it against `ctx`.
pub async fn evaluate<S: MetadataSource>(
    source: &S,
    req: &Requirement,
    ctx: &CheckContext<'_>,
) -> crate::Result<CheckReport> {
    let meta = pypi::resolve(source, req).await?;
    let platform = Platform::current();
    let analysis = meta.analyze(&platform);
    let project_env = env::project_env(ctx.workspace);
    let installed = env::importable(ctx.workspace, project_env.as_ref()).contains(&analysis.name);
    Ok(build(req, analysis, platform, installed, ctx))
}

/// [`evaluate`] after the lookup: pure, so tests can feed it fixtures.
pub fn build(
    req: &Requirement,
    analysis: PackageAnalysis,
    platform: Platform,
    installed: bool,
    ctx: &CheckContext<'_>,
) -> CheckReport {
    let here = platform.label();
    let no_files = analysis.no_files_message(here);
    let conflict = pypi::conflict_with(&analysis, ctx.project);
    let verdict = verdict(&analysis, no_files.as_deref(), conflict.as_ref(), ctx);

    let machine = pixi_advice::Machine {
        platform,
        venv_python: ctx.venv_python,
        python: ctx.venv_python.or_else(|| analysis.supported.max()),
        nvcc: pixi_advice::nvcc_available(),
    };
    let advice = pixi_advice::check_advice(&analysis, crate::matrix::conda_forge(), &machine);

    CheckReport {
        requirement: req.to_string(),
        supported_range: analysis.supported.to_range_string(),
        platform: here,
        installed,
        environment_python: ctx.venv_python,
        pixi_project: ctx.pixi,
        verdict,
        conflict,
        why: advice.reasons,
        alternative: advice.alternative.filter(|_| !ctx.pixi),
        spec: req.is_pinned().then(|| req.spec.trim().to_string()),
        no_files,
        analysis,
    }
}

fn verdict(
    analysis: &PackageAnalysis,
    no_files: Option<&str>,
    conflict: Option<&ConflictPair>,
    ctx: &CheckContext<'_>,
) -> Verdict {
    let make = |kind, status, message: String| Verdict {
        kind,
        status,
        message,
    };
    // No Python helps either of the first two, so don't suggest one.
    if let Some(msg) = no_files {
        return make(VerdictKind::NoFiles, "error", msg.to_string());
    }
    if let Some(c) = conflict {
        return make(
            VerdictKind::Conflict,
            "error",
            format!(
                "{} cannot share an environment with this project's dependencies: {}",
                analysis.name, c.explanation
            ),
        );
    }
    match ctx.venv_python {
        Some(current) if analysis.supported.contains(current) => make(
            VerdictKind::Supported,
            "ok",
            format!("This project's Python {current} can run {}.", analysis.name),
        ),
        Some(current) => {
            let suggestion = analysis
                .supported
                .max()
                .map(|t| format!(" Use Python {t} instead."))
                .unwrap_or_default();
            make(
                VerdictKind::Unsupported,
                "error",
                format!(
                    "{} does not support this project's Python {current}.{suggestion}",
                    analysis.name
                ),
            )
        }
        None => {
            let suggestion = analysis
                .supported
                .max()
                .map(|t| format!(" Python {t} would suit it."))
                .unwrap_or_default();
            make(
                VerdictKind::NoEnvironment,
                "info",
                format!("{}{suggestion}", env::no_env_sentence(ctx.pixi)),
            )
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::core::platform::{Arch, Os};
    use crate::pypi::PyVersionSet;

    pub(crate) fn linux() -> Platform {
        Platform {
            os: Os::Linux,
            arch: Arch::X86_64,
        }
    }

    pub(crate) fn analysis(name: &str, minors: std::ops::RangeInclusive<u8>) -> PackageAnalysis {
        PackageAnalysis {
            name: name.into(),
            version: "1.0".into(),
            supported: PyVersionSet::from_versions(minors.map(PyVersion::py3)),
            has_platform_wheels: true,
            sdist_only: false,
            has_foreign_wheels: false,
            no_files_for_platform: false,
            wheel_platforms: Vec::new(),
            requires_python: Some(">=3.8".into()),
        }
    }

    pub(crate) fn ctx<'a>(
        venv_python: Option<PyVersion>,
        project: &'a [PackageAnalysis],
    ) -> CheckContext<'a> {
        CheckContext {
            workspace: Path::new("."),
            venv_python,
            pixi: false,
            project,
        }
    }

    #[test]
    fn a_supported_python_passes() {
        let r = build(
            &Requirement::any("tidy"),
            analysis("tidy", 8..=14),
            linux(),
            false,
            &ctx(Some(PyVersion::py3(12)), &[]),
        );
        assert_eq!(r.verdict.kind, VerdictKind::Supported);
        assert_eq!(
            r.verdict.message,
            "This project's Python 3.12 can run tidy."
        );
        assert!(!r.failed());
    }

    #[test]
    fn a_conflict_with_the_project_names_both_packages() {
        let project = [analysis("legacy", 8..=10)];
        let r = build(
            &Requirement::any("shiny"),
            analysis("shiny", 11..=14),
            linux(),
            false,
            &ctx(Some(PyVersion::py3(10)), &project),
        );
        assert_eq!(r.verdict.kind, VerdictKind::Conflict);
        assert!(r.failed());
        assert!(r.verdict.message.starts_with(
            "shiny cannot share an environment with this project's dependencies: legacy supports up to Python 3.10 but shiny requires at least Python 3.11"
        ));
        let pair = r.conflict.as_ref().unwrap();
        assert_eq!(
            (pair.low.name.as_str(), pair.high.name.as_str()),
            ("legacy", "shiny")
        );
    }

    #[test]
    fn json_fields_are_stable() {
        let r = build(
            &Requirement {
                name: "tidy".into(),
                spec: "==1.0".into(),
            },
            analysis("tidy", 10..=11),
            linux(),
            true,
            &ctx(Some(PyVersion::py3(13)), &[]),
        );
        let v = serde_json::to_value(&r).unwrap();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        let mut expected = vec![
            "requirement",
            "analysis",
            "supported_range",
            "platform",
            "installed",
            "environment_python",
            "pixi_project",
            "verdict",
            "conflict",
            "why",
            "alternative",
        ];
        expected.sort_unstable();
        let mut keys = keys;
        keys.sort_unstable();
        assert_eq!(keys, expected);
        assert_eq!(v["requirement"], "tidy==1.0");
        assert_eq!(v["supported_range"], "3.10–3.11");
        assert_eq!(v["environment_python"], "3.13");
        assert_eq!(
            v["analysis"]["supported"],
            serde_json::json!(["3.10", "3.11"])
        );
        assert_eq!(
            v["verdict"],
            serde_json::json!({
                "kind": "unsupported",
                "status": "error",
                "message": "tidy does not support this project's Python 3.13. Use Python 3.11 instead.",
            })
        );
        assert_eq!(v["conflict"], serde_json::Value::Null);
    }
}
