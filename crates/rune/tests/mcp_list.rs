//! Exercise MCP listing through the real CLI with isolated trust and fixtures.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic
)]

mod support;
use support::CommandExt as _;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Output};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use camino::Utf8Path;
use rune_core::paths::Paths;
use rune_policy::trust::TrustStore;
use serde_json::{Value, json};

struct Workspace {
    dir: tempfile::TempDir,
    paths: Paths,
}

impl Workspace {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = Paths {
            state_root: root.join("state"),
            config_root: root.join("config/rune"),
            data_root: root.join("data/rune"),
        };
        Self { dir, paths }
    }

    fn configure(&self, entries: &Value) {
        std::fs::write(
            self.dir.path().join(".mcp.json"),
            json!({"mcpServers": entries}).to_string(),
        )
        .expect("write config");
    }

    fn approve(&self, names: &[&str]) {
        let mut store = TrustStore::new();
        store.approve_entries(
            &Utf8Path::from_path(self.dir.path())
                .expect("utf8")
                .canonicalize_utf8()
                .expect("canonical"),
            &names
                .iter()
                .map(|name| format!("server:{name}"))
                .collect::<Vec<_>>(),
        );
        store.save(&self.paths.trust_file()).expect("save trust");
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with_env(args, &[])
    }

    fn run_with_env(&self, args: &[&str], variables: &[(&str, &str)]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rune"))
            .args(args)
            .current_dir(self.dir.path())
            .isolated_env()
            .with_external_programs()
            .env("RUNE_HOME", &self.paths.state_root)
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .env("XDG_DATA_HOME", self.dir.path().join("data"))
            .env("RUNE_CONFIG", self.dir.path().join("absent.toml"))
            .env("RUNE_MCP_LIST_SECRET", "expanded-header-secret")
            .envs(variables.iter().copied())
            .output()
            .expect("run rune")
    }

    fn list(&self, flags: &[&str]) -> Value {
        let mut args = vec!["mcp", "list", "--json"];
        args.extend_from_slice(flags);
        let output = self.run(&args);
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        serde_json::from_slice(&output.stdout).expect("JSON report")
    }
}

/// Minimal HTTP MCP fixture. An error reflects the received secret back to Rune
/// to prove diagnostics redact it, including in a server-supplied message.
fn http_fixture(fail: bool) -> (String, JoinHandle<Vec<String>>) {
    http_scenario(if fail {
        Scenario::Fail
    } else {
        Scenario::Empty
    })
}

#[derive(Clone, Copy)]
enum Scenario {
    Fail,
    Empty,
    Paged,
    StartupTimeout,
    ToolsTimeout,
}

fn http_scenario(scenario: Scenario) -> (String, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let url = format!("http://{}/mcp", listener.local_addr().expect("address"));
    let task = std::thread::spawn(move || {
        let count = match scenario {
            Scenario::Fail | Scenario::StartupTimeout => 1,
            Scenario::Paged => 4,
            Scenario::Empty | Scenario::ToolsTimeout => 3,
        };
        let mut methods = Vec::new();
        for _ in 0..count {
            let deadline = Instant::now()
                .checked_add(Duration::from_secs(10))
                .expect("deadline");
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "fixture was not contacted");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            // `accept` inherits `O_NONBLOCK` from a nonblocking listener on
            // macOS, unlike Linux; the read deadline below is the real bound, so
            // the accepted socket must be blocking before it is read.
            socket
                .set_nonblocking(false)
                .expect("blocking request socket");
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("read deadline");
            let mut reader = BufReader::new(socket.try_clone().expect("clone"));
            let mut length = 0;
            let mut secret_seen = false;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("request line");
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse::<usize>().expect("length");
                }
                secret_seen |= line.contains("expanded-header-secret");
            }
            assert!(secret_seen, "header template was not resolved");
            let mut body = vec![0; length];
            reader.read_exact(&mut body).expect("request body");
            let request: Value = serde_json::from_slice(&body).expect("request JSON");
            let method = request["method"].as_str().expect("method");
            methods.push(method.to_owned());
            if matches!(scenario, Scenario::StartupTimeout)
                || (matches!(scenario, Scenario::ToolsTimeout) && method == "tools/list")
            {
                // Hold the socket without headers so the client's configured
                // deadline, rather than an EOF, must end the check.
                std::thread::sleep(Duration::from_millis(800));
                return methods;
            }
            let (status, reply) = if matches!(scenario, Scenario::Fail) {
                (
                    "200 OK",
                    json!({"jsonrpc": "2.0", "id": request["id"], "error": {
                        "code": -32603, "message": "startup rejected expanded-header-secret"
                    }})
                    .to_string(),
                )
            } else if request["method"] == "notifications/initialized" {
                ("202 Accepted", String::new())
            } else {
                let result = if method == "initialize" {
                    json!({"protocolVersion": "2025-11-25", "capabilities": {"tools": {}}})
                } else {
                    assert_eq!(method, "tools/list");
                    if matches!(scenario, Scenario::Paged) {
                        if request["params"]["cursor"] == "page-two" {
                            json!({"tools": [{"name": "second", "inputSchema": {"type": "object"}}]})
                        } else {
                            assert!(request["params"].get("cursor").is_none());
                            json!({"tools": [{"name": "first", "inputSchema": {"type": "object"}}], "nextCursor": "page-two"})
                        }
                    } else {
                        json!({"tools": []})
                    }
                };
                (
                    "200 OK",
                    json!({"jsonrpc": "2.0", "id": request["id"], "result": result}).to_string(),
                )
            };
            write!(socket, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).expect("reply");
        }
        methods
    });
    (url, task)
}

