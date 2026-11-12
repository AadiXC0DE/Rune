//! Key actions by id, their default keys, and the keymap a key file builds.
//!
//! Every rebindable key of the composer and the viewer is one row of
//! [`ACTIONS`]. The reader asks a [`Keymap`] which action a key means in its
//! current [`Context`], so the defaults here are the only description of what
//! those keys do.

use std::collections::{BTreeMap, HashMap};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Where a key is read.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Context {
    /// The idle prompt's draft.
    Editor,
    /// The draft while a turn runs; editor keys apply there too.
    Turn,
    /// A menu or picker; editor keys apply there too.
    Menu,
    /// The transcript viewer.
    Viewer,
    /// A permission or question prompt; editor keys apply there too.
    Approval,
}

impl Context {
    /// The lower-case name shown in listings and errors.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Editor => "editor",
            Self::Turn => "turn",
            Self::Menu => "menu",
            Self::Viewer => "viewer",
            Self::Approval => "approval",
        }
    }

    /// The context whose keys apply when this one has none for a key.
    const fn fallback(self) -> Option<Self> {
        match self {
            Self::Turn | Self::Menu | Self::Approval => Some(Self::Editor),
            Self::Editor | Self::Viewer => None,
        }
    }
}

/// One bindable action.
#[derive(Debug)]
pub struct ActionInfo {
    /// The id a key file names.
    pub id: &'static str,
    /// Where the action is read.
    pub context: Context,
    /// Its keys when the key file does not name it.
    pub defaults: &'static [&'static str],
    /// One short line for help and listings.
    pub summary: &'static str,
}

const fn action(
    id: &'static str,
    context: Context,
    defaults: &'static [&'static str],
    summary: &'static str,
) -> ActionInfo {
    ActionInfo {
        id,
        context,
        defaults,
        summary,
    }
}

/// Every bindable action, in the order help lists them.
pub const ACTIONS: &[ActionInfo] = &[
    action("submit", Context::Editor, &["enter"], "send the draft"),
    action(
        "newline",
        Context::Editor,
        &["shift+enter", "ctrl+j", "alt+enter"],
        "insert a newline",
    ),
    action(
        "complete",
        Context::Editor,
        &["tab"],
        "complete a command, value or path",
    ),
    action(
        "queue",
        Context::Turn,
        &["tab"],
        "queue the draft for after this turn",
    ),
    action(
        "pull_queued",
        Context::Turn,
        &["alt+up"],
        "edit the newest queued message",
    ),
    action(
        "interrupt",
        Context::Turn,
        &["esc"],
        "stop the running turn; with text, press twice",
    ),
    action(
        "clear",
        Context::Editor,
        &["esc"],
        "clear the draft; up brings it back",
    ),
    action(
        "history_search",
        Context::Editor,
        &["ctrl+r"],
        "search earlier prompts",
    ),
    action(
        "transcript",
        Context::Editor,
        &["ctrl+o"],
        "open the transcript",
    ),
    action(
        "external_editor",
        Context::Editor,
        &["ctrl+g"],
        "edit the draft in VISUAL, EDITOR, or vi",
    ),
    action(
        "mode_cycle",
        Context::Editor,
        &["shift+tab"],
        "cycle ask, auto and plan",
    ),
    action(
        "toggle_plan",
        Context::Editor,
        &["ctrl+t"],
        "show or hide the plan above the prompt",
    ),
    action(
        "word_left",
        Context::Editor,
        &["alt+b", "ctrl+b", "alt+left"],
        "move one word left",
    ),
    action(
        "word_right",
        Context::Editor,
        &["alt+f", "ctrl+f", "alt+right"],
        "move one word right",
    ),
    action(
        "kill_word_back",
        Context::Editor,
        &["ctrl+w", "alt+backspace"],
        "cut the word before the caret",
    ),
    action(
        "kill_word_forward",
        Context::Editor,
        &["alt+d"],
        "cut the word after the caret",
    ),
    action(
        "kill_to_line_end",
        Context::Editor,
        &["ctrl+k"],
        "cut to the end of the line",
    ),
    action(
        "kill_to_line_start",
        Context::Editor,
        &["ctrl+u"],
        "cut to the start of the line",
    ),
    action(
        "yank",
        Context::Editor,
        &["ctrl+y"],
        "insert the last cut text",
    ),
    action(
        "yank_pop",
        Context::Editor,
        &["alt+y"],
        "cycle earlier cuts after a yank",
    ),
    action(
        "undo",
        Context::Editor,
        &["ctrl+_", "ctrl+7"],
        "undo a draft edit",
    ),
    action("redo", Context::Editor, &["alt+r"], "redo a draft edit"),
    action(
        "line_start",
        Context::Editor,
        &["home", "ctrl+a"],
        "start of the line; again for the draft",
    ),
    action(
        "line_end",
        Context::Editor,
        &["end", "ctrl+e"],
        "end of the line; again for the draft",
    ),
    action(
        "viewer_close",
        Context::Viewer,
        &["esc", "ctrl+o", "ctrl+d"],
        "close the transcript",
    ),
];

