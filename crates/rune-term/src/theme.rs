//! Theme resolution: named palette slots, theme files, and color depth.
//!
//! A theme is a sparse map from a palette slot to a color. Every slot a theme
//! leaves out resolves to the built-in value for that theme's base, so a
//! partial theme file is still a complete theme rather than a source of holes.
//!
//! Colors are resolved to an escape sequence at the point of use rather than
//! stored as text, so one theme serves every [`ColorDepth`]: direct color as
//! written, the 256 color palette by quantization, and 16 colors by a choice
//! made by hand for each slot.

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
    /// Warnings.
    Warning,
    /// Markdown headings.
    Heading,
    /// Behind code blocks.
    CodeBg,
    /// Behind an added diff line.
    DiffAddedBg,
    /// Behind a removed diff line.
    DiffRemovedBg,
    /// Behind the changed words of an added diff line.
    DiffAddedWord,
    /// Behind the changed words of a removed diff line.
    DiffRemovedWord,
}

/// Every slot, in palette order.
pub const SLOTS: [Slot; 23] = [
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
    Slot::Warning,
    Slot::Heading,
    Slot::CodeBg,
    Slot::DiffAddedBg,
    Slot::DiffRemovedBg,
    Slot::DiffAddedWord,
    Slot::DiffRemovedWord,
];

/// How many colors the terminal renders, decided once at startup.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ColorDepth {
    /// Direct 24-bit color.
    TrueColor,
    /// The 256 color palette.
    Ansi256,
    /// The 16 basic colors.
    #[default]
    Ansi16,
    /// No color at all.
    None,
}

impl ColorDepth {
    /// Every depth, deepest first.
    pub const ALL: [Self; 4] = [Self::TrueColor, Self::Ansi256, Self::Ansi16, Self::None];

    /// Returns the name the `color` setting and `/status` use.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::TrueColor => "truecolor",
            Self::Ansi256 => "256",
            Self::Ansi16 => "16",
            Self::None => "none",
        }
    }
}

/// One of the 16 basic terminal colors, or the terminal default.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ansi16 {
    /// The terminal's default color.
    Default,
    /// Palette entry 0.
    Black,
    /// Palette entry 1.
    Red,
    /// Palette entry 2.
    Green,
    /// Palette entry 3.
    Yellow,
    /// Palette entry 4.
    Blue,
    /// Palette entry 5.
    Magenta,
    /// Palette entry 6.
    Cyan,
    /// Palette entry 7.
    White,
    /// Palette entry 8.
    BrightBlack,
    /// Palette entry 9.
    BrightRed,
    /// Palette entry 10.
    BrightGreen,
    /// Palette entry 11.
    BrightYellow,
    /// Palette entry 12.
    BrightBlue,
    /// Palette entry 13.
    BrightMagenta,
    /// Palette entry 14.
    BrightCyan,
    /// Palette entry 15.
    BrightWhite,
}

impl Ansi16 {
    /// Returns the color as a palette index, or the terminal default.
    #[must_use]
    pub const fn color(self) -> Color {
        match self {
            Self::Default => Color::Default,
            Self::Black => Color::Indexed(0),
            Self::Red => Color::Indexed(1),
            Self::Green => Color::Indexed(2),
            Self::Yellow => Color::Indexed(3),
            Self::Blue => Color::Indexed(4),
            Self::Magenta => Color::Indexed(5),
            Self::Cyan => Color::Indexed(6),
            Self::White => Color::Indexed(7),
            Self::BrightBlack => Color::Indexed(8),
            Self::BrightRed => Color::Indexed(9),
            Self::BrightGreen => Color::Indexed(10),
            Self::BrightYellow => Color::Indexed(11),
            Self::BrightBlue => Color::Indexed(12),
            Self::BrightMagenta => Color::Indexed(13),
            Self::BrightCyan => Color::Indexed(14),
            Self::BrightWhite => Color::Indexed(15),
        }
    }
}

/// Terminal programs known to render direct color.
const TRUECOLOR_PROGRAMS: [&str; 5] = ["iTerm.app", "vscode", "ghostty", "WezTerm", "Hyper"];

/// `TERM` values known to render direct color.
const TRUECOLOR_TERMS: [&str; 3] = ["xterm-kitty", "alacritty", "foot"];

