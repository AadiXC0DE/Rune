//! `rune bench`: how well the configured model and harness solve real tasks.
//!
//! A task is a directory holding `task.json` and a `repo/` to work in. Each run
//! copies the repository to a fresh directory, gives `rune ask` the task's
//! prompt with full access, restores the task's protected paths so tests
//! cannot be edited into passing, and passes only when the task's `verify`
//! command exits zero. The format is the one other harnesses' suites use, so
//! the same tasks measure each of them.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use rune_core::error::{ErrorCode, Result, RuneError};
use serde::{Deserialize, Serialize};

/// Agent time allowed for a task that declares none.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// Time the verify command is allowed when a task declares none.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(180);

/// Time the setup command is allowed.
const SETUP_TIMEOUT: Duration = Duration::from_secs(300);

/// Largest number of trials per task.
pub const MAX_TRIALS: u32 = 20;

/// A task as `task.json` describes it.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    /// What the agent is asked to do.
    pub prompt: String,
    /// Command whose zero exit is a pass.
    pub verify: String,
    /// Paths restored from the pristine repository before verifying.
    #[serde(default)]
    pub protected: Vec<String>,
    /// Agent time limit in milliseconds.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Directory inside the task whose contents are copied into the workspace just before
    /// verifying; never visible to the agent.
    #[serde(default)]
    pub tests: Option<String>,
    /// Shell command run in the workspace before the agent starts.
    #[serde(default)]
    pub setup: Option<String>,
    /// Verify time limit in milliseconds.
    #[serde(default)]
    pub verify_timeout_ms: Option<u64>,
    /// Difficulty: 1 one file, 2 several files, 3 long or tricky.
    #[serde(default)]
    pub tier: Option<u8>,
    /// `dev` or `heldout`; absent means `dev`.
    #[serde(default)]
    pub split: Option<String>,
    /// Main language of the task; read by report tooling, not by the runner.
    #[serde(default)]
    #[allow(dead_code)]
    pub language: Option<String>,
    /// Free-form labels; read by report tooling, not by the runner.
    #[serde(default)]
    #[allow(dead_code)]
    pub tags: Vec<String>,
}

/// Which tasks a run admits by split.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitFilter {
    /// Development tasks, including those without a split.
    Dev,
    /// Held-out tasks.
    Heldout,
    /// Every task.
    All,
}

impl SplitFilter {
    /// Reads `dev`, `heldout` or `all`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "dev" => Some(Self::Dev),
            "heldout" => Some(Self::Heldout),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    /// Whether the filter lets `task` through.
    #[must_use]
    pub fn admits(self, task: &Task) -> bool {
        let heldout = task.split.as_deref() == Some("heldout");
        match self {
            Self::Dev => !heldout,
            Self::Heldout => heldout,
            Self::All => true,
        }
    }
}

/// One task found on disk.
#[derive(Clone, Debug)]
pub struct Found {
    /// Directory name, used as the task's identifier.
    pub id: String,
    /// The task directory.
    pub dir: Utf8PathBuf,
    /// Its description.
    pub task: Task,
}

/// Outcome of one trial.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Run {
    /// Task identifier.
    pub task: String,
    /// Trial number, from 1.
    pub trial: u32,
    /// Whether the verify command exited zero.
    pub passed: bool,
    /// Whether the agent changed a protected path before it was restored.
    pub tampered: bool,
    /// Whether the agent ran out of time.
    pub timed_out: bool,
    /// Agent wall time in seconds.
    pub seconds: f64,
    /// Model steps the agent took.
    pub steps: u64,
    /// Input tokens across the run, cached parts included.
    pub input_tokens: u64,
    /// Input tokens read from a prompt cache.
    pub cache_read_tokens: u64,
    /// Output tokens across the run.
    pub output_tokens: u64,
    /// Estimated cost in dollars, when the model's prices are known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// The agent's own verdict on whether it checked its change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification: Option<String>,
    /// Why the agent run failed, when it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Reads every task under `root`, sorted by identifier.
