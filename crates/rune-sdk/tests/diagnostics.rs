//! Public SDK diagnostics must describe the actual configuration without host secrets.
#![allow(clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rune_agent::turn::StopReason;
use rune_core::budget::{Budget, BudgetSet, EMERGENCY_CEILING_BYTES, LimitName};
use rune_core::config::{Effort, Layer};
use rune_net::provider::ToolStreaming;
use rune_sdk::{
    Agent, AgentDiagnostics, AgentOptions, DiagnosticSource, Dialect, EVENT_QUEUE_CAPACITY,
    FetchRequest, FetchResponse, HostFetch, HostTool, HostToolResult, PromptOptions, SdkLimit,
};
use serde_json::json;

#[derive(Default)]
struct FixtureFetch(AtomicUsize);

impl HostFetch for FixtureFetch {
    fn post(&self, request: FetchRequest) -> rune_core::error::Result<FetchResponse> {
        self.0.fetch_add(1, Ordering::SeqCst);
        let body: serde_json::Value = serde_json::from_str(&request.body).expect("request body");
        assert_eq!(body["parallel_tool_calls"], false);
        Ok(FetchResponse::new(
            200,
            concat!(
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call\",\"function\":{\"name\":\"repeat\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
                "data: [DONE]\n\n"
            ),
        ))
    }
}

fn options(fetch: Arc<FixtureFetch>) -> AgentOptions {
    AgentOptions {
        api_key: "private-credential-9271".to_owned(),
        model: Some("private-model-9271".to_owned()),
        instructions: Some(vec!["private-instructions-9271".to_owned()]),
        tools: vec![HostTool::new(
            "repeat",
            "private-tool-description-9271",
            json!({"type": "object", "description": "private-schema-9271"}),
            |_, _| Ok(HostToolResult::text("again")),
        )],
        base_url: "https://example.invalid/private-endpoint-9271?key=private-query-9271".to_owned(),
        dialect: Dialect::ChatCompletions,
        fetch: Some(fetch),
    }
}

fn row(report: &AgentDiagnostics, name: LimitName) -> &SdkLimit {
    report
        .limits
        .iter()
        .find(|row| row.configured.name == name)
        .expect("SDK limit")
}

#[test]
fn configured_diagnostics_are_credential_free_and_match_the_turn_limit() {
    let fetch = Arc::new(FixtureFetch::default());
    let mut limits = BudgetSet::new();
    for (name, value, layer) in [
        (LimitName::MaxAgentSteps, 2, Layer::Project),
        (LimitName::MaxToolResultBytes, 4096, Layer::User),
        (LimitName::ProviderHeadTimeoutMs, 2000, Layer::Environment),
        (
            LimitName::ProviderRequestTimeoutMs,
            4000,
            Layer::CommandLine,
        ),
        (LimitName::ParallelToolCalls, 1, Layer::Default),
    ] {
        limits
            .set(name, Budget::Bounded(value), layer)
            .expect("valid limit");
    }
    let mut agent = Agent::new(options(fetch.clone()))
        .expect("agent")
        .with_limits(limits);
    let report = agent.diagnostics();
    assert_eq!(report.event_queue_capacity, EVENT_QUEUE_CAPACITY);
    assert_eq!(report.sources.model, DiagnosticSource::HostSupplied);
    assert_eq!(report.sources.endpoint, DiagnosticSource::HostSupplied);
    assert_eq!(report.sources.dialect, DiagnosticSource::HostSupplied);
    assert_eq!(
        report.sources.instructions,
        Some(DiagnosticSource::HostSupplied)
    );
    assert_eq!(report.sources.tools, DiagnosticSource::HostSupplied);
    assert_eq!(report.sources.fetch, DiagnosticSource::HostSupplied);
    let steps = row(&report, LimitName::MaxAgentSteps);
    assert_eq!(steps.configured.source, Some(Layer::Project));
    assert_eq!(steps.configured.value, Budget::Bounded(2));
    assert_eq!(steps.configured.default, Budget::Bounded(0));
    assert_eq!(steps.origin, DiagnosticSource::HostSupplied);
    assert_eq!(steps.effective, Some(2));
    let parallel = row(&report, LimitName::ParallelToolCalls);
    assert_eq!(parallel.origin, DiagnosticSource::HostSupplied);
    assert_eq!(parallel.configured.source, Some(Layer::Default));
    assert_eq!(
        row(&report, LimitName::ProviderHeadTimeoutMs).effective,
        Some(2000)
    );
    assert_eq!(
        row(&report, LimitName::ProviderRequestTimeoutMs).effective,
        Some(4000)
    );
    assert_eq!(
        row(&report, LimitName::MaxToolResultBytes).effective,
        Some(4096)
    );
    let json = serde_json::to_value(&report).expect("serializable diagnostics");
    assert_eq!(json["sources"]["fetch"], "host_supplied");
    assert_eq!(json["capability_source"], "dialect_implementation");
    for output in [json.to_string(), format!("{report:?}")] {
        assert!(
            !output.contains("private-"),
            "host secrets leaked: {output}"
        );
        assert!(!output.contains("api_key"));
    }
    assert_eq!(fetch.0.load(Ordering::SeqCst), 0, "query never fetches");
    let result = agent
        .prompt("private-conversation-9271", PromptOptions::default())
        .expect("turn")
        .result()
        .expect("result");
    assert_eq!(result.stop_reason, StopReason::StepLimit);
    assert_eq!(
        u64::from(result.steps),
        steps.effective.expect("bounded steps")
    );
    assert_eq!(fetch.0.load(Ordering::SeqCst), 2);
    assert_eq!(
        serde_json::to_value(agent.diagnostics()).expect("diagnostics after turn"),
        json
    );
}

