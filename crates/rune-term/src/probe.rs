//! One startup round trip that asks the terminal what it supports.
//!
//! The queries go out in one write and every reply is read before the key
//! reader starts, so anything typed while the replies arrive is kept and handed
//! to the draft instead of being lost.

use std::time::Duration;

/// Asks for the kitty keyboard flags in force.
pub const KITTY_FLAGS_QUERY: &str = "\u{1b}[?u";
/// Asks for the default background colour.
pub const BACKGROUND_QUERY: &str = "\u{1b}]11;?\u{1b}\\";
/// Asks whether synchronized output (mode 2026) is recognised.
pub const SYNC_QUERY: &str = "\u{1b}[?2026$p";
/// Asks for the terminal's name and version.
pub const VERSION_QUERY: &str = "\u{1b}[>q";
/// Asks for the primary device attributes, which every terminal answers.
pub const DEVICE_ATTRIBUTES_QUERY: &str = "\u{1b}[c";

/// Pushes kitty keyboard flag 1, which reports modified keys unambiguously.
pub const KITTY_PUSH_DISAMBIGUATE: &str = "\u{1b}[>1u";
/// Pops the kitty keyboard flags pushed last.
pub const KITTY_POP: &str = "\u{1b}[<u";
/// Turns on modifyOtherKeys mode 1, which leaves Ctrl+C as a control byte.
pub const MODIFY_OTHER_KEYS_1: &str = "\u{1b}[>4;1m";
/// Turns on modifyOtherKeys mode 2, which reports every modified key.
pub const MODIFY_OTHER_KEYS_2: &str = "\u{1b}[>4;2m";
/// Turns modifyOtherKeys off.
pub const MODIFY_OTHER_KEYS_RESET: &str = "\u{1b}[>4m";
/// Asks for modified keys in the `CSI code ; modifiers u` form.
pub const FORMAT_OTHER_KEYS_CSI_U: &str = "\u{1b}[>4;1f";
/// Puts the modified key format back to its default.
pub const FORMAT_OTHER_KEYS_RESET: &str = "\u{1b}[>4f";
/// Asks the terminal to report when its window gains or loses focus.
pub const FOCUS_ON: &str = "\u{1b}[?1004h";
/// Stops focus reports.
pub const FOCUS_OFF: &str = "\u{1b}[?1004l";

/// How long a terminal that has said nothing is waited for.
const FIRST_REPLY: Duration = Duration::from_millis(100);
/// How long a terminal that has started answering is waited for.
const LAST_REPLY: Duration = Duration::from_millis(1_000);
/// Longest unfinished reply kept between reads.
const MAX_REPLY: usize = 512;
/// Longest terminal name kept.
const MAX_NAME: usize = 64;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// A capability that may be present, absent, or not reported.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Tri {
    Yes,
    No,
    #[default]
    Unknown,
}

/// The environment variables that describe the terminal.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct TerminalEnv {
    pub term: Option<String>,
    pub term_program: Option<String>,
    pub colorterm: Option<String>,
    pub tmux: Option<String>,
    pub sty: Option<String>,
    pub ssh_tty: Option<String>,
    pub ssh_connection: Option<String>,
    pub wt_session: Option<String>,
    pub vte_version: Option<String>,
    pub kitty_window_id: Option<String>,
    pub alacritty_window_id: Option<String>,
    pub wezterm_executable: Option<String>,
    pub colorfgbg: Option<String>,
    pub lc_terminal: Option<String>,
}

impl TerminalEnv {
    /// Reads the variables through `lookup`, treating an empty value as absent.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let get = |name: &str| lookup(name).filter(|value| !value.is_empty());
        Self {
            term: get("TERM"),
            term_program: get("TERM_PROGRAM"),
            colorterm: get("COLORTERM"),
            tmux: get("TMUX"),
            sty: get("STY"),
            ssh_tty: get("SSH_TTY"),
            ssh_connection: get("SSH_CONNECTION"),
            wt_session: get("WT_SESSION"),
            vte_version: get("VTE_VERSION"),
            kitty_window_id: get("KITTY_WINDOW_ID"),
            alacritty_window_id: get("ALACRITTY_WINDOW_ID"),
            wezterm_executable: get("WEZTERM_EXECUTABLE"),
            colorfgbg: get("COLORFGBG"),
            lc_terminal: get("LC_TERMINAL"),
        }
    }
}

