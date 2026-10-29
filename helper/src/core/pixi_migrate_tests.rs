//! Offline tests for [`super`]: recorded prefix.dev mapping documents and
//! `pixi search --json` output (pixi 0.81, trimmed to the fields read)
//! under `tests/fixtures/pixi/`.

use std::collections::HashMap;

use super::*;
use crate::core::pixi::{parse_manifest, ManifestFormat};
use crate::pypi::FixtureSource;

const MAP_NUMPY: &str = include_str!("../../tests/fixtures/pixi/conda-mapping-v1-numpy.json");
const MAP_TORCH: &str = include_str!("../../tests/fixtures/pixi/conda-mapping-v1-torch.json");
const MAP_GDAL: &str = include_str!("../../tests/fixtures/pixi/conda-mapping-v1-gdal.json");
const MAP_OPENCV: &str =
    include_str!("../../tests/fixtures/pixi/conda-mapping-v1-opencv-python.json");
const SEARCH_NUMPY: &str =
    include_str!("../../tests/fixtures/pixi/pixi-0.81-search-numpy-win-64.json");
const SEARCH_GDAL: &str =
    include_str!("../../tests/fixtures/pixi/pixi-0.81-search-gdal-win-64.json");
const SEARCH_REQUESTS: &str =
    include_str!("../../tests/fixtures/pixi/pixi-0.81-search-requests-win-64.json");
const SEARCH_UVLOOP: &str =
    include_str!("../../tests/fixtures/pixi/pixi-0.81-search-uvloop-win-64.json");
const SEARCH_NOT_FOUND: &str =
    include_str!("../../tests/fixtures/pixi/pixi-0.81-search-err-not-found.txt");

/// What prefix.dev returns for a 404 (cached as an empty mapping).
const NOT_PACKAGED: &str = r#"{"conda_versions":{}}"#;

/// Recorded answers. `None` for the mapping means "offline".
#[derive(Default)]
struct FixtureIndex {
    mapping: HashMap<&'static str, &'static str>,
    search: HashMap<&'static str, &'static str>,
    /// `(conda name, platform)` pairs `pixi search` finds nothing for.
    missing: Vec<(&'static str, &'static str)>,
    offline: bool,
}

impl CondaIndex for FixtureIndex {
    async fn conda_name(&self, pypi_name: &str, spec: &str) -> CondaName {
        if self.offline {
            return CondaName::Unknown("offline".into());
        }
        conda_name_from_mapping(self.mapping.get(pypi_name).unwrap_or(&NOT_PACKAGED), spec)
    }

    async fn availability(&self, conda_name: &str, platform: &str, spec: &str) -> Availability {
        if self.missing.contains(&(conda_name, platform)) {
            return Availability::Missing;
        }
        match self.search.get(conda_name) {
            Some(json) => availability_from_search(json, conda_name, spec),
            None => Availability::Unknown,
        }
    }
}

