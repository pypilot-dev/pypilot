//! `pypilot doctor --fix` and the editor's "Fix environment": the fix that
//! fits the findings, not one rebuild for everything.
//!
//! [`plan`] reads the [`FixKind`] of every finding and builds a
//! [`RepairPlan`]; [`run_with_mode`] carries it out, smallest change first,
//! in this order:
//!
//! 0. requirement edits ([`FixKind::EditRequirement`],
//!    [`FixKind::AddRequirements`], run by [`req_edit::apply`]), then a
//!    fresh assessment, since they change what everything after resolves;
//!    requirements uv still cannot resolve stop the run here, before any
//!    environment is touched ([`resolution::blocking_step_in`]);
//! 1. nothing exists yet ([`FixKind::SetupEnvironment`]): the full bootstrap
//!    ([`setup::run_with_mode`]), which honours `.python-version`;
//! 2. rebuild the environment where it is ([`FixKind::RecreateWithPython`],
//!    [`FixKind::RepairEnvironment`]), on the Python a finding requires, else
//!    the one it had, else the pinned one;
//! 3. `uv lock` ([`FixKind::Relock`]);
//! 4. one sync into the existing environment ([`FixKind::SyncEnvironment`],
//!    with `--reinstall-package` for [`FixKind::ReinstallPackages`]);
//! 5. known-breakage fixes ([`FixKind::AddConstraint`],
//!    [`FixKind::ReplaceDist`]), through [`knowledge::apply::apply_all`]:
//!    a constraint such as `numpy<2`, or a stray dist removed and the right
//!    one reinstalled;
//! 6. the notebook kernel, then a check that the result is healthy: the
//!    package checks again, and an import of every declared package.
//!
//! A healthy project gets no steps at all. The Python version only changes
//! when a finding requires it, and the step that changes `.python-version`
//! says which finding forced it.
//!
//! Adding a fix: give the new [`FixKind`] one arm in [`plan`] (the only
//! dispatch point) that sets what it needs on [`RepairPlan`]; add a field
//! and a step in [`run_with_mode`] only when no existing step covers it.

use std::path::{Path, PathBuf};

use crate::core::exec::{self, Effect};
use crate::core::setup::{self, SetupSummary, Step};
use crate::core::{
    health, knowledge, pip, pixi, req_edit, resolution, solver, undeclared, uv, Assessment,
    Finding, FixKind, Severity,
};
use crate::pypi::pyversion::PyVersion;
use crate::pypi::MetadataSource;
use crate::settings::{ResolvedMode, Settings};

/// What the findings call for. Built by [`plan`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepairPlan {
    /// Nothing usable exists: run the full bootstrap.
    pub setup: bool,
    /// Rebuild the environment in place.
    pub rebuild: Option<Rebuild>,
    /// `uv lock` before syncing.
    pub relock: bool,
    /// Distributions to reinstall during the sync.
    pub reinstall: Vec<String>,
    /// Install the declared dependencies into the environment.
    pub sync: bool,
    /// Known-breakage fixes ([`knowledge::apply`]), deduplicated, in order.
    pub known: Vec<FixKind>,
    /// Titles of the findings this plan fixes, in order.
    pub fixes: Vec<String>,
    /// Error and Warning findings PyPilot cannot fix itself.
    pub manual: Vec<Finding>,
    /// Manifest edits to apply before anything else (see
    /// [`req_edit::apply`]).
    pub requirements: Vec<FixKind>,
}

/// A rebuild of the environment and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rebuild {
    /// The Python to rebuild on; `None` lets the run pick (the old one).
    pub python: Option<PyVersion>,
    /// True when a finding requires this version (the dependencies or
    /// requires-python exclude the current one), rather than merely
    /// repairing the environment.
    pub required: bool,
    /// The finding that asked for it.
    pub reason: String,
}

impl RepairPlan {
    /// True when there is nothing to run.
    pub fn is_empty(&self) -> bool {
        !self.setup
            && self.rebuild.is_none()
            && !self.relock
            && !self.sync
            && self.requirements.is_empty()
            && self.known.is_empty()
    }

