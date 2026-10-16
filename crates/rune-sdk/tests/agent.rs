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

use std::io::{Cursor, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use rune_agent::turn::{Event, StopReason};

use serde_json::json;

use rune_core::budget::{Budget, BudgetSet, LimitName};
use rune_core::config::Layer;
use rune_core::error::ErrorCode;
use rune_sdk::agent::{
    Agent, AgentOptions, Dialect, EVENT_QUEUE_CAPACITY, FetchRequest, FetchResponse, HostFetch,
    HostImage, HostTool, PromptOptions, TurnResult,
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
        // The live reader, not a buffered copy: the turn must see each event as
        // the endpoint produces it.
        Ok(FetchResponse {
            status,
            body: Box::new(response.into_body().into_reader()),
        })
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
fn native_gemini_executes_a_host_tool_and_replays_its_signature() {
    struct GeminiFetch(AtomicUsize);
    impl HostFetch for GeminiFetch {
        fn post(&self, request: FetchRequest) -> rune_core::error::Result<FetchResponse> {
            assert_eq!(
                request.url,
                "https://example.invalid/v1beta/models/gemini-fixture:streamGenerateContent?alt=sse"
            );
            assert_eq!(
                request.header("x-goog-api-key"),
                Some("sk-embedder-credential-9271")
            );
            assert!(request.header("authorization").is_none());
            let body: serde_json::Value = serde_json::from_str(&request.body).expect("body");
            let frame = if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                assert_eq!(
                    body["tools"][0]["functionDeclarations"][0]["name"],
                    "lookup"
                );
                json!({"candidates": [{"content": {"parts": [{"text": "looking up"}, {"functionCall": {"name": "lookup", "args": {"key": "alpha"}}, "thoughtSignature": "signed-call"}]}, "finishReason": "STOP"}], "usageMetadata": {"promptTokenCount": 12, "candidatesTokenCount": 3}})
            } else {
                assert_eq!(
                    body["contents"][1]["parts"][1]["thoughtSignature"],
                    "signed-call"
                );
                assert_eq!(
                    body["contents"][2]["parts"][0]["functionResponse"]["response"]["output"],
                    "value for alpha"
                );
                json!({"candidates": [{"content": {"parts": [{"text": "found it"}]}, "finishReason": "STOP"}], "usageMetadata": {"promptTokenCount": 20, "candidatesTokenCount": 4}})
            };
            Ok(FetchResponse {
                status: 200,
                body: Box::new(Cursor::new(format!("data: {frame}\n\n").into_bytes())),
            })
        }
    }
    let fetch = Arc::new(GeminiFetch(AtomicUsize::new(0)));
    let mut opts = options("https://example.invalid/v1beta", fetch.clone());
    opts.model = Some("gemini-fixture".to_owned());
    opts.dialect = Dialect::Gemini;
    opts.tools.push(HostTool::new(
        "lookup",
        "Look up a key",
        json!({"type": "object", "properties": {"key": {"type": "string"}}}),
        |args, _| {
            assert_eq!(args["key"], "alpha");
            Ok(rune_sdk::agent::HostToolResult::text("value for alpha"))
        },
    ));
    let mut agent = Agent::new(opts).expect("agent");
    let mut turn = agent
        .prompt("look up alpha", PromptOptions::default())
        .expect("turn");
    let result = turn.result().expect("result");
    assert_eq!(result.text, "looking upfound it");
    assert_eq!(result.calls.len(), 1);
    assert!(result.calls[0].executed);
    assert_eq!(result.usage.input_tokens, Some(32));
    assert_eq!(result.usage.output_tokens, Some(7));
    assert_eq!(fetch.0.load(Ordering::SeqCst), 2);
}

#[test]
fn an_explicitly_anonymous_agent_completes_without_authentication_headers() {
    struct AnonymousFetch(CountingFetch);
    impl HostFetch for AnonymousFetch {
        fn post(&self, request: FetchRequest) -> rune_core::error::Result<FetchResponse> {
            assert!(!request.headers.iter().any(|(name, _)| name.is_empty()
                || name.eq_ignore_ascii_case("authorization")
                || name.eq_ignore_ascii_case("x-api-key")));
            self.0.post(request)
        }
    }
    let endpoint = MockEndpoint::start(vec![Script::text("anonymous response")]);
    let fetch = Arc::new(AnonymousFetch(CountingFetch::new()));
    let mut options = options(&endpoint.base_url(), fetch);
    options.api_key.clear();
    let mut agent = Agent::new_anonymous(options).expect("anonymous agent");
    let result = agent
        .prompt("hello", PromptOptions::default())
        .expect("turn")
        .result()
        .expect("response");
    assert_eq!(result.stop_reason, StopReason::Completed);
    assert_eq!(result.text, "anonymous response");
    assert_eq!(endpoint.request_count(), 1);
}

#[test]
fn anonymous_agent_uses_default_transport() {
    let endpoint = MockEndpoint::start(vec![Script::text("anonymous response")]);
    let mut options = options(&endpoint.base_url(), Arc::new(CountingFetch::new()));
    options.api_key.clear();
    options.fetch = None;
    let mut agent = Agent::new_anonymous(options).expect("agent");
    let result = agent
        .prompt("hello", PromptOptions::default())
        .expect("turn")
        .result()
        .expect("response");
    assert_eq!(result.text, "anonymous response");
    assert_eq!(endpoint.request_count(), 1);
}

#[test]
fn anonymity_is_explicit_and_refuses_remote_endpoints_or_credentials() {
    let fetch = Arc::new(CountingFetch::new());
    let mut local = options("http://127.0.0.1:8000/v1", fetch.clone());
    local.api_key.clear();
    assert_eq!(
        Agent::new(local)
            .expect_err("credential still required")
            .field(),
        Some("api_key")
    );

    let mut remote = options("https://example.com/v1", fetch.clone());
    remote.api_key.clear();
    assert_eq!(
        Agent::new_anonymous(remote)
            .expect_err("remote refused")
            .field(),
        Some("anonymous")
    );
    let local = options("http://127.0.0.1:8000/v1", fetch.clone());
    assert_eq!(
        Agent::new_anonymous(local)
            .expect_err("credential refused")
            .field(),
        Some("api_key")
    );
    assert_eq!(fetch.hits(), 0);
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

    assert_eq!(result.stop_reason, StopReason::Completed);
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
    assert_eq!(result.stop_reason, StopReason::Completed);

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
    assert_eq!(result.stop_reason, StopReason::Completed);

    // The refusal left no trace: the next prompt is accepted.
    let mut turn = agent
        .prompt("third", PromptOptions::default())
        .expect("turn");
    let _ = turn.result().expect("result");
}

#[test]
fn replacing_a_finished_turn_handle_leaves_the_new_turn_running() {
    let endpoint = MockEndpoint::start(vec![Script::text("unused")]);
    let fetch = Arc::new(SlowFetch {
        delay: Duration::from_millis(200),
        hits: AtomicUsize::new(0),
    });
    let mut agent = Agent::new(options(&endpoint.base_url(), fetch)).expect("agent");

    let mut turn = agent.prompt("one", PromptOptions::default()).expect("turn");
    let first = turn.result().expect("result");
    assert_eq!(first.stop_reason, StopReason::Completed);

    // The assignment drops the finished handle while the new turn is waiting
    // on its request, which must cancel only the turn the handle belonged to.
    turn = agent.prompt("two", PromptOptions::default()).expect("turn");
    let second = turn.result().expect("result");
    assert_eq!(second.stop_reason, StopReason::Completed);
    assert!(second.text.contains("late"), "{}", second.text);
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
    assert_eq!(result.stop_reason, StopReason::Cancelled);

    let err = agent
        .prompt("more", PromptOptions::default())
        .expect_err("a closed agent refuses work");
    assert_eq!(err.code(), ErrorCode::InvalidState);
}

#[test]
fn time_spent_in_the_host_fetch_consumes_the_total_request_budget() {
    let fetch = Arc::new(SlowFetch {
        delay: Duration::from_millis(1100),
        hits: AtomicUsize::new(0),
    });
    let mut agent = Agent::new(options("https://example.invalid/v1", fetch))
        .expect("agent")
        .with_limits(limit(LimitName::ProviderRequestTimeoutMs, 1000));
    let mut turn = agent
        .prompt("work", PromptOptions::default())
        .expect("turn");
    let err = turn.result().expect_err("the request budget was exhausted");
    assert_eq!(err.code(), ErrorCode::Timeout, "{err}");
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
    assert_eq!(result.stop_reason, StopReason::Completed);
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
    assert_eq!(result.stop_reason, StopReason::StepLimit);
    assert_eq!(result.steps, 2);
}

#[test]
fn prompt_step_limits_inherit_agent_limits_unless_overridden() {
    for (budget, prompt_options, expected_steps, expected_stop) in [
        (
            Budget::Bounded(2),
            PromptOptions::default(),
            2,
            StopReason::StepLimit,
        ),
        (
            Budget::Bounded(2),
            PromptOptions { max_steps: 1 },
            1,
            StopReason::StepLimit,
        ),
        (
            Budget::Bounded(1),
            PromptOptions { max_steps: 3 },
            3,
            StopReason::StepLimit,
        ),
        (
            Budget::Bounded(0),
            PromptOptions::default(),
            4,
            StopReason::Completed,
        ),
        (
            Budget::Unbounded,
            PromptOptions::default(),
            4,
            StopReason::Completed,
        ),
    ] {
        let endpoint = MockEndpoint::start(vec![
            Script::tool_call("call_1", "again", "{}"),
            Script::tool_call("call_2", "again", "{}"),
            Script::tool_call("call_3", "again", "{}"),
            Script::text("finished"),
        ]);
        let fetch = Arc::new(CountingFetch::new());
        let mut agent_options = options(&endpoint.base_url(), fetch.clone());
        agent_options.tools.push(HostTool::new(
            "again",
            "Asks for another step.",
            json!({"type": "object"}),
            |_arguments, _context| Ok(rune_sdk::agent::HostToolResult::text("done")),
        ));
        let mut limits = BudgetSet::new();
        limits
            .set(LimitName::MaxAgentSteps, budget, Layer::CommandLine)
            .expect("valid step limit");
        let mut agent = Agent::new(agent_options)
            .expect("agent")
            .with_limits(limits);

        let mut turn = agent.prompt("loop", prompt_options).expect("turn");
        let result = turn.result().expect("result");

        assert_eq!(
            result.stop_reason, expected_stop,
            "{budget}, {prompt_options:?}"
        );
        assert_eq!(result.steps, expected_steps, "{budget}, {prompt_options:?}");
        assert_eq!(fetch.hits(), expected_steps as usize);
        assert_eq!(endpoint.request_count(), expected_steps as usize);
        assert_eq!(result.calls.len(), expected_steps.min(3) as usize);
        assert!(result.calls.iter().all(|call| call.executed));
    }
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

/// Builds a limits set with one override.
fn limit(name: LimitName, value: u64) -> BudgetSet {
    let mut set = BudgetSet::new();
    set.set(name, Budget::Bounded(value), Layer::CommandLine)
        .expect("a valid limit");
    set
}

#[test]
fn a_tool_result_larger_than_its_bound_is_truncated_for_the_model() {
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("call_1", "flood", "{}"),
        Script::text("that was long"),
    ]);
    let fetch = Arc::new(CountingFetch::new());
    let mut agent_options = options(&endpoint.base_url(), fetch);
    agent_options.tools.push(HostTool::new(
        "flood",
        "Returns more than the bound allows.",
        json!({"type": "object"}),
        |_arguments, _context| Ok(rune_sdk::agent::HostToolResult::text("x".repeat(20_000))),
    ));
    let mut agent = Agent::new(agent_options)
        .expect("agent")
        .with_limits(limit(LimitName::MaxToolResultBytes, 4_096));

    let mut turn = agent
        .prompt("flood", PromptOptions::default())
        .expect("turn");
    let result = turn.result().expect("result");
    let recorded = &result.calls[0].output;
    assert!(recorded.text.len() < 20_000, "{}", recorded.text.len());
    assert!(recorded.text.ends_with("(truncated)"), "{}", recorded.text);
    // The full size is still reported, so a host accounts for what was produced.
    assert_eq!(recorded.produced_bytes, 20_000);

    let body = endpoint.last_body().expect("a request body");
    assert!(
        !body.to_string().contains(&"x".repeat(8_000)),
        "the model saw the truncated text"
    );
}

