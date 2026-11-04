//! Read requirements exactly as the manifest writes them, and edit one.
//!
//! [`crate::core::project::scan`] normalizes dependencies for the PyPI engine
//! and drops markers. Proposing an edit needs the text as written, and
//! applying it must touch that one requirement only: pyproject.toml is edited
//! with `toml_edit` (comments and layout kept), a requirements file line by
//! line (comments and line endings kept).
//!
//! [`apply`] is the single entry point for the requirement fixes
//! ([`FixKind::EditRequirement`], [`FixKind::AddRequirements`]).

use std::path::{Path, PathBuf};

use crate::core::exec::{self, Effect};
use crate::core::project::{normalize_name, read_manifest, requirement_name};
use crate::core::setup::Step;
use crate::core::{FixKind, RequirementEdit};
use crate::settings::Settings;

/// One requirement as a manifest writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredReq {
    pub file: PathBuf,
    /// The requirement as written, without a trailing comment.
    pub raw: String,
    /// Normalized per PEP 503.
    pub name: String,
}

impl DeclaredReq {
    /// The name as written, with any extras: `requests[socks]`.
    pub fn head(&self) -> &str {
        let req = self.without_marker();
        let end = req
            .find(['=', '<', '>', '!', '~', ' ', '('])
            .unwrap_or(req.len());
        req[..end].trim()
    }

    /// The version specifier, `>=1.2,<2`, or empty.
    pub fn spec(&self) -> String {
        let req = self.without_marker();
        req[self.head().len()..]
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '(' && *c != ')')
            .collect()
    }

    /// The marker with its `;`, or empty.
    pub fn marker(&self) -> &str {
        self.raw.find(';').map_or("", |i| &self.raw[i..])
    }

    fn without_marker(&self) -> &str {
        self.raw.split(';').next().unwrap_or(&self.raw).trim()
    }

    /// This requirement with `spec` instead of its own, extras and marker kept.
    pub fn with_spec(&self, spec: &str) -> String {
        let marker = self.marker();
        if marker.is_empty() {
            format!("{}{spec}", self.head())
        } else {
            format!("{}{spec}{}", self.head(), marker)
        }
    }
}

/// What a resolver needs to see of a project: its requirements, and the
/// options (indexes, includes) that change how they resolve.
#[derive(Debug, Clone, Default)]
pub struct Declared {
    pub reqs: Vec<DeclaredReq>,
    /// requirements-file options to pass on (`--index-url`, `-r` with an
    /// absolute path, ...).
    pub options: Vec<String>,
    /// The project resolves through uv sources or indexes this reader does
    /// not model (`[tool.uv.sources]`, `[[tool.uv.index]]`), so a resolve
    /// of the plain requirements would not be the one uv makes.
    pub custom_resolution: bool,
}

/// The project's requirements as written: pyproject.toml's
/// `[project] dependencies`, then requirements.txt.
pub fn declared(workspace: &Path) -> Declared {
    let mut out = Declared::default();
    let pyproject = workspace.join("pyproject.toml");
    if let Some(text) = read_manifest(&pyproject) {
        read_pyproject(&text, &pyproject, &mut out);
    }
    let requirements = workspace.join("requirements.txt");
    if let Some(text) = read_manifest(&requirements) {
        let from_pyproject: Vec<String> = out.reqs.iter().map(|r| r.name.clone()).collect();
        read_requirements(&text, &requirements, workspace, &mut out);
        // pyproject.toml's requirement is the one installed: setup never
        // copies requirements.txt's pin over it. Resolving both would report
        // a clash that no install ever meets (the pin disagreement has its
        // own finding).
        out.reqs
            .retain(|r| r.file != requirements || !from_pyproject.contains(&r.name));
    }
    out
}