    fn rebuild_on(&mut self, python: Option<PyVersion>, required: bool, reason: &str) {
        let replace = match &self.rebuild {
            None => true,
            // A version a finding requires outranks a plain repair.
            Some(r) => required && !r.required,
        };
        if replace {
            self.rebuild = Some(Rebuild {
                python,
                required,
                reason: reason.to_string(),
            });
        }
    }
}

/// Turn findings into a [`RepairPlan`]. Info findings are advice and are
/// never acted on. The one place that maps a [`FixKind`] to work.
pub fn plan(findings: &[Finding]) -> RepairPlan {
    let mut p = RepairPlan::default();
    for f in findings.iter().filter(|f| f.severity != Severity::Info) {
        match &f.fix {
            FixKind::SetupEnvironment => p.setup = true,
            // A project uv cannot set up as it stands: the bootstrap hands it
            // to its own tool (`core::foreign_setup`): pixi or a pyproject.toml
            // migration for a conda-only environment.yml, `poetry install`
            // for a Poetry project. A plain "conda detected" note is Info,
            // so it never gets here.
            FixKind::MigrateCondaToPixi | FixKind::MigrateConda | FixKind::PoetryInstall => {
                p.setup = true
            }
            FixKind::RecreateWithPython(py) => p.rebuild_on(Some(*py), true, &f.title),
            FixKind::RepairEnvironment { python } => p.rebuild_on(*python, false, &f.title),
            FixKind::Relock => p.relock = true,
            FixKind::ReinstallPackages(names) => {
                for n in names {
                    if !p.reinstall.contains(n) {
                        p.reinstall.push(n.clone());
                    }
                }
            }
            FixKind::SyncEnvironment => p.sync = true,
            // Requirement edits: applied first, then the project is assessed
            // again (`run_with_mode`).
            FixKind::EditRequirement(_) | FixKind::AddRequirements(_) => {
                if !p.requirements.contains(&f.fix) {
                    p.requirements.push(f.fix.clone());
                }
            }
            // Known breakages: run after the rebuild and the sync.
            FixKind::AddConstraint(_) | FixKind::ReplaceDist(_) => {
                if !p.known.contains(&f.fix) {
                    p.known.push(f.fix.clone());
                }
            }
            // Suggestions the user runs themselves (`migrate-conda`,
            // `migrate-to-pixi`), or guidance only.
            FixKind::Manual | FixKind::MigrateToPixi => {
                p.manual.push(f.clone());
                continue;
            }
        }
        p.fixes.push(f.title.clone());
    }
    // A rebuilt environment starts empty: everything is installed fresh.
    if p.rebuild.is_some() {
        p.reinstall.clear();
    }
    if p.rebuild.is_some() || p.relock || !p.reinstall.is_empty() {
        p.sync = true;
    }
    p
}

/// [`run_with_mode`] in the mode the settings resolve to.
pub async fn run<S: MetadataSource>(
    workspace: &Path,
    settings: &Settings,
    source: &S,
) -> crate::Result<SetupSummary> {
    let mode = settings.resolve_mode(workspace).await;
    run_with_mode(workspace, settings, source, mode).await
}

