//! Import external config through the CLI, then exercise the project trust gate.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

mod support;
use support::CommandExt as _;

use std::process::{Command, Output};

use serde_json::{Value, json};

const FIXTURE: &str = include_str!("fixtures/claude_desktop_config.json");

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::write(dir.path().join("desktop.json"), FIXTURE).expect("external config");
        Self(dir)
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rune"))
            .args(args)
            .current_dir(self.0.path())
            .isolated_env()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("RUNE_HOME", self.0.path().join("state"))
            .env("XDG_CONFIG_HOME", self.0.path().join("config"))
            .env("XDG_DATA_HOME", self.0.path().join("data"))
            .env("RUNE_CONFIG", self.0.path().join("absent.toml"))
            .output()
            .expect("run rune")
    }

    fn import(&self) -> Output {
        self.run(&["mcp", "import", "claude-desktop", "desktop.json"])
    }
}

#[test]
fn fixture_import_emits_reviewable_project_json_without_writes_or_execution() {
    let workspace = Workspace::new();
    let project = workspace.0.path().join(".mcp.json");
    let existing = b"{\"mcpServers\":{}}\n";
    std::fs::write(&project, existing).expect("existing project config");
    let output = workspace.import();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let imported: Value = serde_json::from_slice(&output.stdout).expect("project JSON");
    let external: Value = serde_json::from_str(FIXTURE).expect("fixture JSON");
    let mut expected = external["mcpServers"].clone();
    expected["fixture"]["type"] = json!("stdio");
    assert_eq!(imported, json!({"mcpServers": expected}));
    rune_context::mcp_trust::parse_project_file(
        std::str::from_utf8(&output.stdout).expect("UTF-8"),
        camino::Utf8Path::from_path(&project).expect("path"),
    )
    .expect("Rune project parser accepts output");
    assert_eq!(
        std::fs::read(&project).expect("project unchanged"),
        existing
    );
    assert_eq!(
        std::fs::read_to_string(workspace.0.path().join("desktop.json")).expect("source"),
        FIXTURE
    );
    assert!(!workspace.0.path().join("import-started").exists());
    assert!(!workspace.0.path().join("state").exists());
    let json_output = workspace.run(&[
        "mcp",
        "import",
        "claude-desktop",
        "desktop.json",
        "--json",
        "--offline",
    ]);
    assert!(json_output.status.success(), "{json_output:?}");
    assert_eq!(output.stdout, json_output.stdout);
}

#[cfg(unix)]
#[test]
fn imported_project_command_requires_trust_even_in_full_access_mode() {
    let workspace = Workspace::new();
    let imported = workspace.import();
    assert!(imported.status.success(), "{imported:?}");
    std::fs::write(workspace.0.path().join(".mcp.json"), imported.stdout)
        .expect("apply reviewed config");
    let marker = workspace.0.path().join("import-started");
    let blocked = workspace.run(&["mcp", "check", "--json", "--permission-mode", "full-access"]);
    assert_eq!(blocked.status.code(), Some(1), "{blocked:?}");
    let report: Value = serde_json::from_slice(&blocked.stdout).expect("report");
    assert_eq!(report["servers"][0]["trust"], "pending");
    assert_eq!(report["servers"][0]["status"], "blocked");
    assert!(!marker.exists());
    assert!(!workspace.0.path().join("state").exists());

    let approval = workspace.run(&["projects", "approve"]);
    assert!(approval.status.success(), "{approval:?}");
    let before_import =
        std::fs::read(workspace.0.path().join("state/trust.json")).expect("trust bytes");
    assert!(workspace.import().status.success());
    assert_eq!(
        std::fs::read(workspace.0.path().join("state/trust.json")).expect("trust unchanged"),
        before_import
    );

    let approved = workspace.run(&["mcp", "check", "--json", "--offline"]);
    assert!(approved.status.success(), "{approved:?}");
    let report: Value = serde_json::from_slice(&approved.stdout).expect("report");
    assert_eq!(report["servers"][0]["trust"], "approved");
    assert_eq!(report["servers"][0]["status"], "connected");
    assert_eq!(
        std::fs::read_to_string(marker).expect("started after approval"),
        "fixture environment with spaces"
    );
    assert!(!workspace.0.path().join("state/sessions").exists());
}

