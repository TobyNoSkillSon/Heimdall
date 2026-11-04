//! Release versions: `MAJOR.MINOR.PATCH`, optionally `-PRERELEASE` and `+BUILD`, with or without a leading `v`.
//! Ordered by Semantic Versioning 2.0: numbers first, then a prerelease sorts before its release
//! (`0.1.0-alpha.2 < 0.1.0`), numeric prerelease identifiers compare as numbers and sort before alphanumeric ones,
//! a shorter identifier list sorts first, build metadata is ignored. Ported from Vella's SemanticVersion.

use std::cmp::Ordering;

#[derive(Debug, Clone)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub prerelease: Vec<String>,
    pub build: Option<String>,
}

impl Version {
    pub fn parse(text: &str) -> Option<Version> {
        let mut s = text.trim();
        if let Some(rest) = s.strip_prefix('v').or_else(|| s.strip_prefix('V')) {
            s = rest;
        }
        let mut build = None;
        if let Some((head, tail)) = s.split_once('+') {
            if tail.is_empty() || !tail.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') {
                return None;
            }
            build = Some(tail.to_owned());
            s = head;
        }
        let mut prerelease = vec![];
        if let Some((head, tail)) = s.split_once('-') {
            prerelease = tail.split('.').map(str::to_owned).collect();
            if prerelease.iter().any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')) {
                return None;
            }
            s = head;
        }
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() != 3 {
            return None;
        }
        let mut numbers = [0u64; 3];
        for (i, part) in parts.iter().enumerate() {
            // No leading zeros (SemVer 2): `v01.0.0` is not a release tag.
            if part.is_empty()
                || part.len() > 9
                || !part.chars().all(|c| c.is_ascii_digit())
                || (part.len() > 1 && part.starts_with('0'))
            {
                return None;
            }
            numbers[i] = part.parse().ok()?;
        }
        Some(Version { major: numbers[0], minor: numbers[1], patch: numbers[2], prerelease, build })
    }

    pub fn is_prerelease(&self) -> bool {
        !self.prerelease.is_empty()
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.prerelease.is_empty() {
            write!(f, "-{}", self.prerelease.join("."))?;
        }
        if let Some(build) = &self.build {
            write!(f, "+{build}")?;
        }
        Ok(())
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Version {}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        let core = (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch));
        if core != Ordering::Equal {
            return core;
        }
        match (self.prerelease.is_empty(), other.prerelease.is_empty()) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Greater,
            (false, true) => return Ordering::Less,
            (false, false) => {}
        }
        for (a, b) in self.prerelease.iter().zip(&other.prerelease) {
            if a == b {
                continue;
            }
            let numeric = |s: &str| s.chars().all(|c| c.is_ascii_digit()).then(|| s.parse::<u64>().ok()).flatten();
            return match (numeric(a), numeric(b)) {
                (Some(x), Some(y)) => x.cmp(&y),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => a.cmp(b),
            };
        }
        self.prerelease.len().cmp(&other.prerelease.len())
    }
}