/// Assess the project and run the fixes its findings call for. The summary
/// is `ok` only when every step succeeded and the check after them passed.
pub async fn run_with_mode<S: MetadataSource>(
    workspace: &Path,
    settings: &Settings,
    source: &S,
    mode: ResolvedMode,
) -> crate::Result<SetupSummary> {
    let pixi_project = pixi::detect(workspace).is_some();
    let mode = if pixi_project {
        ResolvedMode::Pixi
    } else {
        mode
    };
    let mut assessment = assess_with_imports(workspace, settings, source).await;
    let mut plan = plan(&assessment.findings);
    let first_why = describe(&plan);

    // Requirement edits first: they change what the rest resolves, so the
    // project is assessed again after them. Findings they fixed are dropped
    // from the new assessment too, which is what keeps a dry run (where
    // the files do not change) from planning them twice.
    let mut edit_steps = Vec::new();
    if !plan.requirements.is_empty() {
        for fix in &plan.requirements {
            match req_edit::apply(workspace, settings, fix).await {
                Ok(steps) => edit_steps.extend(steps),
                Err(e) => edit_steps.push(Step {
                    name: format!("apply {}", fix.kind()),
                    ok: false,
                    detail: format!("{e:#}"),
                }),
            }
        }
        if edit_steps.iter().all(|s| s.ok) {
            let applied = std::mem::take(&mut plan.requirements);
            assessment = assess_with_imports(workspace, settings, source).await;
            assessment.findings.retain(|f| !applied.contains(&f.fix));
            plan = self::plan(&assessment.findings);
            plan.requirements.clear();
        }
    }

    let env = Target::of(workspace, &assessment);
    let mut summary = SetupSummary {
        package_manager: settings.package_manager,
        resolved_mode: mode,
        python: env.python,
        venv_path: env.dir.clone(),
        why: if edit_steps.is_empty() {
            first_why
        } else {
            format!("{first_why} Then: {}", describe(&plan))
        },
        steps: edit_steps,
        ok: true,
    };
    if summary.steps.iter().any(|s| !s.ok) {
        finish(&mut summary);
        return Ok(summary);
    }

    // Requirements uv cannot resolve would fail the sync after the
    // environment was rebuilt: stop while nothing has changed.
    if let Some(step) = resolution::blocking_step_in(&assessment.findings) {
        summary.steps.push(step);
        finish(&mut summary);
        return Ok(summary);
    }

    if plan.is_empty() {
        let blocking: Vec<&Finding> = plan
            .manual
            .iter()
            .filter(|f| f.severity == Severity::Error)
            .collect();
        if let Some(first) = blocking.first() {
            summary.ok = false;
            summary.steps.push(Step {
                name: "fix automatically".into(),
                ok: false,
                detail: format!(
                    "PyPilot cannot fix this by itself: {}. {}",
                    first.title, first.detail
                ),
            });
        }
        return Ok(summary);
    }

    // pixi owns a pixi project's environment: `pixi install` is its fix for
    // every kind. Nothing usable yet: the full bootstrap.
    if mode == ResolvedMode::Pixi || plan.setup {
        let mut s = setup::run_with_mode(workspace, settings, source, mode).await?;
        if !plan.fixes.is_empty() {
            s.why = format!("{} {}", describe(&plan), s.why);
        }
        // A conda-only project the bootstrap just moved to pixi is pixi's
        // now, like any pixi project.
        if s.resolved_mode != ResolvedMode::Pixi {
            if s.steps.iter().all(|st| st.ok) {
                knowledge::apply::apply_all(workspace, settings, &plan.known, &mut s.steps).await;
            }
            verify(workspace, settings, &mut s).await;
        }
        finish(&mut s);
        return Ok(s);
    }

    let uv_info = match mode {
        ResolvedMode::UvFull | ResolvedMode::UvPipCompat => match uv::ensure(settings).await {
            Ok(info) => {
                step(
                    &mut summary,
                    "ensure uv",
                    true,
                    &format!(
                        "uv {} ({})",
                        info.version,
                        if info.managed { "managed" } else { "on PATH" }
                    ),
                );
                Some(info)
            }
            Err(e) => {
                step(&mut summary, "ensure uv", false, &e.to_string());
                finish(&mut summary);
                return Ok(summary);
            }
        },
        _ => None,
    };

    let mut python = env.python.or(assessment.target_python);
    if let Some(rebuild) = &plan.rebuild {
        let py = rebuild
            .python
            .or(env.python)
            .or(assessment.probes.python_pin)
            .or(assessment.target_python)
            .or_else(|| assessment.probes.interpreters.first().map(|i| i.version));
        let Some(py) = py else {
            step(
                &mut summary,
                "choose Python",
                false,
                "no Python version is known for the rebuild and none is installed",
            );
            finish(&mut summary);
            return Ok(summary);
        };
        python = Some(py);
        summary.python = Some(py);
        rebuild_env(
            workspace,
            &assessment,
            &env,
            uv_info.as_ref(),
            mode,
            rebuild,
            py,
            &mut summary,
        )
        .await;
        if summary.steps.iter().any(|s| !s.ok) {
            finish(&mut summary);
            return Ok(summary);
        }
    }

    if plan.sync {
        sync(
            workspace,
            &env,
            uv_info.as_ref(),
            mode,
            &plan,
            python,
            &assessment,
            &mut summary,
        )
        .await;
    }

    if summary.steps.iter().all(|s| s.ok) && !plan.known.is_empty() {
        knowledge::apply::apply_all(workspace, settings, &plan.known, &mut summary.steps).await;
    }

    if summary.steps.iter().all(|s| s.ok) {
        if env.dir == workspace.join(".venv") {
            setup::ensure_ipykernel(workspace, settings, mode, &mut summary).await;
        }
        verify(workspace, settings, &mut summary).await;
    }
    finish(&mut summary);
    Ok(summary)
}

