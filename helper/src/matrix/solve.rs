//! Constraint solve: GPU + driver + framework release -> a concrete answer.
//!
//! One function backs two callers. [`crate::matrix::check`] uses the finding
//! for the read-only report and the F5 toast. [`crate::core::install`] uses
//! the index URL to make an actual `torch` install pull the wheel built for
//! the driver that is really in the machine, instead of whatever the default
//! index happens to hand back.

use crate::core::gpu::{self, Accelerator};
use crate::core::platform::{Os, Platform};
use crate::core::project::Requirement;
use crate::core::{Finding, FixKind, Severity};
use crate::matrix::arch::{ArchRequirement, ArchTable};
use crate::matrix::frameworks::{
    parse_cuda_tag, CudaBuild, Framework, FrameworkTable, ReleaseInfo,
};
use crate::matrix::nvidia::{CudaVersion, DriverSupport, DriverTable};
use crate::matrix::refresh;
use crate::pypi::version::{PackageVersion, VersionSpec};
use crate::settings::Settings;

/// What solving one framework dependency produced.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FrameworkSolve {
    /// The release this was solved against: the pin's target, or the newest
    /// curated release when unpinned. `None` only when the package name is
    /// not a framework this module handles.
    pub resolved_version: Option<String>,
    /// Read-only guidance, `None` when there is nothing worth reporting.
    pub finding: Option<Finding>,
    /// `Some(url)` when the install should be pointed at a specific index —
    /// a CUDA build matched to the driver, or the smaller CPU-only index.
    /// `None` means: not a framework this module handles, tensorflow (which
    /// has no alternate index), or a version outside the bundled snapshot.
    pub index_url: Option<String>,
}

/// Solve for `package_name`, given its declared requirement (`None` when
/// unpinned). Probes the GPU itself, so this is the one entry point both
/// [`crate::matrix::check`] and the install path call. Reads the framework and
/// driver tables through [`refresh::read`], so a background-refreshed copy is
/// used automatically once one exists, per F7's `data_refresh_days`.
pub async fn solve_framework(
    package_name: &str,
    requirement: Option<&Requirement>,
    settings: &Settings,
) -> Option<FrameworkSolve> {
    let framework = Framework::from_package_name(package_name)?;
    let frameworks_json = refresh::read(
        "frameworks.json",
        crate::matrix::FRAMEWORKS_JSON,
        settings.data_refresh_days,
    );
    let table = FrameworkTable::load(&frameworks_json)?;
    let accelerator = gpu::detect().await;

    solve_on(
        &table,
        framework,
        requirement,
        accelerator,
        Platform::current().os,
        || {
            refresh::read(
                "nvidia.json",
                crate::matrix::NVIDIA_JSON,
                settings.data_refresh_days,
            )
        },
    )
}

/// [`solve_framework`] for a given accelerator and OS, without probing the
/// machine, so every platform's answer can be checked from any one of them.
/// `nvidia_json` is only read when the accelerator is an NVIDIA GPU.
pub fn solve_on(
    table: &FrameworkTable,
    framework: Framework,
    requirement: Option<&Requirement>,
    accelerator: Accelerator,
    os: Os,
    nvidia_json: impl FnOnce() -> String,
) -> Option<FrameworkSolve> {
    let version = resolve_version(table, framework, requirement)?;

    // Checked before the release lookup: native Windows is CPU-only for every
    // tensorflow past 2.10 no matter what the driver supports, and 2.10 itself
    // predates the bundled table, so neither branch below would say so.
    if let Some(gpu_name) = native_windows_tensorflow_gpu(framework, os, &accelerator) {
        let mut solved = tensorflow_native_windows(table, requirement, &version, gpu_name);
        solved.resolved_version = Some(version);
        return Some(solved);
    }

    let Some(release) = table.release(framework, &version) else {
        return Some(unknown_release(framework, &version));
    };

    let mut solved = match accelerator {
        Accelerator::AppleSilicon => apple_silicon(framework, &version),
        Accelerator::None => no_gpu(release),
        Accelerator::Nvidia {
            name,
            driver_version,
            compute_cap,
        } => {
            let nvidia_json = nvidia_json();
            let driver_table = DriverTable::load(&nvidia_json);
            let support = driver_table
                .as_ref()
                .and_then(|t| t.support_for(&driver_version, os));
            let arch = compute_cap.as_deref().and_then(|cap| {
                arch_context(
                    &nvidia_json,
                    cap,
                    table,
                    driver_table.as_ref(),
                    support.as_ref(),
                    os,
                )
            });
            with_nvidia(release, &name, &driver_version, support, arch.as_ref())
        }
    };
    solved.resolved_version = Some(version);
    Some(solved)
}

/// What the GPU's architecture adds to a torch solve: the requirement itself,
/// plus precomputed answers for "what should I do instead", because a
/// finding that only says "wrong build" without the command to fix it just
/// sends the user back to the forum threads this check exists to replace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchContext {
    pub req: ArchRequirement,
    /// The driver needed to run `req.min_cuda`, when the table knows it.
    pub min_driver: Option<String>,
    /// Newest curated torch release + build that suits both the GPU and the
    /// installed driver.
    pub alternative: Option<(String, CudaBuild)>,
    /// Same, ignoring the driver: what to install once it is updated.
    pub alternative_after_driver_update: Option<(String, CudaBuild)>,
}

fn arch_context(
    nvidia_json: &str,
    compute_cap: &str,
    table: &FrameworkTable,
    driver_table: Option<&DriverTable>,
    support: Option<&DriverSupport>,
    os: Os,
) -> Option<ArchContext> {
    // A refreshed nvidia.json from before the architectures section existed
    // must not switch the check off; the bundled snapshot always has it.
    let arch_table = ArchTable::load(nvidia_json)
        .filter(|t| !t.is_empty())
        .or_else(|| ArchTable::load(crate::matrix::NVIDIA_JSON))?;
    let req = arch_table.requirement_for(compute_cap)?;
    Some(ArchContext {
        min_driver: driver_table.and_then(|t| t.min_driver_for(req.min_cuda, os)),
        alternative: support.and_then(|s| best_torch_build(table, &req, Some(s.max_known))),
        alternative_after_driver_update: best_torch_build(table, &req, None),
        req,
    })
}