fn read_pyproject(text: &str, path: &Path, out: &mut Declared) {
    let Ok(doc) = toml::from_str::<toml::Value>(text) else {
        return;
    };
    if let Some(uv) = doc.get("tool").and_then(|t| t.get("uv")) {
        out.custom_resolution |= ["sources", "index", "index-url", "extra-index-url"]
            .iter()
            .any(|k| uv.get(k).is_some());
    }
    let Some(project) = doc.get("project") else {
        return;
    };
    let dynamic = project
        .get("dynamic")
        .and_then(|d| d.as_array())
        .is_some_and(|d| d.iter().any(|v| v.as_str() == Some("dependencies")));
    if dynamic {
        out.custom_resolution = true;
        return;
    }
    for raw in project
        .get("dependencies")
        .and_then(|d| d.as_array())
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
    {
        push_requirement(raw, path, out);
    }
}

fn read_requirements(text: &str, path: &Path, workspace: &Path, out: &mut Declared) {
    for line in text.lines() {
        let body = strip_comment(line).trim();
        if body.is_empty() {
            continue;
        }
        if body.starts_with('-') {
            if let Some(option) = resolver_option(body, workspace) {
                out.options.push(option);
            }
            continue;
        }
        push_requirement(body, path, out);
    }
}

/// A requirements-file option that changes resolution, rewritten so it
/// works from another directory. Editable and local installs are left out.
fn resolver_option(line: &str, workspace: &Path) -> Option<String> {
    let (flag, value) = match line.split_once([' ', '=']) {
        Some((f, v)) => (f, v.trim()),
        None => (line, ""),
    };
    match flag {
        "-r" | "--requirement" | "-c" | "--constraint" if !value.is_empty() => {
            let path = workspace.join(value);
            Some(format!("{flag} {}", path.display()))
        }
        "-i" | "--index-url" | "--extra-index-url" | "-f" | "--find-links" | "--no-index"
        | "--pre" => Some(line.to_string()),
        _ => None,
    }
}

fn push_requirement(raw: &str, path: &Path, out: &mut Declared) {
    let raw = raw.trim();
    // Direct references and local paths resolve relative to the project.
    if raw.contains('@') || raw.starts_with('.') || raw.starts_with('/') || raw.contains("://") {
        return;
    }
    if let Some(name) = requirement_name(raw) {
        out.reqs.push(DeclaredReq {
            file: path.to_path_buf(),
            raw: raw.to_string(),
            name: normalize_name(&name),
        });
    }
}

/// `line` without a `#` comment (one at the start, or after whitespace).
pub fn strip_comment(line: &str) -> &str {
    if line.trim_start().starts_with('#') {
        return "";
    }
    match line
        .as_bytes()
        .windows(2)
        .position(|w| w[0].is_ascii_whitespace() && w[1] == b'#')
    {
        Some(i) => &line[..i],
        None => line,
    }
}

/// Is `fix` one of the requirement fixes [`apply`] runs?
pub fn is_requirement_fix(fix: &FixKind) -> bool {
    matches!(
        fix,
        FixKind::EditRequirement(_) | FixKind::AddRequirements(_) | FixKind::RaiseRequiresPython(_)
    )
}