/// Keys that share a context and may still be bound together, one in each:
/// the turn's binding wins while a turn runs.
const SHARED: &[(&str, &str, &str)] =
    &[("tab", "queue", "complete"), ("esc", "interrupt", "clear")];

/// The modifiers a binding can name.
const MODIFIERS: KeyModifiers = KeyModifiers::CONTROL
    .union(KeyModifiers::ALT)
    .union(KeyModifiers::SHIFT);

/// A key with its modifiers, normalised so a name and an event compare equal.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct Key {
    /// The key itself; Shift+Tab is [`KeyCode::BackTab`] and a shifted letter
    /// is its upper-case character.
    pub code: KeyCode,
    /// Control, Alt and Shift only; Shift never accompanies a character.
    pub mods: KeyModifiers,
}

impl Key {
    /// Normalises an event the way [`parse_key`] normalises a name.
    #[must_use]
    pub fn from_event(event: &KeyEvent) -> Self {
        let mut mods = event.modifiers.intersection(MODIFIERS);
        if matches!(event.code, KeyCode::Char(_) | KeyCode::BackTab) {
            mods.remove(KeyModifiers::SHIFT);
        }
        Self {
            code: event.code,
            mods,
        }
    }

    /// The name a key file uses for this key.
    #[must_use]
    pub fn name(&self) -> String {
        let mut name = String::new();
        if self.mods.contains(KeyModifiers::CONTROL) {
            name.push_str("ctrl+");
        }
        if self.mods.contains(KeyModifiers::ALT) {
            name.push_str("alt+");
        }
        let shifted = self.mods.contains(KeyModifiers::SHIFT)
            || matches!(self.code, KeyCode::BackTab)
            || matches!(self.code, KeyCode::Char(c) if c.is_ascii_uppercase());
        if shifted {
            name.push_str("shift+");
        }
        match self.code {
            KeyCode::Char(' ') => name.push_str("space"),
            KeyCode::Char(c) => name.push(c.to_ascii_lowercase()),
            KeyCode::F(n) => {
                name.push('f');
                name.push_str(&n.to_string());
            }
            code => name.push_str(
                NAMED
                    .iter()
                    .find(|(_, named)| *named == code)
                    .map_or("?", |(text, _)| text),
            ),
        }
        name
    }

    /// Keys to try for this key, most specific first.
    ///
    /// A binding without a modifier also answers the key with extra ones, as
    /// Control-Enter submits; Control is kept before Alt, and a character
    /// never loses its last modifier, so Alt-q still types `q`.
    fn candidates(self) -> impl Iterator<Item = Self> {
        let control = KeyModifiers::CONTROL;
        let alt = KeyModifiers::ALT;
        let shift = KeyModifiers::SHIFT;
        let order = [
            control | alt | shift,
            control | alt,
            control | shift,
            control,
            alt | shift,
            alt,
            shift,
            KeyModifiers::NONE,
        ];
        let character = matches!(self.code, KeyCode::Char(_));
        order.into_iter().filter_map(move |mods| {
            let keeps = if !self.mods.contains(mods) {
                false
            } else if character && self.mods.contains(control) {
                mods.contains(control)
            } else if character && self.mods.contains(alt) {
                mods.contains(alt)
            } else {
                true
            };
            keeps.then_some(Self {
                code: self.code,
                mods,
            })
        })
    }
}

