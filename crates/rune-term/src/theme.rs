//! Theme resolution: named palette slots, theme files, and color depth.
//!
//! A theme is a sparse map from a palette slot to a color. Every slot a theme
//! leaves out resolves to the built-in value for that theme's base, so a
//! partial theme file is still a complete theme rather than a source of holes.
//!
//! Colors are resolved to an escape sequence at the point of use rather than
//! stored as text, so a terminal without truecolor gets the same decisions
//! quantized to the 256 color palette.

use camino::Utf8Path;
use rune_core::budget::LimitName;
use rune_core::error::{ErrorCode, Result, RuneError};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::engine::{Color, Style};

/// The largest theme file accepted.
pub const MAX_THEME_BYTES: usize = 512 * 1024;

/// Token rules read from one theme file.
///
/// A rule list is a list of items, so it shares the entry bound a directory
/// listing uses rather than inventing a second ceiling for the same shape.
pub const MAX_THEME_RULES: usize = LimitName::ListEntries.default_value().effective(4096) as usize;

/// One addressable color in the palette.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub enum Slot {
    /// Primary text.
    #[default]
    Fg,
    /// Page background.
    Bg,
    /// Emphasis and interactive controls.
    Accent,
    /// Secondary text and metadata.
    Dim,
    /// Failures.
    Error,
    /// Success.
    Success,
    /// Hyperlinks.
    Link,
    /// Rules between regions.
    Divider,
    /// The rail beside user input.
    UserRail,
    /// Keywords and storage classes.
    Keyword,
    /// String literals.
    String,
    /// Numeric literals.
    Number,
    /// Comments.
    Comment,
    /// Function and type names.
    Function,
    /// Variables and tags.
    Variable,
    /// Operators.
    Operator,
}

/// Every slot, in palette order.
pub const SLOTS: [Slot; 16] = [
    Slot::Fg,
    Slot::Bg,
    Slot::Accent,
    Slot::Dim,
    Slot::Error,
    Slot::Success,
    Slot::Link,
    Slot::Divider,
    Slot::UserRail,
    Slot::Keyword,
    Slot::String,
    Slot::Number,
    Slot::Comment,
    Slot::Function,
    Slot::Variable,
    Slot::Operator,
];

/// Syntax slots, most specific prefix first.
///
/// Order matters: `keyword.operator` matches the `keyword` prefix too, so the
/// operator slot has to be offered the scope before the keyword slot is.
const SYNTAX_SLOTS: [Slot; 7] = [
    Slot::Operator,
    Slot::Number,
    Slot::String,
    Slot::Comment,
    Slot::Function,
    Slot::Variable,
    Slot::Keyword,
];

impl Slot {
    /// Returns the stable name, used in theme files and diagnostics.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Fg => "fg",
            Self::Bg => "bg",
            Self::Accent => "accent",
            Self::Dim => "dim",
            Self::Error => "error",
            Self::Success => "success",
            Self::Link => "link",
            Self::Divider => "divider",
            Self::UserRail => "user_rail",
            Self::Keyword => "keyword",
            Self::String => "string",
            Self::Number => "number",
            Self::Comment => "comment",
            Self::Function => "function",
            Self::Variable => "variable",
            Self::Operator => "operator",
        }
    }

    /// Returns the editor color keys this slot answers to.
    const fn color_keys(self) -> &'static [&'static str] {
        match self {
            Self::Fg => &["editor.foreground"],
            Self::Bg => &["editor.background"],
            Self::Accent => &["terminal.ansiBlue", "focusBorder"],
            Self::Dim => &["descriptionForeground", "editorLineNumber.foreground"],
            Self::Error => &["terminal.ansiRed", "editorError.foreground"],
            Self::Success => &["terminal.ansiGreen", "editorGutter.addedBackground"],
            Self::Link => &["textLink.foreground", "terminal.ansiCyan"],
            Self::Divider => &["panel.border", "editorGroup.border"],
            Self::UserRail => &["terminal.ansiMagenta", "editorGutter.modifiedBackground"],
            Self::Keyword
            | Self::String
            | Self::Number
            | Self::Comment
            | Self::Function
            | Self::Variable
            | Self::Operator => &[],
        }
    }

    /// Returns the token scope prefixes this slot answers to.
    const fn scope_prefixes(self) -> &'static [&'static str] {
        match self {
            Self::Keyword => &["keyword", "storage"],
            Self::String => &["string"],
            Self::Number => &["constant.numeric", "constant.language"],
            Self::Comment => &["comment"],
            Self::Function => &[
                "entity.name.function",
                "support.function",
                "entity.name.type",
            ],
            Self::Variable => &["variable", "entity.name.tag"],
            Self::Operator => &["keyword.operator"],
            Self::Fg
            | Self::Bg
            | Self::Accent
            | Self::Dim
            | Self::Error
            | Self::Success
            | Self::Link
            | Self::Divider
            | Self::UserRail => &[],
        }
    }
}