#[test]
fn checkpoint_creation_obeys_the_serialized_history_bound() {
    let endpoint = MockEndpoint::start(vec![Script::text("noted")]);
    let fetch = Arc::new(CountingFetch::new());
    let build = || options(&endpoint.base_url(), fetch.clone());
    let mut agent = Agent::new(build()).expect("agent");
    let input = "\n\"\\é".repeat(100);
    let mut turn = agent
        .prompt(&input, PromptOptions::default())
        .expect("turn");
    let result = turn.result().expect("result");
    let bytes = agent.checkpoint().expect("checkpoint under default cap");
    let raw_bytes = input.len() + result.text.len();
    assert!(raw_bytes < bytes.len(), "JSON escaping and metadata count");

    for cap in [0, raw_bytes, bytes.len() - 1] {
        let limits = limit(LimitName::PromptHistoryBytes, cap as u64);
        agent = agent.with_limits(limits.clone());
        let err = agent
            .checkpoint()
            .map(|bytes| bytes.len())
            .expect_err("oversized checkpoint creation");
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert_eq!(err.field(), Some("checkpoint"));
        assert_eq!(err.detail().observed, Some(bytes.len().to_string()));
        let restore_err = Agent::restore_with_limits(build(), &bytes, limits)
            .expect_err("the same cap refuses restoration");
        assert_eq!(err, restore_err);
        assert_eq!(agent.usage(), result.usage, "refusal preserves usage");
    }

    let limits = limit(LimitName::PromptHistoryBytes, bytes.len() as u64);
    agent = agent.with_limits(limits.clone());
    assert_eq!(agent.checkpoint().expect("exact cap is allowed"), bytes);
    let restored = Agent::restore_with_limits(build(), &bytes, limits)
        .expect("checkpoint restores under the same cap");
    assert_eq!(restored.checkpoint().expect("restored checkpoint"), bytes);
    assert_eq!(restored.usage(), result.usage);
}

