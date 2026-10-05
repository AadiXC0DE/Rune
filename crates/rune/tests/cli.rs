//! Integration tests for the command-line surface.
//!
//! These run the built binary as a subprocess, so they exercise the real
//! dispatch path including exit codes and stream separation rather than calling
//! into the library.

// Integration tests assert by panicking. The guards that forbid panicking
// apply to shipped code, where a panic on user input is a defect.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::process::Command;

/// Path of the binary under test.
fn binary() -> std::path::PathBuf {
    let mut path = std::env::current_exe().expect("test exe path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join("rune")
}

/// Runs the binary with an isolated state directory.
fn run(args: &[&str]) -> Output {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let output = Command::new(binary())
        .args(args)
        .env("RUNE_HOME", dir.path().join("state"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .output()
        .expect("run binary");

    Output {
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// Captured result of a run.
struct Output {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

#[test]
fn version_prints_one_line_and_exits_zero() {
    let out = run(&["--version"]);
    assert_eq!(out.status, Some(0), "stderr: {}", out.stderr);
    assert_eq!(out.stdout.lines().count(), 1, "{}", out.stdout);
    assert!(out.stdout.starts_with("rune "), "{}", out.stdout);
    assert!(out.stderr.is_empty(), "{}", out.stderr);
}

#[test]
fn version_short_flag_matches_long_flag() {
    assert_eq!(run(&["-v"]).stdout, run(&["--version"]).stdout);
    assert_eq!(run(&["version"]).stdout, run(&["--version"]).stdout);
}

#[test]
fn help_lists_every_command_group() {
    let out = run(&["--help"]);
    assert_eq!(out.status, Some(0));
    for group in [
        "Run:",
        "Sessions and local records:",
        "Account and configuration:",
        "Diagnostics and maintenance:",
    ] {
        assert!(out.stdout.contains(group), "missing group {group}");
    }
    assert!(out.stdout.contains("Global flags:"));
}

#[test]
fn help_for_one_command_shows_its_usage() {
    let out = run(&["help", "sessions"]);
    assert_eq!(out.status, Some(0));
    assert!(out.stdout.contains("rune sessions"));
    assert!(out.stdout.contains("--limit"));
}

#[test]
fn help_for_an_unknown_command_says_so() {
    let out = run(&["help", "frobnicate"]);
    assert!(out.stdout.contains("frobnicate"));
    assert!(out.stdout.contains("rune help"));
}

#[test]
fn unknown_command_fails_with_a_hint() {
    let out = run(&["frobnicate"]);
    assert_eq!(out.status, Some(1));
    assert!(out.stderr.contains("not a command"), "{}", out.stderr);
    assert!(out.stderr.contains("hint:"), "{}", out.stderr);
}

#[test]
fn unknown_global_flag_fails_before_loading_configuration() {
    let out = run(&["--nonsense"]);
    assert_eq!(out.status, Some(1));
    assert!(out.stderr.contains("--nonsense"), "{}", out.stderr);
    assert!(out.stdout.is_empty());
}

#[test]
fn missing_flag_value_is_reported_as_a_missing_field() {
    let out = run(&["--model"]);
    assert_eq!(out.status, Some(1));
    assert!(out.stderr.contains("requires a value"), "{}", out.stderr);
}

#[test]
fn doctor_passes_on_a_clean_setup() {
    let out = run(&["doctor"]);
    assert_eq!(
        out.status,
        Some(0),
        "stdout: {}\nstderr: {}",
        out.stdout,
        out.stderr
    );
    assert!(out.stdout.contains("ok "), "{}", out.stdout);
}

#[test]
fn doctor_json_is_parseable_and_names_every_check() {
    let out = run(&["doctor", "--json"]);
    assert_eq!(out.status, Some(0));
    let value: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid json");
    let checks = value["checks"].as_array().expect("checks array");
    assert!(!checks.is_empty());
    for check in checks {
        assert!(check["name"].is_string());
        assert!(check["outcome"].is_string());
        assert!(check["detail"].is_string());
    }
    assert!(value["version"].is_string());
    assert!(value["state_root"].is_string());
}

#[test]
fn doctor_reports_an_unconfigured_provider_as_unknown_not_failed() {
    let out = run(&["doctor", "--json"]);
    let value: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid json");
    let checks = value["checks"].as_array().expect("checks array");
    let provider = checks
        .iter()
        .find(|check| check["name"] == "provider")
        .expect("provider check");
    assert_eq!(provider["outcome"], "unknown");
    assert!(
        provider["hint"]
            .as_str()
            .expect("hint")
            .contains("rune connect")
    );
}

#[test]
fn a_fresh_install_has_no_provider_and_says_how_to_connect_one() {
    let out = run(&["status", "--json"]);
    assert_eq!(out.status, Some(0));
    let value: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid json");
    assert_eq!(value["provider"], "unconfigured");
    assert_eq!(value["provider_connected"], false);
}

#[test]
fn limits_json_lists_every_limit_with_its_source() {
    let out = run(&["limits", "--json"]);
    assert_eq!(out.status, Some(0));
    let value: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid json");
    let limits = value["limits"].as_array().expect("limits array");
    assert!(
        limits.len() > 20,
        "expected the full set, got {}",
        limits.len()
    );
    for row in limits {
        assert!(row["name"].is_string());
        assert!(!row["value"].is_null() || row["value"] == "off");
        assert!(!row["default"].is_null() || row["default"] == "off");
        assert!(row["unit"].is_string());
        assert!(row["min"].is_number());
        assert!(row["description"].is_string());
    }
}

#[test]
fn limits_text_shows_defaults() {
    let out = run(&["limits"]);
    assert_eq!(out.status, Some(0));
    assert!(out.stdout.contains("max_tool_result_bytes"));
    assert!(out.stdout.contains("provider_head_timeout_ms"));
    assert!(out.stdout.contains("default"));
}

#[test]
fn a_limit_override_is_visible_in_limits_output() {
    let out = run(&["--limit", "list_entries=42", "limits", "--json"]);
    assert_eq!(out.status, Some(0), "stderr: {}", out.stderr);
    let value: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid json");
    let limits = value["limits"].as_array().expect("limits array");
    let row = limits
        .iter()
        .find(|row| row["name"] == "list_entries")
        .expect("list_entries");
    assert_eq!(row["value"], 42);
    assert_eq!(row["source"], "command_line");
}

#[test]
fn a_limit_outside_its_range_is_rejected() {
    let out = run(&["--limit", "compaction_trigger_percent=5", "limits"]);
    assert_eq!(out.status, Some(1));
    assert!(
        out.stderr.contains("compaction_trigger_percent"),
        "{}",
        out.stderr
    );
}

#[test]
fn an_unknown_limit_name_is_rejected() {
    let out = run(&["--limit", "nonsense=5", "limits"]);
    assert_eq!(out.status, Some(1));
    assert!(out.stderr.contains("not a known limit"), "{}", out.stderr);
}

#[test]
fn config_explain_reports_the_source_layer() {
    let out = run(&["config", "--json"]);
    assert_eq!(out.status, Some(0));
    let value: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid json");
    let values = value["values"].as_array().expect("values array");
    let provider = values
        .iter()
        .find(|row| row["key"] == "provider")
        .expect("provider row");
    assert_eq!(provider["source"], "default");
    assert_eq!(value["layers"]["command_line"], "command_line");
}

#[test]
fn a_project_file_cannot_set_the_model() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(dir.path().join(".rune.toml"), "model = \"sneaky\"\n").expect("write");

    let output = Command::new(binary())
        .args(["config", "--json"])
        .current_dir(dir.path())
        .env("RUNE_HOME", dir.path().join("state"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .output()
        .expect("run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");
    let values = value["values"].as_array().expect("values array");
    let model = values
        .iter()
        .find(|row| row["key"] == "model")
        .expect("model row");
    assert_eq!(model["value"], "", "project file set the model");

    let diagnostics = value["diagnostics"].as_array().expect("diagnostics");
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0]["code"], "key_not_allowed_in_scope");
}

#[test]
fn a_malformed_user_config_warns_without_failing() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let config_dir = dir.path().join("config").join("rune");
    std::fs::create_dir_all(&config_dir).expect("mkdir");
    std::fs::write(config_dir.join("config.toml"), "this is not toml =").expect("write");

    let output = Command::new(binary())
        .args(["config", "--json"])
        .env("RUNE_HOME", dir.path().join("state"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .output()
        .expect("run");

    assert!(
        output.status.success(),
        "a warning must not fail the command"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");
    let diagnostics = value["diagnostics"].as_array().expect("diagnostics");
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0]["layer"], "user");
    assert_eq!(diagnostics[0]["code"], "invalid_configuration");
}

#[test]
fn workspace_list_is_empty_on_a_fresh_install() {
    let out = run(&["workspace", "list", "--json"]);
    assert_eq!(out.status, Some(0));
    let value: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid json");
    assert_eq!(value["directories"].as_array().expect("array").len(), 0);
}

#[test]
fn an_unknown_workspace_subcommand_is_rejected() {
    let out = run(&["workspace", "frobnicate"]);
    assert_eq!(out.status, Some(1));
    assert!(
        out.stderr.contains("list, add, remove, or clear"),
        "{}",
        out.stderr
    );
}

#[test]
fn prompt_reports_the_built_in_source() {
    let out = run(&["prompt"]);
    assert_eq!(out.status, Some(0));
    assert!(out.stdout.contains("built in"), "{}", out.stdout);
}

#[test]
fn an_unavailable_surface_says_so_explicitly() {
    // Fetching a release over the network is genuinely unsupported: the
    // installer only takes an artifact the caller supplies.
    let out = run(&[
        "upgrade",
        "--from",
        "https://example.test/rune",
        "--checksum",
        "00",
    ]);
    assert_eq!(out.status, Some(1));
    assert!(
        out.stderr.contains("not available in this build"),
        "{}",
        out.stderr
    );
}

#[test]
fn stdout_stays_clean_of_diagnostics() {
    // Machine consumers read stdout, so an error must not write there.
    let out = run(&["frobnicate"]);
    assert!(
        out.stdout.is_empty(),
        "stdout was not empty: {}",
        out.stdout
    );
    assert!(!out.stderr.is_empty());
}

#[test]
fn benchmark_short_circuits_before_configuration() {
    // With the benchmark variable set the process must exit after dispatch, so
    // a broken configuration cannot make the startup measurement fail.
    let dir = tempfile::TempDir::new().expect("tempdir");
    let config_dir = dir.path().join("config").join("rune");
    std::fs::create_dir_all(&config_dir).expect("mkdir");
    std::fs::write(config_dir.join("config.toml"), "broken =").expect("write");

    let output = Command::new(binary())
        .args(["doctor"])
        .env("RUNE_HOME", dir.path().join("state"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("RUNE_BENCH", "1")
        .output()
        .expect("run");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn benchmark_does_not_suppress_help_or_version() {
    // Help and version must still answer when the benchmark variable is set,
    // otherwise a startup measurement could not cover those paths.
    for args in [vec!["--help"], vec!["--version"]] {
        let output = Command::new(binary())
            .args(&args)
            .env("RUNE_BENCH", "1")
            .output()
            .expect("run");
        assert!(output.status.success(), "{args:?}");
        assert!(!output.stdout.is_empty(), "{args:?} printed nothing");
    }
}

/// A listener that counts the connections it accepts and answers none.
struct Listener {
    port: u16,
    accepts: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Listener {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let accepts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = std::sync::Arc::clone(&accepts);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stream.is_err() {
                    break;
                }
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });
        Self { port, accepts }
    }

    fn count(&self) -> usize {
        self.accepts.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Runs the binary against a provider served at the listener's port.
fn run_against(
    listener: &Listener,
    model: Option<&str>,
    extra: &[(&str, &str)],
    args: &[&str],
) -> Output {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let mut command = Command::new(binary());
    command
        .args(args)
        .env("RUNE_HOME", dir.path().join("state"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env("RUNE_PROVIDER", "chat_completions")
        .env(
            "RUNE_BASE_URL",
            format!("http://127.0.0.1:{}/v1", listener.port),
        )
        .env("RUNE_API_KEY_ENV", "RUNE_CLI_TEST_KEY")
        .env("RUNE_CLI_TEST_KEY", "sk-test")
        .env_remove("RUNE_MODEL")
        .env_remove("RUNE_OFFLINE");
    if let Some(model) = model {
        command.env("RUNE_MODEL", model);
    }
    for (key, value) in extra {
        command.env(key, value);
    }
    let output = command.output().expect("run binary");
    Output {
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

#[test]
fn offline_after_the_command_refuses_the_request() {
    // Offline after the command refuses the request just as it does before it.
    let listener = Listener::start();
    for args in [
        vec!["ask", "--offline", "hi"],
        vec!["ask", "hi", "--offline"],
    ] {
        let out = run_against(&listener, Some("m"), &[], &args);
        assert_eq!(out.status, Some(1), "{args:?}: {}", out.stderr);
        assert!(out.stderr.contains("disabled"), "{args:?}: {}", out.stderr);
    }
    assert_eq!(listener.count(), 0, "an offline run reached the endpoint");
}

#[test]
fn offline_in_the_user_file_preserves_the_model_and_refuses_requests() {
    let listener = Listener::start();
    let dir = tempfile::TempDir::new().expect("tempdir");
    let config_dir = dir.path().join("config").join("rune");
    std::fs::create_dir_all(&config_dir).expect("mkdir");
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            r#"
offline = true
web_tools = true
provider = "chat_completions"
base_url = "http://127.0.0.1:{}/v1"
api_key_env = "RUNE_CLI_TEST_KEY"
[models]
chat_completions = "configured-model"
[limits]
provider_request_timeout_ms = 1000
"#,
            listener.port
        ),
    )
    .expect("write config");

    let run_with_config = |args: &[&str]| {
        Command::new(binary())
            .env_clear()
            .env("HOME", dir.path())
            .env("RUNE_HOME", dir.path().join("state"))
            .env("XDG_CONFIG_HOME", dir.path().join("config"))
            .env("XDG_DATA_HOME", dir.path().join("data"))
            .env("RUNE_CLI_TEST_KEY", "sk-test")
            .current_dir(dir.path())
            .args(args)
            .output()
            .expect("run binary")
    };

    let models = run_with_config(&["models"]);
    assert!(models.status.success(), "{models:?}");
    assert!(
        String::from_utf8_lossy(&models.stdout).contains("configured-model"),
        "{models:?}"
    );
    assert!(models.stderr.is_empty(), "{models:?}");

    let ask = run_with_config(&["ask", "hi"]);
    assert_eq!(ask.status.code(), Some(1), "{ask:?}");
    let stderr = String::from_utf8_lossy(&ask.stderr);
    assert!(stderr.contains("outbound requests are disabled"), "{ask:?}");
    assert!(!stderr.contains("could not parse"), "{ask:?}");
    assert_eq!(
        listener.count(),
        0,
        "the offline user file allowed a request"
    );
}

#[test]
fn a_model_named_after_the_command_is_used() {
    let listener = Listener::start();
    let out = run_against(
        &listener,
        None,
        &[],
        &["ask", "--model", "m", "--offline", "hi"],
    );
    assert!(
        !out.stderr.contains("no model is selected"),
        "the model after the command was ignored: {}",
        out.stderr
    );
}

#[test]
fn a_limit_after_the_command_is_applied() {
    let out = run(&["limits", "--limit", "max_agent_steps=5", "--json"]);
    assert_eq!(out.status, Some(0), "stderr: {}", out.stderr);
    let value: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid json");
    let row = value["limits"]
        .as_array()
        .expect("limits array")
        .iter()
        .find(|row| row["name"] == "max_agent_steps")
        .cloned()
        .expect("max_agent_steps");
    assert_eq!(row["value"], 5);
    assert_eq!(row["source"], "command_line");
}

#[test]
fn listing_models_offline_reports_the_configured_model_without_a_request() {
    // Offline is honoured however it is given: the global flag, the flag after
    // the command, or the environment.
    let listener = Listener::start();
    for (env, args) in [
        (vec![], vec!["--offline", "models"]),
        (vec![], vec!["models", "--offline"]),
        (vec![("RUNE_OFFLINE", "1")], vec!["models"]),
    ] {
        let out = run_against(&listener, Some("configured-model"), &env, &args);
        assert_eq!(out.status, Some(0), "{args:?} {env:?}: {}", out.stderr);
        assert!(
            out.stdout.contains("configured-model"),
            "{args:?} {env:?}: {}",
            out.stdout
        );
    }
    assert_eq!(
        listener.count(),
        0,
        "an offline listing reached the endpoint"
    );
}

/// Runs `rune connect` for a machine caller, with or without the key exported.
fn connect_json(key: Option<&str>) -> (Output, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let mut command = Command::new(binary());
    command
        .args(["connect", "anthropic", "--json"])
        .env("RUNE_HOME", dir.path().join("state"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("RUNE_API_KEY_ENV")
        .env_remove("RUNE_PROVIDER")
        .env_remove("RUNE_BASE_URL")
        .env_remove("RUNE_MODEL");
    if let Some(key) = key {
        command.env("ANTHROPIC_API_KEY", key);
    }
    let output = command.output().expect("run binary");
    let out = Output {
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    };
    (out, dir)
}

#[test]
fn connecting_without_a_credential_fails_and_saves_nothing() {
    // A machine caller with no key is refused, rather than told it connected
    // and left with a selection that no request can use.
    let (out, dir) = connect_json(None);
    assert_eq!(out.status, Some(1), "stdout: {}", out.stdout);
    assert!(out.stderr.contains("no credential"), "{}", out.stderr);
    assert!(out.stdout.is_empty(), "{}", out.stdout);
    assert!(
        !dir.path()
            .join("config")
            .join("rune")
            .join("config.toml")
            .exists(),
        "the selection was saved without a credential"
    );
}

#[test]
fn connecting_with_the_key_exported_succeeds() {
    let (out, _dir) = connect_json(Some("sk-ant-test"));
    assert_eq!(out.status, Some(0), "stderr: {}", out.stderr);
    let value: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid json");
    assert_eq!(value["provider"], "anthropic");
    assert!(!out.stdout.contains("sk-ant-test"), "{}", out.stdout);
}

#[cfg(unix)]
#[test]
fn connecting_in_a_fresh_home_creates_private_state_and_opens_a_session() {
    use std::os::unix::fs::PermissionsExt as _;

    // Set the umask in a child shell so parallel tests keep their own mask.
    for mask in ["022", "002"] {
        for xdg in [false, true] {
            let dir = tempfile::tempdir().expect("fresh home");
            let state = dir.path().join(if xdg {
                "state/rune"
            } else {
                ".local/state/rune"
            });
            let command = || {
                let mut command = Command::new("sh");
                command
                    .args(["-c", r#"umask "$1"; shift; exec "$@""#, "rune-test", mask])
                    .arg(binary())
                    .env_clear()
                    .env("HOME", dir.path())
                    .current_dir(dir.path());
                if xdg {
                    command.env("XDG_STATE_HOME", dir.path().join("state"));
                }
                command
            };
            assert!(!state.exists(), "the home already has Rune state");

            let connected = command()
                .args(["connect", "anthropic", "--json"])
                .env("ANTHROPIC_API_KEY", "sk-ant-test")
                .output()
                .expect("connect");
            assert!(
                connected.status.success(),
                "umask {mask}, xdg {xdg}: {}",
                String::from_utf8_lossy(&connected.stderr)
            );
            for (path, mode) in [
                (state.clone(), 0o700),
                (state.join("credentials.json"), 0o600),
            ] {
                assert_eq!(
                    std::fs::metadata(&path)
                        .expect("metadata")
                        .permissions()
                        .mode()
                        & 0o777,
                    mode,
                    "{} with umask {mask}, xdg {xdg}",
                    path.display()
                );
            }

            // No key in the environment: the session must use the stored one.
            // EOF ends it after startup, without needing a live provider.
            let session = command()
                .args(["--offline", "--model", "claude-test"])
                .stdin(std::process::Stdio::null())
                .output()
                .expect("open session");
            assert!(
                session.status.success(),
                "umask {mask}, xdg {xdg}: {}",
                String::from_utf8_lossy(&session.stderr)
            );
            assert!(
                String::from_utf8_lossy(&session.stdout).contains("session "),
                "the session was not announced"
            );
            let sessions: Vec<_> = std::fs::read_dir(state.join("sessions"))
                .expect("sessions directory")
                .collect::<Result<_, _>>()
                .expect("session entries");
            assert_eq!(sessions.len(), 1, "one session must have opened");
            let session_dir = camino::Utf8PathBuf::from_path_buf(sessions[0].path()).expect("utf8");
            let saved = rune_session::store::load_read_only(&session_dir).expect("session log");
            assert!(
                saved.events.iter().any(|frame| matches!(
                    &frame.event,
                    rune_session::event::SessionEvent::WorkspaceSet { .. }
                )),
                "the session did not record its workspace"
            );
        }
    }
}

#[test]
fn a_flag_the_command_does_not_take_is_refused() {
    let out = run(&["models", "--nonsense"]);
    assert_eq!(out.status, Some(1));
    assert!(
        out.stderr.contains("not a flag of `rune models`"),
        "{}",
        out.stderr
    );
    assert!(out.stderr.contains("hint:"), "{}", out.stderr);
}
