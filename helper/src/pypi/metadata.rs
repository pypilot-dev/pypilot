//! PyPI JSON metadata model and per-package Python-support analysis.
//!
//! We deserialize only the fields we need from `https://pypi.org/pypi/<name>/json`:
//! the `info` block (version + `requires_python`) and `urls` (the distribution
//! files for that latest version). The full `releases` map is huge and we don't
//! need it, so it is skipped entirely.

use serde::{Deserialize, Serialize};

use crate::core::platform::Platform;
use crate::pypi::pyversion::{parse_requires_python, PyVersion, PyVersionSet};
use crate::pypi::wheel::WheelTags;

/// The subset of a PyPI package JSON document PyPilot consumes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageMetadata {
    pub info: Info,
    /// Distribution files for `info.version` (the latest release).
    #[serde(default)]
    pub urls: Vec<DistFile>,
    /// Every published version, needed to resolve a pinned or capped
    /// dependency to the release an installer would actually choose.
    ///
    /// PyPI sends this as a `releases` object whose values are per-version file
    /// lists. Those lists are large and we only want the keys, so they are
    /// discarded while parsing. Serializing writes a plain array, which is why
    /// the deserializer accepts both shapes: the array is what comes back out
    /// of our own cache.
    #[serde(
        rename = "releases",
        default,
        deserialize_with = "deserialize_version_list"
    )]
    pub versions: Vec<String>,
}

/// Accept either PyPI's `{version: [files]}` object or our cached `[version]`
/// array, keeping only the version strings.
fn deserialize_version_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct VersionList;

    impl<'de> serde::de::Visitor<'de> for VersionList {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a releases object or an array of version strings")
        }

        fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
        where
            M: serde::de::MapAccess<'de>,
        {
            let mut out = Vec::new();
            while let Some(key) = map.next_key::<String>()? {
                // Skip the file list without allocating it.
                map.next_value::<serde::de::IgnoredAny>()?;
                out.push(key);
            }
            Ok(out)
        }

        fn visit_seq<S>(self, mut seq: S) -> Result<Self::Value, S::Error>
        where
            S: serde::de::SeqAccess<'de>,
        {
            let mut out = Vec::new();
            while let Some(v) = seq.next_element::<String>()? {
                out.push(v);
            }
            Ok(out)
        }
    }

    deserializer.deserialize_any(VersionList)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Info {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub requires_python: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DistFile {
    pub filename: String,
    /// "bdist_wheel" | "sdist" | ...
    #[serde(default)]
    pub packagetype: String,
    /// Per-file `requires_python`, occasionally set independently of `info`.
    #[serde(default)]
    pub requires_python: Option<String>,
    #[serde(default)]
    pub yanked: bool,
}

impl DistFile {
    pub fn is_wheel(&self) -> bool {
        self.packagetype == "bdist_wheel" || self.filename.ends_with(".whl")
    }

    pub fn is_sdist(&self) -> bool {
        self.packagetype == "sdist"
            || self.filename.ends_with(".tar.gz")
            || self.filename.ends_with(".zip")
    }
}

/// How a package that falls back to its sdist is likely to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SdistKind {
    /// Only an sdist was ever published. Could be pure Python, could be a C
    /// extension; PyPI metadata can't tell, so treat it as "may need a
    /// compiler".
    Unknown,
    /// Wheels exist for other platforms but not this one: the package has
    /// native code and building it here needs a compiler toolchain.
    Native,
}

/// Result of analyzing one package's latest release against a platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PackageAnalysis {
    pub name: String,
    pub version: String,
    /// Python minors this package can actually be installed on, here.
    pub supported: PyVersionSet,
    /// True if the package ships any wheel for this platform.
    pub has_platform_wheels: bool,
    /// True if only an sdist is available (would compile from source).
    pub sdist_only: bool,
    /// True if the release ships wheels, just none that install here. Paired
    /// with `sdist_only` this is the strong "native extension" signal: the
    /// authors build binaries for other platforms, so the sdist needs a
    /// compiler.
    pub has_foreign_wheels: bool,
    /// True if the release ships wheels for other platforms only and no
    /// sdist, so nothing in it installs here at any Python version.
    /// `supported` is then empty. (A release with no files at all is not
    /// flagged: there is nothing to say about which platforms it targets.)
    pub no_files_for_platform: bool,
    /// The platforms the release's wheels do target (`Linux x64`, `macOS`),
    /// for explaining `no_files_for_platform`. Empty when it is not set.
    pub wheel_platforms: Vec<String>,
    /// The `requires_python` string that was in effect (for messaging).
    pub requires_python: Option<String>,
}

