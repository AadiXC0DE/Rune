//! Scenario files: a whole `rune ask` run against a scripted provider.
//!
//! A scenario is a JSON file with a prompt, a workspace, the turns the fake
//! model plays and the expectations checked afterwards. Every key in the
//! scenario and in `expect` is optional unless noted, and an unknown key is an
//! error. Request i (1-based) gets turn i, so a scenario that edits a file and
//! then only runs a read such as `grep` earns one extra verification request
//! and fails its `requests` count; run checks as `sh check.sh`.
//!
//! The final JSON result is read from the last stdout line that is JSON. With
//! `--jsonl` that line is the final record and its `result` field is used.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::scripted::{RecordedRequest, ScriptedProvider, Turn};

/// A loaded scenario.
#[derive(Clone, Debug)]
pub struct Scenario {
    /// The file stem.
    pub name: String,
    /// The prompt given to `ask`.
    pub prompt: String,
    /// Relative path to contents, written before the run.
    pub workspace: BTreeMap<String, String>,
    /// Global flags placed after the fixed ones.
    pub global_args: Vec<String>,
    /// Arguments after `ask`.
    pub ask_args: Vec<String>,
    /// Extra environment, added last.
    pub env: BTreeMap<String, String>,
    /// TOML written to the user configuration file before the run.
    pub user_config: Option<String>,
    /// Seconds before the child is killed.
    pub timeout_s: u64,
    /// The scripted replies.
    pub turns: Vec<Turn>,
    /// What must hold afterwards.
    pub expect: Expect,
}

/// Expectations; an absent key is not checked.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    /// Path to exact content, or `null` for "must not exist".
    #[serde(default)]
    pub files: BTreeMap<String, Option<String>>,
    /// The process exit code.
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// `null` means no `error_code` in the result; a string must equal it.
    #[serde(default, deserialize_with = "present")]
    pub error_code: Option<Option<String>>,
    /// The result's `final_output`.
    #[serde(default)]
    pub final_output: Option<String>,
    /// Lower bound on the result's `steps`.
    #[serde(default)]
    pub min_steps: Option<u64>,
    /// Exact POST count.
    #[serde(default)]
    pub requests: Option<usize>,
    /// Upper bound on the POST count.
    #[serde(default)]
    pub max_requests: Option<usize>,
    /// Upper bound on `usage.input_tokens`.
    #[serde(default)]
    pub max_input_tokens: Option<u64>,
    /// Exact prefix break count.
    #[serde(default)]
    pub prefix_breaks: Option<usize>,
    /// Text that a request body must contain.
    #[serde(default)]
    pub request_contains: Vec<RequestContains>,
    /// Text that a request body must not contain.
    #[serde(default)]
    pub request_absent: Vec<RequestContains>,
    /// Each string must appear in at least one recorded request body.
    #[serde(default)]
    pub any_request_contains: Vec<String>,
}

/// One body assertion.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestContains {
    /// The POST number, 1-based.
    pub request: usize,
    /// The text that must appear in its raw body.
    pub text: String,
}

#[allow(clippy::option_option)]
fn present<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScenario {
    prompt: String,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    global_args: Vec<String>,
    #[serde(default)]
    ask_args: Option<Vec<String>>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    user_config: Option<String>,
    #[serde(default)]
    timeout_s: Option<u64>,
    turns: Vec<Turn>,
    #[serde(default)]
    expect: Expect,
}

/// What a run produced.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// The exit code; `None` when the child was killed.
    pub exit_code: Option<i32>,
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
    /// The parsed final result object.
    pub result: Option<serde_json::Value>,
    /// Every POST the provider saw.
    pub requests: Vec<RecordedRequest>,
    /// The provider's prefix break count.
    pub prefix_breaks: usize,
    /// Contents of every path named in `expect.files`.
    pub files: BTreeMap<String, Option<String>>,
    /// The workspace path the child ran in.
    pub workspace_dir: String,
    /// Whether the child was killed at the timeout.
    pub timed_out: bool,
}

