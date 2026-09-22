//! A scripted MCP server used by the client's integration tests.
//!
//! The client drives this as a child process and speaks the protocol over its
//! standard input and output, so the tests exercise real framing, real timeouts,
//! and real process handling rather than a stub. The scenario is named on the
//! command line, which keeps every case in one file and needs no shell, so the
//! tests run wherever the client does.

// The fixture asserts by panicking when it cannot do its job, which is a
// failure of the test rather than of the code under test.
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::io::{BufRead, BufReader, Write};
use std::time::Duration;

use serde_json::{Value, json};

/// What the fixture does with one request.
enum Reply {
    /// Answers with a result.
    Result(Value),
    /// Answers with a JSON-RPC failure.
    Failure { code: i64, message: String },
    /// Sends nothing, which is how a notification is answered.
    Silence,
    /// Closes the stream.
    Exit,
}

fn main() {
    let mut arguments = std::env::args().skip(1);
    let scenario = arguments.next().unwrap_or_default();
    let marker = arguments.next();

    // The descendant writes an increasing count to the file it is given, so a
    // test can tell it is running without asking the platform for a process
    // list, which is not available everywhere.
    if scenario == "hold" {
        let path = marker.unwrap_or_default();
        let mut count = 0_u64;
        loop {
            let _ = std::fs::write(&path, count.to_string());
            count = count.saturating_add(1);
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    // One descendant per server process, so a leaked process is attributable to
    // the server rather than to one request.
    if scenario == "grouped"
        && let Some(marker) = marker.as_deref()
    {
        spawn_descendant(marker);
    }

    let mut input = BufReader::new(std::io::stdin().lock());
    let mut output = std::io::stdout();

    while let Some(frame) = read_frame(&mut input) {
        let Some((id, request)) = describe(&frame) else {
            continue;
        };
        match respond(&scenario, &request) {
            Reply::Result(result) => {
                send(
                    &mut output,
                    &json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                );
            }
            Reply::Failure { code, message } => {
                send(
                    &mut output,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": code, "message": message }
                    }),
                );
            }
            Reply::Silence => {}
            Reply::Exit => return,
        }
    }
}

/// Answers one request according to the named scenario.
fn respond(scenario: &str, request: &Value) -> Reply {
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();

    match scenario {
        "standard" => standard(method),
        "grouped" => standard(method),
        "paged" => paged(request, method),
        "mixed" => match method {
            "initialize" => Reply::Result(json!({ "protocolVersion": "2025-11-25" })),
            "tools/list" => Reply::Result(json!({
                "tools": [
                    { "name": "good", "inputSchema": { "type": "object" } },
                    { "name": "nameless-schema" },
                    { "name": "", "inputSchema": { "type": "object" } },
                    { "name": "good", "inputSchema": { "type": "object" } }
                ]
            })),
            _ => Reply::Silence,
        },
        "search-budget" => match method {
            "initialize" => Reply::Result(json!({ "protocolVersion": "2025-11-25" })),
            "tools/list" => Reply::Result(json!({
                "tools": [{
                    "name": "big",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "a": {
                                "type": "string",
                                "description": "padding padding padding padding padding padding padding padding"
                            }
                        }
                    }
                }]
            })),
            _ => Reply::Silence,
        },
        "huge" => match method {
            "initialize" => Reply::Result(json!({ "protocolVersion": "2025-11-25" })),
            "tools/list" => Reply::Result(json!({
                "tools": [{
                    "name": "huge",
                    "inputSchema": { "type": "object", "description": "a".repeat(9 * 1024 * 1024) }
                }]
            })),
            _ => Reply::Silence,
        },
        "silent" => match method {
            "initialize" => {
                std::thread::sleep(Duration::from_secs(30));
                Reply::Result(json!({ "protocolVersion": "2025-11-25" }))
            }
            _ => Reply::Silence,
        },
        "hung-call" => match method {
            "initialize" => Reply::Result(json!({ "protocolVersion": "2025-11-25" })),
            "tools/list" => Reply::Result(json!({
                "tools": [{ "name": "slow", "inputSchema": { "type": "object" } }]
            })),
            "tools/call" => {
                std::thread::sleep(Duration::from_secs(30));
                Reply::Result(json!({ "content": [] }))
            }
            _ => Reply::Silence,
        },
        "vanishing" => match method {
            "initialize" => Reply::Result(json!({ "protocolVersion": "2025-11-25" })),
            "tools/list" => Reply::Result(json!({
                "tools": [{ "name": "gone", "inputSchema": { "type": "object" } }]
            })),
            _ => Reply::Silence,
        },
        "flaky-credential" => match method {
            "initialize" => Reply::Result(json!({
                "protocolVersion": "2025-11-25",
                "_meta": { "expires_at_ms": null }
            })),
            "tools/list" => Reply::Result(json!({
                "tools": [{
                    "name": "greet",
                    "description": "Greets.",
                    "inputSchema": { "type": "object", "properties": { "who": { "type": "string" } } }
                }]
            })),
            "tools/call" => flaky_call(),
            _ => Reply::Silence,
        },
        "refuse-two" => match method {
            "initialize" => {
                let requested = request
                    .pointer("/params/protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if matches!(requested, "2025-11-25" | "2025-06-18") {
                    Reply::Failure {
                        code: -32602,
                        message: "unsupported protocol revision".to_owned(),
                    }
                } else {
                    Reply::Result(json!({ "protocolVersion": "2025-03-26" }))
                }
            }
            "tools/list" => Reply::Result(json!({
                "tools": [{ "name": "greet", "inputSchema": { "type": "object" } }]
            })),
            _ => Reply::Silence,
        },
        "future" => match method {
            "initialize" => Reply::Result(json!({ "protocolVersion": "2026-07-28" })),
            _ => Reply::Silence,
        },
        "dead" => match method {
            "initialize" => Reply::Exit,
            _ => Reply::Silence,
        },
        "stale-credential" => match method {
            "initialize" => Reply::Result(json!({
                "protocolVersion": "2025-11-25",
                "_meta": { "expires_at_ms": 1 }
            })),
            "tools/list" => Reply::Result(json!({
                "tools": [{ "name": "greet", "inputSchema": { "type": "object" } }]
            })),
            "tools/call" => Reply::Result(json!({
                "content": [{ "type": "text", "text": "ok" }],
                "isError": false
            })),
            _ => Reply::Silence,
        },
        "denied-credential" => match method {
            "initialize" => Reply::Result(json!({
                "protocolVersion": "2025-11-25",
                "_meta": { "expires_at_ms": null }
            })),
            "tools/list" => Reply::Result(json!({
                "tools": [{ "name": "greet", "inputSchema": { "type": "object" } }]
            })),
            "tools/call" => expired(),
            _ => Reply::Silence,
        },
        _ => Reply::Silence,
    }
}