#[test]
fn checkpoint_creation_obeys_the_default_history_bound() {
    let endpoint = MockEndpoint::start(vec![Script::text("noted")]);
    let fetch = Arc::new(CountingFetch::new());
    let mut agent = Agent::new(options(&endpoint.base_url(), fetch)).expect("agent");
    let cap = BudgetSet::new().get_usize(LimitName::PromptHistoryBytes);
    let mut turn = agent
        .prompt("x".repeat(cap), PromptOptions::default())
        .expect("turn");
    assert_eq!(
        turn.result().expect("result").stop_reason,
        StopReason::Completed
    );

    let err = agent
        .checkpoint()
        .map(|bytes| bytes.len())
        .expect_err("default cap applies to creation");
    assert_eq!(err.code(), ErrorCode::TooLarge);
    assert_eq!(err.field(), Some("checkpoint"));

    agent = agent.with_limits(limit(LimitName::PromptHistoryBytes, (cap * 2) as u64));
    let bytes = agent.checkpoint().expect("raised cap allows creation");
    assert!(bytes.len() > cap);
    assert_eq!(err.detail().observed, Some(bytes.len().to_string()));
}

#[test]
fn a_checkpoint_larger_than_the_history_bound_is_refused() {
    // Two mebibytes, past the one-mebibyte default history bound.
    let padded = format!(
        "{{\"version\":1,\"history\":{{\"instructions\":\"\",\"turns\":[],\"next_seq\":1}},\"usage\":{{}},\"pad\":\"{}\"}}",
        "y".repeat(2 * 1024 * 1024)
    );
    let endpoint = MockEndpoint::start(vec![Script::text("hi")]);
    let base_url = endpoint.base_url();
    let build = || options(&base_url, Arc::new(CountingFetch::new()));

    let err = Agent::restore(build(), padded.as_bytes()).expect_err("an oversized checkpoint");
    assert_eq!(err.code(), ErrorCode::TooLarge);
    assert_eq!(err.field(), Some("checkpoint"));

    // The same payload is accepted once the bound is raised, which is what
    // shows the refusal is the bound rather than the payload.
    let raised = limit(LimitName::PromptHistoryBytes, 4 * 1024 * 1024);
    let mut restored =
        Agent::restore_with_limits(build(), padded.as_bytes(), raised).expect("a raised bound");
    let mut turn = restored
        .prompt("and on", PromptOptions::default())
        .expect("turn");
    let _ = turn.result().expect("result");
}

