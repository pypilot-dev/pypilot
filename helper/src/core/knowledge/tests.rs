//! One test per rule family, on hand-built contexts and the bundled table.

use std::collections::{BTreeMap, HashSet};

use super::rules::{evaluate, lower_bound, Context};
use super::table::{bundled, Table};
use super::*;
use crate::core::project::Requirement;
use crate::core::{FixKind, Severity};
use crate::pypi::pyversion::PyVersion;

fn req(name: &str, spec: &str) -> Requirement {
    Requirement {
        name: name.into(),
        spec: spec.into(),
    }
}

fn installed(pairs: &[(&str, &str)]) -> Option<BTreeMap<String, String>> {
    Some(
        pairs
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect(),
    )
}

fn venv_ctx(minor: u8) -> Context {
    Context {
        python: Some(PyVersion::py3(minor)),
        python_is_venv: true,
        installed: installed(&[]),
        ..Context::default()
    }
}

fn titles(ctx: &Context) -> Vec<String> {
    evaluate(&bundled(), ctx)
        .into_iter()
        .map(|f| f.title)
        .collect()
}

/// The fix for a constraint in a context with no manifest to edit (the
/// hand-built contexts below): install it. [`constraint_fixes_edit_the_manifest`]
/// covers the edits.
fn constraint(r: &str) -> FixKind {
    FixKind::AddRequirements(vec![r.into()])
}

fn pyproject_req(raw: &str) -> crate::core::req_edit::DeclaredReq {
    crate::core::req_edit::DeclaredReq {
        file: "pyproject.toml".into(),
        raw: raw.into(),
        name: crate::core::project::normalize_name(
            crate::core::project::requirement_name(raw)
                .unwrap()
                .as_str(),
        ),
    }
}

#[test]
fn constraint_fixes_edit_the_manifest() {
    use crate::core::RequirementEdit;
    let mut ctx = venv_ctx(12);
    ctx.declared_raw = vec![
        pyproject_req("pandas==2.0.3"),
        pyproject_req("setuptools>=81; python_version >= '3.9'"),
        pyproject_req("numpy<2"),
    ];
    ctx.new_requirement_file = Some("pyproject.toml".into());
    let edit = |old: &str, new: &str| {
        FixKind::EditRequirement(RequirementEdit {
            file: "pyproject.toml".into(),
            old: old.into(),
            new: new.into(),
        })
    };
    // Declared: rewritten in place, marker kept.
    assert_eq!(
        ctx.constraint_fix("pandas>=2.2.2"),
        edit("pandas==2.0.3", "pandas>=2.2.2")
    );
    assert_eq!(
        ctx.constraint_fix("setuptools<81"),
        edit(
            "setuptools>=81; python_version >= '3.9'",
            "setuptools<81; python_version >= '3.9'"
        )
    );
    // Not declared: added.
    assert_eq!(
        ctx.constraint_fix("standard-imghdr"),
        edit("", "standard-imghdr")
    );
    // Already declared as asked: only the venv lags.
    assert_eq!(ctx.constraint_fix("numpy < 2"), FixKind::SyncEnvironment);
    // No manifest: install it.
    ctx.new_requirement_file = None;
    assert_eq!(
        ctx.constraint_fix("standard-imghdr"),
        FixKind::AddRequirements(vec!["standard-imghdr".into()])
    );
    ctx.pixi = true;
    assert_eq!(ctx.constraint_fix("numpy<2"), FixKind::Manual);
}

#[test]
fn an_installed_version_the_manifest_rules_out_asks_for_a_sync() {
    let mut ctx = venv_ctx(12);
    ctx.declared_raw = vec![pyproject_req("requests==2.32.2")];
    ctx.installed = installed(&[("requests", "2.32.0")]);
    let found = evaluate(&Table::default(), &ctx);
    assert_eq!(found.len(), 1, "{found:#?}");
    assert_eq!(
        found[0].title,
        "requests 2.32.0 is installed, but pyproject.toml requires requests==2.32.2"
    );
    assert_eq!(found[0].fix, FixKind::SyncEnvironment);
    // With a uv.lock the health check owns drift; satisfied pins are silent.
    ctx.has_lock = true;
    assert!(evaluate(&Table::default(), &ctx).is_empty());
    ctx.has_lock = false;
    ctx.installed = installed(&[("requests", "2.32.2")]);
    assert!(evaluate(&Table::default(), &ctx).is_empty());
}

#[test]
fn lower_bounds_are_read_off_the_spec() {
    let v = |s: &str| lower_bound(s).map(|v| v.raw);
    assert_eq!(v(">=2"), Some("2".into()));
    assert_eq!(v(">=1.26,<2"), Some("1.26".into()));
    assert_eq!(v("==2.0.3"), Some("2.0.3".into()));
    assert_eq!(v("~=2.1"), Some("2.1".into()));
    assert_eq!(v("<2"), None);
    assert_eq!(v(""), None);
}

// --- numpy 2 ABI ------------------------------------------------------------

