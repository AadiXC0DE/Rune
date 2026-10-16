//! R-182: inspect provider support and reject selections before the first turn.
#![allow(clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod support;
use support::CommandExt as _;

use std::process::{Command, Output};

fn profile(config: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("profile");
    let path = dir.path().join("config.toml");
    rune_core::paths::write_private(camino::Utf8Path::from_path(&path).expect("UTF-8"), config)
        .expect("private config");
    dir
}

fn run(dir: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rune"))
        .isolated_env()
        .env("HOME", dir)
        .env("RUNE_HOME", dir.join("state"))
        .env("RUNE_CONFIG", dir.join("config.toml"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_DATA_HOME", dir.join("data"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("CLI")
}

#[test]
fn diagnostics_report_support_and_configured_context_without_credentials_or_a_turn() {
    let dir = profile(
        "provider = 'responses'\n[models.responses]\nid = 'fixture'\ncontext_window = 4096\n",
    );
    let out = run(
        dir.path(),
        &[
            "capabilities",
            "--effort",
            "high",
            "--fast",
            "--require-images",
            "--require-tool-streaming",
            "--json",
        ],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(out.stderr.is_empty(), "{out:?}");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).expect("report");
    assert_eq!(report["scope"], "dialect");
    assert_eq!(report["dialect"], "responses");
    assert_eq!(
        report["capabilities"]["efforts"],
        serde_json::json!([
            "auto", "none", "minimal", "low", "medium", "high", "xhigh", "max"
        ])
    );
    assert_eq!(report["capabilities"]["images"], true);
    assert_eq!(report["capabilities"]["tool_streaming"], "incremental");
    assert_eq!(report["context_window"], 4096);
    assert_eq!(report["context_source"], "configured:user");
    assert_eq!(report["valid"], true);
    assert!(
        !dir.path().join("state").exists(),
        "preflight created state"
    );
    let out = run(dir.path(), &["capabilities"]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).expect("text");
    for expected in [
        "effort         auto, none",
        "images         true",
        "tool streaming incremental",
        "context source configured:user",
        "selection      valid",
    ] {
        assert!(text.contains(expected), "{text}");
    }
}

#[test]
fn unsupported_controls_are_reviewable_and_rejected_before_credentials_or_requests() {
    let dir = profile("provider = 'gemini'\n[models]\ngemini = 'fixture'\n");
    for flags in [
        vec!["--effort", "high"],
        vec!["--fast"],
        vec!["--require-tool-streaming"],
        vec!["--provider-order", "openai"],
    ] {
        let mut args = vec!["capabilities", "--json"];
        args.extend(flags);
        let out = run(dir.path(), &args);
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        let report: serde_json::Value = serde_json::from_slice(&out.stdout).expect("report");
        assert_eq!(report["valid"], false);
        assert!(report["selection_error"].is_string());
        assert_eq!(
            report["capabilities"]["efforts"],
            serde_json::json!(["auto"])
        );
        assert_eq!(report["capabilities"]["tool_streaming"], "complete_calls");
        let stderr = String::from_utf8(out.stderr).expect("stderr");
        assert!(stderr.contains("gemini"), "{stderr}");
        assert!(!stderr.contains("credential"), "{stderr}");
        assert!(!dir.path().join("state").exists());
    }
    // Actual entry points use the same selection check before setup or a turn.
    for args in [
        vec!["ask", "hi", "--effort", "high", "--no-save"],
        vec!["acp", "--effort", "high"],
        vec!["--effort", "high"],
    ] {
        let out = run(dir.path(), &args);
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        let stderr = String::from_utf8(out.stderr).expect("stderr");
        assert!(stderr.contains("effort"), "{stderr}");
        assert!(!stderr.contains("credential"), "{stderr}");
        assert!(!dir.path().join("state").exists());
    }
}

#[test]
fn offline_context_provenance_distinguishes_cached_capacity_from_the_default() {
    let dir = profile("provider = 'responses'\n[models]\nresponses = 'fixture'\n");
    let out = run(dir.path(), &["capabilities", "--offline", "--json"]);
    assert!(out.status.success(), "{out:?}");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).expect("report");
    assert_eq!(
        report["context_window"],
        rune_core::config::DEFAULT_CONTEXT_WINDOW
    );
    assert_eq!(report["context_source"], "compiled_default");
    let state = dir.path().join("state");
    let state = camino::Utf8Path::from_path(&state).expect("UTF-8");
    let paths = rune_core::paths::Paths::resolve(None, None, None, None, Some(state.as_str()));
    rune_core::paths::create_dir_private(&paths.state_root).expect("state");
    rune_core::paths::write_private(
        &paths.models_cache_file(),
        r#"{"openai":{"models":{"fixture":{"limit":{"context":64000}}}}}"#,
    )
    .expect("cache");
    let out = run(dir.path(), &["capabilities", "--offline", "--json"]);
    assert!(out.status.success(), "{out:?}");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).expect("report");
    assert_eq!(report["context_window"], 64000);
    assert_eq!(report["context_source"], "models.dev:limit.context");
}

#[test]
fn local_context_discovery_reports_ollama_provenance_without_a_completion() {
    use std::io::{BufRead, Read, Write};
    use std::time::{Duration, Instant};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("fixture");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let dir = profile(&format!(
        "provider = 'ollama'\nanonymous = true\nbase_url = 'http://{address}/v1'\n[models]\nollama = 'fixture:latest'\n"
    ));
    let server = std::thread::spawn(move || {
        for (expected, response) in [
            (
                "GET /api/tags",
                serde_json::json!({"models": [{"name": "fixture:latest"}]}),
            ),
            (
                "POST /api/show",
                serde_json::json!({"parameters": "num_ctx 8192"}),
            ),
        ] {
            let start = Instant::now();
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            start.elapsed() < Duration::from_secs(10),
                            "missing metadata request"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            // `accept` inherits `O_NONBLOCK` from a nonblocking listener on
            // macOS, unlike Linux; the read timeout below is the real bound, so
            // the accepted socket must be blocking before it is read.
            stream
                .set_nonblocking(false)
                .expect("blocking request socket");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("timeout");
            let mut reader = std::io::BufReader::new(&mut stream);
            let mut line = String::new();
            reader.read_line(&mut line).expect("request");
            assert!(line.starts_with(expected), "{line}");
            let mut length = 0;
            loop {
                line.clear();
                assert!(reader.read_line(&mut line).expect("header") > 0);
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().expect("length");
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).expect("body");
            drop(reader);
            let body = response.to_string();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).expect("response");
        }
        listener
    });
    let out = run(dir.path(), &["capabilities", "--json"]);
    assert!(out.status.success(), "{out:?}");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).expect("report");
    assert_eq!(report["context_window"], 8192);
    assert_eq!(
        report["context_source"],
        "ollama:/api/show:parameters.num_ctx"
    );
    let listener = server.join().expect("fixture");
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "unexpected completion request"
    );
    assert!(!dir.path().join("state").exists());
}
