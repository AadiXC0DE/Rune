//! User-authorized programs run at points of a session.
//!
//! A hook gets a JSON payload on standard input and may answer with a JSON
//! reply on standard output. Hooks run as the user, outside the sandbox, with
//! a minimal environment.

use std::time::{Duration, Instant};

use camino::Utf8Path;
use rune_agent::steering::Cancellation;
use rune_agent::turn::{CallResult, HookVeto, PostToolEffect, PreToolOutcome, PreparedCall};
use rune_core::config::{Hook, HookEvent, OnError};
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_exec::command::minimal_environment;
use rune_exec::session::{Input, POLL_INTERVAL, Process};
use serde::Deserialize;
use serde_json::{Map, Value};

const MAX_PAYLOAD_BYTES: usize = 256 * 1024;
const MAX_REPLY_BYTES: usize = 16 * 1024;
/// Output a hook converted from `pre_tool_hook` may print, as before.
const MAX_LEGACY_REPLY_BYTES: usize = 4 * 1024;
const MAX_LEGACY_INPUT_BYTES: usize = 32 * 1024;
const MAX_LEGACY_RESULT_BYTES: usize = 4 * 1024;
const MAX_OUTPUT_FIELD_BYTES: usize = 16 * 1024;
const MAX_APPEND_BYTES: usize = 4 * 1024;

/// Fields common to every hook payload.
pub struct Envelope<'a> {
    pub session_id: &'a str,
    pub cwd: &'a Utf8Path,
    pub turn: u64,
}

/// Runs one hook with `payload` on standard input and returns its parsed reply.
/// An empty standard output is `Ok(None)`.
#[cfg_attr(not(test), allow(dead_code))]
pub fn run(
    hook: &Hook,
    envelope: &Envelope<'_>,
    payload: Map<String, Value>,
    cancellation: &Cancellation,
) -> Result<Option<Value>> {
    run_with(hook, envelope, payload, None, &|| cancellation.check())
}

/// The legacy variable a converted hook also receives, with its value.
type Legacy = (&'static str, String);

fn run_with(
    hook: &Hook,
    envelope: &Envelope<'_>,
    payload: Map<String, Value>,
    legacy: Option<Legacy>,
    check: &dyn Fn() -> Result<()>,
) -> Result<Option<Value>> {
    check()?;
    let event = hook.event.name();
    let mut object = Map::new();
    object.insert("event".to_owned(), Value::from(event));
    object.insert("session_id".to_owned(), Value::from(envelope.session_id));
    object.insert("cwd".to_owned(), Value::from(envelope.cwd.as_str()));
    object.insert("turn".to_owned(), Value::from(envelope.turn));
    object.extend(payload);
    let input = serde_json::to_vec(&Value::Object(object))?;
    if input.len() > MAX_PAYLOAD_BYTES {
        return Err(RuneError::new(
            ErrorCode::TooLarge,
            format!("hook payload exceeds {MAX_PAYLOAD_BYTES} bytes"),
        ));
    }
    let mut environment = minimal_environment();
    environment.insert("RUNE_HOOK_EVENT".to_owned(), event.to_owned());
    if let Some((name, value)) = legacy {
        if value.len() > MAX_LEGACY_INPUT_BYTES {
            return Err(RuneError::new(
                ErrorCode::TooLarge,
                format!("hook input exceeds {MAX_LEGACY_INPUT_BYTES} bytes"),
            ));
        }
        environment.insert(name.to_owned(), value);
    }
    let reply_cap = if hook.legacy {
        MAX_LEGACY_REPLY_BYTES
    } else {
        MAX_REPLY_BYTES
    };
    let started = Instant::now();
    let timeout = Duration::from_millis(hook.timeout_ms);
    let process = Process::start_argv(
        &hook.argv,
        Some(envelope.cwd.as_std_path()),
        reply_cap,
        &environment,
        Input::Piped,
    )?;
    // A hook may exit without reading its input, and a large payload may
    // outlast the wait; neither is a failure, the deadline below still holds.
    match process.write(&input, timeout.min(Duration::from_millis(250))) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::BrokenPipe
            ) => {}
        Err(_) if process.exit().is_some() => {}
        Err(error) => return Err(error.into()),
    }
    process.close_input();
    let exit = loop {
        check()?;
        if started.elapsed() >= timeout {
            return Err(RuneError::new(
                ErrorCode::LimitExceeded,
                "hook exceeded its deadline",
            ));
        }
        if let Some(exit) = process.exit() {
            break exit;
        }
        std::thread::sleep(POLL_INTERVAL.min(timeout.saturating_sub(started.elapsed())));
    };
    process.drain_output(
        timeout
            .saturating_sub(started.elapsed())
            .min(Duration::from_millis(250)),
    );
    check()?;
    if !exit.is_success() {
        return Err(RuneError::new(
            ErrorCode::PermissionDenied,
            format!("hook {}", exit.describe()),
        ));
    }
    // A hook converted from `post_tool_hook` only observes, as before.
    if hook.legacy && hook.event == HookEvent::PostTool {
        return Ok(None);
    }
    let stdout = process.stdout().since(0);
    if stdout.produced > reply_cap as u64 {
        return Err(RuneError::new(
            ErrorCode::TooLarge,
            format!("hook output exceeds {reply_cap} bytes"),
        ));
    }
    if stdout.bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    Ok(Some(serde_json::from_slice(&stdout.bytes)?))
    // Dropping a running Process stops its process group on errors.
}