/// Loads a scenario file.
pub fn load(path: &Path) -> Result<Scenario, String> {
    let shown = path.display();
    let text = std::fs::read_to_string(path).map_err(|err| format!("{shown}: {err}"))?;
    let raw: RawScenario = serde_json::from_str(&text).map_err(|err| format!("{shown}: {err}"))?;
    let name = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let workspace = match &raw.workspace {
        None => BTreeMap::new(),
        Some(relative) => {
            let file: PathBuf = path.parent().unwrap_or(Path::new(".")).join(relative);
            let text = std::fs::read_to_string(&file)
                .map_err(|err| format!("{}: {err}", file.display()))?;
            serde_json::from_str(&text).map_err(|err| format!("{}: {err}", file.display()))?
        }
    };
    Ok(Scenario {
        name,
        prompt: raw.prompt,
        workspace,
        global_args: raw.global_args,
        ask_args: raw
            .ask_args
            .unwrap_or_else(|| vec!["--json".to_owned(), "--no-save".to_owned()]),
        env: raw.env,
        user_config: raw.user_config,
        timeout_s: raw.timeout_s.unwrap_or(60),
        turns: raw.turns,
        expect: raw.expect,
    })
}

/// Writes `scenario.user_config` to `<root>/config/rune/config.toml`, the file
/// the child reads through `XDG_CONFIG_HOME`.
fn write_user_config(root: &Path, scenario: &Scenario) {
    if let Some(text) = &scenario.user_config {
        let directory = root.join("config").join("rune");
        std::fs::create_dir_all(&directory).expect("a configuration directory");
        // Rune refuses a configuration directory others can read.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            for private in [root.join("config"), directory.clone()] {
                std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700))
                    .expect("a private configuration directory");
            }
        }
        std::fs::write(directory.join("config.toml"), text).expect("a configuration file");
    }
}

/// Runs the scenario against a fresh provider that keeps request bodies.
pub fn run(binary: &Path, scenario: &Scenario) -> Report {
    run_with(
        binary,
        scenario,
        ScriptedProvider::start(scenario.turns.clone()),
    )
}

/// Runs the scenario against a provider the caller built.
pub fn run_with(binary: &Path, scenario: &Scenario, provider: ScriptedProvider) -> Report {
    run_wrapped(binary, &[], scenario, provider)
}

