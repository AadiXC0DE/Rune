//! Total provider deadlines through the real binary and native agent loop.

#![allow(clippy::expect_used, clippy::panic)]

use std::io::{BufRead as _, Read as _, Write as _};
use std::net::TcpListener;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rune_agent::History;
use rune_agent::steering::{Cancellation, SteeringQueue};
use rune_agent::turn::{Event, Host};
use rune_core::budget::{Budget, BudgetSet, LimitName};
use rune_core::config::Layer;
use rune_core::error::{ErrorCode, Result};
use rune_net::message::ToolSpec;
use rune_net::provider::Provider;
use rune_net::transport::Endpoint;
use rune_policy::decision::Outcome;
use rune_tools::contract::{ExecutionContext, ToolOutput};

/// Spends half the budget on headers, then keeps sending output past it.
fn slow_endpoint(status: u16) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("fixture address");
    let server = std::thread::spawn(move || {
        let started = Instant::now();
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(started.elapsed() < Duration::from_secs(10), "no request");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("accept fixture request: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .expect("write timeout");
        let mut reader = std::io::BufReader::new(&mut stream);
        let mut length = 0;
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).expect("request header") > 0);
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().expect("content length");
            }
        }
        reader
            .read_exact(&mut vec![0; length])
            .expect("request body");
        drop(reader);

        std::thread::sleep(Duration::from_millis(500));
        let delta = b"data: {\"choices\":[{\"delta\":{\"content\":\"ongoing\"}}]}\n\n";
        let tail =
            b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        write!(
            stream,
            "HTTP/1.1 {status} Fixture\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            delta.len().saturating_mul(20).saturating_add(tail.len())
        )
        .expect("response head");
        for _ in 0..20 {
            if stream.write_all(delta).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = stream.write_all(tail);
    });
    (format!("http://{address}/v1"), server)
}

#[test]
fn ask_enforces_the_configured_total_deadline_for_success_and_error_bodies() {
    for status in [200, 503] {
        let (base, server) = slow_endpoint(status);
        let dir = tempfile::tempdir().expect("isolated state");
        let started = Instant::now();
        let output = Command::new(env!("CARGO_BIN_EXE_rune"))
            .env_clear()
            .env("HOME", dir.path())
            .env("RUNE_HOME", dir.path().join("state"))
            .env("XDG_CONFIG_HOME", dir.path().join("config"))
            .env("XDG_DATA_HOME", dir.path().join("data"))
            .env("RUNE_PROVIDER", "chat_completions")
            .env("RUNE_BASE_URL", base)
            .env("RUNE_MODEL", "fixture-model")
            .env("RUNE_API_KEY_ENV", "RUNE_DEADLINE_TEST_KEY")
            .env("RUNE_DEADLINE_TEST_KEY", "fixture-key")
            .current_dir(dir.path())
            .args([
                "ask",
                "--json",
                "--limit",
                "provider_request_timeout_ms=1000",
                "--limit",
                "provider_head_timeout_ms=10000",
                "respond slowly",
            ])
            .output()
            .expect("ask");
        let elapsed = started.elapsed();
        server.join().expect("fixture server");
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let result: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("JSON result");
        assert_eq!(result["error_code"], "timeout", "{result}");
        assert!(
            elapsed < Duration::from_millis(1500),
            "elapsed: {elapsed:?}"
        );
        assert!(
            elapsed >= Duration::from_millis(1000),
            "elapsed: {elapsed:?}"
        );
    }
}

struct DeadlineHost {
    endpoint: Endpoint,
    steering: SteeringQueue,
    events: Mutex<Vec<Event>>,
}

impl Host for DeadlineHost {
    fn dialect(&self) -> &dyn Provider {
        &rune_net::chat_completions::ChatCompletions
    }

    fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    fn model(&self) -> String {
        "fixture-model".to_owned()
    }

    fn instructions(&self) -> String {
        String::new()
    }

    fn tools(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    fn emit(&self, event: Event) {
        self.events.lock().expect("events").push(event);
    }

    fn execute(&self, _name: &str, _arguments: &serde_json::Value) -> Result<ToolOutput> {
        panic!("no tools requested")
    }

    fn decide(&self, _name: &str, _target: Option<&str>) -> (Outcome, String) {
        panic!("no tools requested")
    }

    fn context(&self) -> ExecutionContext {
        ExecutionContext::new(camino::Utf8PathBuf::from("/tmp"))
    }

    fn limits(&self) -> BudgetSet {
        let mut limits = BudgetSet::new();
        for (name, value) in [
            (LimitName::ProviderRequestTimeoutMs, 1000),
            (LimitName::ProviderMaxAttempts, 1),
        ] {
            limits
                .set(name, Budget::Bounded(value), Layer::User)
                .expect("limit");
        }
        limits
    }

    fn cancellation(&self) -> Cancellation {
        Cancellation::new()
    }

    fn steering(&self) -> &SteeringQueue {
        &self.steering
    }
}

#[test]
fn the_agent_stops_ongoing_output_at_the_configured_total_deadline() {
    let (base, server) = slow_endpoint(200);
    let host = DeadlineHost {
        endpoint: Endpoint::new(base, "fixture-key"),
        steering: SteeringQueue::new(8),
        events: Mutex::new(Vec::new()),
    };
    let mut history = History::new();
    history.push_user("respond slowly");
    let started = Instant::now();
    let result = rune_agent::turn::run_turn(&mut history, &host);
    let elapsed = started.elapsed();
    server.join().expect("fixture server");
    let err = result.expect_err("the ongoing stream exceeded its budget");
    assert_eq!(err.code(), ErrorCode::Timeout, "{err}");
    assert!(
        elapsed < Duration::from_millis(1500),
        "elapsed: {elapsed:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(1000),
        "elapsed: {elapsed:?}"
    );
    assert!(
        host.events
            .lock()
            .expect("events")
            .iter()
            .any(|event| matches!(event, Event::TextDelta { .. })),
        "output arrived before the timeout"
    );
}
