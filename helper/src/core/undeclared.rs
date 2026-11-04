//! Imports the code makes that the manifest never declares, for the CLI.
//!
//! The editor's import guardian ([`crate::core::guardian`]) flags these per
//! open file. `pypilot doctor` has no open files, so this walks the
//! workspace the way the LSP's workspace scan does, runs the same guardian
//! over each directory's code, and keeps the imports whose distribution is
//! neither installed nor declared. Each is named with the distribution that
//! provides it (`yaml` is PyYAML), from the same import table.
//!
//! Also maps an import name typed where a distribution name belongs
//! (`pypilot install sklearn`) to that distribution, see [`dist_for_import`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::core::guardian::{self, Problem};
use crate::core::platform::Platform;
use crate::core::project::{normalize_name, ProjectDeps};
use crate::core::{Finding, FixKind, Severity};
use crate::pypi::{MetadataSource, PyVersion};

/// Files read at most; the walk keeps the shallow ones first.
const MAX_FILES: usize = 400;
const MAX_ENTRIES: usize = 20_000;

/// One undeclared import and the distribution that provides it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Undeclared {
    pub import: String,
    pub dist: String,
}

/// The imports in `workspace` that need a distribution the project neither
/// declares nor has installed, one per distribution, sorted by import.
pub async fn find<S: MetadataSource>(
    workspace: &Path,
    project: &ProjectDeps,
    venv: Option<(&Path, Option<PyVersion>)>,
    source: &S,
) -> Vec<Undeclared> {
    let scan = crate::lsp::collect_files_capped(workspace, MAX_FILES, MAX_ENTRIES);
    // One guardian pass per directory: a script's own directory is on its
    // sys.path, so a sibling module is local there and nowhere else.
    let mut by_dir: BTreeMap<PathBuf, String> = BTreeMap::new();
    for file in &scan.files {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        let code = if file.extension().is_some_and(|e| e == "ipynb") {
            match crate::lsp::notebook_python(&text) {
                Some(nb) => nb.text,
                None => continue,
            }
        } else {
            text
        };
        let dir = file.parent().unwrap_or(workspace).to_path_buf();
        let buf = by_dir.entry(dir).or_default();
        buf.push_str(&code);
        buf.push('\n');
    }

    let installed = venv
        .map(|(path, _)| crate::core::installed::scan(path))
        .unwrap_or_default();
    let venv_python = venv.and_then(|(_, py)| py);
    let declared: Vec<String> = project
        .packages
        .iter()
        .map(|r| normalize_name(&r.name))
        .collect();

    let mut out: BTreeMap<String, Undeclared> = BTreeMap::new();
    for (dir, code) in by_dir {
        let findings = guardian::analyze_buffer_in(
            &code,
            workspace,
            Some(&dir),
            Platform::current(),
            venv_python,
            &installed,
            source,
        )
        .await;
        for f in findings {
            let missing = matches!(
                f.problem,
                Problem::NotInstalled
                    | Problem::NoEnvironment { .. }
                    | Problem::IncompatibleInterpreter { .. }
            );
            let dist = normalize_name(&f.package);
            if !missing || declared.contains(&dist) {
                continue;
            }
            // A module its distribution dropped (`pkg_resources` from
            // setuptools 82) is the known-breakage finding's, which asks for
            // the release that still ships it; the newest would not do.
            if crate::core::knowledge::table::table()
                .module_removal(&f.import.module)
                .is_some()
            {
                continue;
            }
            out.entry(dist).or_insert(Undeclared {
                import: f.import.module.clone(),
                dist: f.package.clone(),
            });
        }
    }
    let mut list: Vec<Undeclared> = out.into_values().collect();
    list.sort_by(|a, b| a.import.cmp(&b.import));
    list
}

/// The doctor finding for `list`, with the fix that installs and records them.
pub fn finding(list: &[Undeclared]) -> Option<Finding> {
    if list.is_empty() {
        return None;
    }
    let named: Vec<String> = list
        .iter()
        .map(|u| {
            if normalize_name(&u.import) == normalize_name(&u.dist) {
                format!("`{}`", u.import)
            } else {
                format!("`{}` (from {})", u.import, u.dist)
            }
        })
        .collect();
    let dists: Vec<String> = list.iter().map(|u| u.dist.clone()).collect();
    let (title, them) = match list.len() {
        1 => (
            format!("Import {} is not declared or installed", named[0]),
            "it",
        ),
        n => (format!("{n} imports are not declared or installed"), "them"),
    };
    Some(Finding {
        severity: Severity::Warning,
        title,
        detail: format!(
            "The code imports {}, but the project's dependencies do not list {them} and the environment does not have {them}, so those imports fail. Fix: add {} to the dependencies (`pypilot install <name>` installs and records one).",
            named.join(", "),
            dists.join(", ")
        ),
        fix: FixKind::AddRequirements(dists),
    })
}