/// The newest curated torch release, and its newest CUDA build, that carries
/// kernels for `req` and (when given) does not need a CUDA runtime newer than
/// `driver_max`.
fn best_torch_build(
    table: &FrameworkTable,
    req: &ArchRequirement,
    driver_max: Option<CudaVersion>,
) -> Option<(String, CudaBuild)> {
    let mut versions = table.all_versions(Framework::Torch);
    versions.sort_by_cached_key(|v| PackageVersion::parse(v));
    versions
        .iter()
        .rev()
        .filter(|v| req.accepts_torch(v))
        .find_map(|v| {
            let ReleaseInfo::Torch { cuda_builds, .. } = table.release(Framework::Torch, v)? else {
                return None;
            };
            cuda_builds
                .into_iter()
                .filter(|b| req.accepts_cuda(b.cuda))
                .filter(|b| driver_max.map_or(true, |max| b.cuda <= max))
                .max_by_key(|b| b.cuda)
                .map(|b| (v.clone(), b))
        })
}

/// The version to evaluate: the newest curated release matching a pin, or the
/// newest curated release at all when unpinned. Only versions the bundled
/// table has build data for are candidates, since that data is the entire
/// point of solving.
fn resolve_version(
    table: &FrameworkTable,
    framework: Framework,
    requirement: Option<&Requirement>,
) -> Option<String> {
    if let Some(req) = requirement.filter(|r| r.is_pinned()) {
        let spec = VersionSpec::parse(&req.spec);
        let versions = table.all_versions(framework);
        if let Some(picked) = spec.select_newest(versions.iter().map(|s| s.as_str())) {
            return Some(picked.raw);
        }
        // Pinned to something outside the bundled snapshot: fall through with
        // the spec text so the caller can still name what was asked for,
        // rather than going silent about a version it could not check.
        return Some(
            req.spec
                .trim_start_matches(['=', '~', '>', '<', '!', ' '])
                .to_string(),
        );
    }
    table.latest(framework).map(|s| s.to_string())
}

fn apple_silicon(framework: Framework, version: &str) -> FrameworkSolve {
    let name = framework_display_name(framework);
    FrameworkSolve {
        finding: Some(Finding {
            severity: Severity::Info,
            title: format!("{name} on Apple Silicon uses MPS"),
            detail: format!(
                "Apple Silicon detected. {name} {version} accelerates through Metal (MPS), not CUDA. The default wheel is correct; no special index is needed."
            ),
            fix: FixKind::Manual,
        }),
        index_url: None,
        ..Default::default()
    }
}

fn no_gpu(release: ReleaseInfo) -> FrameworkSolve {
    match release {
        ReleaseInfo::Torch { cpu_index_url, .. } => FrameworkSolve {
            // Silent: this is the "nothing to fix" case. The index is still
            // set, because the default index otherwise risks pulling a much
            // larger CUDA-bundled wheel on a machine that cannot use it.
            finding: None,
            index_url: Some(cpu_index_url),
            ..Default::default()
        },
        ReleaseInfo::TensorFlow { .. } => FrameworkSolve::default(),
    }
}

fn with_nvidia(
    release: ReleaseInfo,
    gpu_name: &str,
    driver_version: &str,
    support: Option<DriverSupport>,
    arch: Option<&ArchContext>,
) -> FrameworkSolve {
    match release {
        ReleaseInfo::Torch {
            version,
            cpu_index_url,
            cuda_builds,
        } => solve_torch(
            &version,
            gpu_name,
            driver_version,
            support,
            arch,
            cpu_index_url,
            cuda_builds,
        ),
        // TensorFlow publishes one wheel per release with no per-CUDA index
        // to steer between, so there is no build choice for the architecture
        // to influence; only the driver check applies.
        ReleaseInfo::TensorFlow { version, min_cuda } => {
            solve_tensorflow(&version, gpu_name, driver_version, support, min_cuda)
        }
    }
}

fn solve_torch(
    version: &str,
    gpu_name: &str,
    driver_version: &str,
    support: Option<DriverSupport>,
    arch: Option<&ArchContext>,
    cpu_index_url: String,
    mut cuda_builds: Vec<CudaBuild>,
) -> FrameworkSolve {
    let Some(support) = support else {
        return FrameworkSolve {
            finding: Some(Finding {
                severity: Severity::Error,
                title: "NVIDIA driver too old for any known CUDA build".to_string(),
                detail: format!(
                    "{gpu_name}'s driver ({driver_version}) is older than every CUDA release PyPilot's data covers. Update the driver, or install the CPU build: pip install torch=={version} --index-url {cpu_index_url}"
                ),
                fix: FixKind::Manual,
            }),
            index_url: Some(cpu_index_url),
            ..Default::default()
        };
    };

    cuda_builds.sort_by_key(|b| b.cuda);
    let fits_arch = |b: &CudaBuild| {
        arch.map_or(true, |a| {
            a.req.accepts_cuda(b.cuda) && a.req.accepts_torch(version)
        })
    };
    let best = cuda_builds
        .iter()
        .rev()
        .find(|b| b.cuda <= support.max_known && fits_arch(b));

    let Some(best) = best else {
        if let Some(arch) = arch {
            // Only an architecture problem if the driver alone would have
            // allowed some build; otherwise the plain driver message below is
            // the accurate one.
            if cuda_builds.iter().any(|b| b.cuda <= support.max_known)
                || support.max_known < arch.req.min_cuda
            {
                return arch_mismatch(
                    version,
                    gpu_name,
                    driver_version,
                    &support,
                    arch,
                    &cuda_builds,
                    cpu_index_url,
                );
            }
        }
        // Driver supports some CUDA, just not enough for anything this torch
        // release ships. Rare (torch's floor is usually generous) but real.
        let oldest = cuda_builds.first();
        let ask = if oldest.is_some() {
            format!(
                " Upgrade the driver, or install an older torch release matching CUDA {}.",
                support.max_known
            )
        } else {
            String::new()
        };
        return FrameworkSolve {
            finding: Some(Finding {
                severity: Severity::Error,
                title: "NVIDIA driver too old for this torch release".to_string(),
                detail: format!(
                    "{gpu_name}'s driver ({driver_version}) supports CUDA up to {}. torch {version} needs at least {}.{ask}",
                    support.max_known,
                    oldest.map(|b| b.cuda.to_string()).unwrap_or_default(),
                ),
                fix: FixKind::Manual,
            }),
            index_url: Some(cpu_index_url),
            ..Default::default()
        };
    };

    let soft_note = if support.exceeds_known_table {
        " (your driver is newer than PyPilot's bundled table; this is a conservative floor, not a ceiling)"
    } else {
        ""
    };
    let arch_note = arch
        .map(|a| {
            format!(
                " {} needs a build against CUDA {}.",
                a.req.describe(gpu_name),
                cuda_range(&a.req)
            )
        })
        .unwrap_or_default();

    FrameworkSolve {
        finding: Some(Finding {
            severity: Severity::Info,
            title: format!("torch build matched to your driver: {}", best.tag),
            detail: format!(
                "{gpu_name}'s driver ({driver_version}) supports CUDA up to {}{soft_note}.{arch_note} Recommended: pip install torch=={version} --index-url {}",
                support.max_known, best.index_url
            ),
            fix: FixKind::Manual,
        }),
        index_url: Some(best.index_url.clone()),
        ..Default::default()
    }
}