/// Named keys other than characters and function keys.
const NAMED: &[(&str, KeyCode)] = &[
    ("enter", KeyCode::Enter),
    ("tab", KeyCode::Tab),
    ("tab", KeyCode::BackTab),
    ("esc", KeyCode::Esc),
    ("backspace", KeyCode::Backspace),
    ("delete", KeyCode::Delete),
    ("up", KeyCode::Up),
    ("down", KeyCode::Down),
    ("left", KeyCode::Left),
    ("right", KeyCode::Right),
    ("home", KeyCode::Home),
    ("end", KeyCode::End),
    ("pageup", KeyCode::PageUp),
    ("pagedown", KeyCode::PageDown),
];

/// Reads a key name such as `ctrl+j`, `alt+enter` or `shift+tab`.
///
/// # Errors
///
/// Returns a message for a name that is not lower case, names no key, or
/// shifts something other than a letter or Tab.
pub fn parse_key(name: &str) -> Result<Key, String> {
    let unreadable = || format!("`{name}` is not a key name");
    if name.chars().any(char::is_uppercase) {
        return Err(unreadable());
    }
    let mut mods = KeyModifiers::NONE;
    let mut rest = name;
    loop {
        if let Some(after) = rest.strip_prefix("ctrl+").filter(|after| !after.is_empty()) {
            mods.insert(KeyModifiers::CONTROL);
            rest = after;
        } else if let Some(after) = rest.strip_prefix("alt+").filter(|after| !after.is_empty()) {
            mods.insert(KeyModifiers::ALT);
            rest = after;
        } else if let Some(after) = rest
            .strip_prefix("shift+")
            .filter(|after| !after.is_empty())
        {
            mods.insert(KeyModifiers::SHIFT);
            rest = after;
        } else {
            break;
        }
    }
    let mut chars = rest.chars();
    let code = match (chars.next(), chars.next()) {
        (Some(c), None) => KeyCode::Char(c),
        _ if rest == "space" => KeyCode::Char(' '),
        _ => {
            if let Some(n) = rest
                .strip_prefix('f')
                .and_then(|digits| digits.parse::<u8>().ok())
                .filter(|n| (1..=12).contains(n))
                .filter(|_| !rest.starts_with("f0"))
            {
                KeyCode::F(n)
            } else {
                NAMED
                    .iter()
                    .find(|(text, code)| *text == rest && *code != KeyCode::BackTab)
                    .map(|(_, code)| *code)
                    .ok_or_else(unreadable)?
            }
        }
    };
    let code = if mods.contains(KeyModifiers::SHIFT) {
        match code {
            KeyCode::Tab => {
                mods.remove(KeyModifiers::SHIFT);
                KeyCode::BackTab
            }
            KeyCode::Char(c) if c.is_ascii_lowercase() => {
                mods.remove(KeyModifiers::SHIFT);
                KeyCode::Char(c.to_ascii_uppercase())
            }
            KeyCode::Char(_) => return Err(unreadable()),
            code => code,
        }
    } else {
        code
    };
    Ok(Key { code, mods })
}

/// Which action each key means in each context.
#[derive(Debug, Clone)]
pub struct Keymap {
    map: HashMap<(Context, Key), &'static str>,
    keys: HashMap<&'static str, Vec<Key>>,
}

impl Keymap {
    /// The keys of [`ACTIONS`] as written there.
    #[must_use]
    pub fn defaults() -> Self {
        Self::with_overrides(&BTreeMap::new()).unwrap_or_else(|_| Self {
            map: HashMap::new(),
            keys: HashMap::new(),
        })
    }

