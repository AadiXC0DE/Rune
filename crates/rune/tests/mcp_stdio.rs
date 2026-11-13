//! R-163: real interactive CLI, local model, and trusted stdio MCP fixtures.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::indexing_slicing)]

mod support;

/// Runs one selection of the Python fixture against the built binary.
fn run_fixture(selection: &str) {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("mcp_stdio.py")),
            env!("CARGO_BIN_EXE_rune"),
            selection,
        ])
        .output()
        .expect("python3 is required for the interactive MCP fixture");
    assert!(
        output.status.success(),
        "stdio MCP regression failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn interactive_stdio_mcp_obeys_trust_policy_and_lifecycle_bounds() {
    run_fixture("interactive");
}

#[test]
fn a_large_server_is_deferred_behind_mcp_call() {
    run_fixture("large");
}

#[test]
fn ask_connects_trusted_mcp_servers() {
    run_fixture("ask");
}

/// A stdio server offering `search` and `delete`.
const USER_SERVER: &str = r#"
import json, sys
for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    method = request["method"]
    if method == "initialize":
        result = {"protocolVersion": request["params"]["protocolVersion"], "capabilities": {"tools": {}}}
    elif method == "tools/list":
        schema = {"type": "object", "properties": {}}
        result = {"tools": [{"name": "search", "description": "Find", "inputSchema": schema},
                            {"name": "delete", "description": "Remove", "inputSchema": schema}]}
    else:
        result = {"content": [{"type": "text", "text": "done"}]}
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
"#;

/// A workspace with no trust record, a user file, and isolated roots.
struct UserWorkspace {
    dir: tempfile::TempDir,
}

impl UserWorkspace {
    fn new(user_file: &str) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("server.py"), USER_SERVER).expect("write server");
        let script = dir.path().join("server.py");
        let text = user_file.replace("{script}", &script.to_string_lossy());
        std::fs::write(dir.path().join("user.toml"), text).expect("write user file");
        std::fs::create_dir(dir.path().join("repo")).expect("repo");
        Self { dir }
    }

    fn project(&self, document: &serde_json::Value) {
        std::fs::write(self.dir.path().join("repo/.mcp.json"), document.to_string())
            .expect("write project file");
    }

    fn run(&self, args: &[&str]) -> serde_json::Value {
        use support::CommandExt as _;
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_rune"))
            .args(args)
            .current_dir(self.dir.path().join("repo"))
            .isolated_env()
            .with_external_programs()
            .env("RUNE_HOME", self.dir.path().join("state"))
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .env("XDG_DATA_HOME", self.dir.path().join("data"))
            .env("RUNE_CONFIG", self.dir.path().join("user.toml"))
            .output()
            .expect("run rune");
        assert!(output.status.success(), "{output:?}");
        serde_json::from_slice(&output.stdout).expect("JSON report")
    }
}

#[test]
fn a_user_server_connects_without_trust() {
    let workspace =
        UserWorkspace::new("[mcp_servers.local]\ncommand = \"python3\"\nargs = [\"{script}\"]\n");
    let report = workspace.run(&["mcp", "list", "--json"]);
    let servers = report["servers"].as_array().expect("servers");
    assert_eq!(servers.len(), 1, "{report}");
    assert_eq!(servers[0]["name"], "local");
    assert_eq!(servers[0]["source"], "user");
    assert_eq!(servers[0]["status"], "connected", "{report}");
}

#[test]
fn doctor_lists_a_user_mcp_server() {
    use support::CommandExt as _;
    let workspace =
        UserWorkspace::new("[mcp_servers.local]\ncommand = \"python3\"\nargs = [\"{script}\"]\n");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_rune"))
        .args(["doctor", "--json", "--offline"])
        .current_dir(workspace.dir.path().join("repo"))
        .isolated_env()
        .with_external_programs()
        .env("RUNE_HOME", workspace.dir.path().join("state"))
        .env("XDG_CONFIG_HOME", workspace.dir.path().join("config"))
        .env("XDG_DATA_HOME", workspace.dir.path().join("data"))
        .env("RUNE_CONFIG", workspace.dir.path().join("user.toml"))
        .output()
        .expect("run rune");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON report");
    let mcp = report["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .find(|check| check["name"] == "mcp")
        .expect("mcp check");
    assert_eq!(mcp["outcome"], "ok", "{report}");
    assert!(
        mcp["detail"]
            .as_str()
            .is_some_and(|detail| detail.starts_with("1 servers connected, ")),
        "{report}"
    );
}

#[test]
fn user_servers_win_name_clashes() {
    let workspace =
        UserWorkspace::new("[mcp_servers.github]\ncommand = \"python3\"\nargs = [\"{script}\"]\n");
    workspace.project(&serde_json::json!({
        "mcpServers": {
            "github": {"command": "/bin/false"},
            "other": {"command": "/bin/false"}
        }
    }));
    let report = workspace.run(&["mcp", "list", "--json"]);
    let servers = report["servers"].as_array().expect("servers");
    let names: Vec<_> = servers
        .iter()
        .map(|server| (server["name"].clone(), server["source"].clone()))
        .collect();
    assert_eq!(
        names,
        [
            (serde_json::json!("github"), serde_json::json!("user")),
            (serde_json::json!("other"), serde_json::json!("project")),
        ],
        "{report}"
    );
    assert_eq!(servers[0]["status"], "connected", "{report}");
    // The project server is still held to its trust gate.
    assert_eq!(servers[1]["status"], "blocked", "{report}");
    assert_eq!(
        report["warnings"],
        serde_json::json!([
            "MCP server github in .mcp.json has the same name as a user server and was skipped"
        ])
    );
}

#[test]
fn the_tool_allow_list_filters_tools() {
    let workspace = UserWorkspace::new(
        "[mcp_servers.local]\ncommand = \"python3\"\nargs = [\"{script}\"]\n\
         tools = [\"search\", \"missing\"]\n",
    );
    let report = workspace.run(&["mcp", "check", "--json"]);
    let check = &report["servers"][0]["check"];
    let tools: Vec<_> = check["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|tool| tool["server_tool"].as_str().expect("name").to_owned())
        .collect();
    assert_eq!(tools, ["search"], "{report}");
    assert!(
        check["warnings"]
            .as_array()
            .expect("warnings")
            .contains(&serde_json::json!("MCP server local has no tool missing")),
        "{report}"
    );
}
