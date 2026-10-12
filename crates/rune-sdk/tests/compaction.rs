//! Explicit SDK compaction preserves complete tool exchanges and checkpoint replay.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::fmt::Write;
use std::sync::{Arc, Mutex};

use rune_agent::History;
use rune_core::budget::{Budget, BudgetSet, LimitName};
use rune_core::config::Layer;
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::id::ToolCallId;
use rune_net::message::ContentPart;
use rune_net::stream::Usage;
use rune_sdk::{Agent, AgentOptions, CHECKPOINT_VERSION, Dialect, FetchRequest, FetchResponse};
use rune_sdk::{HostFetch, PromptOptions};
use serde_json::{Value, json};

const SUMMARY: &str =
    "Earlier lookups completed successfully. Continue the current work with the retained results.";

#[derive(Default)]
struct FixtureFetch {
    requests: Mutex<Vec<Value>>,
}

impl HostFetch for FixtureFetch {
    fn post(&self, request: FetchRequest) -> Result<FetchResponse> {
        self.requests
            .lock()
            .expect("requests")
            .push(serde_json::from_str(&request.body).expect("request JSON"));
        let frames = [
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 10, "output_tokens": 0}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": "continued"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}}),
            json!({"type": "message_stop"}),
        ];
        let mut body = String::new();
        for frame in frames {
            write!(body, "data: {frame}\n\n").expect("write SSE frame");
        }
        Ok(FetchResponse::new(200, body))
    }
}

fn options(fetch: Arc<dyn HostFetch>) -> AgentOptions {
    AgentOptions {
        api_key: "fixture-secret".to_owned(),
        model: Some("fixture-model".to_owned()),
        instructions: Some(vec!["fixture host instructions".to_owned()]),
        tools: Vec::new(),
        base_url: "https://example.invalid/v1".to_owned(),
        dialect: Dialect::Anthropic,
        fetch: Some(fetch),
    }
}

/// Includes multi-call exchanges, opaque signed replay, and reported usage.
/// A single long prompt forces the shared planner to cut at an assistant turn.
fn fixture(single_prompt: bool) -> Vec<u8> {
    let mut history = History::new();
    for index in 0..6 {
        if !single_prompt || index == 0 {
            history.push_user(format!("question {index}"));
        }
        let mut calls = Vec::new();
        let mut results = Vec::new();
        for part in 0..2 {
            let id = ToolCallId::new(format!("call_{index}_{part}")).expect("ID");
            calls.push(ContentPart::ToolCall {
                id: id.clone(),
                name: "lookup".to_owned(),
                arguments: json!({"key": format!("key_{index}_{part}")}).to_string(),
            });
            results.push(ContentPart::ToolResult {
                id,
                name: "lookup".to_owned(),
                content: format!("value_{index}_{part}"),
                is_error: part == 1,
            });
        }
        history.push_assistant_with_replay(
            calls,
            Some(json!([{"type": "thinking", "thinking": format!("thought {index}"), "signature": format!("signature_{index}")}]).to_string()),
        );
        history.push_tool_results(results);
        if !single_prompt || index == 5 {
            history.push_assistant(vec![ContentPart::Text {
                text: format!("answer {index}"),
            }]);
        }
    }
    history.validate().expect("valid fixture");
    serde_json::to_vec(&json!({
        "version": CHECKPOINT_VERSION,
        "history": history,
        "usage": Usage { input_tokens: Some(123), output_tokens: Some(45), ..Usage::default() },
    }))
    .expect("fixture checkpoint")
}

fn checkpoint_history(bytes: &[u8]) -> History {
    let value: Value = serde_json::from_slice(bytes).expect("checkpoint JSON");
    serde_json::from_value(value["history"].clone()).expect("checkpoint history")
}

