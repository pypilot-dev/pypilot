//! Python version values and set algebra over the bounded CPython universe.
//!
//! The whole compatibility problem lives in a small, discrete space: the CPython
//! minor releases a project could realistically target. We model that as
//! `3.MIN_MINOR ..= 3.MAX_MINOR` and treat "which Pythons does package X support"
//! as a set over that universe. `requires_python` (a PEP 440 specifier) and wheel
//! `cpXY` tags each *filter* the universe; the answer for a whole project is the
//! **intersection** of every dependency's set.
//!
//! This is the "equivalent" the plan permits in place of a full `pep440_rs`
//! dependency: because the universe is finite we just evaluate each candidate
//! version against the specifier, which is fully deterministic and trivially
//! testable offline. We only ever reason about `3.x`; Python 2 and 4 are out of
//! scope and excluded by construction.

use std::collections::BTreeSet;
use std::fmt;

/// Lowest CPython minor PyPilot reasons about.
pub const MIN_MINOR: u8 = 7;
/// Highest CPython minor PyPilot reasons about. Bump as new CPython ships.
pub const MAX_MINOR: u8 = 15;

/// A CPython version at minor granularity (major is always 3 in our universe).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PyVersion {
    pub major: u8,
    pub minor: u8,
}

impl PyVersion {
    pub const fn new(major: u8, minor: u8) -> Self {
        PyVersion { major, minor }
    }

    pub const fn py3(minor: u8) -> Self {
        PyVersion { major: 3, minor }
    }

    /// Parse a "3.12" / "3.12.1" / "cp312" style string to a minor version.
    /// Returns `None` for anything that isn't a 3.x version we track.
    pub fn parse(s: &str) -> Option<PyVersion> {
        let s = s.trim();
        if let Some(rest) = s.strip_prefix("cp").or_else(|| s.strip_prefix("pp")) {
            // cp312 -> major 3, minor 12 ; cp39 -> 3.9
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.len() >= 2 {
                let major = digits[..1].parse().ok()?;
                let minor = digits[1..].parse().ok()?;
                return Some(PyVersion { major, minor });
            }
            return None;
        }
        let mut parts = s.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().unwrap_or("0").parse().ok()?;
        Some(PyVersion { major, minor })
    }

    /// The CPython wheel tag for this version, e.g. `cp312`.
    pub fn cp_tag(&self) -> String {
        format!("cp{}{}", self.major, self.minor)
    }
}

impl fmt::Display for PyVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// A set of supported Python versions over the bounded universe.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PyVersionSet {
    minors: BTreeSet<u8>,
}

impl PyVersionSet {
    /// Every CPython minor PyPilot tracks (`3.MIN_MINOR ..= 3.MAX_MINOR`).
    pub fn universe() -> Self {
        PyVersionSet {
            minors: (MIN_MINOR..=MAX_MINOR).collect(),
        }
    }

    pub fn empty() -> Self {
        PyVersionSet::default()
    }

    /// Build from an iterator of `PyVersion`, keeping only 3.x within the universe.
    pub fn from_versions<I: IntoIterator<Item = PyVersion>>(iter: I) -> Self {
        let minors = iter
            .into_iter()
            .filter(|v| v.major == 3 && (MIN_MINOR..=MAX_MINOR).contains(&v.minor))
            .map(|v| v.minor)
            .collect();
        PyVersionSet { minors }
    }

    pub fn insert(&mut self, v: PyVersion) {
        if v.major == 3 && (MIN_MINOR..=MAX_MINOR).contains(&v.minor) {
            self.minors.insert(v.minor);
        }
    }

    pub fn contains(&self, v: PyVersion) -> bool {
        v.major == 3 && self.minors.contains(&v.minor)
    }

    pub fn is_empty(&self) -> bool {
        self.minors.is_empty()
    }

    pub fn len(&self) -> usize {
        self.minors.len()
    }

    /// Set intersection — the workhorse of the whole engine.
    pub fn intersect(&self, other: &PyVersionSet) -> PyVersionSet {
        PyVersionSet {
            minors: self.minors.intersection(&other.minors).copied().collect(),
        }
    }

    /// Set union.
    pub fn union(&self, other: &PyVersionSet) -> PyVersionSet {
        PyVersionSet {
            minors: self.minors.union(&other.minors).copied().collect(),
        }
    }

    pub fn min(&self) -> Option<PyVersion> {
        self.minors.iter().next().map(|&m| PyVersion::py3(m))
    }

    pub fn max(&self) -> Option<PyVersion> {
        self.minors.iter().next_back().map(|&m| PyVersion::py3(m))
    }