/// The two-tool server every ordinary scenario answers with.
fn standard(method: &str) -> Reply {
    match method {
        "initialize" => Reply::Result(json!({
            "protocolVersion": "2025-06-18",
            "capabilities": { "tools": {} },
            "_meta": { "expires_at_ms": null }
        })),
        "tools/list" => Reply::Result(json!({
            "tools": [
                {
                    "name": "greet",
                    "description": "Greets.",
                    "inputSchema": {
                        "type": "object",
                        "properties": { "who": { "type": "string" } },
                        "required": ["who"]
                    }
                },
                { "name": "ping", "inputSchema": { "type": "object" } }
            ]
        })),
        "tools/call" => Reply::Result(json!({
            "content": [{ "type": "text", "text": "hello from the fixture" }],
            "isError": false
        })),
        _ => Reply::Silence,
    }
}

/// Answers a listing that continues on a second page.
fn paged(request: &Value, method: &str) -> Reply {
    match method {
        "initialize" => Reply::Result(json!({ "protocolVersion": "2025-11-25" })),
        "tools/list" => {
            let cursor = request
                .pointer("/params/cursor")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if cursor == "page2" {
                Reply::Result(json!({
                    "tools": [{ "name": "second", "inputSchema": { "type": "object" } }]
                }))
            } else {
                Reply::Result(json!({
                    "tools": [{ "name": "first", "inputSchema": { "type": "object" } }],
                    "nextCursor": "page2"
                }))
            }
        }
        _ => Reply::Silence,
    }
}

/// Reports an expired credential on the first call and answers afterwards.
///
/// The count is kept in a file named by the environment, so one server process
/// can tell its first call from the rest across a restart.
fn flaky_call() -> Reply {
    let path = std::env::var("MCP_FIXTURE_COUNT").unwrap_or_default();
    let seen = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
        .unwrap_or(0);
    if seen == 0 {
        let _ = std::fs::write(&path, "1");
        return expired();
    }
    Reply::Result(json!({
        "content": [{ "type": "text", "text": "hello from the fixture" }],
        "isError": false
    }))
}

/// The failure a server reports for a credential it will not accept.
fn expired() -> Reply {
    Reply::Failure {
        code: -32001,
        message: "credential expired".to_owned(),
    }
}

/// Starts a background process that writes a heartbeat to `marker`.
///
/// The process is this binary in its holding scenario, so the marker is a path
/// rather than something a test has to find in a process listing.
fn spawn_descendant(marker: &str) {
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let _ = std::process::Command::new(executable)
        .args(["hold", marker])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
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