#[test]
fn the_event_stream_reports_the_shape_of_the_turn() {
    let endpoint = MockEndpoint::start(vec![Script::text("answered")]);
    let fetch = Arc::new(CountingFetch::new());
    let mut agent = Agent::new(options(&endpoint.base_url(), fetch)).expect("agent");

    let mut turn = agent
        .prompt("hello", PromptOptions::default())
        .expect("turn");
    let mut steps = Vec::new();
    let mut text = String::new();
    let mut finished = false;
    for event in turn.events() {
        match event {
            Event::TurnStarted { step } => steps.push(step),
            Event::TextDelta { delta } => text.push_str(&delta),
            Event::Finished { .. } => finished = true,
            _ => {}
        }
    }
    assert_eq!(steps, vec![1]);
    assert_eq!(text, "answered");
    assert!(finished);

    // The iterator ends when the turn does, and the result is still readable.
    let result = turn.result().expect("result");
    assert_eq!(result.text, "answered");
}

#[test]
fn every_request_of_a_multi_step_turn_goes_through_the_host_fetch() {
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("call_1", "lookup", "{}"),
        Script::text("done"),
    ]);
    let fetch = Arc::new(CountingFetch::new());
    let mut agent_options = options(&endpoint.base_url(), fetch.clone());
    agent_options.tools.push(HostTool::new(
        "lookup",
        "Looks something up.",
        json!({"type": "object"}),
        |_arguments, _context| Ok(rune_sdk::agent::HostToolResult::text("value")),
    ));
    let mut agent = Agent::new(agent_options).expect("agent");

    let mut turn = agent.prompt("go", PromptOptions::default()).expect("turn");
    let result = turn.result().expect("result");
    assert_eq!(result.steps, 2);
    assert_eq!(fetch.hits(), 2, "both steps used the host fetch");
    assert_eq!(endpoint.request_count(), 2, "and nothing else reached it");
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
    // Each turn reports ten in and five out, and each was billed separately.
    let usage = agent.usage();
    assert_eq!(usage.input_tokens, Some(20));
    assert_eq!(usage.output_tokens, Some(10));
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

/// Serves a real Retry-After header followed by a successful streamed response.
fn rate_limit_endpoint() -> (String, std::thread::JoinHandle<Vec<Instant>>) {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").expect("listen");
    let address = listener.local_addr().expect("address");
    listener.set_nonblocking(true).expect("nonblocking");
    let worker = std::thread::spawn(move || {
        let Script::Frames(frames) = Script::text("retried") else {
            panic!("expected frames");
        };
        let mut success = String::new();
        for frame in frames {
            success.push_str("data: ");
            success.push_str(&frame);
            success.push_str("\n\n");
        }
        let responses = [
            (429, "retry-after: 1\r\n", "busy"),
            (200, "", success.as_str()),
        ];
        let mut received = Vec::new();
        let started = Instant::now();
        for (status, headers, body) in responses {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            started.elapsed() < Duration::from_secs(10),
                            "request never arrived"
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
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("timeout");
            let mut reader = BufReader::new(&mut stream);
            let mut content_length = 0;
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).expect("header") > 0);
                if line == "\r\n" {
                    break;
                }
                if let Some(length) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = length.trim().parse::<usize>().expect("length");
                }
            }
            reader
                .read_exact(&mut vec![0; content_length])
                .expect("body");
            received.push(Instant::now());
            write!(stream,
                "HTTP/1.1 {status} Response\r\n{headers}content-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            ).expect("response");
        }
        received
    });
    (format!("http://{address}/v1"), worker)
}

