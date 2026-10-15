#![cfg(not(target_family = "wasm"))]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rune_context::mcp::ServerConfig;
use rune_context::mcp::oauth::{
    AuthorizationMetadata, LoopbackAuthorization, OAuthTokenStore, ProtectedResourceMetadata,
};
use rune_core::error::ErrorCode;
use serde_json::json;
use sha2::{Digest, Sha256};

const RESOURCE: &str = "https://mcp.example.com/team/mcp";
const CLIENT: &str = "fixture client+ü";
const CODE: &str = "private+authorization/code=ü";
const ACCESS: &str = "private-access-credential";
const REFRESH: &str = "private-refresh-credential";
const TIMEOUT: Duration = Duration::from_secs(5);

fn metadata(token_endpoint: &str) -> AuthorizationMetadata {
    let config = ServerConfig::parse(&json!({
        "name": "remote", "transport": {"transport": "http", "url": RESOURCE}
    }))
    .expect("config");
    ProtectedResourceMetadata::validate(
        &config,
        &json!({"resource": RESOURCE, "authorization_servers": ["https://auth.example.com"]}),
    )
    .expect("resource")
    .validate_authorization_server(
        "https://auth.example.com",
        &json!({
            "issuer": "https://auth.example.com",
            "authorization_endpoint": "https://auth.example.com/authorize?tenant=fixture",
            "token_endpoint": token_endpoint
        }),
    )
    .expect("authorization metadata")
}

fn start(metadata: &AuthorizationMetadata) -> LoopbackAuthorization {
    LoopbackAuthorization::start(metadata, CLIENT, "tools:read tools:write", TIMEOUT)
        .expect("start")
}

fn store(directory: &tempfile::TempDir) -> OAuthTokenStore {
    OAuthTokenStore::new(directory.path().join("oauth").to_str().expect("UTF-8"))
}

fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn decode(text: &str) -> String {
    let mut bytes = text.bytes();
    let mut decoded = Vec::new();
    while let Some(byte) = bytes.next() {
        decoded.push(match byte {
            b'+' => b' ',
            b'%' => {
                let a = char::from(bytes.next().expect("hex"))
                    .to_digit(16)
                    .expect("hex");
                let b = char::from(bytes.next().expect("hex"))
                    .to_digit(16)
                    .expect("hex");
                ((a << 4) | b) as u8
            }
            byte => byte,
        });
    }
    String::from_utf8(decoded).expect("UTF-8")
}

fn form(text: &str) -> BTreeMap<String, String> {
    text.split('&')
        .map(|pair| {
            let (key, value) = pair.split_once('=').expect("form pair");
            (decode(key), decode(value))
        })
        .collect()
}

fn authorization(flow: &LoopbackAuthorization) -> BTreeMap<String, String> {
    form(flow.authorization_url().split_once('?').expect("query").1)
}

// Decode the authorization challenge independently of the client's encoder.
fn challenge_bytes(challenge: &str) -> Vec<u8> {
    let mut bits = 0_u32;
    let mut count = 0_u32;
    let mut out = Vec::new();
    for byte in challenge.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte.saturating_sub(b'A'),
            b'a'..=b'z' => byte.saturating_sub(b'a').saturating_add(26),
            b'0'..=b'9' => byte.saturating_sub(b'0').saturating_add(52),
            b'-' => 62,
            b'_' => 63,
            _ => panic!("invalid challenge"),
        };
        bits = (bits << 6) | u32::from(value);
        count = count.saturating_add(6);
        if count >= 8 {
            count = count.saturating_sub(8);
            out.push((bits >> count) as u8);
        }
    }
    out
}

fn callback(flow: &LoopbackAuthorization, query: &str) -> JoinHandle<String> {
    let redirect = flow.redirect_uri().strip_prefix("http://").expect("HTTP");
    let (host, path) = redirect.split_once('/').expect("path");
    raw_callback(
        host,
        &format!("GET /{path}?{query} HTTP/1.1\r\nHost: {host}\r\n\r\n"),
    )
}

fn raw_callback(host: &str, request: &str) -> JoinHandle<String> {
    let mut stream = TcpStream::connect(host).expect("callback connect");
    stream
        .set_read_timeout(Some(TIMEOUT))
        .expect("read timeout");
    stream
        .write_all(request.as_bytes())
        .expect("callback request");
    std::thread::spawn(move || {
        let mut response = String::new();
        // A rejected oversized request may close while unread input remains.
        let _ = stream.read_to_string(&mut response);
        response
    })
}