/// The polarity a theme is built for.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Base {
    /// Built for a dark terminal.
    #[default]
    Dark,
    /// Built for a light terminal.
    Light,
    /// No color at all, every slot left to the terminal.
    Mono,
}

/// A palette of slot colors.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Theme {
    base: Base,
    overrides: BTreeMap<Slot, Color>,
}

impl Theme {
    /// Returns a theme with no overrides, so every slot uses its built-in value.
    #[must_use]
    pub const fn builtin(base: Base) -> Self {
        Self {
            base,
            overrides: BTreeMap::new(),
        }
    }

    /// Returns the built-in dark theme.
    #[must_use]
    pub const fn fx_dark() -> Self {
        Self::builtin(Base::Dark)
    }

    /// Returns the built-in light theme.
    #[must_use]
    pub const fn fx_light() -> Self {
        Self::builtin(Base::Light)
    }

    /// Returns a theme that emits no color, for a terminal without it.
    ///
    /// A colorless theme reports the terminal default for every slot, including
    /// slots that were set, so a caller can opt out of color wholesale without
    /// the palette being rebuilt.
    #[must_use]
    pub const fn no_color() -> Self {
        Self::builtin(Base::Mono)
    }

    /// Returns the polarity this theme was built for.
    #[must_use]
    pub const fn base(&self) -> Base {
        self.base
    }

    /// Returns the color of a slot, falling back to the base value.
    #[must_use]
    pub fn get(&self, slot: Slot) -> Color {
        if self.base == Base::Mono {
            return Color::Default;
        }
        self.overrides
            .get(&slot)
            .copied()
            .unwrap_or_else(|| base_color(self.base, slot))
    }

    /// Overrides one slot.
    pub fn set(&mut self, slot: Slot, color: Color) {
        self.overrides.insert(slot, color);
    }

    /// Returns the slot overrides, without the base values.
    #[must_use]
    pub const fn overrides(&self) -> &BTreeMap<Slot, Color> {
        &self.overrides
    }

    /// Returns the style for one slot.
    ///
    /// The color is used as the foreground, except for [`Slot::Bg`] where it is
    /// the background, which is the only slot that describes a surface.
    #[must_use]
    pub fn style(&self, slot: Slot, truecolor: bool) -> Style {
        let color = self.indexed(slot, truecolor);
        match slot {
            Slot::Bg => Style {
                bg: color,
                ..Style::new()
            },
            _ => Style {
                fg: color,
                ..Style::new()
            },
        }
    }

    /// Returns the SGR sequence that selects a slot, empty for a default.
    #[must_use]
    pub fn sgr(&self, slot: Slot, truecolor: bool) -> String {
        self.style(slot, truecolor).sgr()
    }

    /// Returns the slot color as the terminal should receive it.
    fn indexed(&self, slot: Slot, truecolor: bool) -> Color {
        let color = self.get(slot);
        if truecolor {
            return color;
        }
        match color {
            Color::Rgb(..) => Color::Indexed(quantize_to_256(color)),
            other => other,
        }
    }

