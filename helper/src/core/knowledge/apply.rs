//! Carry out [`FixKind::ReplaceDist`]. (Constraints are requirement edits,
//! run by [`crate::core::req_edit::apply`].)
//!
//! The manifest is edited the way the project already manages it:
//!
//! - a `pyproject.toml` with a `[project]` table goes through `uv add` /
//!   `uv remove`, which keep its layout and also update `uv.lock` and the venv;
//! - a `requirements.txt` is edited line by line (the line for the name is
//!   replaced, removed, or appended), then the venv is changed with
//!   `uv pip install` / `uv pip uninstall`;
//! - with neither, only the venv changes.
//!
//! The venv's Python is never changed here. Every mutation goes through
//! [`crate::core::exec`], so under [`crate::core::exec::dry_run`] this plans
//! exactly what it would run.

use std::path::Path;

use crate::core::exec::{self, Effect};
use crate::core::project::{normalize_name, requirement_name};
use crate::core::setup::Step;
use crate::core::{installed, uv, FixKind};
use crate::settings::Settings;

use super::Replacement;

/// Where the project declares its dependencies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Manifests {
    /// `pyproject.toml` with a `[project]` table.
    pyproject: bool,
    requirements: bool,
    /// The `.venv`'s Python. uv project commands (`uv add`, `uv remove`)
    /// get it as `--python`: without a pin they would otherwise rebuild
    /// the venv on the newest Python requires-python allows.
    python: Option<crate::pypi::pyversion::PyVersion>,
}

impl Manifests {
    /// `uv <cmd> [--python X.Y] <names...>`.
    fn project_args<'a>(&self, cmd: &str, names: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        let mut args = vec![cmd.to_string()];
        if let Some(py) = self.python {
            args.push("--python".into());
            args.push(py.to_string());
        }
        args.extend(names.into_iter().map(str::to_string));
        args
    }
}

impl Manifests {
    fn of(workspace: &Path) -> Manifests {
        let pyproject = std::fs::read_to_string(workspace.join("pyproject.toml"))
            .ok()
            .and_then(|t| t.parse::<toml::Table>().ok())
            .is_some_and(|d| d.get("project").is_some_and(|p| p.is_table()));
        let python = std::fs::read_to_string(workspace.join(".venv").join("pyvenv.cfg"))
            .ok()
            .and_then(|t| crate::core::probe::parse_pyvenv_version(&t));
        Manifests {
            pyproject,
            requirements: workspace.join("requirements.txt").is_file(),
            python,
        }
    }
}

/// Run one known-breakage fix in `workspace` and return its steps.
///
/// This is the function the `doctor --fix` planner calls for
/// [`FixKind::ReplaceDist`]; any other kind is
/// an error. Steps that fail are returned with `ok: false` rather than as an
/// `Err`, so the caller can report them next to its own; `Err` means nothing
/// could run (no uv, a pixi project).
pub async fn apply(
    workspace: &Path,
    settings: &Settings,
    fix: &FixKind,
) -> crate::Result<Vec<Step>> {
    if crate::core::pixi::detect(workspace).is_some()
        || settings.package_manager == crate::settings::PackageManager::Pixi
    {
        anyhow::bail!(
            "this is a pixi project: change its dependencies with `pixi add` / `pixi remove`"
        );
    }
    if settings.package_manager == crate::settings::PackageManager::Pip {
        anyhow::bail!(
            "the package manager is set to pip, and this fix runs uv; apply it by hand: {}",
            describe(fix)
        );
    }
    let uv_info = uv::ensure(settings).await?;
    let manifests = Manifests::of(workspace);
    let mut steps = Vec::new();
    match fix {
        FixKind::ReplaceDist(r) => {
            replace_dist(workspace, &uv_info, manifests, r, &mut steps).await
        }
        other => anyhow::bail!("`{}` is not a known-breakage fix", other.kind()),
    }
    Ok(steps)
}

