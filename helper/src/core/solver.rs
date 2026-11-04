//! The constraint solver — turns probes + project + PyPI metadata into findings.
//!
//! [`assess`] is the single read-only entry point used by both `doctor` and F5.
//! The decision logic lives in the pure [`synthesize`] function so it can be unit
//! tested without spawning any processes. Every finding states the *why* — which
//! package constrained the Python choice.

use std::path::{Path, PathBuf};

use crate::core::platform::{Os, Platform};
use crate::core::probe::Probes;
use crate::core::project::ProjectDeps;
#[cfg(test)]
use crate::core::project::Requirement;
use crate::core::toolchain::{self, Toolchain};
use crate::core::{Assessment, Finding, FixKind, Severity};
use crate::matrix;
use crate::pypi::metadata::{PackageAnalysis, SdistKind};
use crate::pypi::pyversion::{PyVersion, PyVersionSet};
use crate::pypi::{self, CompatReport, MetadataSource};
use crate::settings::Settings;

/// Flattened, process-free view of the environment for [`synthesize`].
#[derive(Debug, Clone, Default)]
pub struct EnvSummary {
    pub has_venv: bool,
    pub venv_python: Option<PyVersion>,
    pub uv_present: bool,
    pub interpreters: Vec<PyVersion>,
    /// The workspace contains at least one `.ipynb`.
    pub has_notebooks: bool,
    /// The venv has an `ipykernel` that works on its Python, so Jupyter and
    /// VS Code can use it as a kernel.
    pub venv_has_ipykernel: bool,
    /// Build tools on this machine, probed only when some dependency would
    /// compile from source. `None` means not probed: advise everything.
    pub toolchain: Option<Toolchain>,
    /// The interpreter a broken venv is missing (see
    /// [`crate::core::probe::missing_interpreter`]).
    pub venv_missing_python: Option<PathBuf>,
    /// Normalized names of dependencies whose source build a more specific
    /// finding already explains (CUDA extensions, see
    /// [`matrix::HardwareReport::explained_source_builds`]). They get no
    /// generic "Will compile from source" finding on top.
    pub explained_source_builds: Vec<String>,
    /// The venv directory, when there is one (`.venv`, `venv`, `env`).
    pub venv_path: Option<PathBuf>,
    /// The Python `.python-version` pins, when it names one.
    pub python_pin: Option<PyVersion>,
    /// A venv directory whose `pyvenv.cfg` is gone (see
    /// [`crate::core::health::damaged_venv`]), when there is no usable one.
    pub damaged_venv: Option<PathBuf>,
}

impl EnvSummary {
    fn from_probes(p: &Probes, workspace: &Path) -> EnvSummary {
        EnvSummary {
            has_venv: p.venv.is_some(),
            venv_python: p.venv.as_ref().and_then(|v| v.python),
            venv_missing_python: p.venv.as_ref().and_then(|v| v.broken.clone()),
            uv_present: p.uv.is_some(),
            interpreters: p.interpreters.iter().map(|i| i.version).collect(),
            has_notebooks: crate::core::project::has_notebooks(workspace),
            venv_has_ipykernel: p.venv.as_ref().is_some_and(|v| {
                crate::core::installed::has_usable_ipykernel(
                    &crate::core::installed::scan(&v.path),
                    v.python,
                )
            }),
            toolchain: None,
            explained_source_builds: Vec::new(),
            venv_path: p.venv.as_ref().map(|v| v.path.clone()),
            python_pin: p.python_pin,
            damaged_venv: p
                .venv
                .is_none()
                .then(|| crate::core::health::damaged_venv(workspace))
                .flatten(),
        }
    }
}

/// `packages` minus those whose resolved release publishes no files for this
/// platform, matched by normalized name.
fn installable_packages(
    packages: &[crate::core::project::Requirement],
    compat: Option<&CompatReport>,
) -> Vec<crate::core::project::Requirement> {
    let Some(compat) = compat else {
        return packages.to_vec();
    };
    let blocked: std::collections::HashSet<String> = compat
        .per_package
        .iter()
        .filter(|a| a.no_files_for_platform)
        .map(|a| crate::core::project::normalize_name(&a.name))
        .collect();
    packages
        .iter()
        .filter(|r| !blocked.contains(&crate::core::project::normalize_name(&r.name)))
        .cloned()
        .collect()
}

/// The release `uv.lock` pins for each package it locks at one version, when
/// the project installs from it (a uv project, not pixi or Poetry-only) and
/// the lock is current for pyproject.toml. `None` otherwise: a stale lock
/// is re-resolved by the sync, so its versions say nothing.
fn current_lock_versions(
    workspace: &Path,
    settings: &Settings,
    project: &ProjectDeps,
) -> Option<std::collections::BTreeMap<String, String>> {
    if project.is_pixi() || !crate::core::health::uses_lock(settings.package_manager) {
        return None;
    }
    let pyproject = crate::core::project::read_manifest(&workspace.join("pyproject.toml"))?;
    if crate::core::manifests::is_poetry_only(&pyproject) {
        return None;
    }
    let lock = crate::core::health::read_lock(workspace)?;
    lock.root_requires.as_ref()?;
    if !crate::core::health::lock_changes(workspace, &lock).is_empty() {
        return None;
    }
    Some(single_versions(&lock))
}

/// Packages a lock pins at exactly one version.
fn single_versions(lock: &crate::core::health::Lock) -> std::collections::BTreeMap<String, String> {
    lock.versions
        .iter()
        .filter(|(_, v)| v.len() == 1)
        .filter_map(|(name, v)| Some((name.clone(), v.iter().next()?.clone())))
        .collect()
}

/// Full read-only assessment of a workspace (F5 / doctor).
pub async fn assess<S: MetadataSource>(
    workspace: &Path,
    settings: &Settings,
    source: &S,
) -> Assessment {
    let probes = crate::core::probe::run(workspace, settings).await;
    let project = crate::core::project::scan(workspace);

    let compat = if project.packages.is_empty() {
        None
    } else {
        let mut report = pypi::analyze(source, Platform::current(), &project.packages).await;
        // A current uv.lock is what `uv sync --locked` installs: judge the
        // Pythons by its releases, as the lock-file finding says PyPilot
        // follows it.
        let locked = current_lock_versions(workspace, settings, &project);
        if let Some(locked) = &locked {
            pypi::fit_report_to_lock(source, &mut report, locked).await;
        }
        // An existing venv: a version range is judged by the release the
        // installer picks on its Python, not by the newest release. With a
        // current lock that is usually the locked release, already judged;
        // a package locked at several versions (a fork across Pythons, c12c:
        // numpy 2.2 below 3.12, 2.5 from there) is fitted here too. This
        // does nothing when the report already admits the venv's Python.
        if let Some(venv) = probes.venv.as_ref() {
            if let Some(py) = venv.python {
                pypi::fit_report_to_python(source, &mut report, &project.packages, py).await;
                // What is installed there installed on that Python.
                let installed = crate::core::installed::scan(&venv.path).versions;
                pypi::trust_installed(&mut report, &installed, py);
            }
        }
        Some(report)
    };

    // Hardware advice only makes sense for packages that can be installed here.
    // A release with no files for this platform already gets its own finding,
    // and a GPU note on top ("installs and runs CPU-only") would contradict it.
    let installable = installable_packages(&project.packages, compat.as_ref());
    let hardware = matrix::check(workspace, settings, &installable).await;

    let mut env = EnvSummary::from_probes(&probes, workspace);
    env.explained_source_builds = hardware.explained_source_builds;
    // Probing compilers spawns processes, so only do it when a dependency
    // would actually build from source.
    if compat
        .as_ref()
        .is_some_and(|c| needs_source_build(c, env.venv_python))
    {
        env.toolchain = Some(toolchain::detect(Platform::current().os).await);
    }
    let (target_python, mut findings) = synthesize(&env, &project, compat.as_ref());
    findings.extend(hardware.findings);
    // The manifests themselves: unreadable requirements.txt lines, pins that
    // disagree, two lock files, Poetry and conda-only projects.
    crate::core::manifests::apply(
        &mut findings,
        workspace,
        settings,
        &project,
        compat.as_ref(),
        probes.venv.as_ref(),
        source,
    )
    .await;

    // What is installed against what is declared and locked: missing,
    // drifted, half-installed packages, a stale uv.lock (file reads only).
    if project.is_python_project() {
        findings.extend(crate::core::health::check(
            workspace,
            probes.venv.as_ref(),
            settings.package_manager,
            project.is_pixi(),
        ));
    }

    // Walls pip and uv cannot climb but conda-forge can (system libraries,
    // nvcc): say pixi would help. Skips pixi projects.
    crate::core::pixi_advice::apply(
        &mut findings,
        workspace,
        &project,
        compat.as_ref(),
        env.venv_python,
        target_python,
    );

    if let Some(venv) = &probes.venv {
        // A CPU torch on an NVIDIA machine, however it got there: checked
        // against what is installed, not what is declared, since a later
        // install can swap the CUDA build out without touching any manifest.
        findings.extend(crate::core::torch_gpu::static_check(&venv.path, settings).await);

        // Installed packages whose metadata rules out an installed dependency,
        // above all extensions built for NumPy 1.x sitting next to NumPy 2.
        // Read off disk, so it costs file reads rather than a resolve.
        findings.extend(crate::core::depcheck::check(&venv.path, venv.python));
    }

    // Breakages metadata does not declare (data/known_breakages.json): numpy
    // 2 under old compiled packages, uncapped API removals, shadowing
    // dists, removed stdlib modules the source imports, yanked pins.
    let venv_for_knowledge = probes
        .venv
        .as_ref()
        .filter(|v| !v.kind.is_pixi())
        .map(|v| (v.path.as_path(), v.python));
    // A constraint edit is checked with uv before it is offered.
    let resolver = probes
        .uv
        .as_ref()
        .map(|u| crate::core::resolution::UvResolver {
            uv: u.path.as_path(),
            workspace,
            python: env.venv_python.or(target_python),
        });
    findings.extend(
        crate::core::knowledge::assess(
            workspace,
            &project,
            compat.as_ref(),
            venv_for_knowledge,
            target_python,
            source,
            resolver.as_ref(),
        )
        .await,
    );

    drop_explained_conflict(&mut findings, compat.as_ref());

    // pixi: PyPI dependencies that exclude the selected environment's
    // Python, named with the `pixi add python=…` that fixes them
    // (core::pixi_compat), plus the other environments and the other
    // declared platforms (core::pixi_envs).
    if let Some(pixi) = crate::core::pixi::detect(workspace) {
        let pixi_findings = crate::core::pixi_envs::assess(&pixi, workspace, source).await;
        merge_pixi_findings(&mut findings, pixi_findings);
    }

    // Wheel tags cannot see two pins that exclude each other, or a
    // transitive clash: resolve the requirements as written with uv for the
    // Python they will run on, and name the one edit that fixes a failure
    // (core::resolution). Also turns "could not resolve" into "did you
    // mean", and an OS-only package into an environment marker.
    let resolve_python = env.venv_python.or(target_python);
    crate::core::resolution::diagnose_workspace(
        workspace,
        &project,
        compat.as_ref(),
        probes.uv.as_ref().map(|u| u.path.as_path()),
        resolve_python,
        target_python,
        source,
        &mut findings,
    )
    .await;

    Assessment {
        probes,
        project,
        compat,
        target_python,
        findings,
    }
}

