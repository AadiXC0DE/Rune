//! One conversation, driven by the same turn loop the binary runs.
//!
//! The session holds what a turn reads: the dialect, the endpoint, the tools,
//! the rules, and the history. What the page controls from outside a running
//! turn, cancelling it and steering it, lives in [`Control`], apart from the
//! session, so the page can reach it while the turn is suspended on a request.

use std::sync::Arc;

use camino::Utf8PathBuf;
use rune_agent::history::History;
use rune_agent::steering::{Cancellation, SteeringQueue};
use rune_agent::turn::{self, Event, Host, StopReason, TurnOutcome};
use rune_core::budget::{Budget, BudgetSet, LimitName};
use rune_core::config::{Layer, PermissionMode};
use rune_core::error::{Result, RuneError};
use rune_net::fetch::Fetch;
use rune_net::message::ToolSpec;
use rune_net::provider::Provider;
use rune_net::transport::{AuthStyle, Endpoint};
use rune_policy::decision::{Layer as RuleLayer, Outcome};
use rune_policy::rules::{Rule, RuleSet};
use rune_tools::contract::{ExecutionContext, ToolOutput};
use rune_tools::registry::Registry;
use rune_tools::workspace::FileLimits;
use rune_tools::{EditFile, GlobFiles, GrepFiles, ReadFile, WriteFile};
use serde::Deserialize;
use serde_json::json;

use crate::bridge::{Bridge, BridgeFetch};
use crate::shell::PageShell;

/// Steps a turn may take in the page.
///
/// The binary leaves this unlimited. A model small enough to run in a tab can
/// repeat one call indefinitely, and a bound is what turns that into a turn
/// that ends and says why.
pub const DEFAULT_STEPS: u64 = 16;

/// Commands the page's shell offers that only read.
///
/// They are allowed without asking, as the binary allows the same ones. Any
/// other command is put to the person watching.
const READ_ONLY_COMMANDS: [&str; 12] = [
    "ls*",
    "pwd",
    "cat *",
    "head *",
    "tail *",
    "wc *",
    "grep *",
    "find *",
    "tree*",
    "echo *",
    "git status*",
    "git diff*",
];

/// How the page asked for a session to be set up.
#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    /// Wire dialect: `chat_completions`, `anthropic`, or `responses`.
    pub dialect: String,
    /// Base URL of the endpoint.
    pub base_url: String,
    /// Model identifier.
    pub model: String,
    /// Credential, empty where the endpoint needs none.
    #[serde(default)]
    pub api_key: String,
    /// Extra headers the endpoint requires.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// Workspace directory, as the page's filesystem names it.
    #[serde(default = "default_workspace")]
    pub workspace: String,
    /// Steps a turn may take.
    #[serde(default)]
    pub max_steps: Option<u64>,
}

fn default_workspace() -> String {
    String::from("/workspace")
}

/// What the page may do to a turn that is already running.
#[derive(Debug)]
pub struct Control {
    /// Cancels the running turn.
    pub cancellation: Cancellation,
    /// Messages typed while the turn runs.
    pub steering: SteeringQueue,
}

impl Default for Control {
    fn default() -> Self {
        Self {
            cancellation: Cancellation::new(),
            steering: SteeringQueue::new(8),
        }
    }
}