fn query(state: &str) -> String {
    format!("state={}&code={}", encode(state), encode(CODE))
}

// One real token endpoint. It verifies the issued code's challenge and all
// exchange bindings before responding. Accept has a deadline to avoid a hung
// test if a client fails to make its request.
fn token_server(
    listener: TcpListener,
    expected: BTreeMap<String, String>,
    success_body: String,
) -> JoinHandle<String> {
    token_server_response(listener, expected, success_body, "200 OK", None)
}

fn token_server_response(
    listener: TcpListener,
    expected: BTreeMap<String, String>,
    success_body: String,
    success_status: &'static str,
    location: Option<String>,
) -> JoinHandle<String> {
    std::thread::spawn(move || {
        listener.set_nonblocking(true).expect("nonblocking");
        let deadline = Instant::now().checked_add(TIMEOUT).expect("deadline");
        let mut stream = loop {
            if let Ok((stream, _)) = listener.accept() {
                break stream;
            }
            assert!(Instant::now() < deadline, "token endpoint not contacted");
            std::thread::sleep(Duration::from_millis(5));
        };
        stream.set_read_timeout(Some(TIMEOUT)).expect("timeout");
        let mut reader = BufReader::new(&mut stream);
        let mut head = String::new();
        let mut length = 0;
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).expect("header") > 0);
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse::<usize>().expect("length");
            }
            head.push_str(&line);
            assert!(head.len() < 8192);
        }
        assert!(head.starts_with("POST /token HTTP/1.1\r\n"));
        assert!(
            head.to_ascii_lowercase()
                .contains("content-type: application/x-www-form-urlencoded")
        );
        assert!(length > 0 && length < 16384);
        let mut body = vec![0; length];
        reader.read_exact(&mut body).expect("body");
        let values = form(std::str::from_utf8(&body).expect("UTF-8"));
        assert_eq!(values["grant_type"], "authorization_code");
        assert_eq!(values["client_id"], expected["client_id"]);
        assert_eq!(values["redirect_uri"], expected["redirect_uri"]);
        assert_eq!(values["resource"], expected["resource"]);
        assert_eq!(values["code"], CODE);
        let verifier = &values["code_verifier"];
        assert_eq!(verifier.len(), 43);
        let valid = Sha256::digest(verifier.as_bytes()).as_slice()
            == challenge_bytes(&expected["code_challenge"]);
        let (status, body) = if valid {
            (success_status, success_body)
        } else {
            ("400 Bad Request", json!({"error": "invalid_grant", "error_description": format!("{CODE} {verifier} {ACCESS} {REFRESH}")}).to_string())
        };
        let location = location.map_or_else(String::new, |value| format!("Location: {value}\r\n"));
        write!(stream, "HTTP/1.1 {status}\r\n{location}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).expect("response");
        verifier.clone()
    })
}

fn listener() -> (TcpListener, AuthorizationMetadata) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("token listener");
    let metadata = metadata(&format!(
        "http://{}/token",
        listener.local_addr().expect("address")
    ));
    (listener, metadata)
}

fn tokens() -> String {
    json!({"access_token": ACCESS, "refresh_token": REFRESH, "token_type": "Bearer", "expires_in": 3600}).to_string()
}

