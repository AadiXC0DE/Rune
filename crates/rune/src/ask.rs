//! The one-shot runner.
//!
//! Runs a single request, prints the assistant output to standard output and
//! every diagnostic to standard error, and exits. With `--json` it prints one
//! object instead, which is the contract scripts depend on.
//!
//! The key set and its order are fixed. `output` is the text produced during the
//! request; `final_output` is the completed final response. A usage count the
//! provider did not report is `null`, and a reported zero stays `0`, because the
//! difference matters to a caller summing them.

use std::io::Write;

use rune_core::config::{Effort, Settings};
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::paths::Paths;
use rune_net::message::Message;
use rune_net::provider::{Provider, RequestPlan};
use rune_net::stream::FinishReason;
use rune_net::transport::{self, AuthStyle};
use serde::Serialize;

/// Exit code for a successful run.
pub const EXIT_OK: u8 = 0;

/// Exit code for a failed run.
pub const EXIT_FAILURE: u8 = 1;

/// Largest prompt accepted from standard input.
pub const MAX_STDIN_PROMPT_BYTES: usize = 8 * 1024 * 1024;

/// Options for one run.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// The prompt text.
    pub prompt: String,
    /// Emit one JSON object instead of Markdown.
    pub json: bool,
    /// Do not create or update a session.
    pub no_save: bool,
    /// Model override for this run.
    pub model: Option<String>,
    /// Reasoning effort override.
    pub effort: Option<String>,
}

/// One tool call as reported in the JSON result.
#[derive(Clone, Debug, Serialize)]
pub struct ToolCallReport {
    /// Tool name.
    pub name: String,
    /// `success` or `error`.
    pub status: &'static str,
}

/// The JSON result object.
///
/// Field order here is the field order on the wire, because the struct is
/// serialized in declaration order.
#[derive(Clone, Debug, Serialize)]
pub struct JsonResult {
    /// Assistant text produced during the request.
    pub output: String,
    /// The completed final response, or an empty string.
    pub final_output: String,
    /// Process exit code this run will use.
    pub exit_code: i32,
    /// Model identifier that served the request.
    pub model: String,
    /// Upstream provider that served it, when the endpoint reported one.
    pub resolved_provider: Option<String>,
    /// Session identifier, empty when the run was not saved.
    pub session_id: String,
    /// Model steps taken.
    pub steps: u32,
    /// Token counts. A count the provider did not report is absent.
    pub usage: UsageReport,
    /// Tool calls made, in order.
    pub tool_calls: Vec<ToolCallReport>,
    /// Failure detail, present only when the run failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Stable failure code, present only when the run failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<ErrorCode>,
}

/// Token counts as reported.
#[derive(Clone, Debug, Default, Serialize)]
pub struct UsageReport {
    /// Input tokens, or absent when unreported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Output tokens, or absent when unreported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
}

impl JsonResult {
    /// Builds a failing result, used so `--json` always emits one object.
    #[must_use]
    pub fn failure(model: &str, error: &RuneError, exit_code: i32) -> Self {
        Self {
            output: String::new(),
            final_output: String::new(),
            exit_code,
            model: model.to_owned(),
            resolved_provider: None,
            session_id: String::new(),
            steps: 0,
            usage: UsageReport::default(),
            tool_calls: Vec::new(),
            error: Some(error.message().to_owned()),
            error_code: Some(error.code()),
        }
    }

    /// Renders the object as one line of JSON.
    pub fn render(&self) -> Result<String> {
        serde_json::to_string(self)
            .map_err(|err| RuneError::new(ErrorCode::Internal, err.to_string()))
    }
}

/// Reads the prompt from standard input.
///
/// Bounded, because a prompt is attacker-controlled in a pipeline and an
/// unbounded read would let it exhaust memory.
pub fn read_stdin_prompt() -> Result<String> {
    use std::io::Read;

    let mut buffer = Vec::new();
    let stdin = std::io::stdin();
    let mut handle = stdin.lock().take((MAX_STDIN_PROMPT_BYTES + 1) as u64);
    handle.read_to_end(&mut buffer)?;

    if buffer.len() > MAX_STDIN_PROMPT_BYTES {
        return Err(RuneError::too_large(
            "prompt",
            buffer.len(),
            MAX_STDIN_PROMPT_BYTES,
        ));
    }

    Ok(String::from_utf8_lossy(&buffer).trim_end().to_owned())
}

