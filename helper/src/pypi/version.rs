//! Package versions and the specifiers that select them.
//!
//! A dependency line pins a *release*, and the release decides which Python
//! versions are usable. `mediapipe==0.10.14` ships wheels for CPython 3.9 to
//! 3.12; `mediapipe` unpinned resolves to 0.10.35, whose wheels are tagged
//! `py3` and install anywhere. Reading only the newest release therefore gives
//! the wrong answer for any project that pins or caps a dependency.
//!
//! Ordering and specifier matching follow PEP 440
//! (<https://peps.python.org/pep-0440/>): epochs, pre-, post- and dev-releases
//! all count, so `==2.15.0` selects `2.15.0` and not `2.15.0.post1`, whose
//! wheels can be entirely different. Local versions (`+cpu`) are ignored when
//! ordering but honoured by an `==` that names one. Pre-releases are excluded
//! from selection unless the specifier names one, because installers skip
//! them unless asked.

use std::cmp::Ordering;

/// A pre-release phase. The declaration order is the PEP 440 order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Phase {
    Alpha,
    Beta,
    Rc,
}

/// A PyPI release version, ordered as PEP 440 orders it.
#[derive(Debug, Clone)]
pub struct PackageVersion {
    /// The version exactly as PyPI spells it, for display and for URLs.
    pub raw: String,
    epoch: u64,
    release: Vec<u64>,
    pre: Option<(Phase, u64)>,
    post: Option<u64>,
    dev: Option<u64>,
    /// Normalised local segment (`cpu`, `cu121`), without the `+`.
    local: Option<String>,
}

impl PackageVersion {
    /// Parse a version string, accepting the spellings PEP 440 normalises
    /// (`1.0-post1`, `1.0rev1`, `1.0-1`, `1.0alpha2`, `1.0c1`, `v1.0`).
    /// Returns `None` for anything that is not a valid version, which modern
    /// installers skip as well.
    pub fn parse(raw: &str) -> Option<PackageVersion> {
        let text = raw.trim();
        let lower = text.to_ascii_lowercase();
        let mut rest = lower.strip_prefix('v').unwrap_or(&lower);

        let local = match rest.split_once('+') {
            Some((public, local)) => {
                rest = public;
                let normalised = local.replace(['-', '_'], ".");
                let valid = normalised
                    .split('.')
                    .all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric()));
                if !valid {
                    return None;
                }
                Some(normalised)
            }
            None => None,
        };

        let epoch = match rest.split_once('!') {
            Some((e, r)) => {
                rest = r;
                e.parse().ok()?
            }
            None => 0,
        };

        let mut cur = Cursor(rest);
        let mut release = vec![cur.number()?];
        while let Some(after) = cur.0.strip_prefix('.') {
            if !after.starts_with(|c: char| c.is_ascii_digit()) {
                break;
            }
            cur.0 = after;
            release.push(cur.number()?);
        }

        let pre = cur.pre();
        let post = cur.post();
        let dev = cur.dev();
        if !cur.0.is_empty() {
            return None;
        }

        Some(PackageVersion {
            raw: text.to_string(),
            epoch,
            release,
            pre,
            post,
            dev,
            local,
        })
    }

    /// Alphas, betas, release candidates and dev builds. Installers ignore these
    /// unless a specifier names one, so selection does too.
    pub fn is_prerelease(&self) -> bool {
        self.pre.is_some() || self.dev.is_some()
    }

    fn is_postrelease(&self) -> bool {
        self.post.is_some()
    }

    /// The release segments, `[2, 2, 6]` for `2.2.6`, for grouping releases
    /// into series.
    pub fn release(&self) -> &[u64] {
        &self.release
    }

    /// Same epoch and release segments (zero-padded), ignoring every suffix.
    fn same_base(&self, other: &PackageVersion) -> bool {
        self.epoch == other.epoch && cmp_release(&self.release, &other.release).is_eq()
    }

    /// The pre-release sort key. A dev build of a final release (`1.0.dev1`)
    /// sorts before that release's alphas; a version with no pre-release sorts
    /// after all of them.
    fn pre_key(&self) -> (u8, Option<(Phase, u64)>) {
        match self.pre {
            None if self.post.is_none() && self.dev.is_some() => (0, None),
            Some(p) => (1, Some(p)),
            None => (2, None),
        }
    }
}