#[test]
fn malformed_or_unsupported_imports_emit_no_partial_config_or_secret_values() {
    let workspace = Workspace::new();
    let documents = [
        "not JSON".to_owned(),
        json!({"mcpServers": {"valid": {"command": "sh"}, "invalid": {"command": "sh", "cwd": "secret-rejected-value"}}}).to_string(),
        json!({"mcpServers": {"remote": {"url": "https://example.test/mcp"}}}).to_string(),
        json!({"mcpServers": {"invalid": {"command": "sh", "env": {"TOKEN": ["secret-rejected-value"]}}}}).to_string(),
        json!({"mcpServers": {"invalid": {"command": "sh", "args": "secret-rejected-value"}}}).to_string(),
        json!({"mcpServers": {"invalid": {"command": "sh", "env": {"TOKEN": "${secret-rejected-value}"}}}}).to_string(),
        json!({"mcpServers": {"with.dot": {"command": "sh"}}}).to_string(),
        json!({"mcpServers": {"invalid": {"command": ""}}}).to_string(),
        json!({"mcpServers": {"invalid": {"command": "sh", "args": ["\u{0}"]}}}).to_string(),
        json!({"mcpServers": {"invalid": {"command": "sh", "env": {"A=B": "secret-rejected-value"}}}}).to_string(),
        "{}".to_owned(),
    ];
    for document in documents {
        std::fs::write(workspace.0.path().join("desktop.json"), &document).expect("write document");
        let output = workspace.import();
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("invalid_field"), "{error}");
        assert!(!error.contains("secret-rejected-value"), "{error}");
        assert!(!workspace.0.path().join(".mcp.json").exists());
        assert!(!workspace.0.path().join("state").exists());
    }
}

#[test]
fn import_enforces_file_and_server_count_limits() {
    let workspace = Workspace::new();
    let source = workspace.0.path().join("desktop.json");
    let documents = [
        (
            vec![b' '; rune_context::mcp_trust::MAX_PROJECT_BYTES as usize + 1],
            vec!["mcp", "import", "claude-desktop", "desktop.json"],
        ),
        (
            json!({"mcpServers": {"a": {"command": "sh"}, "b": {"command": "sh"}}})
                .to_string()
                .into_bytes(),
            vec![
                "mcp",
                "import",
                "claude-desktop",
                "desktop.json",
                "--limit",
                "list_entries=1",
            ],
        ),
        (
            json!({"mcpServers": {"a": {"command": "sh", "args": vec!["a"; 110_000]}}})
                .to_string()
                .into_bytes(),
            vec!["mcp", "import", "claude-desktop", "desktop.json"],
        ),
    ];
    for (document, args) in documents {
        std::fs::write(&source, document).expect("write document");
        let output = workspace.run(&args);
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("too_large"),
            "{output:?}"
        );
    }
}

#[test]
fn minimal_and_empty_configs_import_with_stable_server_order() {
    let workspace = Workspace::new();
    let source = workspace.0.path().join("desktop.json");
    std::fs::write(
        &source,
        r#"{"mcpServers":{"z":{"command":"last"},"a":{"command":"first"}}}"#,
    )
    .expect("minimal source");
    let output = workspace.import();
    assert!(output.status.success(), "{output:?}");
    let imported: Value = serde_json::from_slice(&output.stdout).expect("project JSON");
    assert_eq!(
        imported["mcpServers"]["a"],
        json!({"type": "stdio", "command": "first", "args": [], "env": {}})
    );
    let names: Vec<_> = imported["mcpServers"]
        .as_object()
        .expect("servers")
        .keys()
        .collect();
    assert_eq!(names, ["a", "z"]);
    std::fs::write(source, r#"{"mcpServers":{}}"#).expect("empty source");
    let empty = workspace.import();
    assert!(empty.status.success(), "{empty:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&empty.stdout).expect("empty JSON"),
        json!({"mcpServers": {}})
    );
}

#[test]
fn import_validates_usage_and_help_explains_the_format() {
    let workspace = Workspace::new();
    for args in [
        vec!["mcp", "import"],
        vec!["mcp", "import", "other-format", "desktop.json"],
        vec!["mcp", "import", "claude-desktop", "desktop.json", "extra"],
        vec!["mcp", "import", "claude-desktop", "."],
        vec!["mcp", "import", "claude-desktop", "missing.json"],
    ] {
        let output = workspace.run(&args);
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
    }
    let help = workspace.run(&["mcp", "--help"]);
    assert!(help.status.success(), "{help:?}");
    assert!(
        String::from_utf8_lossy(&help.stdout).contains("rune mcp import claude-desktop <file>")
    );
}
