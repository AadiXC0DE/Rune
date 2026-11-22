//! The terminal interface in a Windows pseudo console: startup, streaming,
//! approval, paste, resize and exit, against a scripted provider.

#![cfg(windows)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rune_testkit::scripted::{ScriptedProvider, Turn};
use support::conpty::ConPty;

/// Long enough for a cold start on a loaded machine.
const WAIT: Duration = Duration::from_secs(20);

/// A workspace and the isolated folders a run keeps its state in.
struct Fixture {
    root: tempfile::TempDir,
    provider: ScriptedProvider,
}

impl Fixture {
    fn new(turns: Vec<Turn>) -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let config = root.path().join("config").join("rune");
        std::fs::create_dir_all(&config).expect("config folder");
        std::fs::write(config.join("config.toml"), "auto_upgrade = false\n").expect("config");
        Self {
            root,
            provider: ScriptedProvider::start(turns),
        }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.path().join(relative)
    }

    /// Starts the binary with `arguments` in a console of `cols` by `rows`.
    fn spawn(&self, arguments: &[&str], cols: u16, rows: u16) -> ConPty {
        let mut argv = vec![env!("CARGO_BIN_EXE_rune")];
        argv.extend_from_slice(arguments);
        self.spawn_argv(&argv, cols, rows)
    }

    /// Starts `argv` with the run's isolated environment, in the workspace.
    fn spawn_argv(&self, argv: &[&str], cols: u16, rows: u16) -> ConPty {
        let root = self.root.path();
        let text = |relative: &str| self.path(relative).to_string_lossy().into_owned();
        let system_root = std::env::var("SystemRoot").expect("SystemRoot");
        let path = std::env::var("PATH").unwrap_or_default();
        let base_url = self.provider.base_url();
        let owned = [
            ("SystemRoot", system_root.clone()),
            ("windir", system_root),
            ("PATH", path),
            ("USERPROFILE", root.to_string_lossy().into_owned()),
            ("HOME", root.to_string_lossy().into_owned()),
            ("APPDATA", text("appdata")),
            ("LOCALAPPDATA", text("localappdata")),
            ("XDG_CONFIG_HOME", text("config")),
            ("XDG_DATA_HOME", text("data")),
            ("XDG_STATE_HOME", text("xdg-state")),
            ("RUNE_HOME", text("state")),
            ("RUNE_PROVIDER", String::from("chat_completions")),
            ("RUNE_BASE_URL", base_url),
            ("RUNE_MODEL", String::from("console-fixture")),
            ("RUNE_API_KEY_ENV", String::from("RUNE_CONSOLE_TEST_KEY")),
            ("RUNE_CONSOLE_TEST_KEY", String::from("fixture-key")),
            ("NO_COLOR", String::from("1")),
            ("TERM", String::from("xterm-256color")),
        ];
        let env: Vec<(&str, &str)> = owned
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        ConPty::spawn(argv, &env, root, cols, rows).expect("the console starts")
    }
}

/// Returns the screen once it holds `needle`, or fails with the screen.
fn expect_screen(console: &mut ConPty, needle: &str) -> String {
    console
        .wait_for(needle, WAIT)
        .unwrap_or_else(|message| panic!("{message}"))
}

/// Types `text`, then presses Enter once the keys no longer look like a paste.
fn submit(console: &mut ConPty, text: &str) {
    console.write(text.as_bytes()).expect("type");
    std::thread::sleep(Duration::from_millis(300));
    console.write(b"\r").expect("enter");
}

/// Returns the rows that hold the prompt.
fn prompt_rows(screen: &str) -> Vec<&str> {
    screen.lines().filter(|row| row.starts_with('>')).collect()
}

#[test]
fn the_console_harness_shows_what_a_program_prints() {
    let root = std::env::var("SystemRoot").expect("SystemRoot");
    let cmd = format!(r"{root}\System32\cmd.exe");
    let mut console = ConPty::spawn(
        &[&cmd, "/C", "echo console-harness-ok"],
        &[("SystemRoot", &root)],
        Path::new(&root),
        80,
        24,
    )
    .expect("the console starts");
    expect_screen(&mut console, "console-harness-ok");
    assert_eq!(console.wait_exit(WAIT), Some(0));
}

#[test]
fn startup_shows_the_prompt() {
    let fixture = Fixture::new(Vec::new());
    let mut console = fixture.spawn(&[], 100, 24);
    let screen = console
        .wait_until("the session line and an empty prompt", WAIT, |screen| {
            screen.contains("session ") && prompt_rows(screen) == [">"]
        })
        .unwrap_or_else(|message| panic!("{message}"));
    assert!(screen.contains("console-fixture"), "{screen}");
}

#[test]
fn a_streamed_answer_appears() {
    let fixture = Fixture::new(vec![Turn::text("STREAMED_ANSWER_TEXT")]);
    let mut console = fixture.spawn(&[], 100, 24);
    expect_screen(&mut console, "session ");
    submit(&mut console, "hello");
    expect_screen(&mut console, "STREAMED_ANSWER_TEXT");
    assert_eq!(fixture.provider.request_count(), 1);
}