fn assert_private(directory: &tempfile::TempDir) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let root = directory.path().join("oauth");
        assert_eq!(
            root.metadata().expect("directory").permissions().mode() & 0o777,
            0o700
        );
        for entry in std::fs::read_dir(root).expect("private files") {
            assert_eq!(
                entry
                    .expect("entry")
                    .metadata()
                    .expect("file")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
    #[cfg(not(unix))]
    let _ = directory;
}

#[test]
fn wrong_state_then_correct_exchange_stores_private_tokens_and_refuses_replay() {
    let (listener, metadata) = listener();
    let directory = tempfile::tempdir().expect("temp dir");
    let store = store(&directory);
    let mut flow = start(&metadata);
    let authorization = authorization(&flow);
    assert_eq!(authorization["tenant"], "fixture");
    assert_eq!(authorization["response_type"], "code");
    assert_eq!(authorization["code_challenge_method"], "S256");
    assert_eq!(authorization["scope"], "tools:read tools:write");
    assert_eq!(authorization["state"].len(), 43);
    let wrong = callback(&flow, &query("wrong-private-state"));
    let error = flow.complete(&store).expect_err("wrong state");
    assert_eq!(error.field(), Some("state"));
    assert!(!format!("{error:?} {error}").contains("wrong-private-state"));
    assert!(wrong.join().expect("callback").starts_with("HTTP/1.1 400"));
    assert!(!directory.path().join("oauth").exists());
    listener.set_nonblocking(true).expect("nonblocking");
    assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));

    let fixture = token_server(listener, authorization.clone(), tokens());
    let good = callback(&flow, &query(&authorization["state"]));
    flow.complete(&store).expect("correct exchange");
    let verifier = fixture.join().expect("token fixture");
    let response = good.join().expect("callback");
    assert!(response.starts_with("HTTP/1.1 200"));
    let stored = store
        .load(&metadata, CLIENT)
        .expect("load")
        .expect("stored");
    assert_eq!(stored.access_token(), ACCESS);
    assert_eq!(stored.refresh_token(), Some(REFRESH));
    assert_eq!(stored.expires_in(), Some(3600));
    assert_private(&directory);
    assert!(
        store
            .load(&metadata, "another-client")
            .expect("load")
            .is_none()
    );
    let replay = flow.complete(&store).expect_err("replayed callback");
    assert_eq!(replay.field(), Some("callback"));
    let host = flow
        .redirect_uri()
        .strip_prefix("http://")
        .expect("HTTP")
        .split('/')
        .next()
        .expect("host");
    assert!(TcpStream::connect(host).is_err());
    let diagnostics = format!("{flow:?} {stored:?} {replay} {replay:?} {response}");
    for secret in [ACCESS, REFRESH, CODE, &verifier, &authorization["state"]] {
        assert!(!diagnostics.contains(secret), "diagnostics leaked a secret");
    }
    assert!(!flow.authorization_url().contains(&verifier));
}

#[test]
fn verifier_from_another_flow_is_rejected_and_failure_consumes_callback() {
    let (listener, metadata) = listener();
    let directory = tempfile::tempdir().expect("temp dir");
    let store = store(&directory);
    let first = start(&metadata);
    let mut flow = start(&metadata);
    let first_parameters = authorization(&first);
    let mut expected = authorization(&flow);
    assert_ne!(expected["state"], first_parameters["state"]);
    assert_ne!(
        expected["code_challenge"],
        first_parameters["code_challenge"]
    );
    // The code was issued for the first flow's challenge. A verifier from the
    // second flow must not redeem it, even with that flow's correct state.
    expected.insert(
        "code_challenge".into(),
        first_parameters["code_challenge"].clone(),
    );
    let fixture = token_server(listener, expected, tokens());
    let callback = callback(&flow, &query(&authorization(&flow)["state"]));
    let error = flow.complete(&store).expect_err("incorrect verifier");
    assert_eq!(error.field(), Some("token_exchange"));
    let verifier = fixture.join().expect("fixture");
    let response = callback.join().expect("callback");
    assert!(response.starts_with("HTTP/1.1 400"));
    assert!(store.load(&metadata, CLIENT).expect("load").is_none());
    assert!(
        flow.complete(&store)
            .expect_err("consumed")
            .message()
            .contains("consumed")
    );
    let diagnostics = format!("{error} {error:?} {flow:?} {response}");
    for secret in [CODE, &verifier, ACCESS, REFRESH] {
        assert!(!diagnostics.contains(secret));
    }
}

#[test]
fn malformed_callback_targets_and_duplicate_parameters_never_exchange_codes() {
    let (listener, metadata) = listener();
    let directory = tempfile::tempdir().expect("temp dir");
    let store = store(&directory);
    let mut flow = start(&metadata);
    let state = authorization(&flow)["state"].clone();
    let host = flow
        .redirect_uri()
        .strip_prefix("http://")
        .expect("HTTP")
        .split('/')
        .next()
        .expect("host")
        .to_owned();
    for request in [
        format!(
            "GET /other?{} HTTP/1.1\r\nHost: {host}\r\n\r\n",
            query(&state)
        ),
        format!(
            "GET /oauth/callback?{} HTTP/1.1\r\nHost: attacker.example\r\n\r\n",
            query(&state)
        ),
        format!(
            "GET /oauth/callback?{}&st%61te={} HTTP/1.1\r\nHost: {host}\r\n\r\n",
            query(&state),
            state
        ),
        format!(
            "GET /oauth/callback?{}&code=duplicate HTTP/1.1\r\nHost: {host}\r\n\r\n",
            query(&state)
        ),
        format!("GET /oauth/callback?state=%ZZ HTTP/1.1\r\nHost: {host}\r\n\r\n"),
        format!(
            "POST /oauth/callback?{} HTTP/1.1\r\nHost: {host}\r\n\r\n",
            query(&state)
        ),
        format!(
            "GET /oauth/callback?{} HTTP/1.1\r\nHost: {host}\r\nHost: {host}\r\n\r\n",
            query(&state)
        ),
    ] {
        let callback = raw_callback(&host, &request);
        let error = flow.complete(&store).expect_err("invalid callback");
        assert!(!format!("{error} {error:?}").contains(CODE));
        assert!(
            callback
                .join()
                .expect("callback")
                .starts_with("HTTP/1.1 400")
        );
    }
    listener.set_nonblocking(true).expect("nonblocking");
    assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    assert!(!directory.path().join("oauth").exists());
}

