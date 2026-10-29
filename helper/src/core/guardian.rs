//! F4, the live import guardian.
//!
//! Given the text of a Python buffer, work out which imports will fail and why.
//! This runs on a debounce while the user types, so the ordering of the filters
//! matters: stdlib and local modules are rejected on string comparisons alone,
//! installed packages on a directory listing, and only what survives all three
//! reaches PyPI.
//!
//! [`classify`] holds the decision table and takes no I/O, so every branch is
//! testable without a network or a filesystem.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use futures::future::join_all;

use crate::core::imports::{self, ImportRef};
use crate::core::installed::Installed;
use crate::core::modules;
use crate::core::platform::Platform;
use crate::core::stdlib;
use crate::matrix;
use crate::matrix::cuda_ext::{self, ExtensionTable};
use crate::pypi::metadata::PackageAnalysis;
use crate::pypi::pyversion::{PyVersion, PyVersionSet};
use crate::pypi::MetadataSource;

/// Why an import will not work right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// The interpreter in use can run it; it just isn't installed.
    NotInstalled,
    /// A venv exists, but its Python has no wheel for this package.
    IncompatibleInterpreter {
        current: PyVersion,
        supported: PyVersionSet,
        target: Option<PyVersion>,
        /// Packages already in the venv, so the fix can be honest about scope.
        reinstall_count: usize,
    },
    /// No virtualenv yet. The target comes from every import in the file, so the
    /// first environment is built on a version that suits all of them.
    NoEnvironment { target: Option<PyVersion> },
    /// PyPI has no such project. Usually a typo, sometimes a private index.
    NotOnPyPi,
    /// The release publishes nothing installable on this platform: wheels for
    /// other platforms only and no sdist. No interpreter fixes that.
    NoFilesForPlatform {
        /// Which platforms it does ship for, and what that means here.
        message: String,
    },
    /// The package is installed, but the attribute this file uses is not in it.
    ///
    /// This is the case metadata cannot see. mediapipe 0.10.35 installs on every
    /// CPython 3.x and then raises on `mp.solutions`, because that release
    /// dropped the legacy API. Reading the package on disk catches it at edit
    /// time rather than at run time.
    AttributeMissing {
        /// The attribute the buffer reaches for, e.g. `solutions`.
        attribute: String,
        /// Version of the package that is actually installed.
        installed_version: Option<String>,
        /// What the package does expose, to point at the replacement.
        available: Vec<String>,
    },
    /// A CUDA extension (flash-attn, xformers, ...) that a plain install
    /// would get wrong here: no wheel for this platform, a source build that
    /// needs torch, or a release built for a different torch than the venv's.
    /// Replaces the install offer, since that offer is what would fail.
    CudaExtension {
        /// It cannot work on this machine at all.
        blocking: bool,
        /// Why, and what to do instead.
        message: String,
    },
}

/// One actionable import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub import: ImportRef,
    /// The PyPI distribution that provides this import.
    pub package: String,
    /// True when the name came from the bundled table rather than the import
    /// itself. The UI says so, because a rename the user did not write is
    /// exactly the thing they should get to see before installing.
    pub renamed: bool,
    pub problem: Problem,
}

/// Everything [`classify`] needs to know about the environment.
#[derive(Debug, Clone, Default)]
pub struct BufferContext {
    /// Python version of the project venv, or `None` when there is no venv.
    pub venv_python: Option<PyVersion>,
    /// How many distributions are installed in that venv.
    pub installed_count: usize,
    /// Newest interpreter every unsatisfied import in this file agrees on.
    pub file_target: Option<PyVersion>,
    /// The platform wheels were judged against, for naming it in messages.
    pub platform: Option<Platform>,
}

/// The decision table. Pure, so each branch is directly testable.
pub fn classify(
    import: ImportRef,
    package: String,
    renamed: bool,
    analysis: Option<&PackageAnalysis>,
    ctx: &BufferContext,
) -> Finding {
    let problem = match (analysis, ctx.venv_python) {
        // Nothing on PyPI under that name. Never offer to install it.
        (None, _) => Problem::NotOnPyPi,

        // Neither a venv nor a different Python helps: nothing installs here.
        (Some(a), _) if a.no_files_for_platform => Problem::NoFilesForPlatform {
            message: a
                .no_files_message(ctx.platform.map_or("this platform", |p| p.label()))
                .unwrap_or_default(),
        },

        // No venv yet: the fix builds one, sized for the whole file.
        (Some(_), None) => Problem::NoEnvironment {
            target: ctx.file_target,
        },

        (Some(a), Some(current)) => {
            if a.supported.contains(current) {
                Problem::NotInstalled
            } else {
                Problem::IncompatibleInterpreter {
                    current,
                    supported: a.supported.clone(),
                    target: a.supported.max(),
                    reinstall_count: ctx.installed_count,
                }
            }
        }
    };

    Finding {
        import,
        package,
        renamed,
        problem,
    }
}

