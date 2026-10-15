//! Bounded, read-only inspection of staged or unstaged workspace changes.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use rune_core::error::{ErrorCode, Result, RuneError};
use rune_exec::command::{Exit, minimal_environment};
use rune_exec::session::{Input, POLL_INTERVAL, Process, Reading};

use crate::contract::{Activity, ExecutionContext, Tool, ToolOutput, validate_closed_arguments};
use crate::workspace::{FileLimits, bool_arg, resolve, truncate_to_bytes, usize_arg};

const TRUNCATION: &str = "\n[diff truncated at byte cap]\n";
const CONFIG_BYTES: usize = 16 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);

/// Inspects tracked changes under the workspace without staging or editing them.
#[derive(Clone, Copy, Debug, Default)]
pub struct GitDiff {
    limits: FileLimits,
}

impl GitDiff {
    /// Builds an inspector with host-supplied output limits.
    #[must_use]
    pub const fn with_limits(limits: FileLimits) -> Self {
        Self { limits }
    }
}

impl Tool for GitDiff {
    fn name(&self) -> &'static str {
        "git_diff"
    }

    fn description(&self) -> &'static str {
        "Inspect tracked changes under the workspace. Defaults to unstaged changes; set \
         staged to true for the index versus HEAD (also works before the first commit). \
         Untracked files and submodules are excluded. max_bytes lowers the output cap, \
         including the view label and truncation marker. Runs read-only Git inspection \
         with external diff, text conversion, and clean filters disabled. Partial-clone \
         configurations are refused to prevent implicit fetches."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "staged": {
                    "type": "boolean",
                    "description": "Inspect staged changes instead of unstaged changes. Defaults to false."
                },
                "max_bytes": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Total output bytes, capped by host limits. Must fit the label and truncation marker."
                }
            },
            "required": [],
            "additionalProperties": false
        })
    }

    fn activity(&self) -> Activity {
        Activity::Read
    }

    fn call(
        &self,
        arguments: &serde_json::Value,
        context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        context.check_cancelled()?;
        validate_closed_arguments(arguments, &self.input_schema())?;
        let staged = bool_arg(arguments, "staged")?.unwrap_or(false);
        let cap = usize_arg(arguments, "max_bytes")?
            .unwrap_or(usize::MAX)
            .min(self.limits.output_cap(context));
        let label = if staged {
            "Staged diff:\n"
        } else {
            "Unstaged diff:\n"
        };
        let minimum = label.len().saturating_add(TRUNCATION.len());
        if cap < minimum {
            return Err(RuneError::new(
                ErrorCode::LimitExceeded,
                format!("git_diff needs at least {minimum} output bytes for its labels"),
            )
            .with_hint("increase max_bytes and the host output budget"));
        }
        let workspace = resolve(context, ".")?.path;
        let started = Instant::now();
        let environment = environment();

        // A working-tree diff can invoke clean/process filters even with
        // --no-textconv. Discover their names with a bounded config read and
        // override them for this command only, leaving repository config intact.
        // Older Git versions ignore GIT_NO_LAZY_FETCH, so also refuse promisor
        // configuration rather than allowing a diff to spawn a writable fetch.
        let mut argv = base_argv();
        let mut config_argv = argv.clone();
        config_argv.extend([
            "config".to_owned(),
            "--null".to_owned(),
            "--name-only".to_owned(),
            "--get-regexp".to_owned(),
            r"^(filter\..*\.(clean|process|required)|remote\..*\.(promisor|partialclonefilter)|extensions\.partialclone)$".to_owned(),
        ]);
        let config = inspect(
            &config_argv,
            &workspace,
            CONFIG_BYTES,
            &environment,
            context,
            started,
        )?;
        if config.exit != Exit::Code(1) && !config.exit.is_success() {
            return Ok(render(label, &config.stderr, cap, true));
        }
        if config.stdout.produced > CONFIG_BYTES as u64 {
            return Err(RuneError::new(
                ErrorCode::LimitExceeded,
                "Git inspection configuration exceeds the inspection bound",
            ));
        }
        let keys = std::str::from_utf8(&config.stdout.bytes).map_err(|_| {
            RuneError::new(
                ErrorCode::InvalidState,
                "Git configuration names are not UTF-8",
            )
        })?;
        if keys
            .split('\0')
            .any(|key| key.starts_with("remote.") || key == "extensions.partialclone")
        {
            return Err(RuneError::new(
                ErrorCode::InvalidState,
                "git_diff refuses partial-clone configuration to prevent implicit fetches",
            )
            .with_hint("inspect a full clone without promisor or partial-clone configuration"));
        }
        for key in keys.split('\0').filter(|key| !key.is_empty()) {
            argv.push("-c".to_owned());
            argv.push(format!(
                "{key}={}",
                if key.ends_with(".required") {
                    "false"
                } else {
                    ""
                }
            ));
        }
        argv.extend(
            [
                "diff",
                "--patch",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--no-renames",
                "--ignore-submodules=all",
                "--relative",
                "--unified=3",
            ]
            .map(str::to_owned),
        );
        if staged {
            argv.push("--cached".to_owned());
        }
        argv.extend(["--".to_owned(), ".".to_owned()]);
        let diff = inspect(&argv, &workspace, cap, &environment, context, started)?;
        let failed = !diff.exit.is_success();
        Ok(render(
            label,
            if failed { &diff.stderr } else { &diff.stdout },
            cap,
            failed,
        ))
    }
}