/// The distribution for `name` when it is an import name that differs from
/// its distribution's (`sklearn` -> `scikit-learn`, `yaml` -> `PyYAML`).
/// `None` when `name` is already the distribution, or unknown to the table.
pub fn dist_for_import(name: &str) -> Option<String> {
    let dist = crate::matrix::resolve_package(name);
    (crate::matrix::is_mapped(name) && normalize_name(&dist) != normalize_name(name))
        .then_some(dist)
}

/// `requirement` with its name swapped for the distribution it imports as,
/// when it is an import name, and that distribution: `sklearn>=1` ->
/// `scikit-learn>=1`.
pub fn map_requirement(requirement: &str) -> Option<(String, String)> {
    let trimmed = requirement.trim();
    let end = trimmed
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
        .unwrap_or(trimmed.len());
    let dist = dist_for_import(&trimmed[..end])?;
    Some((format!("{dist}{}", &trimmed[end..]), dist))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_names_map_to_their_distribution() {
        assert_eq!(dist_for_import("sklearn").as_deref(), Some("scikit-learn"));
        assert_eq!(dist_for_import("yaml").as_deref(), Some("PyYAML"));
        // Same distribution, different spelling: nothing to say.
        assert_eq!(dist_for_import("django"), None);
        assert_eq!(dist_for_import("requests"), None);
        assert_eq!(
            map_requirement("sklearn>=1.3"),
            Some(("scikit-learn>=1.3".into(), "scikit-learn".into()))
        );
        assert_eq!(map_requirement("numpy<2"), None);
    }

    #[test]
    fn one_finding_names_every_import_and_its_dist() {
        let list = vec![
            Undeclared {
                import: "bs4".into(),
                dist: "beautifulsoup4".into(),
            },
            Undeclared {
                import: "requests".into(),
                dist: "requests".into(),
            },
        ];
        let f = finding(&list).unwrap();
        assert_eq!(f.title, "2 imports are not declared or installed");
        assert!(f.detail.contains("`bs4` (from beautifulsoup4), `requests`"));
        assert_eq!(
            f.fix,
            FixKind::AddRequirements(vec!["beautifulsoup4".into(), "requests".into()])
        );
        assert!(finding(&[]).is_none());
    }

    fn meta(name: &str) -> crate::pypi::metadata::PackageMetadata {
        serde_json::from_value(serde_json::json!({
            "info": {"name": name, "version": "1.0", "requires_python": ">=3.8"},
            "urls": [{"filename": format!("{name}-1.0-py3-none-any.whl"), "packagetype": "bdist_wheel"}],
            "releases": ["1.0"],
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn workspace_imports_not_declared_are_found() {
        let dir = std::env::temp_dir().join(format!("pypilot-undeclared-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("pkg")).unwrap();
        std::fs::write(
            dir.join("app.py"),
            "import os\nimport yaml\nimport requests\nimport helpers\nfrom PIL import Image\nimport pkg_resources\n",
        )
        .unwrap();
        std::fs::write(dir.join("helpers.py"), "").unwrap();
        // A sibling module is local in its own directory only.
        std::fs::write(
            dir.join("pkg").join("mod.py"),
            "import sibling\nimport bs4\n",
        )
        .unwrap();
        std::fs::write(dir.join("pkg").join("sibling.py"), "").unwrap();

        let mut source = crate::pypi::FixtureSource::new();
        // setuptools is on PyPI, but `pkg_resources` is the known-breakage
        // finding's (setuptools<81, C s7a), so it is not listed here.
        for name in [
            "PyYAML",
            "requests",
            "pillow",
            "beautifulsoup4",
            "setuptools",
        ] {
            source.insert(name, meta(name));
        }
        let project = ProjectDeps {
            packages: vec![crate::core::project::Requirement::any("requests")],
            ..Default::default()
        };
        let found = find(&dir, &project, None, &source).await;
        assert_eq!(
            found,
            [
                Undeclared {
                    import: "PIL".into(),
                    dist: "pillow".into()
                },
                Undeclared {
                    import: "bs4".into(),
                    dist: "beautifulsoup4".into()
                },
                Undeclared {
                    import: "yaml".into(),
                    dist: "PyYAML".into()
                },
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