/// Analyze a buffer and return everything worth a diagnostic.
///
/// `workspace` is used to recognize the project's own modules, which must never
/// be reported as missing packages. `platform` decides which wheels count, and
/// is a parameter rather than a lookup so the answer is reproducible off the
/// machine that produced it.
pub async fn analyze_buffer<S: MetadataSource>(
    source: &str,
    workspace: &Path,
    platform: Platform,
    venv_python: Option<PyVersion>,
    installed: &Installed,
    metadata: &S,
) -> Vec<Finding> {
    analyze_buffer_in(
        source,
        workspace,
        None,
        platform,
        venv_python,
        installed,
        metadata,
    )
    .await
}

/// [`analyze_buffer`] for a buffer whose file lives in `document_dir`.
///
/// Python puts a script's own directory on `sys.path`, and Jupyter starts a
/// notebook's kernel there, so a `helpers.py` next to the file is local even
/// when it is nowhere near the workspace root.
pub async fn analyze_buffer_in<S: MetadataSource>(
    source: &str,
    workspace: &Path,
    document_dir: Option<&Path>,
    platform: Platform,
    venv_python: Option<PyVersion>,
    installed: &Installed,
    metadata: &S,
) -> Vec<Finding> {
    let local = local_modules(workspace, document_dir);

    // Keep every occurrence for the squiggles, but resolve each package once.
    let candidates: Vec<(ImportRef, String, bool)> = imports::extract(source)
        .into_iter()
        .filter(|i| !stdlib::is_stdlib(&i.module) && !local.contains(&i.module))
        .map(|i| {
            let package = matrix::resolve_package(&i.module);
            let renamed = matrix::is_mapped(&i.module);
            (i, package, renamed)
        })
        .filter(|(_, package, _)| !installed.contains(package))
        .collect();

    if candidates.is_empty() {
        return Vec::new();
    }

    let unique: BTreeSet<String> = candidates.iter().map(|(_, p, _)| p.clone()).collect();

    // `Some(None)`: PyPI has no such project. Absent: the lookup failed for
    // another reason (offline, a proxy, PyPI down), which says nothing about
    // the package, so those imports get no diagnostic rather than a false
    // "check the spelling".
    let fetched = join_all(unique.into_iter().map(|name| async move {
        let analysis = match metadata.fetch(&name).await {
            Ok(meta) => Some(Some(meta.analyze(&platform))),
            Err(e) if crate::pypi::client::is_not_on_pypi(&e) => Some(None),
            Err(_) => None,
        };
        (name, analysis)
    }))
    .await;

    let analyses: HashMap<String, Option<PackageAnalysis>> = fetched
        .into_iter()
        .filter_map(|(name, analysis)| analysis.map(|a| (name, a)))
        .collect();

    // With no venv, the first environment should suit every import in the file,
    // so intersect them all rather than sizing for whichever one is on screen.
    // A package with no files here can't be helped by any interpreter, so it
    // doesn't get a say (it would empty the set for everything else).
    let file_target = analyses
        .values()
        .flatten()
        .filter(|a| !a.no_files_for_platform)
        .map(|a| a.supported.clone())
        .reduce(|acc, s| acc.intersect(&s))
        .and_then(|set| set.max());

    let ctx = BufferContext {
        venv_python,
        installed_count: installed.count(),
        file_target,
        platform: Some(platform),
    };

    let extensions = matrix::cuda_extensions();
    let torch = installed.version_of("torch");

    candidates
        .into_iter()
        .filter(|(_, package, _)| analyses.contains_key(package))
        .map(|(import, package, renamed)| {
            let analysis = analyses.get(&package).and_then(|a| a.as_ref());
            let finding = classify(import, package, renamed, analysis, &ctx);
            flag_cuda_extension(finding, extensions, platform, torch.as_deref())
        })
        .collect()
}