/// Title of the finding for dependencies with no common Python.
const CONFLICT_TITLE: &str = "Dependencies conflict on Python version";

/// The conflict detail when no pair is to blame: name the packages that
/// support no Python at all here (the `typing` backport, which declares
/// `<3.5`), so the finding never leaves the reader guessing.
fn no_python_detail(compat: &CompatReport) -> String {
    let none: Vec<String> = compat
        .per_package
        .iter()
        .filter(|p| p.supported.is_empty())
        .map(|p| match &p.requires_python {
            Some(rp) => format!("{} {} (requires-python `{rp}`)", p.name, p.version),
            None => format!("{} {}", p.name, p.version),
        })
        .collect();
    if none.is_empty() {
        "The project's dependencies have no common supported Python version.".to_string()
    } else {
        format!(
            "{} supports no Python 3 release PyPilot knows, so the dependencies have no common Python. Remove or replace it.",
            none.join(", ")
        )
    }
}

/// Drop the generic "no common Python" finding when the distributions a
/// known-breakage finding removes are what empties the intersection: the
/// `typing` backport (C s6c) gets one finding, the one that fixes it.
fn drop_explained_conflict(findings: &mut Vec<Finding>, compat: Option<&CompatReport>) {
    let Some(compat) = compat else {
        return;
    };
    let removed: std::collections::HashSet<String> = findings
        .iter()
        .filter_map(|f| match &f.fix {
            FixKind::ReplaceDist(r) => Some(r.uninstall.iter().chain(&r.drop)),
            _ => None,
        })
        .flatten()
        .map(|n| crate::core::project::normalize_name(n))
        .collect();
    if removed.is_empty() || !findings.iter().any(|f| f.title == CONFLICT_TITLE) {
        return;
    }
    let rest = compat
        .per_package
        .iter()
        .filter(|p| !removed.contains(&crate::core::project::normalize_name(&p.name)))
        .fold(PyVersionSet::universe(), |acc, p| {
            acc.intersect(&p.supported)
        });
    if !rest.is_empty() {
        findings.retain(|f| f.title != CONFLICT_TITLE);
    }
}

/// "Python 3.13[ is not supported by this project]".
const UNSUPPORTED_SUFFIX: &str = " is not supported by this project";

/// Fold the pixi findings ([`crate::core::pixi_compat`]) into the generic
/// ones. Both see a pixi project's PyPI dependencies, so both report an
/// environment Python those dependencies exclude, and dependencies with no
/// common Python. Where they describe the same problem the pixi wording,
/// which names the `pixi add python=…` that fixes it, takes the generic
/// finding's place, and the generic fix stays so the editor still offers to
/// move the project. Every other pixi finding is added as is.
pub fn merge_pixi_findings(findings: &mut Vec<Finding>, pixi: Vec<Finding>) {
    for p in pixi {
        match findings.iter_mut().find(|g| same_problem(g, &p)) {
            Some(g) => {
                g.title = p.title;
                g.detail = p.detail;
                if p.fix != FixKind::Manual {
                    g.fix = p.fix;
                }
            }
            None => findings.push(p),
        }
    }
}

/// Do a generic finding and a pixi one report the same problem?
fn same_problem(generic: &Finding, pixi: &Finding) -> bool {
    if generic.title == CONFLICT_TITLE {
        return pixi.title == crate::core::pixi_compat::CONFLICT_TITLE;
    }
    generic
        .title
        .strip_prefix("Python ")
        .and_then(|rest| rest.strip_suffix(UNSUPPORTED_SUFFIX))
        .is_some_and(|py| crate::core::pixi_compat::is_mismatch_title(&pixi.title, py))
}