/// [`solver::assess`] plus the imports no dependency provides
/// ([`undeclared`]), which only a workspace walk finds and so are not part
/// of every assessment.
async fn assess_with_imports<S: MetadataSource>(
    workspace: &Path,
    settings: &Settings,
    source: &S,
) -> Assessment {
    let mut a = solver::assess(workspace, settings, source).await;
    if let Some(project) = pixi::detect(workspace) {
        // A stale pixi.lock is doctor's own check (`pixi lock --check`, too
        // slow for every assessment), so ask for it here: `pixi install`
        // fixes it.
        let status = crate::core::pixi_envs::lock_status(workspace).await;
        a.findings
            .extend(crate::core::pixi_envs::lock_finding(&project, &status));
    } else if !a.project.is_pixi() {
        let venv = a.probes.venv.as_ref().map(|v| (v.path.as_path(), v.python));
        let missing = undeclared::find(workspace, &a.project, venv, source).await;
        a.findings.extend(undeclared::finding(&missing));
    }
    a
}

/// The environment the fix works on: the project's venv wherever it lives
/// (`.venv`, `venv`, `env`), a damaged one, else a new `.venv`.
struct Target {
    dir: PathBuf,
    python: Option<PyVersion>,
    /// The directory exists but is not a venv any more.
    damaged: bool,
}

impl Target {
    fn of(workspace: &Path, a: &Assessment) -> Target {
        if let Some(v) = a.probes.venv.as_ref().filter(|v| !v.kind.is_pixi()) {
            return Target {
                dir: v.path.clone(),
                python: v.python,
                damaged: false,
            };
        }
        if let Some(dir) = health::damaged_venv(workspace) {
            return Target {
                dir,
                python: None,
                damaged: true,
            };
        }
        Target {
            dir: workspace.join(".venv"),
            python: None,
            damaged: false,
        }
    }

    /// `Some(dir)` when it is not the `.venv` uv syncs by default.
    fn custom_dir(&self, workspace: &Path) -> Option<PathBuf> {
        (self.dir != workspace.join(".venv")).then(|| self.dir.clone())
    }
}

/// One line on what the plan fixes, for the summary's `why`.
fn describe(plan: &RepairPlan) -> String {
    if plan.is_empty() {
        return if plan.manual.is_empty() {
            "Nothing to fix: the environment matches the project.".to_string()
        } else {
            "Nothing PyPilot can fix automatically; see the findings above.".to_string()
        };
    }
    format!("Fixing: {}.", plan.fixes.join("; "))
}