#[test]
fn rejected_authorization_consumes_callback_without_contacting_token_endpoint() {
    let (listener, metadata) = listener();
    let directory = tempfile::tempdir().expect("temp dir");
    let store = store(&directory);
    let mut flow = start(&metadata);
    let callback = callback(
        &flow,
        &format!(
            "state={}&error=access_denied&error_description={ACCESS}",
            authorization(&flow)["state"]
        ),
    );
    let error = flow.complete(&store).expect_err("denied");
    assert_eq!(error.field(), Some("authorization"));
    assert!(!format!("{error} {error:?}").contains(ACCESS));
    assert!(
        callback
            .join()
            .expect("callback")
            .starts_with("HTTP/1.1 400")
    );
    assert!(flow.complete(&store).is_err());
    listener.set_nonblocking(true).expect("nonblocking");
    assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
}

#[test]
fn invalid_and_oversized_token_bodies_are_not_stored_or_printed() {
    for body in [
        format!("{{\"access_token\":\"{ACCESS}\",\"token_type\":\"{REFRESH}\"}}"),
        format!(
            "{{\"access_token\":\"{ACCESS}\",\"token_type\":\"Bearer\",\"expires_in\":\"{REFRESH}\"}}"
        ),
        format!("{ACCESS} {REFRESH}"),
        json!({"access_token": "x".repeat(65536), "token_type": "Bearer"}).to_string(),
    ] {
        let (listener, metadata) = listener();
        let directory = tempfile::tempdir().expect("temp dir");
        let store = store(&directory);
        let mut flow = start(&metadata);
        let fixture = token_server(listener, authorization(&flow), body);
        let callback = callback(&flow, &query(&authorization(&flow)["state"]));
        let error = flow.complete(&store).expect_err("bad token response");
        fixture.join().expect("fixture");
        callback.join().expect("callback");
        let diagnostics = format!("{error} {error:?}");
        assert!(!diagnostics.contains(ACCESS));
        assert!(!diagnostics.contains(REFRESH));
        assert!(store.load(&metadata, CLIENT).expect("load").is_none());
        assert!(flow.complete(&store).is_err());
    }
}

#[test]
fn callback_wait_and_silent_read_obey_original_deadline() {
    let metadata = metadata("https://tokens.example.com/token");
    let directory = tempfile::tempdir().expect("temp dir");
    let store = store(&directory);
    for silent_connection in [false, true] {
        let mut flow =
            LoopbackAuthorization::start(&metadata, CLIENT, "", Duration::from_millis(50))
                .expect("start");
        let host = flow
            .redirect_uri()
            .strip_prefix("http://")
            .expect("HTTP")
            .split('/')
            .next()
            .expect("host");
        let _silent = silent_connection.then(|| TcpStream::connect(host).expect("silent callback"));
        let began = Instant::now();
        let error = flow.complete(&store).expect_err("deadline");
        if !silent_connection {
            assert_eq!(error.code(), ErrorCode::Timeout);
        }
        assert!(began.elapsed() < Duration::from_secs(1));
        assert!(store.load(&metadata, CLIENT).expect("load").is_none());
    }
}

