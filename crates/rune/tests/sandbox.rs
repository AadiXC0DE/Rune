//! Sandbox explanation through the real command-line surface.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        for relative in [
            "workspace/.git/hooks",
            "saved/.git/hooks",
            "added",
            "home/.ssh",
            "config",
            "state",
        ] {
            std::fs::create_dir_all(dir.path().join(relative)).expect("fixture directory");
        }
        for relative in ["workspace/.git/config", "saved/.git/config"] {
            std::fs::write(dir.path().join(relative), "[core]\n").expect("config fixture");
        }
        // TOML serialization preserves platform path separators.
        let config = toml::to_string(&serde_json::json!({
            "additional_directories": [dir.path().join("saved").to_str().expect("utf8")],
            "permission_mode": "full-access",
        }))
        .expect("toml");
        std::fs::write(dir.path().join("config.toml"), config).expect("config");
        Self { dir }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rune"))
            .current_dir(self.dir.path().join("workspace"))
            .env_clear()
            .env("HOME", self.dir.path().join("home"))
            .env("RUNE_HOME", self.dir.path().join("state"))
            .env("RUNE_CONFIG", self.dir.path().join("config.toml"))
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .env("XDG_DATA_HOME", self.dir.path().join("data"))
            .env("PATH", self.dir.path().join("missing-bin"))
            .args(args)
            .output()
            .expect("rune")
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        serde_json::from_slice(&output.stdout).expect("json report")
    }
}

fn resolved(path: &Path) -> String {
    path.canonicalize()
        .expect("resolved")
        .to_str()
        .expect("utf8")
        .to_owned()
}

#[test]
fn fixture_command_reports_paths_and_sources_without_executing() {
    let fixture = Fixture::new();
    let added = fixture.dir.path().join("added");
    let command = "echo executed > SHOULD_NOT_EXIST";
    let report = fixture.json(&[
        "sandbox",
        "explain",
        "--json",
        "--add-dir",
        added.to_str().expect("utf8"),
        "--",
        command,
    ]);
    assert_eq!(report["command"], command);
    assert!(
        !fixture
            .dir
            .path()
            .join("workspace/SHOULD_NOT_EXIST")
            .exists()
    );
    assert!(
        !report["backend"]["value"]
            .as_str()
            .expect("backend")
            .is_empty()
    );
    assert!(
        report["backend"]["source"]
            .as_str()
            .expect("source")
            .contains("probe")
    );
    let roots = report["writable_roots"].as_array().expect("roots");
    assert_eq!(roots.len(), 3);
    assert_eq!(
        roots[0]["path"],
        resolved(&fixture.dir.path().join("workspace"))
    );
    assert_eq!(
        roots[1]["path"],
        resolved(&fixture.dir.path().join("saved"))
    );
    assert_eq!(roots[1]["source"], "user additional_directories");
    assert_eq!(roots[2]["path"], resolved(&added));
    assert_eq!(roots[2]["source"], "command_line --add-dir");
    let protected = report["protected_paths"].as_array().expect("protections");
    for relative in [
        "workspace/.git/config",
        "workspace/.git/hooks",
        "saved/.git/config",
        "saved/.git/hooks",
        "home/.ssh",
        "state",
    ] {
        assert!(
            protected.iter().any(|rule| rule["path"]
                == resolved(&fixture.dir.path().join(relative))
                && !rule["source"].as_str().expect("source").is_empty()),
            "missing {relative}: {report}"
        );
    }
    assert_eq!(report["network"]["value"], false);
    let text = fixture.run(&["sandbox", "explain", "--", command]);
    assert!(text.status.success());
    let text = String::from_utf8(text.stdout).expect("utf8");
    for label in [
        "backend:",
        "writable roots:",
        "network policy: denied",
        "protected paths:",
        "source:",
    ] {
        assert!(text.contains(label), "{text}");
    }
}

#[test]
fn offline_overrides_an_explicit_context_grant_and_reports_its_source() {
    let fixture = Fixture::new();
    let granted = fixture.json(&[
        "sandbox",
        "explain",
        "--external-access",
        "--json",
        "--",
        "echo fixture",
    ]);
    assert_eq!(granted["network"]["value"], true);
    assert!(
        granted["network"]["source"]
            .as_str()
            .expect("source")
            .contains("command_line")
    );
    let offline = fixture.json(&[
        "--offline",
        "sandbox",
        "explain",
        "--external-access",
        "--json",
        "--",
        "echo fixture",
    ]);
    assert_eq!(offline["network"]["value"], false);
    assert_eq!(
        offline["network"]["source"],
        "command_line offline=true overrides external access"
    );
}

