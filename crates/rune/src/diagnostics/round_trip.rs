//! `rune doctor --provider`: one short tool round trip to a provider, and a
//! report of what worked.
//!
//! The ordinary doctor sends nothing. This check spends a few tokens to show
//! what only a real exchange can: that a tool call is parsed, that reasoning
//! sent back is accepted, and which usage fields the provider reports.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use rune_agent::History;
use rune_agent::retry::RetryPolicy;
use rune_agent::steering::{Cancellation, SteeringQueue};
use rune_agent::turn::{self, Event, Host};
use rune_core::budget::{Budget, BudgetSet, LimitName};
use rune_core::config::{Effort, Layer, Provider as ProviderName, Settings};
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::paths::Paths;
use rune_net::compat::{EndpointCompat, ModelTraits, ReasoningReplay};
use rune_net::message::ToolSpec;
use rune_net::provider::Provider;
use rune_net::stream::Usage;
use rune_net::transport::{Endpoint, StreamOutcome};
use rune_policy::decision::Outcome;
use rune_tools::contract::{ExecutionContext, ToolOutput};
use serde::Serialize;

/// The only tool offered.
const TOOL_NAME: &str = "current_directory";

/// What the model is asked to do.
const PROMPT: &str =
    "Call the current_directory tool once, then reply with only the name it returned.";

/// How long one request may take.
const REQUEST_TIMEOUT_MS: u64 = 60_000;

/// The usage the estimate is made for: two short requests.
const ESTIMATE_INPUT_TOKENS: u64 = 2_000;
const ESTIMATE_OUTPUT_TOKENS: u64 = 200;

/// How one check ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// It worked.
    Ok,
    /// It did not.
    Failed,
    /// It could not be checked, or does not apply.
    NotApplicable,
}

/// One line of the report.
#[derive(Clone, Debug, Serialize)]
pub struct RoundTripCheck {
    /// Stable name.
    pub name: &'static str,
    /// How it ended.
    pub status: Status,
    /// What was seen.
    pub detail: String,
    /// Whether the line reports a value rather than a pass or a failure.
    #[serde(skip)]
    informational: bool,
}

impl RoundTripCheck {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Ok,
            detail: detail.into(),
            informational: false,
        }
    }

    fn failed(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Failed,
            detail: detail.into(),
            informational: false,
        }
    }

    fn not_applicable(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::NotApplicable,
            detail: detail.into(),
            informational: false,
        }
    }

    fn value(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            informational: true,
            ..Self::ok(name, detail)
        }
    }

    /// The text after the name.
    fn shown(&self) -> String {
        match (self.status, self.informational, self.detail.is_empty()) {
            (Status::Ok, true, _) => self.detail.clone(),
            (Status::Ok, false, true) => "ok".to_owned(),
            (Status::Ok, false, false) => format!("ok ({})", self.detail),
            (Status::Failed, _, _) => format!("failed: {}", self.detail),
            (Status::NotApplicable, _, true) => "not applicable".to_owned(),
            (Status::NotApplicable, _, false) => format!("not applicable ({})", self.detail),
        }
    }
}

/// What the round trip showed.
#[derive(Clone, Debug, Serialize)]
pub struct ProviderReport {
    /// The provider checked.
    pub provider: String,
    /// The model it was asked to use.
    pub model: String,
    /// The base URL requests went to.
    pub endpoint: String,
    /// Every check, in the order shown.
    pub checks: Vec<RoundTripCheck>,
    /// What the exchange cost, when prices are known.
    pub cost_usd: Option<String>,
}

impl ProviderReport {
    /// 0 when no check failed, else 1.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        i32::from(
            self.checks
                .iter()
                .any(|check| check.status == Status::Failed),
        )
    }

    /// The first check that failed, when one did.
    #[must_use]
    pub fn first_failure(&self) -> Option<&RoundTripCheck> {
        self.checks
            .iter()
            .find(|check| check.status == Status::Failed)
    }

    /// The report as text, one check a line.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!(
            "provider   {} ({}) at {}\n",
            self.provider, self.model, self.endpoint
        );
        for check in &self.checks {
            let _ = writeln!(out, "{:<19}{}", check.name, check.shown());
        }
        out
    }
}

/// The line printed before anything is sent.
#[must_use]
pub fn estimate_line(settings: &Settings, paths: &Paths) -> String {
    let estimate = crate::provider_setup::cached_prices(settings, paths)
        .and_then(|prices| {
            let usage = Usage {
                input_tokens: Some(ESTIMATE_INPUT_TOKENS),
                output_tokens: Some(ESTIMATE_OUTPUT_TOKENS),
                ..Usage::default()
            };
            prices.cost_at(&usage, rune_net::transport::now_timestamp())
        })
        .map_or_else(
            || "unknown".to_owned(),
            |micro| format!("${}", four_places(micro)),
        );
    format!(
        "sending 2 short requests to {} ({}); estimated cost {estimate}",
        settings.provider, settings.model
    )
}