#[test]
fn a_429_retry_after_is_honored_once_without_consuming_another_step() {
    let (base_url, worker) = rate_limit_endpoint();
    let mut agent_options = options(&base_url, Arc::new(CountingFetch::new()));
    agent_options.fetch = None;
    let mut agent = Agent::new(agent_options)
        .expect("agent")
        .with_limits(limit(LimitName::ProviderMaxAttempts, 2));
    let mut turn = agent
        .prompt("hello", PromptOptions { max_steps: 1 })
        .expect("turn");
    let result = turn.result().expect("retry succeeds");
    let events: Vec<_> = turn.events().collect();
    let received = worker.join().expect("endpoint");

    assert_eq!(received.len(), 2);
    assert!(received[1].duration_since(received[0]) >= Duration::from_secs(1));
    assert_eq!(result.stop_reason, StopReason::Completed);
    assert_eq!(result.text, "retried");
    assert_eq!(result.steps, 1);
    assert_eq!(result.usage.input_tokens, Some(10));
    assert_eq!(agent.usage(), result.usage);
    assert!(
        matches!(events.iter().find(|event| matches!(event, Event::ProviderRetry { .. })),
        Some(Event::ProviderRetry { step: 1, next_attempt: 2, max_attempts: 2, delay })
        if *delay == Duration::from_secs(1))
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::ProviderRetry { .. }))
            .count(),
        1
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::StepRestarted { step: 1 }))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::ProviderRetryFinished { step: 1 }))
    );
}

#[derive(Debug, Default)]
struct RateLimitedFetch {
    hits: AtomicUsize,
}

impl HostFetch for RateLimitedFetch {
    fn post(&self, _request: FetchRequest) -> rune_core::error::Result<FetchResponse> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        Ok(FetchResponse::new(429, "busy"))
    }

    fn post_with_retry_after(
        &self,
        request: FetchRequest,
    ) -> rune_core::error::Result<(FetchResponse, Option<String>)> {
        self.post(request)
            .map(|response| (response, Some("60".to_owned())))
    }
}

fn waiting_for_retry(turn: &mut rune_sdk::agent::Turn) {
    loop {
        if let Event::ProviderRetry { delay, .. } = turn.next_event().expect("retry event") {
            assert_eq!(delay, Duration::from_secs(60));
            return;
        }
    }
}

#[test]
fn closing_interrupts_retry_after_and_preserves_a_usable_checkpoint() {
    let fetch = Arc::new(RateLimitedFetch::default());
    let mut agent =
        Agent::new(options("https://example.invalid/v1", fetch.clone())).expect("agent");
    let mut turn = agent
        .prompt("work", PromptOptions::default())
        .expect("turn");
    waiting_for_retry(&mut turn);
    let started = Instant::now();
    agent.close().expect("close");
    let result = turn.result().expect("cancelled result");
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(result.stop_reason, StopReason::Cancelled);
    assert_eq!(fetch.hits.load(Ordering::SeqCst), 1);
    assert!(result.text.is_empty());
    assert_eq!(result.usage, rune_net::stream::Usage::default());
    agent.checkpoint().expect("settled checkpoint");
}

#[test]
fn dropping_the_turn_interrupts_retry_after_and_releases_the_agent() {
    let fetch = Arc::new(RateLimitedFetch::default());
    let mut agent =
        Agent::new(options("https://example.invalid/v1", fetch.clone())).expect("agent");
    let mut turn = agent
        .prompt("work", PromptOptions::default())
        .expect("turn");
    waiting_for_retry(&mut turn);
    let started = Instant::now();
    drop(turn);
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(fetch.hits.load(Ordering::SeqCst), 1);
    agent.checkpoint().expect("settled checkpoint");
    let next = agent
        .prompt("again", PromptOptions::default())
        .expect("agent available");
    drop(next);
}

#[test]
fn retry_exhaustion_stops_at_the_configured_attempt_count() {
    let endpoint = MockEndpoint::start(vec![Script::Status {
        code: 503,
        body: "busy".to_owned(),
    }]);
    let fetch = Arc::new(CountingFetch::new());
    let mut agent = Agent::new(options(&endpoint.base_url(), fetch.clone()))
        .expect("agent")
        .with_limits(limit(LimitName::ProviderMaxAttempts, 2));
    let mut turn = agent
        .prompt("work", PromptOptions::default())
        .expect("turn");
    let error = turn.result().expect_err("exhausted");
    assert_eq!(error.code(), ErrorCode::TransportFailure);
    assert_eq!(fetch.hits(), 2);
    assert_eq!(endpoint.request_count(), 2);
    assert_eq!(
        turn.events()
            .filter(|event| matches!(event, Event::ProviderRetry { .. }))
            .count(),
        1
    );
    assert_eq!(agent.usage(), rune_net::stream::Usage::default());
}

