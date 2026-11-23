//! R-164: interactive HTTP MCP discovery, dispatch, trust and offline gating.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::indexing_slicing)]

mod support;

#[test]
fn interactive_http_mcp_obeys_trust_policy_and_offline_mode() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("mcp_http.py")),
            env!("CARGO_BIN_EXE_rune"),
        ])
        .output()
        .expect("python3 is required for the interactive MCP fixture");
    assert!(
        output.status.success(),
        "HTTP MCP regression failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The OAuth and legacy-transport fixture, stopped when dropped.
struct Fixture {
    child: std::process::Child,
    port: u16,
}

impl Fixture {
    fn start(mode: &str) -> Self {
        use std::io::BufRead as _;
        let mut child = std::process::Command::new("python3")
            .args(["-c", include_str!("mcp_oauth_server.py"), mode])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("python3 runs the fixture");
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().expect("stdout"))
            .read_line(&mut line)
            .expect("port");
        let port = line.trim().parse().expect("port number");
        Self { child, port }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.wait();
    }
}

/// Runs the binary in a workspace whose user file is `user_file`.
fn rune_with_user_file(dir: &std::path::Path, user_file: &str) -> std::process::Command {
    use support::CommandExt as _;
    std::fs::write(dir.join("user.toml"), user_file).expect("write user file");
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).expect("repo");
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_rune"));
    command
        .current_dir(repo)
        .isolated_env()
        .with_external_programs()
        .env("RUNE_HOME", dir.join("state"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("XDG_STATE_HOME", dir.join("xdg-state"))
        .env("RUNE_CONFIG", dir.join("user.toml"))
        .env("RUNE_MCP_LOGIN_NO_BROWSER", "1");
    command
}

#[test]
fn sse_only_servers_get_a_clear_message() {
    let fixture = Fixture::start("sse");
    let dir = tempfile::tempdir().expect("tempdir");
    let output = rune_with_user_file(
        dir.path(),
        &format!(
            "[mcp_servers.legacy]\nurl = \"http://127.0.0.1:{}/mcp\"\n",
            fixture.port
        ),
    )
    .args(["mcp", "check", "--json"])
    .output()
    .expect("run rune");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON report");
    let server = &report["servers"][0];
    assert_eq!(server["source"], "user", "{report}");
    assert_eq!(server["status"], "failed", "{report}");
    assert_eq!(
        server["startup_error"]["message"],
        "MCP server legacy offers only the legacy SSE transport, which is not supported; use its Streamable HTTP endpoint",
        "{report}"
    );
    // An approved project server declared as SSE gets the same answer.
    let repo = dir.path().join("repo");
    std::fs::write(
        repo.join(".mcp.json"),
        serde_json::json!({"mcpServers": {"old": {"type": "sse", "url": "http://127.0.0.1:9/sse"}}})
            .to_string(),
    )
    .expect("write project file");
    let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_owned()).expect("utf8");
    let paths = rune_core::paths::Paths {
        state_root: root.join("state"),
        config_root: root.join("config/rune"),
        data_root: root.join("data/rune"),
    };
    let mut store = rune_policy::trust::TrustStore::new();
    store.approve_entries(
        &root.join("repo").canonicalize_utf8().expect("canonical"),
        &["server:old".to_owned()],
    );
    store.save(&paths.trust_file()).expect("save trust");
    let output = rune_with_user_file(dir.path(), "")
        .args(["mcp", "check", "old", "--json"])
        .output()
        .expect("run rune");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON report");
    assert_eq!(report["servers"][0]["source"], "project", "{report}");
    assert_eq!(
        report["servers"][0]["startup_error"]["message"],
        "MCP server old offers only the legacy SSE transport, which is not supported; use its Streamable HTTP endpoint",
        "{report}"
    );
}

/// Tokens go to the system keychain on macOS, which a test must not touch, so
/// the whole-binary login runs where tokens are kept in private files.
#[cfg(not(target_os = "macos"))]
#[test]
fn oauth_login_stores_and_refreshes_tokens() {
    use std::io::{BufRead as _, Read as _, Write as _};
    let fixture = Fixture::start("oauth");
    let dir = tempfile::tempdir().expect("tempdir");
    let user_file = format!(
        "[mcp_servers.remote]\nurl = \"http://127.0.0.1:{}/mcp\"\n",
        fixture.port
    );
    let raw = |port: u16, method: &str, target: &str, body: &str| {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
        write!(
            stream,
            "{method} {target} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .expect("send");
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        response
    };
    let mut login = rune_with_user_file(dir.path(), &user_file)
        .args(["mcp", "login", "remote"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("run rune");
    let mut stdout = std::io::BufReader::new(login.stdout.take().expect("stdout"));
    let mut line = String::new();
    stdout.read_line(&mut line).expect("prompt");
    assert_eq!(line, "open this address to log in to remote:\n");
    line.clear();
    stdout.read_line(&mut line).expect("address");
    let prefix = format!("http://127.0.0.1:{}", fixture.port);
    let path = line
        .trim()
        .strip_prefix(&prefix)
        .expect("authorization endpoint");
    let redirect = raw(fixture.port, "GET", path, "");
    let location = redirect
        .lines()
        .find_map(|line| line.strip_prefix("Location: "))
        .expect("redirect")
        .trim()
        .to_owned();
    let callback = location
        .strip_prefix("http://127.0.0.1:")
        .expect("loopback");
    let (port, target) = callback.split_at(callback.find('/').expect("path"));
    assert!(raw(port.parse().expect("port"), "GET", target, "").starts_with("HTTP/1.1 200"));
    let mut rest = String::new();
    stdout.read_to_string(&mut rest).expect("rest");
    assert!(login.wait().expect("wait").success());
    assert_eq!(rest, "logged in to remote\n");
    let tokens = std::fs::read_dir(dir.path().join("state/mcp-oauth"))
        .expect("token directory")
        .count();
    assert!(tokens >= 2, "a login record and a token file");

    // The stored token is sent; a later 401 costs exactly one refresh.
    raw(fixture.port, "POST", "/expire", "");
    let output = rune_with_user_file(dir.path(), &user_file)
        .args(["mcp", "check", "--json"])
        .output()
        .expect("run rune");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON report");
    assert_eq!(report["servers"][0]["status"], "connected", "{report}");
    let stats = raw(fixture.port, "GET", "/stats", "");
    assert!(stats.contains("\"refreshes\": 1"), "{stats}");
}
