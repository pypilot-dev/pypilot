//! Cheap, offline suggestions for requirements that cannot be installed.
//!
//! * A name PyPI does not know: the closest well-known name ("did you mean
//!   `requests`?"), from the import map and a small embedded list of
//!   popular distributions. No index is downloaded.
//! * A version PyPI does not have: the nearest releases that exist, from the
//!   release list the PyPI JSON already carries, and the closest valid pin.
//! * A package that does not support this operating system at all (uvloop on
//!   Windows): from its `Operating System ::` classifiers, plus a tiny list
//!   for packages whose classifiers say nothing.
//!
//! Everything here is pure, so each rule is tested without a network.

use crate::core::platform::Os;
use crate::core::project::normalize_name;
use crate::pypi::version::PackageVersion;

/// Distributions people install most, for spelling suggestions. Only names
/// whose import name matches or that the import map does not cover need to
/// be here; the map's own values are added at lookup time.
const POPULAR: &[&str] = &[
    "aiohttp",
    "alembic",
    "anyio",
    "attrs",
    "azure-core",
    "beautifulsoup4",
    "black",
    "boto3",
    "botocore",
    "celery",
    "certifi",
    "cffi",
    "charset-normalizer",
    "click",
    "colorama",
    "cryptography",
    "cython",
    "dask",
    "django",
    "djangorestframework",
    "docker",
    "fastapi",
    "filelock",
    "flake8",
    "flask",
    "fsspec",
    "gunicorn",
    "h5py",
    "httpx",
    "huggingface-hub",
    "idna",
    "jinja2",
    "joblib",
    "jsonschema",
    "jupyter",
    "jupyterlab",
    "keras",
    "lightgbm",
    "loguru",
    "lxml",
    "markdown",
    "matplotlib",
    "mypy",
    "networkx",
    "nltk",
    "notebook",
    "numba",
    "numpy",
    "openai",
    "opencv-python",
    "openpyxl",
    "packaging",
    "pandas",
    "paramiko",
    "pillow",
    "pip",
    "plotly",
    "poetry",
    "polars",
    "psutil",
    "psycopg2-binary",
    "pyarrow",
    "pydantic",
    "pydantic-settings",
    "pygments",
    "pyjwt",
    "pymongo",
    "pymysql",
    "pyspark",
    "pytest",
    "pytest-cov",
    "python-dateutil",
    "python-dotenv",
    "pytz",
    "pyyaml",
    "redis",
    "requests",
    "rich",
    "ruff",
    "scikit-image",
    "scikit-learn",
    "scipy",
    "seaborn",
    "selenium",
    "setuptools",
    "six",
    "sqlalchemy",
    "starlette",
    "streamlit",
    "sympy",
    "tensorflow",
    "tokenizers",
    "toml",
    "torch",
    "torchaudio",
    "torchvision",
    "tqdm",
    "transformers",
    "typer",
    "typing-extensions",
    "urllib3",
    "uvicorn",
    "uvloop",
    "virtualenv",
    "websockets",
    "werkzeug",
    "wheel",
    "xgboost",
];

/// Packages that build or install on some operating systems only, whose
/// classifiers do not say so (or that people hit often enough to not wait
/// for a metadata fetch). Each lists the systems it does not support.
const PLATFORM_ONLY: &[(&str, &[Os])] = &[
    ("uvloop", &[Os::Windows]),
    ("pyinotify", &[Os::Windows, Os::Macos]),
    ("systemd-python", &[Os::Windows, Os::Macos]),
    ("python-prctl", &[Os::Windows, Os::Macos]),
    ("pyobjc", &[Os::Windows, Os::Linux]),
    ("pyobjc-core", &[Os::Windows, Os::Linux]),
];

/// Optimal string alignment distance (Levenshtein plus adjacent swaps), so
/// `reqeusts` is one edit from `requests`.
fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = best;
        }
    }
    d[a.len()][b.len()]
}