#[test]
fn permanent_provider_failures_are_never_retried() {
    for status in [400, 401, 402, 403, 404] {
        let endpoint = MockEndpoint::start(vec![Script::Status {
            code: status,
            body: "refused".to_owned(),
        }]);
        let fetch = Arc::new(CountingFetch::new());
        let mut agent = Agent::new(options(&endpoint.base_url(), fetch.clone())).expect("agent");
        let mut turn = agent
            .prompt("work", PromptOptions::default())
            .expect("turn");
        turn.result().expect_err("permanent failure");
        assert_eq!(fetch.hits(), 1, "status {status}");
        assert!(
            !turn
                .events()
                .any(|event| matches!(event, Event::ProviderRetry { .. }))
        );
    }
}

#[test]
fn retrying_a_failed_stream_discards_its_partial_text_and_usage() {
    let endpoint = MockEndpoint::start(vec![Script::truncated("discard me"), Script::text("kept")]);
    let fetch = Arc::new(CountingFetch::new());
    let mut agent = Agent::new(options(&endpoint.base_url(), fetch.clone())).expect("agent");
    let mut turn = agent
        .prompt("work", PromptOptions::default())
        .expect("turn");
    let result = turn.result().expect("retry succeeds");
    let events: Vec<_> = turn.events().collect();
    assert_eq!(fetch.hits(), 2);
    assert_eq!(result.text, "kept");
    assert_eq!(result.steps, 1);
    assert_eq!(result.usage.input_tokens, Some(10));
    let partial = events
        .iter()
        .position(|event| matches!(event, Event::TextDelta { delta } if delta == "discard me"))
        .expect("partial text streamed");
    let restart = events
        .iter()
        .position(|event| matches!(event, Event::StepRestarted { step: 1 }))
        .expect("restart");
    let kept = events
        .iter()
        .position(|event| matches!(event, Event::TextDelta { delta } if delta == "kept"))
        .expect("new text");
    assert!(partial < restart && restart < kept);
    let checkpoint = String::from_utf8(agent.checkpoint().expect("checkpoint")).expect("utf8");
    assert!(!checkpoint.contains("discard me"));
    assert!(checkpoint.contains("kept"));
}

#[test]
fn an_exhausted_retry_after_returns_without_waiting() {
    let fetch = Arc::new(RateLimitedFetch::default());
    let mut agent = Agent::new(options("https://example.invalid/v1", fetch.clone()))
        .expect("agent")
        .with_limits(limit(LimitName::ProviderMaxAttempts, 1));
    let started = Instant::now();
    let mut turn = agent
        .prompt("work", PromptOptions::default())
        .expect("turn");
    assert_eq!(
        turn.result().expect_err("exhausted").code(),
        ErrorCode::RateLimited
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(fetch.hits.load(Ordering::SeqCst), 1);
    assert!(
        !turn
            .events()
            .any(|event| matches!(event, Event::ProviderRetry { .. }))
    );
}

#[derive(Debug)]
struct FailingHostFetch {
    code: ErrorCode,
    hits: AtomicUsize,
}

impl HostFetch for FailingHostFetch {
    fn post(&self, _request: FetchRequest) -> rune_core::error::Result<FetchResponse> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        Err(rune_core::error::RuneError::new(self.code, "host failure").with_hint("host"))
    }
}

#[test]
fn host_transport_failures_retry_but_policy_refusals_keep_their_details() {
    for (code, expected_hits) in [
        (ErrorCode::TransportFailure, 2),
        (ErrorCode::PermissionDenied, 1),
    ] {
        let fetch = Arc::new(FailingHostFetch {
            code,
            hits: AtomicUsize::new(0),
        });
        let mut agent = Agent::new(options("https://example.invalid/v1", fetch.clone()))
            .expect("agent")
            .with_limits(limit(LimitName::ProviderMaxAttempts, 2));
        let mut turn = agent
            .prompt("work", PromptOptions::default())
            .expect("turn");
        let error = turn.result().expect_err("host error");
        assert_eq!(error.code(), code);
        assert_eq!(error.hint(), Some("host"));
        assert_eq!(error.message(), "host failure");
        assert_eq!(fetch.hits.load(Ordering::SeqCst), expected_hits);
    }
}

/// Generates repeated SSE frames without allocating the complete response.
#[derive(Debug)]
struct RepeatingFrame {
    frame: Vec<u8>,
    offset: usize,
    bytes_read: Arc<AtomicUsize>,
    gate: Option<std::sync::mpsc::Receiver<()>>,
}