/// Turn an install offer for a CUDA extension into the reason it would go
/// wrong, when there is one. Other problems (an interpreter conflict, a name
/// PyPI does not know) are left alone: they come first either way.
pub fn flag_cuda_extension(
    mut finding: Finding,
    extensions: &ExtensionTable,
    platform: Platform,
    installed_torch: Option<&str>,
) -> Finding {
    if !matches!(
        finding.problem,
        Problem::NotInstalled | Problem::NoEnvironment { .. }
    ) {
        return finding;
    }
    if let Some(issue) =
        cuda_ext::check_import(extensions, &finding.package, platform, installed_torch)
    {
        finding.problem = Problem::CudaExtension {
            blocking: issue.blocking,
            message: issue.detail,
        };
    }
    finding
}

/// Check attribute accesses against what the installed packages actually expose.
///
/// Runs only over imports that are installed, since an uninstalled package has
/// nothing on disk to read. Purely local: a directory listing and one file read
/// per package, no network.
pub fn check_attributes(source: &str, venv: &Path, installed: &Installed) -> Vec<Finding> {
    let mut out = Vec::new();

    for import in imports::extract(source) {
        if stdlib::is_stdlib(&import.module) {
            continue;
        }
        // Only `import x` / `import x as y` bind the package root.
        let Some(binding) = import.binding.clone() else {
            continue;
        };
        let package = matrix::resolve_package(&import.module);
        if !installed.contains(&package) {
            continue;
        }
        let Some(package_dir) = modules::find_package(venv, &import.module) else {
            continue;
        };

        let uses = imports::extract_attr_uses(source, &[binding]);
        if uses.is_empty() {
            continue;
        }

        let installed_version = installed.version_of(&package);
        let mut reported: HashSet<String> = HashSet::new();

        for used in uses {
            if !reported.insert(used.attr.clone()) {
                continue; // one diagnostic per attribute, not per occurrence
            }
            let modules::AttrStatus::Missing { available } =
                modules::attribute_status(&package_dir, &used.attr)
            else {
                continue;
            };

            out.push(Finding {
                import: ImportRef {
                    module: import.module.clone(),
                    binding: import.binding.clone(),
                    line: used.line,
                    start: used.start,
                    end: used.end,
                },
                package: package.clone(),
                renamed: false,
                problem: Problem::AttributeMissing {
                    attribute: used.attr,
                    installed_version: installed_version.clone(),
                    available,
                },
            });
        }
    }

    out
}