#[test]
fn empty_listing_and_help_work_without_a_provider() {
    let workspace = Workspace::new();
    assert_eq!(workspace.list(&[])["servers"], json!([]));
    assert!(!workspace.paths.state_root.exists());
    let text = workspace.run(&["mcp", "list"]);
    assert!(text.status.success());
    assert_eq!(text.stdout, b"no MCP servers configured\n");
    let help = workspace.run(&["mcp", "--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("rune mcp list [--json]"));
    assert!(workspace.run(&["mcp", "check"]).status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("rune mcp check [server] [--json]"));
    for args in [
        &["mcp"][..],
        &["mcp", "check", "unknown"],
        &["mcp", "check", "one", "two"],
        &["mcp", "list", "extra"],
    ] {
        assert!(!workspace.run(args).status.success(), "{args:?}");
    }
}

#[test]
fn every_eligible_startup_is_reported_even_after_required_failure_without_secrets() {
    let workspace = Workspace::new();
    let (failed_url, failed) = http_fixture(true);
    let (healthy_url, healthy) = http_fixture(false);
    workspace.configure(&json!({
        "z_pending": {"command": "${MISSING_PENDING_VARIABLE}", "env": {"KEY": "pending-env-secret"}},
        "c_healthy": {"url": healthy_url, "headers": {"x-api-key": "${RUNE_MCP_LIST_SECRET}"}},
        "b_missing": {"command": "/rune-mcp-list-no-such-program", "env": {"KEY": "child-env-secret"}},
        "a_required": {"type": "http", "url": failed_url, "required": true, "headers": {"x-api-key": "${RUNE_MCP_LIST_SECRET}"}},
        "d_disabled": {"command": "/rune-mcp-list-no-such-program", "enabled": false},
        "e_expansion": {"command": "${MISSING_APPROVED_VARIABLE}"}
    }));
    workspace.approve(&[
        "a_required",
        "b_missing",
        "c_healthy",
        "d_disabled",
        "e_expansion",
    ]);
    let report = workspace.list(&[]);
    failed.join().expect("failed fixture");
    healthy.join().expect("healthy fixture");
    let servers = report["servers"].as_array().expect("servers");
    assert_eq!(
        servers
            .iter()
            .map(|server| server["name"].as_str().expect("name"))
            .collect::<Vec<_>>(),
        [
            "a_required",
            "b_missing",
            "c_healthy",
            "d_disabled",
            "e_expansion",
            "z_pending"
        ]
    );
    assert_eq!(servers[0]["transport"], "http");
    assert_eq!(servers[0]["trust"], "approved");
    assert_eq!(servers[0]["required"], true);
    assert_eq!(servers[0]["status"], "failed");
    assert!(
        servers[0]["startup_error"]["message"]
            .as_str()
            .expect("message")
            .contains("<redacted>")
    );
    assert_eq!(servers[1]["transport"], "stdio");
    assert_eq!(servers[1]["status"], "failed");
    assert!(servers[1]["startup_error"]["code"].is_string());
    assert_eq!(servers[2]["status"], "connected");
    assert!(servers[2]["startup_error"].is_null());
    assert!(servers[2].get("check").is_none());
    assert_eq!(servers[3]["status"], "disabled");
    assert_eq!(servers[4]["status"], "failed");
    assert_eq!(servers[5]["trust"], "pending");
    assert_eq!(servers[5]["status"], "blocked");
    assert!(servers[5]["startup_error"].is_null());
    let text = report.to_string();
    for secret in [
        "expanded-header-secret",
        "pending-env-secret",
        "child-env-secret",
        "x-api-key",
    ] {
        assert!(!text.contains(secret), "leaked {secret}: {text}");
    }
}

#[test]
fn pending_rejected_disabled_and_offline_servers_are_never_contacted() {
    let workspace = Workspace::new();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    workspace.configure(&json!({"remote": {"type": "sse", "url": format!("http://{}/mcp", listener.local_addr().expect("address")), "headers": {"secret": "${MISSING}"}}}));
    assert_eq!(workspace.list(&[])["servers"][0]["trust"], "pending");
    let rejected = workspace.run(&["projects", "reject"]);
    assert!(rejected.status.success(), "{rejected:?}");
    assert_eq!(workspace.list(&[])["servers"][0]["trust"], "rejected");
    let approved = workspace.run(&["projects", "approve"]);
    assert!(approved.status.success(), "{approved:?}");
    let report = workspace.list(&["--offline"]);
    assert_eq!(report["servers"][0]["trust"], "approved");
    assert_eq!(report["servers"][0]["transport"], "sse");
    assert_eq!(report["servers"][0]["status"], "offline");
    assert!(report["servers"][0]["startup_error"].is_null());
    workspace.configure(&json!({"remote": {"url": "http://127.0.0.1:1", "enabled": false}}));
    assert_eq!(workspace.list(&[])["servers"][0]["status"], "disabled");
    assert_eq!(
        listener.accept().expect_err("no requests").kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[cfg(unix)]
#[test]
fn startup_timeout_is_reported_in_text_and_json_and_untrusted_commands_do_not_run() {
    let workspace = Workspace::new();
    workspace.configure(&json!({
        "hang": {"command": "sh", "args": ["-c", "sleep 30"]},
        "pending": {"command": "sh", "args": ["-c", "touch should-not-exist"]}
    }));
    workspace.approve(&["hang"]);
    let report = workspace.list(&["--limit", "mcp_startup_timeout_ms=50"]);
    assert_eq!(report["servers"][0]["startup_error"]["code"], "timeout");
    assert!(!workspace.dir.path().join("should-not-exist").exists());
    let output = workspace.run(&["mcp", "list", "--limit", "mcp_startup_timeout_ms=50"]);
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).expect("text");
    assert!(text.contains("hang  stdio  approved  failed"), "{text}");
    assert!(text.contains("timeout:"), "{text}");
    assert!(text.contains("pending  stdio  pending  blocked"), "{text}");
}

#[test]
fn malformed_declarations_do_not_echo_secret_values_or_hide_other_errors() {
    let workspace = Workspace::new();
    workspace.configure(&json!({
        "bad_type": {"type": "credential-in-invalid-type", "url": "https://example.test"},
        "bad_header": {"url": "https://example.test", "headers": {"x-secret": ["credential-in-invalid-header"]}}
    }));
    workspace.approve(&["bad_type", "bad_header"]);
    let report = workspace.list(&[]);
    for server in report["servers"].as_array().expect("servers") {
        assert_eq!(server["status"], "failed");
        assert_eq!(server["startup_error"]["code"], "invalid_configuration");
    }
    let text = workspace.run(&["mcp", "list"]);
    assert!(text.status.success(), "{text:?}");
    for output in [
        report.to_string(),
        String::from_utf8(text.stdout).expect("text"),
    ] {
        assert!(!output.contains("credential-in-invalid"), "{output}");
    }
}

#[cfg(unix)]
#[test]
fn approved_stdio_can_start_during_offline_listing() {
    let workspace = Workspace::new();
    let script = r#"printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25"}}' '{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}'; cat >/dev/null"#;
    workspace.configure(&json!({"local": {"command": "sh", "args": ["-c", script]}}));
    workspace.approve(&["local"]);
    let report = workspace.list(&["--offline"]);
    assert_eq!(report["servers"][0]["transport"], "stdio");
    assert_eq!(report["servers"][0]["status"], "connected");
    assert!(report["servers"][0]["startup_error"].is_null());
}

#[cfg(target_os = "linux")]
#[test]
fn offline_stdio_mcp_can_send_network_traffic() {
    let workspace = Workspace::new();
    let listener = TcpListener::bind("127.0.0.1:0").expect("fixture");
    listener.set_nonblocking(true).expect("nonblocking");
    let port = listener.local_addr().expect("address").port();
    // Send bytes before the MCP handshake. A connected report therefore proves
    // startup did not apply the shell tool's offline network sandbox.
    let script = format!(
        r#"printf LOCAL_MCP_EGRESS > /dev/tcp/127.0.0.1/{port} || exit 1; printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"2025-11-25"}}}}' '{{"jsonrpc":"2.0","id":2,"result":{{"tools":[]}}}}'; cat >/dev/null"#
    );
    workspace.configure(&json!({"local": {"command": "/bin/bash", "args": ["-c", script]}}));
    workspace.approve(&["local"]);
    for command in ["list", "check"] {
        let output = workspace.run(&["mcp", command, "--json", "--offline"]);
        assert!(output.status.success(), "{output:?}");
        let report: Value = serde_json::from_slice(&output.stdout).expect("report");
        assert_eq!(report["servers"][0]["status"], "connected");
        let (mut socket, _) = listener.accept().expect("stdio child connection");
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("read deadline");
        let mut received = String::new();
        socket.read_to_string(&mut received).expect("fixture bytes");
        assert_eq!(received, "LOCAL_MCP_EGRESS");
    }
}

#[test]
fn check_reports_protocol_and_all_tools_without_model_requests_or_sessions() {
    for json_output in [true, false] {
        let workspace = Workspace::new();
        let (url, task) = http_scenario(Scenario::Paged);
        workspace.configure(&json!({
            "local": {"url": url, "headers": {"x-api-key": "${RUNE_MCP_LIST_SECRET}"}},
            "unselected": {"command": "${MISSING}"}
        }));
        workspace.approve(&["local"]);
        let model = TcpListener::bind("127.0.0.1:0").expect("model listener");
        model.set_nonblocking(true).expect("nonblocking");
        let model_url = format!("http://{}/v1", model.local_addr().expect("address"));
        let mut args = vec!["mcp", "check", "local"];
        if json_output {
            args.push("--json");
        }
        let output = workspace.run_with_env(
            &args,
            &[
                ("RUNE_BASE_URL", &model_url),
                ("OPENAI_API_KEY", "model-fixture-key"),
            ],
        );
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        assert_eq!(
            task.join().expect("MCP fixture"),
            [
                "initialize",
                "notifications/initialized",
                "tools/list",
                "tools/list"
            ]
        );
        assert_eq!(
            model.accept().expect_err("no model request").kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert!(!workspace.paths.sessions_dir().exists());
        let text = String::from_utf8(output.stdout).expect("text");
        if json_output {
            let report: Value = serde_json::from_str(&text).expect("report");
            assert_eq!(report["servers"].as_array().expect("servers").len(), 1);
            let server = &report["servers"][0];
            assert_eq!(server["status"], "connected");
            assert_eq!(server["check"]["protocol_version"], "2025-11-25");
            assert_eq!(
                server["check"]["tools"],
                json!([
                    {"name": "mcp_local_first", "server_tool": "first"},
                    {"name": "mcp_local_second", "server_tool": "second"}
                ])
            );
            assert_eq!(server["check"]["warnings"], json!([]));
            assert_eq!(server["check"]["timed_out"], false);
            assert_eq!(server["check"]["startup_timeout_ms"], 30_000);
            assert_eq!(server["check"]["operation_timeout_ms"], 60_000);
        } else {
            assert!(text.contains("local  http  approved  connected"), "{text}");
            assert!(text.contains("protocol: 2025-11-25; tools: 2"), "{text}");
            assert!(text.contains("first (mcp_local_first)"), "{text}");
            assert!(text.contains("second (mcp_local_second)"), "{text}");
            assert!(text.contains("timed_out=false"), "{text}");
        }
        assert!(!text.contains("expanded-header-secret"), "{text}");
        assert!(!text.contains("model-fixture-key"), "{text}");
    }
}

#[test]
fn check_reports_startup_and_discovery_timeouts_and_checks_later_servers() {
    for scenario in [Scenario::StartupTimeout, Scenario::ToolsTimeout] {
        for json_output in [true, false] {
            let workspace = Workspace::new();
            let (slow_url, slow) = http_scenario(scenario);
            let (healthy_url, healthy) = http_fixture(false);
            workspace.configure(&json!({
                "a_slow": {"url": slow_url, "headers": {"x-api-key": "${RUNE_MCP_LIST_SECRET}"}, "required": true},
                "b_healthy": {"url": healthy_url, "headers": {"x-api-key": "${RUNE_MCP_LIST_SECRET}"}}
            }));
            workspace.approve(&["a_slow", "b_healthy"]);
            let mut args = vec![
                "mcp",
                "check",
                "--limit",
                "mcp_startup_timeout_ms=200",
                "--limit",
                "mcp_operation_timeout_ms=200",
            ];
            if json_output {
                args.push("--json");
            }
            let output = workspace.run(&args);
            assert_eq!(output.status.code(), Some(1), "{output:?}");
            assert!(output.stderr.is_empty(), "{output:?}");
            let methods = slow.join().expect("slow fixture");
            assert_eq!(
                methods.last().expect("method"),
                if matches!(scenario, Scenario::StartupTimeout) {
                    "initialize"
                } else {
                    "tools/list"
                }
            );
            healthy.join().expect("healthy fixture");
            let text = String::from_utf8(output.stdout).expect("text");
            if json_output {
                let report: Value = serde_json::from_str(&text).expect("report");
                let slow = &report["servers"][0];
                assert_eq!(slow["status"], "failed");
                assert_eq!(slow["startup_error"]["code"], "timeout");
                assert_eq!(slow["check"]["timed_out"], true);
                assert_eq!(slow["check"]["startup_timeout_ms"], 200);
                assert_eq!(slow["check"]["operation_timeout_ms"], 200);
                assert_eq!(report["servers"][1]["status"], "connected");
                assert_eq!(report["servers"][1]["check"]["timed_out"], false);
            } else {
                assert!(text.contains("a_slow  http  approved  failed"), "{text}");
                assert!(text.contains("timeout:"), "{text}");
                assert!(text.contains("timed_out=true"), "{text}");
                assert!(text.contains("startup=200 ms; operation=200 ms"), "{text}");
                assert!(
                    text.contains("b_healthy  http  approved  connected"),
                    "{text}"
                );
            }
        }
    }
}

#[test]
fn check_respects_trust_disabled_and_offline_states_without_contact_or_expansion() {
    let workspace = Workspace::new();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    listener.set_nonblocking(true).expect("nonblocking");
    workspace.configure(&json!({
        "remote": {"url": format!("http://{}/mcp", listener.local_addr().expect("address")), "headers": {"x-secret": "${MISSING}"}}
    }));
    let assert_state = |flags: &[&str], expected: &str, trust: &str| {
        let mut args = vec!["mcp", "check", "remote", "--json"];
        args.extend_from_slice(flags);
        let output = workspace.run(&args);
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let report: Value = serde_json::from_slice(&output.stdout).expect("report");
        assert_eq!(report["servers"][0]["status"], expected);
        assert_eq!(report["servers"][0]["trust"], trust);
        assert!(report["servers"][0]["startup_error"].is_null());
        assert_eq!(report["servers"][0]["check"]["timed_out"], false);
    };
    assert_state(&[], "blocked", "pending");
    assert!(workspace.run(&["projects", "reject"]).status.success());
    assert_state(&[], "blocked", "rejected");
    workspace.approve(&["remote"]);
    assert_state(&["--offline"], "offline", "approved");
    workspace.configure(&json!({"remote": {"command": "${MISSING}", "enabled": false}}));
    assert_state(&[], "disabled", "approved");
    assert_eq!(
        listener.accept().expect_err("no contact").kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn check_unknown_name_does_not_start_other_servers_and_errors_are_redacted() {
    let workspace = Workspace::new();
    let (url, task) = http_fixture(true);
    workspace.configure(
        &json!({"local": {"url": url, "headers": {"x-api-key": "${RUNE_MCP_LIST_SECRET}"}}}),
    );
    workspace.approve(&["local"]);
    let unknown = workspace.run(&["mcp", "check", "missing", "--json"]);
    assert_eq!(unknown.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("no MCP server named"));
    let output = workspace.run(&["mcp", "check", "local", "--json"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(task.join().expect("fixture"), ["initialize"]);
    let report: Value = serde_json::from_slice(&output.stdout).expect("report");
    assert_eq!(report["servers"][0]["check"]["timed_out"], false);
    let text = report.to_string();
    assert!(text.contains("<redacted>"), "{text}");
    assert!(!text.contains("expanded-header-secret"), "{text}");
}

#[cfg(unix)]
#[test]
fn check_local_stdio_works_offline_without_a_provider() {
    let workspace = Workspace::new();
    let script = r#"printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}}' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}}'; cat >/dev/null"#;
    workspace.configure(&json!({"local": {"command": "sh", "args": ["-c", script]}}));
    workspace.approve(&["local"]);
    let output = workspace.run(&["mcp", "check", "local", "--json", "--offline"]);
    assert!(output.status.success(), "{output:?}");
    let report: Value = serde_json::from_slice(&output.stdout).expect("report");
    assert_eq!(report["servers"][0]["status"], "connected");
    assert_eq!(
        report["servers"][0]["check"]["protocol_version"],
        "2024-11-05"
    );
    assert_eq!(
        report["servers"][0]["check"]["tools"][0]["name"],
        "mcp_local_echo"
    );
    assert!(!workspace.paths.sessions_dir().exists());
}