impl Read for RepeatingFrame {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let buffer = if let Some(gate) = &self.gate {
            if buffer.is_empty() {
                return Ok(0);
            }
            gate.recv()
                .map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
            let count = buffer
                .len()
                .min(self.frame.len().saturating_sub(self.offset));
            &mut buffer[..count]
        } else {
            buffer
        };
        let mut written = 0_usize;
        while written < buffer.len() {
            let count = self
                .frame
                .len()
                .saturating_sub(self.offset)
                .min(buffer.len().saturating_sub(written));
            let end = written.saturating_add(count);
            let frame_end = self.offset.saturating_add(count);
            buffer[written..end].copy_from_slice(&self.frame[self.offset..frame_end]);
            self.offset = if frame_end == self.frame.len() {
                0
            } else {
                frame_end
            };
            written = end;
        }
        self.bytes_read.fetch_add(written, Ordering::SeqCst);
        Ok(written)
    }
}

#[derive(Debug)]
struct DeltaFetch {
    deltas: usize,
    delta: String,
    bytes_read: Arc<AtomicUsize>,
    hits: AtomicUsize,
    gate: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

impl DeltaFetch {
    fn new(deltas: usize, delta_bytes: usize) -> Self {
        Self {
            deltas,
            delta: "x".repeat(delta_bytes),
            bytes_read: Arc::new(AtomicUsize::new(0)),
            hits: AtomicUsize::new(0),
            gate: std::sync::Mutex::new(None),
        }
    }

    fn frame(&self) -> Vec<u8> {
        format!(
            "data: {}\n\n",
            json!({"choices": [{"index": 0, "delta": {"content": self.delta}}]})
        )
        .into_bytes()
    }
}

impl HostFetch for DeltaFetch {
    fn post(&self, _request: FetchRequest) -> rune_core::error::Result<FetchResponse> {
        if self.hits.fetch_add(1, Ordering::SeqCst) > 0 {
            return Ok(FetchResponse::new(
                200,
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"recovered\"}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            ));
        }
        let frame = self.frame();
        let bytes = self.deltas.saturating_mul(frame.len());
        let reader = RepeatingFrame {
            frame,
            offset: 0,
            bytes_read: self.bytes_read.clone(),
            gate: self.gate.lock().expect("gate").take(),
        };
        Ok(FetchResponse {
            status: 200,
            body: Box::new(reader.take(bytes as u64).chain(Cursor::new(
                b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            ))),
        })
    }
}

#[test]
fn a_stalled_consumer_bounds_100000_deltas_and_reports_backpressure() {
    // Both many tiny events and larger payloads must stop after the same
    // queue bound. The fixture itself holds only one frame at a time.
    for delta_bytes in [1, 4096] {
        let fetch = Arc::new(DeltaFetch::new(100_000, delta_bytes));
        let mut agent =
            Agent::new(options("https://example.invalid/v1", fetch.clone())).expect("agent");
        let mut turn = agent
            .prompt("stream", PromptOptions::default())
            .expect("turn");
        let (send, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = turn.result();
            send.send((turn, result)).expect("send result");
        });
        // No events are consumed until the worker has settled. In particular,
        // waiting for just the result must not deadlock on a full queue.
        let (mut turn, result) = receive
            .recv_timeout(Duration::from_secs(5))
            .expect("a stalled event consumer must not block completion");
        worker.join().expect("worker");
        let error = result.expect_err("backpressure");
        assert_eq!(error.code(), ErrorCode::LimitExceeded);
        assert_eq!(error.detail().invariant.as_deref(), Some("sdk_event_queue"));
        assert_eq!(error.detail().observed.as_deref(), Some("1024"));
        assert!(error.message().contains("backpressure"));
        assert!(error.hint().expect("repair hint").contains("consume"));

        // Bound the retained payload directly, without allocator/RSS noise.
        let events: Vec<_> = turn.events().collect();
        assert_eq!(events.len(), EVENT_QUEUE_CAPACITY);
        assert!(matches!(events.first(), Some(Event::TurnStarted { .. })));
        let queued_bytes: usize = events
            .iter()
            .map(|event| match event {
                Event::TextDelta { delta } => delta.len(),
                _ => 0,
            })
            .sum();
        assert_eq!(
            queued_bytes,
            EVENT_QUEUE_CAPACITY
                .saturating_sub(1)
                .saturating_mul(delta_bytes)
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::Finished { .. }))
        );
        // Decoding can finish the current 16 KiB chunk; the existing transport
        // helper can read ahead by eight chunks plus its pending read.
        let read_bound = EVENT_QUEUE_CAPACITY
            .saturating_mul(fetch.frame().len())
            .saturating_add(16 * 1024 * 16);
        assert!(fetch.bytes_read.load(Ordering::SeqCst) <= read_bound);
        assert_eq!(
            fetch.hits.load(Ordering::SeqCst),
            1,
            "no retry on backpressure"
        );
        assert_eq!(
            turn.result().expect_err("failure stays explicit").code(),
            ErrorCode::LimitExceeded
        );

        // The failure releases the conversation and does not poison the next
        // turn's cancellation flag, even when the old handle is dropped.
        let mut next = agent
            .prompt("again", PromptOptions::default())
            .expect("next turn");
        drop(turn);
        assert_eq!(next.result().expect("recovered").text, "recovered");
    }
}

