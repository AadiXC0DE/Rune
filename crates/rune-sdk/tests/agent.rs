//! Integration tests for the embedding API.
//!
//! Every test drives the real turn loop against a real endpoint or against a
//! host fetch the test controls. The properties under test are the ones an
//! embedder depends on: the fetch it supplies is the only client used, a
//! checkpoint carries no secret and no instructions, a malformed checkpoint is
//! refused, one turn runs at a time, and closing resolves a cancelled result.

// Integration tests assert by panicking.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::json;

use rune_core::error::ErrorCode;
use rune_sdk::agent::{
    Agent, AgentOptions, Dialect, FetchRequest, FetchResponse, HostFetch, HostImage, HostTool,
    PromptOptions, TurnResult,
};
use rune_testkit::{MockEndpoint, Script};

/// A host fetch that counts every request and forwards it to the endpoint.
#[derive(Debug)]
struct CountingFetch {
    hits: AtomicUsize,
}

impl CountingFetch {
    fn new() -> Self {
        Self {
            hits: AtomicUsize::new(0),
        }
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

impl HostFetch for CountingFetch {
    fn post(&self, request: FetchRequest) -> rune_core::error::Result<FetchResponse> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let response = agent
            .post(&request.url)
            .header("content-type", "application/json")
            .send(&request.body)
            .map_err(|err| {
                rune_core::error::RuneError::new(
                    ErrorCode::TransportFailure,
                    format!("the test fetch failed: {err}"),
                )
            })?;
        let status = response.status().as_u16();
        let mut body = Vec::new();
        response
            .into_body()
            .into_reader()
            .read_to_end(&mut body)
            .expect("read the response body");
        Ok(FetchResponse::new(status, body))
    }
}

/// A fetch that answers after a delay, so a turn can be interrupted mid-flight.
#[derive(Debug)]
struct SlowFetch {
    delay: Duration,
    hits: AtomicUsize,
}

impl HostFetch for SlowFetch {
    fn post(&self, _request: FetchRequest) -> rune_core::error::Result<FetchResponse> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(self.delay);
        Ok(FetchResponse::new(
            200,
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"late\"}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_owned(),
        ))
    }
}

/// Builds options pointed at a mock endpoint.
fn options(base_url: &str, fetch: Arc<dyn HostFetch>) -> AgentOptions {
    AgentOptions {
        api_key: "sk-embedder-credential-9271".to_owned(),
        model: Some("mock-model".to_owned()),
        instructions: Some(vec!["You are a meticulous assistant.".to_owned()]),
        tools: Vec::new(),
        base_url: base_url.to_owned(),
        dialect: Dialect::ChatCompletions,
        fetch: Some(fetch),
    }
}

#[test]
fn every_request_goes_through_the_fetch_the_embedder_supplied() {
    let endpoint = MockEndpoint::start(vec![Script::text("hello")]);
    let fetch = Arc::new(CountingFetch::new());
    let mut agent = Agent::new(options(&endpoint.base_url(), fetch.clone())).expect("agent");

    let mut turn = agent
        .prompt("say hello", PromptOptions::default())
        .expect("turn");
    let result = turn.result().expect("result");

    assert_eq!(result.stop_reason, rune_agent::turn::StopReason::Completed);
    assert!(result.text.contains("hello"), "{}", result.text);
    assert_eq!(fetch.hits(), 1, "one request, through the host fetch");
    assert_eq!(endpoint.request_count(), 1, "the endpoint saw exactly one");
}

