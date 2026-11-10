//! Switches for behaviour under measurement, read from `RUNE_EXPERIMENT`.

use crate::error::{Result, RuneError};

/// The keys `RUNE_EXPERIMENT` accepts.
const KEYS: &[&str] = &["map", "run_note"];

/// Switches for behaviour under measurement. The default is the shipped behaviour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Experiments {
    /// `run_note=on|off`: the non-interactive note in the first user message of `ask`.
    pub run_note: bool,
    /// `map=on|off`: forces the repository map on or off; `None` leaves the setting in force.
    pub map: Option<bool>,
}

impl Default for Experiments {
    fn default() -> Self {
        Self {
            run_note: true,
            map: None,
        }
    }
}

impl Experiments {
    /// The environment variable the switches are read from.
    pub const VARIABLE: &'static str = "RUNE_EXPERIMENT";

    /// Parses a comma-separated `key=value` list.
    ///
    /// # Errors
    ///
    /// Returns `invalid_field` on `RUNE_EXPERIMENT` for an entry that is not
    /// `key=value`, an unknown key, a value the key does not take, or a key
    /// set twice.
    pub fn parse(raw: &str) -> Result<Self> {
        let mut experiments = Self::default();
        let mut seen: Vec<&str> = Vec::new();
        for entry in raw
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
        {
            let Some((key, value)) = entry.split_once('=') else {
                return Err(invalid(format!("`{entry}` is not in key=value form")));
            };
            let (key, value) = (key.trim(), value.trim());
            if !KEYS.contains(&key) {
                return Err(invalid(format!("unknown experiment key `{key}`"))
                    .with_hint(format!("known keys are {}", KEYS.join(", "))));
            }
            if seen.contains(&key) {
                return Err(invalid(format!("`{key}` is set twice")));
            }
            seen.push(key);
            let on = match value {
                "on" => true,
                "off" => false,
                _ => {
                    return Err(invalid(format!("`{value}` is not a value of `{key}`"))
                        .with_hint("use on or off"));
                }
            };
            match key {
                "run_note" => experiments.run_note = on,
                _ => experiments.map = Some(on),
            }
        }
        Ok(experiments)
    }

    /// Reads the switches through `lookup`; an unset variable is the default.
    ///
    /// # Errors
    ///
    /// Returns the parse error of [`Experiments::parse`].
    pub fn from_lookup(lookup: impl FnOnce(&str) -> Option<String>) -> Result<Self> {
        lookup(Self::VARIABLE).map_or_else(|| Ok(Self::default()), |raw| Self::parse(&raw))
    }

    /// Canonical `key=value` list of the keys that differ from the default,
    /// sorted; empty when none.
    #[must_use]
    pub fn render(&self) -> String {
        let default = Self::default();
        let mut parts = Vec::new();
        if let Some(map) = self.map
            && default.map != Some(map)
        {
            parts.push(format!("map={}", spelling(map)));
        }
        if self.run_note != default.run_note {
            parts.push(format!("run_note={}", spelling(self.run_note)));
        }
        parts.sort();
        parts.join(",")
    }
}

const fn spelling(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

fn invalid(message: impl Into<String>) -> RuneError {
    RuneError::invalid_field(Experiments::VARIABLE, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(raw: &str) -> String {
        Experiments::parse(raw).unwrap_err().message().to_owned()
    }

    #[test]
    fn an_empty_value_is_the_default() {
        assert_eq!(Experiments::parse("").unwrap(), Experiments::default());
        assert_eq!(Experiments::parse(" , ").unwrap(), Experiments::default());
        assert_eq!(
            Experiments::from_lookup(|_| None).unwrap(),
            Experiments::default()
        );
    }

    #[test]
    fn keys_parse_on_and_off() {
        let parsed = Experiments::parse("run_note=off, map=on").unwrap();
        assert!(!parsed.run_note);
        assert_eq!(parsed.map, Some(true));
        assert_eq!(Experiments::parse("map=off").unwrap().map, Some(false));
    }

    #[test]
    fn an_unknown_key_names_the_known_keys() {
        let error = Experiments::parse("snapshot=on").unwrap_err();
        assert_eq!(error.message(), "unknown experiment key `snapshot`");
        assert_eq!(error.hint(), Some("known keys are map, run_note"));
        assert_eq!(error.field(), Some("RUNE_EXPERIMENT"));
        assert_eq!(message("map"), "`map` is not in key=value form");
    }

    #[test]
    fn a_repeated_key_is_refused() {
        assert_eq!(message("map=off,map=on"), "`map` is set twice");
    }

    #[test]
    fn true_is_not_a_boolean_spelling() {
        let error = Experiments::parse("run_note=true").unwrap_err();
        assert_eq!(error.message(), "`true` is not a value of `run_note`");
        assert_eq!(error.hint(), Some("use on or off"));
        assert_eq!(message("map=On"), "`On` is not a value of `map`");
    }

    #[test]
    fn render_lists_only_changed_keys_sorted() {
        assert_eq!(Experiments::default().render(), "");
        assert_eq!(
            Experiments::parse("run_note=off,map=off").unwrap().render(),
            "map=off,run_note=off"
        );
        assert_eq!(Experiments::parse("run_note=on").unwrap().render(), "");
    }
}