#[derive(Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
enum PreToolReply {
    Allow,
    #[serde(alias = "veto")]
    Deny {
        reason: String,
    },
    Rewrite {
        arguments: Map<String, Value>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PostToolReply {
    #[serde(default)]
    append: Option<String>,
    #[serde(default)]
    replace: Option<String>,
}

/// What a host needs to run tool hooks.
pub struct ToolHooks<'a> {
    pub hooks: &'a [Hook],
    pub envelope: Envelope<'a>,
    pub cancellation: &'a Cancellation,
    /// Checked with the cancellation, such as a tool context's own.
    pub check: &'a dyn Fn() -> Result<()>,
    /// Receives a safe message when a hook failure has no effect.
    pub warn: &'a dyn Fn(&str),
    /// Provider credential removed from everything a hook sees.
    pub credential: &'a str,
    /// Largest text a post-tool `replace` may carry.
    pub max_result_bytes: usize,
}

impl ToolHooks<'_> {
    fn check(&self) -> Result<()> {
        self.cancellation.check()?;
        (self.check)()
    }

    /// Runs every matching pre-tool hook in order; the first deny wins and a
    /// rewrite passes its arguments to the next hook.
    pub fn pre_tool(&self, call: &PreparedCall) -> Result<PreToolOutcome> {
        let mut arguments = call.arguments.clone();
        let mut rewritten_by = None;
        for hook in self
            .hooks
            .iter()
            .filter(|hook| hook.event == HookEvent::PreTool && hook.applies_to(&call.name))
        {
            let parsed: Value = serde_json::from_str(&arguments)?;
            let mut payload = Map::new();
            payload.insert("id".to_owned(), Value::from(call.id.as_str()));
            payload.insert("name".to_owned(), Value::from(call.name.as_str()));
            payload.insert("arguments".to_owned(), parsed.clone());
            let legacy = hook.legacy.then(|| {
                let old = serde_json::json!({
                    "event": "pre_tool",
                    "id": call.id,
                    "name": call.name,
                    "arguments": parsed,
                });
                ("RUNE_PRE_TOOL_CALL", old.to_string())
            });
            let reply = run_with(hook, &self.envelope, payload, legacy, &|| self.check()).and_then(
                |reply| {
                    let reply = reply.ok_or_else(|| {
                        RuneError::new(ErrorCode::ProtocolViolation, "hook gave no decision")
                    })?;
                    Ok(serde_json::from_value::<PreToolReply>(reply)?)
                },
            );
            match reply {
                Ok(PreToolReply::Allow) => {}
                Ok(PreToolReply::Deny { reason }) => {
                    let reason = if reason.trim().is_empty() {
                        "hook returned an empty veto reason".to_owned()
                    } else {
                        reason
                    };
                    return Ok(PreToolOutcome::Veto(HookVeto {
                        name: hook.name.clone(),
                        reason,
                    }));
                }
                Ok(PreToolReply::Rewrite { arguments: new }) => {
                    arguments = Value::Object(new).to_string();
                    rewritten_by = Some(hook.name.clone());
                }
                Err(error) if error.code() == ErrorCode::Cancelled => return Err(error),
                Err(error) => match hook.on_error() {
                    OnError::Deny => {
                        return Ok(PreToolOutcome::Veto(HookVeto {
                            name: hook.name.clone(),
                            reason: format!("hook failed: {error}"),
                        }));
                    }
                    OnError::Allow => (self.warn)(&redact(
                        &format!("pre-tool hook `{}` failed: {error}", hook.name),
                        self.credential,
                    )),
                },
            }
        }
        Ok(match rewritten_by {
            Some(hook) => PreToolOutcome::Rewrite { hook, arguments },
            None => PreToolOutcome::Allow,
        })
    }

