//! Model requests go through the proxy the environment names, except to a
//! loopback host.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use rune_net::fetch::{Fetch, FetchRequest};
use rune_net::transport::env_proxy;

const MODELS: &[u8] = br#"{"data":[{"id":"m"}]}"#;

/// Reads one request head, up to the blank line.
fn read_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).expect("read") == 1 {
        head.push(byte[0]);
    }
    String::from_utf8_lossy(&head).into_owned()
}

/// Answers a request with the models body.
fn answer(stream: &mut TcpStream) {
    let head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        MODELS.len()
    );
    stream.write_all(head.as_bytes()).expect("write head");
    stream.write_all(MODELS).expect("write body");
}

/// A forward proxy that serves one connection and returns every request line it saw.
///
/// A tunnel request is accepted and the request sent inside it is answered
/// directly, so the target never needs to exist.
fn proxy_fixture(listener: TcpListener) -> std::thread::JoinHandle<Vec<String>> {
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        let mut lines = Vec::new();
        let head = read_head(&mut stream);
        lines.push(head.lines().next().unwrap_or_default().to_owned());
        if head.starts_with("CONNECT ") {
            stream
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .expect("tunnel");
            let inner = read_head(&mut stream);
            lines.push(inner.lines().next().unwrap_or_default().to_owned());
        }
        answer(&mut stream);
        lines
    })
}

/// An agent whose only proxy setting is the one `env` names.
fn agent_with(env: Vec<(&'static str, String)>) -> ureq::Agent {
    let lookup = move |name: &str| {
        env.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.clone())
    };
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .proxy(env_proxy(&lookup))
        .build()
        .into()
}

fn body_of(agent: &ureq::Agent, url: &str) -> String {
    let response = agent
        .send(FetchRequest::get(url).with_timeout(Some(Duration::from_secs(10))))
        .expect("send");
    assert_eq!(response.status, 200);
    let mut body = String::new();
    let mut reader = response.body;
    reader.read_to_string(&mut body).expect("body");
    body
}

#[test]
fn a_remote_endpoint_is_reached_through_the_proxy() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let seen = proxy_fixture(listener);
    let agent = agent_with(vec![("HTTPS_PROXY", format!("http://127.0.0.1:{port}"))]);

    let body = body_of(&agent, "http://rune-proxy-target.test/v1/models");

    assert_eq!(body.as_bytes(), MODELS);
    let lines = seen.join().expect("proxy");
    assert_eq!(
        lines,
        [
            "CONNECT rune-proxy-target.test:80 HTTP/1.1",
            "GET /v1/models HTTP/1.1"
        ]
    );
}

#[test]
fn a_loopback_endpoint_skips_the_proxy() {
    let proxy = TcpListener::bind("127.0.0.1:0").expect("bind proxy");
    proxy.set_nonblocking(true).expect("nonblocking");
    let proxy_port = proxy.local_addr().expect("addr").port();
    let target = TcpListener::bind("127.0.0.1:0").expect("bind target");
    let target_port = target.local_addr().expect("addr").port();
    let served = std::thread::spawn(move || {
        let (mut stream, _) = target.accept().expect("accept");
        let head = read_head(&mut stream);
        answer(&mut stream);
        head.lines().next().unwrap_or_default().to_owned()
    });
    let agent = agent_with(vec![(
        "HTTPS_PROXY",
        format!("http://127.0.0.1:{proxy_port}"),
    )]);

    let body = body_of(&agent, &format!("http://127.0.0.1:{target_port}/v1/models"));

    assert_eq!(body.as_bytes(), MODELS);
    assert_eq!(served.join().expect("target"), "GET /v1/models HTTP/1.1");
    assert!(
        matches!(proxy.accept(), Err(err) if err.kind() == std::io::ErrorKind::WouldBlock),
        "the proxy saw a connection for a loopback endpoint"
    );
}