/// How the terminal is asked to report modified keys such as Shift+Enter.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyProtocol {
    /// Kitty keyboard flags.
    Kitty,
    /// modifyOtherKeys mode 1, inside tmux.
    ModifyOtherKeysTmux,
    /// modifyOtherKeys mode 2 in the `CSI u` format, for xterm.
    ModifyOtherKeysXterm,
    /// Nothing is asked for.
    Legacy,
}

/// What the terminal reported about itself, with the environment it ran in.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct TerminalCaps {
    pub kitty_keyboard: Option<u8>,
    pub background: Option<(u8, u8, u8)>,
    pub sync_output: Tri,
    pub name: Option<String>,
    pub tmux: bool,
    pub ssh: bool,
    pub multiplexer_passthrough: bool,
    pub replied: bool,
    pub env: TerminalEnv,
}

impl TerminalCaps {
    /// The capabilities known from the environment alone.
    #[must_use]
    pub fn from_env(env: TerminalEnv) -> Self {
        let tmux = env.tmux.is_some();
        Self {
            tmux,
            ssh: env.ssh_tty.is_some() || env.ssh_connection.is_some(),
            multiplexer_passthrough: !tmux && env.sty.is_none(),
            env,
            ..Self::default()
        }
    }

    /// One line naming what the terminal reported.
    #[must_use]
    pub fn summary(&self) -> String {
        let name = self.name.as_deref().unwrap_or("unknown");
        let background = self.background.map_or_else(
            || "unknown".to_owned(),
            |(r, g, b)| {
                let shade = if is_light((r, g, b)) { "light" } else { "dark" };
                format!("#{r:02x}{g:02x}{b:02x} ({shade})")
            },
        );
        let sync = match self.sync_output {
            Tri::Yes => "yes",
            Tri::No => "no",
            Tri::Unknown => "unknown",
        };
        let kitty = self
            .kitty_keyboard
            .map_or_else(|| "no".to_owned(), |flags| flags.to_string());
        let yes_no = |value: bool| if value { "yes" } else { "no" };
        format!(
            "{name}; background {background}; synchronized output {sync}; kitty keyboard {kitty}; tmux {}; ssh {}",
            yes_no(self.tmux),
            yes_no(self.ssh)
        )
    }

    /// The way modified keys are asked for in this terminal.
    ///
    /// Only a terminal that answered the probe is asked for anything. On
    /// Windows the console's key records already carry Shift.
    #[must_use]
    pub fn key_protocol(&self) -> KeyProtocol {
        if cfg!(windows) {
            return KeyProtocol::Legacy;
        }
        if self.tmux && self.replied {
            KeyProtocol::ModifyOtherKeysTmux
        } else if self.kitty_keyboard.is_some() {
            KeyProtocol::Kitty
        } else if self
            .name
            .as_deref()
            .is_some_and(|name| name.starts_with("XTerm("))
        {
            KeyProtocol::ModifyOtherKeysXterm
        } else {
            KeyProtocol::Legacy
        }
    }

    /// "shift-enter newline" when Shift+Enter can be told apart, else "ctrl-j newline".
    #[must_use]
    pub fn newline_hint(&self) -> &'static str {
        if cfg!(windows) {
            return "shift-enter newline";
        }
        match self.key_protocol() {
            KeyProtocol::Kitty | KeyProtocol::ModifyOtherKeysXterm => "shift-enter newline",
            KeyProtocol::ModifyOtherKeysTmux | KeyProtocol::Legacy => "ctrl-j newline",
        }
    }

    fn has_reply(&self) -> bool {
        self.kitty_keyboard.is_some()
            || self.background.is_some()
            || self.sync_output != Tri::Unknown
            || self.name.is_some()
    }
}

