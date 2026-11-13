//! Puts text on the clipboard: through the platform's clipboard tool when one
//! is installed, and through the terminal (OSC 52), which is the only way to
//! reach the user's clipboard over SSH.

use std::io::Write as _;
use std::time::{Duration, Instant};

use rune_core::error::{ErrorCode, Result, RuneError};
use rune_term::probe::TerminalCaps;

/// The longest OSC 52 sequence sent; terminals refuse or cut longer ones.
const OSC52_LIMIT: usize = 100_000;

/// The largest piece GNU screen passes through in one device control string.
const SCREEN_CHUNK: usize = 768;

/// How long a clipboard tool may take.
const NATIVE_TIMEOUT: Duration = Duration::from_secs(3);

/// How the text reached the clipboard.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CopyPath {
    /// The platform tool named.
    Native(&'static str),
    /// The terminal, through OSC 52.
    Osc52,
    /// Both the platform tool and the terminal.
    Both(&'static str),
}

impl CopyPath {
    /// How the notice names the path.
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::Native(tool) => tool.to_owned(),
            Self::Osc52 => "OSC 52".to_owned(),
            Self::Both(tool) => format!("{tool} and OSC 52"),
        }
    }
}

/// The platform a native command is chosen for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Os {
    Mac,
    Linux,
    Windows,
}

impl Os {
    /// The platform this build runs on.
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Mac
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Linux
        }
    }
}

/// Returns the bytes that put `text` on the clipboard through the terminal,
/// wrapped for tmux or screen when the session runs inside one, or `None`
/// when the sequence would be longer than terminals accept.
#[must_use]
pub fn osc52(text: &str, caps: &TerminalCaps) -> Option<Vec<u8>> {
    let sequence = format!(
        "\u{1b}]52;c;{}\u{1b}\\",
        crate::session::base64_encode(text.as_bytes())
    );
    if sequence.len() > OSC52_LIMIT {
        return None;
    }
    let mut bytes = sequence.clone().into_bytes();
    if caps.tmux {
        // tmux passes a sequence on only inside its own wrapper, with every
        // escape doubled; the plain form serves a tmux that forwards it.
        bytes.extend_from_slice(b"\x1bPtmux;");
        bytes.extend_from_slice(sequence.replace('\u{1b}', "\u{1b}\u{1b}").as_bytes());
        bytes.extend_from_slice(b"\x1b\\");
    } else if caps.env.sty.is_some() {
        bytes.clear();
        for chunk in sequence.as_bytes().chunks(SCREEN_CHUNK) {
            bytes.extend_from_slice(b"\x1bP");
            bytes.extend_from_slice(chunk);
            bytes.extend_from_slice(b"\x1b\\");
        }
    }
    Some(bytes)
}

/// The platform's clipboard commands, first choice first: `pbcopy` on macOS;
/// `wl-copy` under Wayland, `xclip` then `xsel` under X11; `clip.exe` on
/// Windows.
#[must_use]
pub fn native_commands(os: Os, env: &dyn Fn(&str) -> Option<String>) -> Vec<Vec<String>> {
    let set = |name: &str| env(name).is_some_and(|value| !value.is_empty());
    let command = |parts: &[&str]| parts.iter().map(|part| (*part).to_owned()).collect();
    match os {
        Os::Mac => vec![command(&["pbcopy"])],
        Os::Windows => vec![command(&["clip.exe"])],
        Os::Linux => {
            let mut commands = Vec::new();
            if set("WAYLAND_DISPLAY") {
                commands.push(command(&["wl-copy"]));
            }
            if set("DISPLAY") {
                commands.push(command(&["xclip", "-selection", "clipboard"]));
                commands.push(command(&["xsel", "--clipboard", "--input"]));
            }
            commands
        }
    }
}

/// The name a notice gives a native command.
fn tool_name(program: &str) -> &'static str {
    match program {
        "pbcopy" => "pbcopy",
        "wl-copy" => "wl-copy",
        "xclip" => "xclip",
        "xsel" => "xsel",
        _ => "clip.exe",
    }
}

/// The tool the error suggests installing.
const fn suggested_tool(os: Os, wayland: bool) -> &'static str {
    match os {
        Os::Mac => "pbcopy (built in on macOS)",
        Os::Windows => "clip.exe (it ships with Windows)",
        Os::Linux if wayland => "wl-copy",
        Os::Linux => "xclip",
    }
}

