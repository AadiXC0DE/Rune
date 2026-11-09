//! Parsing and ordering of semantic versions.

use std::cmp::Ordering;

/// A version such as `1.2.3-beta.2+build.5`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Vec<String>,
}

impl Version {
    /// Parses a version, ignoring any build metadata after `+`.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.split('+').next()?;
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, pre.split('.').map(str::to_owned).collect()),
            None => (text, Vec::new()),
        };
        let mut parts = core.split('.').map(|part| part.parse::<u64>().ok());
        let version = Version {
            major: parts.next()??,
            minor: parts.next()??,
            patch: parts.next()??,
            pre,
        };
        parts.next().is_none().then_some(version)
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| self.pre.cmp(&other.pre))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