    /// Builds the keymap with each named action's keys replaced by its list.
    ///
    /// # Errors
    ///
    /// Returns a message naming an unknown action, an unreadable key, a key
    /// bound to two actions in one context, or a reserved key.
    pub fn with_overrides(overrides: &BTreeMap<String, Vec<String>>) -> Result<Self, String> {
        if let Some(id) = overrides
            .keys()
            .find(|id| !ACTIONS.iter().any(|action| action.id == id.as_str()))
        {
            return Err(format!("unknown action `{id}`"));
        }
        if overrides.contains_key("submit") {
            return Err("`submit` cannot be rebound".to_owned());
        }
        let reserved = Key {
            code: KeyCode::Char('c'),
            mods: KeyModifiers::CONTROL,
        };
        let mut keymap = Self {
            map: HashMap::new(),
            keys: HashMap::new(),
        };
        for info in ACTIONS {
            let names: Vec<&str> = overrides.get(info.id).map_or_else(
                || info.defaults.to_vec(),
                |names| names.iter().map(String::as_str).collect(),
            );
            let mut bound = Vec::new();
            for name in names {
                let key = parse_key(name)
                    .map_err(|_| format!("cannot read key `{name}` for `{}`", info.id))?;
                if key == reserved {
                    return Err("ctrl+c cannot be bound".to_owned());
                }
                if bound.contains(&key) {
                    continue;
                }
                keymap.check(info, key)?;
                keymap.map.insert((info.context, key), info.id);
                bound.push(key);
            }
            keymap.keys.insert(info.id, bound);
        }
        Ok(keymap)
    }

    /// Refuses a key another action already holds where both would be read.
    fn check(&self, info: &ActionInfo, key: Key) -> Result<(), String> {
        let shared = match info.context {
            Context::Editor => Some(Context::Turn),
            Context::Turn => Some(Context::Editor),
            _ => None,
        };
        let taken = |context: Context| self.map.get(&(context, key)).copied();
        let clash = taken(info.context).or_else(|| {
            shared.and_then(taken).filter(|other| {
                let name = key.name();
                !SHARED.iter().any(|(shared_key, turn, editor)| {
                    *shared_key == name
                        && ((info.id == *turn && other == editor)
                            || (info.id == *editor && other == turn))
                })
            })
        });
        match clash {
            Some(other) => Err(format!(
                "`{}` is bound to both `{other}` and `{}` in the {} keys",
                key.name(),
                info.id,
                info.context.as_str()
            )),
            None => Ok(()),
        }
    }

    /// Returns the action a key means in a context, if any.
    #[must_use]
    pub fn action(&self, context: Context, key: &KeyEvent) -> Option<&'static str> {
        let contexts = [Some(context), context.fallback()];
        Key::from_event(key).candidates().find_map(|candidate| {
            contexts
                .iter()
                .flatten()
                .find_map(|context| self.map.get(&(*context, candidate)).copied())
        })
    }

    /// Returns the names of the keys bound to an action, in the order given.
    #[must_use]
    pub fn keys(&self, id: &str) -> Vec<String> {
        self.keys
            .get(id)
            .map(|keys| keys.iter().map(Key::name).collect())
            .unwrap_or_default()
    }
}

impl Default for Keymap {
    fn default() -> Self {
        Self::defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overrides(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(id, keys)| {
                (
                    (*id).to_owned(),
                    keys.iter().map(|key| (*key).to_owned()).collect(),
                )
            })
            .collect()
    }