/// One conversation in the page.
pub struct Session {
    bridge: Arc<dyn Bridge>,
    control: Arc<Control>,
    dialect: Box<dyn Provider>,
    endpoint: Endpoint,
    model: String,
    instructions: String,
    registry: Registry,
    tools: Vec<ToolSpec>,
    rules: RuleSet,
    limits: BudgetSet,
    context: ExecutionContext,
    history: History,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// Builds a session from the page's configuration.
    pub fn new(config: &Config, bridge: Arc<dyn Bridge>, control: Arc<Control>) -> Result<Self> {
        let (dialect, auth): (Box<dyn Provider>, AuthStyle) = match config.dialect.as_str() {
            "anthropic" => (
                Box::new(rune_net::anthropic::Anthropic),
                AuthStyle::ApiKeyHeader,
            ),
            "responses" => (Box::new(rune_net::responses::Responses), AuthStyle::Bearer),
            "chat_completions" => (
                Box::new(rune_net::chat_completions::ChatCompletions),
                AuthStyle::Bearer,
            ),
            other => {
                return Err(RuneError::invalid_field(
                    "dialect",
                    format!("`{other}` is not a dialect this build speaks"),
                ));
            }
        };
        rune_net::transport::validate_url(&config.base_url)?;

        let mut endpoint = Endpoint::new(config.base_url.trim_end_matches('/'), &config.api_key);
        endpoint.auth = auth;
        endpoint.headers.clone_from(&config.headers);

        let workspace = Utf8PathBuf::from(&config.workspace);
        let mut limits = BudgetSet::new();
        limits.set(
            LimitName::MaxAgentSteps,
            Budget::Bounded(config.max_steps.unwrap_or(DEFAULT_STEPS)),
            Layer::Default,
        )?;
        // A retry waits between attempts, and a wait here would hold the tab's
        // only thread. The page reports a failure and the person retries.
        limits.set(
            LimitName::ProviderMaxAttempts,
            Budget::Bounded(1),
            Layer::Default,
        )?;

        let file_limits = FileLimits::from_budget(&limits);
        let mut registry = Registry::new();
        registry.insert(Box::new(GlobFiles::with_limits(file_limits)))?;
        registry.insert(Box::new(GrepFiles::with_limits(file_limits)))?;
        registry.insert(Box::new(ReadFile::with_limits(file_limits)))?;
        registry.insert(Box::new(WriteFile))?;
        registry.insert(Box::new(EditFile))?;
        registry.insert(Box::new(PageShell::new(Arc::clone(&bridge))))?;
        let tools = rune_tools::inventory::advertisement(&registry);

        let instructions = rune_context::prompt::instructions_for(
            &workspace,
            camino::Utf8Path::new("/config"),
            &limits,
        );

        Ok(Self {
            bridge,
            control,
            dialect,
            endpoint,
            model: config.model.clone(),
            instructions,
            registry,
            tools,
            rules: rules(),
            limits,
            context: ExecutionContext::new(workspace),
            history: History::new(),
        })
    }

    /// Runs one prompt to the end of its turn.
    ///
    /// The history is taken for the length of the turn and put back after it,
    /// whether the turn finished or failed, so a cancelled turn keeps what it
    /// had already recorded and the next prompt continues the conversation.
    pub fn prompt(&mut self, text: &str) -> Result<TurnOutcome> {
        self.control.cancellation.reset();
        let mut history = std::mem::take(&mut self.history);
        history.push_user(text);
        let result = turn::run_turn(&mut history, &*self);
        self.history = history;
        result
    }

    /// Forgets the conversation, keeping the configuration.
    pub fn reset(&mut self) {
        self.history = History::new();
    }

    /// Returns the instructions the model receives, for `/prompt`.
    #[must_use]
    pub fn instructions_text(&self) -> &str {
        &self.instructions
    }
}

/// The rules the page runs under.
///
/// The binary's defaults: reading and editing inside the workspace proceed,
/// read-only commands proceed, and the network is refused. Anything else falls
/// to the mode default, which in the page is a question for the person
/// watching.
fn rules() -> RuleSet {
    let mut rules = RuleSet::new();
    for tool in [
        "read_file",
        "glob_files",
        "grep_files",
        "write_file",
        "edit_file",
    ] {
        rules.push(Rule::allow(tool, "*", RuleLayer::Default));
    }
    for pattern in READ_ONLY_COMMANDS {
        rules.push(Rule::allow("shell", pattern, RuleLayer::Default));
    }
    rules
}

impl Host for Session {
    fn dialect(&self) -> &dyn Provider {
        self.dialect.as_ref()
    }

    fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    fn model(&self) -> String {
        self.model.clone()
    }

    fn instructions(&self) -> String {
        self.instructions.clone()
    }

    fn tools(&self) -> Vec<ToolSpec> {
        self.tools.clone()
    }