/// The well-known distribution `name` is most likely a misspelling of, or
/// the distribution an import name belongs to (`yaml` -> `PyYAML`).
///
/// `import_map` is the bundled import-name table as (import, distribution)
/// pairs. Names one or two edits away qualify (one for names of four
/// letters or fewer, where two edits reach too many real names). Ties go to
/// the earlier candidate, popular names first.
pub fn did_you_mean<'a, I>(name: &str, import_map: I) -> Option<String>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let wanted = normalize_name(name);
    let mut candidates: Vec<String> = POPULAR.iter().map(|s| s.to_string()).collect();
    for (import, dist) in import_map {
        // An import name used as a distribution name: the table is the answer.
        if normalize_name(import) == wanted && normalize_name(dist) != wanted {
            return Some(dist.to_string());
        }
        candidates.push(dist.to_string());
    }
    let limit = if wanted.chars().count() <= 4 { 1 } else { 2 };
    candidates
        .into_iter()
        .filter(|c| normalize_name(c) != wanted)
        .map(|c| (distance(&wanted, &normalize_name(&c)), c))
        .filter(|(d, _)| *d <= limit)
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

/// What to say about a pin PyPI has no release for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NearestVersions {
    /// Up to four existing releases around the requested one, oldest first.
    pub nearby: Vec<String>,
    /// The replacement specifier, e.g. `==2.34.2`, or empty to drop the pin.
    pub spec: String,
}

/// The releases nearest the version `spec` asks for, and the closest valid
/// replacement for it. `versions` is every release PyPI lists. Pre-releases
/// count only when the specifier names one.
pub fn nearest_versions(spec: &str, versions: &[String]) -> Option<NearestVersions> {
    let spec = spec.trim();
    let op_len = spec
        .find(|c: char| c.is_ascii_alphanumeric())
        .filter(|&i| i > 0)?;
    let (op, wanted) = spec.split_at(op_len);
    // One clause only: a range has no single "nearest" answer.
    if wanted.contains(',') {
        return None;
    }
    let wanted_v = PackageVersion::parse(wanted.trim_end_matches(".*"))?;
    let mut all: Vec<PackageVersion> = versions
        .iter()
        .filter_map(|v| PackageVersion::parse(v))
        .filter(|v| wanted_v.is_prerelease() || !v.is_prerelease())
        .collect();
    if all.is_empty() {
        return None;
    }
    all.sort();
    all.dedup_by(|a, b| a == b);
    let split = all.partition_point(|v| *v <= wanted_v);
    let below = &all[..split];
    let above = &all[split..];
    let mut nearby: Vec<String> = below
        .iter()
        .rev()
        .take(if above.is_empty() { 4 } else { 3 })
        .rev()
        .map(|v| v.raw.clone())
        .collect();
    nearby.extend(above.iter().take(4 - nearby.len()).map(|v| v.raw.clone()));
    let closest = below.last().or_else(|| above.first())?;
    let spec = match op.trim() {
        "==" | "===" | "~=" | ">=" | "<=" => format!("{}{}", op.trim(), closest.raw),
        _ => String::new(),
    };
    Some(NearestVersions { nearby, spec })
}

/// The `sys_platform` value an environment marker uses for `os`.
pub fn sys_platform(os: Os) -> &'static str {
    match os {
        Os::Windows => "win32",
        Os::Macos => "darwin",
        Os::Linux => "linux",
    }
}

/// Does `name` lack support for `os`, by the embedded list or by its
/// `Operating System ::` classifiers?
///
/// Classifiers decide only when they name at least one system and none of
/// them covers `os` (`OS Independent` covers all, `POSIX` covers Linux and
/// macOS). A package without any is assumed portable.
pub fn unsupported_on(name: &str, os: Os, classifiers: &[String]) -> bool {
    let wanted = normalize_name(name);
    if PLATFORM_ONLY
        .iter()
        .any(|(n, systems)| *n == wanted && systems.contains(&os))
    {
        return true;
    }
    let systems: Vec<&str> = classifiers
        .iter()
        .filter_map(|c| c.strip_prefix("Operating System ::"))
        .map(str::trim)
        .collect();
    if systems.is_empty() {
        return false;
    }
    let covers = |s: &str| {
        let s = s.to_ascii_lowercase();
        if s.starts_with("os independent") {
            return true;
        }
        match os {
            Os::Windows => s.contains("windows") || s.starts_with("microsoft"),
            Os::Macos => s.starts_with("macos") || s.starts_with("posix"),
            Os::Linux => s.starts_with("posix") || s.contains("linux") || s.starts_with("unix"),
        }
    };
    !systems.iter().any(|s| covers(s))
}

