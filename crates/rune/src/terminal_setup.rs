//! Configures the terminals that need a binding before Shift-Enter can be told
//! apart from Enter.
//!
//! A settings file is only written after the user agrees, a file that cannot
//! be read as plain data is never rewritten, and what cannot be changed from
//! here is printed for the user to add.

use camino::{Utf8Path, Utf8PathBuf};
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_term::probe::{KeyProtocol, TerminalCaps};
use serde_json::{Value, json};

/// The sequence a terminal is asked to send for Shift-Enter.
const SHIFT_ENTER: &str = "\u{1b}[13;2u";

/// The operating system the settings paths belong to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Os {
    Mac,
    Linux,
    Windows,
}

impl Os {
    /// The system this build runs on.
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::Mac
        } else {
            Self::Linux
        }
    }
}

/// What setting up this terminal takes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Setup {
    /// Nothing to change, with the reason.
    NothingNeeded(String),
    /// Lines for the user to add; nothing is written.
    Print { title: String, lines: Vec<String> },
    /// A settings file to write once the user agrees.
    Write {
        path: Utf8PathBuf,
        before: Option<String>,
        after: String,
        /// The lines the write adds, as shown before asking.
        added: Vec<String>,
        summary: String,
    },
    /// A file that is left alone, with what to add by hand.
    Refuse {
        path: Utf8PathBuf,
        reason: String,
        snippet: String,
    },
}

impl Setup {
    /// The lines shown for this setup before anything is written.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        match self {
            Self::NothingNeeded(reason) => vec![reason.clone()],
            Self::Print { title, lines } => std::iter::once(title.clone())
                .chain(lines.iter().map(|line| format!("  {line}")))
                .collect(),
            Self::Write { added, summary, .. } => std::iter::once(summary.clone())
                .chain(added.iter().map(|line| format!("+ {line}")))
                .collect(),
            Self::Refuse {
                reason, snippet, ..
            } => std::iter::once(reason.clone())
                .chain(snippet.lines().map(|line| format!("  {line}")))
                .collect(),
        }
    }
}

/// Works out what this terminal needs. The first rule that matches wins.
#[must_use]
pub fn plan(caps: &TerminalCaps, home: Option<&Utf8Path>, os: Os) -> Setup {
    let env = &caps.env;
    let program = env.term_program.as_deref();
    if caps.tmux {
        return Setup::Print {
            title: "add these lines to ~/.tmux.conf, then run tmux source-file ~/.tmux.conf:"
                .to_owned(),
            lines: [
                "set -s extended-keys on",
                "set -s extended-keys-format csi-u",
                "set -as terminal-features 'xterm*:extkeys'",
                "set -g allow-passthrough on",
                "set -g focus-events on",
            ]
            .map(str::to_owned)
            .to_vec(),
        };
    }
    if os == Os::Windows {
        return Setup::NothingNeeded("Shift-Enter already inserts a newline here".to_owned());
    }
    if caps.key_protocol() == KeyProtocol::Kitty {
        return Setup::NothingNeeded(
            "this terminal reports Shift-Enter; nothing to change".to_owned(),
        );
    }
    if program == Some("vscode") {
        return editor_keybindings(home, os);
    }
    if program == Some("zed") {
        return zed_keymap(home);
    }
    if env.alacritty_window_id.is_some() || env.term.as_deref() == Some("alacritty") {
        return alacritty(home);
    }
    if env.wt_session.is_some() {
        return Setup::Print {
            title: "add this to the \"actions\" list in Windows Terminal's settings.json:"
                .to_owned(),
            lines: vec![
                r#"{"command":{"action":"sendInput","input":"\u001b[13;2u"},"keys":"shift+enter"}"#
                    .to_owned(),
            ],
        };
    }
    if program == Some("Apple_Terminal") {
        return Setup::Print {
            title: "Terminal.app cannot report Shift-Enter; use one of:".to_owned(),
            lines: [
                "ctrl-j",
                "\\ then enter",
                "option-enter after Settings > Profiles > Keyboard > Use Option as Meta Key",
            ]
            .map(str::to_owned)
            .to_vec(),
        };
    }
    Setup::NothingNeeded(
        "use ctrl-j or \\ then enter for a newline; no setting is known for this terminal"
            .to_owned(),
    )
}

