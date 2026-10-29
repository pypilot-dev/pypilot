//! Wheel filename tag parsing.
//!
//! The real-world compatibility signal is often *not* `requires_python` (which is
//! frequently loose) but whether a binary wheel for the current interpreter and
//! platform actually exists. mediapipe, tensorflow, onnxruntime, open3d — all
//! ship `cp39`–`cp312` wheels with no `cp313`, so on Python 3.13 they simply
//! cannot install even though `requires_python` says `>=3.8`.
//!
//! A wheel filename is:
//!   `{distribution}-{version}(-{build})?-{pytag}-{abitag}-{platformtag}.whl`
//! where the last three dash-separated fields are the compatibility tags, each of
//! which may itself be dot-compound (`cp39.cp310`, `manylinux1.manylinux2010`).

use crate::core::platform::{Libc, Platform};
use crate::pypi::pyversion::{PyVersion, PyVersionSet, MAX_MINOR};

/// A parsed wheel filename's compatibility tags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WheelTags {
    pub py: Vec<String>,
    pub abi: Vec<String>,
    pub platform: Vec<String>,
}

impl WheelTags {
    /// Parse the three tag fields out of a `.whl` filename. Returns `None` if the
    /// name doesn't look like a wheel.
    pub fn parse(filename: &str) -> Option<WheelTags> {
        let stem = filename.strip_suffix(".whl")?;
        let parts: Vec<&str> = stem.split('-').collect();
        // name, version, [build], py, abi, platform  → at least 5 fields.
        if parts.len() < 5 {
            return None;
        }
        let n = parts.len();
        let split_compound = |s: &str| s.split('.').map(|x| x.to_string()).collect::<Vec<_>>();
        Some(WheelTags {
            py: split_compound(parts[n - 3]),
            abi: split_compound(parts[n - 2]),
            platform: split_compound(parts[n - 1]),
        })
    }

    /// Which Python minor versions does this wheel support *on this platform*?
    /// Empty if no platform tag matches (wheel is for a different OS/arch).
    pub fn supported_on(&self, platform: &Platform) -> PyVersionSet {
        self.supported_on_with(platform, Libc::host())
    }

    /// [`WheelTags::supported_on`] for a Linux system with the given C library.
    pub fn supported_on_with(&self, platform: &Platform, libc: Libc) -> PyVersionSet {
        let platform_ok = self
            .platform
            .iter()
            .any(|t| platform.wheel_tag_matches_with(t, libc));
        if !platform_ok {
            return PyVersionSet::empty();
        }

        // A wheel built only for the free-threaded interpreter (`cp313t`,
        // `abi3t`) does not load into the default GIL build, which is what
        // uv and the python.org installers provide. Its `cp313` python tag
        // must not count as support for 3.13.
        if !self.abi.is_empty() && self.abi.iter().all(|a| is_free_threaded_abi(a)) {
            return PyVersionSet::empty();
        }

        // Stable-ABI wheels (`abi3`) work on their floor CPython minor and every
        // newer minor. Detect the floor from the py tag.
        let is_abi3 = self.abi.iter().any(|a| a == "abi3");

        let mut set = PyVersionSet::empty();
        for tag in &self.py {
            match classify_py_tag(tag) {
                PyTag::Cp(v) if is_abi3 => {
                    for minor in v.minor..=MAX_MINOR {
                        set.insert(PyVersion::py3(minor));
                    }
                }
                PyTag::Cp(v) => set.insert(v),
                PyTag::PurePy3 => {
                    // py3 / py2.py3 → any CPython 3.x.
                    set = set.union(&PyVersionSet::universe());
                }
                PyTag::PyMinor(v) => set.insert(v),
                PyTag::Unknown => {}
            }
        }
        set
    }
}

