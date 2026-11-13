//! The terminal modes Rune turns on, recorded so that every way out of the
//! process, and every pause and resume, puts the terminal back exactly as it
//! was found.

use std::io::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// Ends a synchronized update.
const END_SYNC: &[u8] = b"\x1b[?2026l";
/// Resets colours and text attributes.
const RESET_STYLE: &[u8] = b"\x1b[0m";
/// Closes any open hyperlink.
const CLOSE_LINK: &[u8] = b"\x1b]8;;\x1b\\";
/// Stops focus reports.
const FOCUS_OFF: &[u8] = crate::probe::FOCUS_OFF.as_bytes();
/// Starts focus reports.
const FOCUS_ON: &[u8] = crate::probe::FOCUS_ON.as_bytes();
/// Stops bracketed paste.
const PASTE_OFF: &[u8] = b"\x1b[?2004l";
/// Starts bracketed paste.
const PASTE_ON: &[u8] = b"\x1b[?2004h";
/// Pops the keyboard enhancement flags.
const KEYBOARD_POP: &[u8] = crate::probe::KITTY_POP.as_bytes();
/// Turns modifyOtherKeys off.
const MODIFY_OTHER_KEYS_OFF: &[u8] = crate::probe::MODIFY_OTHER_KEYS_RESET.as_bytes();
/// Puts the modified key format back to its default.
const FORMAT_OTHER_KEYS_OFF: &[u8] = crate::probe::FORMAT_OTHER_KEYS_RESET.as_bytes();
/// Turns line wrapping back on.
const WRAP_ON: &[u8] = b"\x1b[?7h";
/// Clears a progress indicator.
const PROGRESS_OFF: &[u8] = b"\x1b]9;4;0;0\x07";
/// Restores the window title pushed at start.
const TITLE_POP: &[u8] = b"\x1b[23;2t";
/// Leaves the alternate screen.
const ALTERNATE_OFF: &[u8] = b"\x1b[?1049l";
/// Enters the alternate screen.
const ALTERNATE_ON: &[u8] = b"\x1b[?1049h";
/// Shows the cursor.
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";

/// Which modes are on now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct ModeState {
    /// Raw input.
    pub raw: bool,
    /// Bracketed paste.
    pub bracketed_paste: bool,
    /// Keyboard enhancement flags were pushed.
    pub keyboard_flags: bool,
    /// modifyOtherKeys was turned on: mode 2 with [`ModeState::format_other_keys`],
    /// else mode 1.
    pub modify_other_keys: bool,
    /// Modified keys were asked for in the `CSI u` format.
    pub format_other_keys: bool,
    /// Focus reports were turned on.
    pub focus: bool,
    /// The window title was pushed.
    pub title_pushed: bool,
    /// A progress indicator was sent.
    pub progress_sent: bool,
    /// The alternate screen is shown.
    pub alternate_screen: bool,
}

const NONE: ModeState = ModeState {
    raw: false,
    bracketed_paste: false,
    keyboard_flags: false,
    modify_other_keys: false,
    format_other_keys: false,
    focus: false,
    title_pushed: false,
    progress_sent: false,
    alternate_screen: false,
};

static MODES: Mutex<ModeState> = Mutex::new(NONE);
/// The modes in force when the process was paused, to turn back on, and
/// whether the pause left the alternate screen.
static PAUSED: Mutex<Option<(ModeState, bool)>> = Mutex::new(None);
/// Set when the process was resumed and the screen must be drawn again.
static REDRAW: AtomicBool = AtomicBool::new(false);

fn modes() -> std::sync::MutexGuard<'static, ModeState> {
    MODES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Records a mode change.
pub fn mark(change: impl FnOnce(&mut ModeState)) {
    change(&mut modes());
}

/// Returns the modes on now.
#[must_use]
pub fn current() -> ModeState {
    *modes()
}

