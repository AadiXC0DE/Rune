//! User-authorized, supervised observers of redacted tool results.

use std::time::{Duration, Instant};

use rune_agent::steering::Cancellation;
use rune_agent::turn::CallResult;
use rune_core::config::PostToolHook;
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_exec::command::minimal_environment;
use rune_exec::session::{Input, POLL_INTERVAL, Process};
use rune_tools::ExecutionContext;

const MAX_INPUT_BYTES: usize = 32 * 1024;
const MAX_RESULT_BYTES: usize = 4 * 1024;
const MAX_OUTPUT_BYTES: usize = 4 * 1024;

/// Returns a safe diagnostic on failure, without changing the completed result.
pub fn notify(
    hook: Option<&PostToolHook>,
    result: &CallResult,
    context: &ExecutionContext,
    cancellation: &Cancellation,
    credential: &str,
) -> Option<String> {
    let hook = hook?;
    match run(hook, result, context, cancellation, credential) {
        Ok(()) => None,
        Err(error) if error.code() == ErrorCode::Cancelled => None,
        Err(error) => Some(redact(
            &format!("post-tool hook `{}` failed: {error}", hook.name),
            credential,
        )),
    }
}

fn redact(text: &str, credential: &str) -> String {
    let text = if credential.is_empty() {
        text.to_owned()
    } else {
        text.replace(credential, "[redacted]")
    };
    rune_net::redact::redact(&text)
}

// Structured results can carry short secrets in nested fields. Redact those
// values before applying the normal diagnostic redactor to individual strings.
fn redact_value(value: &mut serde_json::Value, credential: &str) {
    match value {
        serde_json::Value::Object(fields) => {
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
                    *value = serde_json::Value::String("[redacted]".to_owned());
                } else {
                    redact_value(value, credential);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                redact_value(value, credential);
            }
        }
        serde_json::Value::String(text) => *text = redact(text, credential),
        _ => {}
    }
}