/// Copies `text`: over SSH through the terminal only; locally through the
/// first clipboard tool on `PATH` and also through the terminal, or through
/// the terminal alone when no tool is installed. Terminal bytes go to `write`,
/// the session's single writer.
///
/// # Errors
///
/// Fails when the text is too long for the terminal and no tool can take it.
pub fn copy(text: &str, caps: &TerminalCaps, write: &mut dyn FnMut(&[u8])) -> Result<CopyPath> {
    let os = Os::current();
    let env = |name: &str| std::env::var(name).ok();
    let sequence = osc52(text, caps);
    let native = if caps.ssh {
        None
    } else {
        native_commands(os, &env)
            .into_iter()
            .find(|command| command.first().is_some_and(|program| on_path(program)))
    };
    let ran = native
        .as_ref()
        .and_then(|command| run_native(command, text, os).ok().map(|()| command));
    if let Some(sequence) = &sequence {
        write(sequence);
    }
    match (ran, sequence) {
        (Some(command), Some(_)) => Ok(CopyPath::Both(tool_name(&command[0]))),
        (Some(command), None) => Ok(CopyPath::Native(tool_name(&command[0]))),
        (None, Some(_)) => Ok(CopyPath::Osc52),
        (None, None) => Err(RuneError::new(
            ErrorCode::TooLarge,
            format!(
                "could not copy {} characters: too long for the terminal clipboard",
                text.chars().count()
            ),
        )
        .with_hint(format!(
            "install {}",
            suggested_tool(os, env("WAYLAND_DISPLAY").is_some())
        ))),
    }
}

/// Returns true when `program` is an executable file on `PATH`.
fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|directory| directory.join(program).is_file())
    })
}

/// Runs a clipboard tool with `text` on its standard input, without a shell.
fn run_native(command: &[String], text: &str, os: Os) -> std::io::Result<()> {
    let (program, arguments) = command
        .split_first()
        .ok_or_else(|| std::io::Error::other("empty clipboard command"))?;
    let mut child = std::process::Command::new(program)
        .args(arguments)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let bytes = if os == Os::Windows {
        utf16le_with_bom(text)
    } else {
        text.as_bytes().to_vec()
    };
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&bytes)?;
    }
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return if status.success() {
                Ok(())
            } else {
                Err(std::io::Error::other(format!("{program} failed")))
            };
        }
        if started.elapsed() >= NATIVE_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::other(format!("{program} timed out")));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The text as `clip.exe` reads it: UTF-16LE after the byte order mark.
fn utf16le_with_bom(text: &str) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

/// The commands that read an image from the clipboard into `file`, first
/// choice first: `pngpaste` or `osascript` on macOS; `wl-paste` under
/// Wayland; `xclip` under X11; PowerShell on Windows. A Linux command that
/// lists the clipboard's types comes before the one that reads the image.
#[must_use]
pub fn image_commands(
    os: Os,
    env: &dyn Fn(&str) -> Option<String>,
    file: &str,
) -> Vec<Vec<String>> {
    let set = |name: &str| env(name).is_some_and(|value| !value.is_empty());
    let command = |parts: &[&str]| parts.iter().map(|part| (*part).to_owned()).collect();
    match os {
        Os::Mac => vec![
            command(&["pngpaste", file]),
            vec![
                "osascript".to_owned(),
                "-e".to_owned(),
                format!("set f to open for access POSIX file \"{file}\" with write permission"),
                "-e".to_owned(),
                "write (the clipboard as \u{ab}class PNGf\u{bb}) to f".to_owned(),
                "-e".to_owned(),
                "close access f".to_owned(),
            ],
        ],
        Os::Windows => vec![vec![
            "powershell.exe".to_owned(),
            "-NoProfile".to_owned(),
            "-Command".to_owned(),
            format!(
                "$i = Get-Clipboard -Format Image; if ($i) {{ $i.Save('{file}', [System.Drawing.Imaging.ImageFormat]::Png) }}"
            ),
        ]],
        Os::Linux => {
            let mut commands = Vec::new();
            if set("WAYLAND_DISPLAY") {
                commands.push(command(&["wl-paste", "--list-types"]));
                commands.push(command(&["wl-paste", "--type", "image/png"]));
            }
            if set("DISPLAY") {
                commands.push(command(&[
                    "xclip",
                    "-selection",
                    "clipboard",
                    "-t",
                    "TARGETS",
                    "-o",
                ]));
                commands.push(command(&[
                    "xclip",
                    "-selection",
                    "clipboard",
                    "-t",
                    "image/png",
                    "-o",
                ]));
            }
            commands
        }
    }
}

