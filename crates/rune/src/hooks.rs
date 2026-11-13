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
const MAX_TURN_TEXT_BYTES: usize = 16 * 1024;

/// Fields common to every hook payload.
pub struct Envelope<'a> {
    pub session_id: &'a str,
    pub cwd: &'a Utf8Path,
    pub turn: u64,
}

/// Runs one hook with `payload` on standard input and returns its parsed reply.
/// An empty standard output is `Ok(None)`.
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

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Block {
    Block,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UserPromptReply {
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    decision: Option<Block>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Continue {
    Continue,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TurnEndReply {
    #[serde(default)]
    decision: Option<Continue>,
    #[serde(default)]
    message: Option<String>,
}

/// What `user_prompt` hooks decided about a prompt.
#[derive(Debug, PartialEq, Eq)]
pub enum PromptOutcome {
    /// Send the prompt, with this context appended when there is any.
    Send(Option<String>),
    /// Do not send it.
    Blocked { hook: String, reason: String },
}

/// What a host needs to run the hooks of session events.
pub struct EventHooks<'a> {
    pub hooks: &'a [Hook],
    pub envelope: Envelope<'a>,
    pub cancellation: &'a Cancellation,
    /// Receives a safe message when a hook failure has no effect.
    pub warn: &'a dyn Fn(&str),
    /// Provider credential removed from everything a hook sees.
    pub credential: &'a str,
}

impl EventHooks<'_> {
    fn of(&self, event: HookEvent) -> impl Iterator<Item = &Hook> {
        self.hooks.iter().filter(move |hook| hook.event == event)
    }

    fn failure(&self, hook: &Hook, error: &RuneError) -> String {
        redact(
            &format!("hook {} failed: {error}", hook.name),
            self.credential,
        )
    }

    /// Runs every `user_prompt` hook in order. The first block wins; context
    /// from every hook is kept, in configuration order.
    pub fn user_prompt(&self, prompt: &str) -> PromptOutcome {
        let mut blocks = Vec::new();
        for hook in self.of(HookEvent::UserPrompt) {
            let mut payload = Map::new();
            payload.insert("prompt".to_owned(), Value::from(prompt));
            let reply =
                run(hook, &self.envelope, payload, self.cancellation).and_then(parse_user_prompt);
            match reply {
                Ok(UserPromptReply {
                    decision: Some(Block::Block),
                    reason,
                    ..
                }) => {
                    let reason = reason
                        .filter(|reason| !reason.trim().is_empty())
                        .unwrap_or_else(|| "no reason given".to_owned());
                    return PromptOutcome::Blocked {
                        hook: hook.name.clone(),
                        reason,
                    };
                }
                Ok(UserPromptReply {
                    context: Some(text),
                    ..
                }) => blocks.push(format!(
                    "<hook-context name=\"{}\">\n{text}\n</hook-context>",
                    hook.name
                )),
                Ok(_) => {}
                Err(error) => match hook.on_error() {
                    OnError::Deny => {
                        return PromptOutcome::Blocked {
                            hook: hook.name.clone(),
                            reason: self.failure(hook, &error),
                        };
                    }
                    OnError::Allow => (self.warn)(&self.failure(hook, &error)),
                },
            }
        }
        PromptOutcome::Send((!blocks.is_empty()).then(|| blocks.join("\n")))
    }

    /// Runs `turn_end` hooks in order and returns the first that asks the
    /// model to continue, with its message.
    pub fn turn_end(&self, text: &str, continues: u32) -> Option<(String, String)> {
        let text = redact(text, self.credential);
        let text = cut(&text, MAX_TURN_TEXT_BYTES);
        for hook in self.of(HookEvent::TurnEnd) {
            let mut payload = Map::new();
            payload.insert("text".to_owned(), Value::from(text));
            payload.insert("continues".to_owned(), Value::from(continues));
            let reply =
                run(hook, &self.envelope, payload, self.cancellation).and_then(parse_turn_end);
            match reply {
                Ok(Some(message)) => return Some((hook.name.clone(), message)),
                Ok(None) => {}
                Err(error) if error.code() == ErrorCode::Cancelled => return None,
                Err(error) => (self.warn)(&self.failure(hook, &error)),
            }
        }
        None
    }

    /// Runs the hooks of an event whose output is ignored.
    pub fn observe(&self, event: HookEvent, payload: &Map<String, Value>) {
        for hook in self.of(event) {
            if let Err(error) = run(hook, &self.envelope, payload.clone(), self.cancellation)
                && error.code() != ErrorCode::Cancelled
            {
                (self.warn)(&self.failure(hook, &error));
            }
        }
    }
}

fn parse_user_prompt(reply: Option<Value>) -> Result<UserPromptReply> {
    let Some(reply) = reply else {
        return Ok(UserPromptReply {
            context: None,
            decision: None,
            reason: None,
        });
    };
    let reply: UserPromptReply = serde_json::from_value(reply)?;
    match (&reply.context, &reply.decision, &reply.reason) {
        (Some(_), Some(_), _) => Err(RuneError::new(
            ErrorCode::ProtocolViolation,
            "a user_prompt reply sets both context and decision",
        )),
        (None, None, Some(_)) => Err(RuneError::new(
            ErrorCode::ProtocolViolation,
            "a user_prompt reply gives a reason without a decision",
        )),
        (Some(text), None, _) if text.len() > MAX_APPEND_BYTES => Err(RuneError::new(
            ErrorCode::TooLarge,
            format!("hook context exceeds {MAX_APPEND_BYTES} bytes"),
        )),
        _ => Ok(reply),
    }
}

fn parse_turn_end(reply: Option<Value>) -> Result<Option<String>> {
    let Some(reply) = reply else {
        return Ok(None);
    };
    let reply: TurnEndReply = serde_json::from_value(reply)?;
    match (reply.decision, reply.message) {
        (None, None) => Ok(None),
        (Some(Continue::Continue), Some(message)) if message.len() > MAX_APPEND_BYTES => {
            Err(RuneError::new(
                ErrorCode::TooLarge,
                format!("hook message exceeds {MAX_APPEND_BYTES} bytes"),
            ))
        }
        (Some(Continue::Continue), Some(message)) if !message.trim().is_empty() => {
            Ok(Some(message))
        }
        (Some(Continue::Continue), _) => Err(RuneError::new(
            ErrorCode::ProtocolViolation,
            "a turn_end continue needs a message",
        )),
        (None, Some(_)) => Err(RuneError::new(
            ErrorCode::ProtocolViolation,
            "a turn_end reply gives a message without a decision",
        )),
    }
}

/// The owned parts of an [`EventHooks`], for hooks run on another thread.
pub struct Detached {
    pub hooks: Vec<Hook>,
    pub session_id: String,
    pub cwd: camino::Utf8PathBuf,
    pub turn: u64,
    pub credential: String,
}

impl Detached {
    /// Borrows these parts as [`EventHooks`].
    pub fn events<'a>(
        &'a self,
        cancellation: &'a Cancellation,
        warn: &'a dyn Fn(&str),
    ) -> EventHooks<'a> {
        EventHooks {
            hooks: &self.hooks,
            envelope: Envelope {
                session_id: &self.session_id,
                cwd: &self.cwd,
                turn: self.turn,
            },
            cancellation,
            warn,
            credential: &self.credential,
        }
    }

    /// Runs the hooks of an event whose output is ignored, each bounded by
    /// its own deadline and not by any cancellation.
    pub fn observe(&self, event: HookEvent, payload: &Map<String, Value>, warn: &dyn Fn(&str)) {
        self.events(&Cancellation::new(), warn)
            .observe(event, payload);
    }

    /// Runs `notification` hooks on their own thread, so a slow hook never
    /// delays the caller. Each hook is still bounded by its own deadline.
    pub fn notify(self, kind: &str, message: &str, warn: impl Fn(&str) + Send + 'static) {
        if !self
            .hooks
            .iter()
            .any(|hook| hook.event == HookEvent::Notification)
        {
            return;
        }
        let payload = notification_payload(kind, &redact(message, &self.credential));
        let spawned = std::thread::Builder::new()
            .name("rune-notification-hook".to_owned())
            .spawn(move || self.observe(HookEvent::Notification, &payload, &warn));
        drop(spawned);
    }
}