/// "12.8 or newer", "11.0 to 12.6": the window of CUDA builds that carry
/// kernels for an architecture, phrased for a message.
fn cuda_range(req: &ArchRequirement) -> String {
    match req.max_cuda {
        Some(max) => format!("{} to {max}", req.min_cuda),
        None => format!("{} or newer", req.min_cuda),
    }
}

/// The text users are actually searching for, quoted so the finding is
/// recognisable as the explanation of the crash they already hit.
const NO_KERNEL_IMAGE: &str =
    "CUDA error: no kernel image is available for execution on the device";

/// No build of this torch release suits both the driver and the GPU
/// architecture. Says exactly which of the two to change and the command to
/// run afterwards. Falls back to the CPU index for an unattended install,
/// matching the other error paths: a CUDA build that cannot launch a single
/// kernel is strictly worse than a CPU build that works.
fn arch_mismatch(
    version: &str,
    gpu_name: &str,
    driver_version: &str,
    support: &DriverSupport,
    arch: &ArchContext,
    cuda_builds: &[CudaBuild],
    cpu_index_url: String,
) -> FrameworkSolve {
    let req = &arch.req;
    let gpu = req.describe(gpu_name);
    let range = cuda_range(req);
    let torch_floor = req
        .min_torch
        .as_deref()
        .map(|t| format!(" (torch {t} or newer)"))
        .unwrap_or_default();

    let (title, detail) = if support.max_known < req.min_cuda {
        let driver = arch
            .min_driver
            .as_deref()
            .map(|d| format!(" to {d} or newer"))
            .unwrap_or_default();
        let then = arch
            .alternative_after_driver_update
            .as_ref()
            .map(|(v, b)| format!(", then: pip install torch=={v} --index-url {}", b.index_url))
            .unwrap_or_default();
        (
            format!("NVIDIA driver too old for {}", req.cap.sm()),
            format!(
                "{gpu} needs a torch build against CUDA {range}{torch_floor}, but its driver ({driver_version}) supports CUDA only up to {}. Any build it can run fails with \"{NO_KERNEL_IMAGE}\". Update the NVIDIA driver{driver}{then}.",
                support.max_known
            ),
        )
    } else {
        let shipped = cuda_builds
            .iter()
            .map(|b| b.tag.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let instead = match &arch.alternative {
            Some((v, b)) => format!(
                " Use torch=={v} instead: pip install torch=={v} --index-url {}",
                b.index_url
            ),
            None => " No curated torch release has a matching build; see pytorch.org/get-started/locally.".to_string(),
        };
        (
            format!("torch {version} has no build for {}", req.cap.sm()),
            format!(
                "{gpu} needs a torch build against CUDA {range}{torch_floor}. torch {version} ships {shipped}, none of which include {} kernels, so CUDA calls fail with \"{NO_KERNEL_IMAGE}\".{instead}",
                req.cap.sm()
            ),
        )
    };

    FrameworkSolve {
        finding: Some(Finding {
            severity: Severity::Error,
            title,
            detail,
            fix: FixKind::Manual,
        }),
        index_url: Some(cpu_index_url),
        ..Default::default()
    }
}

/// Check the torch already installed in a venv against the GPU architecture.
///
/// The declared-dependency solve cannot see this: a project that says
/// `torch>=2.0` is satisfied by a `2.5.1+cu121` that was installed months ago
/// on a different card, and that is exactly the build that crashes on an
/// RTX 50-series. The version comes from the `.dist-info` name, whose local
/// segment (`+cu121`) records the CUDA runtime the wheel was built against.
///
/// `fix` is the sentence telling the user what to run instead; the caller
/// builds it because it depends on the project's pins and the driver.
/// `pixi` says the environment is a pixi prefix rather than a `.venv`.
pub fn installed_torch_finding(
    installed: &str,
    gpu_name: &str,
    req: &ArchRequirement,
    fix: &str,
    pixi: bool,
) -> Option<Finding> {
    let local = installed.split_once('+').map(|(_, l)| l);
    // `+cpu` / `+rocm*` builds never launch CUDA kernels: not this check's
    // concern. No local segment (a plain PyPI wheel) means the CUDA runtime
    // is unknown, so only the torch-version floor can be judged.
    let built_cuda = match local {
        Some(tag) => Some(parse_cuda_tag(tag)?),
        None => None,
    };

    let too_old_torch = !req.accepts_torch(installed);
    let too_old_cuda = built_cuda.is_some_and(|c| c < req.min_cuda);
    let too_new_cuda = built_cuda.is_some_and(|c| req.max_cuda.is_some_and(|max| c > max));
    if !(too_old_torch || too_old_cuda || too_new_cuda) {
        return None;
    }

    let built = built_cuda
        .map(|c| format!(", built against CUDA {c}"))
        .unwrap_or_default();
    let torch_floor = req
        .min_torch
        .as_deref()
        .map(|t| format!(" (torch {t} or newer)"))
        .unwrap_or_default();
    let (severity, consequence) = if too_old_torch || too_old_cuda {
        (
            Severity::Error,
            format!("CUDA calls will fail with \"{NO_KERNEL_IMAGE}\"."),
        )
    } else {
        // Ceilings come from PyTorch trimming old architectures out of newer
        // builds, which has varied by release, so this is a strong hint
        // rather than a certainty.
        (
            Severity::Warning,
            format!("Builds this new have dropped {} kernels and are likely to fail with \"{NO_KERNEL_IMAGE}\".", req.cap.sm()),
        )
    };

    Some(Finding {
        severity,
        title: format!(
            "Installed torch {installed} does not support {}",
            req.cap.sm()
        ),
        detail: format!(
            "{} has torch {installed}{built}. {} needs a build against CUDA {}{torch_floor}. {consequence} {fix}",
            env_subject(pixi),
            req.describe(gpu_name),
            cuda_range(req)
        ),
        fix: FixKind::Manual,
    })
}

/// Run [`installed_torch_finding`] for the venv at `venv`, using `solved` (the
/// project's own torch solve) for the reinstall command when it produced a
/// usable CUDA build, and the newest compatible release otherwise.
pub async fn check_installed_torch(
    env: &crate::core::env::ProjectEnv,
    solved: &FrameworkSolve,
    settings: &Settings,
) -> Option<Finding> {
    let installed = crate::core::installed::scan(&env.path).version_of("torch")?;
    let pixi = env.kind.is_pixi();
    let Accelerator::Nvidia {
        name,
        driver_version,
        compute_cap: Some(cap),
    } = gpu::detect().await
    else {
        return None;
    };

    let nvidia_json = refresh::read(
        "nvidia.json",
        crate::matrix::NVIDIA_JSON,
        settings.data_refresh_days,
    );
    let frameworks_json = refresh::read(
        "frameworks.json",
        crate::matrix::FRAMEWORKS_JSON,
        settings.data_refresh_days,
    );
    let table = FrameworkTable::load(&frameworks_json)?;
    let driver_table = DriverTable::load(&nvidia_json);
    let os = Platform::current().os;
    let support = driver_table
        .as_ref()
        .and_then(|t| t.support_for(&driver_version, os));
    let arch = arch_context(
        &nvidia_json,
        &cap,
        &table,
        driver_table.as_ref(),
        support.as_ref(),
        os,
    )?;

    let fix = reinstall_sentence(solved, &arch, pixi);
    installed_torch_finding(&installed, &name, &arch.req, &fix, pixi)
}

/// The environment as the subject of a sentence.
fn env_subject(pixi: bool) -> &'static str {
    if pixi {
        "The pixi environment"
    } else {
        "The .venv"
    }
}