/// Run a requirement fix. [`FixKind::EditRequirement`] rewrites the one
/// requirement in its manifest; [`FixKind::AddRequirements`] installs the
/// packages and records them, as `pypilot install` does. Both honour
/// [`exec::dry_run`]. Any other fix kind is an error: the planner runs those.
pub async fn apply(
    workspace: &Path,
    settings: &Settings,
    fix: &FixKind,
) -> crate::Result<Vec<Step>> {
    match fix {
        FixKind::EditRequirement(edit) => {
            edit_file(edit)?;
            let detail = if edit.old.trim().is_empty() {
                format!("added `{}`", edit.new)
            } else if edit.new.trim().is_empty() {
                format!("removed `{}`", edit.old)
            } else {
                format!("`{}` -> `{}`", edit.old, edit.new)
            };
            Ok(vec![Step {
                name: format!("edit {}", file_label(&edit.file)),
                ok: true,
                detail,
            }])
        }
        FixKind::AddRequirements(names) => {
            let names = crate::core::install::validate_package_names(names)
                .map_err(|bad| anyhow::anyhow!("`{bad}` is not a package name"))?;
            let summary =
                crate::core::install::install_packages(workspace, settings, &names).await?;
            Ok(summary.steps)
        }
        FixKind::RaiseRequiresPython(floor) => {
            let detail = match crate::core::repair::raise_requires_python(workspace, *floor)? {
                Some((old, new)) => format!("requires-python `{old}` -> `{new}`"),
                None => format!("requires-python already allows only Python {floor} and newer"),
            };
            Ok(vec![Step {
                name: "edit pyproject.toml".into(),
                ok: true,
                detail,
            }])
        }
        other => anyhow::bail!("not a requirement fix: {other:?}"),
    }
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Apply one [`RequirementEdit`] to its file. An empty `old` adds `new`
/// (a constraint the project did not declare, such as `numpy<2`), creating
/// a requirements file that does not exist yet.
pub fn edit_file(edit: &RequirementEdit) -> crate::Result<()> {
    if edit.old.trim().is_empty() && !edit.file.exists() {
        exec::write(
            &edit.file,
            format!("{}\n", edit.new.trim()),
            Effect::new(
                format!("edit {}", file_label(&edit.file)),
                format!("add `{}`", edit.new),
            ),
        )?;
        return Ok(());
    }
    let text = read_manifest(&edit.file)
        .ok_or_else(|| anyhow::anyhow!("cannot read {}", edit.file.display()))?;
    let is_toml = edit
        .file
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("toml"));
    let updated = if is_toml {
        edit_pyproject(&text, &edit.old, &edit.new)?
    } else {
        edit_requirements(&text, &edit.old, &edit.new)?
    };
    exec::write(
        &edit.file,
        updated,
        Effect::new(
            format!("edit {}", file_label(&edit.file)),
            if edit.new.trim().is_empty() {
                format!("remove `{}`", edit.old)
            } else {
                format!("replace `{}` with `{}`", edit.old, edit.new)
            },
        ),
    )?;
    Ok(())
}

/// Replace the `[project] dependencies` entry `old` with `new`, keeping the
/// rest of the file as it is.
pub fn edit_pyproject(text: &str, old: &str, new: &str) -> crate::Result<String> {
    let mut doc: toml_edit::DocumentMut = text.parse()?;
    let deps = doc
        .get_mut("project")
        .and_then(|p| p.get_mut("dependencies"))
        .and_then(toml_edit::Item::as_array_mut)
        .ok_or_else(|| anyhow::anyhow!("pyproject.toml has no [project] dependencies"))?;
    if old.trim().is_empty() {
        // Same layout as the entry before it, so a one-per-line array stays so.
        let decor = deps.iter().last().map(|v| v.decor().clone());
        let mut value = toml_edit::Value::from(new.trim());
        if let Some(decor) = decor {
            *value.decor_mut() = decor;
        }
        deps.push_formatted(value);
        return Ok(doc.to_string());
    }
    let hits: Vec<usize> = deps
        .iter()
        .enumerate()
        .filter(|(_, v)| v.as_str().map(str::trim) == Some(old.trim()))
        .map(|(i, _)| i)
        .collect();
    let [index] = hits[..] else {
        anyhow::bail!(
            "expected `{old}` once in [project] dependencies, found it {} times",
            hits.len()
        );
    };
    let decor = deps.get(index).map(|v| v.decor().clone());
    let mut value = toml_edit::Value::from(new);
    if let Some(decor) = decor {
        *value.decor_mut() = decor;
    }
    deps.replace(index, value);
    Ok(doc.to_string())
}

