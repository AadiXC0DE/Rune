//! R-181: importing Ollama uses native local metadata and records provenance.
#![allow(clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::io::{BufRead, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

fn profile() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("isolated profile");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("private profile");
    }
    dir
}

fn run(dir: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rune"))
        .env_clear()
        .env("HOME", dir)
        .env("RUNE_HOME", dir.join("state"))
        .env("RUNE_CONFIG", dir.join("config.toml"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_DATA_HOME", dir.join("data"))
        // An anonymous connection must ignore even an available credential.
        .env("UNUSED_LOCAL_KEY", "must-not-be-stored-or-sent")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run CLI")
}

fn write_config(dir: &std::path::Path, config: &str) {
    let path = dir.join("config.toml");
    rune_core::paths::write_private(
        camino::Utf8Path::from_path(&path).expect("UTF-8 path"),
        config,
    )
    .expect("private config");
}

#[test]
fn imports_two_local_models_without_a_remote_catalog_and_keeps_inference_working() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fixture");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let base = format!("http://{address}/v1");
    let dir = profile();
    // Connecting Ollama explicitly selects anonymous loopback mode. The user
    // supplies an endpoint and model, without inventing a dummy credential.
    write_config(
        dir.path(),
        &format!(
            "provider = 'ollama'\nbase_url = '{base}'\napi_key_env = 'UNUSED_LOCAL_KEY'\n[models]\nollama = 'llama:latest'\n"
        ),
    );
    let connected = run(dir.path(), &["connect", "ollama", "--json"]);
    assert!(connected.status.success(), "{connected:?}");
    let connected: serde_json::Value =
        serde_json::from_slice(&connected.stdout).expect("connection");
    assert_eq!(connected["provider"], "ollama");
    assert_eq!(connected["base_url"], base);
    let saved = std::fs::read_to_string(dir.path().join("config.toml")).expect("saved selection");
    assert!(saved.contains("anonymous = true"), "{saved}");

    let server = std::thread::spawn(move || {
        let mut paths = Vec::new();
        // JSON and terminal listings each use tags followed by show per model.
        for _ in 0..7 {
            let started = Instant::now();
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(started.elapsed() < Duration::from_secs(10), "no request");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("read timeout");
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .expect("write timeout");
            let mut reader = std::io::BufReader::new(&mut stream);
            let mut line = String::new();
            reader.read_line(&mut line).expect("request line");
            let path = line.split_whitespace().nth(1).expect("path").to_owned();
            let method = line.split_whitespace().next().expect("method").to_owned();
            let mut length = 0;
            loop {
                line.clear();
                assert!(reader.read_line(&mut line).expect("header") > 0);
                if line == "\r\n" {
                    break;
                }
                let (name, value) = line.split_once(':').expect("header pair");
                assert!(!name.eq_ignore_ascii_case("authorization"), "{line}");
                assert!(!line.contains("must-not-be-stored-or-sent"));
                if name.eq_ignore_ascii_case("content-length") {
                    length = value.trim().parse().expect("content length");
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).expect("body");
            drop(reader);
            let (content_type, response) = match path.as_str() {
                "/api/tags" => {
                    assert_eq!(method, "GET");
                    (
                        "application/json",
                        r#"{"models":[{"name":"llama:latest"},{"name":"qwen:8b"}]}"#,
                    )
                }
                "/api/show" => {
                    assert_eq!(method, "POST");
                    let request: serde_json::Value =
                        serde_json::from_slice(&body).expect("show body");
                    match request["model"].as_str().expect("model") {
                        "llama:latest" => (
                            "application/json",
                            r#"{"model_info":{"general.architecture":"llama","llama.context_length":8192}}"#,
                        ),
                        "qwen:8b" => (
                            "application/json",
                            r#"{"parameters":"num_ctx 32768","model_info":{"general.architecture":"qwen","qwen.context_length":131072}}"#,
                        ),
                        other => panic!("unexpected model {other}"),
                    }
                }
                "/v1/chat/completions" => {
                    assert_eq!(method, "POST");
                    let request: serde_json::Value =
                        serde_json::from_slice(&body).expect("chat body");
                    assert_eq!(request["model"], "llama:latest");
                    (
                        "text/event-stream",
                        "data: {\"choices\":[{\"delta\":{\"content\":\"Ollama fixture answer\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                    )
                }
                other => panic!("unexpected path {other}"),
            };
            paths.push(path);
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).expect("response");
        }
        paths
    });

    let models = run(dir.path(), &["models", "--json"]);
    assert!(models.status.success(), "{models:?}");
    let models: serde_json::Value = serde_json::from_slice(&models.stdout).expect("catalog JSON");
    assert_eq!(models["provider"], "ollama");
    assert_eq!(models["endpoint"], base);
    assert_eq!(models["source"], "endpoint");
    let entries = models["models"].as_array().expect("models");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["id"], "llama:latest");
    assert_eq!(entries[0]["context_window"], 8192);
    assert_eq!(
        entries[0]["context_source"],
        "ollama:/api/show:model_info.llama.context_length"
    );
    assert_eq!(entries[1]["id"], "qwen:8b");
    assert_eq!(entries[1]["context_window"], 32768);
    assert_eq!(
        entries[1]["context_source"],
        "ollama:/api/show:parameters.num_ctx"
    );

    let rendered = run(dir.path(), &["models"]);
    assert!(rendered.status.success(), "{rendered:?}");
    let rendered = String::from_utf8_lossy(&rendered.stdout);
    assert!(
        rendered.contains(&format!("endpoint  {base}")),
        "{rendered}"
    );
    assert!(
        rendered.contains(
            "context 8192  context source ollama:/api/show:model_info.llama.context_length"
        ),
        "{rendered}"
    );
    assert!(
        rendered.contains("context 32768  context source ollama:/api/show:parameters.num_ctx"),
        "{rendered}"
    );

    let offline = run(dir.path(), &["models", "--json", "--offline"]);
    assert!(offline.status.success(), "{offline:?}");
    let offline: serde_json::Value = serde_json::from_slice(&offline.stdout).expect("offline JSON");
    assert_eq!(offline["source"], "configured");
    assert_eq!(offline["models"].as_array().expect("models").len(), 1);

    let answer = run(dir.path(), &["ask", "--json", "say hello"]);
    assert!(answer.status.success(), "{answer:?}");
    let answer: serde_json::Value = serde_json::from_slice(&answer.stdout).expect("answer JSON");
    assert_eq!(answer["output"], "Ollama fixture answer");
    assert_eq!(
        server.join().expect("server"),
        [
            "/api/tags",
            "/api/show",
            "/api/show",
            "/api/tags",
            "/api/show",
            "/api/show",
            "/v1/chat/completions"
        ]
    );
    assert!(!dir.path().join("state/credentials.json").exists());
    assert!(!dir.path().join("state/models-dev.json").exists());
    assert!(!saved.contains("must-not-be-stored-or-sent"));
}

#[test]
fn connecting_ollama_to_a_remote_host_is_refused_without_saving() {
    let dir = profile();
    let config = "provider = 'ollama'\nbase_url = 'https://example.com/v1'\n";
    write_config(dir.path(), config);
    let output = run(dir.path(), &["connect", "ollama", "--json"]);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("loopback"),
        "{output:?}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("config.toml")).expect("config"),
        config
    );
    assert!(!dir.path().join("state/credentials.json").exists());
}

#[test]
fn a_fresh_connection_saves_the_default_local_endpoint_without_a_key() {
    let dir = profile();
    let output = run(dir.path(), &["connect", "ollama", "--json"]);
    assert!(output.status.success(), "{output:?}");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("connection");
    assert_eq!(value["base_url"], "http://127.0.0.1:11434/v1");
    let saved: toml::Value =
        toml::from_str(&std::fs::read_to_string(dir.path().join("config.toml")).expect("config"))
            .expect("TOML");
    assert_eq!(saved["provider"].as_str(), Some("ollama"));
    assert_eq!(saved["anonymous"].as_bool(), Some(true));
    assert!(!dir.path().join("state/credentials.json").exists());
}