#[test]
fn an_approval_can_be_answered() {
    let fixture = Fixture::new(vec![
        Turn::call(
            "shell-1",
            "shell",
            r#"{"action":"run","command":"echo APPROVAL_RAN"}"#,
        ),
        Turn::text("APPROVAL_DONE"),
    ]);
    let mut console = fixture.spawn(
        &["--permission-mode", "ask", "--allow-unsandboxed"],
        100,
        30,
    );
    expect_screen(&mut console, "session ");
    submit(&mut console, "run it");
    expect_screen(&mut console, "permission required");
    expect_screen(&mut console, "echo APPROVAL_RAN");
    expect_screen(&mut console, "y  run once");
    // Keys in the first moments of a question are ignored.
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(fixture.provider.request_count(), 1, "ran before approval");
    console.write(b"y").expect("approve");
    expect_screen(&mut console, "APPROVAL_DONE");
    let requests = fixture.provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1].body.contains("APPROVAL_RAN"),
        "the command's output did not reach the model: {}",
        requests[1].body
    );
}

#[test]
fn a_large_paste_becomes_a_chip() {
    let fixture = Fixture::new(Vec::new());
    let mut console = fixture.spawn(&[], 100, 24);
    expect_screen(&mut console, "session ");
    // A console paste arrives as keys in a burst; Enter inside the burst is a
    // line of the paste, not a submission.
    let pasted = (1..=40).fold(String::new(), |mut pasted, line| {
        let _ = write!(pasted, "pasted line {line}\r");
        pasted
    });
    console.write(pasted.as_bytes()).expect("paste");
    let screen = expect_screen(&mut console, "[pasted #1:");
    assert!(!screen.contains("pasted line 2"), "{screen}");
    assert_eq!(fixture.provider.request_count(), 0, "the paste was sent");
}

#[test]
fn resize_redraws_without_garbage() {
    let fixture = Fixture::new(Vec::new());
    let mut console = fixture.spawn(&[], 100, 24);
    expect_screen(&mut console, "session ");
    console.write(b"draft before resize").expect("type");
    expect_screen(&mut console, "> draft before resize");
    console.resize(60, 24).expect("resize");
    let screen = console
        .wait_until("one intact prompt line at 60 columns", WAIT, |screen| {
            prompt_rows(screen) == ["> draft before resize"]
        })
        .unwrap_or_else(|message| panic!("{message}"));
    assert!(
        screen.lines().all(|row| row.chars().count() <= 60),
        "{screen}"
    );
    // The draft is still the draft: typing continues it.
    console.write(b" and after").expect("type");
    expect_screen(&mut console, "> draft before resize and after");
}

/// Reports the console's input and output modes, runs the program named by its
/// first argument in the same console, reports the first change it saw while
/// the program ran, and reports the modes again once it has exited.
const MODE_PROBE: &str = "\
import ctypes, subprocess, sys, threading, time
k = ctypes.windll.kernel32
k.CreateFileW.restype = ctypes.c_void_p
def mode(name):
    h = k.CreateFileW(name, 0xC0000000, 3, None, 3, 0, None)
    m = ctypes.c_uint(0)
    k.GetConsoleMode(ctypes.c_void_p(h), ctypes.byref(m))
    k.CloseHandle(ctypes.c_void_p(h))
    return m.value
def modes():
    return 'in=%d out=%d' % (mode('CONIN$'), mode('CONOUT$'))
before = modes()
print('probe-before', before, flush=True)
child = subprocess.Popen(sys.argv[1:])
changed = []
def sample():
    while child.poll() is None and not changed:
        now = modes()
        if now != before:
            changed.append(now)
        time.sleep(0.02)
watcher = threading.Thread(target=sample)
watcher.start()
code = child.wait()
watcher.join()
print('probe-during', changed[0] if changed else 'unchanged', flush=True)
print('probe-exit', code, flush=True)
print('probe-after', modes(), flush=True)
";

#[test]
fn ctrl_d_exits_cleanly() {
    let fixture = Fixture::new(Vec::new());
    let python = support::python();
    let binary = env!("CARGO_BIN_EXE_rune");
    let mut console = fixture.spawn_argv(&[python, "-c", MODE_PROBE, binary], 100, 24);
    expect_screen(&mut console, "session ");
    std::thread::sleep(Duration::from_millis(300));
    console.write(b"\x04").expect("ctrl-d");
    expect_screen(&mut console, "probe-after in=");
    // The probe's lines are read from everything the console drew, because
    // the session's own drawing may have moved the first one off the screen.
    let transcript = console.transcript();
    let value = |prefix: &str| {
        let start = transcript
            .find(prefix)
            .unwrap_or_else(|| panic!("no {prefix:?} in {transcript:?}"))
            + prefix.len();
        transcript[start..]
            .split(['\r', '\n', '\u{1b}'])
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    assert_eq!(value("probe-exit "), "0", "{transcript:?}");
    // Rune changes the modes for its own use and hands back the ones it found.
    let before = value("probe-before ");
    let during = value("probe-during ");
    assert_ne!(during, "unchanged", "{transcript:?}");
    assert_ne!(during, before, "{transcript:?}");
    assert_eq!(value("probe-after "), before, "{transcript:?}");
    assert_eq!(console.wait_exit(WAIT), Some(0));
}