    /// Parses a theme file.
    ///
    /// Reads the `colors` map for surfaces and the `tokenColors` rules for
    /// syntax, keeping the last rule that names a scope. Slots neither source
    /// mentions keep their base value, so an incomplete file stays usable.
    pub fn parse(json: &str) -> Result<Self> {
        let value: Value = serde_json::from_str(json).map_err(|err| {
            RuneError::invalid_field("theme", format!("theme file is not valid JSON: {err}"))
        })?;
        let base = match value.get("type").and_then(Value::as_str) {
            Some("light") => Base::Light,
            Some("mono") => Base::Mono,
            _ => Base::Dark,
        };
        let mut theme = Self::builtin(base);
        if let Some(colors) = value.get("colors").and_then(Value::as_object) {
            for slot in SLOTS {
                if let Some(color) = slot
                    .color_keys()
                    .iter()
                    .find_map(|key| colors.get(*key))
                    .and_then(Value::as_str)
                    .and_then(parse_hex)
                {
                    theme.set(slot, color);
                }
            }
        }
        if let Some(rules) = value.get("tokenColors").and_then(Value::as_array) {
            if rules.len() > MAX_THEME_RULES {
                return Err(
                    RuneError::too_large("theme_rules", rules.len(), MAX_THEME_RULES).with_hint(
                        "a theme colors scopes, it does not list every file in a project",
                    ),
                );
            }
            for rule in rules {
                apply_rule(rule, &mut theme);
            }
        }
        Ok(theme)
    }

    /// Resolves a configured theme name, falling back to the built-in theme.
    ///
    /// [`Self::try_resolve`] reports why a name did not resolve; this one is
    /// the forgiving form a caller uses when any usable theme will do.
    #[must_use]
    pub fn resolve(name: Option<&str>, dark_mode: bool, themes_dir: &Utf8Path) -> Self {
        Self::try_resolve(name, dark_mode, themes_dir).unwrap_or_else(|_| builtin(dark_mode))
    }

    /// Resolves a configured theme name.
    ///
    /// `dark`, `light`, `none`, and `mono` are pins that ignore the terminal.
    /// Any other name is a theme file in `themes_dir`, optionally suffixed
    /// `.json`. A name that does not exist falls back to the built-in theme,
    /// but a name that exists and cannot be used is an error.
    pub fn try_resolve(name: Option<&str>, dark_mode: bool, themes_dir: &Utf8Path) -> Result<Self> {
        let trimmed = name.map(str::trim).unwrap_or_default();
        if trimmed.is_empty() {
            return Ok(builtin(dark_mode));
        }
        match trimmed.to_ascii_lowercase().as_str() {
            "dark" => return Ok(Self::fx_dark()),
            "light" => return Ok(Self::fx_light()),
            "none" | "no-color" | "mono" => return Ok(Self::no_color()),
            _ => {}
        }
        if let Some(theme) = load(trimmed, themes_dir)? {
            return Ok(theme);
        }
        if let Some(alternative) = sibling(trimmed, dark_mode)
            && let Some(theme) = load(&alternative, themes_dir)?
        {
            return Ok(theme);
        }
        Ok(builtin(dark_mode))
    }
}

/// Returns the built-in theme for a terminal polarity.
fn builtin(dark_mode: bool) -> Theme {
    if dark_mode {
        Theme::fx_dark()
    } else {
        Theme::fx_light()
    }
}

/// Applies one token rule to the theme.
fn apply_rule(rule: &Value, theme: &mut Theme) {
    let Some(color) = rule
        .get("settings")
        .and_then(|settings| settings.get("foreground"))
        .and_then(Value::as_str)
        .and_then(parse_hex)
    else {
        return;
    };
    for scope in scopes(rule) {
        if let Some(slot) = scope_slot(scope) {
            theme.set(slot, color);
        }
    }
}