    fn fetch(&self) -> Box<dyn Fetch> {
        Box::new(BridgeFetch::new(Arc::clone(&self.bridge)))
    }

    fn emit(&self, event: Event) {
        self.bridge.emit(&encode(&event).to_string());
    }

    fn execute(&self, name: &str, arguments: &serde_json::Value) -> Result<ToolOutput> {
        let output = match self.registry.call(name, arguments, &self.context) {
            Ok(output) => output,
            Err(err) => {
                // Reported as a result too, so the line the page drew when the
                // call started does not stay on "running" once it has failed.
                self.bridge.emit(
                    &json!({
                        "kind": "tool_output",
                        "name": name,
                        "headline": err.message(),
                        "is_error": true,
                        "lines": 1,
                    })
                    .to_string(),
                );
                return Err(err);
            }
        };
        // The first line is what the binary prints for a finished call, so the
        // page shows the same thing at the moment the call returns.
        let headline = output
            .text
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("done")
            .trim()
            .to_owned();
        self.bridge.emit(
            &json!({
                "kind": "tool_output",
                "name": name,
                "headline": headline,
                "is_error": output.is_error,
                "lines": output.text.lines().count(),
            })
            .to_string(),
        );
        Ok(output)
    }

    fn decide(&self, name: &str, target: Option<&str>) -> (Outcome, String) {
        let (outcome, reason) = turn::decide_call(&self.rules, PermissionMode::Ask, name, target);
        if outcome != Outcome::Ask {
            return (outcome, reason);
        }
        let question = json!({
            "tool": name,
            "target": target.unwrap_or(name),
            "reason": reason,
        });
        if self.bridge.ask(&question) {
            (Outcome::Allow, format!("{reason}; allowed in this tab"))
        } else {
            (Outcome::Deny, format!("{reason}; declined in this tab"))
        }
    }

    fn context(&self) -> ExecutionContext {
        self.context.fork()
    }

    fn limits(&self) -> BudgetSet {
        self.limits.clone()
    }

    fn cancellation(&self) -> Cancellation {
        self.control.cancellation.clone()
    }

    fn steering(&self) -> &SteeringQueue {
        &self.control.steering
    }
}

/// Encodes an event for the page.
fn encode(event: &Event) -> serde_json::Value {
    match event {
        Event::TurnStarted { step } => json!({ "kind": "step", "step": step }),
        Event::TextDelta { delta } => json!({ "kind": "text", "delta": delta }),
        Event::ReasoningDelta { delta } => json!({ "kind": "reasoning", "delta": delta }),
        Event::ToolStarted { call, activity } => json!({
            "kind": "tool_started",
            "id": call.id,
            "name": call.name,
            "arguments": call.arguments,
            "label": activity.running_label(),
        }),
        Event::ToolFinished { call, is_error } => json!({
            "kind": "tool_finished",
            "id": call.id,
            "name": call.name,
            "is_error": is_error,
        }),
        Event::ToolDenied { call, reason } => json!({
            "kind": "tool_denied",
            "id": call.id,
            "name": call.name,
            "arguments": call.arguments,
            "reason": reason,
        }),
        Event::SteeringApplied { count, .. } => json!({ "kind": "steering", "count": count }),
        Event::Finished {
            reason,
            usage,
            steps,
            last_request,
        } => json!({
            "kind": "finished",
            "reason": reason.as_str(),
            "completed": matches!(reason, StopReason::Completed),
            "steps": steps,
            "input_tokens": usage.input_tokens,
            "output_tokens": usage.output_tokens,
            // How full the context is, which the status line reports.
            "context_tokens": last_request.input_tokens,
        }),
        // The page drops what the failed attempt streamed, as the terminal does.
        Event::StepRestarted { step } => json!({ "kind": "restarted", "step": step }),
    }
}

