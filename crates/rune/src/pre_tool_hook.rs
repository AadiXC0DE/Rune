//! User-authorized, supervised pre-tool veto commands.

use std::time::{Duration, Instant};

use rune_agent::steering::Cancellation;
use rune_agent::turn::{HookVeto, PreparedCall};
use rune_core::config::PreToolHook;
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_exec::command::minimal_environment;
use rune_exec::session::{Input, POLL_INTERVAL, Process};
use rune_tools::ExecutionContext;
use serde::Deserialize;

const MAX_INPUT_BYTES: usize = 32 * 1024;
const MAX_OUTPUT_BYTES: usize = 4 * 1024;

#[derive(Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
enum Decision {
    Allow,
    Veto { reason: String },
}

pub fn run(
    hook: Option<&PreToolHook>,
    call: &PreparedCall,
    context: &ExecutionContext,
    cancellation: &Cancellation,
) -> Result<Option<HookVeto>> {
    let Some(hook) = hook else {
        return Ok(None);
    };
    // Hook failures refuse the tool, retaining the configured hook's name.
    match decide(hook, call, context, cancellation) {
        Ok(Decision::Allow) => Ok(None),
        Ok(Decision::Veto { reason }) if !reason.trim().is_empty() => Ok(Some(HookVeto {
            name: hook.name.clone(),
            reason,
        })),
        Ok(Decision::Veto { .. }) => Ok(Some(HookVeto {
            name: hook.name.clone(),
            reason: "hook returned an empty veto reason".to_owned(),
        })),
        Err(error) if error.code() == ErrorCode::Cancelled => Err(error),
        Err(error) => Ok(Some(HookVeto {
            name: hook.name.clone(),
            reason: format!("hook failed: {error}"),
        })),
    }
}

fn decide(
    hook: &PreToolHook,
    call: &PreparedCall,
    context: &ExecutionContext,
    cancellation: &Cancellation,
) -> Result<Decision> {
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
            "pre_tool_hook",
            "name and argv program must be nonempty; timeout_ms must be 1..=60000",
        ));
    }
    let input = serde_json::to_string(&serde_json::json!({
        "event": "pre_tool",
        "id": call.id,
        "name": call.name,
        "arguments": serde_json::from_str::<serde_json::Value>(&call.arguments)?,
    }))?;
    if input.len() > MAX_INPUT_BYTES {
        return Err(RuneError::new(
            ErrorCode::TooLarge,
            "pre-tool hook input exceeds 32768 bytes",
        ));
    }
    let mut environment = minimal_environment();
    environment.insert("RUNE_PRE_TOOL_CALL".to_owned(), input);
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
                "pre-tool hook exceeded its deadline",
            ));
        }
        if let Some(exit) = process.exit() {
            break exit;
        }
        std::thread::sleep(POLL_INTERVAL);
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
            format!("pre-tool hook {}", exit.describe()),
        ));
    }
    let stdout = process.stdout().since(0);
    if stdout.produced > MAX_OUTPUT_BYTES as u64 {
        return Err(RuneError::new(
            ErrorCode::TooLarge,
            "pre-tool hook output exceeds 4096 bytes",
        ));
    }
    Ok(serde_json::from_slice(&stdout.bytes)?)
    // Dropping a running Process stops its process group on errors.
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn cancellation_stops_a_running_hook() {
        let directory = tempfile::tempdir().expect("workspace");
        let workspace =
            camino::Utf8PathBuf::from_path_buf(directory.path().to_owned()).expect("UTF-8");
        let context = ExecutionContext::new(workspace);
        let cancellation = Cancellation::new();
        let hook = PreToolHook {
            name: "cancellation fixture".to_owned(),
            argv: vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "touch started; sleep 30".to_owned(),
            ],
            timeout_ms: 5_000,
        };
        let call = PreparedCall {
            id: "call-fixture".to_owned(),
            name: "read_file".to_owned(),
            arguments: "{}".to_owned(),
        };
        let started = Instant::now();
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| run(Some(&hook), &call, &context, &cancellation));
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