/// Rebuild the environment at `env.dir` on `py`.
#[allow(clippy::too_many_arguments)]
async fn rebuild_env(
    workspace: &Path,
    assessment: &Assessment,
    env: &Target,
    uv_info: Option<&uv::UvInfo>,
    mode: ResolvedMode,
    rebuild: &Rebuild,
    py: PyVersion,
    summary: &mut SetupSummary,
) {
    // `uv venv --clear` and `python -m venv` both refuse or reuse a
    // directory that is no longer a venv; start it from scratch.
    let pip_mode = mode == ResolvedMode::PurePip;
    if (env.damaged || (pip_mode && exec::exists(&env.dir))) && exec::exists(&env.dir) {
        let effect = Effect::new(
            "remove the old environment",
            "it is no longer a usable virtual environment, and the tools that rebuild it refuse or reuse the directory",
        );
        match exec::remove_dir_all(&env.dir, effect) {
            Ok(()) => step(
                summary,
                "remove the old environment",
                true,
                &env.dir.display().to_string(),
            ),
            Err(e) => {
                step(summary, "remove the old environment", false, &e.to_string());
                return;
            }
        }
    }

    match uv_info {
        Some(uv_info) => {
            let name = format!("uv python install {py}");
            match uv::python_install(uv_info, py, workspace).await {
                Ok(out) if out.success() => step(summary, &name, true, "ready"),
                Ok(out) => return step(summary, &name, false, out.stderr.trim()),
                Err(e) => return step(summary, &name, false, &e.to_string()),
            }
            match uv::create_venv(uv_info, py, &env.dir, workspace).await {
                Ok(out) if out.success() => step(
                    summary,
                    "uv venv",
                    true,
                    &format!("{} on Python {py} ({})", env.dir.display(), rebuild.reason),
                ),
                Ok(out) => return step(summary, "uv venv", false, out.stderr.trim()),
                Err(e) => return step(summary, "uv venv", false, &e.to_string()),
            }
            // Move the pin only when it disagrees with a version a finding
            // requires; a repair keeps whatever the project pinned.
            let pin = assessment.probes.python_pin;
            if mode == ResolvedMode::UvFull
                && exec::exists(&workspace.join("pyproject.toml"))
                && pin.is_some_and(|p| p != py)
            {
                let name = format!("uv python pin {py}");
                let why = format!(
                    ".python-version pinned {} ; changed because: {}",
                    pin.map(|p| p.to_string()).unwrap_or_default(),
                    rebuild.reason
                )
                .replace(" ;", ";");
                match uv::pin(uv_info, py, workspace).await {
                    Ok(out) if out.success() => step(summary, &name, true, &why),
                    Ok(out) => step(summary, &name, false, out.stderr.trim()),
                    Err(e) => step(summary, &name, false, &e.to_string()),
                }
            }
        }
        None => {
            let Some(interp) = pip::select_interpreter(&assessment.probes.interpreters, py) else {
                return step(
                    summary,
                    "select interpreter",
                    false,
                    &format!("Python {py} is required but not installed. Install it, then run the fix again. pip mode cannot fetch interpreters."),
                );
            };
            step(
                summary,
                "select interpreter",
                true,
                &format!("{} ({})", interp.command, interp.version),
            );
            match pip::create_venv(interp, &env.dir, workspace).await {
                Ok(out) if out.success() => step(
                    summary,
                    "python -m venv",
                    true,
                    &env.dir.display().to_string(),
                ),
                Ok(out) => step(summary, "python -m venv", false, out.stderr.trim()),
                Err(e) => step(summary, "python -m venv", false, &e.to_string()),
            }
        }
    }
}

/// Install the declared dependencies into `env.dir`, reinstalling
/// `plan.reinstall`: `uv lock` first when the plan says so, then one sync.
#[allow(clippy::too_many_arguments)]
async fn sync(
    workspace: &Path,
    env: &Target,
    uv_info: Option<&uv::UvInfo>,
    mode: ResolvedMode,
    plan: &RepairPlan,
    python: Option<PyVersion>,
    assessment: &Assessment,
    summary: &mut SetupSummary,
) {
    let pyproject = exec::exists(&workspace.join("pyproject.toml"));
    let requirements = workspace.join("requirements.txt");
    match (mode, uv_info) {
        (ResolvedMode::UvFull, Some(uv_info)) if pyproject => {
            if plan.relock {
                match uv::lock(uv_info, workspace).await {
                    Ok(out) if out.success() => step(summary, "uv lock", true, "uv.lock updated"),
                    // Let the sync below explain (and recover from) a
                    // requires-python floor that is too low.
                    Ok(out) if raised_floor(&out.stderr).is_some() => {}
                    Ok(out) => return step(summary, "uv lock", false, out.stderr.trim()),
                    Err(e) => return step(summary, "uv lock", false, &e.to_string()),
                }
            }
            let locked = !plan.relock && exec::exists(&workspace.join("uv.lock"));
            let mut opts = uv::SyncOptions {
                locked,
                python,
                reinstall: plan.reinstall.clone(),
                env_dir: env.custom_dir(workspace),
            };
            uv_sync_recovering(workspace, uv_info, &mut opts, summary).await;
        }
        (ResolvedMode::UvFull | ResolvedMode::UvPipCompat, Some(uv_info)) => {
            let mut args = install_args(&requirements, assessment);
            for p in &plan.reinstall {
                args.push("--reinstall-package".into());
                args.push(p.clone());
            }
            if args.is_empty() {
                return step(
                    summary,
                    "install dependencies",
                    true,
                    "no manifest to install from",
                );
            }
            let exe = pip::venv_python(&env.dir);
            let out = uv::pip_install_into(uv_info, &exe, &args, &env.dir, workspace).await;
            record(summary, &format!("uv pip install {}", args.join(" ")), out);
        }
        _ => {
            if !plan.reinstall.is_empty() {
                let mut args = vec!["--force-reinstall".to_string(), "--no-deps".to_string()];
                args.extend(plan.reinstall.iter().cloned());
                let out = pip::install_packages(&env.dir, &args, None, workspace).await;
                record(summary, &format!("pip install {}", args.join(" ")), out);
                if summary.steps.iter().any(|s| !s.ok) {
                    return;
                }
            }
            if requirements.is_file() {
                let out = pip::install_requirements(&env.dir, &requirements, workspace).await;
                record(summary, "pip install -r requirements.txt", out);
            } else if !assessment.project.packages.is_empty() {
                let names = assessment.project.names();
                let out = pip::install_packages(&env.dir, &names, None, workspace).await;
                record(summary, &format!("pip install {}", names.join(" ")), out);
            }
        }
    }
}