/// `cp313t`, `cp314t`, `abi3t`: ABIs of the free-threaded (no-GIL) build.
fn is_free_threaded_abi(abi: &str) -> bool {
    if abi == "abi3t" {
        return true;
    }
    abi.strip_prefix("cp")
        .and_then(|rest| rest.strip_suffix('t'))
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

enum PyTag {
    /// Concrete CPython tag: cp312.
    Cp(PyVersion),
    /// Generic pure-python 3: py3 / py2.py3.
    PurePy3,
    /// py38-style tag pinned to a minor.
    PyMinor(PyVersion),
    Unknown,
}

fn classify_py_tag(tag: &str) -> PyTag {
    if let Some(rest) = tag.strip_prefix("cp") {
        return match parse_minor(rest) {
            Some(v) => PyTag::Cp(v),
            None => PyTag::Unknown,
        };
    }
    if tag == "py3" || tag == "py2" {
        return PyTag::PurePy3;
    }
    if let Some(rest) = tag.strip_prefix("py") {
        // py38 → 3.8 ; py3 handled above.
        if rest.len() >= 2 {
            if let Some(v) = parse_minor(rest) {
                return PyTag::PyMinor(v);
            }
        }
        return PyTag::PurePy3;
    }
    PyTag::Unknown
}

/// "312" → 3.12, "39" → 3.9.
fn parse_minor(digits: &str) -> Option<PyVersion> {
    let digits: String = digits.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() < 2 {
        return None;
    }
    let major = digits[..1].parse().ok()?;
    let minor = digits[1..].parse().ok()?;
    Some(PyVersion::new(major, minor))
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

    #[test]
    fn parse_cp312_manylinux() {
        let t = WheelTags::parse("mediapipe-0.10.9-cp312-cp312-manylinux_2_17_x86_64.whl").unwrap();
        assert_eq!(t.py, vec!["cp312"]);
        assert_eq!(t.abi, vec!["cp312"]);
        assert!(t.supported_on(&linux()).contains(PyVersion::py3(12)));
    }

    #[test]
    fn parse_with_build_tag() {
        // Optional build tag between version and pytag.
        let t = WheelTags::parse("foo-1.0-1-cp311-cp311-win_amd64.whl").unwrap();
        assert_eq!(t.py, vec!["cp311"]);
        assert_eq!(t.platform, vec!["win_amd64"]);
    }

    #[test]
    fn pure_python_wheel_supports_all() {
        let t = WheelTags::parse("requests-2.31.0-py3-none-any.whl").unwrap();
        let s = t.supported_on(&linux());
        assert!(s.contains(PyVersion::py3(9)));
        assert!(s.contains(PyVersion::py3(13)));
    }

    #[test]
    fn wrong_platform_yields_empty() {
        let t = WheelTags::parse("foo-1.0-cp312-cp312-win_amd64.whl").unwrap();
        assert!(t.supported_on(&linux()).is_empty());
    }

    #[test]
    fn abi3_wheel_supports_floor_and_up() {
        let t = WheelTags::parse("cryptography-42.0-cp39-abi3-manylinux_2_17_x86_64.whl").unwrap();
        let s = t.supported_on(&linux());
        assert!(s.contains(PyVersion::py3(9)));
        assert!(s.contains(PyVersion::py3(13)));
        assert!(!s.contains(PyVersion::py3(8)));
    }

    /// Python 3.15 is released: pure-Python, abi3 and cp315 wheels must all
    /// count as supporting it, or every project on 3.15 is told to downgrade.
    #[test]
    fn wheels_cover_python_3_15() {
        for wheel in [
            "six-1.17.0-py2.py3-none-any.whl",
            "cryptography-42.0-cp39-abi3-manylinux_2_17_x86_64.whl",
            "numpy-2.5.4-cp315-cp315-manylinux_2_28_x86_64.whl",
        ] {
            let t = WheelTags::parse(wheel).unwrap();
            assert!(
                t.supported_on(&linux()).contains(PyVersion::py3(15)),
                "{wheel} should support Python 3.15"
            );
        }
    }

    /// Free-threaded wheels need the free-threaded interpreter; the stable
    /// ABI wheel that also lists `abi3t` still serves the GIL build.
    #[test]
    fn free_threaded_only_wheels_do_not_count() {
        for wheel in [
            "numpy-2.5.4-cp314-cp314t-manylinux_2_27_x86_64.manylinux_2_28_x86_64.whl",
            "cryptography-50.0.2-cp314-cp314t-manylinux_2_28_x86_64.whl",
        ] {
            let t = WheelTags::parse(wheel).unwrap();
            assert!(t.supported_on(&linux()).is_empty(), "{wheel}");
        }
        let t = WheelTags::parse("cryptography-50.0.2-cp315-abi3.abi3t-manylinux_2_28_x86_64.whl")
            .unwrap();
        assert!(t.supported_on(&linux()).contains(PyVersion::py3(15)));
        let t = WheelTags::parse("numpy-2.5.4-cp314-cp314-manylinux_2_28_x86_64.whl").unwrap();
        assert!(t.supported_on(&linux()).contains(PyVersion::py3(14)));
    }

    #[test]
    fn non_wheel_returns_none() {
        assert!(WheelTags::parse("mediapipe-0.10.9.tar.gz").is_none());
    }
}