#[test]
fn installed_pandas_under_numpy_2_gets_the_numpy_cap() {
    // C s5 as installed: pandas 2.0.3 wheel next to numpy 2.2.6.
    let mut ctx = venv_ctx(11);
    ctx.installed = installed(&[("pandas", "2.0.3"), ("numpy", "2.2.6")]);
    ctx.declared = vec![req("pandas", "==2.0.3")];
    let found = evaluate(&bundled(), &ctx);
    assert_eq!(found.len(), 1, "{found:#?}");
    let f = &found[0];
    assert_eq!(
        f.title,
        "pandas 2.0.3 cannot run with the installed numpy 2.2.6"
    );
    assert!(
        f.detail.contains("numpy.dtype size changed"),
        "{}",
        f.detail
    );
    assert!(f.detail.contains("pandas 2.2.2 is the first release"));
    assert_eq!(f.fix, constraint("numpy<2"));
}

#[test]
fn a_declared_numpy_2_floor_turns_the_fix_into_an_upgrade() {
    // C s5 as declared: the project itself requires numpy>=2.
    let mut ctx = venv_ctx(11);
    ctx.installed = installed(&[("pandas", "2.0.3"), ("numpy", "2.2.6")]);
    ctx.declared = vec![req("pandas", "==2.0.3"), req("numpy", ">=2")];
    let found = evaluate(&bundled(), &ctx);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].fix, constraint("pandas>=2.2.2,<3"));
    assert!(found[0].detail.contains("requires numpy>=2"));
}

#[test]
fn a_declared_old_pandas_predicts_the_break_before_install() {
    // A7: requirements.txt pins pandas 1.5.3, nothing caps numpy.
    let mut ctx = venv_ctx(11);
    ctx.declared = vec![req("pandas", "==1.5.3")];
    ctx.resolved.insert("pandas".into(), "1.5.3".into());
    let found = evaluate(&bundled(), &ctx);
    assert_eq!(found.len(), 1, "{found:#?}");
    assert_eq!(found[0].title, "pandas 1.5.3 will break with numpy 2");
    assert_eq!(found[0].fix, constraint("numpy<2"));

    // A numpy cap already in place: silent.
    ctx.declared.push(req("numpy", "<2"));
    assert!(evaluate(&bundled(), &ctx).is_empty());
    // A fixed pandas: silent.
    ctx.declared = vec![req("pandas", "==2.2.2")];
    ctx.resolved.insert("pandas".into(), "2.2.2".into());
    assert!(evaluate(&bundled(), &ctx).is_empty());
}

#[test]
fn numpy_2_needs_python_3_9_so_3_8_is_safe() {
    let mut ctx = venv_ctx(8);
    ctx.declared = vec![req("pandas", "==1.5.3")];
    ctx.resolved.insert("pandas".into(), "1.5.3".into());
    assert!(evaluate(&bundled(), &ctx).is_empty());
}

#[test]
fn several_packages_needing_the_same_cap_share_one_finding() {
    let mut ctx = venv_ctx(11);
    ctx.installed = installed(&[
        ("pandas", "2.0.3"),
        ("matplotlib", "3.7.1"),
        ("numpy", "2.1.0"),
    ]);
    let found = evaluate(&bundled(), &ctx);
    assert_eq!(found.len(), 1, "{found:#?}");
    assert!(found[0].title.contains("pandas 2.0.3 and matplotlib 3.7.1"));
    assert_eq!(found[0].fix, constraint("numpy<2"));
}

#[test]
fn a_pair_depcheck_already_reports_is_not_repeated() {
    let mut ctx = venv_ctx(11);
    ctx.installed = installed(&[("pandas", "2.2.0"), ("numpy", "2.1.0")]);
    ctx.declared_conflicts = HashSet::from([("pandas".to_string(), "numpy".to_string())]);
    assert!(evaluate(&bundled(), &ctx).is_empty());
}

// --- API removals -------------------------------------------------------------

#[test]
fn each_api_removal_names_its_culprit_and_constraint() {
    let cases = [
        ("jinja2", "2.11.3", "markupsafe", "2.1.3", "markupsafe<2.1"),
        ("flask", "2.0.3", "werkzeug", "3.0.3", "werkzeug<3"),
        (
            "flask-sqlalchemy",
            "2.5.1",
            "sqlalchemy",
            "2.0.30",
            "sqlalchemy<2",
        ),
        // toolbelt prefers the upgrade: a urllib3 cap holds back security fixes.
        (
            "requests-toolbelt",
            "0.10.1",
            "urllib3",
            "2.2.2",
            "requests-toolbelt>=1.0.0,<2",
        ),
    ];
    for (pkg, pv, dep, dv, fix) in cases {
        let mut ctx = venv_ctx(11);
        ctx.installed = installed(&[(pkg, pv), (dep, dv)]);
        let found = evaluate(&bundled(), &ctx);
        assert_eq!(found.len(), 1, "{pkg}: {found:#?}");
        assert!(found[0].title.contains(pkg) && found[0].title.contains(dep));
        assert_eq!(found[0].fix, constraint(fix), "{pkg}");
    }
    // flask 1.x under itsdangerous 2.1 and jinja2 3.1: two separate caps.
    let mut ctx = venv_ctx(11);
    ctx.installed = installed(&[
        ("flask", "1.1.2"),
        ("itsdangerous", "2.1.2"),
        ("jinja2", "3.1.4"),
        ("werkzeug", "1.0.1"),
    ]);
    let fixes: Vec<FixKind> = evaluate(&bundled(), &ctx)
        .into_iter()
        .map(|f| f.fix)
        .collect();
    assert!(fixes.contains(&constraint("itsdangerous<2.1")), "{fixes:?}");
    assert!(fixes.contains(&constraint("jinja2<3.1")), "{fixes:?}");
}