/// Returns the bytes that turn off every mode `state` marks, and reset what
/// any frame may have left set, in the order the terminal needs them.
#[must_use]
pub fn restore_bytes(state: &ModeState) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(END_SYNC);
    out.extend_from_slice(RESET_STYLE);
    out.extend_from_slice(CLOSE_LINK);
    let marked: [(bool, &[u8]); 5] = [
        (state.focus, FOCUS_OFF),
        (state.bracketed_paste, PASTE_OFF),
        (state.keyboard_flags, KEYBOARD_POP),
        (state.modify_other_keys, MODIFY_OTHER_KEYS_OFF),
        (state.format_other_keys, FORMAT_OTHER_KEYS_OFF),
    ];
    for (on, bytes) in marked {
        if on {
            out.extend_from_slice(bytes);
        }
    }
    out.extend_from_slice(WRAP_ON);
    let marked: [(bool, &[u8]); 3] = [
        (state.progress_sent, PROGRESS_OFF),
        (state.title_pushed, TITLE_POP),
        (state.alternate_screen, ALTERNATE_OFF),
    ];
    for (on, bytes) in marked {
        if on {
            out.extend_from_slice(bytes);
        }
    }
    out.extend_from_slice(SHOW_CURSOR);
    out
}

/// Returns the bytes that turn on the keyboard reporting `state` marks.
fn keyboard_bytes(state: &ModeState) -> Vec<u8> {
    use crate::probe::{
        FORMAT_OTHER_KEYS_CSI_U, KITTY_PUSH_DISAMBIGUATE, MODIFY_OTHER_KEYS_1, MODIFY_OTHER_KEYS_2,
    };
    let mut out = Vec::new();
    if state.keyboard_flags {
        out.extend_from_slice(KITTY_PUSH_DISAMBIGUATE.as_bytes());
    }
    if state.format_other_keys {
        out.extend_from_slice(FORMAT_OTHER_KEYS_CSI_U.as_bytes());
    }
    if state.modify_other_keys {
        let mode = if state.format_other_keys {
            MODIFY_OTHER_KEYS_2
        } else {
            MODIFY_OTHER_KEYS_1
        };
        out.extend_from_slice(mode.as_bytes());
    }
    out
}

/// Asks the terminal to report modified keys the way `protocol` names, and
/// records it, so every way out turns it off again. Nothing is written for
/// [`KeyProtocol::Legacy`].
pub fn push_keyboard(protocol: crate::probe::KeyProtocol) {
    use crate::probe::KeyProtocol;
    let mut state = ModeState::default();
    match protocol {
        KeyProtocol::Kitty => state.keyboard_flags = true,
        KeyProtocol::ModifyOtherKeysTmux => state.modify_other_keys = true,
        KeyProtocol::ModifyOtherKeysXterm => {
            state.modify_other_keys = true;
            state.format_other_keys = true;
        }
        KeyProtocol::Legacy => return,
    }
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(&keyboard_bytes(&state));
    let _ = stdout.flush();
    mark(|modes| {
        modes.keyboard_flags = state.keyboard_flags;
        modes.modify_other_keys = state.modify_other_keys;
        modes.format_other_keys = state.format_other_keys;
    });
}

/// Turns on virtual terminal processing for the console's output, and says
/// whether the console accepts terminal sequences. A legacy console that does
/// not would print every sequence as text.
#[cfg(windows)]
#[must_use]
pub fn enable_virtual_terminal() -> bool {
    crossterm::ansi_support::supports_ansi()
}

/// Asks the terminal to report focus changes, and records it, so every way
/// out turns the reports off again.
pub fn enable_focus() {
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(FOCUS_ON);
    let _ = stdout.flush();
    mark(|modes| modes.focus = true);
}