#[test]
fn saved_roots_can_be_ignored_while_command_line_roots_remain() {
    let fixture = Fixture::new();
    let added = fixture.dir.path().join("added");
    let report = fixture.json(&[
        "sandbox",
        "explain",
        "--json",
        "--no-additional-dirs",
        "--add-dir",
        added.to_str().expect("utf8"),
        "--",
        "echo fixture",
    ]);
    let roots = report["writable_roots"].as_array().expect("roots");
    assert_eq!(roots.len(), 2);
    assert_eq!(roots[1]["path"], resolved(&added));
    assert_eq!(roots[1]["source"], "command_line --add-dir");
}

#[test]
fn missing_command_invalid_action_and_missing_root_are_rejected() {
    let fixture = Fixture::new();
    for args in [
        vec!["sandbox", "explain"],
        vec!["sandbox", "run", "echo fixture"],
        vec![
            "sandbox",
            "explain",
            "--add-dir",
            "missing-root",
            "--",
            "echo fixture",
        ],
        vec!["sandbox", "explain", "--", "   "],
    ] {
        let output = fixture.run(&args);
        assert!(!output.status.success(), "{args:?}");
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn missing_backend_reports_refusal_or_explicit_unsandboxed_fallback() {
    let fixture = Fixture::new();
    let refused = fixture.json(&["sandbox", "explain", "--json", "--", "echo fixture"]);
    assert_eq!(refused["backend"]["value"], "namespaces");
    assert_eq!(refused["support"], "unsupported");
    assert_eq!(refused["enforcement"], "refused");
    assert_eq!(refused["effective_network"], "not_run");
    assert_eq!(refused["protections_applied"], false);
    let fallback = fixture.json(&[
        "sandbox",
        "explain",
        "--json",
        "--allow-unsandboxed",
        "--",
        "echo fixture",
    ]);
    assert_eq!(fallback["enforcement"], "unsandboxed");
    assert_eq!(fallback["effective_network"], "unrestricted");
    assert_eq!(fallback["protections_applied"], false);
    assert_eq!(
        fallback["allow_unsandboxed"]["source"],
        "command_line allow_unsandboxed"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_supported_backend_only_runs_its_probe_and_never_the_fixture_command() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let bin = fixture.dir.path().join("missing-bin");
    std::fs::create_dir(&bin).expect("bin");
    let helper = bin.join("bwrap");
    std::fs::write(
        &helper,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" >> probe.log\nexit 0\n",
    )
    .expect("helper");
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).expect("executable");
    let report = fixture.json(&[
        "sandbox",
        "explain",
        "--json",
        "--",
        "echo executed > SHOULD_NOT_EXIST",
    ]);
    assert_eq!(report["enforcement"], "enforced");
    assert_eq!(report["protections_applied"], true);
    assert_eq!(report["effective_network"], "denied");
    assert_eq!(report["temporary_roots"][0]["path"], "/tmp");
    assert_eq!(report["writable_device_paths"][0]["path"], "/dev");
    let workspace = fixture.dir.path().join("workspace");
    assert!(!workspace.join("SHOULD_NOT_EXIST").exists());
    let probe = std::fs::read_to_string(workspace.join("probe.log")).expect("probe log");
    assert_eq!(probe, "--ro-bind\n/\n/\n--proc\n/proc\n/bin/true\n");
}

#[cfg(unix)]
#[test]
fn a_root_alias_is_reported_where_the_backend_resolves_it() {
    let fixture = Fixture::new();
    let alias = fixture.dir.path().join("alias");
    let saved = fixture.dir.path().join("saved");
    std::os::unix::fs::symlink(&saved, &alias).expect("alias");
    let report = fixture.json(&[
        "sandbox",
        "explain",
        "--json",
        "--no-additional-dirs",
        "--add-dir",
        alias.to_str().expect("utf8"),
        "--",
        "echo fixture",
    ]);
    assert_eq!(report["writable_roots"][1]["path"], resolved(&saved));
    assert!(
        report["protected_paths"]
            .as_array()
            .expect("protections")
            .iter()
            .any(|rule| rule["path"] == resolved(&saved.join(".git/hooks")))
    );
}