/// Millionths of a dollar as dollars with four decimal places.
fn four_places(micro: u64) -> String {
    let hundreds = micro.saturating_add(50) / 100;
    format!("{}.{:04}", hundreds / 10_000, hundreds % 10_000)
}

/// Sends one short tool round trip to the configured provider and reports
/// what worked. The settings already name the provider and model to check.
///
/// # Errors
///
/// Returns an error when no model is selected or a control is invalid, before
/// anything is sent.
pub fn provider_check(
    settings: &Settings,
    paths: &Paths,
    workspace: &Utf8Path,
) -> Result<ProviderReport> {
    settings.require_model()?;
    crate::provider_setup::validate_controls(settings)?;
    let base_url = crate::provider_setup::configured_base_url(settings).unwrap_or_default();
    let mut report = ProviderReport {
        provider: settings.provider.to_string(),
        model: settings.model.clone(),
        endpoint: base_url.clone(),
        checks: Vec::new(),
        cost_usd: None,
    };
    let endpoint = match crate::provider_setup::configured_endpoint(settings, paths) {
        Ok(endpoint) => endpoint,
        Err(err) if err.code() == ErrorCode::AuthenticationRequired => {
            report
                .checks
                .push(RoundTripCheck::not_applicable("reachable", "nothing sent"));
            report
                .checks
                .push(RoundTripCheck::failed("authenticated", err.message()));
            skip_rest(&mut report, settings, paths);
            return Ok(report);
        }
        Err(err) => return Err(err),
    };
    if endpoint.offline && !rune_net::transport::is_loopback_url(&endpoint.base_url) {
        report.checks.push(RoundTripCheck::failed(
            "reachable",
            "outbound requests are disabled",
        ));
        report
            .checks
            .push(RoundTripCheck::not_applicable("authenticated", ""));
        skip_rest(&mut report, settings, paths);
        return Ok(report);
    }
    let host = RoundTripHost::new(settings, paths, workspace, endpoint)?;
    let mut history = History::new();
    history.push_user(PROMPT);
    let outcome = turn::run_turn(&mut history, &host);
    let (calls, error) = match outcome {
        Ok(outcome) => (outcome.calls, outcome.error),
        Err(err) => (Vec::new(), Some(err)),
    };
    let responses = host
        .responses
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    judge(
        &mut report,
        &Exchange {
            responses: &responses,
            called: calls.iter().map(|call| call.call.name.as_str()).collect(),
            error: error.as_ref(),
            replay: replay_field(settings),
        },
    );
    let total = responses.iter().fold(Usage::default(), |mut total, usage| {
        add_usage(&mut total, &usage.usage);
        total
    });
    report.cost_usd = crate::provider_setup::cached_prices(settings, paths)
        .and_then(|prices| prices.cost_at(&total, rune_net::transport::now_timestamp()))
        .map(rune_net::models_dev::dollars);
    report.checks.push(limits_check(settings, paths));
    report.checks.push(RoundTripCheck::value(
        "cost",
        report
            .cost_usd
            .as_ref()
            .map_or_else(|| "unknown".to_owned(), |cost| format!("${cost}")),
    ));
    Ok(report)
}

/// One response as it arrived.
#[derive(Clone, Debug, Default)]
struct Response {
    reasoning: bool,
    usage: Usage,
}

/// What the exchange produced, for [`judge`].
struct Exchange<'a> {
    responses: &'a [Response],
    called: Vec<&'a str>,
    error: Option<&'a RuneError>,
    /// The field reasoning is sent back in, when the endpoint takes it back.
    replay: Option<&'static str>,
}