impl PackageMetadata {
    /// Compute the set of Python versions this package supports on `platform`.
    ///
    /// Logic:
    /// * Start from `requires_python` (or the full universe if absent).
    /// * If the package ships platform wheels, intersect with the union of those
    ///   wheels' supported minors — this is what catches the "no cp313 wheel" case.
    /// * If there are no platform wheels but an sdist exists, keep the
    ///   `requires_python` set and flag `sdist_only` (compiles from source).
    /// * If there are wheels, none for this platform, and no sdist, nothing
    ///   installs here: the set is empty and `no_files_for_platform` is set.
    /// * With no files at all, keep `requires_python`: there is no evidence
    ///   either way, and an empty set would blame the wrong thing.
    pub fn analyze(&self, platform: &Platform) -> PackageAnalysis {
        let declared = self
            .info
            .requires_python
            .as_deref()
            .map(parse_requires_python)
            .unwrap_or_else(PyVersionSet::universe);

        let mut wheel_union = PyVersionSet::empty();
        let mut has_platform_wheels = false;
        let mut has_any_wheel = false;
        let mut has_sdist = false;
        let mut foreign_platforms: Vec<String> = Vec::new();

        for file in &self.urls {
            if file.yanked {
                continue;
            }
            if file.is_wheel() {
                has_any_wheel = true;
                if let Some(tags) = WheelTags::parse(&file.filename) {
                    let s = tags.supported_on(platform);
                    if !s.is_empty() {
                        has_platform_wheels = true;
                        wheel_union = wheel_union.union(&s);
                    } else {
                        for label in tags.platform.iter().map(|t| platform_label(t)) {
                            if !foreign_platforms.contains(&label) {
                                foreign_platforms.push(label);
                            }
                        }
                    }
                }
            } else if file.is_sdist() {
                has_sdist = true;
            }
        }

        let sdist_only = !has_platform_wheels && has_sdist;
        let has_foreign_wheels = has_any_wheel && !has_platform_wheels;
        let no_files_for_platform = has_foreign_wheels && !has_sdist;

        let supported = if has_platform_wheels {
            declared.intersect(&wheel_union)
        } else if no_files_for_platform {
            // Only other platforms' wheels: no Python can install it here.
            PyVersionSet::empty()
        } else {
            // sdist-only (compiles from source) or no files at all: the
            // interpreter constraint is whatever requires_python says.
            declared
        };

        foreign_platforms.sort();

        PackageAnalysis {
            name: self.info.name.clone(),
            version: self.info.version.clone(),
            supported,
            has_platform_wheels,
            sdist_only,
            has_foreign_wheels,
            no_files_for_platform,
            wheel_platforms: if no_files_for_platform {
                foreign_platforms
            } else {
                Vec::new()
            },
            requires_python: self.info.requires_python.clone(),
        }
    }
}

/// Human name for a wheel platform tag: `manylinux_2_17_x86_64` is `Linux x64`.
/// Unrecognised tags are shown as they are.
fn platform_label(tag: &str) -> String {
    let arch = if tag.ends_with("x86_64") || tag.ends_with("amd64") {
        " x64"
    } else if tag.ends_with("aarch64") || tag.ends_with("arm64") {
        " arm64"
    } else if tag.ends_with("i686") || tag == "win32" {
        " x86"
    } else {
        ""
    };
    let os =
        if tag.starts_with("manylinux") || tag.starts_with("musllinux") || tag.starts_with("linux")
        {
            "Linux"
        } else if tag.starts_with("win") {
            "Windows"
        } else if tag.starts_with("macosx") {
            // Universal and fat builds span arches; the OS is what matters.
            return "macOS".to_string();
        } else {
            return tag.to_string();
        };
    format!("{os}{arch}")
}

impl PackageAnalysis {
    /// The user-facing reason this release cannot be installed on `here`,
    /// when `no_files_for_platform` is set.
    pub fn no_files_message(&self, here: &str) -> Option<String> {
        if !self.no_files_for_platform {
            return None;
        }
        // Wheels tagged for this platform that still don't count are for
        // another interpreter (PyPy, GraalPy).
        let only = if self.wheel_platforms.is_empty()
            || self.wheel_platforms.iter().any(|p| p == here || p == "any")
        {
            "none of its wheels are for CPython".to_string()
        } else {
            format!("only {} wheels", self.wheel_platforms.join(", "))
        };
        Some(format!(
            "{} {} publishes no files for {here} ({only}, no source distribution), so it cannot be installed here.",
            self.name, self.version
        ))
    }