/// `-r requirements.txt`, or the declared names when there is none.
fn install_args(requirements: &Path, assessment: &Assessment) -> Vec<String> {
    if requirements.is_file() {
        vec!["-r".into(), requirements.to_string_lossy().into_owned()]
    } else {
        assessment.project.names()
    }
}

/// `uv sync` with two recoveries uv's own message points at: a lock uv
/// finds out of date (`--locked` refused) is updated and synced again, and
/// a `requires-python` floor below what a dependency supports is raised in
/// pyproject.toml (the step says which package forced it) and retried.
async fn uv_sync_recovering(
    workspace: &Path,
    uv_info: &uv::UvInfo,
    opts: &mut uv::SyncOptions,
    summary: &mut SetupSummary,
) {
    for _ in 0..4 {
        let name = uv::sync_step_name(opts);
        let out = match uv::sync_with(uv_info, opts, workspace).await {
            Ok(out) => out,
            Err(e) => return step(summary, &name, false, &e.to_string()),
        };
        if out.success() {
            let detail = if opts.reinstall.is_empty() {
                "dependencies installed".to_string()
            } else {
                format!("reinstalled {}", opts.reinstall.join(", "))
            };
            return step(summary, &name, true, &detail);
        }
        let err = out.stderr.trim();
        if opts.locked && err.contains("needs to be updated") {
            step(
                summary,
                &name,
                true,
                "uv found uv.lock out of date; updating it and syncing again",
            );
            opts.locked = false;
            continue;
        }
        if let Some(floor) = raised_floor(err) {
            match raise_requires_python(workspace, floor) {
                Ok(Some((old, new))) => {
                    step(
                        summary,
                        "raise requires-python",
                        true,
                        &format!(
                            "pyproject.toml: `{old}` -> `{new}`, because {}",
                            floor_reason(err)
                        ),
                    );
                    opts.locked = false;
                    continue;
                }
                Ok(None) => {}
                Err(e) => return step(summary, "raise requires-python", false, &e.to_string()),
            }
        }
        return step(summary, &name, false, err);
    }
}

/// The Python floor a failed resolve asks for: uv names the Python range
/// it could not solve (`split (markers: python_full_version >= '3.8' and
/// python_full_version < '3.10')`), or says `does not satisfy Python>=3.11`.
pub fn raised_floor(stderr: &str) -> Option<PyVersion> {
    if !stderr.contains("requested Python version") {
        return None;
    }
    if let Some(line) = stderr.lines().find(|l| l.contains("split (markers:")) {
        if let Some(i) = line.rfind("python_full_version < '") {
            let rest = &line[i + "python_full_version < '".len()..];
            if let Some(v) = rest.split('\'').next().and_then(PyVersion::parse) {
                return Some(v);
            }
        }
    }
    let i = stderr.find("does not satisfy Python>=")?;
    let rest = &stderr[i + "does not satisfy Python>=".len()..];
    let v: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    PyVersion::parse(v.trim_end_matches('.'))
}