/// Fills in the checks the exchange decides.
fn judge(report: &mut ProviderReport, exchange: &Exchange<'_>) {
    let checks = &mut report.checks;
    let refused = exchange.error.filter(|err| {
        matches!(
            err.code(),
            ErrorCode::AuthenticationRequired | ErrorCode::PermissionDenied
        )
    });
    if exchange.responses.is_empty() {
        if let Some(err) = refused {
            checks.push(RoundTripCheck::ok("reachable", ""));
            checks.push(RoundTripCheck::failed("authenticated", err.message()));
            push_rest_not_applicable(checks);
            return;
        }
        if let Some(err) = exchange
            .error
            .filter(|err| matches!(err.code(), ErrorCode::TransportFailure | ErrorCode::Timeout))
        {
            checks.push(RoundTripCheck::failed("reachable", err.message()));
            checks.push(RoundTripCheck::not_applicable("authenticated", ""));
            push_rest_not_applicable(checks);
            return;
        }
    }
    checks.push(RoundTripCheck::ok("reachable", ""));
    checks.push(RoundTripCheck::ok("authenticated", ""));
    let first_failed = exchange.responses.is_empty();
    checks.push(match (exchange.called.first(), exchange.error) {
        (Some(&TOOL_NAME), _) => RoundTripCheck::ok("tool call parsed", ""),
        (Some(other), _) => RoundTripCheck::failed(
            "tool call parsed",
            format!("called {other} instead of {TOOL_NAME}"),
        ),
        (None, Some(err)) if first_failed => {
            RoundTripCheck::failed("tool call parsed", err.message())
        }
        (None, _) => RoundTripCheck::failed("tool call parsed", "no tool call"),
    });
    let reasoned = exchange
        .responses
        .first()
        .is_some_and(|first| first.reasoning);
    checks.push(match (reasoned, exchange.replay) {
        (false, _) => RoundTripCheck::not_applicable("reasoning replay", "no reasoning"),
        (true, None) => {
            RoundTripCheck::not_applicable("reasoning replay", "this endpoint takes none back")
        }
        (true, Some(_)) if exchange.called.is_empty() => {
            RoundTripCheck::not_applicable("reasoning replay", "no tool call to answer")
        }
        (true, Some(field)) if exchange.responses.len() >= 2 => {
            RoundTripCheck::ok("reasoning replay", format!("sent back as {field}"))
        }
        (true, Some(_)) => RoundTripCheck::failed(
            "reasoning replay",
            exchange.error.map_or_else(
                || "the second request was not sent".to_owned(),
                |err| err.message().to_owned(),
            ),
        ),
    });
    let mut total = Usage::default();
    for response in exchange.responses {
        add_usage(&mut total, &response.usage);
    }
    checks.push(if total.is_empty() {
        RoundTripCheck::failed("usage reported", "no token counts in the responses")
    } else {
        RoundTripCheck::ok(
            "usage reported",
            format!(
                "input {}, output {}, cached {}",
                total.input_tokens.unwrap_or_default(),
                total.output_tokens.unwrap_or_default(),
                total.cache_read_tokens.unwrap_or_default()
            ),
        )
    });
    let cache_seen = exchange.responses.iter().any(|response| {
        response.usage.cache_read_tokens.is_some() || response.usage.cache_write_tokens.is_some()
    });
    checks.push(RoundTripCheck::value(
        "cache fields",
        if cache_seen { "seen" } else { "not seen" },
    ));
}

/// The checks after `authenticated`, when nothing was exchanged.
fn push_rest_not_applicable(checks: &mut Vec<RoundTripCheck>) {
    for name in [
        "tool call parsed",
        "reasoning replay",
        "usage reported",
        "cache fields",
    ] {
        checks.push(RoundTripCheck::not_applicable(name, ""));
    }
}

/// Completes a report that stopped before sending.
fn skip_rest(report: &mut ProviderReport, settings: &Settings, paths: &Paths) {
    push_rest_not_applicable(&mut report.checks);
    report.checks.push(limits_check(settings, paths));
    report.checks.push(RoundTripCheck::value("cost", "unknown"));
}

/// The model's limits and where each came from.
fn limits_check(settings: &Settings, paths: &Paths) -> RoundTripCheck {
    let limits = crate::model_limits::resolve(settings, paths, &settings.model, None);
    let source = |source: crate::model_limits::LimitSource| {
        serde_json::to_value(source)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default()
    };
    let amount =
        |value: Option<u64>| value.map_or_else(|| "?".to_owned(), |value| value.to_string());
    RoundTripCheck::value(
        "limits",
        format!(
            "context {} ({}), output {} ({})",
            amount(limits.context_window),
            source(limits.context_source),
            amount(limits.max_output),
            source(limits.output_source)
        ),
    )
}

/// Where the configured dialect sends earlier reasoning back.
fn replay_field(settings: &Settings) -> Option<&'static str> {
    match settings.provider {
        ProviderName::Anthropic => Some("signed thinking blocks"),
        ProviderName::Responses => Some("encrypted reasoning items"),
        ProviderName::Gemini => Some("thought signatures"),
        _ => {
            let (compat, _) = crate::provider_setup::CompatSource::from_settings(settings)
                .resolve(&settings.model);
            match compat.reasoning_replay {
                ReasoningReplay::None => None,
                ReasoningReplay::ReasoningContent => Some("reasoning_content"),
                ReasoningReplay::Reasoning => Some("reasoning"),
                ReasoningReplay::ReasoningDetails => Some("reasoning_details"),
                ReasoningReplay::AsObserved => Some("the field it arrived in"),
            }
        }
    }
}