pub fn discover(root: &Utf8Path) -> Result<Vec<Found>> {
    let entries = std::fs::read_dir(root).map_err(|err| {
        RuneError::new(
            ErrorCode::NotFound,
            format!("cannot read the task directory `{root}`: {err}"),
        )
    })?;
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let Ok(dir) = Utf8PathBuf::from_path_buf(entry.path()) else {
            continue;
        };
        let spec = dir.join("task.json");
        if !spec.is_file() || !dir.join("repo").is_dir() {
            continue;
        }
        let text = std::fs::read_to_string(&spec)
            .map_err(|err| RuneError::new(ErrorCode::Internal, format!("`{spec}`: {err}")))?;
        let task: Task = serde_json::from_str(&text).map_err(|err| {
            RuneError::invalid_field("task.json", format!("`{spec}` is not a task: {err}"))
        })?;
        let problem = if task
            .split
            .as_deref()
            .is_some_and(|split| !matches!(split, "dev" | "heldout"))
        {
            Some("split must be dev or heldout".to_owned())
        } else if task.tier.is_some_and(|tier| !(1..=3).contains(&tier)) {
            Some("tier must be 1, 2 or 3".to_owned())
        } else {
            task.tests
                .as_deref()
                .filter(|name| !dir.join(name).is_dir())
                .map(|name| format!("tests directory `{name}` does not exist"))
        };
        if let Some(problem) = problem {
            return Err(RuneError::invalid_field(
                "task.json",
                format!("`{spec}`: {problem}"),
            ));
        }
        let id = dir.file_name().unwrap_or_default().to_owned();
        found.push(Found { id, dir, task });
    }
    found.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(found)
}

/// Runs one trial of a task and reports what happened.
pub fn run_task(
    binary: &Utf8Path,
    overrides: &[String],
    found: &Found,
    trial: u32,
    logs: Option<&Utf8Path>,
) -> Result<Run> {
    let scratch = tempdir()?;
    let workspace = scratch.join("workspace");
    let pristine = found.dir.join("repo");
    copy_tree(&pristine, &workspace)?;

    let mut run = Run {
        task: found.id.clone(),
        trial,
        ..Run::default()
    };
    if let Some(setup) = &found.task.setup {
        let mut command = shell(setup);
        command.current_dir(&workspace);
        let finished = run_with_timeout(&mut command, SETUP_TIMEOUT)?;
        if finished.timed_out || !finished.success {
            run.error = Some(if finished.timed_out {
                "setup timed out".to_owned()
            } else {
                format!("setup failed with exit {}", finished.code.unwrap_or(-1))
            });
            let _ = std::fs::remove_dir_all(&scratch);
            return Ok(run);
        }
    }
    // The starting commit comes after setup, so setup's changes are part of the
    // starting state and never counted as the agent's work.
    initialize_repository(&workspace);
    let timeout = found
        .task
        .timeout_ms
        .map_or(DEFAULT_TIMEOUT, Duration::from_millis);
    let mut command = Command::new(binary);
    command
        .args(overrides)
        .args([
            "--permission-mode",
            "full-access",
            "ask",
            "--json",
            "--no-save",
            "--",
        ])
        .arg(&found.task.prompt)
        .current_dir(&workspace);
    let started = Instant::now();
    let finished = run_with_timeout(&mut command, timeout)?;
    run.seconds = started.elapsed().as_secs_f64();
    run.timed_out = finished.timed_out;
    read_report(&finished.stdout, &mut run);
    if let Some(logs) = logs {
        std::fs::create_dir_all(logs).map_err(|err| io_error(&err))?;
        let stem = format!("{}-t{trial}", found.id);
        std::fs::write(logs.join(format!("{stem}.stderr.txt")), &finished.stderr)
            .map_err(|err| io_error(&err))?;
        std::fs::write(logs.join(format!("{stem}.json")), &finished.stdout)
            .map_err(|err| io_error(&err))?;
    }

    for path in &found.task.protected {
        let original = pristine.join(path);
        let current = workspace.join(path);
        if tree_digest(&original) != tree_digest(&current) {
            run.tampered = true;
        }
        remove_path(&current);
        if original.exists() {
            copy_tree(&original, &current)?;
        }
    }

    if let Some(tests) = &found.task.tests {
        let hidden = found.dir.join(tests);
        if let Ok(entries) = std::fs::read_dir(&hidden) {
            for entry in entries.flatten() {
                if let Ok(path) = Utf8PathBuf::from_path_buf(entry.path()) {
                    let name = path.file_name().unwrap_or_default();
                    copy_tree(&path, &workspace.join(name))?;
                }
            }
        }
    }

    let mut verify = shell(&found.task.verify);
    verify.current_dir(&workspace);
    let verify_timeout = found
        .task
        .verify_timeout_ms
        .map_or(VERIFY_TIMEOUT, Duration::from_millis);
    run.passed = run_with_timeout(&mut verify, verify_timeout)
        .is_ok_and(|verified| !verified.timed_out && verified.success);
    let _ = std::fs::remove_dir_all(&scratch);
    Ok(run)
}

/// Global flags of the parent run that every child run receives.
pub struct Forwarded<'a> {
    /// Model override.
    pub model: Option<&'a str>,
    /// Provider override.
    pub provider: Option<&'a str>,
    /// Reasoning effort override.
    pub effort: Option<&'a str>,
    /// Limit overrides as `name`, `value` pairs.
    pub limits: &'a [(String, String)],
    /// Additional directories.
    pub add_dirs: &'a [String],
    /// Whether commands may run where the host has no sandbox.
    pub allow_unsandboxed: bool,
    /// Whether every outbound request is refused.
    pub offline: bool,
}