fn index() -> FixtureIndex {
    FixtureIndex {
        mapping: HashMap::from([
            ("numpy", MAP_NUMPY),
            ("torch", MAP_TORCH),
            ("gdal", MAP_GDAL),
            ("opencv-python", MAP_OPENCV),
            // requests is packaged under its own name; recorded inline.
            ("requests", r#"{"conda_versions":{"2.34.2":"requests"}}"#),
        ]),
        search: HashMap::from([
            ("numpy", SEARCH_NUMPY),
            ("gdal", SEARCH_GDAL),
            ("requests", SEARCH_REQUESTS),
        ]),
        ..FixtureIndex::default()
    }
}

fn source() -> FixtureSource {
    let mut s = FixtureSource::new();
    s.load_json("numpy", "tests/fixtures/numpy.json").unwrap();
    s
}

fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pypilot-migrate-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn pep440_specifiers_translate_exactly_or_not_at_all() {
    for (pep440, conda) in [
        ("", Some("*")),
        (">=1.26", Some(">=1.26")),
        (">=1.26, <2", Some(">=1.26,<2")),
        // Exact in both: 1.2 and 1.2.0, not 1.2.1.
        ("==1.2", Some("==1.2")),
        ("==1.2.*", Some("1.2.*")),
        ("!=1.2.*", Some("!=1.2.*")),
        ("~=2.1", Some("~=2.1")),
        ("<3", Some("<3")),
        ("===1.0", None),
        ("==2.0.0rc1", None),
        ("==1.0.post1", None),
        ("==2.1.0+cu121", None),
        ("==1!2.0", None),
        (">=1.*", None),
    ] {
        assert_eq!(pep440_to_conda(pep440).as_deref(), conda, "{pep440}");
    }
}

#[test]
fn pep508_lines() {
    let d =
        parse_pep508("Requests[socks, security] >= 2.0 ; python_version < '3.13'  # x").unwrap();
    assert_eq!(d.name, "requests");
    assert_eq!(d.extras, ["socks", "security"]);
    assert_eq!(d.spec, ">=2.0");
    assert_eq!(d.marker.as_deref(), Some("python_version < '3.13'"));

    let d = parse_pep508("pkg @ https://example.com/pkg-1.0-py3-none-any.whl").unwrap();
    assert_eq!(
        d.url.as_deref(),
        Some("https://example.com/pkg-1.0-py3-none-any.whl")
    );
    assert_eq!(parse_pep508("numpy (>=1.2)").unwrap().spec, ">=1.2");
    assert!(parse_pep508("# comment").is_err());
    assert!(parse_pep508("numpy latest").is_err());
}

#[test]
fn requirements_txt_options_are_kept_or_explained() {
    let r = parse_requirements_txt(
        "--index-url https://example.com/simple\n\
         --extra-index-url=https://download.pytorch.org/whl/cu121\n\
         numpy==2.0.1 \\\n    --hash=sha256:abc\n\
         -e .\n-r dev.txt\n./local/pkg\n\n# comment\nrequests  # inline\n",
    );
    assert_eq!(r.index_url.as_deref(), Some("https://example.com/simple"));
    assert_eq!(
        r.extra_index_urls,
        ["https://download.pytorch.org/whl/cu121"]
    );
    let names: Vec<(&str, &str)> = r
        .deps
        .iter()
        .map(|d| (d.name.as_str(), d.spec.as_str()))
        .collect();
    assert_eq!(names, [("numpy", "==2.0.1"), ("requests", "")]);
    let skipped: Vec<&str> = r.skipped.iter().map(|s| s.line.as_str()).collect();
    assert_eq!(skipped, ["-e .", "-r dev.txt", "./local/pkg"]);
}

#[test]
fn the_mapping_names_the_package_that_ships_the_wanted_release() {
    assert_eq!(
        conda_name_from_mapping(MAP_TORCH, ""),
        CondaName::Found("pytorch".into())
    );
    assert_eq!(
        conda_name_from_mapping(MAP_NUMPY, ">=1.26"),
        CondaName::Found("numpy".into())
    );
    // opencv-python's newest release is py-opencv; 4.x releases were packaged
    // as libopencv.
    assert_eq!(
        conda_name_from_mapping(MAP_OPENCV, ""),
        CondaName::Found("py-opencv".into())
    );
    assert_eq!(
        conda_name_from_mapping(MAP_OPENCV, "==4.10.0"),
        CondaName::Found("libopencv".into())
    );
    // PyPI's 4.10.0.84 is not a release conda-forge packaged.
    assert_eq!(
        conda_name_from_mapping(MAP_OPENCV, "==4.10.0.84"),
        CondaName::NoMatchingVersion
    );
    assert_eq!(
        conda_name_from_mapping(NOT_PACKAGED, ""),
        CondaName::NotPackaged
    );
    assert!(matches!(
        conda_name_from_mapping("<html>", ""),
        CondaName::Unknown(_)
    ));
}

#[test]
fn pixi_search_output_gives_availability_and_pythons() {
    let py = |v: &[u8]| {
        Some(PyVersionSet::from_versions(
            v.iter().map(|m| PyVersion::py3(*m)),
        ))
    };
    assert_eq!(
        availability_from_search(SEARCH_NUMPY, "numpy", ""),
        Availability::Available(py(&[12, 13, 14, 15]))
    );
    assert_eq!(
        availability_from_search(SEARCH_GDAL, "gdal", ""),
        Availability::Available(py(&[11, 12, 13, 14, 15]))
    );
    // A noarch build with `python >=3.10`, plus old per-Python builds.
    let Availability::Available(Some(req)) =
        availability_from_search(SEARCH_REQUESTS, "requests", "")
    else {
        panic!("requests is on conda-forge");
    };
    assert!(req.contains(PyVersion::py3(10)) && req.contains(PyVersion::py3(15)));
    // Only the old builds admit `<2.24`.
    assert_eq!(
        availability_from_search(SEARCH_REQUESTS, "requests", "<2.24"),
        Availability::Available(py(&[7, 8]))
    );
    assert_eq!(
        availability_from_search(SEARCH_NUMPY, "numpy", "<2"),
        Availability::Missing
    );
    // pixi's fuzzy match finds hypercorn-uvloop, which is not uvloop.
    assert_eq!(
        availability_from_search(SEARCH_UVLOOP, "uvloop", ""),
        Availability::Missing
    );
    assert!(search_found_nothing(SEARCH_NOT_FOUND));
    assert!(!search_found_nothing("Error: network unreachable"));
}

#[tokio::test]
async fn a_requirements_project_becomes_a_pixi_manifest() {
    let dir = tempdir("reqs");
    let reqs = "numpy>=1.26\nrequests[socks]>=2\nmediapipe==0.10.14\ngdal\ntorch==2.1.0+cu121\n\
                pywin32; sys_platform == 'win32'\n-e .\n";
    let pyproject = "[project]\nname = \"demo\"\nversion = \"0.1.0\"\nrequires-python = \">=3.10\"\n\
                     dependencies = [\"numpy<3\"]\n\n[project.optional-dependencies]\ndev = [\"pytest\"]\n";
    std::fs::write(dir.join("requirements.txt"), reqs).unwrap();
    std::fs::write(dir.join("pyproject.toml"), pyproject).unwrap();

    let opts = MigrateOptions {
        extra_platforms: vec!["linux-64".into(), "osx-arm64".into()],
        ..MigrateOptions::default()
    };
    let mut idx = index();
    idx.missing.push(("gdal", "osx-arm64"));
    let m = migrate_with(&dir, &source(), &idx, None, &opts)
        .await
        .unwrap();

    // A dry run writes nothing.
    assert!(!m.written);
    assert!(!dir.join("pixi.toml").exists());
    assert!(m
        .preview
        .starts_with("--- /dev/null\n+++ b/pixi.toml\n@@ -0,0 +1,"));
    assert!(m.preview.lines().skip(3).all(|l| l.starts_with('+')));
    assert_eq!(
        m.next_command.as_deref(),
        Some("pypilot migrate-to-pixi --write")
    );

    let p = parse_manifest(
        &m.manifest,
        ManifestFormat::PixiToml,
        &dir,
        dir.join("pixi.toml"),
    )
    .expect("the manifest parses as pixi's");
    let mut platforms = vec![current_platform().to_string()];
    for extra in ["linux-64", "osx-arm64"] {
        if !platforms.iter().any(|p| p == extra) {
            platforms.push(extra.to_string());
        }
    }
    assert_eq!(p.platforms, platforms);
    // numpy is on conda-forge; both declarations must hold.
    let conda: Vec<(&str, &str)> = p
        .conda_deps
        .iter()
        .map(|d| (d.name.as_str(), d.spec.as_str()))
        .collect();
    assert_eq!(conda, [("numpy", ">=1.26,<3")]);
    let pypi: Vec<String> = p.pypi_deps.iter().map(|r| r.to_string()).collect();
    assert!(pypi.contains(&"mediapipe==0.10.14".to_string()), "{pypi:?}");
    assert!(pypi.contains(&"requests>=2".to_string()), "{pypi:?}");
    // gdal has no osx-arm64 build here, the torch pin has a local version.
    assert!(pypi.contains(&"gdal".to_string()), "{pypi:?}");
    assert!(pypi.contains(&"torch==2.1.0+cu121".to_string()), "{pypi:?}");
    assert!(m
        .manifest
        .contains("gdal = \"*\"  # conda-forge has no `gdal` build for osx-arm64"));
    assert!(m.manifest.contains("extras = [\"socks\"]"));
    // Python: the newest numpy (the only resolvable PyPI metadata here) and
    // conda-forge's numpy builds both cover, within requires-python.
    assert_eq!(p.python.as_deref(), Some(m.python.as_str()));
    assert!(m.python.ends_with(".*"));
    let py = PyVersion::parse(m.python.trim_end_matches(".*")).unwrap();
    assert!(py >= PyVersion::py3(12), "{py}");

    // Markers and editable installs are listed, not guessed.
    assert_eq!(m.skipped.len(), 2, "{:?}", m.skipped);
    assert!(m.manifest.contains("#   pywin32; sys_platform == 'win32'"));
    assert!(m.notes.iter().any(|n| n.contains("optional-dependencies")));

    // Written for real: the sources stay byte for byte, the manifest is new.
    let opts = MigrateOptions {
        write: true,
        install: true,
        ..opts
    };
    let m = migrate_with(&dir, &source(), &idx, None, &opts)
        .await
        .unwrap();
    assert!(m.written);
    assert_eq!(
        std::fs::read_to_string(dir.join("pixi.toml")).unwrap(),
        m.manifest
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("requirements.txt")).unwrap(),
        reqs
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("pyproject.toml")).unwrap(),
        pyproject
    );
    // No pixi: the install step says so, and what to run instead.
    let install = m.install.as_ref().unwrap();
    assert!(!install.ok && !m.ok());
    assert_eq!(install.steps[0].detail, pixi_manage::MISSING_PIXI);
    assert_eq!(
        m.next_command.as_deref(),
        Some("pypilot install-pixi, then pixi install")
    );
    assert!(!dir.join(".pixi").exists());

    // Now it is a pixi project, and a second run refuses.
    assert!(crate::core::pixi::detect(&dir).is_some());
    let again = migrate_with(&dir, &source(), &idx, None, &opts).await;
    assert!(again
        .unwrap_err()
        .to_string()
        .contains("already is a pixi project"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn offline_everything_stays_on_pypi() {
    let dir = tempdir("offline");
    std::fs::write(dir.join("requirements.txt"), "numpy>=1.26\ngdal\n").unwrap();
    let idx = FixtureIndex {
        offline: true,
        ..FixtureIndex::default()
    };
    let m = plan(
        &dir,
        &FixtureSource::new(),
        &idx,
        &MigrateOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(m.conda_deps().count(), 0);
    assert_eq!(m.pypi_deps().count(), 2);
    assert!(m.notes.iter().any(|n| n.contains("could not be reached")));
    // No PyPI metadata either: pixi chooses Python.
    assert_eq!(m.python, "*");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn refusals() {
    let dir = tempdir("refuse");
    std::fs::write(dir.join("requirements.txt"), "numpy\n").unwrap();
    let bad = MigrateOptions {
        extra_platforms: vec!["amiga-68k".into()],
        ..MigrateOptions::default()
    };
    let err = plan(&dir, &FixtureSource::new(), &index(), &bad)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unknown platform"));

    // An unreadable pixi.toml is still never overwritten.
    std::fs::write(dir.join("pixi.toml"), "not toml [").unwrap();
    let err = plan(
        &dir,
        &FixtureSource::new(),
        &index(),
        &MigrateOptions::default(),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("never overwrites"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn conda_lines_from_environment_yml() {
    let cases = [
        ("numpy", ("numpy", "\"*\"")),
        ("numpy=1.24", ("numpy", "\"1.24.*\"")),
        ("numpy=1.24.*", ("numpy", "\"1.24.*\"")),
        ("numpy==1.24.1", ("numpy", "\"==1.24.1\"")),
        ("gdal>=3.6,<4", ("gdal", "\">=3.6,<4\"")),
        ("numpy 1.24.*", ("numpy", "\"1.24.*\"")),
        (
            "numpy=1.24=py311h1234",
            ("numpy", "{ version = \"1.24.*\", build = \"py311h1234\" }"),
        ),
        (
            "numpy==1.24.1=py311h1234",
            (
                "numpy",
                "{ version = \"==1.24.1\", build = \"py311h1234\" }",
            ),
        ),
        (
            "conda-forge::pytorch=2.1",
            (
                "pytorch",
                "{ version = \"2.1.*\", channel = \"conda-forge\" }",
            ),
        ),
    ];
    for (line, (name, value)) in cases {
        let (n, v, _) = conda_line_to_pixi(line).unwrap();
        assert_eq!((n.as_str(), v.as_str()), (name, value), "{line}");
    }
    assert!(conda_line_to_pixi("numpy[version='>=1.2']").is_err());
}

const ENV_YML: &str = "name: geo-env\n\
channels:\n  - conda-forge\n  - defaults\n\
dependencies:\n  - python=3.11\n  - numpy=1.24\n  - gdal>=3.6\n  - cudatoolkit=11.8   # conda only\n\
  - pytorch::pytorch=2.1\n  - pip\n  - pip:\n      - mediapipe==0.10.14\n      - requests[socks]\n\
variables:\n  GDAL_DATA: /opt/gdal\n";

#[test]
fn environment_yml_is_read() {
    let env = parse_environment_yml(ENV_YML);
    assert_eq!(env.name.as_deref(), Some("geo-env"));
    assert_eq!(env.channels, ["conda-forge", "defaults"]);
    assert_eq!(
        env.conda,
        [
            "python=3.11",
            "numpy=1.24",
            "gdal>=3.6",
            "cudatoolkit=11.8",
            "pytorch::pytorch=2.1",
            "pip"
        ]
    );
    assert_eq!(env.pip, ["mediapipe==0.10.14", "requests[socks]"]);
    assert_eq!(
        env.variables,
        [("GDAL_DATA".to_string(), "/opt/gdal".to_string())]
    );

    let flow = parse_environment_yml(
        "channels: [conda-forge, bioconda]\ndependencies: [numpy, 'scipy>=1']\n",
    );
    assert_eq!(flow.channels, ["conda-forge", "bioconda"]);
    assert_eq!(flow.conda, ["numpy", "scipy>=1"]);
}

#[tokio::test]
async fn an_environment_yml_becomes_a_pixi_manifest() {
    let dir = tempdir("conda");
    std::fs::write(dir.join("environment.yml"), ENV_YML).unwrap();
    let mut opts = MigrateOptions {
        source: MigrationSource::CondaEnvironment,
        ..MigrateOptions::default()
    };
    let dry = migrate_with(
        &dir,
        &FixtureSource::new(),
        &FixtureIndex::default(),
        None,
        &opts,
    )
    .await
    .unwrap();
    assert_eq!(
        dry.next_command.as_deref(),
        Some("pypilot migrate-conda --to pixi --write")
    );
    opts.write = true;
    let m = migrate_with(
        &dir,
        &FixtureSource::new(),
        &FixtureIndex::default(),
        None,
        &opts,
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("environment.yml")).unwrap(),
        ENV_YML
    );
    let p = crate::core::pixi::detect(&dir).expect("now a pixi project");
    assert_eq!(p.python.as_deref(), Some("3.11.*"));
    let conda: Vec<(&str, &str)> = p
        .conda_deps
        .iter()
        .map(|d| (d.name.as_str(), d.spec.as_str()))
        .collect();
    assert_eq!(
        conda,
        [
            ("cudatoolkit", "11.8.*"),
            ("gdal", ">=3.6"),
            ("numpy", "1.24.*"),
            ("pip", "*"),
            ("pytorch", "2.1.*"),
        ]
    );
    let pypi: Vec<String> = p.pypi_deps.iter().map(|r| r.to_string()).collect();
    assert_eq!(pypi, ["mediapipe==0.10.14", "requests"]);
    // `defaults` dropped (with a note), the pinned channel added.
    assert_eq!(m.channels, ["conda-forge", "pytorch"]);
    assert!(m.notes.iter().any(|n| n.contains("defaults")));
    assert!(m
        .manifest
        .contains("[activation.env]\nGDAL_DATA = \"/opt/gdal\""));
    assert!(m.manifest.contains("name = \"geo-env\""));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn setup_with_package_manager_pixi_needs_pixi_and_changes_nothing_without_it() {
    let dir = tempdir("setup");
    std::fs::write(dir.join("requirements.txt"), "numpy\n").unwrap();
    let mut summary = SetupSummary {
        package_manager: PackageManager::Pixi,
        resolved_mode: ResolvedMode::Pixi,
        python: None,
        venv_path: pixi_manage::default_env_path(&dir),
        why: String::new(),
        steps: Vec::new(),
        ok: true,
    };
    let settings = Settings {
        package_manager: PackageManager::Pixi,
        data_refresh_days: 0,
        ..Settings::default()
    };
    let go_on = create_for_setup(&dir, &settings, &FixtureSource::new(), None, &mut summary).await;
    assert!(!go_on && !summary.ok);
    assert_eq!(summary.steps[0].name, pixi_manage::LOCATE_STEP);
    assert_eq!(
        summary.steps[0].detail,
        pixi_manage::MISSING_PIXI_FOR_SETTING
    );
    assert!(!dir.join("pixi.toml").exists());
    let _ = std::fs::remove_dir_all(&dir);
}