/// Writes a planned file, keeping a copy of the one it replaces.
///
/// # Errors
///
/// Fails when the setup writes nothing, or when the backup or the file cannot
/// be written.
pub fn apply(setup: &Setup) -> Result<String> {
    let Setup::Write {
        path,
        before,
        after,
        ..
    } = setup
    else {
        return Err(RuneError::new(ErrorCode::Internal, "nothing to write"));
    };
    let failed = |what: &Utf8Path, err: std::io::Error| {
        RuneError::new(
            ErrorCode::IoFailure,
            format!("could not write {what}: {err}"),
        )
    };
    if before.is_some() {
        let backup = Utf8PathBuf::from(format!("{path}.rune-backup"));
        std::fs::copy(path, &backup).map_err(|err| failed(&backup, err))?;
    } else if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| failed(parent, err))?;
    }
    std::fs::write(path, after).map_err(|err| failed(path, err))?;
    Ok(format!(
        "wrote {path}; restart the terminal or reload its settings"
    ))
}

/// An editor key binding that sends the Shift-Enter sequence to its terminal.
fn editor_keybindings(home: Option<&Utf8Path>, os: Os) -> Setup {
    let entry = json!({
        "key": "shift+enter",
        "command": "workbench.action.terminal.sendSequence",
        "args": {"text": SHIFT_ENTER},
        "when": "terminalFocus",
    });
    let Some(home) = home else {
        return no_home("keybindings.json", &entry);
    };
    let config = if os == Os::Mac {
        home.join("Library/Application Support")
    } else {
        home.join(".config")
    };
    let Some(folder) = ["Code/User", "Cursor/User", "Code - Insiders/User"]
        .iter()
        .map(|name| config.join(name))
        .find(|folder| folder.is_dir())
    else {
        return Setup::Refuse {
            path: config.join("Code/User/keybindings.json"),
            reason: format!(
                "no editor settings folder was found in {config}; add this entry to keybindings.json yourself:"
            ),
            snippet: entry.to_string(),
        };
    };
    json_list(&folder.join("keybindings.json"), entry, |item| {
        item.get("key").and_then(Value::as_str) == Some("shift+enter")
            && item.get("when").and_then(Value::as_str) == Some("terminalFocus")
    })
}

/// A keymap binding in a `Terminal` context.
fn zed_keymap(home: Option<&Utf8Path>) -> Setup {
    let entry = json!({
        "context": "Terminal",
        "bindings": {"shift-enter": ["terminal::SendText", SHIFT_ENTER]},
    });
    let Some(home) = home else {
        return no_home("~/.config/zed/keymap.json", &entry);
    };
    json_list(&home.join(".config/zed/keymap.json"), entry, |item| {
        item.get("context").and_then(Value::as_str) == Some("Terminal")
            && item
                .get("bindings")
                .and_then(Value::as_object)
                .is_some_and(|bindings| bindings.contains_key("shift-enter"))
    })
}

/// Appends `entry` to the JSON list in `path`, unless one already matches.
fn json_list(path: &Utf8Path, entry: Value, configured: impl Fn(&Value) -> bool) -> Setup {
    let refuse = |reason: String| Setup::Refuse {
        path: path.to_owned(),
        reason,
        snippet: entry.to_string(),
    };
    let before = match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            return refuse(format!(
                "could not read {path}: {err}; add this entry yourself:"
            ));
        }
    };
    let mut list: Vec<Value> = match before.as_deref().map(str::trim) {
        None | Some("") => Vec::new(),
        Some(text) => match serde_json::from_str(text) {
            Ok(list) => list,
            Err(_) => {
                return refuse(format!(
                    "{path} could not be read as plain JSON; add this entry yourself:"
                ));
            }
        },
    };
    if list.iter().any(configured) {
        return Setup::NothingNeeded(format!("already configured in {path}"));
    }
    let added = serde_json::to_string_pretty(&entry)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    list.push(entry);
    let mut after = serde_json::to_string_pretty(&list).unwrap_or_default();
    after.push('\n');
    Setup::Write {
        path: path.to_owned(),
        before,
        after,
        added,
        summary: format!("add a shift-enter binding to {path}:"),
    }
}