/// Whether a colour's relative luminance is above one half.
#[must_use]
pub fn is_light(rgb: (u8, u8, u8)) -> bool {
    fn linear(channel: u8) -> f64 {
        let value = f64::from(channel) / 255.0;
        if value <= 0.040_45 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    }
    let (r, g, b) = rgb;
    0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b) > 0.5
}

/// The probe's result: what the terminal reported, and what was typed meanwhile.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Probe {
    pub caps: TerminalCaps,
    pub typed: String,
    /// Whether the queries were written.
    pub ran: bool,
}

/// What one call of [`parse`] found.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Parsed {
    /// The device attributes reply arrived, so no further reply is expected.
    pub done: bool,
    /// The buffer ends inside a reply.
    pub partial: bool,
    /// Bytes at the end of the buffer that belong to that unfinished reply.
    pub pending: usize,
}

enum Reply {
    Kitty(u8),
    Background((u8, u8, u8)),
    Sync(Tri),
    Name(String),
    Attributes,
}

enum Scan {
    Reply(usize, Reply),
    Partial,
    Other,
}

/// Reads replies out of `bytes` into `caps`; every other byte goes to `typed`.
///
/// An unfinished reply at the end is left unconsumed and counted in
/// `Parsed::pending`, so the caller can prepend it to the next read.
pub fn parse(bytes: &[u8], caps: &mut TerminalCaps, typed: &mut Vec<u8>) -> Parsed {
    let mut parsed = Parsed::default();
    let mut at = 0;
    while let Some(rest) = bytes.get(at..).filter(|rest| !rest.is_empty()) {
        if rest.first() != Some(&ESC) {
            typed.extend(rest.first());
            at = at.saturating_add(1);
            continue;
        }
        match scan(rest) {
            Scan::Reply(len, reply) => {
                at = at.saturating_add(len);
                match reply {
                    Reply::Kitty(flags) => caps.kitty_keyboard = Some(flags),
                    Reply::Background(rgb) => caps.background = Some(rgb),
                    Reply::Sync(state) => caps.sync_output = state,
                    Reply::Name(name) => caps.name = Some(name),
                    Reply::Attributes => {
                        caps.replied = true;
                        parsed.done = true;
                        typed.extend_from_slice(bytes.get(at..).unwrap_or_default());
                        return parsed;
                    }
                }
            }
            Scan::Partial => {
                parsed.partial = true;
                parsed.pending = rest.len();
                return parsed;
            }
            Scan::Other => {
                typed.push(ESC);
                at = at.saturating_add(1);
            }
        }
    }
    parsed
}

/// Recognises one reply at the start of `bytes`, which begins with `ESC`.
fn scan(bytes: &[u8]) -> Scan {
    let found = match bytes.get(1) {
        None => Scan::Partial,
        Some(b'[') => scan_csi(bytes),
        Some(b']') => scan_string(bytes, b"11;", background),
        Some(b'P') => scan_string(bytes, b">|", name),
        Some(_) => Scan::Other,
    };
    match found {
        Scan::Partial if bytes.len() > MAX_REPLY => Scan::Other,
        found => found,
    }
}