#[test]
fn token_redirect_cannot_forward_the_code_or_verifier() {
    let (listener, metadata) = listener();
    let destination = TcpListener::bind("127.0.0.1:0").expect("redirect destination");
    let location = format!(
        "http://{}/stolen",
        destination.local_addr().expect("address")
    );
    let directory = tempfile::tempdir().expect("temp dir");
    let store = store(&directory);
    let mut flow = start(&metadata);
    let fixture = token_server_response(
        listener,
        authorization(&flow),
        tokens(),
        "307 Temporary Redirect",
        Some(location),
    );
    let callback = callback(&flow, &query(&authorization(&flow)["state"]));
    assert_eq!(
        flow.complete(&store)
            .expect_err("redirect rejected")
            .field(),
        Some("token_exchange")
    );
    fixture.join().expect("fixture");
    callback.join().expect("callback");
    destination.set_nonblocking(true).expect("nonblocking");
    assert!(matches!(destination.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    assert!(store.load(&metadata, CLIENT).expect("load").is_none());
}

#[test]
fn oversized_callback_is_bounded_and_can_be_followed_by_a_correct_callback() {
    let (listener, metadata) = listener();
    let directory = tempfile::tempdir().expect("temp dir");
    let store = store(&directory);
    let mut flow = start(&metadata);
    let host = flow
        .redirect_uri()
        .strip_prefix("http://")
        .expect("HTTP")
        .split('/')
        .next()
        .expect("host")
        .to_owned();
    let callback = raw_callback(
        &host,
        &format!(
            "GET /oauth/callback?padding={} HTTP/1.1\r\nHost: {host}\r\n\r\n",
            "x".repeat(8192)
        ),
    );
    assert!(
        flow.complete(&store)
            .expect_err("callback too large")
            .message()
            .contains("too large")
    );
    callback.join().expect("callback");
    let fixture = token_server(listener, authorization(&flow), tokens());
    let callback = self::callback(&flow, &query(&authorization(&flow)["state"]));
    flow.complete(&store)
        .expect("correct callback after oversized input");
    fixture.join().expect("fixture");
    assert!(
        callback
            .join()
            .expect("callback")
            .starts_with("HTTP/1.1 200")
    );
}

#[cfg(unix)]
#[test]
fn unsafe_token_storage_fails_without_replay_or_overwriting_a_link_target() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let directory = tempfile::tempdir().expect("temp dir");
    let store = store(&directory);
    let root = directory.path().join("oauth");
    // Establish the private filename through the public flow.
    let (listener, metadata) = listener();
    let mut flow = start(&metadata);
    let fixture = token_server(listener, authorization(&flow), tokens());
    let callback = callback(&flow, &query(&authorization(&flow)["state"]));
    flow.complete(&store).expect("initial tokens");
    fixture.join().expect("fixture");
    callback.join().expect("callback");
    let token_file = std::fs::read_dir(&root)
        .expect("files")
        .map(|e| e.expect("entry").path())
        .find(|p| p.extension().is_some_and(|e| e == "json"))
        .expect("token file");
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755))
        .expect("widen directory");
    assert_eq!(
        store
            .load(&metadata, CLIENT)
            .expect_err("widened directory")
            .code(),
        ErrorCode::UnsafePath
    );
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
        .expect("restore directory");
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o644))
        .expect("widen file");
    assert_eq!(
        store
            .load(&metadata, CLIENT)
            .expect_err("widened file")
            .code(),
        ErrorCode::UnsafePath
    );
    std::fs::remove_file(&token_file).expect("remove tokens");
    let target = directory.path().join("sentinel");
    std::fs::write(&target, "KEEP_PRIVATE_STORAGE_TARGET").expect("sentinel");
    symlink(&target, &token_file).expect("unsafe token link");
    assert_eq!(
        store
            .load(&metadata, CLIENT)
            .expect_err("linked file")
            .code(),
        ErrorCode::UnsafePath
    );
    let (listener, metadata) = self::listener();
    let mut flow = start(&metadata);
    let fixture = token_server(listener, authorization(&flow), tokens());
    let callback = self::callback(&flow, &query(&authorization(&flow)["state"]));
    assert_eq!(
        flow.complete(&store).expect_err("unsafe store").code(),
        ErrorCode::UnsafePath
    );
    fixture.join().expect("fixture");
    assert!(
        callback
            .join()
            .expect("callback")
            .starts_with("HTTP/1.1 400")
    );
    assert!(
        flow.complete(&store)
            .expect_err("consumed")
            .message()
            .contains("consumed")
    );
    assert_eq!(
        std::fs::read_to_string(target).expect("sentinel"),
        "KEEP_PRIVATE_STORAGE_TARGET"
    );
}