/// Writes `state`'s restore bytes in one write and leaves raw mode. A failed
/// write is ignored: after a hang-up the terminal is gone.
fn put_back(state: &ModeState) {
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(&restore_bytes(state));
    let _ = stdout.flush();
    if state.raw {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Puts the terminal back as it was found and forgets every mode.
pub fn restore() {
    let state = std::mem::replace(&mut *modes(), NONE);
    put_back(&state);
}

/// Puts the terminal back for a pause, keeping the modes so [`reapply`] can
/// turn them on again.
pub fn pause() {
    pause_keeping(false);
}

/// Puts the terminal back for a child that takes it over, such as an editor.
/// The alternate screen is kept: the child draws on the screen prepared for it.
pub fn pause_for_child() {
    pause_keeping(true);
}

fn pause_keeping(keep_alternate: bool) {
    let state = std::mem::replace(&mut *modes(), NONE);
    let left_alternate = state.alternate_screen && !keep_alternate;
    *PAUSED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((state, left_alternate));
    put_back(&ModeState {
        alternate_screen: left_alternate,
        ..state
    });
}

/// Turns back on the modes in force before [`pause`], or keeps the current
/// ones on. Returns whether anything was turned on.
pub fn reapply() -> bool {
    let paused = PAUSED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let resumed = paused.is_some();
    let (state, left_alternate) = paused.unwrap_or_else(|| (current(), false));
    if state.raw {
        let _ = crossterm::terminal::enable_raw_mode();
    }
    let out = reapply_bytes(&state, resumed, left_alternate);
    if !out.is_empty() {
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(&out);
        let _ = stdout.flush();
    }
    *modes() = state;
    state != NONE
}

/// Returns the bytes that turn `state`'s modes on again, after a pause when
/// `resumed`, entering the alternate screen again when the pause left it.
fn reapply_bytes(state: &ModeState, resumed: bool, left_alternate: bool) -> Vec<u8> {
    let mut out = Vec::new();
    if state.bracketed_paste {
        out.extend_from_slice(PASTE_ON);
    }
    // Kitty flags are a stack, so they are pushed again only after a pause
    // popped them.
    if resumed {
        out.extend_from_slice(&keyboard_bytes(state));
    }
    if state.focus {
        out.extend_from_slice(FOCUS_ON);
    }
    if left_alternate {
        out.extend_from_slice(ALTERNATE_ON);
    }
    out
}

/// Asks for the whole screen to be drawn again.
pub fn request_redraw() {
    REDRAW.store(true, Ordering::SeqCst);
}

/// Returns whether a redraw is waiting, without taking it.
#[must_use]
pub fn redraw_pending() -> bool {
    REDRAW.load(Ordering::SeqCst)
}

/// Takes a waiting redraw request.
#[must_use]
pub fn take_redraw() -> bool {
    REDRAW.swap(false, Ordering::SeqCst)
}

/// Pauses the process the way the terminal's suspend key does: the terminal is
/// put back first, and drawn again on resume.
pub fn suspend() {
    #[cfg(unix)]
    {
        let _ = signal_hook::low_level::raise(signal_hook::consts::SIGTSTP);
    }
}

/// Starts the thread that puts the terminal back when the process is ended,
/// hung up or paused, and turns it on again when resumed. Installed once.
pub fn install_signal_handlers() {
    #[cfg(unix)]
    {
        use signal_hook::consts::{SIGCONT, SIGHUP, SIGINT, SIGTERM, SIGTSTP};
        static INSTALLED: std::sync::Once = std::sync::Once::new();
        INSTALLED.call_once(|| {
            let Ok(mut signals) =
                signal_hook::iterator::Signals::new([SIGINT, SIGTERM, SIGHUP, SIGTSTP, SIGCONT])
            else {
                return;
            };
            let _ = std::thread::Builder::new()
                .name("terminal-signals".to_owned())
                .spawn(move || {
                    for signal in signals.forever() {
                        match signal {
                            SIGTSTP => {
                                let mut stdout = std::io::stdout();
                                let _ = stdout.write_all(b"\r\n");
                                pause();
                                let _ = signal_hook::low_level::emulate_default_handler(SIGTSTP);
                            }
                            SIGCONT => {
                                reapply();
                                request_redraw();
                            }
                            other => {
                                restore();
                                std::process::exit(128_i32.saturating_add(other));
                            }
                        }
                    }
                });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    #[test]
    fn restore_bytes_follow_the_documented_order() {
        let all = ModeState {
            raw: true,
            bracketed_paste: true,
            keyboard_flags: true,
            modify_other_keys: true,
            format_other_keys: true,
            focus: true,
            title_pushed: true,
            progress_sent: true,
            alternate_screen: true,
        };
        let bytes = restore_bytes(&all);
        let order = [
            END_SYNC,
            RESET_STYLE,
            CLOSE_LINK,
            FOCUS_OFF,
            PASTE_OFF,
            KEYBOARD_POP,
            MODIFY_OTHER_KEYS_OFF,
            FORMAT_OTHER_KEYS_OFF,
            WRAP_ON,
            PROGRESS_OFF,
            TITLE_POP,
            ALTERNATE_OFF,
            SHOW_CURSOR,
        ];
        let positions: Vec<usize> = order
            .iter()
            .map(|part| find(&bytes, part).expect("every part is written"))
            .collect();
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "{positions:?}"
        );
        assert!(bytes.ends_with(SHOW_CURSOR));
        assert_eq!(
            bytes.len(),
            order.iter().map(|part| part.len()).sum::<usize>()
        );
    }

    #[test]
    fn unmarked_modes_are_not_reset() {
        let bytes = restore_bytes(&ModeState {
            raw: true,
            bracketed_paste: true,
            ..ModeState::default()
        });
        assert!(find(&bytes, PASTE_OFF).is_some());
        for part in [
            KEYBOARD_POP,
            FOCUS_OFF,
            MODIFY_OTHER_KEYS_OFF,
            FORMAT_OTHER_KEYS_OFF,
            TITLE_POP,
            ALTERNATE_OFF,
            PROGRESS_OFF,
        ] {
            assert!(find(&bytes, part).is_none(), "{part:?}");
        }
        for part in [END_SYNC, RESET_STYLE, CLOSE_LINK, WRAP_ON, SHOW_CURSOR] {
            assert!(find(&bytes, part).is_some(), "{part:?}");
        }
    }

    #[test]
    fn focus_reports_are_turned_on_again_after_a_pause() {
        let focused = ModeState {
            raw: true,
            focus: true,
            ..ModeState::default()
        };
        assert!(find(&reapply_bytes(&focused, true, false), FOCUS_ON).is_some());
        assert!(find(&reapply_bytes(&focused, false, false), FOCUS_ON).is_some());
        let unfocused = ModeState {
            raw: true,
            ..ModeState::default()
        };
        assert!(find(&reapply_bytes(&unfocused, true, false), FOCUS_ON).is_none());
        assert!(find(&restore_bytes(&focused), FOCUS_OFF).is_some());
    }

    #[test]
    fn each_keyboard_protocol_is_undone_by_its_own_inverse() {
        let kitty = ModeState {
            keyboard_flags: true,
            ..ModeState::default()
        };
        assert_eq!(keyboard_bytes(&kitty), b"\x1b[>1u");
        let bytes = restore_bytes(&kitty);
        assert!(find(&bytes, KEYBOARD_POP).is_some());
        assert!(find(&bytes, MODIFY_OTHER_KEYS_OFF).is_none());

        let tmux = ModeState {
            modify_other_keys: true,
            ..ModeState::default()
        };
        assert_eq!(keyboard_bytes(&tmux), b"\x1b[>4;1m");
        let bytes = restore_bytes(&tmux);
        assert!(find(&bytes, MODIFY_OTHER_KEYS_OFF).is_some());
        assert!(find(&bytes, KEYBOARD_POP).is_none());
        assert!(find(&bytes, FORMAT_OTHER_KEYS_OFF).is_none());

        let xterm = ModeState {
            modify_other_keys: true,
            format_other_keys: true,
            ..ModeState::default()
        };
        assert_eq!(keyboard_bytes(&xterm), b"\x1b[>4;1f\x1b[>4;2m");
        let bytes = restore_bytes(&xterm);
        let reset = find(&bytes, MODIFY_OTHER_KEYS_OFF).expect("modifyOtherKeys reset");
        let format = find(&bytes, FORMAT_OTHER_KEYS_OFF).expect("format reset");
        assert!(reset < format);

        assert!(keyboard_bytes(&ModeState::default()).is_empty());
    }
}