/// Runs one command without a shell, stdin closed, for at most three seconds,
/// keeping at most `cap` bytes of its output.
fn capture(command: &[String], cap: usize) -> Option<Vec<u8>> {
    use std::io::Read as _;
    let (program, arguments) = command.split_first()?;
    let mut child = std::process::Command::new(program)
        .args(arguments)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = (&mut stdout)
            .take(u64::try_from(cap).unwrap_or(u64::MAX))
            .read_to_end(&mut bytes);
        bytes
    });
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let bytes = reader.join().ok()?;
                return status.success().then_some(bytes);
            }
            Ok(None) if started.elapsed() < NATIVE_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Reads an image from the clipboard into
/// `<state root>/attachments/<session id>/clip-{n}.png` (directory 0700,
/// file 0600) and returns its path, or `None` when the clipboard holds no
/// image a tool here can read.
#[must_use]
pub fn read_image(paths: &rune_core::paths::Paths, session_id: &str) -> Option<std::path::PathBuf> {
    let directory = paths.state_root.join("attachments").join(session_id);
    rune_core::paths::create_dir_private(&directory).ok()?;
    let number = std::fs::read_dir(&directory)
        .map_or(0, Iterator::count)
        .saturating_add(1);
    let file = directory.join(format!("clip-{number}.png"));
    let os = Os::current();
    let env = |name: &str| std::env::var(name).ok();
    let commands = image_commands(os, &env, file.as_str());
    let cap = crate::attachments::MAX_IMAGE_BYTES;
    let mut index = 0;
    while let Some(command) = commands.get(index) {
        index = index.saturating_add(1);
        if !command.first().is_some_and(|program| on_path(program)) {
            continue;
        }
        let lists_types = command
            .iter()
            .any(|part| part == "--list-types" || part == "TARGETS");
        let Some(output) = capture(command, cap) else {
            continue;
        };
        if lists_types {
            // The next command reads the image only when the clipboard has one.
            if !String::from_utf8_lossy(&output)
                .lines()
                .any(|line| line.trim() == "image/png")
            {
                index = index.saturating_add(1);
            }
            continue;
        }
        // Tools that print the image to standard output leave the file to us.
        let bytes = if output.is_empty() {
            std::fs::read(&file).ok()?
        } else {
            output
        };
        if rune_tools::vision::image_media_type(&bytes).is_some() {
            if !file.exists() {
                rune_core::paths::write_private_bytes(&file, &bytes).ok()?;
            }
            return Some(file.into_std_path_buf());
        }
    }
    let _ = std::fs::remove_file(&file);
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use rune_term::probe::TerminalEnv;

    fn caps(tmux: bool, ssh: bool, sty: bool) -> TerminalCaps {
        let mut caps = TerminalCaps::from_env(TerminalEnv::default());
        caps.tmux = tmux;
        caps.ssh = ssh;
        caps.env.sty = sty.then(|| "1234.pts-0".to_owned());
        caps
    }

    #[test]
    fn osc52_uses_st_and_base64() {
        let bytes = osc52("hi", &caps(false, false, false)).expect("short");
        assert_eq!(bytes, b"\x1b]52;c;aGk=\x1b\\");
    }

    #[test]
    fn tmux_gets_both_forms_with_doubled_escapes() {
        let bytes = osc52("hi", &caps(true, false, false)).expect("short");
        let text = String::from_utf8(bytes).expect("utf8");
        assert!(text.starts_with("\u{1b}]52;c;aGk=\u{1b}\\"), "{text:?}");
        let wrapped = text
            .split_once("\u{1b}Ptmux;")
            .map(|(_, rest)| rest)
            .expect("passthrough");
        assert_eq!(wrapped, "\u{1b}\u{1b}]52;c;aGk=\u{1b}\u{1b}\\\u{1b}\\");
        // Inside the wrapper no escape stands alone.
        let inner = wrapped.strip_suffix("\u{1b}\\").expect("terminator");
        assert!(inner.replace("\u{1b}\u{1b}", "").find('\u{1b}').is_none());
    }

    #[test]
    fn screen_chunks_at_768_bytes() {
        let text = "x".repeat(2_000);
        let bytes = osc52(&text, &caps(false, false, true)).expect("short");
        let text = String::from_utf8(bytes).expect("utf8");
        let chunks: Vec<&str> = text
            .split("\u{1b}P")
            .filter(|chunk| !chunk.is_empty())
            .collect();
        assert!(chunks.len() > 1, "{}", chunks.len());
        for chunk in &chunks {
            let body = chunk.strip_suffix("\u{1b}\\").expect("each chunk ends");
            assert!(body.len() <= SCREEN_CHUNK, "{}", body.len());
        }
    }

    #[test]
    fn too_long_text_has_no_osc52() {
        // 60 KiB encodes to about 80 KB: sent. 90 KiB is about 120 KB: refused.
        assert!(osc52(&"a".repeat(60 * 1024), &caps(false, true, false)).is_some());
        assert!(osc52(&"a".repeat(90 * 1024), &caps(false, true, false)).is_none());
    }

    #[test]
    fn ssh_prefers_osc52() {
        let mut written = Vec::new();
        let path = copy("over ssh", &caps(false, true, false), &mut |bytes| {
            written.extend_from_slice(bytes);
        })
        .expect("copied");
        assert_eq!(path, CopyPath::Osc52);
        assert_eq!(path.label(), "OSC 52");
        assert!(written.starts_with(b"\x1b]52;c;"));
        let refused = copy(
            &"a".repeat(90 * 1024),
            &caps(false, true, false),
            &mut |_| {},
        )
        .expect_err("too long");
        assert!(
            refused.message().starts_with(
                "could not copy 92160 characters: too long for the terminal clipboard"
            ),
            "{}",
            refused.message()
        );
        assert!(
            refused
                .hint()
                .is_some_and(|hint| hint.starts_with("install "))
        );
    }

    #[test]
    fn the_candidate_commands_follow_the_platform() {
        let none = |_: &str| None;
        let mac = image_commands(Os::Mac, &none, "/s/clip-1.png");
        assert_eq!(mac[0], ["pngpaste", "/s/clip-1.png"]);
        assert_eq!(mac[1][0], "osascript");
        assert!(mac[1][2].contains("POSIX file \"/s/clip-1.png\""));
        assert!(image_commands(Os::Linux, &none, "f").is_empty());
        let wayland = |name: &str| (name == "WAYLAND_DISPLAY").then(|| "wayland-0".to_owned());
        assert_eq!(
            image_commands(Os::Linux, &wayland, "f"),
            [
                vec!["wl-paste".to_owned(), "--list-types".to_owned()],
                vec![
                    "wl-paste".to_owned(),
                    "--type".to_owned(),
                    "image/png".to_owned()
                ],
            ]
        );
        let x11 = |name: &str| (name == "DISPLAY").then(|| ":0".to_owned());
        let commands = image_commands(Os::Linux, &x11, "f");
        assert_eq!(commands.len(), 2);
        assert!(commands[0].contains(&"TARGETS".to_owned()));
        let windows = image_commands(Os::Windows, &none, "C:\\clip.png");
        assert_eq!(windows[0][0], "powershell.exe");
        assert!(windows[0][3].contains("Get-Clipboard -Format Image"));
    }

    #[test]
    fn native_commands_follow_the_platform() {
        let none = |_: &str| None;
        assert_eq!(native_commands(Os::Mac, &none), [vec!["pbcopy".to_owned()]]);
        assert_eq!(
            native_commands(Os::Windows, &none),
            [vec!["clip.exe".to_owned()]]
        );
        assert!(native_commands(Os::Linux, &none).is_empty());
        let wayland = |name: &str| (name == "WAYLAND_DISPLAY").then(|| "wayland-0".to_owned());
        assert_eq!(
            native_commands(Os::Linux, &wayland),
            [vec!["wl-copy".to_owned()]]
        );
        let x11 = |name: &str| (name == "DISPLAY").then(|| ":0".to_owned());
        assert_eq!(
            native_commands(Os::Linux, &x11),
            [
                vec![
                    "xclip".to_owned(),
                    "-selection".to_owned(),
                    "clipboard".to_owned()
                ],
                vec![
                    "xsel".to_owned(),
                    "--clipboard".to_owned(),
                    "--input".to_owned()
                ],
            ]
        );
        assert_eq!(utf16le_with_bom("A"), [0xFF, 0xFE, 0x41, 0x00]);
        assert_eq!(CopyPath::Both("pbcopy").label(), "pbcopy and OSC 52");
    }
}