/// Zero-padded comparison of release segments: `1.0 == 1.0.0`.
fn cmp_release(a: &[u64], b: &[u64]) -> Ordering {
    let n = a.len().max(b.len());
    (0..n)
        .map(|i| {
            let x = a.get(i).copied().unwrap_or(0);
            let y = b.get(i).copied().unwrap_or(0);
            x.cmp(&y)
        })
        .find(|o| o.is_ne())
        .unwrap_or(Ordering::Equal)
}

/// A small parser over the text after the release segments.
struct Cursor<'a>(&'a str);

impl Cursor<'_> {
    fn number(&mut self) -> Option<u64> {
        let end = self
            .0
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(self.0.len());
        if end == 0 {
            return None;
        }
        let n = self.0[..end].parse().ok()?;
        self.0 = &self.0[end..];
        Some(n)
    }

    /// Consume an optional `.`, `-` or `_` followed by one of `labels`.
    fn label(&mut self, labels: &[&'static str]) -> Option<&'static str> {
        let body = self.0.strip_prefix(['.', '-', '_']).unwrap_or(self.0);
        // Longest first, so `preview` is not read as `pre` plus garbage.
        let found = labels
            .iter()
            .copied()
            .filter(|l| body.starts_with(l))
            .max_by_key(|l| l.len())?;
        self.0 = &body[found.len()..];
        Some(found)
    }

    /// The number after a label, with an optional separator; missing means 0.
    fn label_number(&mut self) -> u64 {
        let save = self.0;
        self.0 = self.0.strip_prefix(['.', '-', '_']).unwrap_or(self.0);
        match self.number() {
            Some(n) => n,
            None => {
                self.0 = save;
                0
            }
        }
    }

    fn pre(&mut self) -> Option<(Phase, u64)> {
        let phase = match self.label(&["a", "alpha", "b", "beta", "c", "rc", "pre", "preview"])? {
            "a" | "alpha" => Phase::Alpha,
            "b" | "beta" => Phase::Beta,
            _ => Phase::Rc,
        };
        Some((phase, self.label_number()))
    }

    fn post(&mut self) -> Option<u64> {
        // Implicit post-release: `1.0-1`.
        if let Some(body) = self.0.strip_prefix('-') {
            if body.starts_with(|c: char| c.is_ascii_digit()) {
                self.0 = body;
                return self.number();
            }
        }
        self.label(&["post", "rev", "r"])?;
        Some(self.label_number())
    }

    fn dev(&mut self) -> Option<u64> {
        self.label(&["dev"])?;
        Some(self.label_number())
    }
}

impl Ord for PackageVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        self.epoch
            .cmp(&other.epoch)
            .then_with(|| cmp_release(&self.release, &other.release))
            .then_with(|| self.pre_key().cmp(&other.pre_key()))
            // No post-release sorts before any post-release (`None < Some`).
            .then_with(|| self.post.cmp(&other.post))
            // No dev-release sorts after any dev-release.
            .then_with(|| match (self.dev, other.dev) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => a.cmp(&b),
            })
    }
}

impl PartialOrd for PackageVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// Equality has to agree with the ordering, or `Ord`'s contract is broken.
// Deriving it would compare the raw strings and call 1.0 different from 1.0.0,
// while `cmp` zero-pads and calls them equal.
impl PartialEq for PackageVersion {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for PackageVersion {}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    Eq,
    Ne,
    Ge,
    Le,
    Gt,
    Lt,
    Compatible,
}

/// One clause of a specifier, such as `>=1.2` or `==2.*`.
#[derive(Debug, Clone)]
enum Clause {
    Compare {
        op: Op,
        version: PackageVersion,
        /// `==1.2.*` / `!=1.2.*` prefix matching.
        wildcard: bool,
    },
    /// `===text`: plain, case-insensitive string equality.
    Arbitrary(String),
}

impl Clause {
    fn parse(s: &str) -> Option<Clause> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        if let Some(r) = s.strip_prefix("===") {
            return Some(Clause::Arbitrary(r.trim().to_string()));
        }
        let (op, rest) = [
            ("==", Op::Eq),
            ("!=", Op::Ne),
            ("~=", Op::Compatible),
            (">=", Op::Ge),
            ("<=", Op::Le),
            (">", Op::Gt),
            ("<", Op::Lt),
        ]
        .iter()
        .find_map(|(p, op)| s.strip_prefix(p).map(|r| (*op, r)))
        // A bare version behaves like `==`.
        .unwrap_or((Op::Eq, s));

