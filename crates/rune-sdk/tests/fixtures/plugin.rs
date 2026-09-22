//! A scripted plugin used by the plugin protocol's integration tests.
//!
//! The host drives this as a child process over its standard input and output,
//! so the tests exercise real framing, real timeouts, and real process handling
//! rather than a stub. The scenario is named on the command line, which keeps
//! every case in one file and needs no shell, so the tests run wherever the host
//! does.
//!
//! A scenario may also name a file to record the frames it receives to, which is
//! how a test proves that a denied call never reached the plugin.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::io::{BufRead, BufReader, Write};

use serde_json::{Value, json};

/// Protocol version this fixture speaks, matching the host's.
const PROTOCOL_VERSION: u32 = 1;

/// A result the fixture answers with, or the failure it reports.
enum Reply {
    /// Answers with a result.
    Result(Value),
    /// Answers with a JSON-RPC failure.
    Failure { code: i64, message: String },
    /// Answers nothing.
    Silence,
    /// Ends the process.
    Exit(i32),
}

fn main() {
    let mut arguments = std::env::args().skip(1);
    let scenario = arguments.next().unwrap_or_default();
    // Where to record received frames, when the test asks for a record.
    let journal = arguments.next().filter(|path| !path.is_empty());

    // Reads input forever without answering.
    if scenario == "silent" {
        let mut input = BufReader::new(std::io::stdin().lock());
        let mut line = String::new();
        while input.read_line(&mut line).is_ok_and(|read| read > 0) {
            line.clear();
        }
        return;
    }

    // Exits immediately, so every start fails.
    if scenario == "crasher" {
        std::process::exit(3);
    }

    let mut input = BufReader::new(std::io::stdin().lock());
    let mut output = std::io::stdout();

    while let Some(frame) = read_frame(&mut input) {
        let Some((id, request)) = describe(&frame) else {
            continue;
        };
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();

        if let Some(path) = journal.as_deref()
            && method == "tools/call"
        {
            record(path, &frame);
        }

        match respond(&scenario, method, &request) {
            Reply::Result(result) => send(
                &mut output,
                &json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            ),
            Reply::Failure { code, message } => send(
                &mut output,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": code, "message": message }
                }),
            ),
            Reply::Silence => {}
            Reply::Exit(code) => std::process::exit(code),
        }
        // A scenario that ends after answering a call leaves a process the host
        // has to replace, which is the state a restart is for. The answer is
        // flushed first, so the death lands after the call rather than tearing
        // the pipe the answer is travelling on. It ends only after a call: the
        // handshake is answered once and the process has to outlive it.
        if scenario == "flaky" && method == "tools/call" {
            let _ = output.flush();
            std::process::exit(0);
        }
    }
}

/// Answers one request according to the named scenario.
fn respond(scenario: &str, method: &str, request: &Value) -> Reply {
    // The handshake is answered by every scenario that gets this far, so a
    // scenario only describes what happens after it.
    if method == "initialize" {
        return handshake(scenario, request);
    }
    if method != "tools/call" {
        return Reply::Silence;
    }

    match scenario {
        // Answers both tools and writes back what it was sent.
        "echoer" | "journaler" => Reply::Result(json!({ "text": "echoed" })),
        "refuser" => Reply::Failure {
            code: -32601,
            message: "no such tool".to_owned(),
        },
        "quitter" => Reply::Exit(9),
        // Answers, and the arm below ends the process once the answer is out,
        // so the next call needs a fresh one.
        "flaky" => Reply::Result(json!({ "text": "echoed" })),
        "verbose" => Reply::Result(Value::String("x".repeat(200 * 1024))),
        "flooder" => Reply::Result(Value::String("a".repeat(1024 * 1024 + 4096))),
        // Writes a line of its own before the answer, which the host must read
        // past rather than treat as a frame.
        "chatty" => {
            println!("starting work");
            println!("{{\"level\":\"info\",\"msg\":\"about to answer\"}}");
            Reply::Result(json!({ "text": "echoed" }))
        }
        _ => Reply::Silence,
    }
}

/// Answers the handshake, or the version the scenario reports instead.
fn handshake(scenario: &str, request: &Value) -> Reply {
    // Names a version this host cannot speak.
    if scenario == "liar" {
        return Reply::Result(json!({ "protocol_version": 9 }));
    }
    // Otherwise the version the host offered is echoed, which is what a plugin
    // that agrees with the host does.
    let offered = request
        .pointer("/params/protocol_version")
        .and_then(Value::as_u64)
        .unwrap_or(u64::from(PROTOCOL_VERSION));
    Reply::Result(json!({ "protocol_version": offered }))
}

/// Appends one received frame to the record file.
fn record(path: &str, frame: &str) {
    use std::io::Write as _;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{frame}");
    }
}

/// Reads one frame, returning `None` at the end of input.
fn read_frame(input: &mut impl BufRead) -> Option<String> {
    loop {
        let mut line = String::new();
        match input.read_line(&mut line) {
            Ok(0) | Err(_) => return None,
            Ok(_) => {}
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        return Some(trimmed.to_owned());
    }
}

/// Returns the request identifier and frame, or `None` for a notification.
fn describe(frame: &str) -> Option<(u64, Value)> {
    let value: Value = serde_json::from_str(frame).ok()?;
    let id = value.get("id").and_then(Value::as_u64)?;
    Some((id, value))
}

/// Writes one frame.
fn send(output: &mut impl Write, frame: &Value) {
    let mut text = frame.to_string();
    text.push('\n');
    let _ = output.write_all(text.as_bytes());
    let _ = output.flush();
}