/// A key binding block, appended as text so the rest of the file is kept as
/// written.
fn alacritty(home: Option<&Utf8Path>) -> Setup {
    const BLOCK: &str =
        "[[keyboard.bindings]]\nkey = \"Enter\"\nmods = \"Shift\"\nchars = \"\\u001b[13;2u\"\n";
    let Some(home) = home else {
        return Setup::Refuse {
            path: Utf8PathBuf::from("~/.config/alacritty/alacritty.toml"),
            reason: "no home directory".to_owned(),
            snippet: BLOCK.to_owned(),
        };
    };
    let path = home.join(".config/alacritty/alacritty.toml");
    let before = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            return Setup::Refuse {
                reason: format!("could not read {path}: {err}; add this block yourself:"),
                path,
                snippet: BLOCK.to_owned(),
            };
        }
    };
    let text = before.as_deref().unwrap_or_default();
    if text.contains("mods = \"Shift\"") && text.contains("13;2u") {
        return Setup::NothingNeeded(format!("already configured in {path}"));
    }
    let mut after = text.to_owned();
    if !after.is_empty() {
        if !after.ends_with('\n') {
            after.push('\n');
        }
        after.push('\n');
    }
    after.push_str(BLOCK);
    Setup::Write {
        summary: format!("add a shift-enter binding to {path}:"),
        path,
        before,
        after,
        added: BLOCK.lines().map(str::to_owned).collect(),
    }
}