#[test]
fn fixed_releases_and_old_dependencies_are_silent() {
    let mut ctx = venv_ctx(11);
    ctx.installed = installed(&[("flask", "2.2.5"), ("werkzeug", "3.0.3")]);
    assert!(evaluate(&bundled(), &ctx).is_empty());
    ctx.installed = installed(&[("flask", "2.0.3"), ("werkzeug", "2.3.8")]);
    assert!(evaluate(&bundled(), &ctx).is_empty());
}

#[test]
fn python_bounded_api_rows_respect_the_python() {
    // flake8 3.x with importlib-metadata 5 only breaks below 3.8.
    let mut ctx = venv_ctx(7);
    ctx.installed = installed(&[("flake8", "3.9.2"), ("importlib-metadata", "5.0.0")]);
    assert_eq!(evaluate(&bundled(), &ctx).len(), 1);
    ctx.python = Some(PyVersion::py3(11));
    assert!(evaluate(&bundled(), &ctx).is_empty());
}

// --- packages dead on a newer Python -----------------------------------------

#[test]
fn packages_dead_on_new_python_get_upgrade_or_replacement() {
    let mut ctx = venv_ctx(12);
    ctx.installed = installed(&[("kafka-python", "2.0.2")]);
    let found = evaluate(&bundled(), &ctx);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].fix, constraint("kafka-python>=2.0.3,<3"));

    // E s04: no release of nose works on 3.12. Nothing pins the Python, so
    // the fix is a rebuild on 3.11, which keeps `import nose` working.
    ctx.installed = installed(&[("nose", "1.3.7")]);
    ctx.requires_python = Some(">=3.9".into());
    let found = evaluate(&bundled(), &ctx);
    assert_eq!(found[0].title, "nose 1.3.7 does not work on Python 3.12");
    assert_eq!(
        found[0].fix,
        FixKind::RecreateWithPython(PyVersion::py3(11))
    );
    assert!(found[0].detail.contains("pytest") && found[0].detail.contains("3.11"));
    // No .python-version exists, so the reason names requires-python alone.
    assert!(found[0]
        .detail
        .contains("which the project's requires-python allows;"));
    assert!(!found[0].detail.contains(".python-version"));

    // .python-version pins 3.12, or requires-python excludes 3.11: advice only.
    ctx.python_pin = Some(PyVersion::py3(12));
    let found = evaluate(&bundled(), &ctx);
    assert_eq!(found[0].fix, FixKind::Manual);
    assert!(found[0].detail.contains(".python-version pins Python 3.12"));
    ctx.python_pin = None;
    ctx.requires_python = Some(">=3.12".into());
    let found = evaluate(&bundled(), &ctx);
    assert_eq!(found[0].fix, FixKind::Manual);
    assert!(found[0].detail.contains("does not allow it"));
    ctx.requires_python = None;

    // attrdict3 is a drop-in, so the swap is automatic.
    ctx.installed = installed(&[("attrdict", "2.0.1")]);
    ctx.declared = vec![req("attrdict", "")];
    let found = evaluate(&bundled(), &ctx);
    assert_eq!(
        found[0].fix,
        FixKind::ReplaceDist(Replacement {
            uninstall: vec!["attrdict".into()],
            install: vec!["attrdict3".into()],
            drop: vec!["attrdict".into()],
        })
    );

    // On the last Python that works, silent.
    let mut ctx = venv_ctx(11);
    ctx.installed = installed(&[("nose", "1.3.7"), ("kafka-python", "2.0.2")]);
    assert!(evaluate(&bundled(), &ctx).is_empty());
}

// --- shadowing ----------------------------------------------------------------

#[test]
fn jwt_next_to_pyjwt_drops_jwt_and_reinstalls_pyjwt() {
    // C s6a.
    let mut ctx = venv_ctx(12);
    ctx.installed = installed(&[("jwt", "1.4.0"), ("pyjwt", "2.10.1")]);
    ctx.declared = vec![req("pyjwt", ""), req("jwt", "")];
    let found = evaluate(&bundled(), &ctx);
    assert_eq!(found.len(), 1, "{found:#?}");
    assert_eq!(
        found[0].title,
        "jwt and pyjwt both provide the `jwt` module"
    );
    assert_eq!(
        found[0].fix,
        FixKind::ReplaceDist(Replacement {
            uninstall: vec!["jwt".into(), "pyjwt".into()],
            install: vec!["PyJWT".into()],
            drop: vec!["jwt".into()],
        })
    );
    // Only one of them: silent.
    ctx.installed = installed(&[("pyjwt", "2.10.1")]);
    ctx.declared = vec![req("pyjwt", "")];
    assert!(evaluate(&bundled(), &ctx).is_empty());
}