fn event(result: &CallResult, credential: &str) -> Result<String> {
    let rendered = result.output.render();
    let mut output = match serde_json::from_str::<serde_json::Value>(&rendered) {
        Ok(mut value) => {
            redact_value(&mut value, credential);
            // Also sanitize keys and scalar values without compromising the
            // outer event's JSON encoding.
            redact(&value.to_string(), credential)
        }
        Err(_) => redact(&rendered, credential),
    };
    let truncated = output.len() > MAX_RESULT_BYTES;
    if truncated {
        let mut end = MAX_RESULT_BYTES;
        while !output.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        output.truncate(end);
    }
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

fn run(
    hook: &PostToolHook,
    result: &CallResult,
    context: &ExecutionContext,
    cancellation: &Cancellation,
    credential: &str,
) -> Result<()> {
    let check = || {
        cancellation.check()?;
        context.check_cancelled()
    };
    check()?;
    if hook.name.trim().is_empty()
        || hook
            .argv
            .first()
            .is_none_or(|program| program.trim().is_empty())
        || !(1..=60_000).contains(&hook.timeout_ms)
    {
        return Err(RuneError::invalid_field(
            "post_tool_hook",
            "name and argv program must be nonempty; timeout_ms must be 1..=60000",
        ));
    }
    let input = event(result, credential)?;
    if input.len() > MAX_INPUT_BYTES {
        return Err(RuneError::new(
            ErrorCode::TooLarge,
            "post-tool hook input exceeds 32768 bytes",
        ));
    }
    let mut environment = minimal_environment();
    environment.insert("RUNE_POST_TOOL_RESULT".to_owned(), input);
    let started = Instant::now();
    let timeout = Duration::from_millis(hook.timeout_ms);
    let process = Process::start_argv(
        &hook.argv,
        Some(context.workspace().as_std_path()),
        MAX_OUTPUT_BYTES,
        &environment,
        Input::Closed,
    )?;
    let exit = loop {
        check()?;
        if started.elapsed() >= timeout {
            return Err(RuneError::new(
                ErrorCode::LimitExceeded,
                "post-tool hook exceeded its deadline",
            ));
        }
        if let Some(exit) = process.exit() {
            break exit;
        }
        std::thread::sleep(POLL_INTERVAL.min(timeout.saturating_sub(started.elapsed())));
    };
    check()?;
    if !exit.is_success() {
        return Err(RuneError::new(
            ErrorCode::PermissionDenied,
            format!("post-tool hook {}", exit.describe()),
        ));
    }
    // stdout/stderr are captured with a fixed bound and never become tool data.
    // Dropping a running Process stops its entire process group on errors.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(output: &str) -> CallResult {
        CallResult {
            call: rune_agent::turn::PreparedCall {
                id: "fixture-call".to_owned(),
                name: "fixture_tool".to_owned(),
                arguments: r#"{"password":"argument-secret"}"#.to_owned(),
            },
            output: rune_tools::ToolOutput::success(output),
            executed: true,
        }
    }

    #[test]
    fn event_redacts_nested_short_secrets_and_bounds_utf8_after_redaction() {
        let result = fixture(
            r#"{"message":"visible","nested":[{"password":"tiny","access_token":"short","answer":"fixture-key"}]}"#,
        );
        let input = event(&result, "fixture-key").expect("event");
        for secret in ["tiny", "short", "fixture-key", "argument-secret"] {
            assert!(!input.contains(secret), "{input}");
        }
        assert!(input.contains("visible"));
        assert_eq!(
            result.output.text,
            r#"{"message":"visible","nested":[{"password":"tiny","access_token":"short","answer":"fixture-key"}]}"#
        );
        let result = fixture(&format!("{}sk-tail-secret", "界".repeat(5000)));
        let input = event(&result, "").expect("event");
        let input: serde_json::Value = serde_json::from_str(&input).expect("JSON");
        assert_eq!(input["output_truncated"], true);
        assert!(input["output"].as_str().expect("text").len() <= MAX_RESULT_BYTES);
    }

    #[cfg(unix)]
    #[test]
    fn timeout_stops_the_hooks_descendants() {
        let directory = tempfile::tempdir().expect("workspace");
        let workspace =
            camino::Utf8PathBuf::from_path_buf(directory.path().to_owned()).expect("UTF-8");
        let context = ExecutionContext::new(workspace);
        let hook = PostToolHook {
            name: "timeout fixture".to_owned(),
            argv: vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "touch started; (sleep 1; touch escaped) & wait".to_owned(),
            ],
            timeout_ms: 150,
        };
        let started = Instant::now();
        let error =
            run(&hook, &fixture("done"), &context, &Cancellation::new(), "").expect_err("timeout");
        assert_eq!(error.code(), ErrorCode::LimitExceeded);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(directory.path().join("started").exists());
        std::thread::sleep(Duration::from_millis(1200));
        assert!(
            !directory.path().join("escaped").exists(),
            "descendant outlived timeout"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_stops_a_running_post_tool_hook() {
        let directory = tempfile::tempdir().expect("workspace");
        let workspace =
            camino::Utf8PathBuf::from_path_buf(directory.path().to_owned()).expect("UTF-8");
        let context = ExecutionContext::new(workspace);
        let cancellation = Cancellation::new();
        let hook = PostToolHook {
            name: "cancellation fixture".to_owned(),
            argv: vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "touch started; sleep 30".to_owned(),
            ],
            timeout_ms: 5000,
        };
        let result = fixture("done");
        let started = Instant::now();
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| run(&hook, &result, &context, &cancellation, ""));
            while !directory.path().join("started").exists() {
                assert!(
                    started.elapsed() < Duration::from_secs(3),
                    "hook did not start"
                );
                std::thread::sleep(POLL_INTERVAL);
            }
            cancellation.cancel();
            let error = worker.join().expect("worker").expect_err("cancelled");
            assert_eq!(error.code(), ErrorCode::Cancelled);
        });
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