/// The package uv blamed, for the step's "because ...".
fn floor_reason(stderr: &str) -> String {
    let blamed = stderr
        .lines()
        .map(str::trim)
        .find(|l| {
            l.split(['>', '<', '=', '!', '~']).next().is_some_and(|n| {
                !n.is_empty()
                    && n.chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
            }) && l.contains(['>', '<', '='])
                && !l.starts_with("Python")
        })
        .map(|l| l.trim_end_matches([',', ':']).to_string());
    match blamed {
        Some(req) => format!("{req} does not support the lower Pythons it allowed (uv could not resolve for them)"),
        None => "a dependency does not support the lower Pythons it allowed (uv could not resolve for them)".to_string(),
    }
}

/// Set the lower bound of `[project] requires-python` to `>=floor`, keeping
/// any upper bound. Returns the old and new specifier, or `None` when the
/// bound already is at least `floor` (nothing to do).
pub fn raise_requires_python(
    workspace: &Path,
    floor: PyVersion,
) -> std::io::Result<Option<(String, String)>> {
    let path = workspace.join("pyproject.toml");
    let text = std::fs::read_to_string(&path)?;
    let Some((old, new, edited)) = raised_requires_python(&text, floor) else {
        return Ok(None);
    };
    let effect = Effect::new(
        "raise requires-python",
        format!("a dependency needs Python {floor} or newer, so `{old}` cannot be resolved"),
    );
    exec::write(&path, edited, effect)?;
    Ok(Some((old, new)))
}

/// The edit [`raise_requires_python`] makes, on the file's text.
fn raised_requires_python(text: &str, floor: PyVersion) -> Option<(String, String, String)> {
    let mut doc: toml_edit::DocumentMut = text.parse().ok()?;
    let project = doc.get_mut("project")?.as_table_like_mut()?;
    let old = project
        .get("requires-python")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let current = crate::pypi::pyversion::parse_requires_python(&old);
    if current.min().is_some_and(|m| m >= floor) {
        return None;
    }
    let mut clauses: Vec<String> = old
        .split(',')
        .map(str::trim)
        .filter(|c| {
            !c.is_empty() && !c.starts_with(">=") && !c.starts_with('>') && !c.starts_with("~=")
        })
        .map(str::to_string)
        .collect();
    clauses.insert(0, format!(">={floor}"));
    let new = clauses.join(",");
    project.insert("requires-python", toml_edit::value(new.clone()));
    Some((old, new, doc.to_string()))
}

/// After the fix: the package checks again, and every declared package
/// imported in the environment. A failure fails the run, so "Done" is only
/// printed for an environment that works.
async fn verify(workspace: &Path, settings: &Settings, summary: &mut SetupSummary) {
    const NAME: &str = "verify";
    if summary.steps.iter().any(|s| !s.ok) {
        return;
    }
    if exec::note(Effect::new(
        NAME,
        "check the environment again (installed packages against pyproject.toml and uv.lock) and import every declared package in it",
    )) {
        return;
    }
    let Some(env) = crate::core::env::project_env(workspace) else {
        return step(summary, NAME, false, "no environment found after the fix");
    };
    summary.venv_path = env.path.clone();
    if env.python.is_some() {
        summary.python = env.python;
    }
    let info = crate::core::probe::VenvInfo::from(env.clone());
    let left: Vec<String> = health::check(workspace, Some(&info), settings.package_manager, false)
        .into_iter()
        .filter(|f| f.severity != Severity::Info)
        .map(|f| f.title)
        .collect();
    if !left.is_empty() {
        return step(
            summary,
            NAME,
            false,
            &format!("still wrong: {}", left.join("; ")),
        );
    }
    let modules = import_targets(workspace, &env.path);
    if modules.is_empty() {
        return step(summary, NAME, true, "environment healthy");
    }
    match import_check(&env.python_exe, &modules, workspace).await {
        Ok(()) => step(
            summary,
            NAME,
            true,
            &format!("environment healthy; imported {}", modules.join(", ")),
        ),
        Err(why) => step(summary, NAME, false, &why),
    }
}