/// The "what to run" sentence for a wrong installed build. In a pixi
/// environment a pip reinstall would be undone by the next `pixi install`,
/// so the build is pinned in the manifest instead.
fn reinstall_sentence(solved: &FrameworkSolve, arch: &ArchContext, pixi: bool) -> String {
    let solved_ok = solved
        .finding
        .as_ref()
        .map_or(true, |f| f.severity != Severity::Error);
    let from_solve = solved
        .resolved_version
        .as_ref()
        .zip(solved.index_url.as_ref())
        .filter(|(_, url)| solved_ok && !url.ends_with("/cpu"))
        .map(|(v, url)| (v.clone(), url.clone()));
    let pick = from_solve.or_else(|| {
        arch.alternative
            .as_ref()
            .map(|(v, b)| (v.clone(), b.index_url.clone()))
    });
    match pick {
        Some((v, url)) if pixi => format!(
            "Pin the right build in the pixi manifest's PyPI dependencies, `torch = {{ version = \"=={v}\", index = \"{url}\" }}`, then run `pixi install`; a pip reinstall would be undone by the next `pixi install`."
        ),
        Some((v, url)) => {
            format!("Reinstall the right build: pip install --force-reinstall torch=={v} --index-url {url}")
        }
        None => {
            let driver = arch
                .min_driver
                .as_deref()
                .map(|d| format!(" to {d} or newer"))
                .unwrap_or_default();
            let cuda = cuda_range(&arch.req);
            if pixi {
                format!("Update the NVIDIA driver{driver}, then point torch at a CUDA {cuda} build in the pixi manifest and run `pixi install`.")
            } else {
                format!("Update the NVIDIA driver{driver}, then reinstall torch from a CUDA {cuda} build.")
            }
        }
    }
}

fn solve_tensorflow(
    version: &str,
    gpu_name: &str,
    driver_version: &str,
    support: Option<DriverSupport>,
    min_cuda: CudaVersion,
) -> FrameworkSolve {
    let Some(support) = support else {
        return FrameworkSolve {
            finding: Some(Finding {
                severity: Severity::Error,
                title: "NVIDIA driver too old for any known CUDA build".to_string(),
                detail: format!(
                    "{gpu_name}'s driver ({driver_version}) is older than every CUDA release PyPilot's data covers, and tensorflow {version} needs CUDA {min_cuda}. Update the driver."
                ),
                fix: FixKind::Manual,
            }),
            ..Default::default()
        };
    };

    if support.max_known >= min_cuda {
        // Fine — silent, nothing to fix.
        return FrameworkSolve::default();
    }

    let soft_note = if support.exceeds_known_table {
        " (your driver is newer than PyPilot's bundled table; this is a conservative floor)"
    } else {
        ""
    };

    FrameworkSolve {
        finding: Some(Finding {
            severity: Severity::Error,
            title: "NVIDIA driver too old for this tensorflow release".to_string(),
            detail: format!(
                "{gpu_name}'s driver ({driver_version}) supports CUDA up to {}{soft_note}. tensorflow {version} needs CUDA {min_cuda} or newer (minimum driver {}). Update the driver, or pin an older tensorflow release.",
                support.max_known, support.min_driver_for_max
            ),
            fix: FixKind::Manual,
        }),
        ..Default::default()
    }
}