/// `ESC [ ? <params> u|c` and `ESC [ ? 2026 ; <Ps> $ y`.
fn scan_csi(bytes: &[u8]) -> Scan {
    match bytes.get(2) {
        None => return Scan::Partial,
        Some(b'?') => {}
        Some(_) => return Scan::Other,
    }
    let params = bytes
        .get(3..)
        .unwrap_or_default()
        .iter()
        .take_while(|byte| byte.is_ascii_digit() || **byte == b';')
        .count();
    let end = params.saturating_add(3);
    let text = std::str::from_utf8(bytes.get(3..end).unwrap_or_default()).unwrap_or_default();
    let digits_only = !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    match bytes.get(end) {
        None => Scan::Partial,
        Some(b'u') if digits_only => {
            let flags = text.parse::<u64>().map_or(u8::MAX, |n| {
                u8::try_from(n.min(u64::from(u8::MAX))).unwrap_or(u8::MAX)
            });
            Scan::Reply(end.saturating_add(1), Reply::Kitty(flags))
        }
        Some(b'c') => Scan::Reply(end.saturating_add(1), Reply::Attributes),
        Some(b'$') => match bytes.get(end.saturating_add(1)) {
            None => Scan::Partial,
            Some(b'y') => {
                let state = match text.strip_prefix("2026;") {
                    Some("1" | "2" | "3") => Tri::Yes,
                    Some("0" | "4") => Tri::No,
                    _ => return Scan::Other,
                };
                Scan::Reply(end.saturating_add(2), Reply::Sync(state))
            }
            Some(_) => Scan::Other,
        },
        Some(_) => Scan::Other,
    }
}

/// An OSC or DCS string whose body starts with `prefix`, ended by `ST` or `BEL`.
fn scan_string(bytes: &[u8], prefix: &[u8], read: fn(&[u8]) -> Option<Reply>) -> Scan {
    let body = bytes.get(2..).unwrap_or_default();
    let mut at = 0;
    let (end, terminator) = loop {
        match body.get(at) {
            None => {
                let matches = if body.len() < prefix.len() {
                    prefix.starts_with(body)
                } else {
                    body.starts_with(prefix)
                };
                return if matches { Scan::Partial } else { Scan::Other };
            }
            Some(&BEL) => break (at, 1),
            Some(&ESC) => match body.get(at.saturating_add(1)) {
                None => {
                    return if body.starts_with(prefix) {
                        Scan::Partial
                    } else {
                        Scan::Other
                    };
                }
                Some(b'\\') => break (at, 2),
                Some(_) => return Scan::Other,
            },
            Some(_) => at = at.saturating_add(1),
        }
    };
    let content = body.get(..end).unwrap_or_default();
    match content.strip_prefix(prefix).and_then(read) {
        Some(reply) => Scan::Reply(end.saturating_add(2).saturating_add(terminator), reply),
        None => Scan::Other,
    }
}

/// `rgb:<h>/<h>/<h>` or `rgba:<h>/<h>/<h>/<h>`, each channel scaled to 8 bits.
fn background(body: &[u8]) -> Option<Reply> {
    let text = std::str::from_utf8(body).ok()?;
    let (channels, count) = if let Some(rest) = text.strip_prefix("rgba:") {
        (rest, 4)
    } else {
        (text.strip_prefix("rgb:")?, 3)
    };
    let parts: Vec<&str> = channels.split('/').collect();
    if parts.len() != count {
        return None;
    }
    let mut scaled = [0u8; 3];
    for (slot, part) in scaled.iter_mut().zip(&parts) {
        *slot = scale(part)?;
    }
    let [r, g, b] = scaled;
    Some(Reply::Background((r, g, b)))
}

/// Scales one to four hex digits to 8 bits as `value * 255 / (16^len - 1)`.
fn scale(hex: &str) -> Option<u8> {
    if !(1..=4).contains(&hex.len()) || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(hex, 16).ok()?;
    let len = u32::try_from(hex.len()).ok()?;
    let max = 16u32.checked_pow(len)?.checked_sub(1)?;
    u8::try_from(value.checked_mul(255)?.checked_div(max)?).ok()
}

/// The terminal's name and version, without control characters.
fn name(body: &[u8]) -> Option<Reply> {
    let text: String = String::from_utf8_lossy(body)
        .chars()
        .filter(|ch| !ch.is_control())
        .take(MAX_NAME)
        .collect();
    Some(Reply::Name(text))
}

/// What a read during the probe produced.
enum Read {
    Bytes(Vec<u8>),
    Nothing,
    End,
}