/// One line saying what `fix` changes, for messages.
pub fn describe(fix: &FixKind) -> String {
    match fix {
        FixKind::ReplaceDist(r) => {
            let mut parts = Vec::new();
            if !r.drop.is_empty() {
                parts.push(format!("drop {} from the dependencies", r.drop.join(", ")));
            }
            if !r.uninstall.is_empty() {
                parts.push(format!("uninstall {}", r.uninstall.join(", ")));
            }
            if !r.install.is_empty() {
                parts.push(format!("reinstall {}", r.install.join(", ")));
            }
            parts.join(", then ")
        }
        other => other.kind().replace('_', " "),
    }
}

/// Every distinct known-breakage fix in `fixes`, in order, stopping at the
/// first failed step. For the planner: it appends to its own step list.
pub async fn apply_all(
    workspace: &Path,
    settings: &Settings,
    fixes: &[FixKind],
    steps: &mut Vec<Step>,
) {
    let mut seen: Vec<&FixKind> = Vec::new();
    for fix in fixes {
        if seen.contains(&fix) {
            continue;
        }
        seen.push(fix);
        match apply(workspace, settings, fix).await {
            Ok(done) => {
                let failed = done.iter().any(|s| !s.ok);
                steps.extend(done);
                if failed {
                    return;
                }
            }
            Err(e) => {
                steps.push(Step {
                    name: fix.kind().replace('_', " "),
                    ok: false,
                    detail: format!("{e:#}"),
                });
                return;
            }
        }
    }
}

fn record(
    steps: &mut Vec<Step>,
    name: &str,
    result: crate::Result<crate::core::command::Output>,
    ok_detail: &str,
) -> bool {
    match result {
        Ok(out) if out.success() => {
            steps.push(Step {
                name: name.to_string(),
                ok: true,
                detail: ok_detail.to_string(),
            });
            true
        }
        Ok(out) => {
            let detail = if out.stderr.trim().is_empty() {
                out.stdout.trim().to_string()
            } else {
                out.stderr.trim().to_string()
            };
            steps.push(Step {
                name: name.to_string(),
                ok: false,
                detail,
            });
            false
        }
        Err(e) => {
            steps.push(Step {
                name: name.to_string(),
                ok: false,
                detail: format!("{e:#}"),
            });
            false
        }
    }
}

fn venv_writes(effect: Effect, workspace: &Path) -> Effect {
    effect.writes(workspace.join(".venv"))
}

fn manifest_writes(effect: Effect, workspace: &Path) -> Effect {
    effect
        .writes(workspace.join("pyproject.toml"))
        .writes(workspace.join("uv.lock"))
        .writes(workspace.join(".venv"))
}