/// Decides the color depth from the `color` setting and the environment.
///
/// The first rule that matches wins. `NO_COLOR` comes before the setting, so a
/// terminal that refuses color is never forced into it; `FORCE_COLOR` comes
/// before the checks that a pipe or `TERM=dumb` would otherwise fail.
#[must_use]
pub fn detect_depth(
    setting: &str,
    stdout_is_terminal: bool,
    lookup: impl Fn(&str) -> Option<String>,
) -> ColorDepth {
    if lookup("NO_COLOR").is_some_and(|value| !value.is_empty()) {
        return ColorDepth::None;
    }
    match setting.trim().to_ascii_lowercase().as_str() {
        "truecolor" => return ColorDepth::TrueColor,
        "256" => return ColorDepth::Ansi256,
        "16" => return ColorDepth::Ansi16,
        "none" => return ColorDepth::None,
        _ => {}
    }
    match lookup("FORCE_COLOR").as_deref() {
        Some("3") => return ColorDepth::TrueColor,
        Some("2") => return ColorDepth::Ansi256,
        Some("1") => return ColorDepth::Ansi16,
        Some("0") => return ColorDepth::None,
        _ => {}
    }
    let term = lookup("TERM").unwrap_or_default();
    if term == "dumb" || !stdout_is_terminal {
        return ColorDepth::None;
    }
    let colorterm = lookup("COLORTERM").unwrap_or_default();
    if colorterm.eq_ignore_ascii_case("truecolor") || colorterm.eq_ignore_ascii_case("24bit") {
        return ColorDepth::TrueColor;
    }
    if lookup("WT_SESSION").is_some() {
        return ColorDepth::TrueColor;
    }
    let program = lookup("TERM_PROGRAM").unwrap_or_default();
    if TRUECOLOR_PROGRAMS.contains(&program.as_str()) {
        return ColorDepth::TrueColor;
    }
    if term.contains("direct") || TRUECOLOR_TERMS.contains(&term.as_str()) {
        return ColorDepth::TrueColor;
    }
    if term.contains("256color") || program == "Apple_Terminal" {
        return ColorDepth::Ansi256;
    }
    ColorDepth::Ansi16
}

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
            Self::Warning => "warning",
            Self::Heading => "heading",
            Self::CodeBg => "code_bg",
            Self::DiffAddedBg => "diff_added_bg",
            Self::DiffRemovedBg => "diff_removed_bg",
            Self::DiffAddedWord => "diff_added_word",
            Self::DiffRemovedWord => "diff_removed_word",
        }
    }

    /// Returns true for a slot that colors a surface rather than text.
    #[must_use]
    pub const fn is_background(self) -> bool {
        matches!(
            self,
            Self::Bg
                | Self::CodeBg
                | Self::DiffAddedBg
                | Self::DiffRemovedBg
                | Self::DiffAddedWord
                | Self::DiffRemovedWord
        )
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
            | Self::Operator
            | Self::Warning
            | Self::Heading
            | Self::CodeBg
            | Self::DiffAddedBg
            | Self::DiffRemovedBg
            | Self::DiffAddedWord
            | Self::DiffRemovedWord => &[],
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
            | Self::UserRail
            | Self::Warning
            | Self::Heading
            | Self::CodeBg
            | Self::DiffAddedBg
            | Self::DiffRemovedBg
            | Self::DiffAddedWord
            | Self::DiffRemovedWord => &[],
        }
    }
}