fn no_home(file: &str, entry: &Value) -> Setup {
    Setup::Refuse {
        path: Utf8PathBuf::from(file),
        reason: "no home directory".to_owned(),
        snippet: entry.to_string(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)]

    use super::*;
    use rune_term::probe::TerminalEnv;

    fn temp_home() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("temp home");
        let home = Utf8PathBuf::from_path_buf(dir.path().to_owned()).expect("utf8");
        (dir, home)
    }

    fn caps(env: TerminalEnv) -> TerminalCaps {
        TerminalCaps::from_env(env)
    }

    fn program(name: &str) -> TerminalCaps {
        caps(TerminalEnv {
            term_program: Some(name.to_owned()),
            ..TerminalEnv::default()
        })
    }

    #[test]
    fn vscode_entry_is_appended_and_detected_next_time() {
        let (_dir, home) = temp_home();
        let folder = home.join(".config/Code/User");
        std::fs::create_dir_all(&folder).expect("settings folder");
        let path = folder.join("keybindings.json");
        std::fs::write(&path, r#"[{"key":"ctrl+k","command":"other"}]"#).expect("existing");
        let setup = plan(&program("vscode"), Some(&home), Os::Linux);
        let Setup::Write {
            path: target,
            before,
            after,
            ..
        } = &setup
        else {
            panic!("{setup:?}");
        };
        assert_eq!(target, &path);
        assert!(before.is_some());
        assert!(
            setup
                .lines()
                .iter()
                .any(|line| line == r#"+   "key": "shift+enter","#),
            "{:?}",
            setup.lines()
        );
        let written: Vec<Value> = serde_json::from_str(after).expect("json");
        assert_eq!(written.len(), 2);
        assert_eq!(written[0]["key"], "ctrl+k");
        assert_eq!(written[1]["args"]["text"], SHIFT_ENTER);
        assert!(after.contains("\n  {\n    \"key\": \"ctrl+k\""), "{after}");
        assert_eq!(
            apply(&setup).expect("written"),
            format!("wrote {path}; restart the terminal or reload its settings")
        );
        assert_eq!(
            std::fs::read_to_string(format!("{path}.rune-backup")).expect("backup"),
            r#"[{"key":"ctrl+k","command":"other"}]"#
        );
        assert_eq!(
            plan(&program("vscode"), Some(&home), Os::Linux),
            Setup::NothingNeeded(format!("already configured in {path}"))
        );
    }

    #[test]
    fn json_with_comments_is_refused_with_the_snippet() {
        let (_dir, home) = temp_home();
        let folder = home.join("Library/Application Support/Cursor/User");
        std::fs::create_dir_all(&folder).expect("settings folder");
        let path = folder.join("keybindings.json");
        let text = "// Place your key bindings in this file\n[]\n";
        std::fs::write(&path, text).expect("existing");
        let setup = plan(&program("vscode"), Some(&home), Os::Mac);
        assert_eq!(
            setup,
            Setup::Refuse {
                path: path.clone(),
                reason: format!(
                    "{path} could not be read as plain JSON; add this entry yourself:"
                ),
                snippet: r#"{"key":"shift+enter","command":"workbench.action.terminal.sendSequence","args":{"text":"\u001b[13;2u"},"when":"terminalFocus"}"#.to_owned(),
            }
        );
        assert_eq!(std::fs::read_to_string(&path).expect("unchanged"), text);
        assert!(apply(&setup).is_err());
    }

    #[test]
    fn tmux_prints_the_five_lines() {
        // tmux inside kitty is configured as tmux.
        let mut inside = caps(TerminalEnv {
            tmux: Some("/tmp/x,1,0".to_owned()),
            term_program: Some("vscode".to_owned()),
            ..TerminalEnv::default()
        });
        inside.kitty_keyboard = Some(1);
        inside.replied = true;
        let Setup::Print { title, lines } = plan(&inside, None, Os::Linux) else {
            panic!("tmux prints");
        };
        assert_eq!(
            title,
            "add these lines to ~/.tmux.conf, then run tmux source-file ~/.tmux.conf:"
        );
        assert_eq!(
            lines,
            [
                "set -s extended-keys on",
                "set -s extended-keys-format csi-u",
                "set -as terminal-features 'xterm*:extkeys'",
                "set -g allow-passthrough on",
                "set -g focus-events on",
            ]
        );
    }

    #[test]
    fn kitty_needs_nothing() {
        let mut kitty = program("vscode");
        kitty.kitty_keyboard = Some(0);
        kitty.replied = true;
        let expected = if cfg!(windows) {
            "Shift-Enter already inserts a newline here"
        } else {
            "this terminal reports Shift-Enter; nothing to change"
        };
        assert_eq!(
            plan(&kitty, None, Os::current()),
            Setup::NothingNeeded(expected.to_owned())
        );
        assert_eq!(
            plan(&program("vscode"), None, Os::Windows),
            Setup::NothingNeeded("Shift-Enter already inserts a newline here".to_owned())
        );
        assert_eq!(
            plan(&program("Unknown"), None, Os::Linux),
            Setup::NothingNeeded(
                "use ctrl-j or \\ then enter for a newline; no setting is known for this terminal"
                    .to_owned()
            )
        );
        let Setup::Print { title, .. } = plan(&program("Apple_Terminal"), None, Os::Mac) else {
            panic!("Terminal.app prints");
        };
        assert_eq!(title, "Terminal.app cannot report Shift-Enter; use one of:");
    }

    #[test]
    fn alacritty_block_is_appended_once() {
        let (_dir, home) = temp_home();
        let alacritty = caps(TerminalEnv {
            alacritty_window_id: Some("1".to_owned()),
            ..TerminalEnv::default()
        });
        let folder = home.join(".config/alacritty");
        std::fs::create_dir_all(&folder).expect("config folder");
        let path = folder.join("alacritty.toml");
        std::fs::write(&path, "[font]\nsize = 12").expect("existing");
        let setup = plan(&alacritty, Some(&home), Os::Linux);
        apply(&setup).expect("written");
        let text = std::fs::read_to_string(&path).expect("written");
        assert_eq!(
            text,
            "[font]\nsize = 12\n\n[[keyboard.bindings]]\nkey = \"Enter\"\nmods = \"Shift\"\nchars = \"\\u001b[13;2u\"\n"
        );
        assert!(text.parse::<toml::Table>().is_ok());
        let by_term = caps(TerminalEnv {
            term: Some("alacritty".to_owned()),
            ..TerminalEnv::default()
        });
        assert_eq!(
            plan(&by_term, Some(&home), Os::Linux),
            Setup::NothingNeeded(format!("already configured in {path}"))
        );
        // A missing file is created with the block alone.
        let (_other, empty) = temp_home();
        let Setup::Write { before, after, .. } = plan(&alacritty, Some(&empty), Os::Linux) else {
            panic!("a write");
        };
        assert!(before.is_none());
        assert!(after.starts_with("[[keyboard.bindings]]\n"));
        assert_eq!(
            plan(&alacritty, None, Os::Linux),
            Setup::Refuse {
                path: Utf8PathBuf::from("~/.config/alacritty/alacritty.toml"),
                reason: "no home directory".to_owned(),
                snippet: "[[keyboard.bindings]]\nkey = \"Enter\"\nmods = \"Shift\"\nchars = \"\\u001b[13;2u\"\n".to_owned(),
            }
        );
    }

    #[test]
    fn zed_keymap_is_appended_and_detected_next_time() {
        let (_dir, home) = temp_home();
        let setup = plan(&program("zed"), Some(&home), Os::Mac);
        apply(&setup).expect("written");
        let path = home.join(".config/zed/keymap.json");
        let written: Vec<Value> =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("keymap")).expect("json");
        assert_eq!(
            written[0]["bindings"]["shift-enter"],
            json!(["terminal::SendText", SHIFT_ENTER])
        );
        assert!(!Utf8PathBuf::from(format!("{path}.rune-backup")).exists());
        assert_eq!(
            plan(&program("zed"), Some(&home), Os::Mac),
            Setup::NothingNeeded(format!("already configured in {path}"))
        );
    }
}