/// Module names that belong to this project, so `import utils` next to
/// `utils.py` never gets reported as a missing PyPI package.
fn local_modules(workspace: &Path, document_dir: Option<&Path>) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut roots = vec![workspace.to_path_buf(), workspace.join("src")];
    if let Some(dir) = document_dir {
        if !roots.iter().any(|r| r == dir) {
            roots.push(dir.to_path_buf());
        }
    }
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.join("__init__.py").is_file() {
                    if let Some(n) = path.file_name() {
                        names.insert(n.to_string_lossy().into_owned());
                    }
                }
            } else if path.extension().is_some_and(|e| e == "py") {
                if let Some(stem) = path.file_stem() {
                    names.insert(stem.to_string_lossy().into_owned());
                }
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn import_of(module: &str) -> ImportRef {
        ImportRef {
            module: module.into(),
            binding: Some(module.into()),
            line: 0,
            start: 7,
            end: 7 + module.len() as u32,
        }
    }

    #[test]
    fn compatible_but_missing_offers_a_plain_install() {
        let ctx = BufferContext {
            venv_python: Some(PyVersion::py3(12)),
            installed_count: 3,
            file_target: Some(PyVersion::py3(12)),
            platform: None,
        };
        let f = classify(
            import_of("mediapipe"),
            "mediapipe".into(),
            false,
            Some(&analysis("mediapipe", &[9, 10, 11, 12])),
            &ctx,
        );
        assert_eq!(f.problem, Problem::NotInstalled);
    }

    #[test]
    fn incompatible_interpreter_reports_target_and_reinstall_scope() {
        // The week-two case: 3.13 venv with packages in it, new import caps at 3.12.
        let ctx = BufferContext {
            venv_python: Some(PyVersion::py3(13)),
            installed_count: 3,
            file_target: Some(PyVersion::py3(12)),
            platform: None,
        };
        let f = classify(
            import_of("mediapipe"),
            "mediapipe".into(),
            false,
            Some(&analysis("mediapipe", &[9, 10, 11, 12])),
            &ctx,
        );
        match f.problem {
            Problem::IncompatibleInterpreter {
                current,
                target,
                reinstall_count,
                ref supported,
            } => {
                assert_eq!(current, PyVersion::py3(13));
                assert_eq!(target, Some(PyVersion::py3(12)));
                assert_eq!(reinstall_count, 3);
                assert_eq!(supported.to_range_string(), "3.9–3.12");
            }
            other => panic!("expected an interpreter conflict, got {other:?}"),
        }
    }

    #[test]
    fn no_venv_sizes_the_environment_for_the_whole_file() {
        let ctx = BufferContext {
            venv_python: None,
            installed_count: 0,
            file_target: Some(PyVersion::py3(12)),
            platform: None,
        };
        let f = classify(
            import_of("mediapipe"),
            "mediapipe".into(),
            false,
            Some(&analysis("mediapipe", &[9, 10, 11, 12])),
            &ctx,
        );
        assert_eq!(
            f.problem,
            Problem::NoEnvironment {
                target: Some(PyVersion::py3(12))
            }
        );
    }

    #[test]
    fn modules_beside_the_document_are_local() {
        let ws = std::env::temp_dir().join("pypilot-local-modules-test");
        let _ = std::fs::remove_dir_all(&ws);
        let notebooks = ws.join("notebooks");
        std::fs::create_dir_all(&notebooks).unwrap();
        std::fs::write(notebooks.join("helpers.py"), "").unwrap();

        assert!(!local_modules(&ws, None).contains("helpers"));
        assert!(local_modules(&ws, Some(&notebooks)).contains("helpers"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn cuda_extension_install_offers_carry_the_reason_instead() {
        use crate::core::platform::{Arch, Os};
        let windows = Platform {
            os: Os::Windows,
            arch: Arch::X86_64,
        };
        let linux = Platform {
            os: Os::Linux,
            arch: Arch::X86_64,
        };
        let table = ExtensionTable::load(matrix::CUDA_EXTENSIONS_JSON).unwrap();
        let missing = |package: &str| Finding {
            import: import_of(package),
            package: package.into(),
            renamed: false,
            problem: Problem::NotInstalled,
        };

        let f = flag_cuda_extension(missing("flash_attn"), &table, windows, None);
        let Problem::CudaExtension { blocking, message } = f.problem else {
            panic!("expected a CUDA extension warning, got {:?}", f.problem);
        };
        assert!(!blocking);
        assert!(message.contains("sdpa"));

        // xformers next to an older torch: installing the latest replaces it.
        let f = flag_cuda_extension(missing("xformers"), &table, linux, Some("2.7.0"));
        assert!(matches!(f.problem, Problem::CudaExtension { .. }));
        let f = flag_cuda_extension(missing("xformers"), &table, linux, Some("2.11.0"));
        assert_eq!(f.problem, Problem::NotInstalled);

        // Ordinary packages and other problems are untouched.
        let f = flag_cuda_extension(missing("numpy"), &table, windows, None);
        assert_eq!(f.problem, Problem::NotInstalled);
        let not_on_pypi = Finding {
            problem: Problem::NotOnPyPi,
            ..missing("flash_attn")
        };
        let f = flag_cuda_extension(not_on_pypi, &table, windows, None);
        assert_eq!(f.problem, Problem::NotOnPyPi);
    }

    #[test]
    fn unknown_project_never_becomes_an_install_offer() {
        let ctx = BufferContext {
            venv_python: Some(PyVersion::py3(12)),
            ..Default::default()
        };
        let f = classify(import_of("mediapip"), "mediapip".into(), false, None, &ctx);
        assert_eq!(f.problem, Problem::NotOnPyPi);
    }

    #[test]
    fn release_without_files_here_is_neither_an_install_nor_a_python_switch() {
        let mut a = analysis("tensorflow", &[]);
        a.version = "2.15.0.post1".into();
        a.has_platform_wheels = false;
        a.has_foreign_wheels = true;
        a.no_files_for_platform = true;
        a.wheel_platforms = vec!["Linux x64".into()];
        let platform = Platform {
            os: crate::core::platform::Os::Windows,
            arch: crate::core::platform::Arch::X86_64,
        };
        for venv_python in [None, Some(PyVersion::py3(11))] {
            let ctx = BufferContext {
                venv_python,
                installed_count: 0,
                file_target: None,
                platform: Some(platform),
            };
            let f = classify(
                import_of("tensorflow"),
                "tensorflow".into(),
                false,
                Some(&a),
                &ctx,
            );
            let Problem::NoFilesForPlatform { message } = f.problem else {
                panic!("expected NoFilesForPlatform, got {:?}", f.problem);
            };
            assert!(message.starts_with(
                "tensorflow 2.15.0.post1 publishes no files for Windows x64 (only Linux x64 wheels"
            ));
        }
    }
}