    /// Runs every matching post-tool hook in order and combines their effects:
    /// a later `replace` wins over earlier effects, appends accumulate.
    pub fn post_tool(&self, result: &CallResult) -> Option<PostToolEffect> {
        let mut replaced: Option<String> = None;
        let mut appended: Vec<String> = Vec::new();
        for hook in self
            .hooks
            .iter()
            .filter(|hook| hook.event == HookEvent::PostTool && hook.applies_to(&result.call.name))
        {
            let reply = self.post_tool_hook(hook, result);
            match reply {
                Ok(Some(PostToolReply {
                    replace: Some(text),
                    ..
                })) => {
                    replaced = Some(text);
                    appended.clear();
                }
                Ok(Some(PostToolReply {
                    append: Some(text), ..
                })) => appended.push(text),
                Ok(_) => {}
                Err(error) if error.code() == ErrorCode::Cancelled => return None,
                Err(error) => (self.warn)(&redact(
                    &format!("post-tool hook `{}` failed: {error}", hook.name),
                    self.credential,
                )),
            }
        }
        let appended = appended.join("\n");
        match replaced {
            Some(mut text) => {
                if !appended.is_empty() {
                    text.push_str("\n\n[hook output]\n");
                    text.push_str(&appended);
                }
                Some(PostToolEffect::Replace(text))
            }
            None if !appended.is_empty() => Some(PostToolEffect::Append(appended)),
            None => None,
        }
    }

    fn post_tool_hook(&self, hook: &Hook, result: &CallResult) -> Result<Option<PostToolReply>> {
        let (output, output_truncated) =
            redacted_output(result, self.credential, MAX_OUTPUT_FIELD_BYTES);
        let mut arguments = serde_json::from_str::<Value>(&result.call.arguments)
            .unwrap_or_else(|_| Value::from(result.call.arguments.as_str()));
        redact_value(&mut arguments, self.credential);
        let mut payload = Map::new();
        payload.insert(
            "id".to_owned(),
            Value::from(redact(&result.call.id, self.credential)),
        );
        payload.insert(
            "name".to_owned(),
            Value::from(redact(&result.call.name, self.credential)),
        );
        payload.insert("arguments".to_owned(), arguments);
        payload.insert("output".to_owned(), Value::from(output));
        payload.insert("output_truncated".to_owned(), Value::from(output_truncated));
        payload.insert("is_error".to_owned(), Value::from(result.output.is_error));
        payload.insert("executed".to_owned(), Value::from(result.executed));
        let legacy = if hook.legacy {
            Some((
                "RUNE_POST_TOOL_RESULT",
                legacy_post_event(result, self.credential)?,
            ))
        } else {
            None
        };
        let Some(reply) = run_with(hook, &self.envelope, payload, legacy, &|| self.check())? else {
            return Ok(None);
        };
        let reply: PostToolReply = serde_json::from_value(reply)?;
        match (&reply.append, &reply.replace) {
            (Some(_), Some(_)) => Err(RuneError::new(
                ErrorCode::ProtocolViolation,
                "a post_tool reply sets both append and replace",
            )),
            (Some(text), None) if text.len() > MAX_APPEND_BYTES => Err(RuneError::new(
                ErrorCode::TooLarge,
                format!("hook append exceeds {MAX_APPEND_BYTES} bytes"),
            )),
            (None, Some(text)) if text.len() > self.max_result_bytes => Err(RuneError::new(
                ErrorCode::TooLarge,
                format!("hook replace exceeds {} bytes", self.max_result_bytes),
            )),
            _ => Ok(Some(reply)),
        }
    }
}