/// Returns the scopes a rule names.
fn scopes(rule: &Value) -> Vec<&str> {
    match rule.get("scope") {
        Some(Value::String(raw)) => raw.split(',').map(str::trim).collect(),
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

/// Returns the slot a scope belongs to.
fn scope_slot(scope: &str) -> Option<Slot> {
    SYNTAX_SLOTS.iter().copied().find(|slot| {
        slot.scope_prefixes()
            .iter()
            .any(|prefix| matches(scope, prefix))
    })
}

/// Returns true when a scope is a prefix of the scope or one of its children.
fn matches(scope: &str, prefix: &str) -> bool {
    scope == prefix
        || scope
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('.'))
}

/// Returns the other polarity of a theme name.
///
/// A theme shipped for both polarities names them with a `-dark` or `-light`
/// suffix, so a name built for the other polarity has a sibling that fits this
/// terminal.
fn sibling(name: &str, dark_mode: bool) -> Option<String> {
    let (stem, suffix) = if let Some(stem) = name.strip_suffix("-dark") {
        (stem, Base::Dark)
    } else if let Some(stem) = name.strip_suffix("-light") {
        (stem, Base::Light)
    } else {
        let wanted = if dark_mode { "-dark" } else { "-light" };
        return Some(format!("{name}{wanted}"));
    };
    let wanted = if dark_mode { Base::Dark } else { Base::Light };
    if suffix == wanted {
        return None;
    }
    let wanted = if dark_mode { "-dark" } else { "-light" };
    Some(format!("{stem}{wanted}"))
}

/// Loads a theme file by name, returning `None` when no file matches.
fn load(name: &str, themes_dir: &Utf8Path) -> Result<Option<Theme>> {
    if name.contains(['/', '\\']) || name.contains("..") {
        return Err(RuneError::invalid_field(
            "theme",
            format!("`{name}` is a path, not a theme name"),
        )
        .with_hint("theme names resolve inside the themes directory"));
    }
    for candidate in [
        themes_dir.join(name),
        themes_dir.join(format!("{name}.json")),
    ] {
        if !candidate.is_file() {
            continue;
        }
        let size = std::fs::metadata(&candidate).map_err(|err| {
            RuneError::new(
                ErrorCode::NotFound,
                format!("theme file `{candidate}` cannot be read: {err}"),
            )
        })?;
        let observed = usize::try_from(size.len()).unwrap_or(usize::MAX);
        if observed > MAX_THEME_BYTES {
            return Err(
                RuneError::too_large("theme_file", observed, MAX_THEME_BYTES)
                    .with_hint("a theme file holds colors, not content"),
            );
        }
        let text = std::fs::read_to_string(&candidate).map_err(|err| {
            RuneError::new(
                ErrorCode::NotFound,
                format!("theme file `{candidate}` cannot be read: {err}"),
            )
        })?;
        return Ok(Some(Theme::parse(&text)?));
    }
    Ok(None)
}

/// Parses a `#rgb`, `#rrggbb`, or `#rrggbbaa` color.
fn parse_hex(raw: &str) -> Option<Color> {
    let trimmed = raw.trim();
    let digits = trimmed.strip_prefix('#').unwrap_or(trimmed);
    match digits.len() {
        3 => {
            let mut out = [0u8; 3];
            for (index, ch) in digits.chars().enumerate() {
                let value = ch.to_digit(16)?;
                out[index] = u8::try_from(value.saturating_mul(17)).ok()?;
            }
            Some(Color::Rgb(out[0], out[1], out[2]))
        }
        6 | 8 => {
            let body = digits.get(..6)?;
            Some(Color::Rgb(
                channel(body.get(0..2)?)?,
                channel(body.get(2..4)?)?,
                channel(body.get(4..6)?)?,
            ))
        }
        _ => None,
    }
}

/// Parses one hexadecimal color channel.
fn channel(digits: &str) -> Option<u8> {
    u8::from_str_radix(digits, 16).ok()
}

/// Returns the 256 color palette index nearest a color.
///
/// The palette holds a 6x6x6 cube of saturated colors and a 24 step grey ramp.
/// Whichever of the two is closer to the requested color wins, which is what
/// keeps a neutral grey out of the cube where it would gain a color cast.
#[must_use]
pub fn quantize_to_256(color: Color) -> u8 {
    match color {
        // The default foreground has no palette entry; index 7 is the
        // conventional stand-in for it.
        Color::Default => 7,
        Color::Indexed(index) => index,
        Color::Rgb(r, g, b) => nearest_index(r, g, b),
    }
}

/// Returns the palette index nearest an RGB triple.
fn nearest_index(r: u8, g: u8, b: u8) -> u8 {
    let (red, green, blue) = (level(r), level(g), level(b));
    let cube = 16u8
        .saturating_add(red.saturating_mul(36))
        .saturating_add(green.saturating_mul(6))
        .saturating_add(blue);
    let (grey_level, grey) = grey_step(r, g, b);
    let cube_distance = distance(r, g, b, axis(red), axis(green), axis(blue));
    let grey_distance = distance(r, g, b, grey, grey, grey);
    if grey_distance < cube_distance {
        232u8.saturating_add(grey_level)
    } else {
        cube
    }
}

/// Returns the cube coordinate nearest a channel value.
///
/// The cube's levels are 0, 95, 135, 175, 215, and 255, so the boundaries
/// between them are not evenly spaced.
const fn level(value: u8) -> u8 {
    if value < 48 {
        0
    } else if value < 115 {
        1
    } else if value < 155 {
        2
    } else if value < 195 {
        3
    } else if value < 235 {
        4
    } else {
        5
    }
}

/// Returns the channel value of a cube coordinate.
const fn axis(level: u8) -> u8 {
    match level {
        0 => 0,
        1 => 95,
        2 => 135,
        3 => 175,
        4 => 215,
        _ => 255,
    }
}

/// Returns the grey ramp step nearest a color and the grey it stands for.
fn grey_step(r: u8, g: u8, b: u8) -> (u8, u8) {
    let total = u16::from(r)
        .saturating_add(u16::from(g))
        .saturating_add(u16::from(b));
    let average = total.checked_div(3).unwrap_or(0);
    let step = average
        .saturating_sub(8)
        .checked_div(10)
        .unwrap_or(0)
        .min(23);
    let step = u8::try_from(step).unwrap_or(23);
    (step, 8u8.saturating_add(step.saturating_mul(10)))
}

/// Returns the squared distance between two RGB triples.
fn distance(r: u8, g: u8, b: u8, pr: u8, pg: u8, pb: u8) -> u32 {
    let diff = |left: u8, right: u8| {
        let value = i32::from(left).saturating_sub(i32::from(right));
        value.saturating_mul(value)
    };
    let sum = diff(r, pr)
        .saturating_add(diff(g, pg))
        .saturating_add(diff(b, pb));
    u32::try_from(sum).unwrap_or(u32::MAX)
}

/// Returns the built-in color of a slot under a base.
const fn base_color(base: Base, slot: Slot) -> Color {
    match base {
        Base::Mono => Color::Default,
        Base::Dark => dark_color(slot),
        Base::Light => light_color(slot),
    }
}

/// Returns the built-in dark palette.
const fn dark_color(slot: Slot) -> Color {
    match slot {
        Slot::Fg => Color::Rgb(0xe6, 0xe6, 0xe6),
        Slot::Bg => Color::Rgb(0x10, 0x10, 0x14),
        Slot::Accent => Color::Rgb(0x7a, 0xa2, 0xf7),
        Slot::Dim => Color::Rgb(0x6b, 0x72, 0x80),
        Slot::Error => Color::Rgb(0xf7, 0x76, 0x8e),
        Slot::Success => Color::Rgb(0x9e, 0xce, 0x6a),
        Slot::Link => Color::Rgb(0x7d, 0xcf, 0xff),
        Slot::Divider => Color::Rgb(0x2a, 0x2a, 0x33),
        Slot::UserRail => Color::Rgb(0xbb, 0x9a, 0xf7),
        Slot::Keyword => Color::Rgb(0xbb, 0x9a, 0xf7),
        Slot::String => Color::Rgb(0x9e, 0xce, 0x6a),
        Slot::Number => Color::Rgb(0xff, 0x9e, 0x64),
        Slot::Comment => Color::Rgb(0x56, 0x5f, 0x89),
        Slot::Function => Color::Rgb(0x7a, 0xa2, 0xf7),
        Slot::Variable => Color::Rgb(0xc0, 0xca, 0xf5),
        Slot::Operator => Color::Rgb(0x89, 0xdd, 0xff),
    }
}

/// Returns the built-in light palette.
const fn light_color(slot: Slot) -> Color {
    match slot {
        Slot::Fg => Color::Rgb(0x1f, 0x23, 0x28),
        Slot::Bg => Color::Rgb(0xff, 0xff, 0xff),
        Slot::Accent => Color::Rgb(0x09, 0x69, 0xda),
        Slot::Dim => Color::Rgb(0x6e, 0x77, 0x81),
        Slot::Error => Color::Rgb(0xcf, 0x22, 0x2e),
        Slot::Success => Color::Rgb(0x1a, 0x7f, 0x37),
        Slot::Link => Color::Rgb(0x09, 0x69, 0xda),
        Slot::Divider => Color::Rgb(0xd0, 0xd7, 0xde),
        Slot::UserRail => Color::Rgb(0x82, 0x50, 0xdf),
        Slot::Keyword => Color::Rgb(0xcf, 0x22, 0x2e),
        Slot::String => Color::Rgb(0x0a, 0x30, 0x69),
        Slot::Number => Color::Rgb(0x95, 0x38, 0x00),
        Slot::Comment => Color::Rgb(0x6e, 0x77, 0x81),
        Slot::Function => Color::Rgb(0x82, 0x50, 0xdf),
        Slot::Variable => Color::Rgb(0x24, 0x29, 0x2f),
        Slot::Operator => Color::Rgb(0x05, 0x50, 0xae),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_slot_inherits_the_default() {
        let theme = Theme::parse(r##"{"type":"dark","colors":{"editor.foreground":"#ff0000"}}"##)
            .expect("theme");
        assert_eq!(theme.get(Slot::Fg), Color::Rgb(0xff, 0x00, 0x00));
        assert_eq!(theme.get(Slot::Error), Theme::fx_dark().get(Slot::Error));
    }

    #[test]
    fn a_partial_theme_keeps_every_slot_resolvable() {
        let theme = Theme::parse(r#"{"type":"light","colors":{}}"#).expect("theme");
        for slot in SLOTS {
            assert_eq!(theme.get(slot), Theme::fx_light().get(slot));
        }
    }

    #[test]
    fn token_colors_fill_the_syntax_slots() {
        let json = r##"{
            "type": "dark",
            "tokenColors": [
                {"scope": "variable", "settings": {"foreground": "#111111"}},
                {"scope": ["keyword.operator"], "settings": {"foreground": "#222222"}},
                {"scope": "keyword", "settings": {"foreground": "#333333"}}
            ]
        }"##;
        let theme = Theme::parse(json).expect("theme");
        assert_eq!(theme.get(Slot::Variable), Color::Rgb(0x11, 0x11, 0x11));
        assert_eq!(theme.get(Slot::Operator), Color::Rgb(0x22, 0x22, 0x22));
        assert_eq!(theme.get(Slot::Keyword), Color::Rgb(0x33, 0x33, 0x33));
    }

    #[test]
    fn the_last_rule_for_a_scope_wins() {
        let json = r##"{
            "tokenColors": [
                {"scope": "string", "settings": {"foreground": "#111111"}},
                {"scope": "string.quoted", "settings": {"foreground": "#444444"}}
            ]
        }"##;
        let theme = Theme::parse(json).expect("theme");
        assert_eq!(theme.get(Slot::String), Color::Rgb(0x44, 0x44, 0x44));
    }

    #[test]
    fn a_short_hex_color_expands() {
        assert_eq!(parse_hex("#abc"), Some(Color::Rgb(0xaa, 0xbb, 0xcc)));
        assert_eq!(parse_hex("#aabbccdd"), Some(Color::Rgb(0xaa, 0xbb, 0xcc)));
        assert_eq!(parse_hex("nope"), None);
    }

    #[test]
    fn parse_rejects_a_file_that_is_not_json() {
        let err = Theme::parse("{oops").expect_err("invalid theme");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.field(), Some("theme"));
    }

    #[test]
    fn quantize_picks_the_cube_for_a_saturated_color() {
        assert_eq!(quantize_to_256(Color::Rgb(0, 0, 0)), 16);
        assert_eq!(quantize_to_256(Color::Rgb(255, 0, 0)), 196);
        assert_eq!(quantize_to_256(Color::Rgb(255, 255, 255)), 231);
    }

    #[test]
    fn quantize_picks_the_grey_ramp_for_a_neutral_midtone() {
        // The cube's nearest column is 135, a distance of 147; the ramp holds
        // 128 exactly.
        assert_eq!(quantize_to_256(Color::Rgb(128, 128, 128)), 244);
        assert_eq!(quantize_to_256(Color::Rgb(8, 8, 8)), 232);
    }

    #[test]
    fn quantize_keeps_an_existing_palette_index() {
        assert_eq!(quantize_to_256(Color::Indexed(200)), 200);
        assert_eq!(quantize_to_256(Color::Default), 7);
    }

    #[test]
    fn a_pin_ignores_the_terminal_polarity() {
        let dir = Utf8Path::new("/nonexistent-themes");
        let dark = Theme::resolve(Some("dark"), false, dir);
        assert_eq!(dark.base(), Base::Dark);
        let light = Theme::resolve(Some("light"), true, dir);
        assert_eq!(light.base(), Base::Light);
    }

    #[test]
    fn a_dark_name_resolves_to_its_light_sibling_on_a_light_terminal() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = Utf8Path::from_path(dir.path()).expect("utf8 path");
        std::fs::write(
            root.join("solarized-light.json"),
            r##"{"type":"light","colors":{"editor.foreground":"#657b83"}}"##,
        )
        .expect("write theme");
        let theme = Theme::try_resolve(Some("solarized-dark"), false, root).expect("theme");
        assert_eq!(theme.get(Slot::Fg), Color::Rgb(0x65, 0x7b, 0x83));
        // The same name on a dark terminal has no file and no sibling to use.
        let theme = Theme::try_resolve(Some("solarized-dark"), true, root).expect("theme");
        assert_eq!(theme.get(Slot::Fg), Theme::fx_dark().get(Slot::Fg));
    }

    #[test]
    fn a_named_file_wins_over_the_builtin() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = Utf8Path::from_path(dir.path()).expect("utf8 path");
        std::fs::write(
            root.join("work.json"),
            r##"{"type":"dark","colors":{"editor.foreground":"#010203"}}"##,
        )
        .expect("write theme");
        let theme = Theme::try_resolve(Some("work"), true, root).expect("theme");
        assert_eq!(theme.get(Slot::Fg), Color::Rgb(0x01, 0x02, 0x03));
        assert_eq!(theme.overrides().len(), 1);
    }

    #[test]
    fn an_unusable_theme_file_is_an_error_rather_than_a_silent_fallback() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = Utf8Path::from_path(dir.path()).expect("utf8 path");
        std::fs::write(root.join("broken.json"), "{oops").expect("write theme");
        let err = Theme::try_resolve(Some("broken"), true, root).expect_err("broken theme");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        // The forgiving form still produces a usable theme.
        let theme = Theme::resolve(Some("broken"), true, root);
        assert_eq!(theme, Theme::fx_dark());
    }

    #[test]
    fn a_theme_name_that_is_a_path_is_rejected() {
        let dir = Utf8Path::new("/nonexistent-themes");
        let err = Theme::try_resolve(Some("../escape"), true, dir).expect_err("path name");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(
            Theme::resolve(Some("../escape"), true, dir),
            Theme::fx_dark()
        );
    }

    #[test]
    fn a_theme_file_past_the_cap_is_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = Utf8Path::from_path(dir.path()).expect("utf8 path");
        let filler = "x".repeat(MAX_THEME_BYTES.saturating_add(1));
        std::fs::write(root.join("huge.json"), filler).expect("write theme");
        let err = Theme::try_resolve(Some("huge"), true, root).expect_err("oversized theme");
        assert_eq!(err.code(), ErrorCode::TooLarge);
    }

    #[test]
    fn a_no_color_theme_emits_nothing() {
        let theme = Theme::no_color();
        assert!(theme.sgr(Slot::Error, true).is_empty());
        assert!(!theme.style(Slot::Error, true).has_sgr());
        assert_eq!(theme.get(Slot::Error), Color::Default);
    }

    #[test]
    fn without_truecolor_an_rgb_color_is_quantized() {
        let theme = Theme::fx_dark();
        assert!(theme.sgr(Slot::Error, true).contains("38;2;"));
        assert!(theme.sgr(Slot::Error, false).contains("38;5;"));
    }

    #[test]
    fn a_theme_with_more_rules_than_the_cap_is_refused() {
        let rules: Vec<String> = (0..MAX_THEME_RULES.saturating_add(1))
            .map(|index| {
                format!(
                    r##"{{"scope":"variable.tag{index}","settings":{{"foreground":"#123456"}}}}"##
                )
            })
            .collect();
        let json = format!(r#"{{"tokenColors":[{}]}}"#, rules.join(","));
        let err = Theme::parse(&json).expect_err("oversized rule list");
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert_eq!(err.field(), Some("theme_rules"));
        assert_eq!(err.detail().observed.as_deref(), Some("1001"));

        // One rule fewer is accepted, so the cap is the boundary and not a
        // blanket refusal of a large theme.
        let rules: Vec<String> = (0..MAX_THEME_RULES)
            .map(|index| {
                format!(
                    r##"{{"scope":"variable.tag{index}","settings":{{"foreground":"#123456"}}}}"##
                )
            })
            .collect();
        let json = format!(r#"{{"tokenColors":[{}]}}"#, rules.join(","));
        let theme = Theme::parse(&json).expect("theme at the cap");
        assert_eq!(theme.get(Slot::Variable), Color::Rgb(0x12, 0x34, 0x56));
    }

    #[test]
    fn slot_names_are_stable() {
        assert_eq!(Slot::UserRail.name(), "user_rail");
        assert_eq!(SLOTS.len(), 16);
    }
}