/// Reads replies until the device attributes arrive or the deadline passes.
///
/// `read` waits at most the duration it is given. The deadline is
/// [`FIRST_REPLY`] after the queries were written, extended once to
/// [`LAST_REPLY`] when a reply has begun.
fn collect(mut read: impl FnMut(Duration) -> Read, caps: &mut TerminalCaps) -> Vec<u8> {
    let start = std::time::Instant::now();
    let mut limit = FIRST_REPLY;
    let mut pending: Vec<u8> = Vec::new();
    let mut typed = Vec::new();
    loop {
        let left = limit.saturating_sub(start.elapsed());
        if left.is_zero() {
            break;
        }
        match read(left) {
            Read::Bytes(chunk) => {
                pending.extend_from_slice(&chunk);
                let parsed = parse(&pending, caps, &mut typed);
                if parsed.done {
                    pending.clear();
                    break;
                }
                let consumed = pending.len().saturating_sub(parsed.pending);
                pending.drain(..consumed);
                if limit == FIRST_REPLY && (parsed.partial || caps.has_reply()) {
                    limit = LAST_REPLY;
                }
            }
            Read::Nothing => {}
            Read::End => break,
        }
    }
    typed.append(&mut pending);
    if caps.replied && caps.sync_output == Tri::Unknown {
        caps.sync_output = Tri::No;
    }
    typed
}

/// Turns the bytes typed during the probe into draft text.
///
/// Escape sequences and control characters are removed, and line breaks are
/// dropped so an early Enter never submits.
fn type_ahead(bytes: &[u8]) -> String {
    crate::transcript::sanitize(&String::from_utf8_lossy(bytes))
        .chars()
        .filter(|ch| *ch != '\r' && *ch != '\n')
        .collect()
}

/// Queries the terminal once and waits briefly for the replies.
///
/// Nothing is written when `enabled` is false, on Windows, when stdin or
/// stdout is not a terminal, or when `TERM` is `dumb`. Raw mode must already
/// be on.
#[must_use]
pub fn run(env: TerminalEnv, enabled: bool) -> Probe {
    use std::io::IsTerminal;
    let skip = !enabled
        || cfg!(windows)
        || !std::io::stdin().is_terminal()
        || !std::io::stdout().is_terminal()
        || env.term.as_deref() == Some("dumb");
    let mut caps = TerminalCaps::from_env(env);
    if skip {
        return Probe {
            caps,
            typed: String::new(),
            ran: false,
        };
    }
    let typed = exchange(&mut caps);
    Probe {
        caps,
        typed: type_ahead(&typed),
        ran: true,
    }
}

#[cfg(unix)]
fn exchange(caps: &mut TerminalCaps) -> Vec<u8> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    use rustix::io::Errno;
    let queries = [
        KITTY_FLAGS_QUERY,
        BACKGROUND_QUERY,
        SYNC_QUERY,
        VERSION_QUERY,
        DEVICE_ATTRIBUTES_QUERY,
    ]
    .concat();
    let stdout = rustix::stdio::stdout();
    let mut unsent = queries.as_bytes();
    while !unsent.is_empty() {
        match rustix::io::write(stdout, unsent) {
            Ok(0) => return Vec::new(),
            Ok(written) => unsent = unsent.get(written..).unwrap_or_default(),
            Err(Errno::INTR) => {}
            Err(_) => return Vec::new(),
        }
    }
    let stdin = rustix::stdio::stdin();
    let mut buffer = vec![0u8; 4_096];
    collect(
        |left| {
            let timeout = Timespec::try_from(left).ok();
            let mut fds = [PollFd::new(&stdin, PollFlags::IN)];
            match poll(&mut fds, timeout.as_ref()) {
                Ok(0) | Err(Errno::INTR) => return Read::Nothing,
                Ok(_) => {}
                Err(_) => return Read::End,
            }
            match rustix::io::read(stdin, buffer.as_mut_slice()) {
                Ok(0) => Read::End,
                Ok(count) => Read::Bytes(buffer.get(..count).unwrap_or_default().to_vec()),
                Err(Errno::INTR | Errno::AGAIN) => Read::Nothing,
                Err(_) => Read::End,
            }
        },
        caps,
    )
}