#[test]
fn stdlib_backports_are_flagged_on_modern_python() {
    // C s6b / s6c.
    for (dist, module) in [
        ("enum34", "enum"),
        ("typing", "typing"),
        ("dataclasses", "dataclasses"),
        ("futures", "concurrent"),
    ] {
        let mut ctx = venv_ctx(12);
        ctx.installed = installed(&[(dist, "1.0")]);
        ctx.declared = vec![req(dist, "")];
        let found = evaluate(&bundled(), &ctx);
        assert_eq!(found.len(), 1, "{dist}: {found:#?}");
        assert!(
            found[0].title.contains(&format!("`{module}`")),
            "{}",
            found[0].title
        );
        assert_eq!(found[0].severity, Severity::Warning);
        assert_eq!(
            found[0].fix,
            FixKind::ReplaceDist(Replacement {
                uninstall: vec![dist.into()],
                install: vec![],
                drop: vec![dist.into()],
            })
        );
    }
    // configparser is harmless in practice: Info only.
    let mut ctx = venv_ctx(12);
    ctx.installed = installed(&[("configparser", "7.1.0")]);
    let found = evaluate(&bundled(), &ctx);
    assert_eq!(found[0].severity, Severity::Info);
    assert!(found[0].detail.contains("another package pulls it in"));
}

#[test]
fn a_backport_declared_behind_a_marker_is_left_alone() {
    let mut ctx = venv_ctx(12);
    ctx.declared = vec![req("dataclasses", "")];
    ctx.with_marker = HashSet::from(["dataclasses".to_string()]);
    assert!(evaluate(&bundled(), &ctx).is_empty());
}

#[test]
fn the_sklearn_stub_is_replaced_by_scikit_learn() {
    let mut ctx = venv_ctx(12);
    ctx.declared = vec![req("sklearn", "")];
    let found = evaluate(&bundled(), &ctx);
    assert_eq!(found.len(), 1);
    assert_eq!(
        found[0].title,
        "sklearn is a dead name: install scikit-learn instead"
    );
    assert_eq!(
        found[0].fix,
        FixKind::ReplaceDist(Replacement {
            uninstall: vec![],
            install: vec!["scikit-learn".into()],
            drop: vec!["sklearn".into()],
        })
    );
}

// --- pkg_resources --------------------------------------------------------------

/// The cap the table gives for `import pkg_resources` (`setuptools<82`).
fn pkg_resources_cap() -> String {
    bundled()
        .module_removal("pkg_resources")
        .unwrap()
        .constraint
        .clone()
}

fn importing(module: &str) -> BTreeMap<String, String> {
    BTreeMap::from([(module.to_string(), "app.py".to_string())])
}

#[test]
fn pkg_resources_with_setuptools_82_asks_for_the_table_cap() {
    // C s7b: setuptools 84 installed, no pkg_resources on disk.
    let mut ctx = venv_ctx(12);
    ctx.imports = importing("pkg_resources");
    ctx.installed = installed(&[("setuptools", "84.2.1"), ("requests", "2.32.3")]);
    ctx.declared = vec![req("requests", ""), req("setuptools", ">=81")];
    let found = evaluate(&bundled(), &ctx);
    assert_eq!(found.len(), 1, "{found:#?}");
    assert_eq!(
        found[0].title,
        "`import pkg_resources` fails: setuptools 84.2.1 no longer ships it"
    );
    assert_eq!(found[0].fix, constraint(&pkg_resources_cap()));
    assert!(found[0].detail.contains("importlib.metadata"));

    // C s7a: setuptools not installed at all.
    ctx.installed = installed(&[("requests", "2.32.3")]);
    ctx.declared = vec![req("requests", "")];
    let found = evaluate(&bundled(), &ctx);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].fix, constraint(&pkg_resources_cap()));

    // Installed and shipping it: silent.
    ctx.installed = installed(&[("setuptools", "80.9.0")]);
    ctx.site_modules = HashSet::from(["pkg_resources".to_string()]);
    assert!(evaluate(&bundled(), &ctx).is_empty());
    // Declared below the removal, not yet installed: setup handles it.
    ctx.site_modules.clear();
    ctx.installed = installed(&[]);
    ctx.declared = vec![req("setuptools", "<81")];
    assert!(evaluate(&bundled(), &ctx).is_empty());
}

#[test]
fn the_guardian_offers_the_constraint_not_the_newest_setuptools() {
    let t = bundled();
    let none = BTreeMap::new();
    assert_eq!(
        rules::install_requirement_for(&t, "pkg_resources", &none),
        Some(pkg_resources_cap())
    );
    let new = BTreeMap::from([("setuptools".to_string(), "84.2.1".to_string())]);
    assert!(rules::install_requirement_for(&t, "pkg_resources", &new).is_some());
    let old = BTreeMap::from([("setuptools".to_string(), "80.9.0".to_string())]);
    assert!(rules::install_requirement_for(&t, "pkg_resources", &old).is_none());
    assert!(rules::install_requirement_for(&t, "yaml", &none).is_none());
}

// --- removed stdlib modules --------------------------------------------------