/// Pure decision logic: given a flattened environment, the project, and the
/// compatibility report, produce the target interpreter and the findings.
pub fn synthesize(
    env: &EnvSummary,
    project: &ProjectDeps,
    compat: Option<&CompatReport>,
) -> (Option<PyVersion>, Vec<Finding>) {
    let mut findings = Vec::new();
    // A pixi project's environment is pixi's prefix, and every fix goes
    // through pixi (see `crate::core::pixi_manage`), so its findings say so.
    let pixi = project.is_pixi();

    // A venv whose Python is gone can't run anything, so every other verdict
    // about it (its version, its kernel) is moot. Say that one thing, ahead of
    // the Python-project gate: the `.venv` alone is reason enough.
    let broken = env.venv_missing_python.as_ref().filter(|_| env.has_venv);
    if let Some(missing) = broken.filter(|_| pixi) {
        findings.push(Finding {
            severity: Severity::Error,
            title: format!(
                "The pixi environment is broken: its Python at {} is missing",
                missing.display()
            ),
            detail: "The environment's interpreter is gone (deleted, or the install was interrupted), so nothing can run in it. Reinstall it with `pixi install`.".to_string(),
            fix: FixKind::SetupEnvironment,
        });
    } else if let Some(missing) = broken {
        findings.push(broken_venv_finding(env, missing));
    }

    // Notebooks run on whatever kernel Jupyter can find, and an environment
    // without ipykernel is not one. Checked before the Python-project gate: a
    // `.venv` next to notebooks is reason enough, even with no manifest or
    // `.py` file. With no venv at all, the "no environment" finding covers it.
    if env.has_venv && broken.is_none() && env.has_notebooks && !env.venv_has_ipykernel {
        findings.push(Finding {
            severity: Severity::Warning,
            title: if pixi {
                "Notebooks can't use the pixi environment yet".to_string()
            } else {
                "Notebooks can't use .venv yet".to_string()
            },
            detail: "This project has Jupyter notebooks, but its environment has no usable ipykernel (on Python 3.14, ipykernel 7.0.0 and releases before 6.30.1 cannot connect), so VS Code and Jupyter can't offer it as a kernel and fall back to another Python where the project's packages are missing. Set up the environment to install ipykernel.".to_string(),
            // The environment exists: installing into it is enough.
            fix: FixKind::SyncEnvironment,
        });
    }

    // Not a Python project at all → stay silent (bar the notebook kernel above).
    if !project.is_python_project() {
        return (None, findings);
    }

    // A venv directory that lost its pyvenv.cfg is not "no environment":
    // it has to be removed before anything can be built there.
    let damaged = env.damaged_venv.as_ref().filter(|_| !env.has_venv && !pixi);
    if let Some(dir) = damaged {
        let label = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| ".venv".into());
        findings.push(Finding {
            severity: Severity::Error,
            title: format!("The {label} is damaged: its pyvenv.cfg is missing"),
            detail: format!(
                "{label} still holds an interpreter and packages, but without pyvenv.cfg Python no longer treats it as a virtual environment: it runs without the project's packages, and uv refuses to reuse the directory. Remove it and rebuild it{}.",
                env.python_pin
                    .map(|p| format!(" on Python {p} (from .python-version)"))
                    .unwrap_or_default()
            ),
            fix: FixKind::RepairEnvironment {
                python: env.python_pin,
            },
        });
    }
    let has_env_dir = env.has_venv || damaged.is_some();

    // `requires-python` that no Python satisfies: nothing can be built, and
    // every recommendation would contradict the project. pixi's own spec is
    // judged by the pixi checks.
    let declared_rp = project
        .declared_requires_python
        .as_deref()
        .filter(|_| !pixi)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(problem) = declared_rp.and_then(requires_python_problem) {
        findings.push(problem);
        return (None, findings);
    }
    let rp_set = declared_rp
        .map(crate::pypi::pyversion::parse_requires_python)
        .unwrap_or_else(PyVersionSet::universe);

    // Conda projects: detect + offer migration, don't fight conda in v1.
    // pixi already manages a pixi project's conda packages.
    if project.has_conda_env && !pixi {
        findings.push(Finding {
            severity: Severity::Info,
            title: "Conda environment detected".to_string(),
            detail: "environment.yml found. Run `pypilot migrate-conda` to generate a pyproject.toml from it; PyPilot won't manage conda directly.".to_string(),
            fix: FixKind::MigrateConda,
        });
    }

    let Some(compat) = compat else {
        // Python files but no declared dependencies to check.
        // With no dependencies the only Python worth keeping is one the
        // project already chose: its pin, or the venv's.
        let mut keep = None;
        if broken.is_none() && !pixi {
            let existing = [env.python_pin, env.venv_python]
                .into_iter()
                .flatten()
                .find(|p| rp_set.contains(*p));
            let target = existing.or_else(|| rp_set.max());
            match version_findings(env, declared_rp, &rp_set, &rp_set, target, None) {
                Some(f) => {
                    findings.push(f);
                    keep = target;
                }
                None => keep = existing,
            }
        }
        if !env.has_venv && pixi {
            findings.push(pixi_not_installed(
                "This pixi project's environment has not been installed yet, so nothing in it can run. Run `pixi install` to create it.".to_string(),
            ));
        } else if !has_env_dir {
            findings.push(Finding {
                severity: Severity::Warning,
                title: "No virtual environment".to_string(),
                detail: "This project has Python files but no .venv. Create one to isolate its dependencies.".to_string(),
                fix: FixKind::SetupEnvironment,
            });
        }
        return (keep, findings);
    };

    // A lookup that failed for another reason (offline, a proxy) says
    // nothing about the package: one note, no "typo".
    if !compat.unreachable.is_empty() {
        let names: Vec<String> = compat
            .unreachable
            .iter()
            .map(|(n, _)| format!("`{n}`"))
            .collect();
        let first = &compat.unreachable[0].1;
        findings.push(Finding {
            severity: Severity::Info,
            title: "Could not reach PyPI for some dependencies".to_string(),
            detail: format!(
                "{} could not be checked ({first}), so the supported range leaves them out. Run doctor again when PyPI is reachable.",
                names.join(", ")
            ),
            fix: FixKind::Manual,
        });
    }

    // Surface unresolved dependency names (typos, private packages).
    for (name, err) in &compat.unresolved {
        findings.push(Finding {
            severity: Severity::Warning,
            title: format!("Could not resolve `{name}`"),
            detail: format!(
                "PyPI lookup failed for `{name}` ({err}). It may be a typo or a private package."
            ),
            fix: FixKind::Manual,
        });
    }

    // sdist-only compile warnings, with the toolchain advice for this OS.
    // Skipped where a specific finding (a CUDA extension) already says it.
    for p in &compat.per_package {
        if env
            .explained_source_builds
            .contains(&crate::core::project::normalize_name(&p.name))
        {
            continue;
        }
        if let Some(kind) = p.sdist_kind() {
            findings.push(sdist_finding(p, kind, compat, env.toolchain));
        }
    }

    // Releases with wheels for other platforms only and no sdist: no Python
    // version helps, so say what is actually wrong rather than reporting a
    // version conflict.
    let here = compat.platform.label();
    for p in compat.uninstallable() {
        let mut detail = p.no_files_message(here).unwrap_or_default();
        detail.push_str(&format!(
            " Pin a release of {} that publishes files for {here}.",
            p.name
        ));
        findings.push(Finding {
            severity: Severity::Error,
            title: format!("{} {} cannot be installed on {here}", p.name, p.version),
            detail,
            fix: FixKind::Manual,
        });
    }

    // Empty intersection → hard conflict; can't be auto-fixed by choosing a version.
    if compat.intersection.is_empty() {
        if compat.conflicts.is_empty() && compat.uninstallable().next().is_some() {
            return (None, findings);
        }
        let detail = compat
            .conflicts
            .first()
            .map(|c| c.explanation.clone())
            .unwrap_or_else(|| no_python_detail(compat));
        findings.push(Finding {
            severity: Severity::Error,
            title: CONFLICT_TITLE.to_string(),
            detail,
            fix: FixKind::Manual,
        });
        return (None, findings);
    }

    // What the project itself allows on top of what the dependencies do.
    let allowed = compat.intersection.intersect(&rp_set);
    if allowed.is_empty() && !pixi {
        findings.push(Finding {
            severity: Severity::Error,
            title: format!(
                "requires-python `{}` excludes every Python the dependencies support",
                declared_rp.unwrap_or_default()
            ),
            detail: format!(
                "The dependencies support Python {}{}, but pyproject.toml's requires-python allows none of those, so no environment can satisfy both. Widen requires-python or change the dependency that caps it.",
                compat.intersection.to_range_string(),
                capping_clause(compat)
            ),
            fix: FixKind::Manual,
        });
        return (None, findings);
    }

    // An existing choice wins over "newest": the pinned Python, then the one
    // the environment already runs, as long as the project allows it.
    let healthy_venv = env.venv_python.filter(|_| broken.is_none());
    let target = if pixi {
        pick_target(project, compat)
    } else {
        choose(&allowed, &[env.python_pin, healthy_venv])
    };

    // A rebuild must land on a Python the project allows.
    for f in &mut findings {
        if let FixKind::RepairEnvironment { python } = &mut f.fix {
            if !python.is_some_and(|p| allowed.contains(p)) {
                *python = target;
            }
        }
    }

    // The broken-venv finding already asks for a rebuild; judging the dead
    // interpreter's version would only add noise.
    if broken.is_some() {
        return (target, findings);
    }

    // Determine the interpreter currently in effect: the venv's, else nothing.
    match env.venv_python {
        // Venv exists and the dependencies support it: still check it
        // against the project's own requires-python and pin.
        Some(current) if compat.is_compatible(current) => {
            if !pixi {
                if let Some(f) =
                    version_findings(env, declared_rp, &rp_set, &allowed, target, Some(compat))
                {
                    findings.push(f);
                }
            }
        }

        // Venv exists but its Python is unsupported → recreate on the right version.
        Some(current) => {
            let mut why = incompat_reason(compat, current, target);
            if let (true, Some(t)) = (pixi, target) {
                why = why.replace(
                    &format!("Recreate the environment with Python {t}."),
                    &format!(
                        "Move the project to it with `pixi add python={t}` (then `pixi install`)."
                    ),
                );
            }
            why.push_str(&wheel_gap_note(compat, current, target, env.toolchain));
            findings.push(Finding {
                severity: Severity::Error,
                title: format!("Python {current}{UNSUPPORTED_SUFFIX}"),
                detail: why,
                fix: target
                    .map(FixKind::RecreateWithPython)
                    .unwrap_or(FixKind::Manual),
            });
        }

        // A pixi environment without a Python: the manifest never asked for
        // one, so `pixi install` alone will not add it.
        None if pixi && env.has_venv => {
            let range = compat.intersection.to_range_string();
            let ask = target
                .map(|t| format!(" Add one with `pixi add python={t}`."))
                .unwrap_or_default();
            findings.push(Finding {
                severity: Severity::Warning,
                title: "The pixi environment has no Python".to_string(),
                detail: format!(
                    "The project's PyPI dependencies need Python {range}, but its pixi manifest does not ask for Python.{ask}"
                ),
                fix: target
                    .map(FixKind::RecreateWithPython)
                    .unwrap_or(FixKind::Manual),
            });
        }

        None if pixi => {
            let range = compat.intersection.to_range_string();
            findings.push(pixi_not_installed(format!(
                "This pixi project's environment has not been installed yet. Its PyPI dependencies support Python {range}. Run `pixi install` to create it."
            )));
        }

        // The damaged-venv finding already asks for the rebuild.
        None if damaged.is_some() => {}

        // No venv (or unknown version) → set one up on the correct interpreter.
        None => {
            let range = compat.intersection.to_range_string();
            let why = match target {
                Some(t) if env.python_pin == Some(t) => format!(
                    "Dependencies support Python {range}. It will use Python {t}, pinned in .python-version."
                ),
                Some(t) => format!(
                    "Dependencies support Python {range}. Recommended: {t}{}.",
                    capping_clause(compat)
                ),
                None => format!("Dependencies support Python {range}."),
            };
            findings.push(Finding {
                severity: Severity::Warning,
                title: "No environment set up yet".to_string(),
                detail: why,
                fix: FixKind::SetupEnvironment,
            });
        }
    }

    (target, findings)
}

/// The first of `prefer` that `allowed` contains, else its newest.
fn choose(allowed: &PyVersionSet, prefer: &[Option<PyVersion>]) -> Option<PyVersion> {
    prefer
        .iter()
        .flatten()
        .find(|p| allowed.contains(**p))
        .copied()
        .or_else(|| allowed.max())
}

/// How text names the venv: its directory name (`.venv`, `venv`, `env`).
fn venv_label(env: &EnvSummary) -> String {
    env.venv_path
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".venv".into())
}

/// The finding for a venv whose interpreter is gone. When the missing file
/// is the venv's own `python`, the base Python is fine and only the
/// environment needs rebuilding, so the wording says that rather than
/// blaming a removed install.
fn broken_venv_finding(env: &EnvSummary, missing: &Path) -> Finding {
    let label = venv_label(env);
    let own = env
        .venv_path
        .as_ref()
        .is_some_and(|venv| missing.starts_with(venv));
    let python = env.venv_python.or(env.python_pin);
    let on = python
        .map(|p| format!(" on Python {p}, the version it was made with"))
        .unwrap_or_default();
    let (title, detail) = if own {
        (
            format!(
                "The {label} is broken: its own interpreter {} is missing",
                missing.display()
            ),
            format!(
                "The environment's python executable was deleted (by hand, a cleanup tool or an interrupted install), so nothing can run in it. The Python it was built from is not affected. Rebuild the environment in place{on}."
            ),
        )
    } else {
        (
            format!(
                "The {label} is broken: its Python at {} no longer exists",
                missing.display()
            ),
            format!(
                "The interpreter this environment was built on was moved or removed (a uv-managed Python upgraded or uninstalled, a system Python removed, or the project copied from another machine), so every command run through it fails. Rebuild the environment{on} (uv downloads it again if needed)."
            ),
        )
    };
    Finding {
        severity: Severity::Error,
        title,
        detail,
        fix: FixKind::RepairEnvironment { python },
    }
}