fn base_argv() -> Vec<String> {
    [
        "git",
        "--no-pager",
        "-c",
        "core.fsmonitor=false",
        "-c",
        "diff.autoRefreshIndex=false",
    ]
    .map(str::to_owned)
    .to_vec()
}

fn environment() -> BTreeMap<String, String> {
    let mut environment = minimal_environment();
    // Do not inherit Git overrides, credentials, pagers, or helper commands.
    // Suppress optional index writes and network fetches in partial clones.
    for (key, value) in [
        ("GIT_OPTIONAL_LOCKS", "0"),
        ("GIT_NO_LAZY_FETCH", "1"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", null_device()),
        ("GIT_ATTR_NOSYSTEM", "1"),
    ] {
        environment.insert(key.to_owned(), value.to_owned());
    }
    environment
}

const fn null_device() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

struct Inspection {
    stdout: Reading,
    stderr: Reading,
    exit: Exit,
}

fn inspect(
    argv: &[String],
    workspace: &camino::Utf8Path,
    cap: usize,
    environment: &BTreeMap<String, String>,
    context: &ExecutionContext,
    started: Instant,
) -> Result<Inspection> {
    context.check_cancelled()?;
    let process = Process::start_argv(
        argv,
        Some(workspace.as_std_path()),
        cap,
        environment,
        Input::Closed,
    )
    .map_err(|err| {
        RuneError::new(
            ErrorCode::TransportFailure,
            format!("Git inspection could not start: {err}"),
        )
        .with_hint("install Git and check the workspace directory")
    })?;
    let exit = loop {
        context.check_cancelled()?;
        if let Some(exit) = process.exit() {
            break exit;
        }
        if started.elapsed() >= TIMEOUT {
            return Err(RuneError::new(
                ErrorCode::LimitExceeded,
                "Git inspection exceeded 30 seconds",
            ));
        }
        std::thread::sleep(POLL_INTERVAL);
    };
    process.drain_output(Duration::from_millis(250));
    context.check_cancelled()?;
    Ok(Inspection {
        stdout: process.stdout().since(0),
        stderr: process.stderr().since(0),
        exit,
    })
}

fn render(label: &str, reading: &Reading, cap: usize, failed: bool) -> ToolOutput {
    let body = String::from_utf8_lossy(&reading.bytes);
    let body = if body.is_empty() {
        if failed {
            "Git inspection failed.\n"
        } else {
            "No changes.\n"
        }
    } else {
        body.as_ref()
    };
    let truncated = reading.produced > reading.bytes.len() as u64
        || label.len().saturating_add(body.len()) > cap;
    let room = cap
        .saturating_sub(label.len())
        .saturating_sub(if truncated { TRUNCATION.len() } else { 0 });
    let mut text = String::from(label);
    text.push_str(truncate_to_bytes(body, room));
    if truncated {
        text.push_str(TRUNCATION);
    }
    ToolOutput {
        produced_bytes: reading
            .produced
            .saturating_add(label.len() as u64)
            .max(text.len() as u64),
        text,
        is_error: failed,
        plan_update: None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};

    use camino::Utf8Path;
    use serde_json::json;

    use super::*;

    fn git(root: &Utf8Path, arguments: &[&str]) -> Output {
        let output = Command::new("git")
            .args(arguments)
            .current_dir(root)
            .env_clear()
            .envs(environment())
            .output()
            .expect("fixture Git");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn fixture() -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("fixture directory");
        let root = Utf8Path::from_path(directory.path()).expect("UTF-8 root");
        git(root, &["init", "-q"]);
        std::fs::create_dir(root.join("nested")).expect("nested directory");
        // Build an initial index with plumbing in this disposable fixture.
        // No add, commit, push, or stash is needed, even during test setup.
        for file in ["text.txt", "nested/inside.txt"] {
            std::fs::write(root.join(file), "staged content\n").expect("staged fixture");
            let blob = git(root, &["hash-object", "-w", file]);
            let blob = std::str::from_utf8(&blob.stdout).expect("blob id").trim();
            git(
                root,
                &["update-index", "--add", "--cacheinfo", "100644", blob, file],
            );
            std::fs::write(root.join(file), "unstaged content\n").expect("unstaged fixture");
        }
        std::fs::write(root.join("untracked.txt"), "untracked secret\n").expect("untracked");
        directory
    }

    fn snapshot(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        fn visit(root: &Path, directory: &Path, files: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
            for entry in std::fs::read_dir(directory).expect("entries") {
                let path = entry.expect("entry").path();
                let directory = path.is_dir();
                files.insert(
                    path.strip_prefix(root).expect("relative").to_owned(),
                    if directory {
                        None
                    } else {
                        Some(std::fs::read(&path).expect("bytes"))
                    },
                );
                if directory {
                    visit(root, &path, files);
                }
            }
        }
        let mut files = BTreeMap::new();
        visit(root, root, &mut files);
        files
    }

    #[test]
    fn registered_tool_reads_both_views_without_mutating_the_repository() {
        let directory = fixture();
        let root = Utf8Path::from_path(directory.path()).expect("root");
        let context = ExecutionContext::new(root.to_owned());
        let registry = crate::inventory::builtin_default().expect("registry");
        let before = snapshot(directory.path());
        assert_eq!(registry.activity("git_diff"), Some(Activity::Read));
        assert!(registry.get("git_diff").expect("registered").is_read_only());
        for staged in [false, true] {
            let result = registry
                .call("git_diff", &json!({"staged": staged}), &context)
                .expect("diff");
            assert!(!result.is_error, "{}", result.text);
            if staged {
                assert!(result.text.starts_with("Staged diff:\n"));
                assert!(result.text.contains("+staged content"));
                assert!(!result.text.contains("unstaged content"));
            } else {
                assert!(result.text.starts_with("Unstaged diff:\n"));
                assert!(result.text.contains("-staged content\n+unstaged content"));
            }
            assert!(!result.text.contains("untracked.txt"));
            assert!(!result.text.contains("truncated"));
            assert_eq!(snapshot(directory.path()), before);
        }
        let default = registry
            .call("git_diff", &json!({}), &context)
            .expect("default view");
        assert!(default.text.starts_with("Unstaged diff:\n"));
    }

    #[test]
    fn both_views_obey_requested_host_and_context_caps_with_labelled_truncation() {
        let directory = fixture();
        let root = Utf8Path::from_path(directory.path()).expect("root");
        let context = ExecutionContext::new(root.to_owned());
        let before = snapshot(directory.path());
        for staged in [false, true] {
            let arguments = json!({"staged": staged, "max_bytes": 128});
            let requested = GitDiff::default()
                .call(&arguments, &context)
                .expect("requested cap");
            let host = GitDiff::with_limits(FileLimits {
                output_bytes: 128,
                ..FileLimits::default()
            })
            .call(&json!({"staged": staged, "max_bytes": 4096}), &context)
            .expect("host cap");
            let per_call = GitDiff::default()
                .call(
                    &json!({"staged": staged, "max_bytes": 4096}),
                    &context.fork().with_output_cap(128),
                )
                .expect("context cap");
            for output in [&requested, &host, &per_call] {
                assert!(!output.is_error);
                assert!(output.text.len() <= 128, "{}", output.text.len());
                assert!(output.text.starts_with(if staged {
                    "Staged diff:\n"
                } else {
                    "Unstaged diff:\n"
                }));
                assert!(output.text.ends_with(TRUNCATION));
                assert!(output.produced_bytes > output.text.len() as u64);
            }
            assert_eq!(requested, host);
            assert_eq!(requested, per_call);
        }
        assert_eq!(snapshot(directory.path()), before);
    }

    #[test]
    fn workspace_subdirectory_excludes_changes_outside_it() {
        let directory = fixture();
        let root = Utf8Path::from_path(directory.path()).expect("root");
        let context = ExecutionContext::new(root.join("nested"));
        for staged in [false, true] {
            let output = GitDiff::default()
                .call(&json!({"staged": staged}), &context)
                .expect("diff");
            assert!(!output.is_error, "{}", output.text);
            assert!(output.text.contains("inside.txt"));
            assert!(!output.text.contains("text.txt"));
        }
    }

    #[test]
    fn configured_helpers_are_disabled_and_configuration_is_unchanged() {
        let directory = fixture();
        let root = Utf8Path::from_path(directory.path()).expect("root");
        let config = root.join(".git/config");
        let mut contents = std::fs::read_to_string(&config).expect("config");
        contents.push_str(
            "\n[diff]\n\texternal = sh -c 'echo external > helper-ran'\n\
             [diff \"probe\"]\n\tcommand = sh -c 'echo diff > helper-ran'\n\
             \ttextconv = sh -c 'echo textconv > helper-ran'\n\
             [filter \"probe\"]\n\tclean = sh -c 'echo clean > helper-ran; cat'\n\
             \tprocess = sh -c 'echo process > helper-ran'\n\trequired = true\n\
             [core]\n\tfsmonitor = sh -c 'echo fsmonitor > helper-ran'\n",
        );
        std::fs::write(config, contents).expect("helper configuration");
        std::fs::write(
            root.join(".gitattributes"),
            "*.txt diff=probe filter=probe\n",
        )
        .expect("attributes");
        let before = snapshot(directory.path());
        let context = ExecutionContext::new(root.to_owned());
        for staged in [false, true] {
            let output = GitDiff::default()
                .call(&json!({"staged": staged}), &context)
                .expect("diff");
            assert!(!output.is_error, "{}", output.text);
            assert!(output.text.contains(if staged {
                "+staged content"
            } else {
                "+unstaged content"
            }));
            assert!(!root.join("helper-ran").exists());
            assert_eq!(snapshot(directory.path()), before);
        }
    }

    #[test]
    fn no_changes_and_missing_repository_are_reported_distinctly() {
        let directory = fixture();
        let root = Utf8Path::from_path(directory.path()).expect("root");
        for file in ["text.txt", "nested/inside.txt"] {
            std::fs::write(root.join(file), "staged content\n").expect("restore worktree");
        }
        let output = GitDiff::default()
            .call(&json!({}), &ExecutionContext::new(root.to_owned()))
            .expect("clean");
        assert!(!output.is_error);
        assert_eq!(output.text, "Unstaged diff:\nNo changes.\n");
        let outside = tempfile::tempdir().expect("non-repository");
        let context = ExecutionContext::new(
            Utf8Path::from_path(outside.path())
                .expect("root")
                .to_owned(),
        )
        .with_output_cap(128);
        let output = GitDiff::default()
            .call(&json!({}), &context)
            .expect("Git failure");
        assert!(output.is_error);
        assert!(output.text.len() <= 128);
        assert!(output.text.starts_with("Unstaged diff:\n"));
    }

    #[test]
    fn malformed_arguments_tiny_caps_and_cancellation_are_refused() {
        let context = ExecutionContext::new("/unused".into());
        for arguments in [
            json!({"staged": "yes"}),
            json!({"max_bytes": -1}),
            json!({"command": "reset"}),
        ] {
            let error = GitDiff::default()
                .call(&arguments, &context)
                .expect_err("invalid arguments");
            assert_eq!(error.code(), ErrorCode::InvalidField);
        }
        for cap in [0, 1, 40] {
            let error = GitDiff::default()
                .call(&json!({"max_bytes": cap}), &context)
                .expect_err("tiny cap");
            assert_eq!(error.code(), ErrorCode::LimitExceeded);
        }
        context.cancellation().cancel();
        let error = GitDiff::default()
            .call(&json!({}), &context)
            .expect_err("cancelled");
        assert_eq!(error.code(), ErrorCode::Cancelled);
    }

    #[test]
    fn rendering_handles_exact_caps_and_utf8_without_exceeding_the_budget() {
        let label = "Staged diff:\n";
        for bytes in ["é".repeat(80).into_bytes(), vec![0xff; 160]] {
            let reading = Reading {
                produced: bytes.len() as u64,
                next: bytes.len(),
                bytes,
            };
            for cap in label.len() + TRUNCATION.len()..160 {
                let output = render(label, &reading, cap, false);
                assert!(output.text.len() <= cap);
                assert!(output.text.ends_with(TRUNCATION));
            }
        }
        let reading = Reading {
            bytes: b"abc\n".to_vec(),
            next: 4,
            produced: 4,
        };
        let exact = render(label, &reading, label.len() + reading.bytes.len(), false);
        assert_eq!(exact.text, "Staged diff:\nabc\n");
    }

    #[test]
    fn excessive_filter_configuration_is_refused_before_the_diff() {
        let directory = fixture();
        let root = Utf8Path::from_path(directory.path()).expect("root");
        let config = root.join(".git/config");
        let mut contents = std::fs::read_to_string(&config).expect("config");
        for number in 0..1000 {
            use std::fmt::Write as _;
            writeln!(
                contents,
                "[filter \"driver{number}\"]\nclean = forbidden-helper"
            )
            .expect("append config");
        }
        std::fs::write(config, contents).expect("large configuration");
        let before = snapshot(directory.path());
        let error = GitDiff::default()
            .call(&json!({}), &ExecutionContext::new(root.to_owned()))
            .expect_err("config cap");
        assert_eq!(error.code(), ErrorCode::LimitExceeded);
        assert!(error.message().contains("inspection configuration"));
        assert_eq!(snapshot(directory.path()), before);
    }

    #[test]
    fn partial_clone_configuration_is_refused_before_missing_objects_can_be_fetched() {
        let directory = fixture();
        let root = Utf8Path::from_path(directory.path()).expect("root");
        let blob = git(root, &["rev-parse", ":text.txt"]);
        let blob = std::str::from_utf8(&blob.stdout).expect("blob id").trim();
        std::fs::remove_file(root.join(".git/objects").join(&blob[..2]).join(&blob[2..]))
            .expect("remove blob");
        let config = root.join(".git/config");
        let original = std::fs::read_to_string(&config).expect("config");
        for partial in [
            "[remote \"origin\"]\npromisor = true\nurl = https://example.invalid/missing\n",
            "[remote \"origin\"]\npartialclonefilter = blob:none\n",
            "[extensions]\npartialClone = origin\n",
        ] {
            std::fs::write(&config, format!("{original}\n{partial}"))
                .expect("partial clone configuration");
            let before = snapshot(directory.path());
            let error = GitDiff::default()
                .call(&json!({}), &ExecutionContext::new(root.to_owned()))
                .expect_err("partial clone refused");
            assert_eq!(error.code(), ErrorCode::InvalidState);
            assert!(error.message().contains("partial-clone"));
            assert_eq!(snapshot(directory.path()), before);
        }
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_and_timeout_stop_a_running_inspection() {
        let directory = tempfile::tempdir().expect("directory");
        let root = Utf8Path::from_path(directory.path()).expect("root");
        let argv = ["/bin/sh", "-c", "printf started > started; exec sleep 30"].map(str::to_owned);
        let context = ExecutionContext::new(root.to_owned());
        let cancellation = context.cancellation();
        let marker = root.join("started");
        let canceller = std::thread::spawn(move || {
            let start = Instant::now();
            while !marker.exists() && start.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(POLL_INTERVAL);
            }
            let started = marker.exists();
            cancellation.cancel();
            assert!(started, "the inspection started before cancellation");
        });
        let started = Instant::now();
        let error = inspect(&argv, root, 128, &environment(), &context, started)
            .err()
            .expect("cancelled");
        canceller.join().expect("canceller");
        assert_eq!(error.code(), ErrorCode::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(10));

        let expired = Instant::now()
            .checked_sub(TIMEOUT)
            .expect("expired deadline");
        let error = inspect(
            &argv,
            root,
            128,
            &environment(),
            &ExecutionContext::new(root.to_owned()),
            expired,
        )
        .err()
        .expect("timeout");
        assert_eq!(error.code(), ErrorCode::LimitExceeded);
    }
}
