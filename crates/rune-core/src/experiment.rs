//! Switches for behaviour under measurement, read from `RUNE_EXPERIMENT`.

use crate::error::{Result, RuneError};

/// The keys `RUNE_EXPERIMENT` accepts.
const KEYS: &[&str] = &[
    "goal",
    "map",
    "overlay",
    "prompt",
    "question_nudge",
    "run_note",
    "snapshot",
];

/// Which system prompt text is sent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PromptVariant {
    /// The shipped prompt, with no tool guidance.
    #[default]
    V1,
    /// The shipped prompt with a final-answer contract and tool guidance.
    V2Min,
    /// The same rules under headings, with tool guidance.
    V2Structured,
}

impl PromptVariant {
    /// Returns the `RUNE_EXPERIMENT` spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::V1 => "v1",
            Self::V2Min => "v2-min",
            Self::V2Structured => "v2-structured",
        }
    }
}

/// Whether the model-family notes are sent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OverlaySwitch {
    /// Only the notes adopted for a family.
    #[default]
    Default,
    /// The notes of every family.
    On,
    /// No notes.
    Off,
}

impl OverlaySwitch {
    /// Returns the `RUNE_EXPERIMENT` spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::On => "on",
            Self::Off => "off",
        }
    }
}

/// Switches for behaviour under measurement. The default is the shipped behaviour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Experiments {
    /// `run_note=on|off`: the non-interactive note in the first user message of `ask`.
    pub run_note: bool,
    /// `map=on|off`: forces the repository map on or off; `None` leaves the setting in force.
    pub map: Option<bool>,
    /// `question_nudge=on|off`: tells an `ask` run that ends on a question to decide and continue.
    pub question_nudge: bool,
    /// `snapshot=on|off`: adds the branch, changed files and top-level names to the first
    /// message of `ask`.
    pub snapshot: bool,
    /// `goal=off|judge`: makes an `ask` run without `--goal` work toward its prompt, with a judge.
    pub goal_judge: bool,
    /// `prompt=v1|v2-min|v2-structured`: the system prompt text and its tool guidance.
    pub prompt: PromptVariant,
    /// `overlay=default|on|off`: the notes for the model's family after the tool guidance.
    pub overlay: OverlaySwitch,
}

impl Default for Experiments {
    fn default() -> Self {
        Self {
            run_note: true,
            map: None,
            question_nudge: true,
            snapshot: false,
            goal_judge: false,
            prompt: PromptVariant::V1,
            overlay: OverlaySwitch::Default,
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
            if key == "goal" {
                experiments.goal_judge = match value {
                    "judge" => true,
                    "off" => false,
                    _ => {
                        return Err(invalid(format!("`{value}` is not a value of `{key}`"))
                            .with_hint("use off or judge"));
                    }
                };
                continue;
            }
            if key == "prompt" {
                experiments.prompt = match value {
                    "v1" => PromptVariant::V1,
                    "v2-min" => PromptVariant::V2Min,
                    "v2-structured" => PromptVariant::V2Structured,
                    _ => {
                        return Err(invalid(format!("`{value}` is not a value of `{key}`"))
                            .with_hint("use v1, v2-min or v2-structured"));
                    }
                };
                continue;
            }
            if key == "overlay" {
                experiments.overlay = match value {
                    "default" => OverlaySwitch::Default,
                    "on" => OverlaySwitch::On,
                    "off" => OverlaySwitch::Off,
                    _ => {
                        return Err(invalid(format!("`{value}` is not a value of `{key}`"))
                            .with_hint("use default, on or off"));
                    }
                };
                continue;
            }
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
                "question_nudge" => experiments.question_nudge = on,
                "snapshot" => experiments.snapshot = on,
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
        if self.question_nudge != default.question_nudge {
            parts.push(format!("question_nudge={}", spelling(self.question_nudge)));
        }
        if self.snapshot != default.snapshot {
            parts.push(format!("snapshot={}", spelling(self.snapshot)));
        }
        if self.goal_judge != default.goal_judge {
            parts.push("goal=judge".to_owned());
        }
        if self.prompt != default.prompt {
            parts.push(format!("prompt={}", self.prompt.as_str()));
        }
        if self.overlay != default.overlay {
            parts.push(format!("overlay={}", self.overlay.as_str()));
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
        let error = Experiments::parse("unknown=on").unwrap_err();
        assert_eq!(error.message(), "unknown experiment key `unknown`");
        assert_eq!(
            error.hint(),
            Some("known keys are goal, map, overlay, prompt, question_nudge, run_note, snapshot")
        );
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
    fn goal_takes_off_or_judge() {
        assert!(!Experiments::default().goal_judge);
        let judged = Experiments::parse("goal=judge").unwrap();
        assert!(judged.goal_judge);
        assert_eq!(judged.render(), "goal=judge");
        assert!(!Experiments::parse("goal=off").unwrap().goal_judge);
        assert_eq!(Experiments::parse("goal=off").unwrap().render(), "");
        let error = Experiments::parse("goal=on").unwrap_err();
        assert_eq!(error.message(), "`on` is not a value of `goal`");
        assert_eq!(error.hint(), Some("use off or judge"));
    }

    #[test]
    fn prompt_takes_one_of_three_variants() {
        assert_eq!(Experiments::default().prompt, PromptVariant::V1);
        assert_eq!(
            Experiments::parse("prompt=v2-min").unwrap().prompt,
            PromptVariant::V2Min
        );
        let structured = Experiments::parse("prompt=v2-structured").unwrap();
        assert_eq!(structured.prompt, PromptVariant::V2Structured);
        assert_eq!(structured.render(), "prompt=v2-structured");
        assert_eq!(Experiments::parse("prompt=v1").unwrap().render(), "");
        let error = Experiments::parse("prompt=v2").unwrap_err();
        assert_eq!(error.message(), "`v2` is not a value of `prompt`");
        assert_eq!(error.hint(), Some("use v1, v2-min or v2-structured"));
    }

    #[test]
    fn overlay_takes_default_on_or_off() {
        assert_eq!(Experiments::default().overlay, OverlaySwitch::Default);
        let on = Experiments::parse("overlay=on").unwrap();
        assert_eq!(on.overlay, OverlaySwitch::On);
        assert_eq!(on.render(), "overlay=on");
        let off = Experiments::parse("overlay=off,prompt=v1").unwrap();
        assert_eq!(off.overlay, OverlaySwitch::Off);
        assert_eq!(off.prompt, PromptVariant::V1);
        assert_eq!(off.render(), "overlay=off");
        assert_eq!(Experiments::parse("overlay=default").unwrap().render(), "");
        let error = Experiments::parse("overlay=yes").unwrap_err();
        assert_eq!(error.message(), "`yes` is not a value of `overlay`");
        assert_eq!(error.hint(), Some("use default, on or off"));
    }

    #[test]
    fn render_lists_only_changed_keys_sorted() {
        assert_eq!(Experiments::default().render(), "");
        assert_eq!(
            Experiments::parse("run_note=off,map=off").unwrap().render(),
            "map=off,run_note=off"
        );
        assert_eq!(Experiments::parse("run_note=on").unwrap().render(), "");
        let quiet = Experiments::parse("question_nudge=off").unwrap();
        assert!(!quiet.question_nudge);
        assert_eq!(quiet.render(), "question_nudge=off");
        assert!(Experiments::parse("snapshot=on").unwrap().snapshot);
        assert_eq!(
            Experiments::parse("snapshot=on").unwrap().render(),
            "snapshot=on"
        );
    }
}