/// An Error finding when `spec` (a project's `requires-python`) matches no
/// Python at all: an empty range (`>=3.10,<=3.9`), or only versions newer
/// than any CPython release (`>=3.16`).
pub fn requires_python_problem(spec: &str) -> Option<Finding> {
    use crate::pypi::pyversion::{parse_requires_python, MAX_MINOR};
    if !parse_requires_python(spec).is_empty() {
        return None;
    }
    let newest = PyVersion::py3(MAX_MINOR);
    let future = spec.split(',').any(|clause| {
        let c = clause.trim();
        let lower = c.starts_with('>') || c.starts_with("==") || c.starts_with("~=");
        lower
            && PyVersion::parse(c.trim_start_matches(['>', '=', '~', ' ']))
                .is_some_and(|v| v.major == 3 && v.minor > MAX_MINOR)
    });
    let (title, detail) = if future {
        (
            format!("requires-python `{spec}` asks for a Python that does not exist yet"),
            format!(
                "The newest CPython PyPilot knows is {newest}, and `{spec}` excludes it, so no interpreter can be installed for this project. Lower requires-python (for example `>=3.9`); PyPilot builds nothing until it does."
            ),
        )
    } else {
        (
            format!("requires-python `{spec}` matches no Python version"),
            "Its bounds contradict each other, so no interpreter satisfies it and uv refuses the project. Fix requires-python in pyproject.toml (often only the lower bound is meant, such as `>=3.10`); PyPilot builds nothing until it does.".to_string(),
        )
    };
    Some(Finding {
        severity: Severity::Error,
        title,
        detail,
        fix: FixKind::Manual,
    })
}

/// A healthy venv's Python against the project's own choices: outside its
/// `requires-python` (Error), or not the version `.python-version` pins
/// (Warning). `allowed` is what the project and its dependencies allow;
/// `target` the Python a rebuild would use.
fn version_findings(
    env: &EnvSummary,
    declared_rp: Option<&str>,
    rp_set: &PyVersionSet,
    allowed: &PyVersionSet,
    target: Option<PyVersion>,
    compat: Option<&CompatReport>,
) -> Option<Finding> {
    let current = env.venv_python?;
    let label = venv_label(env);
    if !rp_set.contains(current) {
        let spec = declared_rp.unwrap_or_default();
        let then = match target {
            Some(t) => format!(
                " Recreate the environment with Python {t}, the newest that requires-python and the dependencies allow{}.",
                compat.map(capping_clause).unwrap_or_default()
            ),
            None => String::new(),
        };
        return Some(Finding {
            severity: Severity::Error,
            title: format!("Python {current} is outside the project's requires-python `{spec}`"),
            detail: format!(
                "pyproject.toml says the project runs on `{spec}`, but {label} is on Python {current}, so uv will not install the project there and the code may use features this Python lacks.{then}"
            ),
            fix: target
                .map(FixKind::RecreateWithPython)
                .unwrap_or(FixKind::Manual),
        });
    }
    let pin = env.python_pin?;
    if pin == current {
        return None;
    }
    if allowed.contains(pin) {
        return Some(Finding {
            severity: Severity::Warning,
            title: format!("{label} runs Python {current}, but .python-version pins {pin}"),
            detail: format!(
                "The project pins Python {pin}, so `uv sync` and `uv run` would rebuild {label} on it, and tools that read the pin disagree with the environment. Recreate the environment on Python {pin}; the pin stays as it is."
            ),
            fix: FixKind::RecreateWithPython(pin),
        });
    }
    Some(Finding {
        severity: Severity::Warning,
        title: format!(".python-version pins Python {pin}, which this project cannot use"),
        detail: format!(
            "requires-python and the dependencies allow Python {}, not {pin}, so uv commands that follow the pin fail. Point the pin at the environment's Python with `uv python pin {current}`.",
            allowed.to_range_string()
        ),
        fix: FixKind::Manual,
    })
}

/// The Pythons a pixi project's `python` spec allows, when it is a pixi
/// project whose spec narrows Python at all.
fn pixi_python_window(project: &ProjectDeps) -> Option<PyVersionSet> {
    if !project.is_pixi() {
        return None;
    }
    crate::core::pixi::python_spec_set(project.pixi_python.as_deref()?)
}

/// The interpreter to aim for: the newest every dependency supports. In a
/// pixi project it must also be one the manifest's `python` spec allows,
/// since that is what pixi installs; when the spec excludes every supported
/// Python, the dependencies' choice stands and the project has to move to it
/// (`pixi add python=X.Y`, see [`recommendation`]).
fn pick_target(project: &ProjectDeps, compat: &CompatReport) -> Option<PyVersion> {
    match pixi_python_window(project) {
        Some(window) => compat
            .intersection
            .intersect(&window)
            .max()
            .or_else(|| compat.suggested_python()),
        None => compat.suggested_python(),
    }
}

/// The recommended-Python line of a report (doctor, the LSP report), for the
/// `target` [`synthesize`] chose. In a pixi project it is measured against
/// the manifest's `python` spec as written, and says so when that spec rules
/// out every Python the dependencies support.
pub fn recommendation(
    project: &ProjectDeps,
    compat: &CompatReport,
    target: Option<PyVersion>,
) -> Option<String> {
    let target = target?;
    let (Some(window), Some(spec)) = (pixi_python_window(project), project.pixi_python.as_deref())
    else {
        return Some(format!("Python {target}"));
    };
    if window.contains(target) {
        Some(format!(
            "Python {target} (newest the dependencies support within pixi's `python {spec}`)"
        ))
    } else {
        Some(format!(
            "none within pixi's `python {spec}`: the dependencies support Python {}. Run `pixi add python={target}` to move the project to Python {target}",
            compat.intersection.to_range_string()
        ))
    }
}

/// The finding for a pixi project whose environment pixi has not built yet.
fn pixi_not_installed(detail: String) -> Finding {
    Finding {
        severity: Severity::Warning,
        title: "Pixi environment not installed yet".to_string(),
        detail,
        fix: FixKind::SetupEnvironment,
    }
}

/// Would installing the project compile anything from source? True for any
/// sdist-only dependency, and for one whose wheels skip the venv's Python.
fn needs_source_build(compat: &CompatReport, venv_python: Option<PyVersion>) -> bool {
    compat
        .per_package
        .iter()
        .any(|p| p.sdist_only || venv_python.is_some_and(|py| p.lacks_wheel_for(py)))
}

fn os_name(os: Os) -> &'static str {
    match os {
        Os::Windows => "Windows",
        Os::Macos => "macOS",
        Os::Linux => "Linux",
    }
}

/// The "Will compile from source" finding for one sdist-only dependency.
///
/// No other Python helps here (no wheel exists for this platform at all), so
/// the fix is the toolchain. When the needed tools are already installed the
/// finding drops to Info: the build should just work.
fn sdist_finding(
    p: &PackageAnalysis,
    kind: SdistKind,
    compat: &CompatReport,
    toolchain: Option<Toolchain>,
) -> Finding {
    let os = compat.platform.os;
    let advice = toolchain::build_advice(&p.name, os, toolchain);
    let what = match kind {
        SdistKind::Native => format!(
            "{} {} ships prebuilt wheels for other platforms but none for {}, so installing it compiles native code from source.",
            p.name,
            p.version,
            os_name(os)
        ),
        SdistKind::Unknown => format!(
            "{} {} publishes only a source distribution, so it builds from source; if it has C/C++/Rust extensions that needs a compiler.",
            p.name, p.version
        ),
    };
    let (severity, detail) = match advice {
        Some(a) => (
            Severity::Warning,
            format!("{what} If the build fails (\"Failed building wheel\", \"Microsoft Visual C++ 14.0 or greater is required\"): {a}."),
        ),
        None => (
            Severity::Info,
            format!("{what} The build tools it needs are installed."),
        ),
    };
    Finding {
        severity,
        title: "Will compile from source".to_string(),
        detail,
        fix: FixKind::Manual,
    }
}

/// Extra explanation when the venv's Python is excluded only because some
/// dependency has no wheel for it: pip would fall back to a source build that
/// needs a compiler, and switching to `target` (which has wheels) avoids it.
fn wheel_gap_note(
    compat: &CompatReport,
    current: PyVersion,
    target: Option<PyVersion>,
    toolchain: Option<Toolchain>,
) -> String {
    let gaps: Vec<&PackageAnalysis> = compat
        .per_package
        .iter()
        .filter(|p| p.lacks_wheel_for(current))
        .collect();
    if gaps.is_empty() {
        return String::new();
    }
    let names: Vec<&str> = gaps.iter().map(|p| p.name.as_str()).collect();
    let mut note = format!(
        " {} {} no prebuilt wheel for Python {current} on {}, so installing on it means compiling from source",
        names.join(", "),
        if gaps.len() == 1 { "has" } else { "have" },
        os_name(compat.platform.os)
    );
    if let Some(t) = target {
        note.push_str(&format!("; Python {t} has wheels and needs no compiler"));
    }
    note.push('.');
    // Advice for staying on this Python anyway.
    let advice: Vec<String> = gaps
        .iter()
        .filter_map(|p| toolchain::build_advice(&p.name, compat.platform.os, toolchain))
        .collect();
    if let Some(a) = advice.iter().max_by_key(|a| a.len()) {
        note.push_str(&format!(" To keep Python {current}: {a}."));
    }
    note
}