/// Encodes a failed turn for the page.
#[must_use]
pub fn encode_error(err: &RuneError) -> serde_json::Value {
    json!({
        "kind": "error",
        "code": err.code().as_str(),
        "message": err.message(),
        "hint": err.hint(),
    })
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::Mutex;

    use super::*;
    use crate::bridge::{Head, Ran};

    /// A page that answers from a script and records what it was told.
    #[derive(Default)]
    struct ScriptedPage {
        replies: Mutex<Vec<Vec<u8>>>,
        bodies: Mutex<Vec<(i32, Vec<u8>)>>,
        events: Mutex<Vec<serde_json::Value>>,
        requests: Mutex<Vec<serde_json::Value>>,
        questions: Mutex<Vec<serde_json::Value>>,
        commands: Mutex<Vec<String>>,
        allow: bool,
    }

    impl ScriptedPage {
        fn new(replies: Vec<String>, allow: bool) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies.into_iter().rev().map(String::into_bytes).collect()),
                allow,
                ..Self::default()
            })
        }

        fn kinds(&self) -> Vec<String> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter_map(|event| event["kind"].as_str().map(str::to_owned))
                .collect()
        }
    }

    impl Bridge for ScriptedPage {
        fn emit(&self, event: &str) {
            self.events
                .lock()
                .unwrap()
                .push(serde_json::from_str(event).unwrap());
        }

        fn open(&self, request: &serde_json::Value) -> Result<Head, String> {
            self.requests.lock().unwrap().push(request.clone());
            let body = self
                .replies
                .lock()
                .unwrap()
                .pop()
                .ok_or("the script is spent")?;
            let mut bodies = self.bodies.lock().unwrap();
            let handle = i32::try_from(bodies.len()).unwrap();
            bodies.push((handle, body));
            Ok(Head {
                handle,
                status: 200,
                content_type: String::from("text/event-stream"),
            })
        }

        fn read(&self, handle: i32, buffer: &mut [u8]) -> io::Result<usize> {
            let mut bodies = self.bodies.lock().unwrap();
            let (_, body) = bodies
                .iter_mut()
                .find(|(known, _)| *known == handle)
                .unwrap();
            // A small chunk, so a frame arrives split across reads the way a
            // network delivers it.
            let count = body.len().min(buffer.len()).min(7);
            buffer[..count].copy_from_slice(&body[..count]);
            body.drain(..count);
            Ok(count)
        }

        fn close(&self, _handle: i32) {}

        fn ask(&self, question: &serde_json::Value) -> bool {
            self.questions.lock().unwrap().push(question.clone());
            self.allow
        }

        fn run(&self, command: &str, _cwd: &str) -> Ran {
            self.commands.lock().unwrap().push(command.to_owned());
            Ran {
                output: String::from("ran"),
                exit_code: 0,
            }
        }
    }

    fn chunk(delta: &serde_json::Value, finish: Option<&str>) -> String {
        let frame = json!({
            "id": "c",
            "object": "chat.completion.chunk",
            "model": "scripted",
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        });
        format!("data: {frame}\n\n")
    }

    fn tool_call(id: &str, name: &str, arguments: &serde_json::Value) -> String {
        let mut body = chunk(
            &json!({ "tool_calls": [{ "index": 0, "id": id, "type": "function",
                "function": { "name": name, "arguments": "" } }] }),
            None,
        );
        body.push_str(&chunk(
            &json!({ "tool_calls": [{ "index": 0,
                "function": { "arguments": arguments.to_string() } }] }),
            None,
        ));
        body.push_str(&chunk(&json!({}), Some("tool_calls")));
        body.push_str("data: [DONE]\n\n");
        body
    }

    fn answer(text: &str) -> String {
        let mut body = String::new();
        for word in text.split_inclusive(' ') {
            body.push_str(&chunk(&json!({ "content": word }), None));
        }
        body.push_str(&chunk(&json!({}), Some("stop")));
        body.push_str("data: [DONE]\n\n");
        body
    }

    fn session(page: &Arc<ScriptedPage>, workspace: &camino::Utf8Path) -> Session {
        let config = Config {
            dialect: String::from("chat_completions"),
            base_url: String::from("http://127.0.0.1:7777/v1"),
            model: String::from("scripted"),
            api_key: String::new(),
            headers: Vec::new(),
            workspace: workspace.to_string(),
            max_steps: None,
        };
        let page = Arc::clone(page);
        let bridge: Arc<dyn Bridge> = page;
        Session::new(&config, bridge, Arc::new(Control::default())).expect("session")
    }

    fn workspace() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        std::fs::write(path.join("notes.txt"), "the answer is forty-two\n").expect("fixture");
        (dir, path)
    }

    #[test]
    fn a_turn_reads_a_real_file_and_answers() {
        let (_guard, root) = workspace();
        let page = ScriptedPage::new(
            vec![
                tool_call("call_1", "read_file", &json!({ "path": "notes.txt" })),
                answer("It says forty-two."),
            ],
            false,
        );
        let mut session = session(&page, &root);

        let outcome = session.prompt("what do the notes say?").expect("turn");

        assert_eq!(outcome.text, "It says forty-two.");
        let kinds = page.kinds();
        for expected in [
            "step",
            "tool_started",
            "tool_output",
            "tool_finished",
            "text",
            "finished",
        ] {
            assert!(
                kinds.contains(&expected.to_owned()),
                "no {expected} event: {kinds:?}"
            );
        }
        // The second request carries the file's contents back to the model,
        // which is the loop working rather than the page pretending.
        let requests = page.requests.lock().unwrap();
        let second = requests[1]["body"].as_str().unwrap();
        assert!(second.contains("forty-two"), "{second}");
        assert!(
            page.questions.lock().unwrap().is_empty(),
            "a read asked first"
        );
    }

    #[test]
    fn a_command_that_is_not_read_only_is_put_to_the_person_watching() {
        let (_guard, root) = workspace();
        let page = ScriptedPage::new(
            vec![
                tool_call("call_1", "shell", &json!({ "command": "cargo test" })),
                answer("The command was declined."),
            ],
            false,
        );
        let mut session = session(&page, &root);

        session.prompt("run the tests").expect("turn");

        let questions = page.questions.lock().unwrap();
        assert_eq!(questions.len(), 1, "{questions:?}");
        assert_eq!(questions[0]["target"], "cargo test");
        assert!(
            page.commands.lock().unwrap().is_empty(),
            "a declined command ran"
        );
        assert!(page.kinds().contains(&String::from("tool_denied")));
    }

    #[test]
    fn a_read_only_command_runs_without_asking() {
        let (_guard, root) = workspace();
        let page = ScriptedPage::new(
            vec![
                tool_call("call_1", "shell", &json!({ "command": "ls" })),
                answer("Listed."),
            ],
            false,
        );
        let mut session = session(&page, &root);

        session.prompt("list the files").expect("turn");

        assert!(page.questions.lock().unwrap().is_empty());
        assert_eq!(page.commands.lock().unwrap().as_slice(), ["ls"]);
    }

    #[test]
    fn a_turn_that_fails_keeps_the_conversation() {
        let (_guard, root) = workspace();
        let page = ScriptedPage::new(vec![answer("First answer.")], false);
        let mut session = session(&page, &root);
        session.prompt("one").expect("first turn");

        // The script is spent, so the second request fails at the transport.
        session
            .prompt("two")
            .expect_err("the second turn has no reply");

        assert!(
            session.history.to_messages().len() >= 3,
            "a failed turn dropped the conversation"
        );
    }

    #[test]
    fn an_unknown_dialect_is_refused() {
        let (_guard, root) = workspace();
        let page = ScriptedPage::new(Vec::new(), false);
        let config = Config {
            dialect: String::from("carrier_pigeon"),
            base_url: String::from("https://example.com"),
            model: String::from("m"),
            api_key: String::new(),
            headers: Vec::new(),
            workspace: root.to_string(),
            max_steps: None,
        };
        let bridge: Arc<dyn Bridge> = page;
        let err = Session::new(&config, bridge, Arc::new(Control::default())).expect_err("refused");
        assert_eq!(err.field(), Some("dialect"));
    }
}