/// The payload of a `notification` hook.
pub fn notification_payload(kind: &str, message: &str) -> Map<String, Value> {
    let mut payload = payload("kind", kind);
    payload.insert("message".to_owned(), Value::from(message));
    payload
}

/// Builds the payload object of an event from one field.
pub fn payload(key: &str, value: impl Into<Value>) -> Map<String, Value> {
    let mut payload = Map::new();
    payload.insert(key.to_owned(), value.into());
    payload
}

fn cut(text: &str, cap: usize) -> &str {
    if text.len() <= cap {
        return text;
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    &text[..end]
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

    fn events<'a>(
        fixture: &'a Fixture,
        hooks: &'a [Hook],
        warn: &'a dyn Fn(&str),
    ) -> EventHooks<'a> {
        EventHooks {
            hooks,
            envelope: Envelope {
                session_id: "fixture-session",
                cwd: fixture.cwd(),
                turn: 2,
            },
            cancellation: &fixture.cancellation,
            warn,
            credential: "fixture-key",
        }
    }

    #[cfg(unix)]
    #[test]
    fn user_prompt_context_is_appended() {
        let fixture = Fixture::new();
        let mut first = hook(
            HookEvent::UserPrompt,
            "cat > prompt.json; printf '{\"context\":\"branch is main\"}'",
            5_000,
        );
        first.name = "branch".to_owned();
        let quiet = hook(HookEvent::UserPrompt, "cat > /dev/null", 5_000);
        let mut second = hook(
            HookEvent::UserPrompt,
            "printf '%s' '{\"context\":\"two\\nlines\"}'",
            5_000,
        );
        second.name = "notes".to_owned();
        let mut failing = hook(HookEvent::UserPrompt, "exit 4", 5_000);
        failing.on_error = Some(OnError::Allow);
        let other = hook(HookEvent::TurnEnd, "exit 9", 5_000);
        let hooks = [first, quiet, failing, second, other];
        let warnings = std::sync::Mutex::new(Vec::new());
        let warn = |message: &str| warnings.lock().expect("warnings").push(message.to_owned());
        let outcome = events(&fixture, &hooks, &warn).user_prompt("fix the build");
        assert_eq!(
            outcome,
            PromptOutcome::Send(Some(
                "<hook-context name=\"branch\">\nbranch is main\n</hook-context>\n\
                 <hook-context name=\"notes\">\ntwo\nlines\n</hook-context>"
                    .to_owned()
            ))
        );
        assert_eq!(
            *warnings.lock().expect("warnings"),
            ["hook fixture hook failed: permission_denied: hook exited with status 4"]
        );
        let payload: Value =
            serde_json::from_slice(&std::fs::read(fixture.path("prompt.json")).expect("payload"))
                .expect("JSON");
        assert_eq!(payload["event"], "user_prompt");
        assert_eq!(payload["prompt"], "fix the build");
        assert_eq!(payload["turn"], 2);
        // No hook, or no context, sends the prompt as it is.
        let outcome = events(&fixture, &hooks[1..2], &warn).user_prompt("plain");
        assert_eq!(outcome, PromptOutcome::Send(None));
    }

    #[cfg(unix)]
    #[test]
    fn user_prompt_block_stops_the_prompt() {
        let fixture = Fixture::new();
        let warn = |_: &str| {};
        let mut blocking = hook(
            HookEvent::UserPrompt,
            "printf '{\"decision\":\"block\",\"reason\":\"no secrets\"}'",
            5_000,
        );
        blocking.name = "guard".to_owned();
        let later = hook(HookEvent::UserPrompt, "touch later", 5_000);
        let hooks = [blocking, later];
        let outcome = events(&fixture, &hooks, &warn).user_prompt("print the key");
        assert_eq!(
            outcome,
            PromptOutcome::Blocked {
                hook: "guard".to_owned(),
                reason: "no secrets".to_owned()
            }
        );
        assert!(!fixture.path("later").exists());
        // A hook that times out blocks by default.
        let mut slow = hook(HookEvent::UserPrompt, "sleep 5", 150);
        slow.name = "slow".to_owned();
        let outcome = events(&fixture, std::slice::from_ref(&slow), &warn).user_prompt("hello");
        assert_eq!(
            outcome,
            PromptOutcome::Blocked {
                hook: "slow".to_owned(),
                reason: "hook slow failed: limit_exceeded: hook exceeded its deadline".to_owned()
            }
        );
        // A reply that is not one of the accepted shapes is a failure.
        let odd = hook(
            HookEvent::UserPrompt,
            "printf '{\"decision\":\"allow\"}'",
            5_000,
        );
        let outcome = events(&fixture, std::slice::from_ref(&odd), &warn).user_prompt("hello");
        assert!(
            matches!(outcome, PromptOutcome::Blocked { .. }),
            "{outcome:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn turn_end_hooks_ask_to_continue() {
        let fixture = Fixture::new();
        let warn = |_: &str| {};
        let quiet = hook(HookEvent::TurnEnd, "cat > turn.json; printf '{}'", 5_000);
        let mut asking = hook(
            HookEvent::TurnEnd,
            "printf '{\"decision\":\"continue\",\"message\":\"run the tests\"}'",
            5_000,
        );
        asking.name = "tests".to_owned();
        let hooks = [quiet, asking];
        let reply = events(&fixture, &hooks, &warn).turn_end("done with fixture-key", 1);
        assert_eq!(
            reply,
            Some(("tests".to_owned(), "run the tests".to_owned()))
        );
        let payload: Value =
            serde_json::from_slice(&std::fs::read(fixture.path("turn.json")).expect("payload"))
                .expect("JSON");
        assert_eq!(payload["continues"], 1);
        assert_eq!(payload["text"], "done with [redacted]");
        assert_eq!(events(&fixture, &hooks[..1], &warn).turn_end("x", 0), None);
    }

    #[cfg(unix)]
    #[test]
    fn notifications_do_not_wait_for_the_hook() {
        let fixture = Fixture::new();
        let slow = hook(
            HookEvent::Notification,
            "cat > note.json; sleep 2; touch finished",
            5_000,
        );
        let started = Instant::now();
        Detached {
            hooks: vec![slow],
            session_id: "fixture-session".to_owned(),
            cwd: fixture.cwd().to_owned(),
            turn: 1,
            credential: String::new(),
        }
        .notify("approval_needed", "approve shell: ls", |_| {});
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(!fixture.path("finished").exists());
        let deadline = Instant::now() + Duration::from_secs(10);
        while !fixture.path("finished").exists() {
            assert!(Instant::now() < deadline, "the hook did not run");
            std::thread::sleep(POLL_INTERVAL);
        }
        let payload: Value =
            serde_json::from_slice(&std::fs::read(fixture.path("note.json")).expect("payload"))
                .expect("JSON");
        assert_eq!(payload["kind"], "approval_needed");
        assert_eq!(payload["message"], "approve shell: ls");
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

    #[test]
    fn context_update_and_hook_replies_never_panic() {
        let seeds = [
            r#"{"context": "the build is red; fix it first"}"#,
            r#"{"decision": "deny", "reason": "not on a weekend"}"#,
            r#"{"decision": "continue", "message": "run the tests again"}"#,
            r#"{"decision": "rewrite", "arguments": {"command": "ls -la"}}"#,
            r#"{"append": "note", "replace": null}"#,
        ];
        let mut garble =
            rune_testkit::garble::Garble::new("context_update_and_hook_replies_never_panic");
        let mut parsed = 0_usize;
        for seed in seeds {
            for variant in garble.variants(seed, 5_000) {
                let Ok(reply) = serde_json::from_slice::<Value>(variant.as_bytes()) else {
                    continue;
                };
                parsed += 1;
                let _ = parse_user_prompt(Some(reply.clone()));
                let _ = parse_turn_end(Some(reply.clone()));
                let _ = serde_json::from_value::<PreToolReply>(reply.clone());
                let _ = serde_json::from_value::<PostToolReply>(reply);
            }
        }
        assert!(parsed > 500, "only {parsed} variants parsed as JSON");
    }
}