#[test]
fn compact_preserves_tool_pairs_signed_replay_and_checkpoint_continuation() {
    for single_prompt in [false, true] {
        let fetch = Arc::new(FixtureFetch::default());
        let before = fixture(single_prompt);
        let original = checkpoint_history(&before);
        let mut agent = Agent::restore(options(fetch.clone()), &before).expect("restore fixture");
        let usage = agent.usage();
        let mut calls = 0;
        let removed = agent
            .compact(|request| {
                calls += 1;
                assert!(request.contains("<conversation>"));
                assert!(request.contains("question 0"));
                assert!(request.contains("calls lookup"));
                assert!(request.contains("result from lookup: value_0_0"));
                assert!(request.contains("error from lookup: value_0_1"));
                assert!(!request.contains("key_5_0"));
                assert!(!request.contains("answer 5"));
                Ok(SUMMARY.to_owned())
            })
            .expect("compact");
        assert_eq!(calls, 1);
        assert_eq!(removed, if single_prompt { 9 } else { 20 });
        assert_eq!(agent.usage(), usage);
        assert!(fetch.requests.lock().expect("requests").is_empty());

        let compacted = agent.checkpoint().expect("compacted checkpoint");
        let history = checkpoint_history(&compacted);
        history.validate().expect("tool pairs remain valid");
        assert!(history.pending_tool_calls().is_empty());
        assert_eq!(history.len(), original.len() - removed + 1);
        assert!(compacted.len() < before.len());
        assert!(history.turns()[0].text().contains(SUMMARY));
        assert!(history.turns()[0].text().contains("<context_handoff>"));
        let text = String::from_utf8_lossy(&compacted);
        assert!(!text.contains("fixture-secret"));
        assert!(!text.contains("fixture host instructions"));
        for (retained, old) in history.turns()[1..]
            .iter()
            .zip(&original.turns()[removed..])
        {
            assert_eq!(retained.role, old.role);
            assert_eq!(retained.parts, old.parts);
            assert_eq!(retained.replay, old.replay);
        }
        for (index, turn) in history.turns().iter().enumerate() {
            assert_eq!(turn.seq, index as u64 + 1);
        }

        let mut restored =
            Agent::restore(options(fetch.clone()), &compacted).expect("restore compacted");
        assert_eq!(restored.checkpoint().expect("checkpoint again"), compacted);
        assert_eq!(restored.usage(), usage);
        let result = restored
            .prompt("continue", PromptOptions::default())
            .expect("continued turn")
            .result()
            .expect("continued result");
        assert_eq!(result.text, "continued");
        let requests = fetch.requests.lock().expect("requests");
        assert_eq!(requests.len(), 1);
        let body = &requests[0];
        assert_eq!(body["system"], "fixture host instructions");
        let blocks: Vec<&Value> = body["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .flat_map(|message| message["content"].as_array().expect("blocks"))
            .collect();
        assert!(blocks.iter().any(|block| block["text"] == "continue"));
        assert!(blocks.iter().any(|block| {
            block["text"]
                .as_str()
                .is_some_and(|text| text.contains(SUMMARY))
        }));
        for turn in &history.turns()[1..] {
            if let Some(replay) = &turn.replay {
                let expected: Vec<Value> = serde_json::from_str(replay).expect("replay blocks");
                for block in expected {
                    assert!(blocks.contains(&&block), "signed replay missing: {block}");
                }
            }
            for (id, _, _) in turn.tool_calls() {
                assert_eq!(
                    blocks
                        .iter()
                        .filter(|block| block["type"] == "tool_use" && block["id"] == id.as_str())
                        .count(),
                    1
                );
                assert_eq!(
                    blocks
                        .iter()
                        .filter(|block| block["type"] == "tool_result"
                            && block["tool_use_id"] == id.as_str())
                        .count(),
                    1
                );
            }
        }
        assert!(!body.to_string().contains("call_0_0"));
    }
}

#[test]
fn compaction_failures_preserve_the_exact_checkpoint_and_usage() {
    let fetch = Arc::new(FixtureFetch::default());
    let before = fixture(false);
    let mut agent = Agent::restore(options(fetch.clone()), &before).expect("fixture");
    let before = agent.checkpoint().expect("before");
    let usage = agent.usage();
    let error = agent
        .compact(|_| Err(RuneError::new(ErrorCode::Cancelled, "summarizer cancelled")))
        .expect_err("callback failure");
    assert_eq!(error.code(), ErrorCode::Cancelled);
    assert_eq!(agent.checkpoint().expect("after cancellation"), before);
    for summary in ["", "   ", "too short"] {
        assert_eq!(
            agent
                .compact(|_| Ok(summary.to_owned()))
                .expect_err("unusable summary")
                .code(),
            ErrorCode::InvalidState
        );
        assert_eq!(agent.checkpoint().expect("after invalid summary"), before);
    }
    let mut limits = BudgetSet::new();
    limits
        .set(
            LimitName::PromptHistoryBytes,
            Budget::Bounded(before.len() as u64),
            Layer::CommandLine,
        )
        .expect("limit");
    agent = agent.with_limits(limits);
    let error = agent
        .compact(|_| Ok("x".repeat(before.len())))
        .expect_err("oversized summary");
    assert_eq!(error.code(), ErrorCode::TooLarge);
    assert_eq!(error.field(), Some("checkpoint"));
    assert_eq!(agent.checkpoint().expect("after oversized summary"), before);
    assert_eq!(agent.usage(), usage);
    assert!(fetch.requests.lock().expect("requests").is_empty());
    assert!(agent.compact(|_| Ok(SUMMARY.to_owned())).expect("retry") > 0);
}

#[test]
fn compaction_can_recover_a_conversation_above_the_checkpoint_cap() {
    let fetch = Arc::new(FixtureFetch::default());
    let mut agent = Agent::restore(options(fetch.clone()), &fixture(false)).expect("fixture");
    let mut limits = BudgetSet::new();
    limits
        .set(
            LimitName::PromptHistoryBytes,
            Budget::Bounded(2048),
            Layer::CommandLine,
        )
        .expect("limit");
    agent = agent.with_limits(limits.clone());
    assert_eq!(
        agent.checkpoint().expect_err("over cap").code(),
        ErrorCode::TooLarge
    );
    assert!(agent.compact(|_| Ok(SUMMARY.to_owned())).expect("compact") > 0);
    let bytes = agent.checkpoint().expect("fits after compaction");
    Agent::restore_with_limits(options(fetch), &bytes, limits).expect("restores under same cap");
}

#[test]
fn short_conversations_skip_the_summarizer_without_changing_the_checkpoint() {
    let fetch = Arc::new(FixtureFetch::default());
    let mut agent = Agent::new(options(fetch)).expect("agent");
    for prompt in [None, Some("hello")] {
        if let Some(prompt) = prompt {
            agent
                .prompt(prompt, PromptOptions::default())
                .expect("turn")
                .result()
                .expect("result");
        }
        let before = agent.checkpoint().expect("before");
        assert_eq!(
            agent
                .compact(|_| panic!("no prefix to summarize"))
                .expect("no-op"),
            0
        );
        assert_eq!(agent.checkpoint().expect("after"), before);
    }
}

#[test]
fn running_turns_and_closed_agents_refuse_compaction_before_calling_the_host() {
    struct GatedFetch(Mutex<std::sync::mpsc::Receiver<()>>);
    impl HostFetch for GatedFetch {
        fn post(&self, request: FetchRequest) -> Result<FetchResponse> {
            self.0.lock().expect("gate").recv().expect("release");
            FixtureFetch::default().post(request)
        }
    }
    let (release, gate) = std::sync::mpsc::channel();
    let mut agent = Agent::new(options(Arc::new(GatedFetch(Mutex::new(gate))))).expect("agent");
    let mut turn = agent
        .prompt("work", PromptOptions::default())
        .expect("turn");
    assert_eq!(
        agent
            .compact(|_| panic!("running turn"))
            .expect_err("refused")
            .code(),
        ErrorCode::InvalidState
    );
    release.send(()).expect("release fetch");
    turn.result().expect("settled turn");
    agent.close().expect("close");
    let before = agent.checkpoint().expect("before");
    assert_eq!(
        agent
            .compact(|_| panic!("closed agent"))
            .expect_err("refused")
            .code(),
        ErrorCode::InvalidState
    );
    assert_eq!(agent.checkpoint().expect("after"), before);
}