#[test]
fn default_and_unbounded_limits_report_operational_fallbacks() {
    let fetch = Arc::new(FixtureFetch::default());
    let mut opts = options(fetch.clone());
    opts.fetch = None;
    opts.instructions = None;
    let agent = Agent::new(opts).expect("agent");
    let defaults = agent.diagnostics();
    assert_eq!(defaults.sources.fetch, DiagnosticSource::CompiledDefault);
    assert_eq!(defaults.sources.instructions, None);
    assert_eq!(defaults.limits.len(), 7);
    for limit in &defaults.limits {
        assert_eq!(limit.origin, DiagnosticSource::CompiledDefault);
        assert_eq!(limit.configured.source, None);
        assert_eq!(limit.configured.value, limit.configured.default);
    }
    assert_eq!(row(&defaults, LimitName::MaxAgentSteps).effective, None);
    assert!(
        !defaults
            .limits
            .iter()
            .any(|row| row.configured.name == LimitName::CommandOutputBytes)
    );
    let mut limits = BudgetSet::new();
    for limit in &defaults.limits {
        limits
            .set(limit.configured.name, Budget::Unbounded, Layer::User)
            .expect("unbounded limit");
    }
    let report = agent.with_limits(limits).diagnostics();
    for (name, effective) in [
        (LimitName::MaxAgentSteps, None),
        (LimitName::ProviderHeadTimeoutMs, Some(120_000)),
        (LimitName::ProviderRequestTimeoutMs, None),
        (LimitName::MaxToolResultBytes, Some(EMERGENCY_CEILING_BYTES)),
        (LimitName::PromptHistoryBytes, Some(EMERGENCY_CEILING_BYTES)),
        (LimitName::ParallelToolCalls, Some(EMERGENCY_CEILING_BYTES)),
        (
            LimitName::ProviderMaxAttempts,
            Some(EMERGENCY_CEILING_BYTES),
        ),
    ] {
        let limit = row(&report, name);
        assert_eq!(limit.effective, effective, "{name:?}");
        assert_eq!(limit.configured.value, Budget::Unbounded);
        assert_eq!(limit.origin, DiagnosticSource::HostSupplied);
        assert_eq!(limit.configured.source, Some(Layer::User));
    }
    assert_eq!(fetch.0.load(Ordering::SeqCst), 0);
}