#[test]
fn a_checkpoint_carries_no_credential_and_no_instructions() {
    let endpoint = MockEndpoint::start(vec![Script::text("noted")]);
    let fetch = Arc::new(CountingFetch::new());
    let mut agent = Agent::new(options(&endpoint.base_url(), fetch)).expect("agent");

    let mut turn = agent
        .prompt("remember the passphrase", PromptOptions::default())
        .expect("turn");
    let _ = turn.result().expect("result");

    let bytes = agent.checkpoint().expect("checkpoint");
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        !text.contains("sk-embedder-credential-9271"),
        "the checkpoint carries the credential"
    );
    assert!(
        !text.contains("meticulous assistant"),
        "the checkpoint carries the instructions"
    );
    assert!(text.contains("remember the passphrase"), "{text}");

    // The restored agent runs the conversation on, which is what makes the
    // checkpoint useful rather than merely secret-free.
    let fetch = Arc::new(CountingFetch::new());
    let mut restored =
        Agent::restore(options(&endpoint.base_url(), fetch), &bytes).expect("restore");
    let mut turn = restored
        .prompt("and again", PromptOptions::default())
        .expect("turn");
    let result = turn.result().expect("result");
    assert_eq!(result.stop_reason, rune_agent::turn::StopReason::Completed);

    // The exchanged turns travel in the checkpoint; only the credential and
    // the instructions do not.
    let body = endpoint.last_body().expect("a request body");
    assert!(body.to_string().contains("noted"), "{body}");
    assert!(body.to_string().contains("and again"), "{body}");
}

#[test]
fn a_malformed_checkpoint_is_refused() {
    let endpoint = MockEndpoint::start(vec![Script::text("hi")]);
    let fetch = Arc::new(CountingFetch::new());
    let build = || options(&endpoint.base_url(), fetch.clone());

    let err = Agent::restore(build(), b"{\"version\":1,\"history\":").expect_err("truncated JSON");
    assert_eq!(err.code(), ErrorCode::CorruptRecord);
    assert_eq!(err.detail().invariant.as_deref(), Some("checkpoint"));

    let err = Agent::restore(build(), b"[]").expect_err("not an object");
    assert_eq!(err.code(), ErrorCode::CorruptRecord);

    let err = Agent::restore(build(), b"{\"version\":9999}").expect_err("another version");
    assert_eq!(err.code(), ErrorCode::UnsupportedVersion);
    assert!(err.message().contains("9999"), "{}", err.message());

    let err = Agent::restore(build(), b"{\"version\":1,\"history\":{}}")
        .expect_err("a history of the wrong shape");
    assert_eq!(err.code(), ErrorCode::CorruptRecord);
}

#[test]
fn a_second_prompt_while_one_runs_is_refused() {
    let endpoint = MockEndpoint::start(vec![Script::text("slow")]);
    let fetch = Arc::new(SlowFetch {
        delay: Duration::from_millis(300),
        hits: AtomicUsize::new(0),
    });
    let mut agent = Agent::new(options(&endpoint.base_url(), fetch)).expect("agent");

    let mut turn = agent
        .prompt("first", PromptOptions::default())
        .expect("turn");
    let err = agent
        .prompt("second", PromptOptions::default())
        .expect_err("a second turn is refused rather than queued");
    assert_eq!(err.code(), ErrorCode::InvalidState);
    assert!(
        err.message().contains("already running"),
        "{}",
        err.message()
    );

    let result = turn.result().expect("result");
    assert_eq!(result.stop_reason, rune_agent::turn::StopReason::Completed);

    // The refusal left no trace: the next prompt is accepted.
    let mut turn = agent
        .prompt("third", PromptOptions::default())
        .expect("turn");
    let _ = turn.result().expect("result");
}

#[test]
fn closing_during_a_turn_resolves_a_cancelled_result() {
    let endpoint = MockEndpoint::start(vec![Script::text("slow")]);
    let fetch = Arc::new(SlowFetch {
        delay: Duration::from_millis(300),
        hits: AtomicUsize::new(0),
    });
    let mut agent = Agent::new(options(&endpoint.base_url(), fetch)).expect("agent");

    let mut turn = agent
        .prompt("work", PromptOptions::default())
        .expect("turn");
    agent.close().expect("close");
    assert!(agent.is_closed());

    let result = turn.result().expect("result");
    assert_eq!(result.stop_reason, rune_agent::turn::StopReason::Cancelled);

    let err = agent
        .prompt("more", PromptOptions::default())
        .expect_err("a closed agent refuses work");
    assert_eq!(err.code(), ErrorCode::InvalidState);
}