#[test]
fn removed_stdlib_imports_name_the_last_python_and_the_replacement() {
    // A8: distutils and imp on 3.12.
    let mut ctx = venv_ctx(12);
    ctx.imports = BTreeMap::from([
        ("distutils".to_string(), "main.py".to_string()),
        ("imp".to_string(), "main.py".to_string()),
        ("os".to_string(), "main.py".to_string()),
    ]);
    let found = evaluate(&bundled(), &ctx);
    assert_eq!(found.len(), 2, "{found:#?}");
    let distutils = found
        .iter()
        .find(|f| f.title.contains("distutils"))
        .unwrap();
    assert_eq!(distutils.title, "`distutils` was removed in Python 3.12");
    assert!(distutils
        .detail
        .contains("Python 3.11 is the last that has it"));
    assert!(distutils.detail.contains("does not change the Python"));
    assert_eq!(distutils.fix, constraint("setuptools"));
    let imp = found.iter().find(|f| f.title.contains("imp")).unwrap();
    assert_eq!(imp.fix, constraint("zombie-imp"));

    // On 3.11 both still exist.
    ctx.python = Some(PyVersion::py3(11));
    assert!(evaluate(&bundled(), &ctx).is_empty());
}

#[test]
fn dead_batteries_on_3_13_and_their_providers() {
    let mut ctx = venv_ctx(13);
    ctx.imports = BTreeMap::from([
        ("imghdr".to_string(), "img.py".to_string()),
        ("cgi".to_string(), "web.py".to_string()),
        ("nis".to_string(), "net.py".to_string()),
    ]);
    let found = evaluate(&bundled(), &ctx);
    assert_eq!(found.len(), 3);
    let fix_of = |m: &str| {
        found
            .iter()
            .find(|f| f.title.contains(&format!("`{m}`")))
            .unwrap()
            .fix
            .clone()
    };
    assert_eq!(fix_of("imghdr"), constraint("standard-imghdr"));
    assert_eq!(fix_of("cgi"), constraint("standard-cgi"));
    assert_eq!(fix_of("nis"), FixKind::Manual);

    // The replacement installed, declared, or the module on disk: silent.
    ctx.installed = installed(&[("standard-imghdr", "3.13.0")]);
    ctx.declared = vec![req("standard-cgi", "")];
    ctx.site_modules = HashSet::from(["nis".to_string()]);
    assert!(evaluate(&bundled(), &ctx).is_empty());
}

#[test]
fn setuptools_provides_distutils() {
    let mut ctx = venv_ctx(12);
    ctx.imports = importing("distutils");
    ctx.installed = installed(&[("setuptools", "80.9.0")]);
    assert!(titles(&ctx).is_empty());
}

#[test]
fn pixi_projects_get_findings_with_manual_fixes() {
    let mut ctx = venv_ctx(12);
    ctx.pixi = true;
    ctx.imports = importing("imghdr");
    ctx.python = Some(PyVersion::py3(13));
    let found = evaluate(&bundled(), &ctx);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].fix, FixKind::Manual);
}

#[test]
fn an_empty_table_finds_nothing() {
    let mut ctx = venv_ctx(13);
    ctx.imports = importing("imghdr");
    ctx.installed = installed(&[("pandas", "2.0.3"), ("numpy", "2.2.6")]);
    assert!(evaluate(&Table::default(), &ctx).is_empty());
}