#[test]
fn each_dialect_reports_its_implementation_capabilities_without_fetching() {
    let fetch = Arc::new(FixtureFetch::default());
    for (dialect, streaming, fast, routing, efforts) in [
        (
            Dialect::ChatCompletions,
            ToolStreaming::Incremental,
            false,
            true,
            rune_net::provider::ALL_EFFORTS,
        ),
        (
            Dialect::Anthropic,
            ToolStreaming::Incremental,
            false,
            false,
            rune_net::provider::ALL_EFFORTS,
        ),
        (
            Dialect::Responses,
            ToolStreaming::Incremental,
            true,
            false,
            rune_net::provider::ALL_EFFORTS,
        ),
        (
            Dialect::Gemini,
            ToolStreaming::CompleteCalls,
            false,
            false,
            &[Effort::Auto][..],
        ),
    ] {
        let mut opts = options(fetch.clone());
        opts.dialect = dialect;
        let report = Agent::new(opts).expect("agent").diagnostics();
        assert_eq!(
            report.capability_source,
            DiagnosticSource::DialectImplementation
        );
        assert!(report.capabilities.images);
        assert_eq!(report.capabilities.tool_streaming, streaming);
        assert_eq!(report.capabilities.fast_mode, fast);
        assert_eq!(report.capabilities.efforts, efforts);
        assert_eq!(report.routing, routing);
        assert!(!report.dialect.is_empty());
    }
    assert_eq!(fetch.0.load(Ordering::SeqCst), 0);
}

#[test]
fn restored_agents_report_current_host_limits_and_sources() {
    let fetch = Arc::new(FixtureFetch::default());
    let agent = Agent::new(options(fetch.clone())).expect("agent");
    let checkpoint = agent.checkpoint().expect("checkpoint");
    let mut opts = options(fetch.clone());
    opts.api_key.clear();
    opts.base_url = "http://127.0.0.1:12345".to_owned();
    opts.dialect = Dialect::Gemini;
    opts.instructions = None;
    let anonymous = Agent::new_anonymous(opts).expect("anonymous agent");
    assert_eq!(anonymous.diagnostics().dialect, "gemini");
    let mut limits = BudgetSet::new();
    limits
        .set(
            LimitName::PromptHistoryBytes,
            Budget::Bounded(0),
            Layer::User,
        )
        .expect("zero cap");
    let replacement = agent.with_limits(limits);
    assert_eq!(
        row(&replacement.diagnostics(), LimitName::PromptHistoryBytes).effective,
        Some(1)
    );
    let mut restore_options = options(fetch.clone());
    restore_options.fetch = None;
    restore_options.instructions = None;
    restore_options.dialect = Dialect::Responses;
    let mut restore_limits = BudgetSet::new();
    restore_limits
        .set(
            LimitName::MaxAgentSteps,
            Budget::Bounded(3),
            Layer::Environment,
        )
        .expect("restored step limit");
    let mut restored =
        Agent::restore_with_limits(restore_options, &checkpoint, restore_limits).expect("restore");
    let restored_report = restored.diagnostics();
    assert_eq!(restored_report.dialect, "responses");
    assert_eq!(
        restored_report.sources.fetch,
        DiagnosticSource::CompiledDefault
    );
    assert_eq!(restored_report.sources.instructions, None);
    assert_eq!(
        row(&restored_report, LimitName::MaxAgentSteps).effective,
        Some(3)
    );
    assert_eq!(
        row(&restored_report, LimitName::MaxAgentSteps)
            .configured
            .source,
        Some(Layer::Environment)
    );
    assert_eq!(
        row(&restored.diagnostics(), LimitName::PromptHistoryBytes).origin,
        DiagnosticSource::CompiledDefault
    );
    restored.close().expect("close");
    assert_eq!(
        restored.diagnostics().sources.fetch,
        DiagnosticSource::CompiledDefault
    );
    assert_eq!(fetch.0.load(Ordering::SeqCst), 0);
}