/// Runs one request and returns the result to report.
///
/// Performs no printing, so the caller decides the output shape and the tests
/// can assert on the result directly.
pub fn run(
    settings: &Settings,
    paths: &Paths,
    options: &Options,
) -> std::result::Result<JsonResult, RuneError> {
    // Validate before touching the network so a missing credential or an
    // unselected model is reported immediately and names the remedy.
    settings.require_model()?;

    if settings.model.trim().is_empty() && options.model.is_none() {
        return Err(rune_core::config::unconfigured_provider_error());
    }

    // Configuration is validated before a credential is looked up, so a user
    // who has not set an endpoint is told that rather than being told about a
    // credential for an endpoint that does not exist.
    let provider_name = settings.provider.to_string();
    let base_url = settings.base_url.clone().ok_or_else(|| {
        RuneError::new(
            ErrorCode::InvalidConfiguration,
            format!("no endpoint is configured for provider `{provider_name}`"),
        )
        .with_hint("set `base_url` in the user config, or run `rune connect`")
    })?;
    transport::validate_url(&base_url)?;

    let credential =
        rune_net::auth::resolve(paths, &provider_name, settings.api_key_env.as_deref())?
            .ok_or_else(|| {
                rune_net::auth::missing_credential_error(
                    &provider_name,
                    settings.api_key_env.as_deref(),
                )
            })?;

    let (dialect, auth): (Box<dyn Provider>, AuthStyle) = match settings.provider {
        rune_core::config::Provider::Anthropic => (
            Box::new(rune_net::anthropic::Anthropic),
            AuthStyle::ApiKeyHeader,
        ),
        rune_core::config::Provider::Responses => {
            (Box::new(rune_net::responses::Responses), AuthStyle::Bearer)
        }
        _ => (
            Box::new(rune_net::chat_completions::ChatCompletions),
            AuthStyle::Bearer,
        ),
    };

    let model = options
        .model
        .clone()
        .unwrap_or_else(|| settings.model.clone());
    if model.trim().is_empty() {
        return Err(RuneError::missing_field("model"));
    }

    // An image attachment is not wired into this path yet, so a run that
    // reaches here has no images to send.
    let mut plan = RequestPlan::new(model.clone());
    plan.messages = vec![Message::user(options.prompt.clone())];
    plan.effort = parse_effort(options.effort.as_deref()).unwrap_or(settings.effort);
    plan.fast_mode = settings.fast_mode;
    plan.provider_order.clone_from(&settings.provider_order);
    plan.provider_strict = settings.provider_strict;

    let endpoint = crate::provider_setup::endpoint(
        &settings.provider,
        &base_url,
        credential.expose(),
        auth,
        settings.offline,
    );

    let agent = transport::agent();
    let head_timeout = std::time::Duration::from_millis(
        settings
            .limits
            .get(rune_core::LimitName::ProviderHeadTimeoutMs)
            .value()
            .unwrap_or(120_000),
    );

    let outcome = transport::stream_completion(
        &agent,
        &endpoint,
        dialect.as_ref(),
        &plan,
        head_timeout,
        &|| false,
    )
    .map_err(|err| err.to_rune_error())?;

    let text = outcome.text();
    let tool_calls: Vec<ToolCallReport> = outcome
        .tool_calls()
        .into_iter()
        .map(|(_, name, _)| ToolCallReport {
            name,
            status: "success",
        })
        .collect();

    let finish = outcome.finish.unwrap_or(FinishReason::Stop);
    let exit_code = if matches!(finish, FinishReason::Stop | FinishReason::ToolCalls) {
        i32::from(EXIT_OK)
    } else {
        i32::from(EXIT_FAILURE)
    };

    Ok(JsonResult {
        final_output: text.clone(),
        output: text,
        exit_code,
        model,
        resolved_provider: None,
        session_id: if options.no_save {
            String::new()
        } else {
            // Sessions land next; a run without one reports an empty id rather
            // than an identifier that cannot be resolved.
            String::new()
        },
        steps: 1,
        usage: UsageReport {
            input_tokens: outcome.usage.input_tokens,
            output_tokens: outcome.usage.output_tokens,
        },
        tool_calls,
        error: None,
        error_code: None,
    })
}