        let rest = rest.trim();
        let (text, wildcard) = match rest.strip_suffix(".*") {
            Some(t) if matches!(op, Op::Eq | Op::Ne) => (t, true),
            Some(_) => return None,
            None => (rest, false),
        };
        let version = PackageVersion::parse(text)?;
        // `~=1` is invalid; read it as the `>=1` it plainly intends.
        let op = if op == Op::Compatible && version.release.len() < 2 {
            Op::Ge
        } else {
            op
        };
        Some(Clause::Compare {
            op,
            version,
            wildcard,
        })
    }

    /// Does the clause name a pre-release (which then allows selecting one)?
    fn names_prerelease(&self) -> bool {
        match self {
            Clause::Compare { op, version, .. } => *op != Op::Ne && version.is_prerelease(),
            Clause::Arbitrary(text) => {
                PackageVersion::parse(text).is_some_and(|v| v.is_prerelease())
            }
        }
    }

    fn matches(&self, cand: &PackageVersion) -> bool {
        let (op, spec, wildcard) = match self {
            Clause::Arbitrary(text) => return cand.raw.eq_ignore_ascii_case(text),
            Clause::Compare {
                op,
                version,
                wildcard,
            } => (*op, version, *wildcard),
        };
        match op {
            Op::Eq if wildcard => prefix_match(cand, spec),
            Op::Ne if wildcard => !prefix_match(cand, spec),
            Op::Eq => exact_match(cand, spec),
            Op::Ne => !exact_match(cand, spec),
            Op::Ge => cand >= spec,
            Op::Le => cand <= spec,
            // `>1.7` does not admit `1.7.post1` unless it names a post-release.
            Op::Gt => {
                cand > spec
                    && !(cand.is_postrelease() && !spec.is_postrelease() && cand.same_base(spec))
            }
            // `<1.7` does not admit `1.7rc1` unless it names a pre-release.
            Op::Lt => {
                cand < spec
                    && !(cand.is_prerelease() && !spec.is_prerelease() && cand.same_base(spec))
            }
            // `~=1.4.5` is `>=1.4.5, ==1.4.*`.
            Op::Compatible => {
                let prefix = &spec.release[..spec.release.len() - 1];
                cand >= spec && cand.epoch == spec.epoch && release_prefix(&cand.release, prefix)
            }
        }
    }
}

/// `==X`: equal under PEP 440 ordering, and the same local segment when the
/// specifier names one. Without one, any local build of X matches.
fn exact_match(cand: &PackageVersion, spec: &PackageVersion) -> bool {
    cand == spec && (spec.local.is_none() || cand.local == spec.local)
}

/// `==X.*`: the candidate's release starts with X (zero-padded), whatever its
/// pre, post, dev or local suffix.
fn prefix_match(cand: &PackageVersion, spec: &PackageVersion) -> bool {
    cand.epoch == spec.epoch && release_prefix(&cand.release, &spec.release)
}

fn release_prefix(release: &[u64], prefix: &[u64]) -> bool {
    prefix
        .iter()
        .enumerate()
        .all(|(i, p)| release.get(i).copied().unwrap_or(0) == *p)
}

/// The version constraint from a dependency line, such as `>=1.2,<2`.
#[derive(Default)]
pub struct VersionSpec {
    clauses: Vec<Clause>,
    /// True when the specifier names a pre-release, which allows selecting one.
    allows_prerelease: bool,
}

impl VersionSpec {
    /// Parse the specifier portion of a requirement. An empty string means the
    /// dependency is unconstrained.
    pub fn parse(spec: &str) -> VersionSpec {
        let clauses: Vec<Clause> = spec.split(',').filter_map(Clause::parse).collect();
        let allows_prerelease = clauses.iter().any(Clause::names_prerelease);
        VersionSpec {
            clauses,
            allows_prerelease,
        }
    }

    /// No constraint at all, so the newest release is the right one.
    pub fn is_unconstrained(&self) -> bool {
        self.clauses.is_empty()
    }