// --- gathering from disk -------------------------------------------------------

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pp-kb-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn project_imports_skip_local_modules_and_venvs() {
    let dir = scratch("imports");
    std::fs::write(
        dir.join("main.py"),
        "import imghdr\nimport parser\nfrom distutils import util\n",
    )
    .unwrap();
    std::fs::write(dir.join("parser.py"), "x = 1\n").unwrap();
    std::fs::create_dir_all(dir.join(".venv/Lib/site-packages")).unwrap();
    std::fs::write(dir.join(".venv/pyvenv.cfg"), "home = x\n").unwrap();
    std::fs::write(
        dir.join(".venv/Lib/site-packages/x.py"),
        "import telnetlib\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("pkg")).unwrap();
    std::fs::write(
        dir.join("pkg/nb.ipynb"),
        r#"{"cells": [{"cell_type": "code", "source": ["import cgi\n", "!pip install x\n"]}], "metadata": {}}"#,
    )
    .unwrap();
    let imports = project_imports(&dir);
    assert_eq!(imports.get("imghdr").map(String::as_str), Some("main.py"));
    assert_eq!(
        imports.get("distutils").map(String::as_str),
        Some("main.py")
    );
    assert_eq!(imports.get("cgi").map(String::as_str), Some("pkg/nb.ipynb"));
    assert!(!imports.contains_key("parser"), "local parser.py");
    assert!(!imports.contains_key("telnetlib"), "venv content");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn markers_are_read_off_both_manifests() {
    let dir = scratch("markers");
    std::fs::write(
        dir.join("pyproject.toml"),
        "[project]\nname = \"x\"\ndependencies = [\"dataclasses; python_version < '3.7'\", \"requests\"]\n[project.optional-dependencies]\nold = [\"enum34 ; python_version<'3.4'\"]\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("requirements.txt"),
        "typing; python_version < '3.5'\nflask\n",
    )
    .unwrap();
    let marked = declared_with_marker(&dir);
    assert_eq!(
        marked,
        HashSet::from([
            "dataclasses".to_string(),
            "enum34".to_string(),
            "typing".to_string()
        ])
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn site_modules_list_packages_and_single_files() {
    let dir = scratch("site");
    let site = dir.join("Lib").join("site-packages");
    std::fs::create_dir_all(site.join("pkg_resources")).unwrap();
    std::fs::create_dir_all(site.join("setuptools-80.9.0.dist-info")).unwrap();
    std::fs::write(site.join("imghdr.py"), "").unwrap();
    let mods = site_modules(&dir);
    assert!(mods.contains("pkg_resources") && mods.contains("imghdr"));
    assert!(!mods.iter().any(|m| m.contains("dist-info")));
    let _ = std::fs::remove_dir_all(&dir);
}

// --- the doctor --fix planner ------------------------------------------------

#[test]
fn the_planner_runs_known_fixes_once_and_nothing_else() {
    use crate::core::{repair, Finding};
    let f = |title: &str, fix: FixKind| Finding {
        severity: Severity::Warning,
        title: title.into(),
        detail: String::new(),
        fix,
    };
    let numpy = FixKind::EditRequirement(crate::core::RequirementEdit {
        file: "pyproject.toml".into(),
        old: String::new(),
        new: "numpy<2".into(),
    });
    let jwt = FixKind::ReplaceDist(Replacement {
        uninstall: vec!["jwt".into()],
        install: vec!["PyJWT".into()],
        drop: vec!["jwt".into()],
    });
    let plan = repair::plan(&[
        f("pandas", numpy.clone()),
        f("matplotlib", numpy.clone()),
        f("jwt", jwt.clone()),
    ]);
    // The constraint is a requirement edit (run first); ReplaceDist is ours.
    assert_eq!(plan.requirements, vec![numpy]);
    assert_eq!(plan.known, vec![jwt]);
    assert!(!plan.is_empty());
    assert!(plan.rebuild.is_none() && !plan.setup && !plan.sync);
    assert_eq!(plan.fixes.len(), 3);
    // Info findings are advice only.
    let info = Finding {
        severity: Severity::Info,
        ..f("configparser", constraint("x"))
    };
    assert!(repair::plan(&[info]).is_empty());
}

#[test]
fn fixes_describe_what_they_change() {
    let jwt = FixKind::ReplaceDist(Replacement {
        uninstall: vec!["jwt".into(), "pyjwt".into()],
        install: vec!["PyJWT".into()],
        drop: vec!["jwt".into()],
    });
    assert_eq!(
        apply::describe(&jwt),
        "drop jwt from the dependencies, then uninstall jwt, pyjwt, then reinstall PyJWT"
    );
}

#[tokio::test]
async fn a_dry_run_plans_the_replacement_without_touching_anything() {
    let dir = scratch("dry-replace");
    std::fs::write(
        dir.join("pyproject.toml"),
        "[project]
name = \"x\"
version = \"0\"
dependencies = [\"jwt\", \"pyjwt\"]
",
    )
    .unwrap();
    let before = std::fs::read_to_string(dir.join("pyproject.toml")).unwrap();
    let fix = FixKind::ReplaceDist(Replacement {
        uninstall: vec!["jwt".into(), "pyjwt".into()],
        install: vec!["PyJWT".into()],
        drop: vec!["jwt".into()],
    });
    let settings = crate::settings::Settings::default();
    let (result, planned) = crate::core::exec::dry_run(apply::apply(&dir, &settings, &fix)).await;
    // No uv on the machine is an error; otherwise remove, uninstall, reinstall.
    if result.is_ok() {
        let names: Vec<&str> = planned.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "uv remove jwt",
                // `uv remove` syncs jwt away; pyjwt is reinstalled clean.
                "uv pip uninstall pyjwt",
                "uv pip install --reinstall PyJWT"
            ]
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("pyproject.toml")).unwrap(),
            before
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// --- removed symbols ----------------------------------------------------------

fn code_use(module: &str, symbol: &str, written: &str, line: u32) -> rules::CodeUse {
    rules::CodeUse {
        module: module.into(),
        symbol: symbol.into(),
        file: "app.py".into(),
        line,
        written: written.into(),
        from_import: written.starts_with("from "),
    }
}

#[test]
fn np_float_with_numpy_1_24_points_at_the_code_and_caps_numpy() {
    // E s08: numpy 1.26 installed, app.py uses np.float.
    let mut ctx = venv_ctx(12);
    ctx.installed = installed(&[("numpy", "1.26.4")]);
    ctx.symbol_uses = vec![code_use("numpy", "float", "np.float", 2)];
    ctx.declared = vec![req("numpy", "")];
    let found = evaluate(&bundled(), &ctx);
    assert_eq!(found.len(), 1, "{found:#?}");
    let f = &found[0];
    assert_eq!(f.title, "`np.float` fails: numpy 1.24 removed it");
    assert!(
        f.detail.contains("app.py:2 uses `np.float`"),
        "{}",
        f.detail
    );
    assert!(f.detail.contains("module 'numpy' has no attribute 'float'"));
    assert_eq!(f.fix, constraint("numpy<1.24"));

    // The project asks for numpy>=1.24 itself: the cap is ruled out, the
    // code has to change.
    ctx.declared = vec![req("numpy", ">=1.24")];
    let found = evaluate(&bundled(), &ctx);
    assert_eq!(found[0].fix, FixKind::Manual);
    assert!(found[0].detail.contains("rules out `numpy<1.24`"));

    // numpy 1.23 still has it: silent.
    ctx.installed = installed(&[("numpy", "1.23.5")]);
    assert!(evaluate(&bundled(), &ctx).is_empty());
}

#[test]
fn a_cap_keeps_the_users_floor() {
    let mut ctx = venv_ctx(12);
    ctx.declared_raw = vec![pyproject_req("numpy>=1.20")];
    ctx.new_requirement_file = Some("pyproject.toml".into());
    assert_eq!(
        ctx.constraint_fix("numpy<1.24"),
        FixKind::EditRequirement(crate::core::RequirementEdit {
            file: "pyproject.toml".into(),
            old: "numpy>=1.20".into(),
            new: "numpy>=1.20,<1.24".into(),
        })
    );
    // A floor at or above the cap cannot stay.
    ctx.declared_raw = vec![pyproject_req("numpy>=1.25")];
    let FixKind::EditRequirement(e) = ctx.constraint_fix("numpy<1.24") else {
        panic!()
    };
    assert_eq!(e.new, "numpy<1.24");
}

#[test]
fn pydantic_2_base_settings_adds_pydantic_settings() {
    // E s09: pydantic>=2 declared, `from pydantic import BaseSettings`.
    let mut ctx = venv_ctx(12);
    ctx.installed = installed(&[("pydantic", "2.8.0")]);
    ctx.declared = vec![req("pydantic", ">=2")];
    ctx.symbol_uses = vec![code_use(
        "pydantic",
        "BaseSettings",
        "from pydantic import BaseSettings",
        1,
    )];
    let found = evaluate(&bundled(), &ctx);
    assert_eq!(found.len(), 1, "{found:#?}");
    let f = &found[0];
    assert!(f.detail.contains("pydantic_settings"), "{}", f.detail);
    assert!(f.detail.contains("PydanticImportError"));
    assert_eq!(f.fix, constraint("pydantic-settings"));
    // The fix names the line and the import to write, and says the finding
    // stays until the code changes.
    assert!(
        f.detail
            .contains("Fix: change app.py:1 to `from pydantic_settings import BaseSettings`"),
        "{}",
        f.detail
    );
    assert!(f.detail.contains("stays reported until the code changes"));
    // Nothing is left to the user while PyPilot still has the addition to make.
    assert!(rules::code_changes(&bundled(), &ctx).is_empty());
    // Once pydantic-settings is there, only the code change is left, and
    // the planner lists it after the fix.
    ctx.declared.push(req("pydantic-settings", ""));
    let after = evaluate(&bundled(), &ctx);
    assert_eq!(after[0].fix, FixKind::Manual);
    assert!(after[0]
        .detail
        .contains("Fix: change app.py:1 to `from pydantic_settings import BaseSettings`"));
    assert_eq!(
        rules::code_changes(&bundled(), &ctx),
        vec!["`from pydantic import BaseSettings` fails: pydantic 2 removed it: change app.py:1 to `from pydantic_settings import BaseSettings` (from the pydantic-settings package)".to_string()]
    );
    // Code that no longer uses it: nothing left.
    ctx.symbol_uses.clear();
    assert!(rules::code_changes(&bundled(), &ctx).is_empty());
}

#[test]
fn stdlib_symbols_name_the_last_python_and_the_code_change() {
    // E s10: collections.Mapping on 3.10.
    let mut ctx = venv_ctx(10);
    ctx.symbol_uses = vec![code_use("collections", "Mapping", "collections.Mapping", 3)];
    let found = evaluate(&bundled(), &ctx);
    assert_eq!(found.len(), 1, "{found:#?}");
    let f = &found[0];
    assert_eq!(f.severity, Severity::Warning);
    assert_eq!(f.title, "`collections.Mapping` was removed in Python 3.10");
    assert!(f.detail.contains("Python 3.9 is the last"), "{}", f.detail);
    assert!(f
        .detail
        .contains("app.py:3 uses `collections.Mapping`: write `collections.abc.Mapping`"));
    assert_eq!(f.fix, FixKind::Manual);

    // E s11: asyncio.coroutine on 3.11.
    let mut ctx = venv_ctx(11);
    ctx.symbol_uses = vec![code_use("asyncio", "coroutine", "asyncio.coroutine", 2)];
    let found = evaluate(&bundled(), &ctx);
    assert_eq!(
        found[0].title,
        "`asyncio.coroutine` was removed in Python 3.11"
    );
    assert!(found[0].detail.contains("async def"));

    // On 3.9 it still works; requires-python allowing 3.10 makes it a note.
    let mut ctx = venv_ctx(9);
    ctx.symbol_uses = vec![code_use("collections", "Mapping", "collections.Mapping", 3)];
    assert!(evaluate(&bundled(), &ctx).is_empty());
    ctx.requires_python = Some(">=3.9".into());
    let found = evaluate(&bundled(), &ctx);
    assert_eq!(found[0].severity, Severity::Info);
    ctx.requires_python = Some(">=3.9,<3.10".into());
    assert!(evaluate(&bundled(), &ctx).is_empty());
}

#[test]
fn distutils_on_3_11_warns_when_requires_python_allows_3_12() {
    // E c12c: the venv runs 3.11, where distutils exists, but `>=3.9`
    // lets the project move to 3.12.
    let mut ctx = venv_ctx(11);
    ctx.imports = importing("distutils");
    assert!(evaluate(&bundled(), &ctx).is_empty());
    ctx.requires_python = Some(">=3.9".into());
    let found = evaluate(&bundled(), &ctx);
    assert_eq!(found.len(), 1, "{found:#?}");
    assert_eq!(
        found[0].title,
        "`distutils` fails on Python 3.12 and newer, which the project allows"
    );
    assert_eq!(found[0].fix, constraint("setuptools"));
    // Capped below 3.12: nothing to say.
    ctx.requires_python = Some(">=3.9,<3.12".into());
    assert!(evaluate(&bundled(), &ctx).is_empty());
}

/// A resolver that fails any input containing all of `clash`.
struct Clash(Vec<&'static str>);

impl crate::core::resolution::Resolver for Clash {
    async fn resolve(&self, lines: &[String]) -> Option<crate::core::resolution::Outcome> {
        use crate::core::resolution::Outcome;
        if self.0.iter().all(|c| lines.iter().any(|l| l == c)) {
            Some(Outcome::NoSolution("No solution found".into()))
        } else {
            Some(Outcome::Resolved(Vec::new()))
        }
    }
}

#[tokio::test]
async fn an_upgrade_that_does_not_resolve_becomes_the_cap() {
    // E s05: flask 1.1.2 with itsdangerous 2.1.2, and jinja2 pinned at
    // 2.11.3, which flask 2 does not accept.
    let dir = scratch("s05");
    std::fs::write(
        dir.join("pyproject.toml"),
        "[project]\nname = \"s05\"\nversion = \"0.1.0\"\ndependencies = [\"flask==1.1.2\", \"jinja2==2.11.3\", \"itsdangerous==2.1.2\"]\n",
    )
    .unwrap();
    let mut ctx = venv_ctx(12);
    ctx.installed = installed(&[
        ("flask", "1.1.2"),
        ("itsdangerous", "2.1.2"),
        ("jinja2", "2.11.3"),
    ]);
    ctx.declared = vec![
        req("flask", "==1.1.2"),
        req("jinja2", "==2.11.3"),
        req("itsdangerous", "==2.1.2"),
    ];
    ctx.declared_raw = crate::core::req_edit::declared(&dir).reqs;
    ctx.new_requirement_file = Some(dir.join("pyproject.toml"));
    let mut found = evaluate(&bundled(), &ctx);
    assert_eq!(found.len(), 1, "{found:#?}");
    let FixKind::EditRequirement(e) = &found[0].fix else {
        panic!("{:?}", found[0].fix)
    };
    assert_eq!(e.new, "flask>=2.0.0,<3");

    let alternatives = rules::alternatives(&bundled(), &ctx);
    let resolver = Clash(vec!["flask>=2.0.0,<3", "jinja2==2.11.3"]);
    verify_alternatives(&dir, &alternatives, &mut found, &resolver).await;
    let FixKind::EditRequirement(e) = &found[0].fix else {
        panic!("{:?}", found[0].fix)
    };
    assert_eq!(
        (e.old.as_str(), e.new.as_str()),
        ("itsdangerous==2.1.2", "itsdangerous<2.1")
    );
    assert!(
        found[0].detail.contains("does not resolve"),
        "{}",
        found[0].detail
    );
    assert!(!found[0].detail.contains("rules out the cap"));

    // When the upgrade resolves, it stays.
    let mut found = evaluate(&bundled(), &ctx);
    verify_alternatives(&dir, &alternatives, &mut found, &Clash(vec!["nothing"])).await;
    let FixKind::EditRequirement(e) = &found[0].fix else {
        panic!()
    };
    assert_eq!(e.new, "flask>=2.0.0,<3");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn source_scan_finds_symbol_uses_with_file_and_line() {
    let dir = scratch("symbols");
    std::fs::write(
        dir.join("app.py"),
        "import numpy as np\nx = np.zeros(3, dtype=np.float)\n",
    )
    .unwrap();
    std::fs::write(dir.join("other.py"), "np.float\n").unwrap();
    let scan = scan_source(&dir, &bundled().symbol_targets());
    assert_eq!(scan.symbol_uses.len(), 1, "{:?}", scan.symbol_uses);
    let u = &scan.symbol_uses[0];
    assert_eq!(
        (u.file.as_str(), u.line, u.written.as_str()),
        ("app.py", 2, "np.float")
    );
    let _ = std::fs::remove_dir_all(&dir);
}