fn add_usage(total: &mut Usage, usage: &Usage) {
    let add = |total: &mut Option<u64>, value: Option<u64>| {
        if let Some(value) = value {
            *total = Some(total.unwrap_or_default().saturating_add(value));
        }
    };
    add(&mut total.input_tokens, usage.input_tokens);
    add(&mut total.output_tokens, usage.output_tokens);
    add(&mut total.cache_read_tokens, usage.cache_read_tokens);
    add(&mut total.cache_write_tokens, usage.cache_write_tokens);
    add(&mut total.reasoning_tokens, usage.reasoning_tokens);
}

/// A turn host with one tool, no project context and tight limits.
struct RoundTripHost {
    dialect: Box<dyn Provider>,
    endpoint: Endpoint,
    compat: crate::provider_setup::CompatSource,
    model: String,
    effort: Effort,
    max_output: Option<u64>,
    context_window: Option<u64>,
    workspace: Utf8PathBuf,
    limits: BudgetSet,
    cancellation: Cancellation,
    steering: SteeringQueue,
    responses: Mutex<Vec<Response>>,
}

impl RoundTripHost {
    fn new(
        settings: &Settings,
        paths: &Paths,
        workspace: &Utf8Path,
        endpoint: Endpoint,
    ) -> Result<Self> {
        let compat = crate::provider_setup::CompatSource::from_settings(settings);
        let effort = rune_net::compat::effective_effort(
            settings.effort,
            settings.source_of("effort") != Layer::Default,
            &compat.resolve(&settings.model).1,
        );
        let limits_known = crate::model_limits::resolve(settings, paths, &settings.model, None);
        let mut limits = settings.limits.clone();
        limits.set(
            LimitName::ParallelToolCalls,
            Budget::Bounded(1),
            Layer::CommandLine,
        )?;
        limits.set(
            LimitName::ProviderRequestTimeoutMs,
            Budget::Bounded(REQUEST_TIMEOUT_MS),
            Layer::CommandLine,
        )?;
        // The call and the answer, with room for one repair.
        limits.set(
            LimitName::MaxAgentSteps,
            Budget::Bounded(3),
            Layer::CommandLine,
        )?;
        Ok(Self {
            dialect: crate::provider_setup::dialect_for(&settings.provider),
            endpoint,
            compat,
            model: settings.model.clone(),
            effort,
            max_output: limits_known.max_output,
            context_window: limits_known.context_window,
            workspace: workspace.to_owned(),
            limits,
            cancellation: Cancellation::new(),
            steering: SteeringQueue::new(1),
            responses: Mutex::new(Vec::new()),
        })
    }
}

impl Host for RoundTripHost {
    fn dialect(&self) -> &dyn Provider {
        self.dialect.as_ref()
    }