/// The first tensorflow release without GPU support on native Windows. 2.10
/// was the last with it; from 2.11 on, tensorflow.org/install/pip sends
/// Windows GPU users to WSL2, and a native install silently runs on the CPU.
const TF_FIRST_CPU_ONLY_ON_WINDOWS: &str = "2.11";

/// The NVIDIA GPU's name when this is tensorflow on native Windows with an
/// NVIDIA GPU present: the one combination where tensorflow's GPU story
/// depends on the version rather than the driver. Linux, WSL2 (which reports
/// itself as Linux) and GPU-less machines go through the usual solve.
fn native_windows_tensorflow_gpu(
    framework: Framework,
    os: Os,
    accelerator: &Accelerator,
) -> Option<&str> {
    match accelerator {
        Accelerator::Nvidia { name, .. }
            if framework == Framework::TensorFlow && os == Os::Windows =>
        {
            Some(name)
        }
        _ => None,
    }
}

/// Can this requirement install a tensorflow newer than 2.10? Unpinned always
/// can. A pin is tested against the curated releases, the first CPU-only one,
/// and whatever [`resolve_version`] made of it, so a pin newer than the bundled
/// table still counts while a cap like `<2.11` does not.
fn tensorflow_past_native_windows_gpu(
    table: &FrameworkTable,
    requirement: Option<&Requirement>,
    resolved: &str,
) -> bool {
    let Some(req) = requirement.filter(|r| r.is_pinned()) else {
        return true;
    };
    let spec = VersionSpec::parse(&req.spec);
    let Some(first) = PackageVersion::parse(TF_FIRST_CPU_ONLY_ON_WINDOWS) else {
        return true;
    };
    let mut candidates = table.all_versions(Framework::TensorFlow);
    candidates.push(TF_FIRST_CPU_ONLY_ON_WINDOWS.to_string());
    candidates.push(resolved.to_string());
    candidates
        .iter()
        .filter_map(|v| PackageVersion::parse(v))
        .any(|v| v >= first && spec.matches(&v))
}

/// tensorflow with an NVIDIA GPU on native Windows. Past 2.10 there is no GPU
/// build to match the driver to, so the driver checks would only mislead; the
/// useful answer is that the GPU will sit idle and what to do about it.
fn tensorflow_native_windows(
    table: &FrameworkTable,
    requirement: Option<&Requirement>,
    version: &str,
    gpu_name: &str,
) -> FrameworkSolve {
    let finding = if tensorflow_past_native_windows_gpu(table, requirement, version) {
        Finding {
            severity: Severity::Warning,
            title: "tensorflow will run on the CPU on native Windows".to_string(),
            detail: format!(
                "{gpu_name} detected, but tensorflow 2.10 was the last release with GPU support on native Windows; tensorflow {version} installs and runs CPU-only here. To use the GPU: run the project in WSL2 and install tensorflow[and-cuda]; or pin tensorflow<2.11 on Python 3.10 or older with CUDA 11.2 and cuDNN 8.1 (conda install -c conda-forge cudatoolkit=11.2 cudnn=8.1.0); or use PyTorch, which ships CUDA builds for Windows. See tensorflow.org/install/pip."
            ),
            fix: FixKind::Manual,
        }
    } else {
        // Only reachable with a pin, and the pin text is what the user wrote:
        // for a cap like `<2.11` the resolved version would read as 2.11.
        let pinned = requirement.map(|r| r.spec.as_str()).unwrap_or_default();
        Finding {
            severity: Severity::Info,
            title: "tensorflow GPU on native Windows needs CUDA 11.2".to_string(),
            detail: format!(
                "tensorflow{pinned} predates the end of native-Windows GPU support, so it can use {gpu_name}, but its wheel does not bundle CUDA: it needs Python 3.10 or older plus CUDA 11.2 and cuDNN 8.1 (conda install -c conda-forge cudatoolkit=11.2 cudnn=8.1.0). See tensorflow.org/install/pip."
            ),
            fix: FixKind::Manual,
        }
    };
    FrameworkSolve {
        finding: Some(finding),
        index_url: None,
        ..Default::default()
    }
}

fn unknown_release(framework: Framework, version: &str) -> FrameworkSolve {
    let name = framework_display_name(framework);
    FrameworkSolve {
        resolved_version: Some(version.to_string()),
        finding: Some(Finding {
            severity: Severity::Info,
            title: format!("{name} {version} is not in PyPilot's bundled CUDA data"),
            detail: format!(
                "{name} {version} is newer than PyPilot's bundled matrix, so its driver compatibility could not be checked here. See pytorch.org/get-started/locally or tensorflow.org/install for the current index URL."
            ),
            fix: FixKind::Manual,
        }),
        index_url: None,
    }
}