/// Builds the child's leading arguments. Relative `--add-dir` paths are made
/// absolute against `cwd`, because children run in their own workspace.
#[must_use]
pub fn overrides(forwarded: &Forwarded<'_>, cwd: &Utf8Path) -> Vec<String> {
    let mut args = Vec::new();
    for (flag, value) in [
        ("--model", forwarded.model),
        ("--provider", forwarded.provider),
        ("--effort", forwarded.effort),
    ] {
        if let Some(value) = value {
            args.push(flag.to_owned());
            args.push(value.to_owned());
        }
    }
    for (name, value) in forwarded.limits {
        args.push("--limit".to_owned());
        args.push(format!("{name}={value}"));
    }
    for dir in forwarded.add_dirs {
        args.push("--add-dir".to_owned());
        args.push(absolute(cwd, dir).into_string());
    }
    if forwarded.allow_unsandboxed {
        args.push("--allow-unsandboxed".to_owned());
    }
    if forwarded.offline {
        args.push("--offline".to_owned());
    }
    args
}

/// Joins `path` onto `cwd` and resolves `.` and `..` without touching the disk.
fn absolute(cwd: &Utf8Path, path: &str) -> Utf8PathBuf {
    use camino::Utf8Component;
    let path = Utf8Path::new(path);
    let mut out = if path.is_absolute() {
        Utf8PathBuf::new()
    } else {
        cwd.to_owned()
    };
    for part in path.components() {
        match part {
            Utf8Component::CurDir => {}
            Utf8Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_str()),
        }
    }
    out
}

/// Runs every (trial, task) pair, `jobs` at a time, and returns the runs
/// sorted by trial and task. `on_run` sees each run as it finishes. The first
/// error stops scheduling and is returned once running workers finish.
pub fn run_all(
    binary: &Utf8Path,
    overrides: &[String],
    tasks: &[Found],
    trials: u32,
    jobs: usize,
    logs: Option<&Utf8Path>,
    on_run: &(dyn Fn(&Run) + Sync),
) -> Result<Vec<Run>> {
    let queue: Mutex<VecDeque<(u32, &Found)>> = Mutex::new(
        (1..=trials)
            .flat_map(|trial| tasks.iter().map(move |task| (trial, task)))
            .collect(),
    );
    let done: Mutex<Vec<Run>> = Mutex::new(Vec::new());
    let failure: Mutex<Option<RuneError>> = Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..jobs.max(1) {
            scope.spawn(|| {
                loop {
                    if failure.lock().is_ok_and(|failure| failure.is_some()) {
                        return;
                    }
                    let Some((trial, task)) =
                        queue.lock().ok().and_then(|mut jobs| jobs.pop_front())
                    else {
                        return;
                    };
                    match run_task(binary, overrides, task, trial, logs) {
                        Ok(run) => {
                            on_run(&run);
                            if let Ok(mut done) = done.lock() {
                                done.push(run);
                            }
                        }
                        Err(err) => {
                            if let Ok(mut failure) = failure.lock() {
                                failure.get_or_insert(err);
                            }
                            return;
                        }
                    }
                }
            });
        }
    });
    if let Some(err) = failure.into_inner().ok().flatten() {
        return Err(err);
    }
    let mut runs = done.into_inner().unwrap_or_default();
    runs.sort_by(|left, right| (left.trial, &left.task).cmp(&(right.trial, &right.task)));
    Ok(runs)
}

/// Copies what `rune ask --json` reported into a run.
fn read_report(stdout: &str, run: &mut Run) {
    let Some(value) = stdout
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
    else {
        run.error = Some(if run.timed_out {
            "the agent ran out of time".to_owned()
        } else {
            "the agent printed no result".to_owned()
        });
        return;
    };
    let count = |value: &serde_json::Value| value.as_u64().unwrap_or_default();
    run.steps = count(&value["steps"]);
    run.input_tokens = count(&value["usage"]["input_tokens"]);
    run.output_tokens = count(&value["usage"]["output_tokens"]);
    run.cache_read_tokens = count(&value["usage"]["cache_read_tokens"]);
    run.cost_usd = value["cost_usd"]
        .as_str()
        .and_then(|cost| cost.parse().ok());
    run.verification = value["verification"].as_str().map(str::to_owned);
    run.error = value["error"].as_str().map(str::to_owned);
}

/// A finished child process.
struct Finished {
    success: bool,
    code: Option<i32>,
    timed_out: bool,
    stdout: String,
    stderr: String,
}