#[test]
fn a_checkpoint_taken_mid_turn_is_refused() {
    let endpoint = MockEndpoint::start(vec![Script::text("slow")]);
    let fetch = Arc::new(SlowFetch {
        delay: Duration::from_millis(200),
        hits: AtomicUsize::new(0),
    });
    let mut agent = Agent::new(options(&endpoint.base_url(), fetch)).expect("agent");

    let mut turn = agent
        .prompt("work", PromptOptions::default())
        .expect("turn");
    let err = agent.checkpoint().expect_err("mid-turn checkpoint");
    assert_eq!(err.code(), ErrorCode::InvalidState);
    let _ = turn.result().expect("result");

    assert!(agent.checkpoint().expect("a settled checkpoint").len() > 2);
}

#[test]
fn a_host_tool_runs_and_its_result_reaches_the_model() {
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("call_1", "lookup", "{\"key\":\"alpha\"}"),
        Script::text("found it"),
    ]);
    let fetch = Arc::new(CountingFetch::new());
    let mut agent_options = options(&endpoint.base_url(), fetch);
    agent_options.tools.push(HostTool::new(
        "lookup",
        "Looks a key up.",
        json!({"type": "object", "properties": {"key": {"type": "string"}}}),
        |arguments, _context| {
            let key = arguments
                .get("key")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            Ok(rune_sdk::agent::HostToolResult::text(format!(
                "value for {key}"
            )))
        },
    ));
    let mut agent = Agent::new(agent_options).expect("agent");

    let mut turn = agent
        .prompt("look alpha up", PromptOptions::default())
        .expect("turn");
    let result: TurnResult = turn.result().expect("result");

    assert_eq!(result.calls.len(), 1);
    assert!(result.calls[0].executed);
    assert_eq!(result.calls[0].output.text, "value for alpha");
    let body = endpoint.last_body().expect("a request body");
    assert!(body.to_string().contains("value for alpha"), "{body}");
}

#[test]
fn images_from_a_host_tool_are_returned_and_recorded() {
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("call_1", "shot", "{}"),
        Script::text("looks right"),
    ]);
    let fetch = Arc::new(CountingFetch::new());
    let mut agent_options = options(&endpoint.base_url(), fetch);
    agent_options.tools.push(HostTool::new(
        "shot",
        "Takes a screenshot.",
        json!({"type": "object"}),
        |_arguments, _context| {
            Ok(
                rune_sdk::agent::HostToolResult::text("captured").with_image(HostImage {
                    media_type: "image/png".to_owned(),
                    data: "aGVsbG8=".to_owned(),
                }),
            )
        },
    ));
    let mut agent = Agent::new(agent_options).expect("agent");

    let mut turn = agent
        .prompt("take a shot", PromptOptions::default())
        .expect("turn");
    let result = turn.result().expect("result");
    assert_eq!(result.images.len(), 1);
    assert_eq!(result.images[0].media_type, "image/png");
}

#[test]
fn a_tool_that_fails_is_information_rather_than_the_end_of_the_turn() {
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("call_1", "broken", "{}"),
        Script::text("that failed, so I tried something else"),
    ]);
    let fetch = Arc::new(CountingFetch::new());
    let mut agent_options = options(&endpoint.base_url(), fetch);
    agent_options.tools.push(HostTool::new(
        "broken",
        "Always fails.",
        json!({"type": "object"}),
        |_arguments, _context| Ok(rune_sdk::agent::HostToolResult::failure("it broke")),
    ));
    let mut agent = Agent::new(agent_options).expect("agent");

    let mut turn = agent
        .prompt("try it", PromptOptions::default())
        .expect("turn");
    let result = turn.result().expect("result");
    assert_eq!(result.stop_reason, rune_agent::turn::StopReason::Completed);
    assert!(result.calls[0].output.is_error);
    assert_eq!(result.steps, 2);
}