    pub fn iter(&self) -> impl Iterator<Item = PyVersion> + '_ {
        self.minors.iter().map(|&m| PyVersion::py3(m))
    }

    /// Human-friendly range, e.g. "3.9–3.12" or "3.9, 3.11" for gaps.
    pub fn to_range_string(&self) -> String {
        if self.minors.is_empty() {
            return "none".to_string();
        }
        // Collapse into contiguous runs.
        let mut runs: Vec<(u8, u8)> = Vec::new();
        for &m in &self.minors {
            match runs.last_mut() {
                Some(run) if run.1 + 1 == m => run.1 = m,
                _ => runs.push((m, m)),
            }
        }
        runs.iter()
            .map(|(a, b)| {
                if a == b {
                    format!("3.{a}")
                } else {
                    format!("3.{a}–3.{b}")
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Parse a PEP 440 `requires_python` specifier into the set of versions it allows.
///
/// Supports the operators that actually appear in `requires_python`:
/// `>= <= > < == != ~=`, comma-joined (all must hold). An empty/None specifier
/// means "no constraint" → the full universe.
pub fn parse_requires_python(spec: &str) -> PyVersionSet {
    let spec = spec.trim();
    if spec.is_empty() {
        return PyVersionSet::universe();
    }

    let clauses: Vec<Clause> = spec
        .split(',')
        .filter_map(|c| Clause::parse(c.trim()))
        .collect();

    if clauses.is_empty() {
        return PyVersionSet::universe();
    }

    let mut set = PyVersionSet::empty();
    for minor in MIN_MINOR..=MAX_MINOR {
        let allowed = patch_candidates(&clauses, minor).iter().any(|&patch| {
            let candidate = [3u64, minor as u64, patch];
            clauses.iter().all(|c| c.matches(&candidate))
        });
        if allowed {
            set.minors.insert(minor);
        }
    }
    set
}

/// Stand-in for "the newest patch release of a minor" when testing clauses.
const LATEST_PATCH: u64 = 999;

/// Patch releases of `3.<minor>` worth testing against `clauses`: a minor is
/// allowed when any of its patch releases is. `.0` alone would drop 3.8 from
/// `>=3.8.1` and 3.9 from `!=3.9.0,!=3.9.1,>=3.9`, though every current 3.8 or
/// 3.9 interpreter satisfies them. The answer can only change at a patch a
/// clause names, so its neighbours plus `.0` and a latest stand-in cover
/// every case.
fn patch_candidates(clauses: &[Clause], minor: u8) -> Vec<u64> {
    let mut out = vec![0, LATEST_PATCH];
    for c in clauses {
        if let [3, m, patch, ..] = c.version[..] {
            if m == u64::from(minor) {
                out.extend([patch.saturating_sub(1), patch, patch + 1]);
            }
        }
    }
    out
}

/// One comparator clause from a specifier, e.g. `>=3.8` or `~=3.9` or `!=3.10.*`.
///
/// Interpreter versions are plain release numbers, so this only compares
/// release segments. Package versions need the full PEP 440 rules (pre-, post-
/// and dev-releases); [`crate::pypi::version`] has its own clause for those.
pub(crate) struct Clause {
    op: Op,
    version: Vec<u64>,
    /// True for `==3.10.*` style prefix-wildcard matching.
    wildcard: bool,
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Op {
    Ge,
    Le,
    Gt,
    Lt,
    Eq,
    Ne,
    Compatible, // ~=
}

impl Clause {
    pub(crate) fn parse(s: &str) -> Option<Clause> {
        let (op, rest) = if let Some(r) = s.strip_prefix(">=") {
            (Op::Ge, r)
        } else if let Some(r) = s.strip_prefix("<=") {
            (Op::Le, r)
        } else if let Some(r) = s.strip_prefix("==") {
            (Op::Eq, r)
        } else if let Some(r) = s.strip_prefix("!=") {
            (Op::Ne, r)
        } else if let Some(r) = s.strip_prefix("~=") {
            (Op::Compatible, r)
        } else if let Some(r) = s.strip_prefix('>') {
            (Op::Gt, r)
        } else if let Some(r) = s.strip_prefix('<') {
            (Op::Lt, r)
        } else {
            // A bare version behaves like `==`.
            (Op::Eq, s)
        };

        let rest = rest.trim();
        let wildcard = rest.ends_with(".*");
        let core = rest.trim_end_matches(".*");
        let version: Vec<u64> = core
            .split('.')
            .filter(|p| !p.is_empty())
            .map(|p| p.parse().ok())
            .collect::<Option<Vec<u64>>>()?;
        if version.is_empty() {
            return None;
        }
        Some(Clause {
            op,
            version,
            wildcard,
        })
    }

    pub(crate) fn matches(&self, candidate: &[u64]) -> bool {
        match self.op {
            Op::Ge => cmp(candidate, &self.version) >= 0,
            Op::Gt => cmp(candidate, &self.version) > 0,
            Op::Le => cmp(candidate, &self.version) <= 0,
            Op::Lt => cmp(candidate, &self.version) < 0,
            Op::Ne => {
                if self.wildcard {
                    !prefix_matches(candidate, &self.version)
                } else {
                    cmp(candidate, &self.version) != 0
                }
            }
            Op::Eq => {
                if self.wildcard {
                    prefix_matches(candidate, &self.version)
                } else {
                    cmp(candidate, &self.version) == 0
                }
            }
            Op::Compatible => {
                // ~=X.Y  => >=X.Y, <X+1
                // ~=X.Y.Z => >=X.Y.Z, <X.Y+1
                if cmp(candidate, &self.version) < 0 {
                    return false;
                }
                let mut upper = self.version.clone();
                if upper.len() >= 2 {
                    let drop_at = upper.len() - 1;
                    upper.truncate(drop_at);
                    let last = upper.len() - 1;
                    upper[last] += 1;
                } else {
                    upper[0] += 1;
                }
                cmp(candidate, &upper) < 0
            }
        }
    }
}

/// Compare two dotted release segments, zero-padding the shorter. Returns -1/0/1.
fn cmp(a: &[u64], b: &[u64]) -> i32 {
    let n = a.len().max(b.len());
    for i in 0..n {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        if x < y {
            return -1;
        }
        if x > y {
            return 1;
        }
    }
    0
}

/// Does `candidate` share the given prefix (for `==3.10.*`)?
fn prefix_matches(candidate: &[u64], prefix: &[u64]) -> bool {
    prefix
        .iter()
        .enumerate()
        .all(|(i, &p)| candidate.get(i).copied().unwrap_or(0) == p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(minors: &[u8]) -> PyVersionSet {
        PyVersionSet {
            minors: minors.iter().copied().collect(),
        }
    }

    #[test]
    fn parse_cp_tag() {
        assert_eq!(PyVersion::parse("cp312"), Some(PyVersion::py3(12)));
        assert_eq!(PyVersion::parse("cp39"), Some(PyVersion::py3(9)));
        assert_eq!(PyVersion::parse("3.11"), Some(PyVersion::py3(11)));
        assert_eq!(PyVersion::parse("3.10.4"), Some(PyVersion::py3(10)));
    }

    #[test]
    fn requires_python_lower_bound() {
        let s = parse_requires_python(">=3.8");
        assert!(s.contains(PyVersion::py3(8)));
        assert!(s.contains(PyVersion::py3(13)));
        assert!(!s.contains(PyVersion::py3(7)));
    }

    #[test]
    fn requires_python_bounded_range() {
        let s = parse_requires_python(">=3.7,<3.13");
        assert!(s.contains(PyVersion::py3(12)));
        assert!(!s.contains(PyVersion::py3(13)));
        assert!(s.contains(PyVersion::py3(7)));
    }

    #[test]
    fn requires_python_compatible_release() {
        // ~=3.9 means >=3.9,<4 → all of 3.9..
        let s = parse_requires_python("~=3.9");
        assert!(s.contains(PyVersion::py3(9)));
        assert!(s.contains(PyVersion::py3(13)));
        assert!(!s.contains(PyVersion::py3(8)));
    }

    #[test]
    fn requires_python_not_equal_wildcard() {
        let s = parse_requires_python(">=3.8,!=3.9.*");
        assert!(s.contains(PyVersion::py3(8)));
        assert!(!s.contains(PyVersion::py3(9)));
        assert!(s.contains(PyVersion::py3(10)));
    }

    /// A patch-level floor keeps its minor: 3.8.20 satisfies `>=3.8.1`.
    #[test]
    fn patch_level_bounds_keep_their_minor() {
        // uvloop 0.23.0
        let s = parse_requires_python(">=3.8.1");
        assert!(s.contains(PyVersion::py3(8)));
        assert!(!s.contains(PyVersion::py3(7)));
        // cryptography 50.0.2
        let s = parse_requires_python("!=3.9.0,!=3.9.1,>=3.9");
        assert!(s.contains(PyVersion::py3(9)));
        assert!(!s.contains(PyVersion::py3(8)));
        // A window inside one minor.
        let s = parse_requires_python(">=3.10.2,<3.10.5");
        assert_eq!(s, set(&[10]));
        // Excluding one patch release does not exclude the minor.
        assert!(parse_requires_python("!=3.12.0").contains(PyVersion::py3(12)));
        // Whole-minor caps are unchanged.
        assert!(!parse_requires_python("<3.13").contains(PyVersion::py3(13)));
        assert!(parse_requires_python("<=3.10").contains(PyVersion::py3(10)));
        assert!(!parse_requires_python("==3.11.*").contains(PyVersion::py3(12)));
    }

    #[test]
    fn empty_spec_is_universe() {
        assert_eq!(parse_requires_python(""), PyVersionSet::universe());
    }

    #[test]
    fn intersection_and_range_string() {
        let a = set(&[9, 10, 11, 12]);
        let b = parse_requires_python(">=3.11");
        let i = a.intersect(&b);
        assert_eq!(i, set(&[11, 12]));
        assert_eq!(i.to_range_string(), "3.11–3.12");
        assert_eq!(a.to_range_string(), "3.9–3.12");
    }

    #[test]
    fn range_string_with_gap() {
        assert_eq!(set(&[9, 11, 12]).to_range_string(), "3.9, 3.11–3.12");
    }

    #[test]
    fn empty_intersection() {
        let a = parse_requires_python("<=3.10"); // ..3.10
        let b = parse_requires_python(">=3.11"); // 3.11..
        assert!(a.intersect(&b).is_empty());
    }
}