/// Time a child gets to exit after the termination signal before it is killed.
const TERM_GRACE: Duration = Duration::from_secs(5);

/// Reads a pipe to its end on a thread of its own.
fn drain<R: std::io::Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

/// Runs a command in a process group of its own, ending the whole group if it
/// outlives `timeout` and after it exits, so no stray process keeps a pipe open.
fn run_with_timeout(command: &mut Command, timeout: Duration) -> Result<Finished> {
    rune_exec::command::own_group(command);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| RuneError::new(ErrorCode::Internal, format!("could not start: {err}")))?;
    let group = child.id();
    let stdout_reader = drain(child.stdout.take());
    let stderr_reader = drain(child.stderr.take());
    let deadline = Instant::now().checked_add(timeout);
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|err| io_error(&err))? {
            break Some(status);
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            timed_out = true;
            rune_exec::command::signal_group(group, "TERM");
            let grace = Instant::now();
            let mut exited = None;
            while grace.elapsed() < TERM_GRACE {
                exited = child.try_wait().map_err(|err| io_error(&err))?;
                if exited.is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            if exited.is_none() && !rune_exec::command::signal_group(group, "KILL") {
                let _ = child.kill();
            }
            break exited.or_else(|| child.wait().ok());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    rune_exec::command::signal_group(group, "KILL");
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    Ok(Finished {
        success: status.is_some_and(|status| status.success()),
        code: status.and_then(|status| status.code()),
        timed_out,
        stdout,
        stderr,
    })
}

fn io_error(err: &std::io::Error) -> RuneError {
    RuneError::new(ErrorCode::Internal, err.to_string())
}

/// Builds the platform shell running one command line.
fn shell(line: &str) -> Command {
    let mut command = Command::new(rune_exec::command::default_shell());
    command
        .arg(rune_exec::command::SHELL_COMMAND_FLAG)
        .arg(line);
    command
}

/// Makes the workspace a repository with one commit, so the agent's diff
/// tools see the task's starting point. Skipped quietly where git is absent.
fn initialize_repository(workspace: &Utf8Path) {
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(workspace)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    let _ = git(&["init", "-q"])
        && git(&["add", "-A"])
        && git(&[
            "-c",
            "user.name=rune bench",
            "-c",
            "user.email=bench@localhost",
            "commit",
            "-q",
            "-m",
            "task",
        ]);
}

/// Creates a fresh, private scratch directory.
fn tempdir() -> Result<Utf8PathBuf> {
    let base = Utf8PathBuf::from_path_buf(std::env::temp_dir())
        .map_err(|_| RuneError::new(ErrorCode::Internal, "the temporary directory is not UTF-8"))?;
    let dir = base.join(format!("rune-bench-{}", uuid_like()));
    std::fs::create_dir_all(&dir).map_err(|err| io_error(&err))?;
    Ok(dir)
}

/// Returns a value unique enough for a scratch directory name.
fn uuid_like() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!("{}-{nanos:x}", std::process::id())
}

/// Copies a file or directory tree, skipping symbolic links.
fn copy_tree(from: &Utf8Path, to: &Utf8Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(from).map_err(|err| io_error(&err))?;
    if meta.is_symlink() {
        return Ok(());
    }
    if meta.is_file() {
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(|err| io_error(&err))?;
        }
        std::fs::copy(from, to).map_err(|err| io_error(&err))?;
        return Ok(());
    }
    std::fs::create_dir_all(to).map_err(|err| io_error(&err))?;
    for entry in std::fs::read_dir(from)
        .map_err(|err| io_error(&err))?
        .flatten()
    {
        let Ok(path) = Utf8PathBuf::from_path_buf(entry.path()) else {
            continue;
        };
        let name = path.file_name().unwrap_or_default();
        copy_tree(&path, &to.join(name))?;
    }
    Ok(())
}

fn remove_path(path: &Utf8Path) {
    if path.is_dir() {
        let _ = std::fs::remove_dir_all(path);
    } else {
        let _ = std::fs::remove_file(path);
    }
}

/// Digests a file or a tree by relative path and content.
fn tree_digest(root: &Utf8Path) -> Option<String> {
    use sha2::{Digest as _, Sha256};
    let mut files = BTreeMap::new();
    collect(root, Utf8Path::new(""), &mut files);
    if files.is_empty() && !root.exists() {
        return None;
    }
    let mut hasher = Sha256::new();
    for (path, bytes) in files {
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
        hasher.update(bytes.len().to_le_bytes());
        hasher.update(&bytes);
    }
    Some(format!("{:x}", hasher.finalize()))
}