    /// How the sdist fallback builds, or `None` when wheels cover this
    /// platform (or there is no sdist to fall back to).
    pub fn sdist_kind(&self) -> Option<SdistKind> {
        if !self.sdist_only {
            None
        } else if self.has_foreign_wheels {
            Some(SdistKind::Native)
        } else {
            Some(SdistKind::Unknown)
        }
    }

    /// The Python versions `requires_python` admits, before wheel tags narrow
    /// it. The universe when the package doesn't declare one.
    pub fn declared(&self) -> PyVersionSet {
        self.requires_python
            .as_deref()
            .map(parse_requires_python)
            .unwrap_or_else(PyVersionSet::universe)
    }

    /// True when `py` is excluded only because no wheel exists for it here:
    /// `requires_python` allows it and other Pythons do get wheels. Installing
    /// on `py` would fall back to compiling the sdist.
    pub fn lacks_wheel_for(&self, py: PyVersion) -> bool {
        self.has_platform_wheels && !self.supported.contains(py) && self.declared().contains(py)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::platform::{Arch, Os};

    fn linux() -> Platform {
        Platform {
            os: Os::Linux,
            arch: Arch::X86_64,
        }
    }

    fn wheel(name: &str) -> DistFile {
        DistFile {
            filename: name.to_string(),
            packagetype: "bdist_wheel".to_string(),
            requires_python: None,
            yanked: false,
        }
    }

    #[test]
    fn wheel_tags_narrow_requires_python() {
        // requires_python loose (>=3.8) but wheels only up to cp312.
        let meta = PackageMetadata {
            info: Info {
                name: "mediapipe".into(),
                version: "0.10.9".into(),
                requires_python: Some(">=3.8".into()),
            },
            urls: vec![
                wheel("mediapipe-0.10.9-cp39-cp39-manylinux_2_17_x86_64.whl"),
                wheel("mediapipe-0.10.9-cp312-cp312-manylinux_2_17_x86_64.whl"),
            ],
            versions: vec!["0.10.9".into()],
        };
        let a = meta.analyze(&linux());
        assert!(a.has_platform_wheels);
        assert!(a.supported.contains(PyVersion::py3(9)));
        assert!(a.supported.contains(PyVersion::py3(12)));
        assert!(!a.supported.contains(PyVersion::py3(13)));
    }

    #[test]
    fn sdist_only_keeps_requires_python_and_flags() {
        let meta = PackageMetadata {
            info: Info {
                name: "somelib".into(),
                version: "1.0".into(),
                requires_python: Some(">=3.9".into()),
            },
            urls: vec![DistFile {
                filename: "somelib-1.0.tar.gz".into(),
                packagetype: "sdist".into(),
                requires_python: None,
                yanked: false,
            }],
            versions: vec!["1.0".into()],
        };
        let a = meta.analyze(&linux());
        assert!(a.sdist_only);
        assert!(!a.has_platform_wheels);
        assert!(a.supported.contains(PyVersion::py3(13)));
        assert_eq!(a.sdist_kind(), Some(SdistKind::Unknown));
    }

    fn sdist(name: &str) -> DistFile {
        DistFile {
            filename: name.to_string(),
            packagetype: "sdist".into(),
            requires_python: None,
            yanked: false,
        }
    }

    #[test]
    fn wheels_for_other_platforms_mean_native_build() {
        // Windows-only wheels; on Linux the sdist needs a compiler.
        let meta = PackageMetadata {
            info: Info {
                name: "nativelib".into(),
                version: "2.0".into(),
                requires_python: Some(">=3.9".into()),
            },
            urls: vec![
                wheel("nativelib-2.0-cp312-cp312-win_amd64.whl"),
                sdist("nativelib-2.0.tar.gz"),
            ],
            versions: vec!["2.0".into()],
        };
        let a = meta.analyze(&linux());
        assert!(a.sdist_only);
        assert!(a.has_foreign_wheels);
        assert_eq!(a.sdist_kind(), Some(SdistKind::Native));
    }

    #[test]
    fn platform_wheels_mean_no_sdist_kind() {
        let meta = PackageMetadata {
            info: Info {
                name: "mediapipe".into(),
                version: "0.10.9".into(),
                requires_python: Some(">=3.8".into()),
            },
            urls: vec![
                wheel("mediapipe-0.10.9-cp312-cp312-manylinux_2_17_x86_64.whl"),
                sdist("mediapipe-0.10.9.tar.gz"),
            ],
            versions: vec!["0.10.9".into()],
        };
        let a = meta.analyze(&linux());
        assert_eq!(a.sdist_kind(), None);
        assert!(!a.has_foreign_wheels);
        // 3.13 is allowed by requires_python but has no wheel: a source build.
        assert!(a.lacks_wheel_for(PyVersion::py3(13)));
        // 3.7 is excluded by requires_python itself, not by a missing wheel.
        assert!(!a.lacks_wheel_for(PyVersion::py3(7)));
        assert!(!a.lacks_wheel_for(PyVersion::py3(12)));
    }

    fn windows() -> Platform {
        Platform {
            os: Os::Windows,
            arch: Arch::X86_64,
        }
    }

    /// tensorflow 2.15.0.post1: manylinux x86_64 wheels only, no sdist.
    fn linux_only_release() -> PackageMetadata {
        PackageMetadata {
            info: Info {
                name: "tensorflow".into(),
                version: "2.15.0.post1".into(),
                requires_python: Some(">=3.9".into()),
            },
            urls: vec![
                wheel("tensorflow-2.15.0.post1-cp39-cp39-manylinux_2_17_x86_64.manylinux2014_x86_64.whl"),
                wheel("tensorflow-2.15.0.post1-cp311-cp311-manylinux_2_17_x86_64.manylinux2014_x86_64.whl"),
            ],
            versions: vec!["2.15.0.post1".into()],
        }
    }

    #[test]
    fn foreign_wheels_without_sdist_support_nothing_here() {
        let a = linux_only_release().analyze(&windows());
        assert!(a.no_files_for_platform);
        assert!(a.supported.is_empty());
        assert!(!a.sdist_only);
        assert_eq!(a.sdist_kind(), None);
        assert_eq!(a.wheel_platforms, vec!["Linux x64".to_string()]);
        assert_eq!(
            a.no_files_message(windows().label()).unwrap(),
            "tensorflow 2.15.0.post1 publishes no files for Windows x64 (only Linux x64 wheels, no source distribution), so it cannot be installed here."
        );
        // Nothing is missing a wheel for one Python: nothing installs at all.
        assert!(!a.lacks_wheel_for(PyVersion::py3(11)));
    }

    #[test]
    fn the_same_release_is_fine_where_its_wheels_are() {
        let a = linux_only_release().analyze(&linux());
        assert!(!a.no_files_for_platform);
        assert!(a.wheel_platforms.is_empty());
        assert_eq!(a.no_files_message(linux().label()), None);
        assert!(a.supported.contains(PyVersion::py3(11)));
    }

    #[test]
    fn foreign_wheels_with_sdist_stay_a_source_build() {
        let mut meta = linux_only_release();
        meta.urls.push(sdist("tensorflow-2.15.0.post1.tar.gz"));
        let a = meta.analyze(&windows());
        assert!(!a.no_files_for_platform);
        assert!(a.sdist_only);
        assert!(a.supported.contains(PyVersion::py3(12)));
    }

    #[test]
    fn no_files_at_all_keeps_requires_python() {
        let mut meta = linux_only_release();
        meta.urls.clear();
        let a = meta.analyze(&windows());
        assert!(!a.no_files_for_platform);
        assert!(!a.sdist_only);
        assert!(a.supported.contains(PyVersion::py3(12)));
        assert!(!a.supported.contains(PyVersion::py3(8)));
    }

    #[test]
    fn yanked_sdist_does_not_count() {
        let mut meta = linux_only_release();
        let mut yanked = sdist("tensorflow-2.15.0.post1.tar.gz");
        yanked.yanked = true;
        meta.urls.push(yanked);
        assert!(meta.analyze(&windows()).no_files_for_platform);
    }

    #[test]
    fn wheel_platform_labels() {
        let mut meta = linux_only_release();
        meta.urls = vec![
            wheel("x-1.0-cp312-cp312-macosx_11_0_arm64.whl"),
            wheel("x-1.0-cp312-cp312-macosx_10_9_universal2.whl"),
            wheel("x-1.0-cp312-cp312-manylinux_2_17_aarch64.whl"),
        ];
        let a = meta.analyze(&windows());
        assert_eq!(a.wheel_platforms, vec!["Linux arm64", "macOS"]);
    }

    #[test]
    fn pypy_only_wheels_say_so() {
        let mut meta = linux_only_release();
        meta.urls = vec![wheel("x-1.0-pp310-pypy310_pp73-win_amd64.whl")];
        let a = meta.analyze(&windows());
        assert!(a.no_files_for_platform);
        assert!(a
            .no_files_message(windows().label())
            .unwrap()
            .contains("none of its wheels are for CPython"));
    }
}