/// The top-level modules of every declared package that is installed: from
/// `top_level.txt`, else the packages and modules its `RECORD` lists, else
/// the name's usual import form. Private (`_x`) and test modules are left
/// out, and at most two per package are imported.
fn import_targets(workspace: &Path, venv: &Path) -> Vec<String> {
    let declared = health::declared_packages(workspace).names;
    let sites = crate::core::installed::site_packages_dirs(venv);
    let mut out = Vec::new();
    for name in declared {
        let mut mods = dist_modules(&sites, &name);
        if mods.is_empty() {
            mods = health::import_candidates(&name)
                .into_iter()
                .filter(|m| {
                    sites.iter().any(|s| {
                        s.join(m).join("__init__.py").is_file()
                            || s.join(format!("{m}.py")).is_file()
                    })
                })
                .collect();
        }
        for m in mods.into_iter().take(2) {
            if !out.contains(&m) {
                out.push(m);
            }
        }
    }
    out
}

/// Top-level importable names a distribution installs.
fn dist_modules(sites: &[PathBuf], name: &str) -> Vec<String> {
    for site in sites {
        let Ok(entries) = std::fs::read_dir(site) else {
            continue;
        };
        for entry in entries.flatten() {
            let file = entry.file_name().to_string_lossy().into_owned();
            if !file.ends_with(".dist-info")
                || crate::core::installed::distribution_name(&file).as_deref() != Some(name)
            {
                continue;
            }
            let usable = |m: &str| {
                !m.is_empty()
                    && !m.starts_with('_')
                    && m != "tests"
                    && m != "test"
                    && m.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            };
            if let Ok(text) = std::fs::read_to_string(entry.path().join("top_level.txt")) {
                let mods: Vec<String> = text
                    .lines()
                    .map(str::trim)
                    .filter(|m| usable(m))
                    .map(str::to_string)
                    .collect();
                if !mods.is_empty() {
                    return mods;
                }
            }
            let record = std::fs::read_to_string(entry.path().join("RECORD")).unwrap_or_default();
            let mut mods: Vec<String> = Vec::new();
            for line in record.lines() {
                let path = line.split(',').next().unwrap_or("");
                let mut parts = path.split('/');
                let top = parts.next().unwrap_or("");
                let module = match parts.next() {
                    Some("__init__.py") => Some(top.to_string()),
                    None => top.strip_suffix(".py").map(str::to_string),
                    _ => None,
                };
                if let Some(m) = module.filter(|m| usable(m)) {
                    if !mods.contains(&m) {
                        mods.push(m);
                    }
                }
            }
            return mods;
        }
    }
    Vec::new()
}

/// Import `modules` with the environment's Python, in one process.
async fn import_check(python: &Path, modules: &[String], cwd: &Path) -> Result<(), String> {
    const SCRIPT: &str = "import importlib, sys\nbad = []\nfor m in sys.argv[1:]:\n    try:\n        importlib.import_module(m)\n    except Exception as e:\n        bad.append('%s (%s: %s)' % (m, type(e).__name__, str(e).splitlines()[0] if str(e) else ''))\nprint('; '.join(bad))\nsys.exit(1 if bad else 0)\n";
    let mut args: Vec<String> = vec!["-I".into(), "-c".into(), SCRIPT.into()];
    args.extend(modules.iter().cloned());
    let out = crate::core::command::run_with_timeout(
        python,
        &args,
        Some(cwd),
        std::time::Duration::from_secs(180),
    )
    .await
    .map_err(|e| format!("could not run the import check: {e}"))?;
    if out.success() {
        Ok(())
    } else {
        let why = out.stdout.trim();
        Err(if why.is_empty() {
            format!("import check failed: {}", out.stderr.trim())
        } else {
            format!("these imports fail: {why}")
        })
    }
}

/// Every step must have succeeded for the run to count.
fn finish(summary: &mut SetupSummary) {
    if summary.steps.iter().any(|s| !s.ok) {
        summary.ok = false;
    }
}

fn step(summary: &mut SetupSummary, name: &str, ok: bool, detail: &str) {
    summary.steps.push(Step {
        name: name.to_string(),
        ok,
        detail: detail.to_string(),
    });
    if !ok {
        summary.ok = false;
    }
}

fn record(
    summary: &mut SetupSummary,
    name: &str,
    out: crate::Result<crate::core::command::Output>,
) {
    match out {
        Ok(o) if o.success() => step(summary, name, true, "installed"),
        Ok(o) => step(summary, name, false, o.stderr.trim()),
        Err(e) => step(summary, name, false, &e.to_string()),
    }
}

#[cfg(test)]
#[path = "repair_tests.rs"]
mod tests;