fn framework_display_name(f: Framework) -> &'static str {
    match f {
        Framework::Torch => "torch",
        Framework::TensorFlow => "tensorflow",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> FrameworkTable {
        FrameworkTable::load(crate::matrix::FRAMEWORKS_JSON).unwrap()
    }

    fn torch_2_4() -> (String, String, Vec<CudaBuild>) {
        let ReleaseInfo::Torch {
            version,
            cpu_index_url,
            cuda_builds,
        } = table().release(Framework::Torch, "2.4.0").unwrap()
        else {
            unreachable!()
        };
        (version, cpu_index_url, cuda_builds)
    }

    #[test]
    fn driver_supporting_cu121_but_not_cu124_gets_cu121() {
        // torch 2.4.0 offers cu118/cu121/cu124. A driver capped at CUDA 12.2
        // (real GA floor for 12.2 is 535.54.03) can't run cu124.
        let (version, cpu_index_url, cuda_builds) = torch_2_4();
        let support = DriverSupport {
            max_known: CudaVersion {
                major: 12,
                minor: 2,
            },
            min_driver_for_max: "535.54.03".into(),
            exceeds_known_table: false,
        };
        let solved = solve_torch(
            &version,
            "RTX 3080",
            "535.54.03",
            Some(support),
            None,
            cpu_index_url,
            cuda_builds,
        );
        assert_eq!(
            solved.index_url.as_deref(),
            Some("https://download.pytorch.org/whl/cu121")
        );
        assert_eq!(solved.finding.unwrap().severity, Severity::Info);
    }

    #[test]
    fn driver_supporting_the_newest_build_is_offered_it() {
        let (version, cpu_index_url, cuda_builds) = torch_2_4();
        let support = DriverSupport {
            max_known: CudaVersion {
                major: 12,
                minor: 6,
            },
            min_driver_for_max: "560.28.03".into(),
            exceeds_known_table: false,
        };
        let solved = solve_torch(
            &version,
            "RTX 4090",
            "560.28.03",
            Some(support),
            None,
            cpu_index_url,
            cuda_builds,
        );
        assert_eq!(
            solved.index_url.as_deref(),
            Some("https://download.pytorch.org/whl/cu124")
        );
    }

    #[test]
    fn no_driver_data_falls_back_to_cpu_with_an_error() {
        let (version, cpu_index_url, cuda_builds) = torch_2_4();
        let solved = solve_torch(
            &version,
            "GT 210",
            "195.36.15",
            None,
            None,
            cpu_index_url.clone(),
            cuda_builds,
        );
        assert_eq!(solved.index_url, Some(cpu_index_url));
        assert_eq!(solved.finding.unwrap().severity, Severity::Error);
    }

    #[test]
    fn tensorflow_within_driver_capability_is_silent() {
        let support = DriverSupport {
            max_known: CudaVersion {
                major: 12,
                minor: 5,
            },
            min_driver_for_max: "555.42.02".into(),
            exceeds_known_table: false,
        };
        let solved = solve_tensorflow(
            "2.16.1",
            "RTX 4090",
            "555.42.02",
            Some(support),
            CudaVersion {
                major: 12,
                minor: 3,
            },
        );
        assert!(solved.finding.is_none());
        assert!(solved.index_url.is_none());
    }

    #[test]
    fn tensorflow_needing_newer_cuda_than_the_driver_has_is_an_error() {
        let support = DriverSupport {
            max_known: CudaVersion {
                major: 11,
                minor: 8,
            },
            min_driver_for_max: "520.61.05".into(),
            exceeds_known_table: false,
        };
        let solved = solve_tensorflow(
            "2.16.1",
            "GTX 1080",
            "520.61.05",
            Some(support),
            CudaVersion {
                major: 12,
                minor: 3,
            },
        );
        let f = solved.finding.unwrap();
        assert_eq!(f.severity, Severity::Error);
        assert!(f.detail.contains("12.3"));
    }

    #[test]
    fn soft_note_appears_when_driver_exceeds_the_table() {
        let (version, cpu_index_url, cuda_builds) = torch_2_4();
        let support = DriverSupport {
            max_known: CudaVersion {
                major: 12,
                minor: 9,
            },
            min_driver_for_max: "575.51.03".into(),
            exceeds_known_table: true,
        };
        let solved = solve_torch(
            &version,
            "RTX 5090",
            "999.00",
            Some(support),
            None,
            cpu_index_url,
            cuda_builds,
        );
        assert!(solved
            .finding
            .unwrap()
            .detail
            .contains("conservative floor"));
    }

    fn nvidia() -> Accelerator {
        Accelerator::Nvidia {
            name: "RTX 4070".into(),
            driver_version: "560.94".into(),
            compute_cap: Some("8.9".into()),
        }
    }

    fn pinned(spec: &str) -> Requirement {
        Requirement {
            name: "tensorflow".into(),
            spec: spec.into(),
        }
    }

    #[test]
    fn native_windows_tensorflow_needs_windows_nvidia_and_tensorflow() {
        let gpu = nvidia();
        assert_eq!(
            native_windows_tensorflow_gpu(Framework::TensorFlow, Os::Windows, &gpu),
            Some("RTX 4070")
        );
        // Linux covers WSL2, where tensorflow[and-cuda] does use the GPU.
        assert!(native_windows_tensorflow_gpu(Framework::TensorFlow, Os::Linux, &gpu).is_none());
        assert!(native_windows_tensorflow_gpu(Framework::Torch, Os::Windows, &gpu).is_none());
        assert!(native_windows_tensorflow_gpu(
            Framework::TensorFlow,
            Os::Windows,
            &Accelerator::None
        )
        .is_none());
    }

    #[test]
    fn tensorflow_versions_past_2_10_are_cpu_only_on_windows() {
        let t = table();
        let past = |req: Option<&Requirement>, resolved: &str| {
            tensorflow_past_native_windows_gpu(&t, req, resolved)
        };
        let latest = t.latest(Framework::TensorFlow).unwrap().to_string();

        assert!(past(None, &latest), "unpinned installs the newest release");
        assert!(past(Some(&pinned("==2.16.1")), "2.16.1"));
        assert!(past(Some(&pinned(">=2.10")), &latest));
        assert!(past(Some(&pinned("~=2.10")), &latest));
        assert!(past(Some(&pinned("==2.11.0")), "2.11.0"));
        // Newer than the bundled table still counts.
        assert!(past(Some(&pinned("==2.99.0")), "2.99.0"));

        assert!(!past(Some(&pinned("<2.11")), "2.11"));
        assert!(!past(Some(&pinned("==2.10.1")), "2.10.1"));
        assert!(!past(Some(&pinned("~=2.10.0")), "2.10.0"));
        assert!(!past(Some(&pinned(">=2.9,<=2.10.1")), "2.10.1"));
    }

    #[test]
    fn modern_tensorflow_on_native_windows_warns_with_the_ways_out() {
        let solved = tensorflow_native_windows(&table(), None, "2.21.0", "RTX 4070");
        assert!(solved.index_url.is_none(), "no GPU build to point at");
        let f = solved.finding.unwrap();
        assert_eq!(f.severity, Severity::Warning);
        assert!(f.title.contains("CPU"));
        assert!(f.detail.contains("RTX 4070"));
        assert!(f.detail.contains("WSL2"));
        assert!(f.detail.contains("tensorflow[and-cuda]"));
        assert!(f.detail.contains("tensorflow<2.11"));
        assert!(f.detail.contains("PyTorch"));
        assert!(
            !f.detail.contains("Update the driver"),
            "the driver is not what keeps tensorflow off the GPU here"
        );
    }

    #[test]
    fn legacy_tensorflow_on_native_windows_names_its_cuda_needs() {
        let req = pinned("<2.11");
        let solved = tensorflow_native_windows(&table(), Some(&req), "2.11", "RTX 4070");
        assert!(solved.index_url.is_none());
        let f = solved.finding.unwrap();
        assert_eq!(f.severity, Severity::Info);
        assert!(f.detail.contains("tensorflow<2.11"));
        assert!(f.detail.contains("CUDA 11.2"));
        assert!(f.detail.contains("cuDNN 8.1"));
        assert!(f.detail.contains("Python 3.10"));
    }

    #[tokio::test]
    async fn tensorflow_solve_never_claims_a_gpu_build_on_native_windows() {
        // Whatever the machine has: on Windows with an NVIDIA GPU the finding
        // must be the CPU-only warning, and tensorflow never gets an index.
        let solved = solve_framework("tensorflow", None, &Settings::default())
            .await
            .unwrap();
        assert!(solved.index_url.is_none());
        let windows_nvidia = native_windows_tensorflow_gpu(
            Framework::TensorFlow,
            Platform::current().os,
            &gpu::detect().await,
        )
        .is_some();
        if windows_nvidia {
            assert_eq!(solved.finding.unwrap().severity, Severity::Warning);
        }
    }

    #[tokio::test]
    async fn non_framework_package_is_not_handled() {
        assert!(solve_framework("numpy", None, &Settings::default())
            .await
            .is_none());
    }

    #[tokio::test]
    async fn resolved_version_is_set_on_the_returned_solve() {
        // Whatever the machine's actual accelerator is, an unpinned torch
        // request must resolve to the table's newest curated release.
        let solved = solve_framework("torch", None, &Settings::default())
            .await
            .unwrap();
        let table = table();
        assert_eq!(
            solved.resolved_version.as_deref(),
            table.latest(Framework::Torch)
        );
    }

    fn torch_release(v: &str) -> (String, String, Vec<CudaBuild>) {
        let ReleaseInfo::Torch {
            version,
            cpu_index_url,
            cuda_builds,
        } = table().release(Framework::Torch, v).unwrap()
        else {
            unreachable!()
        };
        (version, cpu_index_url, cuda_builds)
    }

    fn support(major: u32, minor: u32) -> DriverSupport {
        DriverSupport {
            max_known: CudaVersion { major, minor },
            min_driver_for_max: "0".into(),
            exceeds_known_table: false,
        }
    }

    /// The arch context `solve_framework` would build for `cap` on a Windows
    /// machine whose driver supports CUDA up to `driver`.
    fn arch_for(cap: &str, driver: &DriverSupport) -> ArchContext {
        let drivers = DriverTable::load(crate::matrix::NVIDIA_JSON).unwrap();
        arch_context(
            crate::matrix::NVIDIA_JSON,
            cap,
            &table(),
            Some(&drivers),
            Some(driver),
            Os::Windows,
        )
        .unwrap()
    }

    fn solve(v: &str, cap: &str, driver: DriverSupport) -> FrameworkSolve {
        let arch = arch_for(cap, &driver);
        let (version, cpu_index_url, cuda_builds) = torch_release(v);
        solve_torch(
            &version,
            "NVIDIA GeForce RTX",
            "576.02",
            Some(driver),
            Some(&arch),
            cpu_index_url,
            cuda_builds,
        )
    }

    #[test]
    fn blackwell_with_a_new_driver_gets_cu128() {
        let solved = solve("2.7.0", "12.0", support(12, 9));
        assert_eq!(
            solved.index_url.as_deref(),
            Some("https://download.pytorch.org/whl/cu128")
        );
        let f = solved.finding.unwrap();
        assert_eq!(f.severity, Severity::Info);
        assert!(f.detail.contains("sm_120"));
    }

    #[test]
    fn blackwell_pinned_to_an_old_torch_is_an_error_naming_the_fix() {
        // torch 2.5.1 ships cu118/cu121/cu124: the driver could run all of
        // them, none of them has sm_120 kernels. This is the forum case.
        let solved = solve("2.5.1", "12.0", support(12, 9));
        assert!(solved.index_url.unwrap().ends_with("/cpu"));
        let f = solved.finding.unwrap();
        assert_eq!(f.severity, Severity::Error);
        assert!(f.title.contains("no build for sm_120"), "{}", f.title);
        assert!(f.detail.contains("no kernel image is available"));
        assert!(f.detail.contains("torch 2.7.0 or newer"));
        assert!(
            f.detail
                .contains("--index-url https://download.pytorch.org/whl/cu128"),
            "{}",
            f.detail
        );
    }

    #[test]
    fn blackwell_with_a_driver_capped_below_12_8_must_update_the_driver() {
        let solved = solve("2.7.0", "12.0", support(12, 6));
        let f = solved.finding.unwrap();
        assert_eq!(f.severity, Severity::Error);
        assert!(f.title.contains("driver too old for sm_120"), "{}", f.title);
        // Windows floor for CUDA 12.8, then the build to install afterwards.
        assert!(f.detail.contains("570.65 or newer"), "{}", f.detail);
        assert!(f.detail.contains("pip install torch=="));
        assert!(f.detail.contains("/cu1"));
    }

    #[test]
    fn ada_is_unaffected_by_the_architecture_check() {
        let solved = solve("2.4.0", "8.9", support(12, 6));
        assert_eq!(
            solved.index_url.as_deref(),
            Some("https://download.pytorch.org/whl/cu124")
        );
        assert_eq!(solved.finding.unwrap().severity, Severity::Info);
    }

    #[test]
    fn pascal_is_steered_to_the_newest_build_that_still_has_its_kernels() {
        // torch 2.9.0 ships cu126/cu128/cu130; Pascal kernels only reliably
        // survive in cu126.
        let solved = solve("2.9.0", "6.1", support(12, 9));
        assert_eq!(
            solved.index_url.as_deref(),
            Some("https://download.pytorch.org/whl/cu126")
        );
    }

    #[test]
    fn best_build_respects_both_the_driver_and_the_architecture() {
        let arch = arch_for("12.0", &support(12, 9));
        let (version, build) = arch.alternative.unwrap();
        // Newest curated release with a build in [12.8, 12.9].
        assert_eq!(build.tag, "cu128");
        assert!(arch.req.accepts_torch(&version));
        let (_, after_update) = arch.alternative_after_driver_update.unwrap();
        assert!(after_update.cuda >= build.cuda);
    }

    #[test]
    fn a_refreshed_nvidia_json_without_architectures_falls_back_to_bundled() {
        let driver_only = r#"{"drivers": []}"#;
        let ctx = arch_context(driver_only, "12.0", &table(), None, None, Os::Linux).unwrap();
        assert_eq!(
            ctx.req.min_cuda,
            CudaVersion {
                major: 12,
                minor: 8
            }
        );
        assert!(ctx.alternative.is_none(), "no driver support known");
    }

    fn requirement(cap: &str) -> ArchRequirement {
        ArchTable::load(crate::matrix::NVIDIA_JSON)
            .unwrap()
            .requirement_for(cap)
            .unwrap()
    }

    #[test]
    fn installed_cu121_torch_on_blackwell_is_an_error_with_a_reinstall() {
        let arch = arch_for("12.0", &support(12, 9));
        let fix = reinstall_sentence(&FrameworkSolve::default(), &arch, false);
        let f = installed_torch_finding("2.5.1+cu121", "RTX 5090", &arch.req, &fix, false).unwrap();
        assert_eq!(f.severity, Severity::Error);
        assert!(f.detail.contains("built against CUDA 12.1"));
        assert!(
            f.detail.contains("pip install --force-reinstall torch=="),
            "{}",
            f.detail
        );
        assert!(f.detail.contains("/cu128"));
    }

    #[test]
    fn installed_torch_in_a_pixi_environment_is_named_and_fixed_through_pixi() {
        let arch = arch_for("12.0", &support(12, 9));
        let fix = reinstall_sentence(&FrameworkSolve::default(), &arch, true);
        let f = installed_torch_finding("2.5.1+cu121", "RTX 5090", &arch.req, &fix, true).unwrap();
        assert!(
            f.detail
                .starts_with("The pixi environment has torch 2.5.1+cu121"),
            "{}",
            f.detail
        );
        assert!(!f.detail.contains(".venv"), "{}", f.detail);
        assert!(!f.detail.contains("pip install"), "{}", f.detail);
        assert!(f
            .detail
            .contains("index = \"https://download.pytorch.org/whl/cu128\""));
        assert!(f.detail.contains("`pixi install`"));

        let venv =
            installed_torch_finding("2.5.1+cu121", "RTX 5090", &arch.req, "", false).unwrap();
        assert!(venv.detail.starts_with("The .venv has torch 2.5.1+cu121"));

        let mut arch = arch;
        arch.alternative = None;
        let fix = reinstall_sentence(&FrameworkSolve::default(), &arch, true);
        assert!(
            fix.contains("in the pixi manifest and run `pixi install`"),
            "{fix}"
        );
    }

    #[test]
    fn installed_cu126_build_of_a_new_enough_torch_is_still_wrong() {
        // torch 2.7.1 is new enough, but its cu126 build has no sm_120.
        let f = installed_torch_finding(
            "2.7.1+cu126",
            "RTX 5080",
            &requirement("12.0"),
            "fix",
            false,
        );
        assert_eq!(f.unwrap().severity, Severity::Error);
    }

    #[test]
    fn installed_correct_build_is_silent() {
        let req = requirement("12.0");
        assert!(installed_torch_finding("2.7.1+cu128", "RTX 5090", &req, "", false).is_none());
        assert!(installed_torch_finding("2.9.0+cu130", "RTX 5090", &req, "", false).is_none());
        // Ada is happy with anything PyPilot curates.
        let ada = requirement("8.9");
        assert!(installed_torch_finding("2.4.0+cu121", "RTX 4090", &ada, "", false).is_none());
    }

    #[test]
    fn installed_cpu_or_rocm_build_is_not_this_checks_business() {
        let req = requirement("12.0");
        assert!(installed_torch_finding("2.5.1+cpu", "RTX 5090", &req, "", false).is_none());
        assert!(installed_torch_finding("2.5.1+rocm6.2", "RTX 5090", &req, "", false).is_none());
    }

    #[test]
    fn installed_plain_pypi_torch_is_judged_on_version_alone() {
        let req = requirement("12.0");
        assert!(installed_torch_finding("2.5.1", "RTX 5090", &req, "", false).is_some());
        assert!(installed_torch_finding("2.8.0", "RTX 5090", &req, "", false).is_none());
    }

    #[test]
    fn installed_cuda_13_torch_on_pascal_is_a_warning() {
        let pascal = requirement("6.1");
        let f = installed_torch_finding("2.9.0+cu130", "GTX 1080", &pascal, "fix", false).unwrap();
        assert_eq!(f.severity, Severity::Warning);
        assert!(installed_torch_finding("2.9.0+cu126", "GTX 1080", &pascal, "", false).is_none());
    }

    #[test]
    fn reinstall_prefers_the_projects_own_solve() {
        let arch = arch_for("12.0", &support(12, 9));
        let solved = FrameworkSolve {
            resolved_version: Some("2.7.1".into()),
            index_url: Some("https://download.pytorch.org/whl/cu128".into()),
            ..Default::default()
        };
        let fix = reinstall_sentence(&solved, &arch, false);
        assert!(fix.contains("torch==2.7.1 --index-url https://download.pytorch.org/whl/cu128"));
    }

    #[test]
    fn reinstall_without_any_usable_build_says_to_update_the_driver() {
        let mut arch = arch_for("12.0", &support(12, 9));
        arch.alternative = None;
        let fix = reinstall_sentence(&FrameworkSolve::default(), &arch, false);
        assert!(
            fix.contains("Update the NVIDIA driver to 570.65 or newer"),
            "{fix}"
        );
    }
}