async fn replace_dist(
    workspace: &Path,
    uv_info: &uv::UvInfo,
    manifests: Manifests,
    r: &Replacement,
    steps: &mut Vec<Step>,
) {
    // 1. The manifest stops asking for the strays.
    if !r.drop.is_empty() {
        let declared = pyproject_names(workspace);
        let in_pyproject: Vec<&str> = r
            .drop
            .iter()
            .filter(|d| declared.contains(&normalize_name(d)))
            .map(String::as_str)
            .collect();
        if manifests.pyproject && !in_pyproject.is_empty() {
            let name = format!("uv remove {}", in_pyproject.join(" "));
            let effect = manifest_writes(
                Effect::new(&name, "drop the stray distributions from pyproject.toml"),
                workspace,
            );
            let args = manifests.project_args("remove", in_pyproject.iter().copied());
            let out = exec::run(&uv_info.path, &args, Some(workspace), effect).await;
            if !record(steps, &name, out, "removed from pyproject.toml") {
                return;
            }
        }
        if manifests.requirements {
            let path = workspace.join("requirements.txt");
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let edited = remove_requirements(&text, &r.drop);
            if edited != text {
                let effect = Effect::new(
                    format!("remove {} from requirements.txt", r.drop.join(", ")),
                    "drop the stray distributions where the project declares its dependencies",
                );
                if let Err(e) = exec::write(&path, edited, effect) {
                    steps.push(Step {
                        name: "edit requirements.txt".into(),
                        ok: false,
                        detail: e.to_string(),
                    });
                    return;
                }
                steps.push(Step {
                    name: "edit requirements.txt".into(),
                    ok: true,
                    detail: format!("removed {}", r.drop.join(", ")),
                });
            }
        }
    }

    // 2. Uninstall what is still there (a sync above may have removed some).
    let venv = workspace.join(".venv");
    let present = installed::scan(&venv);
    let still: Vec<&str> = r
        .uninstall
        .iter()
        .filter(|d| exec::is_dry_run() || present.contains(d))
        .map(String::as_str)
        .collect();
    if !still.is_empty() {
        let name = format!("uv pip uninstall {}", still.join(" "));
        let effect = venv_writes(
            Effect::new(
                &name,
                "remove the distributions whose files overlap or shadow others",
            ),
            workspace,
        );
        let mut args = vec!["pip", "uninstall"];
        args.extend(still.iter().copied());
        let out = exec::run(&uv_info.path, &args, Some(workspace), effect).await;
        if !record(steps, &name, out, "uninstalled") {
            return;
        }
    }

    // 3. The right distribution, freshly installed so no overwritten file
    // survives.
    if r.install.is_empty() {
        return;
    }
    let declared = pyproject_names(workspace);
    let missing: Vec<&str> = r
        .install
        .iter()
        .filter(|i| !declared.contains(&normalize_name(i)))
        .map(String::as_str)
        .collect();
    if manifests.pyproject && !missing.is_empty() {
        let name = format!("uv add {}", missing.join(" "));
        let effect = manifest_writes(
            Effect::new(
                &name,
                "record the replacement in pyproject.toml and install it",
            ),
            workspace,
        );
        let args = manifests.project_args("add", missing.iter().copied());
        let out = exec::run(&uv_info.path, &args, Some(workspace), effect).await;
        if !record(
            steps,
            &name,
            out,
            "recorded in pyproject.toml and installed",
        ) {
            return;
        }
    } else if manifests.requirements && !manifests.pyproject {
        let path = workspace.join("requirements.txt");
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let mut edited = text.clone();
        for i in &r.install {
            if !has_requirement(&edited, i) {
                edited = set_requirement(&edited, i);
            }
        }
        if edited != text {
            let effect = Effect::new(
                format!("add {} to requirements.txt", r.install.join(", ")),
                "record the replacement where the project declares its dependencies",
            );
            if let Err(e) = exec::write(&path, edited, effect) {
                steps.push(Step {
                    name: "edit requirements.txt".into(),
                    ok: false,
                    detail: e.to_string(),
                });
                return;
            }
        }
    }
    let name = format!("uv pip install --reinstall {}", r.install.join(" "));
    let effect = venv_writes(
        Effect::new(
            &name,
            "reinstall the kept distribution so none of its files are the stray's",
        ),
        workspace,
    );
    let mut args = vec!["pip", "install", "--reinstall"];
    args.extend(r.install.iter().map(String::as_str));
    let out = exec::run(&uv_info.path, &args, Some(workspace), effect).await;
    record(steps, &name, out, "reinstalled");
}

/// Normalized names in `[project] dependencies` (and the optional lists).
fn pyproject_names(workspace: &Path) -> Vec<String> {
    std::fs::read_to_string(workspace.join("pyproject.toml"))
        .ok()
        .and_then(|t| t.parse::<toml::Table>().ok())
        .map(|d| {
            super::pyproject_dependencies(&d)
                .iter()
                .filter_map(|r| requirement_name(r))
                .map(|n| normalize_name(&n))
                .collect()
        })
        .unwrap_or_default()
}

fn line_name(line: &str) -> Option<String> {
    requirement_name(line).map(|n| normalize_name(&n))
}

fn has_requirement(text: &str, req: &str) -> bool {
    let Some(want) = line_name(req) else {
        return false;
    };
    text.lines()
        .any(|l| line_name(l).as_deref() == Some(want.as_str()))
}

