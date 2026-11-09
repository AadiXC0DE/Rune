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

fn number(part: &str) -> Option<u64> {
    if part.is_empty() || (part.len() > 1 && part.starts_with('0')) {
        return None;
    }
    part.parse().ok()
}

impl Version {
    /// Parses a version, ignoring any build metadata after `+`.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.split('+').next()?;
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => {
                let pre: Vec<String> = pre.split('.').map(str::to_owned).collect();
                if pre.iter().any(String::is_empty) {
                    return None;
                }
                (core, pre)
            }
            None => (text, Vec::new()),
        };
        let mut parts = core.split('.').map(number);
        let version = Version {
            major: parts.next()??,
            minor: parts.next()??,
            patch: parts.next()??,
            pre,
        };
        parts.next().is_none().then_some(version)
    }
}

fn compare_identifier(left: &str, right: &str) -> Ordering {
    match (left.parse::<u64>(), right.parse::<u64>()) {
        (Ok(left), Ok(right)) => left.cmp(&right),
        (Ok(_), Err(_)) => Ordering::Less,
        (Err(_), Ok(_)) => Ordering::Greater,
        (Err(_), Err(_)) => left.cmp(right),
    }
}

fn compare_pre(left: &[String], right: &[String]) -> Ordering {
    match (left.is_empty(), right.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        (false, false) => {}
    }
    for (left, right) in left.iter().zip(right) {
        let ordering = compare_identifier(left, right);
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left.len().cmp(&right.len())
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| compare_pre(&self.pre, &other.pre))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