fn collect(path: &Utf8Path, relative: &Utf8Path, files: &mut BTreeMap<String, Vec<u8>>) {
    if path.is_file() {
        files.insert(
            relative.to_string(),
            std::fs::read(path).unwrap_or_default(),
        );
    } else if path.is_dir()
        && let Ok(entries) = std::fs::read_dir(path)
    {
        for entry in entries.flatten() {
            if let Ok(child) = Utf8PathBuf::from_path_buf(entry.path()) {
                let name = child.file_name().unwrap_or_default().to_owned();
                collect(&child, &relative.join(name), files);
            }
        }
    }
}

/// Totals across every run.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Summary {
    /// Runs that passed.
    pub passed: usize,
    /// Runs made.
    pub runs: usize,
    /// Median agent time in seconds.
    pub median_seconds: f64,
    /// Mean input tokens per run.
    pub input_tokens_per_run: u64,
    /// Share of input tokens read from a cache, in percent.
    pub cache_hit_percent: u64,
    /// Total estimated cost, when every run was priced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Estimated cost per passing run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_per_pass_usd: Option<f64>,
    /// Runs whose agent changed a protected path.
    pub tampered: usize,
}

/// Summarizes a set of runs.
#[must_use]
pub fn summarize(runs: &[Run]) -> Summary {
    let mut seconds: Vec<f64> = runs.iter().map(|run| run.seconds).collect();
    seconds.sort_by(f64::total_cmp);
    let median_seconds = match seconds.len() {
        0 => 0.0,
        count if count % 2 == 1 => seconds[count / 2],
        count => f64::midpoint(seconds[(count / 2).saturating_sub(1)], seconds[count / 2]),
    };
    let input: u64 = runs.iter().map(|run| run.input_tokens).sum();
    let cached: u64 = runs.iter().map(|run| run.cache_read_tokens).sum();
    let passed = runs.iter().filter(|run| run.passed).count();
    let cost: Option<f64> = runs.iter().map(|run| run.cost_usd).sum();
    Summary {
        passed,
        runs: runs.len(),
        median_seconds,
        input_tokens_per_run: input.checked_div(runs.len() as u64).unwrap_or_default(),
        cache_hit_percent: cached
            .saturating_mul(100)
            .checked_div(input)
            .unwrap_or_default(),
        cost_usd: cost,
        cost_per_pass_usd: cost.filter(|_| passed > 0).map(|cost| cost / passed as f64),
        tampered: runs.iter().filter(|run| run.tampered).count(),
    }
}

/// Renders one run as a table row.
#[must_use]
pub fn row(run: &Run) -> String {
    let mut out = format!(
        "{:<24} {:>2}  {:<4}  {:>7.1}s  {:>3} steps  {:>8} in  {:>6} out",
        run.task,
        run.trial,
        if run.passed { "pass" } else { "FAIL" },
        run.seconds,
        run.steps,
        run.input_tokens,
        run.output_tokens
    );
    if let Some(cost) = run.cost_usd {
        let _ = write!(out, "  ${cost:.4}");
    }
    for (flag, label) in [
        (run.tampered, "tampered"),
        (run.timed_out, "timed out"),
        (
            run.verification.as_deref() == Some("unverified"),
            "unverified",
        ),
    ] {
        if flag {
            let _ = write!(out, "  [{label}]");
        }
    }
    if let Some(error) = &run.error {
        let _ = write!(out, "  {error}");
    }
    out
}