/// Removes the credential and known secret shapes from text.
pub fn redact(text: &str, credential: &str) -> String {
    let text = if credential.is_empty() {
        text.to_owned()
    } else {
        text.replace(credential, "[redacted]")
    };
    rune_net::redact::redact(&text)
}

// Structured results can carry short secrets in nested fields. Redact those
// values before applying the normal diagnostic redactor to individual strings.
fn redact_value(value: &mut Value, credential: &str) {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                let key = key.to_ascii_lowercase();
                if [
                    "api_key",
                    "api-key",
                    "apikey",
                    "authorization",
                    "token",
                    "secret",
                    "credential",
                    "password",
                ]
                .iter()
                .any(|name| key.contains(name))
                {
                    *value = Value::String("[redacted]".to_owned());
                } else {
                    redact_value(value, credential);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_value(value, credential);
            }
        }
        Value::String(text) => *text = redact(text, credential),
        _ => {}
    }
}

/// The redacted result text, cut at `cap` bytes on a character boundary.
fn redacted_output(result: &CallResult, credential: &str, cap: usize) -> (String, bool) {
    let rendered = result.output.render();
    let mut output = match serde_json::from_str::<Value>(&rendered) {
        Ok(mut value) => {
            redact_value(&mut value, credential);
            // Also sanitize keys and scalar values without compromising the
            // outer event's JSON encoding.
            redact(&value.to_string(), credential)
        }
        Err(_) => redact(&rendered, credential),
    };
    let truncated = output.len() > cap;
    if truncated {
        let mut end = cap;
        while !output.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        output.truncate(end);
    }
    (output, truncated)
}