/// Explain why the current interpreter is unsupported, naming the blockers.
fn incompat_reason(compat: &CompatReport, current: PyVersion, target: Option<PyVersion>) -> String {
    let blockers = compat.blockers_for(current);
    let names: Vec<String> = blockers
        .iter()
        .map(|p| {
            format!(
                "{} {} (supports {})",
                p.name,
                p.version,
                p.supported.to_range_string()
            )
        })
        .collect();
    let who = if names.is_empty() {
        "some dependencies".to_string()
    } else {
        names.join(", ")
    };
    // One package "does not", several (or "some dependencies") "do not".
    let verb = if names.len() == 1 { "does" } else { "do" };
    match target {
        Some(t) => format!(
            "{who} {verb} not support Python {current}. Recreate the environment with Python {t}."
        ),
        None => format!("{who} {verb} not support Python {current}."),
    }
}

/// "(constrained by X, which tops out at 3.Y)" for the recommendation message.
fn capping_clause(compat: &CompatReport) -> String {
    let Some(cap) = compat.intersection.max() else {
        return String::new();
    };
    // The package whose upper bound equals the intersection's cap is the limiter.
    let limiter = compat
        .per_package
        .iter()
        .filter(|p| p.supported.max() == Some(cap))
        .min_by_key(|p| p.supported.len());
    match limiter {
        Some(p) => format!(
            " (upper bound set by {}, which supports {})",
            p.name,
            p.supported.to_range_string()
        ),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_current_uv_lock_pins_the_releases_judged() {
        let dir = std::env::temp_dir().join(format!("pypilot-lock-fit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("pyproject.toml"),
            "[project]\nname = \"demo\"\nversion = \"0.1.0\"\nrequires-python = \">=3.9\"\ndependencies = [\"six\"]\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("uv.lock"),
            "version = 1\nrequires-python = \">=3.9\"\n\n[[package]]\nname = \"demo\"\nversion = \"0.1.0\"\nsource = { virtual = \".\" }\n\n[package.metadata]\nrequires-dist = [{ name = \"six\" }]\n\n[[package]]\nname = \"six\"\nversion = \"1.17.0\"\nsource = { registry = \"https://pypi.org/simple\" }\n",
        )
        .unwrap();
        let project = crate::core::project::scan(&dir);
        let uv = Settings::default();
        let locked = current_lock_versions(&dir, &uv, &project).unwrap();
        assert_eq!(locked.get("six").map(String::as_str), Some("1.17.0"));

        // pip mode installs requirements, not the lock.
        let pip = Settings {
            package_manager: crate::settings::PackageManager::Pip,
            ..Settings::default()
        };
        assert!(current_lock_versions(&dir, &pip, &project).is_none());

        // A dependency the lock does not know yet: the sync re-resolves.
        std::fs::write(
            dir.join("pyproject.toml"),
            "[project]\nname = \"demo\"\nversion = \"0.1.0\"\nrequires-python = \">=3.9\"\ndependencies = [\"six\", \"attrs\"]\n",
        )
        .unwrap();
        let project = crate::core::project::scan(&dir);
        assert!(current_lock_versions(&dir, &uv, &project).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn analysis(name: &str, minors: &[u8]) -> PackageAnalysis {
        PackageAnalysis {
            name: name.into(),
            version: "1.0".into(),
            supported: PyVersionSet::from_versions(minors.iter().map(|&m| PyVersion::py3(m))),
            has_platform_wheels: true,
            sdist_only: false,
            has_foreign_wheels: false,
            no_files_for_platform: false,
            wheel_platforms: Vec::new(),
            requires_python: None,
        }
    }

    fn report(pkgs: Vec<PackageAnalysis>) -> CompatReport {
        let intersection = pkgs
            .iter()
            .map(|p| p.supported.clone())
            .reduce(|a, b| a.intersect(&b))
            .unwrap_or_else(PyVersionSet::universe);
        CompatReport {
            platform: Platform::current(),
            per_package: pkgs,
            unresolved: vec![],
            unreachable: Vec::new(),
            intersection,
            conflicts: vec![],
            sdist_warnings: vec![],
        }
    }

    fn python_project() -> ProjectDeps {
        ProjectDeps {
            sources: vec!["requirements.txt".into()],
            packages: vec![Requirement::any("mediapipe")],
            ..Default::default()
        }
    }

    #[test]
    fn the_unsupported_python_reason_agrees_with_the_number_of_packages() {
        // A s4: one package "does not", two "do not".
        let one = report(vec![analysis("numpy", &[8, 9, 10, 11])]);
        let why = incompat_reason(&one, PyVersion::py3(12), Some(PyVersion::py3(11)));
        assert!(
            why.contains("numpy 1.0 (supports 3.8–3.11) does not support Python 3.12."),
            "{why}"
        );
        let two = report(vec![
            analysis("numpy", &[8, 9, 10, 11]),
            analysis("pandas", &[9, 10, 11]),
        ]);
        let why = incompat_reason(&two, PyVersion::py3(12), None);
        assert!(why.contains(") do not support Python 3.12."), "{why}");
    }

    #[test]
    fn a_backport_with_no_python_is_named_and_left_to_its_finding() {
        // C s6c: `typing` declares `<3.5`, so it supports no Python here.
        let mut typing = analysis("typing", &[]);
        typing.version = "3.7.4.3".into();
        typing.requires_python = Some("<3.5".into());
        let compat = report(vec![typing, analysis("requests", &[9, 10, 11, 12])]);
        let detail = no_python_detail(&compat);
        assert!(
            detail.contains("typing 3.7.4.3 (requires-python `<3.5`)"),
            "{detail}"
        );

        let conflict = Finding {
            severity: Severity::Error,
            title: CONFLICT_TITLE.into(),
            detail,
            fix: FixKind::Manual,
        };
        let backport = Finding {
            severity: Severity::Warning,
            title: "typing is an obsolete backport".into(),
            detail: String::new(),
            fix: FixKind::ReplaceDist(crate::core::knowledge::Replacement {
                uninstall: vec!["typing".into()],
                install: Vec::new(),
                drop: vec!["typing".into()],
            }),
        };
        let mut findings = vec![conflict.clone(), backport.clone()];
        drop_explained_conflict(&mut findings, Some(&compat));
        assert_eq!(findings, std::slice::from_ref(&backport));
        // Without the finding that removes it, the conflict stays.
        let mut findings = vec![conflict.clone()];
        drop_explained_conflict(&mut findings, Some(&compat));
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn mediapipe_on_313_recommends_312() {
        // mediapipe supports 3.9–3.12; venv is 3.13.
        let compat = report(vec![analysis("mediapipe", &[9, 10, 11, 12])]);
        let env = EnvSummary {
            has_venv: true,
            venv_python: Some(PyVersion::py3(13)),
            uv_present: true,
            interpreters: vec![PyVersion::py3(13)],
            ..Default::default()
        };
        let (target, findings) = synthesize(&env, &python_project(), Some(&compat));
        assert_eq!(target, Some(PyVersion::py3(12)));
        let f = findings
            .iter()
            .find(|f| f.severity == Severity::Error)
            .unwrap();
        assert_eq!(f.fix, FixKind::RecreateWithPython(PyVersion::py3(12)));
        // The release judged is named: a range's newest may support less.
        assert!(
            f.detail.contains("mediapipe 1.0 (supports 3.9–3.12)"),
            "{}",
            f.detail
        );
        assert!(f.detail.contains("3.13"));
    }

    #[test]
    fn compatible_venv_stays_silent() {
        let compat = report(vec![analysis("numpy", &[9, 10, 11, 12, 13])]);
        let env = EnvSummary {
            has_venv: true,
            venv_python: Some(PyVersion::py3(12)),
            uv_present: true,
            interpreters: vec![PyVersion::py3(12)],
            ..Default::default()
        };
        let (_t, findings) = synthesize(&env, &python_project(), Some(&compat));
        assert!(findings.iter().all(|f| f.severity == Severity::Info));
    }

    #[test]
    fn no_venv_suggests_setup_with_reason() {
        let compat = report(vec![analysis("mediapipe", &[9, 10, 11, 12])]);
        let env = EnvSummary::default();
        let (target, findings) = synthesize(&env, &python_project(), Some(&compat));
        assert_eq!(target, Some(PyVersion::py3(12)));
        let f = &findings[0];
        assert_eq!(f.fix, FixKind::SetupEnvironment);
        assert!(f.detail.contains("mediapipe"));
    }

    #[test]
    fn notebooks_without_ipykernel_suggest_setup() {
        let compat = report(vec![analysis("numpy", &[9, 10, 11, 12, 13])]);
        let env = EnvSummary {
            has_venv: true,
            venv_python: Some(PyVersion::py3(12)),
            has_notebooks: true,
            venv_has_ipykernel: false,
            ..Default::default()
        };
        let (_t, findings) = synthesize(&env, &python_project(), Some(&compat));
        let f = findings
            .iter()
            .find(|f| f.title == "Notebooks can't use .venv yet")
            .unwrap();
        assert_eq!(f.severity, Severity::Warning);
        assert_eq!(f.fix, FixKind::SyncEnvironment);
        assert!(f.detail.contains("ipykernel"));

        // Already a kernel: silent again.
        let ready = EnvSummary {
            venv_has_ipykernel: true,
            ..env.clone()
        };
        let (_t, findings) = synthesize(&ready, &python_project(), Some(&compat));
        assert!(findings.iter().all(|f| f.severity == Severity::Info));

        // No venv: only the existing "no environment" finding, not this one.
        let none = EnvSummary {
            has_notebooks: true,
            ..Default::default()
        };
        let (_t, findings) = synthesize(&none, &python_project(), Some(&compat));
        assert!(findings
            .iter()
            .all(|f| f.title != "Notebooks can't use .venv yet"));
        assert!(findings
            .iter()
            .any(|f| f.title == "No environment set up yet"));
    }

    fn report_on(os: Os, pkgs: Vec<PackageAnalysis>) -> CompatReport {
        CompatReport {
            platform: Platform {
                os,
                arch: crate::core::platform::Arch::X86_64,
            },
            ..report(pkgs)
        }
    }

    fn sdist_pkg(name: &str, foreign_wheels: bool) -> PackageAnalysis {
        PackageAnalysis {
            has_platform_wheels: false,
            sdist_only: true,
            has_foreign_wheels: foreign_wheels,
            ..analysis(name, &[9, 10, 11, 12, 13])
        }
    }

    fn compat_venv(py: u8) -> EnvSummary {
        EnvSummary {
            has_venv: true,
            venv_python: Some(PyVersion::py3(py)),
            uv_present: true,
            interpreters: vec![PyVersion::py3(py)],
            ..Default::default()
        }
    }

    fn requests_project(rp: Option<&str>) -> ProjectDeps {
        ProjectDeps {
            sources: vec!["pyproject.toml".into()],
            packages: vec![Requirement::any("requests")],
            declared_requires_python: rp.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn venv_outside_requires_python_is_an_error_with_a_rebuild_inside_it() {
        let compat = report(vec![analysis("requests", &[10, 11, 12, 13, 14, 15])]);
        let env = compat_venv(15);
        let (target, findings) =
            synthesize(&env, &requests_project(Some(">=3.9,<3.13")), Some(&compat));
        assert_eq!(target, Some(PyVersion::py3(12)));
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Error);
        assert!(f
            .title
            .contains("outside the project's requires-python `>=3.9,<3.13`"));
        assert_eq!(f.fix, FixKind::RecreateWithPython(PyVersion::py3(12)));
    }

    #[test]
    fn pin_wins_over_newest_and_a_venv_off_the_pin_is_reported() {
        let compat = report(vec![analysis("requests", &[10, 11, 12, 13, 14, 15])]);
        let env = EnvSummary {
            python_pin: Some(PyVersion::py3(12)),
            ..compat_venv(13)
        };
        let (target, findings) = synthesize(&env, &requests_project(None), Some(&compat));
        assert_eq!(target, Some(PyVersion::py3(12)));
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Warning);
        assert_eq!(
            f.title,
            ".venv runs Python 3.13, but .python-version pins 3.12"
        );
        assert_eq!(f.fix, FixKind::RecreateWithPython(PyVersion::py3(12)));

        // On the pin: silent, and the target stays the pin, not 3.15.
        let on_pin = EnvSummary {
            python_pin: Some(PyVersion::py3(12)),
            ..compat_venv(12)
        };
        let (target, findings) = synthesize(&on_pin, &requests_project(None), Some(&compat));
        assert_eq!(target, Some(PyVersion::py3(12)));
        assert!(findings.is_empty(), "{findings:?}");

        // No venv: setup uses the pin and says so.
        let none = EnvSummary {
            python_pin: Some(PyVersion::py3(12)),
            ..Default::default()
        };
        let (target, findings) = synthesize(&none, &requests_project(None), Some(&compat));
        assert_eq!(target, Some(PyVersion::py3(12)));
        assert!(findings[0].detail.contains("pinned in .python-version"));
    }

    #[test]
    fn pin_the_dependencies_exclude_is_not_used() {
        let compat = report(vec![analysis("numpy", &[12, 13, 14, 15])]);
        let env = EnvSummary {
            python_pin: Some(PyVersion::py3(8)),
            ..compat_venv(13)
        };
        let (target, findings) = synthesize(&env, &requests_project(None), Some(&compat));
        // The healthy venv's 3.13 is kept rather than jumping to 3.15.
        assert_eq!(target, Some(PyVersion::py3(13)));
        assert!(findings[0]
            .title
            .contains("pins Python 3.8, which this project cannot use"));
        assert_eq!(findings[0].fix, FixKind::Manual);
    }

    #[test]
    fn impossible_requires_python_is_an_error_and_picks_nothing() {
        for (spec, words) in [
            (">=3.16", "does not exist yet"),
            (">=3.10,<=3.9", "matches no Python version"),
        ] {
            let compat = report(vec![analysis("requests", &[10, 11, 12, 13, 14, 15])]);
            let (target, findings) = synthesize(
                &EnvSummary::default(),
                &requests_project(Some(spec)),
                Some(&compat),
            );
            assert_eq!(target, None, "{spec}");
            assert_eq!(findings.len(), 1, "{findings:?}");
            assert_eq!(findings[0].severity, Severity::Error);
            assert!(findings[0].title.contains(spec), "{}", findings[0].title);
            assert!(findings[0].title.contains(words), "{}", findings[0].title);
            assert_eq!(findings[0].fix, FixKind::Manual);
        }
    }

    #[test]
    fn requires_python_excluding_every_supported_python_is_an_error() {
        let compat = report(vec![analysis("numpy", &[12, 13, 14, 15])]);
        let (target, findings) = synthesize(
            &EnvSummary::default(),
            &requests_project(Some("<3.11")),
            Some(&compat),
        );
        assert_eq!(target, None);
        assert!(findings[0]
            .title
            .contains("excludes every Python the dependencies support"));
    }

    #[test]
    fn venv_without_pyvenv_cfg_is_damaged_not_missing() {
        let compat = report(vec![analysis("requests", &[10, 11, 12, 13, 14, 15])]);
        let env = EnvSummary {
            damaged_venv: Some(PathBuf::from("/ws/.venv")),
            python_pin: Some(PyVersion::py3(12)),
            ..Default::default()
        };
        let (_t, findings) = synthesize(&env, &requests_project(None), Some(&compat));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(
            findings[0].title,
            "The .venv is damaged: its pyvenv.cfg is missing"
        );
        assert_eq!(
            findings[0].fix,
            FixKind::RepairEnvironment {
                python: Some(PyVersion::py3(12))
            }
        );
        // Without dependencies too: no "No virtual environment" beside it.
        let (_t, findings) = synthesize(&env, &requests_project(None), None);
        assert_eq!(findings.len(), 1, "{findings:?}");
    }

    #[test]
    fn no_dependencies_keeps_the_existing_python() {
        let env = EnvSummary {
            python_pin: Some(PyVersion::py3(12)),
            ..compat_venv(12)
        };
        let project = ProjectDeps {
            sources: vec!["main.py".into()],
            ..Default::default()
        };
        let (target, findings) = synthesize(&env, &project, None);
        assert_eq!(target, Some(PyVersion::py3(12)));
        assert!(findings.is_empty());
        let (target, _) = synthesize(&EnvSummary::default(), &project, None);
        assert_eq!(target, None);
    }

    fn sdist_findings(findings: &[Finding]) -> Vec<&Finding> {
        findings
            .iter()
            .filter(|f| f.title == "Will compile from source")
            .collect()
    }

    #[test]
    fn native_sdist_on_windows_names_build_tools() {
        let compat = report_on(Os::Windows, vec![sdist_pkg("nativelib", true)]);
        let (_t, findings) = synthesize(&compat_venv(12), &python_project(), Some(&compat));
        let f = sdist_findings(&findings)[0];
        assert_eq!(f.severity, Severity::Warning);
        assert!(f
            .detail
            .contains("wheels for other platforms but none for Windows"));
        assert!(f.detail.contains("Microsoft C++ Build Tools"));
        assert!(f.detail.contains("Desktop development with C++"));
        assert_eq!(f.fix, FixKind::Manual);
    }

    #[test]
    fn unknown_sdist_hedges_and_uses_os_advice() {
        let compat = report_on(Os::Macos, vec![sdist_pkg("oldlib", false)]);
        let (_t, findings) = synthesize(&compat_venv(12), &python_project(), Some(&compat));
        let f = sdist_findings(&findings)[0];
        assert!(f.detail.contains("only a source distribution"));
        assert!(f.detail.contains("xcode-select --install"));

        let compat = report_on(Os::Linux, vec![sdist_pkg("tokenizers", true)]);
        let (_t, findings) = synthesize(&compat_venv(12), &python_project(), Some(&compat));
        let f = sdist_findings(&findings)[0];
        assert!(f.detail.contains("build-essential"));
        assert!(f.detail.contains("rustup"));
    }

    #[test]
    fn installed_toolchain_downgrades_sdist_finding() {
        let compat = report_on(Os::Windows, vec![sdist_pkg("nativelib", true)]);
        let env = EnvSummary {
            toolchain: Some(Toolchain {
                c_compiler: true,
                rust: false,
            }),
            ..compat_venv(12)
        };
        let (_t, findings) = synthesize(&env, &python_project(), Some(&compat));
        let f = sdist_findings(&findings)[0];
        assert_eq!(f.severity, Severity::Info);
        assert!(!f.detail.contains("Build Tools"));
        assert!(findings.iter().all(|f| f.severity == Severity::Info));
    }

    #[test]
    fn cuda_extension_finding_replaces_the_generic_source_build_one() {
        // What `assess` does: the CUDA-extension check runs first and names
        // the packages it explained, and synthesize skips those.
        let windows = Platform {
            os: Os::Windows,
            arch: crate::core::platform::Arch::X86_64,
        };
        let table = matrix::cuda_ext::ExtensionTable::load(matrix::CUDA_EXTENSIONS_JSON).unwrap();
        let declared = vec![Requirement::any("flash_attn")];
        let issues = matrix::cuda_ext::assess_project(&table, &declared, windows, &[]);
        let env = EnvSummary {
            explained_source_builds: issues
                .iter()
                .filter(|i| i.no_wheel)
                .map(|i| crate::core::project::normalize_name(&i.package))
                .collect(),
            ..compat_venv(12)
        };
        let compat = report_on(
            Os::Windows,
            vec![sdist_pkg("flash-attn", false), sdist_pkg("oldlib", false)],
        );
        let (_t, mut findings) = synthesize(&env, &python_project(), Some(&compat));
        findings.extend(issues.iter().map(matrix::cuda_ext::Issue::to_finding));

        let about_flash_attn: Vec<&Finding> = findings
            .iter()
            .filter(|f| f.title.contains("from source") && f.detail.contains("flash-attn"))
            .collect();
        assert_eq!(about_flash_attn.len(), 1, "{findings:#?}");
        assert!(about_flash_attn[0].title.starts_with("flash-attn"));
        assert!(about_flash_attn[0].detail.contains("nvcc"));

        // A plain sdist-only package still gets the generic finding.
        let generic = sdist_findings(&findings);
        assert_eq!(generic.len(), 1);
        assert!(generic[0].detail.contains("oldlib"));
    }

    #[test]
    fn wheel_gap_recommends_python_with_wheels() {
        // Wheels for 3.9-3.12 only, requires_python allows 3.13: on a 3.13
        // venv pip would compile it, so recommend 3.12 and explain why.
        let gap = PackageAnalysis {
            requires_python: Some(">=3.9".into()),
            ..analysis("mediapipe", &[9, 10, 11, 12])
        };
        let compat = report_on(Os::Windows, vec![gap]);
        let (target, findings) = synthesize(&compat_venv(13), &python_project(), Some(&compat));
        assert_eq!(target, Some(PyVersion::py3(12)));
        let f = findings
            .iter()
            .find(|f| f.severity == Severity::Error)
            .unwrap();
        assert_eq!(f.fix, FixKind::RecreateWithPython(PyVersion::py3(12)));
        assert!(f
            .detail
            .contains("no prebuilt wheel for Python 3.13 on Windows"));
        assert!(f.detail.contains("Python 3.12 has wheels"));
        assert!(f.detail.contains("Microsoft C++ Build Tools"));
        assert!(needs_source_build(&compat, Some(PyVersion::py3(13))));
        assert!(!needs_source_build(&compat, Some(PyVersion::py3(12))));
    }

    #[test]
    fn requires_python_exclusion_is_not_a_wheel_gap() {
        let capped = PackageAnalysis {
            requires_python: Some(">=3.9,<3.13".into()),
            ..analysis("capped", &[9, 10, 11, 12])
        };
        let compat = report_on(Os::Linux, vec![capped]);
        let (_t, findings) = synthesize(&compat_venv(13), &python_project(), Some(&compat));
        let f = findings
            .iter()
            .find(|f| f.severity == Severity::Error)
            .unwrap();
        assert!(!f.detail.contains("compiling from source"));
    }

    #[test]
    fn broken_venv_is_one_clear_rebuild_finding() {
        // Its recorded Python (3.13) is unsupported and it has no kernel, but
        // the only thing worth saying is that it can't run at all.
        let compat = report(vec![analysis("mediapipe", &[9, 10, 11, 12])]);
        let missing = PathBuf::from("/opt/uv/python/cpython-3.13.0/bin/python3.13");
        let env = EnvSummary {
            has_venv: true,
            venv_python: Some(PyVersion::py3(13)),
            has_notebooks: true,
            venv_missing_python: Some(missing.clone()),
            ..Default::default()
        };
        let (target, findings) = synthesize(&env, &python_project(), Some(&compat));
        assert_eq!(target, Some(PyVersion::py3(12)));
        assert_eq!(findings.len(), 1, "{findings:?}");
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Error);
        // Rebuilt in place, on a Python the dependencies support rather than
        // the 3.13 it had.
        assert_eq!(
            f.fix,
            FixKind::RepairEnvironment {
                python: Some(PyVersion::py3(12))
            }
        );
        assert!(f.title.contains("broken"));
        assert!(f.title.contains(&missing.display().to_string()));
        assert!(f.detail.contains("moved or removed"));

        // Reported even outside a recognised Python project, keeping its
        // version when nothing rules it out.
        let (_t, findings) = synthesize(&env, &ProjectDeps::default(), None);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].fix,
            FixKind::RepairEnvironment {
                python: Some(PyVersion::py3(13))
            }
        );

        // Its own python.exe deleted: the base Python is not blamed.
        let own = EnvSummary {
            venv_path: Some(PathBuf::from("/ws/.venv")),
            venv_missing_python: Some(PathBuf::from("/ws/.venv/bin/python")),
            ..env.clone()
        };
        let (_t, findings) = synthesize(&own, &ProjectDeps::default(), None);
        assert!(
            findings[0].title.contains("its own interpreter"),
            "{}",
            findings[0].title
        );
        assert!(findings[0].detail.contains("not affected"));
    }

    fn pixi_project() -> ProjectDeps {
        ProjectDeps {
            sources: vec!["pixi.toml".into()],
            packages: vec![Requirement::any("mediapipe")],
            pixi_manifest: Some("pixi.toml".into()),
            ..Default::default()
        }
    }

    #[test]
    fn uninstalled_pixi_env_gets_the_pixi_wording() {
        let compat = report(vec![analysis("mediapipe", &[9, 10, 11, 12])]);
        let (target, findings) = synthesize(&EnvSummary::default(), &pixi_project(), Some(&compat));
        assert_eq!(target, Some(PyVersion::py3(12)));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].title, "Pixi environment not installed yet");
        assert_eq!(findings[0].severity, Severity::Warning);
        assert_eq!(findings[0].fix, FixKind::SetupEnvironment);
        assert!(findings[0].detail.contains("pixi install"));

        // No dependencies to check: same finding, never "No virtual environment".
        let project = ProjectDeps {
            packages: Vec::new(),
            ..pixi_project()
        };
        let (_, findings) = synthesize(&EnvSummary::default(), &project, None);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].title, "Pixi environment not installed yet");
    }

    #[test]
    fn installed_pixi_env_is_judged_like_a_venv() {
        let compat = report(vec![analysis("mediapipe", &[9, 10, 11, 12])]);
        let ok = EnvSummary {
            has_venv: true,
            venv_python: Some(PyVersion::py3(12)),
            ..Default::default()
        };
        let (_, findings) = synthesize(&ok, &pixi_project(), Some(&compat));
        assert!(findings.is_empty(), "{findings:?}");

        let wrong = EnvSummary {
            venv_python: Some(PyVersion::py3(13)),
            ..ok.clone()
        };
        let (_, findings) = synthesize(&wrong, &pixi_project(), Some(&compat));
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].fix,
            FixKind::RecreateWithPython(PyVersion::py3(12))
        );
        assert!(
            findings[0].detail.contains("`pixi add python=3.12`"),
            "{}",
            findings[0].detail
        );

        let no_python = EnvSummary {
            venv_python: None,
            ..ok
        };
        let (_, findings) = synthesize(&no_python, &pixi_project(), Some(&compat));
        assert_eq!(findings[0].title, "The pixi environment has no Python");
        assert_eq!(
            findings[0].fix,
            FixKind::RecreateWithPython(PyVersion::py3(12))
        );
    }

    fn pinned_pixi_project(spec: &str) -> ProjectDeps {
        ProjectDeps {
            packages: vec![Requirement::any("requests")],
            pixi_python: Some(spec.to_string()),
            ..pixi_project()
        }
    }

    #[test]
    fn pixi_recommendation_stays_inside_the_python_spec() {
        // requests runs on every tracked Python, but pixi installs 3.12.
        let compat = report(vec![analysis("requests", &[9, 10, 11, 12, 13, 14])]);
        let env = EnvSummary {
            has_venv: true,
            venv_python: Some(PyVersion::py3(12)),
            ..Default::default()
        };
        let project = pinned_pixi_project("3.12.*");
        let (target, findings) = synthesize(&env, &project, Some(&compat));
        assert_eq!(target, Some(PyVersion::py3(12)));
        assert!(findings.is_empty(), "{findings:?}");
        assert_eq!(
            recommendation(&project, &compat, target).as_deref(),
            Some("Python 3.12 (newest the dependencies support within pixi's `python 3.12.*`)")
        );

        // A range: the newest both allow.
        let project = pinned_pixi_project(">=3.10,<3.14");
        let (target, _) = synthesize(&env, &project, Some(&compat));
        assert_eq!(target, Some(PyVersion::py3(13)));

        // No spec, or not pixi: the dependencies alone decide.
        let (target, _) = synthesize(&env, &pixi_project(), Some(&compat));
        assert_eq!(target, Some(PyVersion::py3(14)));
        assert_eq!(
            recommendation(&pixi_project(), &compat, target).as_deref(),
            Some("Python 3.14")
        );
    }

    #[test]
    fn pixi_spec_excluding_every_supported_python_says_so() {
        let compat = report(vec![analysis("tensorflow", &[9, 10, 11])]);
        let project = pinned_pixi_project("3.12.*");
        let (target, _) = synthesize(&EnvSummary::default(), &project, Some(&compat));
        assert_eq!(target, Some(PyVersion::py3(11)));
        assert_eq!(
            recommendation(&project, &compat, target).as_deref(),
            Some("none within pixi's `python 3.12.*`: the dependencies support Python 3.9–3.11. Run `pixi add python=3.11` to move the project to Python 3.11")
        );
    }

    fn parsed_pixi(python: &str, pypi: &str) -> crate::core::pixi::PixiProject {
        crate::core::pixi::parse_manifest(
            &format!(
                "[workspace]\nname = \"p\"\n\n[dependencies]\npython = \"{python}\"\n\n[pypi-dependencies]\n{pypi}\n"
            ),
            crate::core::pixi::ManifestFormat::PixiToml,
            Path::new("/proj"),
            PathBuf::from("/proj/pixi.toml"),
        )
        .unwrap()
    }

    #[test]
    fn pixi_python_mismatch_is_reported_once_in_pixi_words() {
        // The pixi environment runs 3.12; tensorflow has wheels up to 3.11.
        let compat = report(vec![analysis("tensorflow", &[9, 10, 11])]);
        let env = EnvSummary {
            has_venv: true,
            venv_python: Some(PyVersion::py3(12)),
            ..Default::default()
        };
        let project = ProjectDeps {
            packages: vec![Requirement::any("tensorflow")],
            ..pinned_pixi_project("3.12.*")
        };
        let (_, mut findings) = synthesize(&env, &project, Some(&compat));
        let pixi = crate::core::pixi_compat::pixi_findings(
            &parsed_pixi("3.12.*", "tensorflow = \"*\""),
            Some(PyVersion::py3(12)),
            &compat,
        );
        assert_eq!((findings.len(), pixi.len()), (1, 1));
        merge_pixi_findings(&mut findings, pixi);
        assert_eq!(findings.len(), 1, "{findings:#?}");
        let f = &findings[0];
        assert_eq!(
            f.title,
            "tensorflow does not support Python 3.12 in this pixi environment"
        );
        assert!(
            f.detail.contains("Run `pixi add python=3.11`"),
            "{}",
            f.detail
        );
        assert!(!f.detail.contains("Recreate"));
        // The editor's rebuild action survives the rewording.
        assert_eq!(f.fix, FixKind::RecreateWithPython(PyVersion::py3(11)));
    }

    #[test]
    fn pixi_dependency_conflict_is_reported_once_in_pixi_words() {
        let compat = report(vec![
            analysis("legacy", &[8, 9, 10]),
            analysis("shiny", &[11, 12]),
        ]);
        assert!(compat.intersection.is_empty());
        let (_, mut findings) = synthesize(&EnvSummary::default(), &pixi_project(), Some(&compat));
        let pixi = crate::core::pixi_compat::pixi_findings(
            &parsed_pixi("3.12.*", "legacy = \"*\"\nshiny = \"*\""),
            None,
            &compat,
        );
        merge_pixi_findings(&mut findings, pixi);
        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert_eq!(findings[0].title, crate::core::pixi_compat::CONFLICT_TITLE);
    }

    #[test]
    fn unrelated_pixi_findings_are_kept() {
        // Before `pixi install`: "not installed" and the manifest's mismatch
        // are two problems, so both stay.
        let compat = report(vec![analysis("tensorflow", &[9, 10, 11])]);
        let project = pinned_pixi_project("3.12.*");
        let (_, mut findings) = synthesize(&EnvSummary::default(), &project, Some(&compat));
        let pixi = crate::core::pixi_compat::pixi_findings(
            &parsed_pixi("3.12.*", "tensorflow = \"*\""),
            None,
            &compat,
        );
        merge_pixi_findings(&mut findings, pixi);
        let titles: Vec<&str> = findings.iter().map(|f| f.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "Pixi environment not installed yet",
                "tensorflow does not support Python 3.12 in this pixi environment"
            ]
        );
    }

    #[test]
    fn broken_pixi_env_asks_for_pixi_install() {
        let env = EnvSummary {
            has_venv: true,
            venv_missing_python: Some(PathBuf::from("proj/.pixi/envs/default/python.exe")),
            ..Default::default()
        };
        let (_, findings) = synthesize(&env, &pixi_project(), None);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0]
            .title
            .starts_with("The pixi environment is broken"));
        assert!(findings[0].detail.contains("pixi install"));
    }

    /// End to end: a pixi project whose environment pixi installed gets no
    /// "no environment" finding, and the probe reports the pixi prefix.
    #[tokio::test]
    async fn assess_finds_the_pixi_environment() {
        let (root, prefix) = crate::core::env::tests::pixi_fixture("assess", true);
        std::fs::write(
            root.join("pixi.toml"),
            "[workspace]\nchannels = [\"conda-forge\"]\nplatforms = [\"win-64\"]\n\n[dependencies]\npython = \"3.12.*\"\nnumpy = \"*\"\n",
        )
        .unwrap();
        std::fs::write(root.join("main.py"), "import numpy\n").unwrap();
        let settings = crate::settings::Settings {
            data_refresh_days: 0,
            ..Default::default()
        };
        let a = assess(&root, &settings, &crate::pypi::FixtureSource::new()).await;
        let venv = a.probes.venv.as_ref().expect("the pixi env is found");
        assert_eq!(venv.path, prefix);
        assert!(venv.kind.is_pixi());
        assert_eq!(venv.python, Some(PyVersion::py3(12)));
        assert!(a.project.is_pixi());
        assert!(a.compat.is_none(), "conda deps never go to PyPI");
        assert!(
            a.findings
                .iter()
                .all(|f| !f.title.contains("environment") && !f.title.contains("virtual")),
            "{:?}",
            a.findings
        );

        // Before `pixi install`: the pixi wording, not the venv one.
        std::fs::remove_dir_all(root.join(".pixi")).unwrap();
        let a = assess(&root, &settings, &crate::pypi::FixtureSource::new()).await;
        assert!(a.probes.venv.is_none());
        let titles: Vec<&str> = a.findings.iter().map(|f| f.title.as_str()).collect();
        assert!(
            titles.contains(&"Pixi environment not installed yet"),
            "{titles:?}"
        );
        assert!(!titles.contains(&"No virtual environment"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// pixi also manages C++ and Rust workspaces: one with no `python`, no
    /// PyPI dependency and no Python file gets no pixi environment warning.
    #[tokio::test]
    async fn non_python_pixi_workspace_is_silent() {
        let root = std::env::temp_dir().join(format!("pypilot-pixi-cpp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("pixi.toml"),
            "[workspace]\nchannels = [\"conda-forge\"]\nplatforms = [\"win-64\", \"linux-64\"]\n\n[dependencies]\ncmake = \">=3.28\"\ncxx-compiler = \"*\"\nninja = \"*\"\n\n[tasks]\nbuild = \"cmake --build build\"\n",
        )
        .unwrap();
        std::fs::write(root.join("main.cpp"), "int main() { return 0; }\n").unwrap();
        let settings = crate::settings::Settings {
            data_refresh_days: 0,
            ..Default::default()
        };
        let a = assess(&root, &settings, &crate::pypi::FixtureSource::new()).await;
        assert!(a.project.is_pixi());
        assert!(!a.project.is_python_project());
        assert!(a.findings.is_empty(), "{:?}", a.findings);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn not_a_python_project_is_silent() {
        let (target, findings) = synthesize(&EnvSummary::default(), &ProjectDeps::default(), None);
        assert!(target.is_none());
        assert!(findings.is_empty());
    }

    #[test]
    fn release_without_files_here_is_named_not_called_a_conflict() {
        let mut tf = analysis("tensorflow", &[]);
        tf.version = "2.15.0.post1".into();
        tf.has_platform_wheels = false;
        tf.has_foreign_wheels = true;
        tf.no_files_for_platform = true;
        tf.wheel_platforms = vec!["Linux x64".into()];
        let compat = report_on(Os::Windows, vec![tf, analysis("numpy", &[10, 11, 12])]);
        let (target, findings) = synthesize(&compat_venv(11), &python_project(), Some(&compat));

        assert_eq!(target, None);
        assert_eq!(findings.len(), 1, "{findings:?}");
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Error);
        assert_eq!(
            f.title,
            "tensorflow 2.15.0.post1 cannot be installed on Windows x64"
        );
        assert!(f.detail.starts_with(
            "tensorflow 2.15.0.post1 publishes no files for Windows x64 (only Linux x64 wheels"
        ));
        assert!(!findings.iter().any(|f| f.title.contains("conflict")));
    }

    #[test]
    fn real_conflict_is_still_reported_beside_a_platformless_release() {
        let mut gone = analysis("winonly", &[]);
        gone.no_files_for_platform = true;
        let mut compat = report(vec![
            gone,
            analysis("legacy", &[8, 9, 10]),
            analysis("shiny", &[11, 12]),
        ]);
        compat.conflicts = vec![pypi::ConflictPair {
            low: analysis("legacy", &[8, 9, 10]),
            high: analysis("shiny", &[11, 12]),
            explanation:
                "legacy supports up to Python 3.10 but shiny requires at least Python 3.11".into(),
        }];
        let (_, findings) = synthesize(&compat_venv(11), &python_project(), Some(&compat));
        assert!(findings.iter().any(|f| f.title.starts_with("winonly")));
        assert!(findings
            .iter()
            .any(|f| f.title == "Dependencies conflict on Python version"));
    }

    #[test]
    fn hardware_checks_skip_packages_with_no_files_here() {
        // tensorflow 2.15.0.post1 ships Linux wheels only: on Windows the GPU
        // check must not describe how it "runs CPU-only" when it cannot install.
        let mut tf = analysis("tensorflow", &[]);
        tf.no_files_for_platform = true;
        let compat = report(vec![tf, analysis("numpy", &[12])]);
        let packages = vec![Requirement::any("TensorFlow"), Requirement::any("numpy")];

        let names: Vec<String> = installable_packages(&packages, Some(&compat))
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(names, vec!["numpy"]);
        assert_eq!(installable_packages(&packages, None).len(), 2);
    }
}