/// `requirement` with a marker that skips it on `os`. An existing marker is
/// kept and both must hold.
pub fn with_platform_marker(requirement: &str, os: Os) -> String {
    let skip = format!("sys_platform != '{}'", sys_platform(os));
    match requirement.split_once(';') {
        Some((req, marker)) if !marker.trim().is_empty() => {
            format!("{}; ({}) and {skip}", req.trim_end(), marker.trim())
        }
        Some((req, _)) => format!("{}; {skip}", req.trim_end()),
        None => format!("{}; {skip}", requirement.trim_end()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> Vec<(&'static str, &'static str)> {
        vec![
            ("yaml", "PyYAML"),
            ("PIL", "pillow"),
            ("sklearn", "scikit-learn"),
        ]
    }

    #[test]
    fn typos_get_the_nearest_popular_name() {
        assert_eq!(did_you_mean("reqests", map()).as_deref(), Some("requests"));
        assert_eq!(did_you_mean("reqeusts", map()).as_deref(), Some("requests"));
        assert_eq!(did_you_mean("nunpy", map()).as_deref(), Some("numpy"));
        assert_eq!(did_you_mean("Flaks", map()).as_deref(), Some("flask"));
        assert_eq!(
            did_you_mean("scikit_lean", map()).as_deref(),
            Some("scikit-learn")
        );
        // Import names point at their distribution.
        assert_eq!(did_you_mean("yaml", map()).as_deref(), Some("PyYAML"));
        // Nothing close: no guess.
        assert_eq!(did_you_mean("my-private-lib", map()), None);
        // Short names allow one edit only.
        assert_eq!(did_you_mean("abcd", map()), None);
        // A real name is not its own suggestion.
        assert_eq!(did_you_mean("requests", map()), None);
    }

    fn versions(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn missing_version_lists_the_nearest_releases() {
        let all = versions(&["2.30.0", "2.31.0", "2.32.0", "2.32.3", "2.34.2", "3.0.0b1"]);
        let n = nearest_versions("==9.9.9", &all).unwrap();
        assert_eq!(n.nearby, ["2.31.0", "2.32.0", "2.32.3", "2.34.2"]);
        assert_eq!(n.spec, "==2.34.2");

        let n = nearest_versions("==2.32.1", &all).unwrap();
        assert_eq!(n.nearby, ["2.30.0", "2.31.0", "2.32.0", "2.32.3"]);
        assert_eq!(n.spec, "==2.32.0");

        // Below every release: the oldest one.
        let n = nearest_versions("==0.1", &all).unwrap();
        assert_eq!(n.spec, "==2.30.0");
        assert_eq!(n.nearby.first().map(String::as_str), Some("2.30.0"));

        // `>99` has no single nearest pin: drop it.
        assert_eq!(nearest_versions(">99", &all).unwrap().spec, "");
        assert_eq!(nearest_versions(">=99", &all).unwrap().spec, ">=2.34.2");
        assert_eq!(nearest_versions(">=1,<2", &all), None);
    }

    #[test]
    fn platform_support_from_classifiers_and_the_list() {
        let c = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let posix = c(&[
            "Operating System :: POSIX",
            "Operating System :: MacOS :: MacOS X",
        ]);
        assert!(unsupported_on("somepkg", Os::Windows, &posix));
        assert!(!unsupported_on("somepkg", Os::Linux, &posix));
        assert!(!unsupported_on("somepkg", Os::Macos, &posix));
        assert!(!unsupported_on(
            "somepkg",
            Os::Windows,
            &c(&["Operating System :: OS Independent"])
        ));
        assert!(!unsupported_on(
            "somepkg",
            Os::Windows,
            &c(&[
                "Operating System :: Microsoft :: Windows",
                "Operating System :: POSIX"
            ])
        ));
        // No classifiers: assume portable, unless listed.
        assert!(!unsupported_on("somepkg", Os::Windows, &[]));
        assert!(unsupported_on("uvloop", Os::Windows, &[]));
        assert!(!unsupported_on("uvloop", Os::Linux, &[]));
    }

    #[test]
    fn platform_marker_is_added_or_combined() {
        assert_eq!(
            with_platform_marker("uvloop>=0.19", Os::Windows),
            "uvloop>=0.19; sys_platform != 'win32'"
        );
        assert_eq!(
            with_platform_marker("uvloop>=0.19 ; python_version >= '3.9'", Os::Windows),
            "uvloop>=0.19; (python_version >= '3.9') and sys_platform != 'win32'"
        );
        assert_eq!(
            with_platform_marker("pyobjc", Os::Linux),
            "pyobjc; sys_platform != 'linux'"
        );
    }
}
