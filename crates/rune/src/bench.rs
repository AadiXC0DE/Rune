//! `rune bench`: how well the configured model and harness solve real tasks.
//!
//! A task is a directory holding `task.json` and a `repo/` to work in. Each run
//! copies the repository to a fresh directory, gives `rune ask` the task's
//! prompt with full access, restores the task's protected paths so tests
//! cannot be edited into passing, and passes only when the task's `verify`
//! command exits zero. The format is the one other harnesses' suites use, so
//! the same tasks measure each of them.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Read as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use rune_core::error::{ErrorCode, Result, RuneError};
use serde::{Deserialize, Serialize};

/// Agent time allowed for a task that declares none.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// Time the verify command is allowed.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(300);

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
        let id = dir.file_name().unwrap_or_default().to_owned();
        found.push(Found { id, dir, task });
    }
    found.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(found)
}

/// Runs one trial of a task and reports what happened.
pub fn run_task(binary: &Utf8Path, overrides: &[String], found: &Found, trial: u32) -> Result<Run> {
    let scratch = tempdir()?;
    let workspace = scratch.join("workspace");
    let pristine = found.dir.join("repo");
    copy_tree(&pristine, &workspace)?;
    initialize_repository(&workspace);

    let mut run = Run {
        task: found.id.clone(),
        trial,
        ..Run::default()
    };
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

    let mut verify = shell(&found.task.verify);
    verify.current_dir(&workspace);
    run.passed = run_with_timeout(&mut verify, VERIFY_TIMEOUT)
        .is_ok_and(|verified| !verified.timed_out && verified.success);
    let _ = std::fs::remove_dir_all(&scratch);
    Ok(run)
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
    timed_out: bool,
    stdout: String,
}

/// Runs a command, killing it if it outlives `timeout`.
fn run_with_timeout(command: &mut Command, timeout: Duration) -> Result<Finished> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| RuneError::new(ErrorCode::Internal, format!("could not start: {err}")))?;
    let mut pipe = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(pipe) = pipe.as_mut() {
            let _ = pipe.read_to_string(&mut text);
        }
        text
    });
    let deadline = Instant::now().checked_add(timeout);
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|err| io_error(&err))? {
            break Some(status);
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            timed_out = true;
            let _ = child.kill();
            break child.wait().ok();
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stdout = reader.join().unwrap_or_default();
    Ok(Finished {
        success: status.is_some_and(|status| status.success()),
        timed_out,
        stdout,
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
}