/// Replace the requirements-file line whose requirement is `old`, keeping
/// its indentation, comment and line ending. An empty `new` removes the line.
pub fn edit_requirements(text: &str, old: &str, new: &str) -> crate::Result<String> {
    if old.trim().is_empty() {
        let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
        let mut out = text.to_string();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push_str(eol);
        }
        out.push_str(new.trim());
        out.push_str(eol);
        return Ok(out);
    }
    let mut out = String::with_capacity(text.len() + new.len());
    let mut hits = 0;
    for line in text.split_inclusive('\n') {
        let body = strip_comment(line.trim_end_matches(['\r', '\n']));
        if body.trim() == old.trim() && !old.trim().is_empty() {
            hits += 1;
            if new.trim().is_empty() {
                // An empty replacement removes the line, comment and all.
                continue;
            }
            let start = line.find(body.trim()).unwrap_or(0);
            out.push_str(&line[..start]);
            out.push_str(new);
            out.push_str(&line[start + body.trim().len()..]);
        } else {
            out.push_str(line);
        }
    }
    if hits != 1 {
        anyhow::bail!("expected `{old}` once in the requirements file, found it {hits} times");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PYPROJECT: &str = "[project]\nname = \"s1a\"\n# pins\ndependencies = [\n    \"requests==2.31.0\",  # http\n    'urllib3<1.21',\n]\n\n[project.optional-dependencies]\ndev = [\"urllib3<1.21\"]\n";

    #[test]
    fn pyproject_edit_touches_one_entry() {
        let out = edit_pyproject(PYPROJECT, "urllib3<1.21", "urllib3>=1.21.1,<3").unwrap();
        assert_eq!(
            out,
            PYPROJECT.replace("'urllib3<1.21',", "\"urllib3>=1.21.1,<3\",")
        );
        // Markers with quotes survive.
        let out = edit_pyproject(
            PYPROJECT,
            "requests==2.31.0",
            "requests==2.31.0; sys_platform != 'win32'",
        )
        .unwrap();
        assert!(out.contains("\"requests==2.31.0; sys_platform != 'win32'\",  # http"));
        assert!(edit_pyproject(PYPROJECT, "flask", "flask>=3").is_err());
    }

    #[test]
    fn requirements_edit_keeps_comments_and_crlf() {
        let text = "# top\r\nrequests==2.31.0\r\n  urllib3<1.21  # old\r\n-r more.txt\r\n";
        let out = edit_requirements(text, "urllib3<1.21", "urllib3>=1.21.1,<3").unwrap();
        assert_eq!(
            out,
            "# top\r\nrequests==2.31.0\r\n  urllib3>=1.21.1,<3  # old\r\n-r more.txt\r\n"
        );
        assert!(edit_requirements(text, "numpy", "numpy<2").is_err());
        // An empty replacement drops the line.
        assert_eq!(
            edit_requirements(text, "-r more.txt", "").unwrap(),
            "# top\r\nrequests==2.31.0\r\n  urllib3<1.21  # old\r\n"
        );
    }

    #[test]
    fn an_empty_old_adds_the_requirement() {
        assert_eq!(
            edit_requirements("pandas==1.5.3\r\n", "", "numpy<2").unwrap(),
            "pandas==1.5.3\r\nnumpy<2\r\n"
        );
        assert_eq!(
            edit_requirements("pandas==1.5.3", "", "numpy<2").unwrap(),
            "pandas==1.5.3\nnumpy<2\n"
        );
        let out = edit_pyproject(PYPROJECT, "", "setuptools<81").unwrap();
        let doc: toml::Value = toml::from_str(&out).unwrap();
        let deps: Vec<&str> = doc["project"]["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(deps, ["requests==2.31.0", "urllib3<1.21", "setuptools<81"]);
        assert!(out.contains("# pins"), "comments kept: {out}");
        let flat = "[project]\nname = \"x\"\ndependencies = [\"requests\"]\n";
        let out = edit_pyproject(flat, "", "numpy<2").unwrap();
        assert!(out.contains("\"requests\""), "{out}");
        assert!(out.contains("\"numpy<2\""), "{out}");
    }

    #[test]
    fn declared_reads_both_manifests_as_written() {
        let dir = tempdir();
        std::fs::write(dir.join("pyproject.toml"), PYPROJECT).unwrap();
        std::fs::write(
            dir.join("requirements.txt"),
            "--index-url https://example.org/simple\n-r base.txt\n-e .\nflask[async]>=3 ; python_version >= '3.9'  # web\nmylib @ file:///x\n",
        )
        .unwrap();
        let d = declared(&dir);
        let raws: Vec<&str> = d.reqs.iter().map(|r| r.raw.as_str()).collect();
        assert_eq!(
            raws,
            [
                "requests==2.31.0",
                "urllib3<1.21",
                "flask[async]>=3 ; python_version >= '3.9'"
            ]
        );
        assert_eq!(d.options[0], "--index-url https://example.org/simple");
        assert!(d.options[1].starts_with("-r ") && d.options[1].ends_with("base.txt"));
        assert!(!d.custom_resolution);
        let flask = &d.reqs[2];
        assert_eq!(flask.name, "flask");
        assert_eq!(flask.head(), "flask[async]");
        assert_eq!(flask.spec(), ">=3");
        assert_eq!(flask.marker(), "; python_version >= '3.9'");
        assert_eq!(flask.with_spec(""), "flask[async]; python_version >= '3.9'");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pyproject_pins_win_over_requirements_lines() {
        // D 5a: requirements.txt pins requests and pyyaml differently.
        // pyproject.toml's are the ones installed, so only they resolve.
        let dir = tempdir();
        std::fs::write(
            dir.join("pyproject.toml"),
            "[project]\nname = \"demo\"\ndependencies = [\"requests==2.31.0\", \"pyyaml==6.0.1\"]\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("requirements.txt"),
            "requests==2.25.0\nPyYAML==5.4.1\nattrs>=23\n",
        )
        .unwrap();
        let raws: Vec<String> = declared(&dir).reqs.into_iter().map(|r| r.raw).collect();
        assert_eq!(raws, ["requests==2.31.0", "pyyaml==6.0.1", "attrs>=23"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn uv_sources_mark_custom_resolution() {
        let dir = tempdir();
        std::fs::write(
            dir.join("pyproject.toml"),
            "[project]\nname='x'\ndependencies=['mylib']\n[tool.uv.sources]\nmylib = { path = '../mylib' }\n",
        )
        .unwrap();
        assert!(declared(&dir).custom_resolution);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn apply_edits_the_file_and_dry_run_only_records() {
        let dir = tempdir();
        let file = dir.join("pyproject.toml");
        std::fs::write(&file, PYPROJECT).unwrap();
        let fix = FixKind::EditRequirement(RequirementEdit {
            file: file.clone(),
            old: "urllib3<1.21".into(),
            new: "urllib3>=1.21.1,<3".into(),
        });
        let settings = Settings::default();
        let (result, steps) = exec::dry_run(apply(&dir, &settings, &fix)).await;
        assert!(result.is_ok());
        assert_eq!(steps.len(), 1, "{steps:?}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), PYPROJECT);

        let steps = apply(&dir, &settings, &fix).await.unwrap();
        assert!(steps[0].ok);
        assert!(std::fs::read_to_string(&file)
            .unwrap()
            .contains("\"urllib3>=1.21.1,<3\""));
        assert!(apply(&dir, &settings, &FixKind::Manual).await.is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn raising_requires_python_is_a_requirement_fix() {
        // c12b, A s3: `numpy>=2.1` needs Python 3.10, the project said 3.8.
        let dir = tempdir();
        let file = dir.join("pyproject.toml");
        let text = "[project]
name = \"x\"
requires-python = \">=3.8\"
dependencies = [\"numpy>=2.1\"]
";
        std::fs::write(&file, text).unwrap();
        let fix = FixKind::RaiseRequiresPython(crate::pypi::pyversion::PyVersion::py3(10));
        assert!(is_requirement_fix(&fix));
        let settings = Settings::default();
        let (result, _) = exec::dry_run(apply(&dir, &settings, &fix)).await;
        assert_eq!(
            result.unwrap()[0].detail,
            "requires-python `>=3.8` -> `>=3.10`"
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
        let steps = apply(&dir, &settings, &fix).await.unwrap();
        assert!(steps[0].ok);
        let after = std::fs::read_to_string(&file).unwrap();
        assert!(after.contains("requires-python = \">=3.10\""), "{after}");
        assert!(after.contains("\"numpy>=2.1\""));
        std::fs::remove_dir_all(&dir).ok();
    }

    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pypilot-req-edit-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