/// The built-in palette a theme is based on.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Base {
    /// Built for a dark terminal.
    #[default]
    Dark,
    /// Built for a light terminal.
    Light,
    /// Bright foregrounds on black, with at least 7:1 contrast for every slot.
    HighContrast,
    /// The dark or light palette with changes shown in blue and orange, which
    /// colour-blind users tell apart where red and green blur.
    Colorblind {
        /// Built for a light terminal.
        light: bool,
    },
    /// The terminal's own 16 colours for every slot, never a background, so the
    /// user's palette decides every colour.
    Ansi {
        /// Built for a light terminal.
        light: bool,
    },
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
    pub const fn dark() -> Self {
        Self::builtin(Base::Dark)
    }

    /// Returns the built-in light theme.
    #[must_use]
    pub const fn light() -> Self {
        Self::builtin(Base::Light)
    }

    /// Returns the built-in high-contrast theme.
    ///
    /// Every foreground has at least a 7:1 WCAG contrast ratio against the
    /// explicit black background, including after 256-color quantization.
    #[must_use]
    pub const fn high_contrast() -> Self {
        Self::builtin(Base::HighContrast)
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

    /// Returns the built-in palette this theme is based on.
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

    /// Returns the style for one slot at a color depth.
    ///
    /// A background slot colors the surface and every other slot the text.
    /// High contrast always sets both colors at the two palette depths, so
    /// terminal defaults cannot lower the contrast of a themed run. At 16
    /// colors each slot takes the base's hand-picked color, a background slot
    /// takes none, and a user override is not consulted.
    #[must_use]
    pub fn style(&self, slot: Slot, depth: ColorDepth) -> Style {
        if self.base == Base::Mono || depth == ColorDepth::None {
            return Style::new();
        }
        if depth == ColorDepth::Ansi16 || matches!(self.base, Base::Ansi { .. }) {
            if slot.is_background() {
                return Style::new();
            }
            return Style {
                fg: base_ansi16(self.base, slot).color(),
                ..Style::new()
            };
        }
        if self.base == Base::HighContrast {
            let (fg, bg) = if slot.is_background() {
                (Slot::Fg, slot)
            } else {
                (slot, Slot::Bg)
            };
            return Style {
                fg: self.indexed(fg, depth),
                bg: self.indexed(bg, depth),
                ..Style::new()
            };
        }
        let color = self.indexed(slot, depth);
        if slot.is_background() {
            Style {
                bg: color,
                ..Style::new()
            }
        } else {
            Style {
                fg: color,
                ..Style::new()
            }
        }
    }

    /// Returns the SGR sequence that selects a slot, empty for a default.
    #[must_use]
    pub fn sgr(&self, slot: Slot, depth: ColorDepth) -> String {
        self.style(slot, depth).sgr()
    }

    /// Returns the slot color at one of the two palette depths.
    fn indexed(&self, slot: Slot, depth: ColorDepth) -> Color {
        let color = self.get(slot);
        if depth == ColorDepth::TrueColor {
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
    pub fn resolve(
        name: Option<&str>,
        light: bool,
        config_themes: &Utf8Path,
        data_themes: &Utf8Path,
    ) -> Self {
        Self::try_resolve(name, light, config_themes, data_themes)
            .unwrap_or_else(|_| builtin(!light))
    }

    /// Resolves a configured theme name.
    ///
    /// Empty or `auto` follows the terminal polarity. `dark`, `light`,
    /// `high-contrast`, `colorblind`, `ansi`, `none`, `no-color` and `mono` are
    /// built in; the first three and the colourless names ignore the polarity.
    /// Any other name is `<name>.toml` in `config_themes`, then a theme file in
    /// `data_themes`, optionally suffixed `.json`. A name that does not exist
    /// falls back to the built-in theme, but a name that exists and cannot be
    /// used is an error.
    pub fn try_resolve(
        name: Option<&str>,
        light: bool,
        config_themes: &Utf8Path,
        data_themes: &Utf8Path,
    ) -> Result<Self> {
        let dark_mode = !light;
        let trimmed = name.map(str::trim).unwrap_or_default();
        match trimmed.to_ascii_lowercase().as_str() {
            "" | "auto" => return Ok(builtin(dark_mode)),
            "dark" => return Ok(Self::dark()),
            "light" => return Ok(Self::light()),
            "high-contrast" => return Ok(Self::high_contrast()),
            "colorblind" => return Ok(Self::builtin(Base::Colorblind { light })),
            "ansi" => return Ok(Self::builtin(Base::Ansi { light })),
            "none" | "no-color" | "mono" => return Ok(Self::no_color()),
            _ => {}
        }
        if let Some(theme) = load_toml(trimmed, config_themes)? {
            return Ok(theme);
        }
        if let Some(theme) = load(trimmed, data_themes)? {
            return Ok(theme);
        }
        if let Some(alternative) = sibling(trimmed, dark_mode)
            && let Some(theme) = load(&alternative, data_themes)?
        {
            return Ok(theme);
        }
        Ok(builtin(dark_mode))
    }

    /// Parses a theme written as TOML: a required `base` of `dark` or `light`,
    /// then any slot name set to a `#rrggbb` colour. Unset slots keep the base.
    pub fn parse_toml(text: &str) -> Result<Self> {
        let table: toml::Table = text.parse().map_err(|err| {
            RuneError::invalid_field("theme", format!("theme file is not valid TOML: {err}"))
        })?;
        let base = match table.get("base").and_then(toml::Value::as_str) {
            Some("dark") => Base::Dark,
            Some("light") => Base::Light,
            _ => {
                return Err(RuneError::invalid_field(
                    "theme",
                    "theme file needs base = \"dark\" or base = \"light\"",
                ));
            }
        };
        let mut theme = Self::builtin(base);
        for (key, value) in &table {
            if key == "base" {
                continue;
            }
            let Some(slot) = SLOTS.iter().copied().find(|slot| slot.name() == key) else {
                let valid: Vec<&str> = SLOTS.iter().map(|slot| slot.name()).collect();
                return Err(RuneError::invalid_field(
                    "theme",
                    format!(
                        "unknown theme slot `{key}`; valid slots: {}",
                        valid.join(", ")
                    ),
                ));
            };
            let color = value
                .as_str()
                .filter(|raw| raw.trim().len() == 7)
                .and_then(parse_hex)
                .ok_or_else(|| {
                    RuneError::invalid_field(
                        "theme",
                        format!("`{key}` must be a colour such as #7aa2f7"),
                    )
                })?;
            theme.set(slot, color);
        }
        Ok(theme)
    }
}

/// Returns true when the terminal's background is light: from the probe's
/// answer when there is one, else from `COLORFGBG`, whose last field `7` or
/// `15` names a light background; dark otherwise.
#[must_use]
pub fn terminal_is_light(caps: &crate::probe::TerminalCaps) -> bool {
    light_from(caps.background, std::env::var("COLORFGBG").ok().as_deref())
}

/// The polarity rule of [`terminal_is_light`], with its inputs stated.
#[must_use]
pub fn light_from(background: Option<(u8, u8, u8)>, colorfgbg: Option<&str>) -> bool {
    if let Some(background) = background {
        return crate::probe::is_light(background);
    }
    colorfgbg
        .and_then(|value| value.rsplit(';').next())
        .is_some_and(|last| matches!(last.trim(), "7" | "15"))
}

/// The WCAG contrast ratio of two colours, from 1 to 21.
#[must_use]
pub fn contrast(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
    fn linear(channel: u8) -> f64 {
        let value = f64::from(channel) / 255.0;
        if value <= 0.040_45 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    }
    let luminance =
        |(r, g, b): (u8, u8, u8)| 0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b);
    let (high, low) = {
        let (x, y) = (luminance(a), luminance(b));
        if x >= y { (x, y) } else { (y, x) }
    };
    (high + 0.05) / (low + 0.05)
}

/// Loads `<name>.toml` from the configuration themes directory, if it exists.
fn load_toml(name: &str, themes_dir: &Utf8Path) -> Result<Option<Theme>> {
    if name.contains(['/', '\\']) || name.contains("..") {
        return Err(RuneError::invalid_field(
            "theme",
            format!("`{name}` is a path, not a theme name"),
        )
        .with_hint("theme names resolve inside the themes directory"));
    }
    let path = themes_dir.join(format!("{name}.toml"));
    if !path.is_file() {
        return Ok(None);
    }
    let size = std::fs::metadata(&path).map_or(0, |metadata| metadata.len());
    let observed = usize::try_from(size).unwrap_or(usize::MAX);
    if observed > MAX_THEME_BYTES {
        return Err(
            RuneError::too_large("theme_file", observed, MAX_THEME_BYTES)
                .with_hint("a theme file holds colors, not content"),
        );
    }
    let text = std::fs::read_to_string(&path).map_err(|err| {
        RuneError::new(
            ErrorCode::NotFound,
            format!("theme file `{path}` cannot be read: {err}"),
        )
    })?;
    Theme::parse_toml(&text).map(Some)
}

/// Returns the built-in theme for a terminal polarity.
fn builtin(dark_mode: bool) -> Theme {
    if dark_mode {
        Theme::dark()
    } else {
        Theme::light()
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
        Base::Dark => dark_color(slot).0,
        Base::Light => light_color(slot).0,
        Base::HighContrast => high_contrast_color(slot).0,
        Base::Colorblind { light } => match colorblind_color(slot, light) {
            Some(color) => color,
            None if light => light_color(slot).0,
            None => dark_color(slot).0,
        },
        Base::Ansi { .. } => base_ansi16(base, slot).color(),
    }
}

/// The colours the colour-blind palette changes: blue for what was added or
/// went well, orange for what was removed or failed.
const fn colorblind_color(slot: Slot, light: bool) -> Option<Color> {
    let hex = match (slot, light) {
        (Slot::Success | Slot::String, false) => 0x56_b4e9,
        (Slot::Error, false) => 0xe6_9f00,
        (Slot::DiffAddedBg, false) => 0x1d_2f45,
        (Slot::DiffRemovedBg, false) => 0x45_301a,
        (Slot::DiffAddedWord, false) => 0x2d_4f75,
        (Slot::DiffRemovedWord, false) => 0x7a_5220,
        (Slot::Success | Slot::String, true) => 0x00_72b2,
        (Slot::Error, true) => 0xd5_5e00,
        (Slot::DiffAddedBg, true) => 0xdc_ecf7,
        (Slot::DiffRemovedBg, true) => 0xf9_e3d3,
        (Slot::DiffAddedWord, true) => 0xb3_d4ec,
        (Slot::DiffRemovedWord, true) => 0xf0_c3a0,
        _ => return None,
    };
    Some(rgb(hex))
}

/// Returns the 16 color choice of a slot under a base.
const fn base_ansi16(base: Base, slot: Slot) -> Ansi16 {
    match base {
        Base::Mono => Ansi16::Default,
        Base::Dark | Base::Ansi { light: false } => dark_color(slot).1,
        Base::Light | Base::Ansi { light: true } => light_color(slot).1,
        Base::HighContrast => high_contrast_color(slot).1,
        Base::Colorblind { light } => match (slot, light) {
            (Slot::Success, _) => Ansi16::Blue,
            (Slot::Error, false) => Ansi16::Yellow,
            (Slot::Error, true) => Ansi16::Red,
            (_, true) => light_color(slot).1,
            (_, false) => dark_color(slot).1,
        },
    }
}

/// Returns a hex color as a direct color.
const fn rgb(hex: u32) -> Color {
    let [_, r, g, b] = hex.to_be_bytes();
    Color::Rgb(r, g, b)
}

/// Bright colors from the fixed 256 color palette, unchanged by quantization.
const fn high_contrast_color(slot: Slot) -> (Color, Ansi16) {
    match slot {
        Slot::Fg => (rgb(0xff_ffff), Ansi16::Default),
        Slot::Bg => (rgb(0x00_0000), Ansi16::Default),
        Slot::Accent => (rgb(0x5f_d7ff), Ansi16::Blue),
        Slot::Dim => (rgb(0xd7_d7d7), Ansi16::BrightBlack),
        Slot::Error => (rgb(0xff_8787), Ansi16::Red),
        Slot::Success => (rgb(0x87_ff87), Ansi16::Green),
        Slot::Link => (rgb(0x5f_d7ff), Ansi16::Cyan),
        Slot::Divider => (rgb(0xd7_d7d7), Ansi16::BrightBlack),
        Slot::UserRail => (rgb(0xd7_afff), Ansi16::Magenta),
        Slot::Keyword => (rgb(0xd7_afff), Ansi16::Magenta),
        Slot::String => (rgb(0x87_ff87), Ansi16::Green),
        Slot::Number => (rgb(0xff_d75f), Ansi16::Yellow),
        Slot::Comment => (rgb(0xd7_d7d7), Ansi16::BrightBlack),
        Slot::Function => (rgb(0x5f_d7ff), Ansi16::Blue),
        Slot::Variable => (rgb(0xff_ffff), Ansi16::Default),
        Slot::Operator => (rgb(0x5f_d7ff), Ansi16::Cyan),
        Slot::Warning => (rgb(0xff_d75f), Ansi16::Yellow),
        Slot::Heading => (rgb(0x5f_d7ff), Ansi16::Blue),
        Slot::CodeBg => (rgb(0x1c_1c1c), Ansi16::Default),
        Slot::DiffAddedBg => (rgb(0x00_5f00), Ansi16::Default),
        Slot::DiffRemovedBg => (rgb(0x5f_0000), Ansi16::Default),
        Slot::DiffAddedWord => (rgb(0x00_8700), Ansi16::Default),
        Slot::DiffRemovedWord => (rgb(0x87_0000), Ansi16::Default),
    }
}

/// Returns the built-in dark palette.
const fn dark_color(slot: Slot) -> (Color, Ansi16) {
    match slot {
        Slot::Fg => (rgb(0xe6_e6e6), Ansi16::Default),
        Slot::Bg => (rgb(0x10_1014), Ansi16::Default),
        Slot::Accent => (rgb(0x7a_a2f7), Ansi16::Blue),
        Slot::Dim => (rgb(0x6b_7280), Ansi16::BrightBlack),
        Slot::Error => (rgb(0xf7_768e), Ansi16::Red),
        Slot::Success => (rgb(0x9e_ce6a), Ansi16::Green),
        Slot::Link => (rgb(0x7d_cfff), Ansi16::Cyan),
        Slot::Divider => (rgb(0x2a_2a33), Ansi16::BrightBlack),
        Slot::UserRail => (rgb(0xbb_9af7), Ansi16::Magenta),
        Slot::Keyword => (rgb(0xbb_9af7), Ansi16::Magenta),
        Slot::String => (rgb(0x9e_ce6a), Ansi16::Green),
        Slot::Number => (rgb(0xff_9e64), Ansi16::Yellow),
        Slot::Comment => (rgb(0x56_5f89), Ansi16::BrightBlack),
        Slot::Function => (rgb(0x7a_a2f7), Ansi16::Blue),
        Slot::Variable => (rgb(0xc0_caf5), Ansi16::Default),
        Slot::Operator => (rgb(0x89_ddff), Ansi16::Cyan),
        Slot::Warning => (rgb(0xe0_af68), Ansi16::Yellow),
        Slot::Heading => (rgb(0x7a_a2f7), Ansi16::Blue),
        Slot::CodeBg => (rgb(0x1a_1b26), Ansi16::Default),
        Slot::DiffAddedBg => (rgb(0x1f_3326), Ansi16::Default),
        Slot::DiffRemovedBg => (rgb(0x3b_1f24), Ansi16::Default),
        Slot::DiffAddedWord => (rgb(0x2f_5a3a), Ansi16::Default),
        Slot::DiffRemovedWord => (rgb(0x6b_2a33), Ansi16::Default),
    }
}

/// Returns the built-in light palette.
///
/// Yellow and cyan text are hard to read on white, so the 16 color choices
/// that would use them take magenta or blue instead.
const fn light_color(slot: Slot) -> (Color, Ansi16) {
    match slot {
        Slot::Fg => (rgb(0x1f_2328), Ansi16::Default),
        Slot::Bg => (rgb(0xff_ffff), Ansi16::Default),
        Slot::Accent => (rgb(0x09_69da), Ansi16::Blue),
        Slot::Dim => (rgb(0x6e_7781), Ansi16::BrightBlack),
        Slot::Error => (rgb(0xcf_222e), Ansi16::Red),
        Slot::Success => (rgb(0x1a_7f37), Ansi16::Green),
        Slot::Link => (rgb(0x09_69da), Ansi16::Blue),
        Slot::Divider => (rgb(0xd0_d7de), Ansi16::BrightBlack),
        Slot::UserRail => (rgb(0x82_50df), Ansi16::Magenta),
        Slot::Keyword => (rgb(0xcf_222e), Ansi16::Magenta),
        Slot::String => (rgb(0x0a_3069), Ansi16::Green),
        Slot::Number => (rgb(0x95_3800), Ansi16::Magenta),
        Slot::Comment => (rgb(0x6e_7781), Ansi16::BrightBlack),
        Slot::Function => (rgb(0x82_50df), Ansi16::Blue),
        Slot::Variable => (rgb(0x24_292f), Ansi16::Default),
        Slot::Operator => (rgb(0x05_50ae), Ansi16::Blue),
        Slot::Warning => (rgb(0x9a_6700), Ansi16::Magenta),
        Slot::Heading => (rgb(0x09_69da), Ansi16::Blue),
        Slot::CodeBg => (rgb(0xf6_f8fa), Ansi16::Default),
        Slot::DiffAddedBg => (rgb(0xda_fbe1), Ansi16::Default),
        Slot::DiffRemovedBg => (rgb(0xff_ebe9), Ansi16::Default),
        Slot::DiffAddedWord => (rgb(0xac_eebb), Ansi16::Default),
        Slot::DiffRemovedWord => (rgb(0xff_cecb), Ansi16::Default),
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
        assert_eq!(theme.get(Slot::Error), Theme::dark().get(Slot::Error));
    }

    #[test]
    fn a_partial_theme_keeps_every_slot_resolvable() {
        let theme = Theme::parse(r#"{"type":"light","colors":{}}"#).expect("theme");
        for slot in SLOTS {
            assert_eq!(theme.get(slot), Theme::light().get(slot));
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
        let dark = Theme::resolve(Some("dark"), true, dir, dir);
        assert_eq!(dark.base(), Base::Dark);
        let light = Theme::resolve(Some("light"), false, dir, dir);
        assert_eq!(light.base(), Base::Light);
    }

    #[test]
    fn high_contrast_is_a_named_pin_for_either_terminal_polarity() {
        let dir = Utf8Path::new("/nonexistent-themes");
        for dark_mode in [false, true] {
            for name in ["high-contrast", " HIGH-CONTRAST "] {
                let theme = Theme::try_resolve(Some(name), !dark_mode, dir, dir).expect("theme");
                assert_eq!(theme, Theme::high_contrast());
            }
        }
    }

    #[test]
    fn every_high_contrast_pair_meets_seven_to_one_at_both_color_depths() {
        let theme = Theme::high_contrast();
        for depth in [ColorDepth::Ansi256, ColorDepth::TrueColor] {
            for slot in SLOTS {
                let style = theme.style(slot, depth);
                let mut grid = crate::engine::Grid::new(2, 1).expect("grid");
                grid.feed(format!("{}x", theme.sgr(slot, depth)).as_bytes())
                    .expect("SGR");
                assert_eq!(grid.cell(0, 0).expect("cell").style, style);
                if slot.is_background() && slot != Slot::Bg {
                    // A surface slot keeps the primary text on its own color.
                    assert_eq!(style.fg, theme.style(Slot::Fg, depth).fg);
                    assert_eq!(style.bg, theme.indexed(slot, depth));
                    continue;
                }
                let foreground = luminance(palette_rgb(style.fg));
                let background = luminance(palette_rgb(style.bg));
                let ratio = contrast_ratio(foreground, background);
                assert!(ratio >= 7.0, "{}: {ratio}:1 at {depth:?}", slot.name());
                assert_eq!(palette_rgb(style.bg), [0, 0, 0]);
            }
        }
    }

    /// Decode the fixed xterm color cube independently of quantization.
    fn palette_rgb(color: Color) -> [u8; 3] {
        match color {
            Color::Rgb(r, g, b) => [r, g, b],
            Color::Indexed(index @ 16..=231) => {
                let cube = index.saturating_sub(16);
                let levels = [0, 95, 135, 175, 215, 255];
                [
                    levels[usize::from(cube / 36)],
                    levels[usize::from((cube / 6) % 6)],
                    levels[usize::from(cube % 6)],
                ]
            }
            Color::Indexed(index @ 232..=255) => {
                let grey = 8u8.saturating_add(index.saturating_sub(232).saturating_mul(10));
                [grey, grey, grey]
            }
            _ => panic!("high contrast must use explicit RGB or fixed cube colors: {color:?}"),
        }
    }

    #[allow(clippy::arithmetic_side_effects)]
    fn luminance(rgb: [u8; 3]) -> f64 {
        let linear = rgb.map(|channel| {
            let value = f64::from(channel) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        });
        0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2]
    }

    #[allow(clippy::arithmetic_side_effects)]
    fn contrast_ratio(first: f64, second: f64) -> f64 {
        (first.max(second) + 0.05) / (first.min(second) + 0.05)
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
        let theme = Theme::try_resolve(Some("solarized-dark"), true, root, root).expect("theme");
        assert_eq!(theme.get(Slot::Fg), Color::Rgb(0x65, 0x7b, 0x83));
        // The same name on a dark terminal has no file and no sibling to use.
        let theme = Theme::try_resolve(Some("solarized-dark"), false, root, root).expect("theme");
        assert_eq!(theme.get(Slot::Fg), Theme::dark().get(Slot::Fg));
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
        let theme = Theme::try_resolve(Some("work"), false, root, root).expect("theme");
        assert_eq!(theme.get(Slot::Fg), Color::Rgb(0x01, 0x02, 0x03));
        assert_eq!(theme.overrides().len(), 1);
    }

    #[test]
    fn an_unusable_theme_file_is_an_error_rather_than_a_silent_fallback() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = Utf8Path::from_path(dir.path()).expect("utf8 path");
        std::fs::write(root.join("broken.json"), "{oops").expect("write theme");
        let err = Theme::try_resolve(Some("broken"), false, root, root).expect_err("broken theme");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        // The forgiving form still produces a usable theme.
        let theme = Theme::resolve(Some("broken"), false, root, root);
        assert_eq!(theme, Theme::dark());
    }

    #[test]
    fn a_theme_name_that_is_a_path_is_rejected() {
        let dir = Utf8Path::new("/nonexistent-themes");
        let err = Theme::try_resolve(Some("../escape"), false, dir, dir).expect_err("path name");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(
            Theme::resolve(Some("../escape"), false, dir, dir),
            Theme::dark()
        );
    }

    #[test]
    fn a_theme_file_past_the_cap_is_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = Utf8Path::from_path(dir.path()).expect("utf8 path");
        let filler = "x".repeat(MAX_THEME_BYTES.saturating_add(1));
        std::fs::write(root.join("huge.json"), filler).expect("write theme");
        let err = Theme::try_resolve(Some("huge"), false, root, root).expect_err("oversized theme");
        assert_eq!(err.code(), ErrorCode::TooLarge);
    }

    #[test]
    fn a_no_color_theme_emits_nothing() {
        let theme = Theme::no_color();
        assert!(theme.sgr(Slot::Error, ColorDepth::TrueColor).is_empty());
        assert!(!theme.style(Slot::Error, ColorDepth::TrueColor).has_sgr());
        assert_eq!(theme.get(Slot::Error), Color::Default);
    }

    #[test]
    fn at_256_colors_an_rgb_color_is_quantized() {
        let theme = Theme::dark();
        assert!(
            theme
                .sgr(Slot::Error, ColorDepth::TrueColor)
                .contains("38;2;")
        );
        assert!(
            theme
                .sgr(Slot::Error, ColorDepth::Ansi256)
                .contains("38;5;")
        );
    }

    #[test]
    fn depth_detection_follows_the_documented_order() {
        use ColorDepth::{Ansi16, Ansi256, None, TrueColor};
        /// A setting, whether stdout is a terminal, the environment, the depth.
        type Case = (
            &'static str,
            bool,
            &'static [(&'static str, &'static str)],
            ColorDepth,
        );
        let cases: &[Case] = &[
            (
                "auto",
                true,
                &[("NO_COLOR", "1"), ("COLORTERM", "truecolor")],
                None,
            ),
            ("truecolor", true, &[("NO_COLOR", "1")], None),
            (
                "auto",
                true,
                &[("NO_COLOR", ""), ("COLORTERM", "truecolor")],
                TrueColor,
            ),
            ("truecolor", false, &[("TERM", "dumb")], TrueColor),
            ("256", true, &[("COLORTERM", "truecolor")], Ansi256),
            (" 16 ", true, &[("COLORTERM", "truecolor")], Ansi16),
            ("NONE", true, &[("COLORTERM", "truecolor")], None),
            ("auto", false, &[("FORCE_COLOR", "3")], TrueColor),
            ("auto", false, &[("FORCE_COLOR", "2")], Ansi256),
            ("auto", false, &[("FORCE_COLOR", "1")], Ansi16),
            (
                "auto",
                true,
                &[("FORCE_COLOR", "0"), ("COLORTERM", "truecolor")],
                None,
            ),
            ("auto", false, &[("FORCE_COLOR", "yes")], None),
            (
                "auto",
                true,
                &[("TERM", "dumb"), ("COLORTERM", "truecolor")],
                None,
            ),
            ("auto", false, &[("COLORTERM", "truecolor")], None),
            ("auto", true, &[("COLORTERM", "TrueColor")], TrueColor),
            ("auto", true, &[("COLORTERM", "24bit")], TrueColor),
            ("auto", true, &[("WT_SESSION", "abc")], TrueColor),
            ("auto", true, &[("TERM_PROGRAM", "iTerm.app")], TrueColor),
            ("auto", true, &[("TERM_PROGRAM", "WezTerm")], TrueColor),
            ("auto", true, &[("TERM", "xterm-direct")], TrueColor),
            ("auto", true, &[("TERM", "xterm-kitty")], TrueColor),
            ("auto", true, &[("TERM", "tmux-256color")], Ansi256),
            ("auto", true, &[("TERM_PROGRAM", "Apple_Terminal")], Ansi256),
            ("auto", true, &[("TERM", "xterm")], Ansi16),
            ("auto", true, &[], Ansi16),
            ("sideways", true, &[("TERM", "xterm")], Ansi16),
        ];
        for (setting, terminal, vars, expected) in cases {
            let depth = detect_depth(setting, *terminal, |key| {
                vars.iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| (*value).to_owned())
            });
            assert_eq!(depth, *expected, "{setting:?} {terminal} {vars:?}");
        }
    }

    #[test]
    fn every_slot_has_a_value_at_every_depth() {
        for theme in [Theme::dark(), Theme::light(), Theme::high_contrast()] {
            for slot in SLOTS {
                let pick = |style: Style| {
                    if slot.is_background() {
                        style.bg
                    } else {
                        style.fg
                    }
                };
                let full = theme.style(slot, ColorDepth::TrueColor);
                assert!(matches!(pick(full), Color::Rgb(..)), "{slot:?} {full:?}");
                let palette = theme.style(slot, ColorDepth::Ansi256);
                assert!(
                    matches!(pick(palette), Color::Indexed(16..)),
                    "{slot:?} {palette:?}"
                );
                let basic = theme.style(slot, ColorDepth::Ansi16);
                assert_eq!(basic.bg, Color::Default, "{slot:?}");
                assert!(
                    matches!(basic.fg, Color::Default | Color::Indexed(0..16)),
                    "{slot:?} {basic:?}"
                );
                assert!(!theme.style(slot, ColorDepth::None).has_sgr(), "{slot:?}");
            }
            // The 16 color choices are colors, not the terminal default for
            // everything.
            let colored = SLOTS
                .iter()
                .filter(|slot| theme.style(**slot, ColorDepth::Ansi16).has_sgr())
                .count();
            assert!(colored >= 14, "{:?}: {colored}", theme.base());
        }
        let light = Theme::light();
        for slot in [Slot::Warning, Slot::Number, Slot::Link, Slot::Operator] {
            let fg = light.style(slot, ColorDepth::Ansi16).fg;
            assert!(
                !matches!(fg, Color::Indexed(3 | 6 | 11 | 14)),
                "{slot:?} is yellow or cyan on white"
            );
        }
    }

    #[test]
    fn background_slots_are_empty_at_sixteen_colours() {
        for theme in [Theme::dark(), Theme::light(), Theme::high_contrast()] {
            for slot in SLOTS.into_iter().filter(|slot| slot.is_background()) {
                assert!(theme.sgr(slot, ColorDepth::Ansi16).is_empty(), "{slot:?}");
                assert!(!theme.sgr(slot, ColorDepth::Ansi256).is_empty(), "{slot:?}");
            }
        }
        let background = SLOTS.iter().filter(|slot| slot.is_background()).count();
        assert_eq!(background, 6);
    }

    #[test]
    fn user_overrides_do_not_change_sixteen_colour_choices() {
        let json = r##"{
            "type": "dark",
            "colors": {"editor.foreground": "#ff0000", "terminal.ansiRed": "#00ff00"},
            "tokenColors": [{"scope": "keyword", "settings": {"foreground": "#0000ff"}}]
        }"##;
        let theme = Theme::parse(json).expect("theme");
        let builtin = Theme::dark();
        for slot in [Slot::Fg, Slot::Error, Slot::Keyword] {
            assert_eq!(
                theme.style(slot, ColorDepth::Ansi16),
                builtin.style(slot, ColorDepth::Ansi16),
                "{slot:?}"
            );
            assert_ne!(
                theme.style(slot, ColorDepth::TrueColor),
                builtin.style(slot, ColorDepth::TrueColor),
                "{slot:?}"
            );
            assert_ne!(
                theme.style(slot, ColorDepth::Ansi256),
                builtin.style(slot, ColorDepth::Ansi256),
                "{slot:?}"
            );
        }
        assert_eq!(
            theme.style(Slot::Error, ColorDepth::Ansi16).fg,
            Color::Indexed(1)
        );
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
        assert_eq!(Slot::DiffRemovedWord.name(), "diff_removed_word");
        assert_eq!(SLOTS.len(), 23);
        for (index, slot) in SLOTS.iter().enumerate() {
            assert!(!SLOTS[..index].contains(slot), "{slot:?} listed twice");
        }
    }

    fn rgb_of(color: Color) -> (u8, u8, u8) {
        match color {
            Color::Rgb(r, g, b) => (r, g, b),
            other => panic!("not a direct colour: {other:?}"),
        }
    }

    #[test]
    fn auto_follows_the_terminal_background() {
        assert!(light_from(Some((0xff, 0xff, 0xff)), None));
        assert!(!light_from(Some((0x1e, 0x1e, 0x1e)), Some("0;15")));
        let dir = Utf8Path::new("/nonexistent-themes");
        assert_eq!(
            Theme::try_resolve(Some("auto"), true, dir, dir).expect("theme"),
            Theme::light()
        );
        assert_eq!(
            Theme::try_resolve(None, false, dir, dir).expect("theme"),
            Theme::dark()
        );
        // A pin ignores the polarity.
        assert_eq!(
            Theme::try_resolve(Some("light"), false, dir, dir).expect("theme"),
            Theme::light()
        );
    }

    #[test]
    fn colorfgbg_last_field_decides_without_a_reply() {
        assert!(light_from(None, Some("0;15")));
        assert!(light_from(None, Some("0;default;7")));
        assert!(!light_from(None, Some("15;0")));
        assert!(!light_from(None, None));
    }

    fn toml_dir(text: &str) -> (tempfile::TempDir, camino::Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_owned()).expect("utf8");
        std::fs::write(root.join("mine.toml"), text).expect("theme");
        (dir, root)
    }

    #[test]
    fn toml_theme_overrides_named_slots_and_inherits_the_rest() {
        let (_dir, root) = toml_dir("base = \"light\"\naccent = \"#7aa2f7\"\n");
        let empty = Utf8Path::new("/nonexistent-themes");
        let theme = Theme::try_resolve(Some("mine"), false, &root, empty).expect("theme");
        assert_eq!(theme.base(), Base::Light);
        assert_eq!(theme.get(Slot::Accent), Color::Rgb(0x7a, 0xa2, 0xf7));
        assert_eq!(theme.get(Slot::Error), Theme::light().get(Slot::Error));
    }

    #[test]
    fn toml_theme_with_an_unknown_slot_lists_the_valid_ones() {
        let (_dir, root) = toml_dir("base = \"dark\"\ncolour = \"#fff\"\n");
        let empty = Utf8Path::new("/nonexistent-themes");
        let err = Theme::try_resolve(Some("mine"), false, &root, empty).expect_err("unknown slot");
        let message = err.to_string();
        assert!(
            message.contains("unknown theme slot `colour`; valid slots: fg, bg, accent"),
            "{message}"
        );
        let (_dir, root) = toml_dir("base = \"dark\"\naccent = \"blue\"\n");
        let err = Theme::try_resolve(Some("mine"), false, &root, empty).expect_err("bad colour");
        assert!(
            err.to_string()
                .contains("`accent` must be a colour such as #7aa2f7"),
            "{err}"
        );
    }

    #[test]
    fn toml_theme_requires_a_base() {
        let (_dir, root) = toml_dir("accent = \"#7aa2f7\"\n");
        let empty = Utf8Path::new("/nonexistent-themes");
        let err = Theme::try_resolve(Some("mine"), false, &root, empty).expect_err("no base");
        assert!(
            err.to_string()
                .contains("theme file needs base = \"dark\" or base = \"light\""),
            "{err}"
        );
        let (_dir, root) = toml_dir("base = \"sepia\"\n");
        assert!(Theme::try_resolve(Some("mine"), false, &root, empty).is_err());
    }

    #[test]
    fn colorblind_uses_blue_and_orange_for_changes() {
        let dark = Theme::builtin(Base::Colorblind { light: false });
        assert_eq!(dark.get(Slot::Success), Color::Rgb(0x56, 0xb4, 0xe9));
        assert_eq!(dark.get(Slot::Error), Color::Rgb(0xe6, 0x9f, 0x00));
        assert_eq!(dark.get(Slot::DiffAddedBg), Color::Rgb(0x1d, 0x2f, 0x45));
        assert_eq!(dark.get(Slot::Accent), Theme::dark().get(Slot::Accent));
        let light = Theme::builtin(Base::Colorblind { light: true });
        assert_eq!(light.get(Slot::String), Color::Rgb(0x00, 0x72, 0xb2));
        assert_eq!(light.get(Slot::Error), Color::Rgb(0xd5, 0x5e, 0x00));
        assert_eq!(
            light.get(Slot::DiffRemovedWord),
            Color::Rgb(0xf0, 0xc3, 0xa0)
        );
        assert_eq!(
            dark.style(Slot::Success, ColorDepth::Ansi16).fg,
            Ansi16::Blue.color()
        );
        assert_eq!(
            dark.style(Slot::Error, ColorDepth::Ansi16).fg,
            Ansi16::Yellow.color()
        );
        assert_eq!(
            light.style(Slot::Error, ColorDepth::Ansi16).fg,
            Ansi16::Red.color()
        );
        let dir = Utf8Path::new("/nonexistent-themes");
        assert_eq!(
            Theme::try_resolve(Some("colorblind"), true, dir, dir).expect("theme"),
            light
        );
    }

    #[test]
    fn ansi_theme_emits_only_sixteen_colour_codes() {
        let theme = Theme::builtin(Base::Ansi { light: false });
        for slot in SLOTS {
            for depth in [
                ColorDepth::TrueColor,
                ColorDepth::Ansi256,
                ColorDepth::Ansi16,
            ] {
                let sgr = theme.sgr(slot, depth);
                assert!(
                    !sgr.contains("38;2") && !sgr.contains("38;5") && !sgr.contains("48;"),
                    "{slot:?} {depth:?}: {sgr:?}"
                );
            }
            assert_eq!(theme.style(slot, ColorDepth::None), Style::new());
        }
        assert_ne!(theme.sgr(Slot::Error, ColorDepth::TrueColor), "");
    }

    #[test]
    fn built_in_text_meets_the_contrast_rule() {
        let cases = [
            (Theme::dark(), (0x10, 0x10, 0x14)),
            (Theme::light(), (0xff, 0xff, 0xff)),
            (Theme::high_contrast(), (0x00, 0x00, 0x00)),
            (
                Theme::builtin(Base::Colorblind { light: false }),
                (0x10, 0x10, 0x14),
            ),
            (
                Theme::builtin(Base::Colorblind { light: true }),
                (0xff, 0xff, 0xff),
            ),
        ];
        for (theme, background) in cases {
            let fg = contrast(rgb_of(theme.get(Slot::Fg)), background);
            let dim = contrast(rgb_of(theme.get(Slot::Dim)), background);
            assert!(fg >= 4.5, "{:?} fg {fg}", theme.base());
            assert!(dim >= 3.0, "{:?} dim {dim}", theme.base());
        }
        assert!((contrast((0, 0, 0), (0xff, 0xff, 0xff)) - 21.0).abs() < 0.01);
    }
}