    fn event(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn every_default_parses() {
        for info in ACTIONS {
            assert!(!info.summary.is_empty(), "{} has no summary", info.id);
            for name in info.defaults {
                let key = parse_key(name).unwrap_or_else(|err| panic!("{}: {err}", info.id));
                assert_eq!(&key.name(), name, "{} does not round-trip", info.id);
            }
        }
        assert_eq!(
            parse_key("shift+tab"),
            Ok(Key {
                code: KeyCode::BackTab,
                mods: KeyModifiers::NONE
            })
        );
        assert_eq!(
            parse_key("ctrl+j"),
            Ok(Key {
                code: KeyCode::Char('j'),
                mods: KeyModifiers::CONTROL
            })
        );
        assert_eq!(
            parse_key("alt+shift+n").map(|key| key.name()),
            Ok("alt+shift+n".to_owned())
        );
        assert_eq!(parse_key("f12").map(|key| key.code), Ok(KeyCode::F(12)));
        assert_eq!(
            parse_key("space").map(|key| key.code),
            Ok(KeyCode::Char(' '))
        );
        for bad in [
            "Ctrl+J", "ctrl+", "f13", "f0", "hyper+x", "shift+1", "", "N",
        ] {
            assert!(parse_key(bad).is_err(), "{bad} parsed");
        }
    }

    #[test]
    fn the_default_table_has_no_conflicts() {
        let keymap = Keymap::with_overrides(&BTreeMap::new()).expect("defaults build");
        let mut ids: Vec<&str> = ACTIONS.iter().map(|info| info.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), ACTIONS.len(), "an id is listed twice");
        for info in ACTIONS {
            assert_eq!(keymap.keys(info.id), info.defaults, "{}", info.id);
        }
        let tab = event(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(keymap.action(Context::Editor, &tab), Some("complete"));
        assert_eq!(keymap.action(Context::Turn, &tab), Some("queue"));
        let esc = event(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(keymap.action(Context::Editor, &esc), Some("clear"));
        assert_eq!(keymap.action(Context::Turn, &esc), Some("interrupt"));
        assert_eq!(keymap.action(Context::Viewer, &esc), Some("viewer_close"));
        let control_enter = event(KeyCode::Enter, KeyModifiers::CONTROL);
        assert_eq!(
            keymap.action(Context::Editor, &control_enter),
            Some("submit")
        );
        let alt_q = event(KeyCode::Char('q'), KeyModifiers::ALT);
        assert_eq!(keymap.action(Context::Editor, &alt_q), None);
    }

    #[test]
    fn an_override_replaces_defaults() {
        let keymap = Keymap::with_overrides(&overrides(&[
            ("queue", &["ctrl+q"]),
            ("newline", &["ctrl+j"]),
        ]))
        .expect("valid");
        assert_eq!(keymap.keys("queue"), ["ctrl+q"]);
        assert_eq!(keymap.keys("newline"), ["ctrl+j"]);
        let tab = event(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(keymap.action(Context::Turn, &tab), Some("complete"));
        let control_q = event(KeyCode::Char('q'), KeyModifiers::CONTROL);
        assert_eq!(keymap.action(Context::Turn, &control_q), Some("queue"));
        let shift_enter = event(KeyCode::Enter, KeyModifiers::SHIFT);
        assert_eq!(keymap.action(Context::Editor, &shift_enter), Some("submit"));

        let unbound = Keymap::with_overrides(&overrides(&[("queue", &[])])).expect("valid");
        assert!(unbound.keys("queue").is_empty());
        assert_eq!(unbound.action(Context::Turn, &tab), Some("complete"));
        assert!(Keymap::with_overrides(&overrides(&[("newline", &["ctrl+enter"])])).is_ok());
        assert_eq!(
            Keymap::with_overrides(&overrides(&[("bogus", &["x"])])).err(),
            Some("unknown action `bogus`".to_owned())
        );
        assert_eq!(
            Keymap::with_overrides(&overrides(&[("newline", &["Ctrl+J"])])).err(),
            Some("cannot read key `Ctrl+J` for `newline`".to_owned())
        );
    }

    #[test]
    fn a_conflict_names_both_actions() {
        assert_eq!(
            Keymap::with_overrides(&overrides(&[("transcript", &["ctrl+r"])])).err(),
            Some(
                "`ctrl+r` is bound to both `history_search` and `transcript` in the editor keys"
                    .to_owned()
            )
        );
        assert_eq!(
            Keymap::with_overrides(&overrides(&[("queue", &["ctrl+g"])])).err(),
            Some(
                "`ctrl+g` is bound to both `queue` and `external_editor` in the editor keys"
                    .to_owned()
            )
        );
        let moved = overrides(&[("transcript", &["ctrl+r"]), ("history_search", &["ctrl+s"])]);
        assert!(Keymap::with_overrides(&moved).is_ok());
    }

    #[test]
    fn submit_and_ctrl_c_are_reserved() {
        assert_eq!(
            Keymap::with_overrides(&overrides(&[("submit", &["ctrl+s"])])).err(),
            Some("`submit` cannot be rebound".to_owned())
        );
        assert_eq!(
            Keymap::with_overrides(&overrides(&[("clear", &["ctrl+c"])])).err(),
            Some("ctrl+c cannot be bound".to_owned())
        );
    }
}