#[test]
fn a_result_only_turn_keeps_all_events_up_to_the_queue_capacity() {
    // TurnStarted and Finished each occupy one slot as well as the deltas.
    let deltas = EVENT_QUEUE_CAPACITY.saturating_sub(2);
    let fetch = Arc::new(DeltaFetch::new(deltas, 1));
    let mut agent = Agent::new(options("https://example.invalid/v1", fetch)).expect("agent");
    let mut turn = agent
        .prompt("stream", PromptOptions::default())
        .expect("turn");
    let result = turn.result().expect("fits exactly");
    assert_eq!(result.stop_reason, StopReason::Completed);
    assert_eq!(result.text, "x".repeat(deltas));
    let events: Vec<_> = turn.events().collect();
    assert_eq!(events.len(), EVENT_QUEUE_CAPACITY);
    assert!(matches!(events.first(), Some(Event::TurnStarted { .. })));
    assert!(matches!(
        events.last(),
        Some(Event::Finished {
            reason: StopReason::Completed,
            ..
        })
    ));
    assert_eq!(
        events
            .iter()
            .filter_map(|event| match event {
                Event::TextDelta { delta } => Some(delta.as_str()),
                _ => None,
            })
            .collect::<String>(),
        result.text
    );
}

#[test]
fn overflow_of_the_finished_event_is_still_an_explicit_failure() {
    let fetch = Arc::new(DeltaFetch::new(EVENT_QUEUE_CAPACITY.saturating_sub(1), 1));
    let mut agent = Agent::new(options("https://example.invalid/v1", fetch)).expect("agent");
    let mut turn = agent
        .prompt("stream", PromptOptions::default())
        .expect("turn");
    let error = turn
        .result()
        .expect_err("the terminal event cannot be silently lost");
    assert_eq!(error.code(), ErrorCode::LimitExceeded);
    assert_eq!(error.detail().invariant.as_deref(), Some("sdk_event_queue"));
    assert_eq!(turn.events().count(), EVENT_QUEUE_CAPACITY);
}

#[test]
fn consuming_events_allows_a_response_larger_than_the_queue() {
    let deltas = EVENT_QUEUE_CAPACITY.saturating_mul(2);
    let fetch = Arc::new(DeltaFetch::new(deltas, 1));
    let (advance, gate) = std::sync::mpsc::channel();
    *fetch.gate.lock().expect("gate") = Some(gate);
    // Pace the host reader with consumption, so the producer cannot race
    // ahead because of test-machine scheduling.
    advance.send(()).expect("first frame");
    let mut agent = Agent::new(options("https://example.invalid/v1", fetch)).expect("agent");
    let mut turn = agent
        .prompt("stream", PromptOptions::default())
        .expect("turn");
    let mut text = String::new();
    let mut finished = false;
    for event in turn.events() {
        match event {
            Event::TextDelta { delta } => {
                text.push_str(&delta);
                if text.len() < deltas {
                    advance.send(()).expect("next frame");
                }
            }
            Event::Finished { reason, .. } => {
                assert_eq!(reason, StopReason::Completed);
                finished = true;
            }
            _ => {}
        }
    }
    let result = turn.result().expect("completed");
    assert!(finished);
    assert_eq!(text, "x".repeat(deltas));
    assert_eq!(result.text, text);
    assert_eq!(result.stop_reason, StopReason::Completed);
}

#[test]
fn backpressure_on_tool_started_prevents_the_host_tool_from_running() {
    let mut frames = vec![
        json!({"choices": [{"index": 0, "delta": {"content": "x"}}]}).to_string();
        EVENT_QUEUE_CAPACITY.saturating_sub(1)
    ];
    let Script::Frames(tool_frames) = Script::tool_call("call_1", "lookup", "{}") else {
        panic!("tool fixture must stream frames");
    };
    frames.extend(tool_frames);
    let endpoint = MockEndpoint::start(vec![Script::Frames(frames)]);
    let calls = Arc::new(AtomicUsize::new(0));
    let tool_calls = calls.clone();
    let mut agent_options = options(&endpoint.base_url(), Arc::new(CountingFetch::new()));
    agent_options.tools.push(HostTool::new(
        "lookup",
        "Looks a key up.",
        json!({"type": "object", "properties": {}}),
        move |_, _| {
            tool_calls.fetch_add(1, Ordering::SeqCst);
            Ok(rune_sdk::agent::HostToolResult::text("value"))
        },
    ));
    let mut agent = Agent::new(agent_options).expect("agent");
    let mut turn = agent
        .prompt("lookup", PromptOptions::default())
        .expect("turn");
    let error = turn.result().expect_err("ToolStarted overflow");
    assert_eq!(error.code(), ErrorCode::LimitExceeded);
    assert_eq!(error.detail().invariant.as_deref(), Some("sdk_event_queue"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(endpoint.request_count(), 1);
    assert_eq!(turn.events().count(), EVENT_QUEUE_CAPACITY);
}