#[test]
fn a_step_limit_ends_the_turn_with_tool_calls_still_pending() {
    let endpoint = MockEndpoint::start(vec![Script::tool_call("call_1", "again", "{}")]);
    let fetch = Arc::new(CountingFetch::new());
    let mut agent_options = options(&endpoint.base_url(), fetch);
    agent_options.tools.push(HostTool::new(
        "again",
        "Always asks for another step.",
        json!({"type": "object"}),
        |_arguments, _context| Ok(rune_sdk::agent::HostToolResult::text("done")),
    ));
    let mut agent = Agent::new(agent_options).expect("agent");

    let mut turn = agent
        .prompt("loop", PromptOptions { max_steps: 2 })
        .expect("turn");
    let result = turn.result().expect("result");
    assert_eq!(result.stop_reason, rune_agent::turn::StopReason::StepLimit);
    assert_eq!(result.steps, 2);
}

#[test]
fn an_unknown_tool_name_is_answered_without_calling_anything() {
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("call_1", "nonexistent", "{}"),
        Script::text("I do not have that tool"),
    ]);
    let fetch = Arc::new(CountingFetch::new());
    let mut agent = Agent::new(options(&endpoint.base_url(), fetch)).expect("agent");

    let mut turn = agent
        .prompt("use it", PromptOptions::default())
        .expect("turn");
    let result = turn.result().expect("result");
    assert!(!result.calls[0].executed);
    assert!(result.calls[0].output.text.contains("no tool named"));
}

#[test]
fn usage_accumulates_across_the_conversation() {
    let endpoint = MockEndpoint::start(vec![Script::text("one"), Script::text("two")]);
    let fetch = Arc::new(CountingFetch::new());
    let mut agent = Agent::new(options(&endpoint.base_url(), fetch)).expect("agent");

    for prompt in ["first", "second"] {
        let mut turn = agent
            .prompt(prompt, PromptOptions::default())
            .expect("turn");
        let _ = turn.result().expect("result");
    }
    let usage = agent.usage();
    assert_eq!(usage.input_tokens, Some(10));
    assert_eq!(usage.output_tokens, Some(5));
}

#[test]
fn a_provider_rejection_is_reported_with_its_status() {
    let endpoint = MockEndpoint::start(vec![Script::Status {
        code: 401,
        body: r#"{"error":{"message":"no credential"}}"#.to_owned(),
    }]);
    let fetch = Arc::new(CountingFetch::new());
    let mut agent = Agent::new(options(&endpoint.base_url(), fetch)).expect("agent");

    let mut turn = agent
        .prompt("hello", PromptOptions::default())
        .expect("turn");
    let err = turn.result().expect_err("a rejected request");
    assert_eq!(err.code(), ErrorCode::AuthenticationRequired);
    assert!(err.message().contains("401"), "{}", err.message());
}

#[test]
fn a_conversation_runs_on_from_a_checkpoint_in_a_new_process_like_agent() {
    let endpoint = MockEndpoint::start(vec![Script::text("first answer"), Script::text("second")]);
    let fetch = Arc::new(CountingFetch::new());
    let mut agent = Agent::new(options(&endpoint.base_url(), fetch)).expect("agent");
    let mut turn = agent.prompt("one", PromptOptions::default()).expect("turn");
    let _ = turn.result().expect("result");
    let bytes = agent.checkpoint().expect("checkpoint");

    let fetch = Arc::new(CountingFetch::new());
    let mut restored =
        Agent::restore(options(&endpoint.base_url(), fetch), &bytes).expect("restore");
    let mut turn = restored
        .prompt("two", PromptOptions::default())
        .expect("turn");
    let _ = turn.result().expect("result");

    let body = endpoint.last_body().expect("a request body");
    let messages = body.get("messages").expect("messages");
    // The earlier exchange is present, so the model is not asked to answer a
    // question it has already been given.
    assert!(messages.to_string().contains("one"), "{messages}");
    assert!(messages.to_string().contains("first answer"), "{messages}");
    assert!(messages.to_string().contains("two"), "{messages}");
}