/// Parses an effort name given on the command line.
fn parse_effort(raw: Option<&str>) -> Option<Effort> {
    match raw?.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(Effort::Auto),
        "none" => Some(Effort::None),
        "minimal" => Some(Effort::Minimal),
        "low" => Some(Effort::Low),
        "medium" => Some(Effort::Medium),
        "high" => Some(Effort::High),
        "xhigh" => Some(Effort::Xhigh),
        "max" => Some(Effort::Max),
        _ => None,
    }
}

/// Writes a result to the two output streams.
///
/// Markdown goes to standard output and diagnostics to standard error, so a
/// pipeline can consume the answer while a human still sees progress.
pub fn report(result: &JsonResult, options: &Options) -> Result<u8> {
    if options.json {
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "{}", result.render()?)
            .map_err(|err| RuneError::new(ErrorCode::Internal, err.to_string()))?;
    } else if result.error.is_none() {
        let mut stdout = std::io::stdout().lock();
        write!(stdout, "{}", result.final_output)
            .map_err(|err| RuneError::new(ErrorCode::Internal, err.to_string()))?;
        if !result.final_output.ends_with('\n') {
            let _ = writeln!(stdout);
        }
    }

    if let Some(error) = &result.error {
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "rune: {error}");
    }

    Ok(u8::try_from(result.exit_code).unwrap_or(EXIT_FAILURE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune_core::config::{EnvironmentOverrides, Provider};

    fn settings_with(provider: Provider, model: &str) -> Settings {
        Settings {
            provider,
            model: model.to_owned(),
            base_url: Some("https://api.example.com".to_owned()),
            ..Settings::default()
        }
    }

    #[test]
    fn a_missing_provider_is_reported_before_any_request() {
        let settings = Settings::default();
        let paths = Paths::resolve(Some("/tmp"), None, None, None, Some("/tmp/rune-state"));
        let options = Options {
            prompt: "hi".to_owned(),
            ..Options::default()
        };
        let err = run(&settings, &paths, &options).expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::AuthenticationRequired);
    }

    #[test]
    fn a_missing_model_is_reported_before_any_request() {
        let settings = settings_with(Provider::Anthropic, "");
        let paths = Paths::resolve(Some("/tmp"), None, None, None, Some("/tmp/rune-state"));
        let options = Options {
            prompt: "hi".to_owned(),
            ..Options::default()
        };
        let err = run(&settings, &paths, &options).expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::InvalidConfiguration);
    }

    #[test]
    fn a_missing_credential_names_every_source_tried() {
        let settings = settings_with(Provider::Anthropic, "claude-test");
        let paths = Paths::resolve(Some("/tmp"), None, None, None, Some("/tmp/rune-state"));
        let options = Options {
            prompt: "hi".to_owned(),
            ..Options::default()
        };
        let err = run(&settings, &paths, &options).expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::AuthenticationRequired);
        assert!(err.detail().hint.is_some());
    }

    #[test]
    fn a_missing_endpoint_is_reported_with_a_remedy() {
        let mut settings = settings_with(Provider::Anthropic, "claude-test");
        settings.base_url = None;
        let dir = tempfile::TempDir::new().expect("tempdir");
        let paths = Paths::resolve(
            Some(dir.path().to_str().unwrap_or("/tmp")),
            None,
            None,
            None,
            Some(dir.path().join("state").to_str().unwrap_or("/tmp/s")),
        );
        let options = Options {
            prompt: "hi".to_owned(),
            ..Options::default()
        };
        let err = run(&settings, &paths, &options).expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::InvalidConfiguration);
        assert!(err.detail().hint.is_some());
    }

    #[test]
    fn a_failure_result_serializes_with_the_stable_code() {
        let err = RuneError::new(ErrorCode::RateLimited, "slow down");
        let result = JsonResult::failure("m", &err, 1);
        let json = result.render().expect("render");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(value["error_code"], "rate_limited");
        assert_eq!(value["exit_code"], 1);
        assert_eq!(value["output"], "");
        assert_eq!(value["final_output"], "");
    }

    #[test]
    fn the_json_object_has_the_documented_key_set() {
        let result = JsonResult::failure("m", &RuneError::new(ErrorCode::Internal, "x"), 1);
        let json = result.render().expect("render");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        let object = value.as_object().expect("object");
        for key in [
            "output",
            "final_output",
            "exit_code",
            "model",
            "resolved_provider",
            "session_id",
            "steps",
            "usage",
            "tool_calls",
        ] {
            assert!(object.contains_key(key), "missing key `{key}`");
        }
    }

    #[test]
    fn the_json_key_order_is_stable() {
        let result = JsonResult::failure("m", &RuneError::new(ErrorCode::Internal, "x"), 1);
        let json = result.render().expect("render");
        let output = json.find("\"output\"").expect("output");
        let final_output = json.find("\"final_output\"").expect("final_output");
        let exit_code = json.find("\"exit_code\"").expect("exit_code");
        assert!(output < final_output);
        assert!(final_output < exit_code);
    }

    #[test]
    fn an_unreported_usage_count_is_absent_rather_than_zero() {
        let usage = UsageReport::default();
        let json = serde_json::to_value(&usage).expect("serialize");
        assert_eq!(json, serde_json::json!({}));
    }

    #[test]
    fn a_reported_zero_usage_count_is_present() {
        let usage = UsageReport {
            input_tokens: Some(0),
            output_tokens: None,
        };
        let json = serde_json::to_value(&usage).expect("serialize");
        assert_eq!(json["input_tokens"], 0);
        assert!(json.get("output_tokens").is_none());
    }

    #[test]
    fn a_success_result_has_no_error_fields() {
        let result = JsonResult {
            output: "hi".to_owned(),
            final_output: "hi".to_owned(),
            exit_code: 0,
            model: "m".to_owned(),
            resolved_provider: None,
            session_id: String::new(),
            steps: 1,
            usage: UsageReport::default(),
            tool_calls: Vec::new(),
            error: None,
            error_code: None,
        };
        let json = result.render().expect("render");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert!(value.get("error").is_none());
        assert!(value.get("error_code").is_none());
    }

    #[test]
    fn a_tool_call_report_carries_a_name_and_status() {
        let report = ToolCallReport {
            name: "read_file".to_owned(),
            status: "success",
        };
        let json = serde_json::to_value(&report).expect("serialize");
        assert_eq!(json["name"], "read_file");
        assert_eq!(json["status"], "success");
    }

    #[test]
    fn effort_parsing_accepts_every_documented_name() {
        assert_eq!(parse_effort(Some("high")), Some(Effort::High));
        assert_eq!(parse_effort(Some("XHIGH")), Some(Effort::Xhigh));
        assert_eq!(parse_effort(Some("auto")), Some(Effort::Auto));
        assert_eq!(parse_effort(Some("sideways")), None);
        assert_eq!(parse_effort(None), None);
    }

    #[test]
    fn the_environment_override_reaches_the_settings() {
        let vars = std::collections::HashMap::from([
            ("RUNE_PROVIDER", "anthropic"),
            ("RUNE_MODEL", "claude-test"),
        ]);
        let env = EnvironmentOverrides::from_lookup(|key| vars.get(key).map(|v| (*v).to_owned()));
        let settings = rune_core::config::load(None, None, &env);
        assert_eq!(settings.provider, Provider::Anthropic);
        assert_eq!(settings.model, "claude-test");
    }
}