/// `text` with the requirement line for `req`'s name replaced by `req`, or
/// `req` appended when there is none. Line endings and the rest are kept.
pub fn set_requirement(text: &str, req: &str) -> String {
    let Some(want) = line_name(req) else {
        return text.to_string();
    };
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut replaced = false;
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        if line_name(line).as_deref() == Some(want.as_str()) {
            if !replaced {
                // Keep a marker the line had: it still decides where it applies.
                let marker = line.split_once(';').map(|(_, m)| m.trim());
                lines.push(match marker {
                    Some(m) if !req.contains(';') => format!("{req}; {m}"),
                    _ => req.to_string(),
                });
                replaced = true;
            }
            continue;
        }
        lines.push(line.to_string());
    }
    if !replaced {
        lines.push(req.to_string());
    }
    let mut out = lines.join(eol);
    out.push_str(eol);
    out
}

/// `text` without the requirement lines for `names`.
pub fn remove_requirements(text: &str, names: &[String]) -> String {
    let drop: Vec<String> = names.iter().map(|n| normalize_name(n)).collect();
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let kept: Vec<&str> = text
        .lines()
        .filter(|l| !line_name(l).is_some_and(|n| drop.contains(&n)))
        .collect();
    if kept.is_empty() {
        return String::new();
    }
    let mut out = kept.join(eol);
    out.push_str(eol);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_requirement_replaces_the_line_for_its_name() {
        let text = "pandas==1.5.3\nNumPy>=1.20  # numeric\nrequests\n";
        assert_eq!(
            set_requirement(text, "numpy<2"),
            "pandas==1.5.3\nnumpy<2\nrequests\n"
        );
        assert_eq!(
            set_requirement("pandas==1.5.3\n", "numpy<2"),
            "pandas==1.5.3\nnumpy<2\n"
        );
        assert_eq!(set_requirement("", "numpy<2"), "numpy<2\n");
        // Markers and CRLF are kept.
        assert_eq!(
            set_requirement("a\r\nnumpy; python_version >= '3.9'\r\n", "numpy<2"),
            "a\r\nnumpy<2; python_version >= '3.9'\r\n"
        );
    }

    #[test]
    fn strays_are_removed_by_normalized_name() {
        let text = "PyJWT\njwt==1.3.1\n# jwt is needed\nrequests\n";
        assert_eq!(
            remove_requirements(text, &["jwt".to_string()]),
            "PyJWT\n# jwt is needed\nrequests\n"
        );
        assert!(has_requirement(text, "pyjwt"));
        assert!(!has_requirement(text, "flask"));
        assert_eq!(remove_requirements("jwt\n", &["jwt".to_string()]), "");
    }

    #[test]
    fn manifests_are_told_apart() {
        let dir = std::env::temp_dir().join(format!("pp-kb-manifests-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            Manifests::of(&dir),
            Manifests {
                pyproject: false,
                requirements: false,
                python: None,
            }
        );
        std::fs::write(
            dir.join("pyproject.toml"),
            "[tool.black]\nline-length = 88\n",
        )
        .unwrap();
        std::fs::write(dir.join("requirements.txt"), "numpy\n").unwrap();
        assert_eq!(
            Manifests::of(&dir),
            Manifests {
                pyproject: false,
                requirements: true,
                python: None,
            }
        );
        std::fs::write(
            dir.join("pyproject.toml"),
            "[project]\nname = \"x\"\ndependencies = [\"jwt\", \"PyJWT>=2\"]\n",
        )
        .unwrap();
        assert!(Manifests::of(&dir).pyproject);
        std::fs::create_dir_all(dir.join(".venv")).unwrap();
        std::fs::write(
            dir.join(".venv/pyvenv.cfg"),
            "version_info = 3.11.9
",
        )
        .unwrap();
        let m = Manifests::of(&dir);
        assert_eq!(
            m.project_args("add", ["numpy<2"]),
            ["add", "--python", "3.11", "numpy<2"]
        );
        assert_eq!(pyproject_names(&dir), ["jwt", "pyjwt"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn other_kinds_are_refused() {
        let dir = std::env::temp_dir().join(format!("pp-kb-refuse-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (result, _) = exec::dry_run(apply(&dir, &Settings::default(), &FixKind::Relock)).await;
        // Either uv is unavailable here or the kind is refused; never Ok.
        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