    pub fn matches(&self, version: &PackageVersion) -> bool {
        if version.is_prerelease() && !self.allows_prerelease {
            return false;
        }
        self.admits(version)
    }

    /// Does a version that is already installed satisfy this specifier?
    ///
    /// Unlike [`VersionSpec::matches`], pre-releases are not filtered out: that
    /// rule is about what an installer would *pick*, and once a release is on
    /// disk only the comparisons matter.
    pub fn admits(&self, version: &PackageVersion) -> bool {
        self.clauses.iter().all(|c| c.matches(version))
    }

    /// The newest release satisfying this specifier, which is what an installer
    /// would pick.
    pub fn select_newest<'a, I>(&self, versions: I) -> Option<PackageVersion>
    where
        I: IntoIterator<Item = &'a str>,
    {
        versions
            .into_iter()
            .filter_map(PackageVersion::parse)
            .filter(|v| self.matches(v))
            .max()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> PackageVersion {
        PackageVersion::parse(s).unwrap()
    }

    fn pick(spec: &str, versions: &[&str]) -> Option<String> {
        VersionSpec::parse(spec)
            .select_newest(versions.iter().copied())
            .map(|v| v.raw)
    }

    fn admits(spec: &str, version: &str) -> bool {
        VersionSpec::parse(spec).admits(&v(version))
    }

    #[test]
    fn orders_by_release_segments() {
        assert!(v("0.10.35") > v("0.10.14"));
        // Numeric, not lexicographic: "10" sorts above "2" here.
        assert!(v("0.10.10") > v("0.10.2"));
        assert!(v("1.0") < v("1.0.1"));
        assert!(v("2.0") > v("1.99.99"));
    }

    #[test]
    fn zero_pads_shorter_versions() {
        assert_eq!(v("1.0.0"), v("1.0"));
        assert_eq!(v("1.0.0").cmp(&v("1.0")), Ordering::Equal);
    }

    #[test]
    fn detects_prereleases() {
        assert!(v("1.0.0rc1").is_prerelease());
        assert!(v("2.0b3").is_prerelease());
        assert!(v("1.0.dev1").is_prerelease());
        assert!(v("1.0.post1.dev2").is_prerelease());
        assert!(!v("1.0.0").is_prerelease());
        assert!(!v("1.0.post1").is_prerelease());
    }

    #[test]
    fn releases_outrank_their_prereleases() {
        assert!(v("1.0.0") > v("1.0.0rc1"));
    }

    #[test]
    fn full_pep440_order() {
        let ordered = [
            "1.0.dev1",
            "1.0a1.dev1",
            "1.0a1",
            "1.0a1.post1",
            "1.0a2",
            "1.0b1",
            "1.0rc1",
            "1.0",
            "1.0.post1.dev1",
            "1.0.post1",
            "1.0.post2",
            "1.0.1",
            "1!0.1",
        ];
        for pair in ordered.windows(2) {
            assert!(v(pair[0]) < v(pair[1]), "{} < {}", pair[0], pair[1]);
        }
    }

    #[test]
    fn post_releases_are_distinct_from_the_release() {
        assert_ne!(v("2.15.0.post1"), v("2.15.0"));
        assert!(v("2.15.0.post1") > v("2.15.0"));
        assert!(v("2.15.0.post1") < v("2.15.1"));
    }

    #[test]
    fn normalises_alternative_spellings() {
        assert_eq!(v("1.0-post1"), v("1.0.post1"));
        assert_eq!(v("1.0post1"), v("1.0.post1"));
        assert_eq!(v("1.0.rev1"), v("1.0.post1"));
        assert_eq!(v("1.0r1"), v("1.0.post1"));
        assert_eq!(v("1.0-1"), v("1.0.post1"));
        assert_eq!(v("1.0.post"), v("1.0.post0"));
        assert_eq!(v("1.0alpha2"), v("1.0a2"));
        assert_eq!(v("1.0-beta.3"), v("1.0b3"));
        assert_eq!(v("1.0c1"), v("1.0rc1"));
        assert_eq!(v("1.0pre1"), v("1.0rc1"));
        assert_eq!(v("1.0preview1"), v("1.0rc1"));
        assert_eq!(v("1.0RC1"), v("1.0rc1"));
        assert_eq!(v("1.0-dev"), v("1.0.dev0"));
        assert_eq!(v("v1.0"), v("1.0"));
        assert!(PackageVersion::parse("not-a-version").is_none());
        assert!(PackageVersion::parse("1.0garbage").is_none());
    }

    #[test]
    fn handles_epochs_and_local_versions() {
        assert_eq!(v("1!2.0").release, vec![2, 0]);
        assert!(v("1!1.0") > v("2.0"));
        assert_eq!(v("2.4.0+cpu").release, vec![2, 4, 0]);
        // Local segments do not affect ordering.
        assert_eq!(v("2.4.0+cpu"), v("2.4.0"));
        assert_eq!(v("2.4.0+cu121").local.as_deref(), Some("cu121"));
    }

    #[test]
    fn exact_pin_selects_that_release() {
        assert_eq!(
            pick("==0.10.14", &["0.10.9", "0.10.14", "0.10.35"]).as_deref(),
            Some("0.10.14")
        );
    }

    #[test]
    fn tensorflow_post_release_is_not_an_exact_pin_match() {
        // tensorflow 2.15.0.post1 ships only manylinux wheels; `==2.15.0` must
        // resolve to 2.15.0 itself.
        let tf = ["2.14.1", "2.15.0", "2.15.0.post1", "2.15.1", "2.16.1"];
        assert_eq!(pick("==2.15.0", &tf).as_deref(), Some("2.15.0"));
        assert_eq!(pick("==2.15.0.post1", &tf).as_deref(), Some("2.15.0.post1"));
        assert_eq!(
            pick(">=2.15", &["2.15.0", "2.15.0.post1"]).as_deref(),
            Some("2.15.0.post1")
        );
        assert_eq!(pick("~=2.15.0", &tf).as_deref(), Some("2.15.1"));
        assert_eq!(
            pick("~=2.15.0", &["2.15.0", "2.15.0.post1"]).as_deref(),
            Some("2.15.0.post1")
        );
        assert_eq!(pick("<2.16", &tf).as_deref(), Some("2.15.1"));
        assert_eq!(
            pick("<2.16", &["2.15.0", "2.15.0.post1", "2.16.0rc0"]).as_deref(),
            Some("2.15.0.post1")
        );
        assert_eq!(
            pick(">2.15.0", &["2.15.0", "2.15.0.post1"]).as_deref(),
            None
        );
        assert_eq!(pick(">2.15.0", &tf).as_deref(), Some("2.16.1"));
        assert_eq!(pick("==2.15.*", &tf).as_deref(), Some("2.15.1"));
        assert_eq!(pick("!=2.15.1,<2.16", &tf).as_deref(), Some("2.15.0.post1"));
    }

    #[test]
    fn exact_match_semantics() {
        assert!(admits("==1.0", "1.0.0"));
        assert!(admits("==1.0", "1.0+cpu"));
        assert!(!admits("==1.0", "1.0.post1"));
        assert!(!admits("==1.0", "1.0rc1"));
        assert!(!admits("==1.0", "1.0.dev1"));
        // A local in the specifier must match exactly.
        assert!(admits("==1.0+cpu", "1.0+cpu"));
        assert!(!admits("==1.0+cpu", "1.0+cu121"));
        assert!(!admits("==1.0+cpu", "1.0"));
        assert!(admits("!=1.0", "1.0.post1"));
        assert!(!admits("!=1.0", "1.0+cpu"));
    }

    #[test]
    fn wildcard_matches_the_prefix() {
        assert!(admits("==1.1.*", "1.1"));
        assert!(admits("==1.1.*", "1.1.5"));
        assert!(admits("==1.1.*", "1.1.post1"));
        assert!(admits("==1.1.*", "1.1.dev1"));
        assert!(admits("==1.1.*", "1.1a1"));
        assert!(!admits("==1.1.*", "1.10"));
        assert!(!admits("==1.1.*", "1.2"));
        assert!(!admits("!=1.1.*", "1.1.3"));
        assert!(admits("!=1.1.*", "1.2"));
    }

    #[test]
    fn compatible_release() {
        assert!(admits("~=2.2", "2.2"));
        assert!(admits("~=2.2", "2.9"));
        assert!(!admits("~=2.2", "3.0"));
        assert!(admits("~=1.4.5", "1.4.9"));
        assert!(!admits("~=1.4.5", "1.5.0"));
        assert!(!admits("~=1.4.5", "1.4.4"));
        assert!(admits("~=2.2.post3", "2.3"));
        assert!(!admits("~=2.2.post3", "2.2"));
    }

    #[test]
    fn exclusive_comparisons() {
        assert!(!admits(">1.7", "1.7.post1"));
        assert!(admits(">1.7", "1.7.1"));
        assert!(!admits(">1.7", "1.7+local"));
        assert!(admits(">1.7.post2", "1.7.post3"));
        assert!(!admits("<1.7", "1.7rc1"));
        assert!(!admits("<1.7", "1.7.dev1"));
        assert!(admits("<1.7", "1.6.9"));
        assert!(admits("<1.7rc2", "1.7rc1"));
        assert!(admits("<=1.7", "1.7+local"));
        assert!(admits(">=1.7", "1.7.post1"));
    }

    #[test]
    fn arbitrary_equality() {
        assert!(admits("===1.0", "1.0"));
        assert!(!admits("===1.0", "1.0.0"));
        assert_eq!(
            pick("===foobar", &["1.0"]).as_deref(),
            None,
            "an unparseable version never matches a real one"
        );
    }

    #[test]
    fn spaces_around_operators_are_fine() {
        assert_eq!(
            pick(" >= 1.0 , < 2 ", &["0.9", "1.5", "2.0"]).as_deref(),
            Some("1.5")
        );
    }

    #[test]
    fn upper_bound_selects_newest_below_it() {
        assert_eq!(
            pick(
                ">=0.10,<0.11",
                &["0.9.1", "0.10.9", "0.10.14", "0.11.0", "1.0.0"]
            )
            .as_deref(),
            Some("0.10.14")
        );
    }

    #[test]
    fn unconstrained_takes_the_newest() {
        let spec = VersionSpec::parse("");
        assert!(spec.is_unconstrained());
        let picked = spec.select_newest(["0.10.14", "0.10.35", "0.9.0"]).unwrap();
        assert_eq!(picked.raw, "0.10.35");
    }

    #[test]
    fn lower_bound_still_takes_the_newest() {
        assert_eq!(
            pick(">=0.10", &["0.9.0", "0.10.14", "0.10.35"]).as_deref(),
            Some("0.10.35")
        );
    }

    #[test]
    fn compatible_release_caps_the_minor() {
        // ~=0.10.0 means >=0.10.0,<0.11
        assert_eq!(
            pick("~=0.10.0", &["0.10.14", "0.10.35", "0.11.0"]).as_deref(),
            Some("0.10.35")
        );
    }

    #[test]
    fn prereleases_are_skipped_unless_requested() {
        assert_eq!(
            pick(">=1.0", &["1.0.0", "1.1.0rc1"]).as_deref(),
            Some("1.0.0"),
            "an rc should not win by default"
        );
        assert_eq!(
            pick("==1.1.0rc1", &["1.0.0", "1.1.0rc1"]).as_deref(),
            Some("1.1.0rc1")
        );
        assert_eq!(
            pick(">=1.1.0rc1", &["1.0.0", "1.1.0rc1"]).as_deref(),
            Some("1.1.0rc1")
        );
        // Excluding a pre-release does not opt into pre-releases.
        assert_eq!(
            pick("!=1.1.0rc1", &["1.0.0", "1.1.0rc2"]).as_deref(),
            Some("1.0.0")
        );
    }

    #[test]
    fn admits_checks_an_installed_version_including_prereleases() {
        let below_two = VersionSpec::parse("<2,>=1.21");
        assert!(below_two.admits(&v("1.26.4")));
        assert!(!below_two.admits(&v("2.1.0")));
        assert!(!below_two.admits(&v("2.0.0rc1")));
        // `matches` would refuse the rc as a pick; `admits` judges it on its release.
        assert!(VersionSpec::parse(">=1.0").admits(&v("1.1.0rc1")));
    }

    #[test]
    fn impossible_constraint_selects_nothing() {
        let spec = VersionSpec::parse(">=99.0");
        assert!(spec.select_newest(["1.0.0", "2.0.0"]).is_none());
    }
}