/// Renders the summary.
#[must_use]
pub fn render_summary(summary: &Summary) -> String {
    let mut out = format!(
        "passed {}/{} · median {:.1}s · {} input tokens per run · {}% cached",
        summary.passed,
        summary.runs,
        summary.median_seconds,
        summary.input_tokens_per_run,
        summary.cache_hit_percent
    );
    if let Some(cost) = summary.cost_usd {
        let _ = write!(out, " · ${cost:.4} total");
    }
    if let Some(cost) = summary.cost_per_pass_usd {
        let _ = write!(out, " · ${cost:.4} per pass");
    }
    if summary.tampered > 0 {
        let _ = write!(out, " · {} tampered", summary.tampered);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(passed: bool, seconds: f64, input: u64, cached: u64, cost: Option<f64>) -> Run {
        Run {
            task: "t".to_owned(),
            trial: 1,
            passed,
            seconds,
            input_tokens: input,
            cache_read_tokens: cached,
            cost_usd: cost,
            ..Run::default()
        }
    }

    #[test]
    fn a_summary_reports_pass_rate_median_tokens_and_cost_per_pass() {
        let summary = summarize(&[
            run(true, 9.0, 30_000, 24_000, Some(0.0015)),
            run(true, 12.0, 20_000, 10_000, Some(0.0010)),
            run(false, 40.0, 10_000, 0, Some(0.0005)),
        ]);
        assert_eq!((summary.passed, summary.runs), (2, 3));
        assert!((summary.median_seconds - 12.0).abs() < f64::EPSILON);
        assert_eq!(summary.input_tokens_per_run, 20_000);
        assert_eq!(summary.cache_hit_percent, 56);
        let cost = summary.cost_usd.expect("cost");
        assert!((cost - 0.003).abs() < 1e-9, "{cost}");
        let per_pass = summary.cost_per_pass_usd.expect("per pass");
        assert!((per_pass - 0.0015).abs() < 1e-9, "{per_pass}");
    }

    #[test]
    fn an_unpriced_run_leaves_the_total_unknown() {
        let summary = summarize(&[run(true, 1.0, 1, 0, Some(0.1)), run(true, 1.0, 1, 0, None)]);
        assert_eq!(summary.cost_usd, None);
    }

    #[test]
    fn a_report_line_fills_a_run() {
        let mut parsed = Run::default();
        read_report(
            "noise\n{\"steps\":4,\"usage\":{\"input_tokens\":900,\"output_tokens\":40,\"cache_read_tokens\":600},\"cost_usd\":\"0.000312\",\"verification\":\"verified\"}\n",
            &mut parsed,
        );
        assert_eq!(parsed.steps, 4);
        assert_eq!(parsed.input_tokens, 900);
        assert_eq!(parsed.cache_read_tokens, 600);
        assert_eq!(parsed.cost_usd, Some(0.000_312));
        assert_eq!(parsed.verification.as_deref(), Some("verified"));
        assert!(parsed.error.is_none());
    }

    #[test]
    fn tasks_are_discovered_and_a_changed_protected_tree_is_detected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_owned()).expect("utf8");
        let task = root.join("b-task");
        std::fs::create_dir_all(task.join("repo/test")).expect("repo");
        std::fs::write(task.join("repo/test/a.js"), "assert(1)").expect("test file");
        std::fs::write(
            task.join("task.json"),
            r#"{"title":"T","prompt":"fix","verify":"true","protected":["test"],"timeoutMs":1000}"#,
        )
        .expect("task");
        std::fs::create_dir_all(root.join("not-a-task")).expect("other dir");
        let found = discover(&root).expect("discovered");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "b-task");
        assert_eq!(found[0].task.timeout_ms, Some(1000));

        let copy = root.join("copy");
        copy_tree(&task.join("repo"), &copy).expect("copied");
        let original = task.join("repo/test");
        assert_eq!(tree_digest(&original), tree_digest(&copy.join("test")));
        std::fs::write(copy.join("test/a.js"), "assert(true)").expect("tamper");
        assert_ne!(tree_digest(&original), tree_digest(&copy.join("test")));
    }

    fn write_task(root: &Utf8Path, id: &str, json: &str) -> Utf8PathBuf {
        let task = root.join(id);
        std::fs::create_dir_all(task.join("repo")).expect("repo");
        std::fs::write(task.join("task.json"), json).expect("task");
        task
    }

    fn temp_root() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_owned()).expect("utf8");
        (dir, root)
    }

    #[test]
    fn a_v1_task_loads_with_v2_defaults() {
        let (_dir, root) = temp_root();
        write_task(
            &root,
            "old",
            r#"{"title":"T","kind":"fix","prompt":"fix","verify":"true","protected":["test"],"timeoutMs":1000}"#,
        );
        let task = &discover(&root).expect("discovered")[0].task;
        assert_eq!(task.timeout_ms, Some(1000));
        assert_eq!(task.protected, ["test"]);
        assert!(task.tests.is_none() && task.setup.is_none());
        assert!(task.verify_timeout_ms.is_none() && task.tier.is_none());
        assert!(task.split.is_none() && task.language.is_none() && task.tags.is_empty());
    }

    #[test]
    fn v2_fields_are_read_and_bad_values_are_refused() {
        let (_dir, root) = temp_root();
        let task = write_task(
            &root,
            "new",
            r#"{"prompt":"p","verify":"true","tests":"tests","setup":"true","verifyTimeoutMs":500,"tier":2,"split":"heldout","language":"go","tags":["a","b"]}"#,
        );
        std::fs::create_dir_all(task.join("tests")).expect("tests");
        let found = discover(&root).expect("discovered");
        let read = &found[0].task;
        assert_eq!(read.tests.as_deref(), Some("tests"));
        assert_eq!(read.setup.as_deref(), Some("true"));
        assert_eq!((read.verify_timeout_ms, read.tier), (Some(500), Some(2)));
        assert_eq!(read.split.as_deref(), Some("heldout"));
        assert_eq!(read.language.as_deref(), Some("go"));
        assert_eq!(read.tags, ["a", "b"]);

        for (json, problem) in [
            (
                r#"{"prompt":"p","verify":"true","split":"test"}"#,
                "split must be dev or heldout",
            ),
            (
                r#"{"prompt":"p","verify":"true","tier":4}"#,
                "tier must be 1, 2 or 3",
            ),
            (
                r#"{"prompt":"p","verify":"true","tests":"missing"}"#,
                "tests directory `missing` does not exist",
            ),
        ] {
            let (_dir, root) = temp_root();
            write_task(&root, "bad", json);
            let err = discover(&root).expect_err("refused").to_string();
            assert!(err.contains(problem), "{err}");
        }
    }

    #[test]
    fn the_split_filter_treats_an_absent_split_as_dev() {
        let task = |split: Option<&str>| Task {
            prompt: String::new(),
            verify: String::new(),
            protected: Vec::new(),
            timeout_ms: None,
            tests: None,
            setup: None,
            verify_timeout_ms: None,
            tier: None,
            split: split.map(str::to_owned),
            language: None,
            tags: Vec::new(),
        };
        let (absent, dev, held) = (task(None), task(Some("dev")), task(Some("heldout")));
        assert_eq!(SplitFilter::parse("heldout"), Some(SplitFilter::Heldout));
        assert_eq!(SplitFilter::parse("other"), None);
        assert!(SplitFilter::Dev.admits(&absent) && SplitFilter::Dev.admits(&dev));
        assert!(!SplitFilter::Dev.admits(&held));
        assert!(SplitFilter::Heldout.admits(&held) && !SplitFilter::Heldout.admits(&absent));
        assert!(SplitFilter::All.admits(&absent) && SplitFilter::All.admits(&held));
    }

    #[cfg(unix)]
    fn fake_agent(root: &Utf8Path, body: &str) -> Utf8PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = root.join("agent.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("agent");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("mode");
        path
    }

    #[cfg(unix)]
    #[test]
    fn hidden_tests_reach_verify_but_not_the_agent() {
        let (_dir, root) = temp_root();
        let task = write_task(
            &root,
            "hidden",
            r#"{"prompt":"p","verify":"sh check.sh","tests":"tests"}"#,
        );
        std::fs::create_dir_all(task.join("tests")).expect("tests");
        std::fs::write(task.join("tests/check.sh"), "exit 0").expect("check");
        let agent = fake_agent(
            &root,
            r#"printf '{"steps":1,"error":"%s"}\n' "$(ls -A | tr '\n' ' ')""#,
        );
        let found = discover(&root).expect("discovered");
        let run = run_task(&agent, &[], &found[0], 1, None).expect("run");
        assert!(run.passed, "{run:?}");
        let seen = run.error.unwrap_or_default();
        assert!(!seen.contains("check.sh"), "{seen}");
    }

    #[cfg(unix)]
    #[test]
    fn setup_runs_before_the_agent_and_its_failure_stops_the_run() {
        let (_dir, root) = temp_root();
        write_task(
            &root,
            "a-ok",
            r#"{"prompt":"p","verify":"test \"$(cat value.txt)\" = 2","setup":"printf 2 > value.txt"}"#,
        );
        write_task(
            &root,
            "b-bad",
            r#"{"prompt":"p","verify":"true","setup":"exit 3"}"#,
        );
        let agent = fake_agent(&root, r#"echo '{"steps":1}'"#);
        let found = discover(&root).expect("discovered");
        let ok = run_task(&agent, &[], &found[0], 1, None).expect("run");
        assert!(ok.passed, "{ok:?}");
        let bad = run_task(&agent, &[], &found[1], 1, None).expect("run");
        assert!(!bad.passed);
        assert_eq!(bad.error.as_deref(), Some("setup failed with exit 3"));
        assert_eq!(bad.steps, 0);
    }

    #[cfg(unix)]
    #[test]
    fn verify_timeout_ms_bounds_the_verify_command() {
        let (_dir, root) = temp_root();
        write_task(
            &root,
            "slow",
            r#"{"prompt":"p","verify":"sleep 5","verifyTimeoutMs":200}"#,
        );
        let agent = fake_agent(&root, r#"echo '{"steps":1}'"#);
        let found = discover(&root).expect("discovered");
        let started = Instant::now();
        let run = run_task(&agent, &[], &found[0], 1, None).expect("run");
        assert!(!run.passed);
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[cfg(unix)]
    #[test]
    fn a_verify_timeout_ends_a_forked_child() {
        let (_dir, root) = temp_root();
        write_task(
            &root,
            "slow",
            r#"{"prompt":"p","verify":"sleep 5; true","verifyTimeoutMs":200}"#,
        );
        let agent = fake_agent(&root, r#"echo '{"steps":1}'"#);
        let found = discover(&root).expect("discovered");
        let started = Instant::now();
        let run = run_task(&agent, &[], &found[0], 1, None).expect("run");
        assert!(!run.passed);
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn overrides_forward_the_global_flags_in_order() {
        let limits = [("steps".to_owned(), "9".to_owned())];
        let dirs = ["../shared".to_owned(), "/abs/dir".to_owned()];
        // A rooted path is rebuilt from its components, so it comes back with
        // the platform's separator.
        let rooted = if cfg!(windows) {
            "\\abs\\dir"
        } else {
            "/abs/dir"
        };
        let args = overrides(
            &Forwarded {
                model: Some("m"),
                provider: Some("p"),
                effort: Some("high"),
                limits: &limits,
                add_dirs: &dirs,
                allow_unsandboxed: true,
                offline: true,
            },
            Utf8Path::new("/work"),
        );
        assert_eq!(
            args,
            [
                "--model",
                "m",
                "--provider",
                "p",
                "--effort",
                "high",
                "--limit",
                "steps=9",
                "--add-dir",
                "/shared",
                "--add-dir",
                rooted,
                "--allow-unsandboxed",
                "--offline",
            ]
        );
        let none = overrides(
            &Forwarded {
                model: None,
                provider: None,
                effort: None,
                limits: &[],
                add_dirs: &[],
                allow_unsandboxed: false,
                offline: false,
            },
            Utf8Path::new("/work"),
        );
        assert!(none.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_timeout_ends_the_agents_process_group() {
        let (_dir, root) = temp_root();
        write_task(
            &root,
            "hang",
            // Long enough for a loaded machine to start the shell and write the
            // pid before the timeout fires.
            r#"{"prompt":"p","verify":"true","timeoutMs":5000}"#,
        );
        let pidfile = root.join("child.pid");
        let agent = fake_agent(&root, &format!("sleep 30 & echo $! > {pidfile}; wait"));
        let found = discover(&root).expect("discovered");
        let run = run_task(&agent, &[], &found[0], 1, None).expect("run");
        assert!(run.timed_out);
        let pid = std::fs::read_to_string(&pidfile)
            .expect("pid file")
            .trim()
            .to_owned();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let alive = Command::new("kill")
                .args(["-0", &pid])
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if !alive {
                break;
            }
            assert!(Instant::now() < deadline, "process {pid} is still running");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[cfg(unix)]
    #[test]
    fn logs_keep_each_runs_stderr_and_report() {
        let (_dir, root) = temp_root();
        write_task(&root, "noisy", r#"{"prompt":"p","verify":"true"}"#);
        let agent = fake_agent(&root, "echo 'agent diagnostics' >&2\necho '{\"steps\":2}'");
        let found = discover(&root).expect("discovered");
        let logs = root.join("logs/nested");
        run_task(&agent, &[], &found[0], 2, Some(&logs)).expect("run");
        let stderr = std::fs::read_to_string(logs.join("noisy-t2.stderr.txt")).expect("stderr");
        assert_eq!(stderr.trim(), "agent diagnostics");
        let report = std::fs::read_to_string(logs.join("noisy-t2.json")).expect("report");
        assert_eq!(report.trim(), r#"{"steps":2}"#);
    }

    #[cfg(unix)]
    #[test]
    fn jobs_run_tasks_at_the_same_time() {
        let (_dir, root) = temp_root();
        write_task(&root, "b", r#"{"prompt":"p","verify":"true"}"#);
        write_task(&root, "a", r#"{"prompt":"p","verify":"true"}"#);
        // Each run marks that it started, then waits for the other to start.
        // Run one after the other, the first would give up and report no steps.
        let agent = fake_agent(
            &root,
            &format!(
                "touch {root}/started.$$\n\
                 i=0\n\
                 while [ $i -lt 200 ]; do\n\
                 set -- {root}/started.*\n\
                 if [ $# -ge 2 ]; then echo '{{\"steps\":1}}'; exit 0; fi\n\
                 sleep 0.05; i=$((i+1))\n\
                 done\n\
                 echo '{{\"steps\":0}}'"
            ),
        );
        let found = discover(&root).expect("discovered");
        let runs = run_all(&agent, &[], &found, 1, 2, None, &|_| {}).expect("runs");
        let ids: Vec<&str> = runs.iter().map(|run| run.task.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
        assert!(
            runs.iter().all(|run| run.steps == 1),
            "the runs did not overlap"
        );
    }

    #[test]
    fn a_failed_report_keeps_its_usage() {
        let mut parsed = Run::default();
        read_report(
            r#"{"steps":3,"usage":{"input_tokens":900,"output_tokens":40},"error":"boom"}"#,
            &mut parsed,
        );
        assert_eq!(parsed.steps, 3);
        assert_eq!((parsed.input_tokens, parsed.output_tokens), (900, 40));
        assert_eq!(parsed.error.as_deref(), Some("boom"));
    }
}