/// Runs the scenario with the binary started by a wrapper command, such as a
/// resource meter: `wrapper[0]`, then `wrapper[1..]`, then the binary path.
pub fn run_wrapped(
    binary: &Path,
    wrapper: &[String],
    scenario: &Scenario,
    provider: ScriptedProvider,
) -> Report {
    let temp = tempfile::TempDir::new().expect("a temporary directory");
    let root = temp.path();
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("a workspace directory");
    let workspace = std::fs::canonicalize(&workspace).expect("a canonical workspace path");
    for (relative, contents) in &scenario.workspace {
        let target = workspace.join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).expect("a workspace subdirectory");
        }
        std::fs::write(&target, contents).expect("a workspace file");
    }
    write_user_config(root, scenario);

    let mut command = match wrapper.split_first() {
        Some((program, rest)) => {
            let mut command = Command::new(program);
            command.args(rest).arg(binary);
            command
        }
        None => Command::new(binary),
    };
    command.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    command
        .env("HOME", root.join("home"))
        .env("RUNE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_STATE_HOME", root.join("xdg-state"))
        .env("RUNE_PROVIDER", "chat_completions")
        .env("RUNE_BASE_URL", provider.base_url())
        .env("RUNE_MODEL", "scripted")
        .env("RUNE_API_KEY_ENV", "RUNE_SCENARIO_KEY")
        .env("RUNE_SCENARIO_KEY", "scenario-key");
    for (key, value) in &scenario.env {
        command.env(key, value);
    }
    command
        .args(["--permission-mode", "full-access", "--allow-unsandboxed"])
        .args(&scenario.global_args)
        .arg("ask")
        .args(&scenario.ask_args)
        .arg("--")
        .arg(&scenario.prompt)
        .current_dir(&workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn().expect("the binary to start");
    let stdout = reader(child.stdout.take());
    let stderr = reader(child.stderr.take());
    let started = Instant::now();
    let limit = Duration::from_secs(scenario.timeout_s);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait().expect("the child to be pollable") {
            Some(status) => break Some(status),
            None if started.elapsed() >= limit => {
                let _ = child.kill();
                let _ = child.wait();
                timed_out = true;
                break None;
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();

    let files = scenario
        .expect
        .files
        .keys()
        .map(|relative| {
            (
                relative.clone(),
                std::fs::read_to_string(workspace.join(relative)).ok(),
            )
        })
        .collect();

    let requests = provider.requests();
    let prefix_breaks = provider.prefix_breaks();
    drop(provider);

    Report {
        exit_code: status.and_then(|status| status.code()),
        result: final_result(&stdout),
        stdout,
        stderr,
        requests,
        prefix_breaks,
        files,
        workspace_dir: workspace.to_string_lossy().into_owned(),
        timed_out,
    }
}

fn reader<R: Read + Send + 'static>(stream: Option<R>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut stream) = stream {
            let _ = stream.read_to_end(&mut bytes);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

fn final_result(stdout: &str) -> Option<serde_json::Value> {
    let value = stdout
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str::<serde_json::Value>(line.trim()).ok())?;
    if value.get("type").and_then(serde_json::Value::as_str) == Some("final") {
        return value.get("result").cloned();
    }
    Some(value)
}

/// Compares a report with the expectations; one line per failure.
#[must_use]
pub fn check(scenario: &Scenario, report: &Report) -> Vec<String> {
    let expect = &scenario.expect;
    let mut failures = Vec::new();
    if report.timed_out {
        failures.push(format!("timed out after {} s", scenario.timeout_s));
    }
    for (path, want) in &expect.files {
        let found = report.files.get(path).cloned().flatten();
        if found != *want {
            failures.push(format!(
                "files: {path}: expected {}, found {}",
                describe(want.as_deref()),
                describe(found.as_deref())
            ));
        }
    }
    if let Some(want) = expect.exit_code
        && report.exit_code != Some(want)
    {
        failures.push(format!(
            "exit_code: expected {want}, found {}",
            report
                .exit_code
                .map_or_else(|| "none".to_owned(), |code| code.to_string())
        ));
    }
    if let Some(want) = &expect.error_code {
        let found = field(report, "error_code").and_then(|value| value.as_str().map(str::to_owned));
        if found != *want {
            failures.push(format!(
                "error_code: expected {}, found {}",
                describe(want.as_deref()),
                describe(found.as_deref())
            ));
        }
    }
    if let Some(want) = &expect.final_output {
        let found =
            field(report, "final_output").and_then(|value| value.as_str().map(str::to_owned));
        if found.as_deref() != Some(want.as_str()) {
            failures.push(format!(
                "final_output: expected {want:?}, found {}",
                describe(found.as_deref())
            ));
        }
    }
    if let Some(want) = expect.min_steps {
        let found = field(report, "steps")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        if found < want {
            failures.push(format!(
                "min_steps: expected at least {want}, found {found}"
            ));
        }
    }
    if let Some(want) = expect.requests
        && report.requests.len() != want
    {
        failures.push(format!(
            "requests: expected {want}, found {}",
            report.requests.len()
        ));
    }
    if let Some(max) = expect.max_requests
        && report.requests.len() > max
    {
        failures.push(format!(
            "max_requests: expected at most {max}, found {}",
            report.requests.len()
        ));
    }
    if let Some(max) = expect.max_input_tokens {
        let found = report
            .result
            .as_ref()
            .and_then(|result| result.get("usage"))
            .and_then(|usage| usage.get("input_tokens"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        if found > max {
            failures.push(format!(
                "max_input_tokens: expected at most {max}, found {found}"
            ));
        }
    }
    if let Some(want) = expect.prefix_breaks
        && report.prefix_breaks != want
    {
        failures.push(format!(
            "prefix_breaks: expected {want}, found {}",
            report.prefix_breaks
        ));
    }
    for text in &expect.any_request_contains {
        if !report
            .requests
            .iter()
            .any(|request| request.body.contains(text.as_str()))
        {
            failures.push(format!(
                "any_request_contains: no request contains {text:?}"
            ));
        }
    }
    for entry in &expect.request_contains {
        let body = entry
            .request
            .checked_sub(1)
            .and_then(|index| report.requests.get(index));
        match body {
            None => failures.push(format!(
                "request_contains: request {} does not exist",
                entry.request
            )),
            Some(request) if !request.body.contains(&entry.text) => failures.push(format!(
                "request_contains: request {} does not contain {:?}",
                entry.request, entry.text
            )),
            Some(_) => {}
        }
    }
    for entry in &expect.request_absent {
        let body = entry
            .request
            .checked_sub(1)
            .and_then(|index| report.requests.get(index));
        match body {
            None => failures.push(format!(
                "request_absent: request {} does not exist",
                entry.request
            )),
            Some(request) if request.body.contains(&entry.text) => failures.push(format!(
                "request_absent: request {} contains {:?}",
                entry.request, entry.text
            )),
            Some(_) => {}
        }
    }
    failures
}

fn field<'a>(report: &'a Report, key: &str) -> Option<&'a serde_json::Value> {
    report
        .result
        .as_ref()?
        .get(key)
        .filter(|value| !value.is_null())
}

fn describe(value: Option<&str>) -> String {
    value.map_or_else(|| "nothing".to_owned(), |text| format!("{text:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_rejects_an_unknown_expect_key() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("typo.json");
        std::fs::write(
            &path,
            r#"{"prompt":"p","turns":[],"expect":{"request_count":1}}"#,
        )
        .expect("write");
        let err = load(&path).expect_err("an unknown key must fail");
        assert!(err.contains("typo.json"), "{err}");
        assert!(err.contains("request_count"), "{err}");
    }

    #[test]
    fn load_names_a_missing_workspace_file() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("s.json");
        std::fs::write(&path, r#"{"prompt":"p","turns":[],"workspace":"w.json"}"#).expect("write");
        let err = load(&path).expect_err("a missing workspace must fail");
        assert!(err.contains("w.json"), "{err}");
    }

    #[test]
    fn user_config_is_written_before_the_run() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("s.json");
        std::fs::write(
            &path,
            r#"{"prompt":"p","turns":[],"user_config":"[limits]\nprovider_max_attempts = 1\n"}"#,
        )
        .expect("write");
        let scenario = load(&path).expect("a scenario");
        let root = dir.path().join("run");
        write_user_config(&root, &scenario);
        let written = std::fs::read_to_string(root.join("config/rune/config.toml"))
            .expect("the configuration file");
        assert_eq!(written, "[limits]\nprovider_max_attempts = 1\n");

        let mut plain = scenario;
        plain.user_config = None;
        let empty = dir.path().join("empty");
        write_user_config(&empty, &plain);
        assert!(!empty.join("config").exists());
    }

    #[test]
    fn any_request_contains_needs_one_matching_body() {
        let mut scenario = load_inline(
            r#"{"prompt":"p","turns":[],"expect":{"any_request_contains":["needle"]}}"#,
        );
        let request = |body: &str| RecordedRequest {
            path: String::new(),
            body: body.to_owned(),
            bytes: body.len(),
            prompt_tokens: 0,
            cached_tokens: 0,
            completion_tokens: 0,
        };
        let mut report = Report {
            requests: vec![request("hay"), request("more hay")],
            ..Report::default()
        };
        assert_eq!(check(&scenario, &report).len(), 1);
        report.requests.push(request("a needle here"));
        assert!(check(&scenario, &report).is_empty());
        scenario.expect.any_request_contains.clear();
        report.requests.clear();
        assert!(check(&scenario, &report).is_empty());
    }

    #[test]
    fn request_absent_fails_when_the_text_is_there() {
        let scenario = load_inline(
            r#"{"prompt":"p","turns":[],"expect":{"request_absent":[{"request":1,"text":"secret"}]}}"#,
        );
        let request = |body: &str| RecordedRequest {
            path: String::new(),
            body: body.to_owned(),
            bytes: body.len(),
            prompt_tokens: 0,
            cached_tokens: 0,
            completion_tokens: 0,
        };
        let mut report = Report {
            requests: vec![request("nothing to see")],
            ..Report::default()
        };
        assert!(check(&scenario, &report).is_empty());
        report.requests[0] = request("the secret is here");
        assert_eq!(check(&scenario, &report).len(), 1);
        report.requests.clear();
        assert_eq!(
            check(&scenario, &report).len(),
            1,
            "a missing request fails"
        );
    }

    fn load_inline(text: &str) -> Scenario {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("inline.json");
        std::fs::write(&path, text).expect("write");
        load(&path).expect("a scenario")
    }

    #[test]
    fn check_reports_each_failed_expectation() {
        let scenario = Scenario {
            name: "synthetic".to_owned(),
            prompt: String::new(),
            workspace: BTreeMap::new(),
            global_args: Vec::new(),
            ask_args: Vec::new(),
            env: BTreeMap::new(),
            user_config: None,
            timeout_s: 60,
            turns: Vec::new(),
            expect: Expect {
                files: BTreeMap::from([("a.txt".to_owned(), Some("x\n".to_owned()))]),
                exit_code: Some(0),
                requests: Some(4),
                ..Expect::default()
            },
        };
        let report = Report {
            exit_code: Some(1),
            files: BTreeMap::from([("a.txt".to_owned(), Some("y\n".to_owned()))]),
            requests: Vec::new(),
            ..Report::default()
        };
        let failures = check(&scenario, &report);
        assert_eq!(failures.len(), 3, "{failures:?}");
        assert_eq!(
            failures[0],
            "files: a.txt: expected \"x\\n\", found \"y\\n\""
        );
        assert_eq!(failures[1], "exit_code: expected 0, found 1");
        assert_eq!(failures[2], "requests: expected 4, found 0");
    }
}
