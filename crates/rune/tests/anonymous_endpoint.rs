//! Anonymous connections use the real CLI and a loopback HTTP fixture.
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
fn connect_list_and_ask_an_anonymous_loopback_without_storing_a_credential() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let dir = profile();
    write_config(
        dir.path(),
        &format!(
            "provider = \"chat_completions\"\nbase_url = \"http://{address}/v1\"\nanonymous = true\napi_key_env = \"UNUSED_LOCAL_KEY\"\n[models]\nchat_completions = \"local-model\"\n"
        ),
    );

    let connected = run(dir.path(), &["connect", "chat_completions", "--json"]);
    assert!(connected.status.success(), "{connected:?}");
    let status = run(dir.path(), &["auth"]);
    assert!(status.status.success(), "{status:?}");
    assert!(String::from_utf8_lossy(&status.stdout).contains("credential anonymous"));

    let server = std::thread::spawn(move || {
        for expected_path in ["GET /v1/models", "POST /v1/chat/completions"] {
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
            assert!(line.starts_with(expected_path), "{line}");
            let mut length = 0;
            loop {
                line.clear();
                assert!(reader.read_line(&mut line).expect("header") > 0);
                if line == "\r\n" {
                    break;
                }
                let (name, value) = line.split_once(':').expect("header pair");
                assert!(!name.eq_ignore_ascii_case("authorization"), "{line}");
                assert!(!name.eq_ignore_ascii_case("x-api-key"), "{line}");
                assert!(!line.contains("must-not-be-stored-or-sent"));
                if name.eq_ignore_ascii_case("content-length") {
                    length = value.trim().parse().expect("content length");
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).expect("body");
            drop(reader);
            let (content_type, response) = if expected_path.starts_with("GET") {
                ("application/json", "{\"data\":[{\"id\":\"local-model\"}]}")
            } else {
                let request: serde_json::Value =
                    serde_json::from_slice(&body).expect("JSON request");
                assert_eq!(request["model"], "local-model");
                (
                    "text/event-stream",
                    "data: {\"choices\":[{\"delta\":{\"content\":\"local answer\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                )
            };
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).expect("response");
        }
    });
    let models = run(dir.path(), &["models", "--json"]);
    assert!(models.status.success(), "{models:?}");
    assert!(String::from_utf8_lossy(&models.stdout).contains("local-model"));
    let answer = run(dir.path(), &["ask", "--json", "say hello"]);
    assert!(answer.status.success(), "{answer:?}");
    let answer: serde_json::Value = serde_json::from_slice(&answer.stdout).expect("JSON answer");
    assert_eq!(answer["output"], "local answer");
    server.join().expect("fixture server");
    assert!(!dir.path().join("state/credentials.json").exists());
    assert!(
        !std::fs::read_to_string(dir.path().join("config.toml"))
            .expect("saved config")
            .contains("must-not-be-stored-or-sent")
    );
}

#[test]
fn an_anonymous_remote_connection_is_refused_before_saving_anything() {
    let dir = profile();
    let config = "provider = \"chat_completions\"\nbase_url = \"https://example.com/v1\"\nanonymous = true\n";
    write_config(dir.path(), config);
    let output = run(dir.path(), &["connect", "chat_completions", "--json"]);
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