    fn endpoint(&self) -> Cow<'_, Endpoint> {
        Cow::Borrowed(&self.endpoint)
    }

    fn model(&self) -> String {
        self.model.clone()
    }

    fn compat(&self, model: &str) -> (EndpointCompat, ModelTraits) {
        self.compat.resolve(model)
    }

    fn instructions(&self) -> String {
        "You are checking a connection. Do exactly what the user asks.".to_owned()
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: TOOL_NAME.to_owned(),
            description: "Returns the name of the current directory.".to_owned(),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
        }]
    }

    fn effort(&self) -> Effort {
        self.effort
    }

    fn max_output_tokens(&self) -> Option<u64> {
        self.max_output
    }

    fn context_window(&self) -> Option<u64> {
        self.context_window
    }

    fn observe_response(&self, outcome: &StreamOutcome) {
        let response = Response {
            // Hidden reasoning shows only in its token count.
            reasoning: !outcome.reasoning().is_empty()
                || outcome
                    .usage
                    .reasoning_tokens
                    .is_some_and(|tokens| tokens > 0),
            usage: outcome.usage,
        };
        self.responses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(response);
    }

    fn emit(&self, _event: Event) {}

    fn execute(&self, name: &str, _arguments: &serde_json::Value) -> Result<ToolOutput> {
        if name != TOOL_NAME {
            return Ok(ToolOutput::failure(format!("unknown tool {name}")));
        }
        Ok(ToolOutput::success(
            self.workspace.file_name().unwrap_or("rune"),
        ))
    }

    fn decide(&self, name: &str, _target: Option<&str>) -> (Outcome, String) {
        if name == TOOL_NAME {
            (Outcome::Allow, "the doctor's own tool".to_owned())
        } else {
            (
                Outcome::Deny,
                "only current_directory is offered".to_owned(),
            )
        }
    }

    fn context(&self) -> ExecutionContext {
        ExecutionContext::new(self.workspace.clone())
    }

    fn limits(&self) -> BudgetSet {
        self.limits.clone()
    }

    fn cancellation(&self) -> Cancellation {
        self.cancellation.clone()
    }

    fn steering(&self) -> &SteeringQueue {
        &self.steering
    }

    fn retry_policy(&self, max_attempts: usize) -> RetryPolicy {
        RetryPolicy::headless(
            max_attempts,
            Some(Duration::from_millis(REQUEST_TIMEOUT_MS)),
        )
    }

    fn provider_head_timeout_ms(&self) -> Option<u64> {
        Some(REQUEST_TIMEOUT_MS)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    fn exchange<'a>(
        responses: &'a [Response],
        called: Vec<&'a str>,
        error: Option<&'a RuneError>,
        replay: Option<&'static str>,
    ) -> ProviderReport {
        let mut report = ProviderReport {
            provider: "p".to_owned(),
            model: "m".to_owned(),
            endpoint: "http://x".to_owned(),
            checks: Vec::new(),
            cost_usd: None,
        };
        judge(
            &mut report,
            &Exchange {
                responses,
                called,
                error,
                replay,
            },
        );
        report
    }

    fn status(report: &ProviderReport, name: &str) -> (Status, String) {
        let check = report
            .checks
            .iter()
            .find(|check| check.name == name)
            .expect("check");
        (check.status, check.shown())
    }

    fn usage(input: u64, output: u64, cached: Option<u64>) -> Usage {
        Usage {
            input_tokens: Some(input),
            output_tokens: Some(output),
            cache_read_tokens: cached,
            ..Usage::default()
        }
    }

    #[test]
    fn reasoning_sent_back_is_reported_by_its_field() {
        let responses = [
            Response {
                reasoning: true,
                usage: usage(100, 10, Some(0)),
            },
            Response {
                reasoning: false,
                usage: usage(120, 5, Some(90)),
            },
        ];
        let report = exchange(&responses, vec![TOOL_NAME], None, Some("reasoning_content"));
        assert_eq!(
            status(&report, "reasoning replay").1,
            "ok (sent back as reasoning_content)"
        );
        assert_eq!(
            status(&report, "usage reported").1,
            "ok (input 220, output 15, cached 90)"
        );
        assert_eq!(status(&report, "cache fields").1, "seen");
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn a_refused_second_request_fails_the_replay() {
        let responses = [Response {
            reasoning: true,
            usage: usage(100, 10, None),
        }];
        let refusal = RuneError::new(
            ErrorCode::RequestRejected,
            "Missing reasoning_content field",
        );
        let report = exchange(
            &responses,
            vec![TOOL_NAME],
            Some(&refusal),
            Some("reasoning_content"),
        );
        assert_eq!(status(&report, "tool call parsed").0, Status::Ok);
        assert_eq!(
            status(&report, "reasoning replay").1,
            "failed: Missing reasoning_content field"
        );
        assert_eq!(status(&report, "cache fields").1, "not seen");
        assert_eq!(report.exit_code(), 1);
    }

    #[test]
    fn text_without_a_call_fails_the_parse() {
        let responses = [Response {
            reasoning: false,
            usage: usage(100, 10, None),
        }];
        let report = exchange(&responses, Vec::new(), None, None);
        assert_eq!(
            status(&report, "tool call parsed").1,
            "failed: no tool call"
        );
        assert_eq!(
            status(&report, "reasoning replay").1,
            "not applicable (no reasoning)"
        );
    }

    #[test]
    fn a_connection_failure_marks_reachable_failed() {
        let failure = RuneError::new(ErrorCode::TransportFailure, "connection refused");
        let report = exchange(&[], Vec::new(), Some(&failure), None);
        assert_eq!(status(&report, "reachable").1, "failed: connection refused");
        assert_eq!(status(&report, "authenticated").0, Status::NotApplicable);
        assert_eq!(status(&report, "usage reported").0, Status::NotApplicable);
    }

    #[test]
    fn the_estimate_has_four_decimal_places() {
        assert_eq!(four_places(0), "0.0000");
        assert_eq!(four_places(1_234), "0.0012");
        assert_eq!(four_places(1_250_049), "1.2500");
        assert_eq!(four_places(9_999_950), "10.0000");
    }
}