/// The event `post_tool_hook` scripts read from `RUNE_POST_TOOL_RESULT`.
fn legacy_post_event(result: &CallResult, credential: &str) -> Result<String> {
    let (output, truncated) = redacted_output(result, credential, MAX_LEGACY_RESULT_BYTES);
    Ok(serde_json::to_string(&serde_json::json!({
        "event": "post_tool",
        "id": redact(&result.call.id, credential),
        "name": redact(&result.call.name, credential),
        "executed": result.executed,
        "is_error": result.output.is_error,
        "output": output,
        "output_truncated": truncated,
    }))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(output: &str) -> CallResult {
        CallResult {
            call: PreparedCall {
                id: "fixture-call".to_owned(),
                name: "fixture_tool".to_owned(),
                arguments: r#"{"password":"argument-secret"}"#.to_owned(),
            },
            output: rune_tools::ToolOutput::success(output),
            executed: true,
        }
    }

    fn hook(event: HookEvent, script: &str, timeout_ms: u64) -> Hook {
        Hook {
            event,
            name: "fixture hook".to_owned(),
            argv: vec!["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()],
            timeout_ms,
            on_error: None,
            tools: None,
            project: false,
            legacy: false,
        }
    }

    struct Fixture {
        directory: tempfile::TempDir,
        warnings: std::sync::Mutex<Vec<String>>,
        cancellation: Cancellation,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                directory: tempfile::tempdir().expect("workspace"),
                warnings: std::sync::Mutex::new(Vec::new()),
                cancellation: Cancellation::new(),
            }
        }

        fn cwd(&self) -> &Utf8Path {
            Utf8Path::from_path(self.directory.path()).expect("UTF-8")
        }

        fn path(&self, name: &str) -> std::path::PathBuf {
            self.directory.path().join(name)
        }

        fn with<T>(&self, hooks: &[Hook], body: impl FnOnce(&ToolHooks<'_>) -> T) -> T {
            let warn = |message: &str| {
                self.warnings
                    .lock()
                    .expect("warnings")
                    .push(message.to_owned());
            };
            let tool_hooks = ToolHooks {
                hooks,
                envelope: Envelope {
                    session_id: "fixture-session",
                    cwd: self.cwd(),
                    turn: 3,
                },
                cancellation: &self.cancellation,
                check: &|| Ok(()),
                warn: &warn,
                credential: "fixture-key",
                max_result_bytes: 1024,
            };
            body(&tool_hooks)
        }
    }

    fn read_call() -> PreparedCall {
        PreparedCall {
            id: "call-1".to_owned(),
            name: "read_file".to_owned(),
            arguments: r#"{"path":"a.txt"}"#.to_owned(),
        }
    }

    #[test]
    fn event_redacts_nested_short_secrets_and_bounds_utf8_after_redaction() {
        let result = fixture(
            r#"{"message":"visible","nested":[{"password":"tiny","access_token":"short","answer":"fixture-key"}]}"#,
        );
        let input = legacy_post_event(&result, "fixture-key").expect("event");
        for secret in ["tiny", "short", "fixture-key", "argument-secret"] {
            assert!(!input.contains(secret), "{input}");
        }
        assert!(input.contains("visible"));
        assert_eq!(
            result.output.text,
            r#"{"message":"visible","nested":[{"password":"tiny","access_token":"short","answer":"fixture-key"}]}"#
        );
        let result = fixture(&format!("{}sk-tail-secret", "界".repeat(6000)));
        let input = legacy_post_event(&result, "").expect("event");
        let input: Value = serde_json::from_str(&input).expect("JSON");
        assert_eq!(input["output_truncated"], true);
        assert!(input["output"].as_str().expect("text").len() <= MAX_LEGACY_RESULT_BYTES);
        let (output, truncated) = redacted_output(&result, "", MAX_OUTPUT_FIELD_BYTES);
        assert!(truncated);
        assert!(output.len() <= MAX_OUTPUT_FIELD_BYTES);
    }

    #[cfg(unix)]
    #[test]
    fn the_payload_arrives_on_standard_input() {
        let fixture = Fixture::new();
        let hooks = [hook(
            HookEvent::PreTool,
            "cat > out.json; printf '%s' \"$RUNE_HOOK_EVENT\" > event.txt; \
             printf '%s' \"${RUNE_PRE_TOOL_CALL-unset}\" > legacy.txt; \
             printf '{\"decision\":\"allow\"}'",
            5_000,
        )];
        let outcome = fixture
            .with(&hooks, |tool_hooks| tool_hooks.pre_tool(&read_call()))
            .expect("outcome");
        assert!(matches!(outcome, PreToolOutcome::Allow));
        let payload: Value =
            serde_json::from_slice(&std::fs::read(fixture.path("out.json")).expect("payload"))
                .expect("JSON");
        assert_eq!(payload["event"], "pre_tool");
        assert_eq!(payload["session_id"], "fixture-session");
        assert_eq!(payload["turn"], 3);
        assert_eq!(payload["cwd"], fixture.cwd().as_str());
        assert_eq!(payload["id"], "call-1");
        assert_eq!(payload["name"], "read_file");
        assert_eq!(payload["arguments"], serde_json::json!({"path": "a.txt"}));
        assert_eq!(
            std::fs::read_to_string(fixture.path("event.txt")).expect("event"),
            "pre_tool"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.path("legacy.txt")).expect("legacy"),
            "unset"
        );
        // An empty reply from a hook that only observes is no reply.
        let quiet = hook(HookEvent::SessionStart, "cat > /dev/null", 5_000);
        let reply = run(
            &quiet,
            &Envelope {
                session_id: "s",
                cwd: fixture.cwd(),
                turn: 0,
            },
            Map::new(),
            &fixture.cancellation,
        )
        .expect("run");
        assert!(reply.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn legacy_hooks_still_get_their_variable() {
        let fixture = Fixture::new();
        let pre = Hook::from(&rune_core::config::PreToolHook {
            name: "old guard".to_owned(),
            argv: vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "printf '%s' \"$RUNE_PRE_TOOL_CALL\" > pre.json; cat > pre-stdin.json; \
                 printf '{\"decision\":\"veto\",\"reason\":\"held\"}'"
                    .to_owned(),
            ],
            timeout_ms: 5_000,
        });
        let outcome = fixture
            .with(std::slice::from_ref(&pre), |tool_hooks| {
                tool_hooks.pre_tool(&read_call())
            })
            .expect("outcome");
        assert!(matches!(
            outcome,
            PreToolOutcome::Veto(HookVeto { ref name, ref reason })
                if name == "old guard" && reason == "held"
        ));
        let old: Value =
            serde_json::from_slice(&std::fs::read(fixture.path("pre.json")).expect("variable"))
                .expect("JSON");
        assert_eq!(
            old,
            serde_json::json!({"event":"pre_tool","id":"call-1","name":"read_file","arguments":{"path":"a.txt"}})
        );
        let stdin: Value =
            serde_json::from_slice(&std::fs::read(fixture.path("pre-stdin.json")).expect("stdin"))
                .expect("JSON");
        assert_eq!(stdin["session_id"], "fixture-session");

        let post = Hook::from(&rune_core::config::PostToolHook {
            name: "old observer".to_owned(),
            argv: vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "printf '%s' \"$RUNE_POST_TOOL_RESULT\" > post.json; printf 'not json'".to_owned(),
            ],
            timeout_ms: 5_000,
        });
        let effect = fixture.with(std::slice::from_ref(&post), |tool_hooks| {
            tool_hooks.post_tool(&fixture_result())
        });
        assert_eq!(effect, None);
        assert!(fixture.warnings.lock().expect("warnings").is_empty());
        let old: Value =
            serde_json::from_slice(&std::fs::read(fixture.path("post.json")).expect("variable"))
                .expect("JSON");
        assert_eq!(old["event"], "post_tool");
        assert_eq!(old["output"], "done");
        assert!(old.get("arguments").is_none());
    }

    fn fixture_result() -> CallResult {
        fixture("done")
    }

    #[cfg(unix)]
    #[test]
    fn a_timeout_denies_a_pre_tool_call() {
        let fixture = Fixture::new();
        let hooks = [hook(
            HookEvent::PreTool,
            "touch started; (sleep 1; touch escaped) & wait",
            150,
        )];
        let started = Instant::now();
        let outcome = fixture
            .with(&hooks, |tool_hooks| tool_hooks.pre_tool(&read_call()))
            .expect("outcome");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(matches!(
            outcome,
            PreToolOutcome::Veto(HookVeto { ref reason, .. })
                if reason == "hook failed: limit_exceeded: hook exceeded its deadline"
        ));
        assert!(fixture.path("started").exists());
        std::thread::sleep(Duration::from_millis(1200));
        assert!(
            !fixture.path("escaped").exists(),
            "descendant outlived timeout"
        );
    }

    #[cfg(unix)]
    #[test]
    fn on_error_allow_lets_the_call_run() {
        let fixture = Fixture::new();
        let mut failing = hook(HookEvent::PreTool, "exit 3", 5_000);
        failing.on_error = Some(OnError::Allow);
        let mut rewriting = hook(
            HookEvent::PreTool,
            "printf '{\"decision\":\"rewrite\",\"arguments\":{\"path\": \"b.txt\"}}'",
            5_000,
        );
        rewriting.name = "rewriter".to_owned();
        let mut other_tool = hook(HookEvent::PreTool, "exit 9", 5_000);
        other_tool.tools = Some(vec!["shell".to_owned()]);
        let hooks = [failing, rewriting, other_tool];
        let outcome = fixture
            .with(&hooks, |tool_hooks| tool_hooks.pre_tool(&read_call()))
            .expect("outcome");
        assert!(matches!(
            outcome,
            PreToolOutcome::Rewrite { ref hook, ref arguments }
                if hook == "rewriter" && arguments == r#"{"path":"b.txt"}"#
        ));
        assert_eq!(
            *fixture.warnings.lock().expect("warnings"),
            ["pre-tool hook `fixture hook` failed: permission_denied: hook exited with status 3"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn oversized_output_is_a_failure() {
        let fixture = Fixture::new();
        let hooks = [hook(HookEvent::PreTool, "head -c 20000 /dev/zero", 5_000)];
        let outcome = fixture
            .with(&hooks, |tool_hooks| tool_hooks.pre_tool(&read_call()))
            .expect("outcome");
        assert!(matches!(
            outcome,
            PreToolOutcome::Veto(HookVeto { ref reason, .. })
                if reason == "hook failed: too_large: hook output exceeds 16384 bytes"
        ));
        let mut long = hook(
            HookEvent::PostTool,
            "printf '{\"append\":\"'; head -c 5000 /dev/zero | tr '\\0' x; printf '\"}'",
            5_000,
        );
        long.name = "long".to_owned();
        let effect = fixture.with(std::slice::from_ref(&long), |tool_hooks| {
            tool_hooks.post_tool(&fixture_result())
        });
        assert_eq!(effect, None);
        assert_eq!(
            *fixture.warnings.lock().expect("warnings"),
            ["post-tool hook `long` failed: too_large: hook append exceeds 4096 bytes"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn post_tool_effects_combine_in_order() {
        let fixture = Fixture::new();
        let reply = |text: &str| {
            hook(
                HookEvent::PostTool,
                &format!("cat > /dev/null; printf '%s' '{text}'"),
                5_000,
            )
        };
        let hooks = [
            reply(r#"{"append":"one"}"#),
            reply(r#"{"replace":"fresh"}"#),
            reply(r#"{"append":"two"}"#),
            reply(r#"{"append":"three"}"#),
            reply("{}"),
        ];
        let effect = fixture.with(&hooks, |tool_hooks| tool_hooks.post_tool(&fixture_result()));
        assert_eq!(
            effect,
            Some(PostToolEffect::Replace(
                "fresh\n\n[hook output]\ntwo\nthree".to_owned()
            ))
        );
        let effect = fixture.with(&hooks[2..], |tool_hooks| {
            tool_hooks.post_tool(&fixture_result())
        });
        assert_eq!(
            effect,
            Some(PostToolEffect::Append("two\nthree".to_owned()))
        );
        // The payload carries redacted arguments and output.
        let capture = hook(HookEvent::PostTool, "cat > post.json", 5_000);
        let mut result = self::fixture("fixture-key and more");
        result.call.arguments = r#"{"password":"argument-secret","path":"x"}"#.to_owned();
        fixture.with(std::slice::from_ref(&capture), |tool_hooks| {
            tool_hooks.post_tool(&result)
        });
        let text = std::fs::read_to_string(fixture.path("post.json")).expect("payload");
        assert!(
            !text.contains("fixture-key") && !text.contains("argument-secret"),
            "{text}"
        );
        let payload: Value = serde_json::from_str(&text).expect("JSON");
        assert_eq!(payload["arguments"]["path"], "x");
        assert_eq!(payload["executed"], true);
        assert_eq!(payload["is_error"], false);
        assert_eq!(payload["output_truncated"], false);
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_stops_a_running_hook() {
        let fixture = Fixture::new();
        let hooks = [hook(HookEvent::PreTool, "touch started; sleep 30", 5_000)];
        let started = Instant::now();
        std::thread::scope(|scope| {
            let worker = scope
                .spawn(|| fixture.with(&hooks, |tool_hooks| tool_hooks.pre_tool(&read_call())));
            while !fixture.path("started").exists() {
                assert!(
                    started.elapsed() < Duration::from_secs(3),
                    "hook did not start"
                );
                std::thread::sleep(POLL_INTERVAL);
            }
            fixture.cancellation.cancel();
            let error = worker.join().expect("worker").expect_err("cancelled");
            assert_eq!(error.code(), ErrorCode::Cancelled);
        });
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