#[cfg(not(unix))]
fn exchange(_caps: &mut TerminalCaps) -> Vec<u8> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(chunks: &[&[u8]]) -> (TerminalCaps, Vec<u8>, Parsed) {
        let mut caps = TerminalCaps::default();
        let mut typed = Vec::new();
        let mut pending: Vec<u8> = Vec::new();
        let mut last = Parsed::default();
        for chunk in chunks {
            pending.extend_from_slice(chunk);
            last = parse(&pending, &mut caps, &mut typed);
            let consumed = pending.len().saturating_sub(last.pending);
            pending.drain(..consumed);
            if last.done {
                break;
            }
        }
        (caps, typed, last)
    }

    const REPLIES: &[u8] = b"\x1b[?1u\x1b]11;rgb:ffff/ffff/ffff\x1b\\\x1b[?2026;2$y\x1bP>|FakeTerm(1.0)\x1b\\\x1b[?62;22c";

    #[test]
    fn parses_every_reply_and_keeps_type_ahead() {
        let (caps, typed, parsed) = parse_all(&[REPLIES]);
        assert!(parsed.done);
        assert!(caps.replied);
        assert_eq!(caps.kitty_keyboard, Some(1));
        assert_eq!(caps.background, Some((255, 255, 255)));
        assert_eq!(caps.sync_output, Tri::Yes);
        assert_eq!(caps.name.as_deref(), Some("FakeTerm(1.0)"));
        assert!(typed.is_empty());

        // Typed before the replies, between two of them, and after the last.
        let before = [b"hi".as_slice(), REPLIES].concat();
        let between = b"\x1b[?1uhi\x1b[?62c".to_vec();
        let after = [REPLIES, b"hi".as_slice()].concat();
        for bytes in [before, between, after] {
            let (caps, typed, parsed) = parse_all(&[&bytes]);
            assert!(parsed.done && caps.replied);
            assert_eq!(type_ahead(&typed), "hi");
        }

        // A paste during the probe keeps its text and loses its markers.
        let (_, typed, _) = parse_all(&[b"\x1b[200~pasted\r\nline\x1b[201~\x1b[?62c"]);
        assert_eq!(type_ahead(&typed), "pastedline");

        // Device attributes alone: replied, and no colour.
        let mut caps = TerminalCaps::default();
        let typed = collect(
            {
                let mut once = Some(b"\x1b[?62c".to_vec());
                move |_| once.take().map_or(Read::End, Read::Bytes)
            },
            &mut caps,
        );
        assert!(typed.is_empty());
        assert!(caps.replied);
        assert_eq!(caps.sync_output, Tri::No);
        assert_eq!(caps.background, None);
    }

    #[test]
    fn background_scales_one_to_four_hex_digits() {
        let expected = Some((0xff, 0xee, 0xdd));
        for reply in [
            b"\x1b]11;rgb:ffff/eeee/dddd\x07".as_slice(),
            b"\x1b]11;rgb:ffff/eeee/dddd\x1b\\",
            b"\x1b]11;rgb:f/e/d\x1b\\",
            b"\x1b]11;rgb:ff/ee/dd\x07",
            b"\x1b]11;rgb:fff/eee/ddd\x07",
            b"\x1b]11;rgba:ffff/eeee/dddd/ffff\x1b\\",
        ] {
            let (caps, typed, _) = parse_all(&[reply]);
            assert_eq!(caps.background, expected, "{reply:?}");
            assert!(typed.is_empty(), "{reply:?}");
        }
        assert_eq!(scale("1e"), Some(0x1e));
        assert_eq!(scale("8"), Some(136));
        assert_eq!(scale("12345"), None);
        let (caps, typed, _) = parse_all(&[b"\x1b]11;rgb:ff/ee\x07"]);
        assert_eq!(caps.background, None);
        assert!(!typed.is_empty());
    }

    #[test]
    fn sync_reply_values_map_to_tri() {
        for (ps, expected) in [
            ("0", Tri::No),
            ("1", Tri::Yes),
            ("2", Tri::Yes),
            ("3", Tri::Yes),
            ("4", Tri::No),
        ] {
            let reply = format!("\x1b[?2026;{ps}$y");
            let (caps, typed, _) = parse_all(&[reply.as_bytes()]);
            assert_eq!(caps.sync_output, expected, "{ps}");
            assert!(typed.is_empty());
        }
        let (caps, _, _) = parse_all(&[b"\x1b[?1000;1$y"]);
        assert_eq!(caps.sync_output, Tri::Unknown);
    }

    #[test]
    fn a_split_reply_is_completed_by_the_next_read() {
        let middle = REPLIES.len() / 2;
        for at in [1, 3, 9, middle, REPLIES.len() - 1] {
            let (head, tail) = REPLIES.split_at(at);
            let mut caps = TerminalCaps::default();
            let mut typed = Vec::new();
            let first = parse(head, &mut caps, &mut typed);
            assert!(first.partial && !first.done, "{at}");
            let mut rest = head[head.len().saturating_sub(first.pending)..].to_vec();
            rest.extend_from_slice(tail);
            let second = parse(&rest, &mut caps, &mut typed);
            assert!(second.done, "{at}");
            assert!(typed.is_empty(), "{at}");
            assert_eq!(caps.name.as_deref(), Some("FakeTerm(1.0)"), "{at}");
            assert_eq!(caps.background, Some((255, 255, 255)), "{at}");
        }
    }

    #[test]
    fn unknown_sequences_are_type_ahead() {
        let (caps, typed, parsed) = parse_all(&[b"a\x1b[Ab\x1b]52;c;x\x07\x1b7c"]);
        assert!(!parsed.done && !parsed.partial);
        assert_eq!(caps, TerminalCaps::default());
        assert_eq!(typed, b"a\x1b[Ab\x1b]52;c;x\x07\x1b7c");
        assert_eq!(type_ahead(&typed), "abc");
        // A lone Escape at the end may begin a reply until the probe ends.
        let mut caps = TerminalCaps::default();
        let mut typed = Vec::new();
        let parsed = parse(b"x\x1b", &mut caps, &mut typed);
        assert_eq!((parsed.partial, parsed.pending), (true, 1));
        assert_eq!(typed, b"x");
    }

    #[test]
    fn luminance_threshold_separates_light_and_dark() {
        assert!(is_light((0xff, 0xff, 0xff)));
        assert!(!is_light((0x1e, 0x1e, 0x1e)));
        assert!(!is_light((0x80, 0x80, 0x80)));
        assert!(is_light((0xc0, 0xc0, 0xc0)));
    }

    #[test]
    fn a_silent_terminal_is_waited_for_briefly() {
        let mut caps = TerminalCaps::default();
        let started = std::time::Instant::now();
        let typed = collect(
            |left| {
                std::thread::sleep(left);
                Read::Nothing
            },
            &mut caps,
        );
        let waited = started.elapsed();
        assert!(waited >= FIRST_REPLY && waited < LAST_REPLY, "{waited:?}");
        assert!(typed.is_empty());
        assert!(!caps.replied);
        assert_eq!(caps.sync_output, Tri::Unknown);
        assert_eq!(caps.kitty_keyboard, None);
    }

    #[test]
    fn a_begun_reply_extends_the_wait_once() {
        let mut caps = TerminalCaps::default();
        let mut sent = false;
        let started = std::time::Instant::now();
        let typed = collect(
            |left| {
                if sent {
                    std::thread::sleep(left);
                    Read::Nothing
                } else {
                    sent = true;
                    Read::Bytes(b"\x1b[?1uhi\x1b[?20".to_vec())
                }
            },
            &mut caps,
        );
        let waited = started.elapsed();
        assert!(waited >= LAST_REPLY, "{waited:?}");
        assert_eq!(caps.kitty_keyboard, Some(1));
        assert!(!caps.replied);
        assert_eq!(caps.sync_output, Tri::Unknown);
        assert_eq!(type_ahead(&typed), "hi");
    }

    #[test]
    fn environment_sets_multiplexer_and_remote_flags() {
        let env = TerminalEnv::from_lookup(|name| match name {
            "TMUX" => Some("/tmp/tmux".to_owned()),
            "SSH_CONNECTION" => Some("a b c d".to_owned()),
            "STY" => Some(String::new()),
            _ => None,
        });
        assert_eq!(env.sty, None);
        let caps = TerminalCaps::from_env(env);
        assert!(caps.tmux && caps.ssh && !caps.multiplexer_passthrough);
        let caps = TerminalCaps::from_env(TerminalEnv::default());
        assert!(!caps.tmux && !caps.ssh && caps.multiplexer_passthrough);
        assert_eq!(
            caps.summary(),
            "unknown; background unknown; synchronized output unknown; kitty keyboard no; tmux no; ssh no"
        );
    }

    #[test]
    fn key_protocol_prefers_tmux_then_kitty_then_xterm() {
        if cfg!(windows) {
            assert_eq!(TerminalCaps::default().key_protocol(), KeyProtocol::Legacy);
            return;
        }
        let kitty = TerminalCaps {
            kitty_keyboard: Some(0),
            replied: true,
            ..TerminalCaps::default()
        };
        assert_eq!(kitty.key_protocol(), KeyProtocol::Kitty);
        // tmux inside kitty is still tmux.
        let tmux = TerminalCaps {
            tmux: true,
            ..kitty.clone()
        };
        assert_eq!(tmux.key_protocol(), KeyProtocol::ModifyOtherKeysTmux);
        let xterm = TerminalCaps {
            name: Some("XTerm(390)".to_owned()),
            replied: true,
            ..TerminalCaps::default()
        };
        assert_eq!(xterm.key_protocol(), KeyProtocol::ModifyOtherKeysXterm);
        let both = TerminalCaps {
            kitty_keyboard: Some(1),
            ..xterm.clone()
        };
        assert_eq!(both.key_protocol(), KeyProtocol::Kitty);
        let other = TerminalCaps {
            name: Some("FakeTerm(1.0)".to_owned()),
            replied: true,
            ..TerminalCaps::default()
        };
        assert_eq!(other.key_protocol(), KeyProtocol::Legacy);
        // A skipped probe asks for nothing, even inside tmux.
        let skipped = TerminalCaps {
            tmux: true,
            ..TerminalCaps::default()
        };
        assert_eq!(skipped.key_protocol(), KeyProtocol::Legacy);
    }

    #[test]
    fn newline_hint_follows_protocol() {
        let hint = |caps: TerminalCaps| caps.newline_hint();
        let shift = if cfg!(windows) {
            "shift-enter newline"
        } else {
            "ctrl-j newline"
        };
        assert_eq!(hint(TerminalCaps::default()), shift);
        assert_eq!(
            hint(TerminalCaps {
                tmux: true,
                replied: true,
                kitty_keyboard: Some(1),
                ..TerminalCaps::default()
            }),
            shift
        );
        assert_eq!(
            hint(TerminalCaps {
                kitty_keyboard: Some(1),
                replied: true,
                ..TerminalCaps::default()
            }),
            "shift-enter newline"
        );
        assert_eq!(
            hint(TerminalCaps {
                name: Some("XTerm(390)".to_owned()),
                replied: true,
                ..TerminalCaps::default()
            }),
            "shift-enter newline"
        );
    }

    #[test]
    fn nothing_is_probed_when_disabled_or_dumb() {
        let probe = run(TerminalEnv::default(), false);
        assert!(!probe.caps.replied && probe.typed.is_empty());
        let dumb = TerminalEnv {
            term: Some("dumb".to_owned()),
            ..TerminalEnv::default()
        };
        let started = std::time::Instant::now();
        let probe = run(dumb, true);
        assert!(started.elapsed() < FIRST_REPLY);
        assert!(!probe.caps.replied);
    }
}
