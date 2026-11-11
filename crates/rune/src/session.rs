//! The interactive session.
//!
//! Connects the terminal shell loop to the agent. One turn runs at a time; a
//! line typed while a turn is running is queued rather than refused, which is
//! what makes the shell usable while the model is working.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{BufRead, IsTerminal as _, Write as _};
use std::sync::{Arc, Mutex, mpsc};

use crate::context_watch;
use crate::session_log::{self, Recorder};
use camino::{Utf8Path, Utf8PathBuf};
use rune_agent::history::History;
use rune_agent::steering::{Cancellation, SteeringQueue};
use rune_agent::turn::{self, Event, Host, StopReason};
use rune_context::mcp::Client as McpClient;
use rune_context::prompt::{self, Prompt};
use rune_core::budget::BudgetSet;
use rune_core::config::{Effort, PermissionMode, Settings};
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::id::SessionId;
use rune_core::paths::Paths;
use rune_net::message::ToolSpec;
use rune_net::provider::Provider;
use rune_net::transport::Endpoint;
use rune_policy::approval::{ApprovalOutcome, Grants, SessionGrant};
use rune_policy::decision::Outcome;
use rune_policy::review::{ReviewOutcome, ReviewRequest, ReviewSession, Reviewer};
use rune_policy::rules::RuleSet;
use rune_session::usage::{HelperKind, Ledger, UsageRecord, now_ms};
use rune_term::footer::{self, FooterState, ProviderRetry};
use rune_term::input::KeyAction;
use rune_term::probe::TerminalEnv;
use rune_term::shell::ExitReason;
use rune_term::shell::{Action, Input, InputSource, Shell};
use rune_term::theme::{ColorDepth, Slot, Theme, detect_depth};
use rune_term::transcript::{self, Display, Entry};
use rune_tools::ask_user::{Answer, Answerer, Question, Unavailable};
use rune_tools::contract::{ExecutionContext, ToolOutput};
use rune_tools::inventory;
use rune_tools::registry::Registry;

/// Everything a session needs to run.
pub struct SessionConfig {
    /// Where the reviewer and the image client leave what they used.
    pub helper_usage: crate::helper_usage::HelperUsage,
    /// Use canonical line input and append-only labelled output.
    pub accessible: bool,
    /// Resolved settings.
    pub settings: Settings,
    /// State paths, used for the session log.
    pub paths: Paths,
    /// Session to resume, when the launch asked to resume one.
    pub resume: Option<SessionId>,
    /// Primary workspace.
    pub workspace: Utf8PathBuf,
    /// Endpoint for model requests.
    pub endpoint: Endpoint,
    /// Dialect the endpoint speaks.
    pub dialect: Box<dyn Provider>,
    /// Tool registry.
    pub registry: Registry,
    subagent: Option<Arc<crate::subagent::Bridge>>,
    /// The skill catalog the registry's search reads, for the MCP-aware one.
    catalog: Arc<dyn rune_tools::skill::Catalog>,
    /// Rules in force.
    pub rules: RuleSet,
    /// Question bridge, enabled only while terminal input is being polled.
    questions: Arc<TerminalQuestions>,
}

/// A writer that locks the shared stream for the length of one write.
///
/// Holding the lock across a turn would deadlock: the turn draws streamed text
/// through the same stream, so it would wait on a lock its own caller holds.
/// Locking per write keeps the bytes ordered without ever holding it that long.
struct LockedSink {
    stream: LiveSink,
}

impl std::io::Write for LockedSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut stream = self
            .stream
            .lock()
            .map_err(|_| std::io::Error::other("the output lock was poisoned"))?;
        stream.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut stream = self
            .stream
            .lock()
            .map_err(|_| std::io::Error::other("the output lock was poisoned"))?;
        stream.flush()
    }
}

/// Text that has streamed in but is not yet a finished line.
///
/// Held apart from the transcript because it is redrawn on every delta: the
/// transcript is printed once, while this is replaced in place until the step
/// ends and the text becomes final.
#[derive(Default)]
struct StreamingText {
    /// Assistant text so far in the current step.
    answer: String,
    /// Reasoning text so far, shown while the model is thinking.
    reasoning: String,
    /// Wrapped rows for each lane, and how much of the source they cover.
    ///
    /// Kept so a delta wraps only the text that arrived, rather than the whole
    /// answer again. Re-wrapping everything per delta is quadratic in the length
    /// of the response, which is what made a long answer stutter.
    answer_rows: transcript::AssistantRows,
    reasoning_rows: LazyRows,
}

/// Wrapped rows derived from a growing string.
///
/// Every complete line is wrapped once and kept. Only the line being written is
/// wrapped again on the next delta, so the cost of a delta is bounded by one
/// line rather than by the whole answer. Rewrapping everything per delta is
/// quadratic in the length of the response, which is what made a long answer
/// stutter: a twenty thousand character answer was re-wrapped twenty million
/// characters' worth.
///
/// The boundary is a newline because that is a hard break the wrapper already
/// respects, so a line that is complete can never be wrapped differently by
/// later text.
#[derive(Default)]
struct LazyRows {
    /// Rows for every line that is finished, ending in a newline.
    finished: Vec<String>,
    /// Bytes of the source those rows cover.
    covered: usize,
}

/// Returns the rows for a growing string, wrapping only what is new.
fn rows_for(lazy: &mut LazyRows, text: &str, width: usize) -> Vec<String> {
    // Text that shrank means the lane was cleared, so the cached rows describe
    // a response that is gone.
    if text.len() < lazy.covered {
        lazy.finished.clear();
        lazy.covered = 0;
    }

    // Everything up to the last newline is final. A newline is a hard break, so
    // no later text can change how the text before it wraps, and the wrap for
    // that prefix is computed once no matter how many deltas follow it.
    let sealed = text.rfind('\n').map_or(0, |at| at.saturating_add(1));
    if sealed != lazy.covered {
        lazy.finished = rune_term::width::wrap(text.get(..sealed).unwrap_or_default(), width);
        lazy.covered = sealed;
    }

    let tail = text.get(lazy.covered..).unwrap_or_default();
    if tail.is_empty() {
        return lazy.finished.clone();
    }

    // The tail continues the row the sealed prefix ended on. A prefix ending in
    // a newline wraps to a trailing empty row, which the tail fills rather than
    // sitting under, so that row is dropped before the tail is appended.
    let mut rows = lazy.finished.clone();
    rows.pop();
    rows.extend(rune_term::width::wrap(tail, width));
    rows
}

/// The stream a session draws on.
///
/// Shared rather than borrowed because the host draws from inside a turn, where
/// the caller holds no lock. Every write takes the lock for its own duration
/// only, so a delta arriving mid-turn can never wait on the turn that is
/// producing it.
type LiveSink = Arc<Mutex<dyn std::io::Write + Send>>;

/// One call waiting for the input thread's answer.
struct ApprovalRequest {
    tool: String,
    target: String,
    reason: String,
    /// Whether the answer may be remembered for the session. Never true for an
    /// action that destroys data or publishes.
    rememberable: bool,
    /// What the call would change, drawn under the scope, for an edit.
    preview: Vec<String>,
    answer: mpsc::SyncSender<ApprovalOutcome>,
}

/// A tool worker hands questions to the thread that owns terminal input.
#[derive(Debug)]
struct QuestionRequest {
    questions: Vec<Question>,
    answer: mpsc::SyncSender<Result<Answer>>,
}

#[derive(Debug, Default)]
struct TerminalQuestions {
    requests: Mutex<Option<mpsc::Sender<QuestionRequest>>>,
    cancellation: Cancellation,
}

impl Answerer for TerminalQuestions {
    fn ask(&self, questions: &[Question], context: &ExecutionContext) -> Result<Answer> {
        let Some(requests) = self.requests.lock().ok().and_then(|slot| slot.clone()) else {
            return Unavailable.ask(questions, context);
        };
        let (answer, response) = mpsc::sync_channel(1);
        if requests
            .send(QuestionRequest {
                questions: questions.to_vec(),
                answer,
            })
            .is_err()
        {
            return Unavailable.ask(questions, context);
        }
        loop {
            self.cancellation.check()?;
            context.check_cancelled()?;
            match response.recv_timeout(rune_term::shell::POLL_INTERVAL) {
                Ok(answer) => {
                    self.cancellation.check()?;
                    context.check_cancelled()?;
                    return answer;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Unavailable.ask(questions, context);
                }
            }
        }
    }
}

/// Host state for a turn.
struct SessionHost {
    /// Whether the active objective is checked before a turn finishes.
    goal_judge: std::sync::atomic::AtomicBool,
    /// The cache routing key of every request of this session, taken from
    /// the first request's prefix.
    cache_key: std::sync::OnceLock<String>,
    usage_paths: Option<Paths>,
    pre_tool_hook: Option<rune_core::config::PreToolHook>,
    post_tool_hook: Option<rune_core::config::PostToolHook>,
    accessible: bool,
    ascii: bool,
    recorder: Option<Arc<Mutex<Recorder>>>,
    endpoint: Endpoint,
    /// How requests to the endpoint are shaped, per model.
    compat: crate::provider_setup::CompatSource,
    dialect: Box<dyn Provider>,
    /// Model the session sends to, changeable while the session runs.
    ///
    /// Behind a lock because a slash command changes it from the input loop
    /// while a turn reads it on the turn's own thread. It is read once per
    /// request attempt, so the lock is not on the streaming path.
    model: Mutex<String>,
    /// Base instructions, rebuilt only at a checkpoint or on `/reload`.
    instructions: Mutex<String>,
    /// Leading bytes of the instructions that are the same in every workspace.
    stable_instructions_len: std::sync::atomic::AtomicUsize,
    /// The objective written into the instructions, fixed with them.
    frozen_objective: Mutex<FrozenObjective>,
    /// Changes the model has not been told about yet, in the order they happened.
    pending_updates: Mutex<Vec<PendingUpdate>>,
    /// What the instructions were built from, to notice when it changes.
    watched: Mutex<context_watch::Watched>,
    /// Instruction files below the workspace root, sent on first touch.
    nested_rules: Mutex<Vec<rune_context::InstructionFile>>,
    /// Bytes of each instruction section, for the context breakdown.
    instruction_sizes: Mutex<rune_agent::tokens::InstructionSizes>,
    /// The instructions with the repository map, fixed until compaction.
    mapped: prompt::MappedInstructions,
    tools: Vec<ToolSpec>,
    rules: Mutex<RuleSet>,
    mode: Mutex<PermissionMode>,
    /// The settings the rules were built from, so `/permissions` can rebuild
    /// them for another mode.
    permission_settings: Settings,
    /// Behind a lock because `/effort` changes it between turns.
    effort: Mutex<Effort>,
    /// Whether the user chose the effort, in the settings or with `/effort`;
    /// otherwise the model's default applies.
    effort_set: std::sync::atomic::AtomicBool,
    /// The model and requested level pairs whose clamping was already told.
    effort_noticed: Mutex<std::collections::BTreeSet<(String, &'static str)>>,
    fast_mode: bool,
    limits: BudgetSet,
    context: ExecutionContext,
    registry: Registry,
    /// MCP connections live until the session host is dropped.
    mcp: Option<McpClient>,
    subagent: Option<Arc<crate::subagent::Bridge>>,
    cancellation: Cancellation,
    steering: SteeringQueue,
    events: Arc<Mutex<Vec<Event>>>,
    /// The pending or active retry, drawn only through the live footer.
    provider_retry: Mutex<Option<ProviderRetry>>,
    /// Largest observed conversation size, estimated when usage is unavailable.
    context_used: std::sync::atomic::AtomicU64,
    /// Largest answer the current model produces, or zero when unknown.
    output_limit: std::sync::atomic::AtomicU64,
    /// Published prices of the current model, when a catalog lists them.
    prices: Mutex<Option<rune_net::models_dev::Prices>>,
    /// Whether the fetched catalog says the current model takes images.
    vision: std::sync::atomic::AtomicBool,
    /// Images tools returned, carried into every request.
    tool_images: crate::attachments::ToolImages,
    /// Estimated spend, in millionths of a dollar, at which requests stop.
    spend_limit: Mutex<Option<u64>>,
    /// Whether sandboxed shell commands may use the network; `/network` sets it.
    network: std::sync::atomic::AtomicBool,
    /// Present while the meter uses an estimate rather than provider usage.
    context_source: Mutex<Option<&'static str>>,
    /// Size of the context window, zero when the provider stated none.
    ///
    /// Atomic because choosing another model mid-session changes the window the
    /// status line is measured against, and the status line only has `&self`.
    context_limit: std::sync::atomic::AtomicU64,
    /// Theme the status line is drawn with.
    theme: Mutex<Theme>,
    /// The theme name in force, as the user wrote it.
    theme_name: Mutex<String>,
    /// Session identifier, shown shortened in the status line.
    ///
    /// Behind a lock because `/new` replaces it while the session runs, and the
    /// status line reads it through `&self`.
    session_id: Mutex<String>,
    /// Workspace, shown as given.
    workspace: String,
    /// What the terminal reported at startup.
    caps: rune_term::probe::TerminalCaps,
    /// How many colors the terminal renders.
    depth: ColorDepth,
    /// Terminal width the live region is drawn for.
    ///
    /// Held behind an atomic because a resize is noticed while the region is
    /// being drawn, and the width the status line was measured against has to
    /// move with it.
    width: std::sync::atomic::AtomicU16,
    /// Terminal height, kept for the layout the status line is solved in.
    height: std::sync::atomic::AtomicU16,
    /// Terminal size a test draws for, when it pins one.
    ///
    /// A real session leaves this `None` and reads the size from the terminal on
    /// every frame. A test pins it, because the terminal that runs the test
    /// reports a size that has nothing to do with the fixed grid the test
    /// replays the frame into.
    pinned_size: Option<(u16, u16)>,
    /// Draws the live region and owns every write to the terminal.
    inline: Mutex<rune_term::inline::Inline>,
    /// Held across a whole frame, paint and write together.
    ///
    /// Two threads draw once a turn runs on its own thread: the turn draws the
    /// text arriving, and the loop draws the line being typed. Without one lock
    /// covering both steps, one frame's bytes can land inside another's and the
    /// screen shows a mix of two.
    frame: Mutex<()>,
    /// Complete recorded conversation, retained even when model context compacts.
    transcript: Mutex<Vec<Entry>>,
    /// History turns already captured, adjusted whenever compaction renumbers it.
    transcript_history_len: std::sync::atomic::AtomicUsize,
    /// Prevents streaming frames from painting over the transcript or editor.
    transcript_open: std::sync::atomic::AtomicBool,
    /// The line being typed while a turn runs, with the caret's display column.
    ///
    /// Held on the host because the streaming thread redraws the whole frame
    /// for every token, and a frame drawn without this would put an empty
    /// prompt over the correction the user is halfway through typing. The
    /// column is the reader's rather than one worked out from the text, because
    /// the caret may be mid-line and a wide character takes two columns.
    typed: Mutex<(String, usize)>,
    /// Ordered upstream provider preference.
    provider_order: Vec<String>,
    /// Whether requests are restricted to that preference.
    provider_strict: bool,
    /// Escape that opens a reasoning line, resolved from the theme once.
    reasoning: Mutex<String>,
    /// Text streamed so far in the current step, drawn below the input.
    ///
    /// Shared and mutable because a delta arrives on the turn's own thread
    /// while the renderer needs to read what has accumulated.
    streaming: Arc<Mutex<StreamingText>>,
    /// Calls still running in the current step, by call id, with the row that
    /// stands for each in the live region.
    running: Mutex<Vec<Running>>,
    /// When the live region was last redrawn by the turn's idle tick.
    last_tick: Mutex<Option<std::time::Instant>>,
    /// Whether the stream changed since the last frame was written.
    frame_dirty: std::sync::atomic::AtomicBool,
    /// When the last stream frame was written.
    last_frame: Mutex<Option<std::time::Instant>>,
    /// When the turn now running sent its first request.
    turn_started: Mutex<Option<std::time::Instant>>,
    /// Where the live region is written, so a delta can be shown as it lands.
    ///
    /// Absent until a session attaches one, which keeps every other caller of
    /// this host, including the tests, free of a terminal.
    live_out: Arc<Mutex<Option<LiveSink>>>,
    /// Reviewer for unresolved actions, absent when none is configured.
    reviewer: Option<Box<dyn Reviewer>>,
    /// Requests made outside the main loop, waiting to be counted.
    helper_usage: crate::helper_usage::HelperUsage,
    /// What this session has spent, for `/cost`.
    totals: Mutex<Totals>,
    /// The bytes each file held before this session first wrote to it, for
    /// `/undo`.
    ///
    /// Keyed by path and written once per file, so the entry is the state
    /// before the session's first change rather than before its latest: putting
    /// a file back to where a chain of edits began is what a single undo
    /// command can honestly do. A file the session created is held as absent,
    /// so undoing removes it.
    undo: Mutex<BTreeMap<Utf8PathBuf, Option<Vec<u8>>>>,
    /// Review activity for the current turn.
    review_session: Arc<Mutex<ReviewSession>>,
    /// Attached only while a terminal input loop can collect an answer.
    approval_requests: Mutex<Option<mpsc::Sender<ApprovalRequest>>>,
    /// The change an edit awaiting a decision would make, for its prompt.
    approval_preview: Mutex<Vec<String>>,
    /// Exact actions approved for the rest of this session.
    grants: Mutex<Grants>,
    /// Whether the session is planning: only reads and plan updates may run.
    planning: std::sync::atomic::AtomicBool,
    /// Whether commands run without a sandbox: none works here and the user
    /// allowed that. Decided once, at the start.
    unsandboxed: bool,
    questions: Arc<TerminalQuestions>,
    /// The last request as it was sent, which a cache warming ping repeats.
    last_plan: Mutex<Option<rune_net::provider::RequestPlan>>,
    /// Ends cache warming when the session is no longer idle.
    warming: Arc<crate::cache_warming::WarmState>,
    /// Time between cache warming pings.
    warm_interval: std::time::Duration,
    /// Model a custom command chose for the next prompt only.
    pending_model: Mutex<Option<String>>,
    /// Custom commands whose ignored directives were already reported.
    ignored_reported: Mutex<std::collections::BTreeSet<String>>,
}

/// Runs one turn on a model other than the session's, restoring it on drop.
///
/// A guard rather than a reset after the turn, because a turn can end through
/// an error on any of several paths and each must leave the session model.
struct TurnModel<'a> {
    host: &'a SessionHost,
    previous: String,
}

impl<'a> TurnModel<'a> {
    fn apply(host: &'a SessionHost, model: &str) -> Self {
        let previous = std::mem::replace(&mut *lock_model(&host.model), model.to_owned());
        Self { host, previous }
    }
}

impl Drop for TurnModel<'_> {
    fn drop(&mut self) {
        std::mem::take(&mut self.previous).clone_into(&mut lock_model(&self.host.model));
    }
}

/// Locks the model, recovering from a poisoned lock.
///
/// A panic elsewhere must not make the session unable to name its own model,
/// so the value is taken as it stands.
fn lock_model(model: &Mutex<String>) -> std::sync::MutexGuard<'_, String> {
    model
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Locks the review session, recovering from a poisoned lock.
///
/// A panic while holding this lock must not make every later tool call fail, so
/// the state is taken as it stands.
fn lock_review(session: &Mutex<ReviewSession>) -> std::sync::MutexGuard<'_, ReviewSession> {
    session
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Host for SessionHost {
    fn dialect(&self) -> &dyn Provider {
        self.dialect.as_ref()
    }

    fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    fn compat(
        &self,
        model: &str,
    ) -> (
        rune_net::compat::EndpointCompat,
        rune_net::compat::ModelTraits,
    ) {
        self.compat.resolve(model)
    }

    fn model(&self) -> String {
        lock_model(&self.model).clone()
    }

    fn instructions(&self) -> String {
        let base = lock_value(&self.instructions).clone();
        let mut instructions = self.mapped.get(
            &base,
            self.context.workspace(),
            &crate::ask::map_limits(&self.limits, self.permission_settings.experiments),
        );
        // Fixed with the rest of the prefix: a goal set later reaches the
        // model as an update, and is written here at the next rebuild.
        let objective = {
            let mut frozen = lock_value(&self.frozen_objective);
            if *frozen == FrozenObjective::Unread {
                *frozen = FrozenObjective::Fixed(
                    self.recorder
                        .as_ref()
                        .and_then(|recorder| lock_value(recorder).objective().map(str::to_owned)),
                );
            }
            match &*frozen {
                FrozenObjective::Fixed(objective) => objective.clone(),
                FrozenObjective::Unread => None,
            }
        };
        if let Some(objective) = objective {
            instructions.push_str("\n\nActive session objective:\n");
            instructions.push_str(&objective);
        }
        instructions
    }

    fn stable_instructions_len(&self) -> usize {
        self.stable_instructions_len
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn instruction_sizes(&self) -> rune_agent::tokens::InstructionSizes {
        let base = lock_value(&self.instructions).clone();
        let mapped = self.mapped.get(
            &base,
            self.context.workspace(),
            &crate::ask::map_limits(&self.limits, self.permission_settings.experiments),
        );
        // The map follows the base after a blank line.
        let map = mapped.len().saturating_sub(base.len().saturating_add(2));
        rune_agent::tokens::InstructionSizes {
            map,
            ..*lock_value(&self.instruction_sizes)
        }
    }

    fn step_appendix(
        &self,
        history: &History,
        results: &[turn::CallResult],
    ) -> Option<turn::Appendix> {
        self.watch_instruction_files();
        let lines = self.take_updates();
        let rules = context_watch::scoped_rules(
            history,
            results,
            &lock_value(&self.nested_rules),
            self.context.workspace(),
            &self.limits,
        );
        let mut appendix = rules.unwrap_or_default();
        if !lines.is_empty() {
            let update = turn::context_update(&lines);
            appendix.text = if appendix.text.is_empty() {
                update
            } else {
                format!("{update}\n{}", appendix.text)
            };
        }
        (!appendix.text.is_empty()).then_some(appendix)
    }

    fn goal_judge(&self) -> Option<String> {
        if !self.goal_judge.load(std::sync::atomic::Ordering::Relaxed) {
            return None;
        }
        self.recorder
            .as_ref()?
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .objective()
            .map(str::to_owned)
    }

    fn lowest_effort(&self) -> Effort {
        turn::lowest_accepted_effort(self)
    }

    fn objective_tokens_remaining(&self) -> Option<u64> {
        let recorder = self
            .recorder
            .as_ref()?
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        recorder
            .objective_budget()
            .map(|(budget, tokens)| budget.saturating_sub(tokens))
    }

    fn account_objective_tokens(&self, tokens: u64) -> Result<()> {
        if let Some(recorder) = &self.recorder {
            recorder
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .account_objective_tokens(tokens)?;
        }
        Ok(())
    }

    fn tools(&self) -> Vec<ToolSpec> {
        self.tools.clone()
    }

    fn tool_activity(&self, name: &str) -> Option<rune_tools::Activity> {
        self.registry.activity(name)
    }

    fn tool_permission_target(&self, name: &str, arguments: &serde_json::Value) -> Option<String> {
        turn::permission_target_for(&self.registry, name, arguments)
    }

    #[cfg(not(target_family = "wasm"))]
    fn concurrent_tool_host(&self, name: &str) -> Option<&(dyn Host + Sync)> {
        // Children are read-only, so leading `subagent` calls run together too.
        (name == "subagent" || self.registry.get(name)?.is_read_only()).then_some(self)
    }

    fn tool_attachments(&self, result: &turn::CallResult) -> Vec<rune_net::message::ContentPart> {
        self.tool_images.attach(result)
    }

    fn provider_head_timeout_ms(&self) -> Option<u64> {
        crate::provider_setup::provider_head_timeout_ms(&self.permission_settings)
    }

    fn temperature(&self) -> Option<f64> {
        self.permission_settings.temperature
    }

    fn cache_key(&self) -> Option<String> {
        Some(
            self.cache_key
                .get_or_init(|| {
                    rune_net::provider::prefix_cache_key(&self.instructions(), &self.tools())
                })
                .clone(),
        )
    }

    fn accepts_images(&self) -> bool {
        if !self.dialect().capabilities().images {
            return false;
        }
        if self.vision.load(std::sync::atomic::Ordering::Relaxed) {
            return true;
        }
        let Some(paths) = &self.usage_paths else {
            return false;
        };
        let mut settings = self.permission_settings.clone();
        settings.model = self.model_name();
        crate::provider_setup::cached_vision(&settings, paths).unwrap_or(false)
    }

    fn effort(&self) -> Effort {
        let (requested, effective) = self.effective_effort();
        if requested != Effort::Auto && effective != requested {
            let model = self.model_name();
            let fresh = self
                .effort_noticed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert((model.clone(), requested.as_str()));
            if fresh {
                self.draw_notice(&format!(
                    "effort {requested} is not accepted by {model}; using {effective}"
                ));
            }
        }
        effective
    }

    fn max_output_tokens(&self) -> Option<u64> {
        Some(self.output_limit.load(std::sync::atomic::Ordering::Relaxed))
            .filter(|limit| *limit > 0)
    }

    fn context_window(&self) -> Option<u64> {
        Some(
            self.context_limit
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    fn fast_mode(&self) -> bool {
        self.fast_mode
    }

    fn compaction_model(&self) -> Option<String> {
        self.permission_settings.compaction_model.clone()
    }

    fn compaction_window(&self) -> Option<u64> {
        let model = self.compaction_model()?;
        let paths = self.usage_paths.as_ref()?;
        crate::model_limits::resolve(&self.permission_settings, paths, &model, None).context_window
    }

    fn prepare_request(
        &self,
        history: &mut History,
        plan: &mut rune_net::provider::RequestPlan,
        client: &dyn rune_net::fetch::Fetch,
    ) -> Result<()> {
        self.warming.stop();
        // The terminal shows reasoning as it streams.
        plan.show_reasoning = true;
        self.tool_images.hydrate(plan);
        self.check_spend()?;
        let estimate = request_estimate(self, plan, history.anchor())?;
        if self.context_used.load(std::sync::atomic::Ordering::Relaxed) == 0
            || self
                .context_source
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some()
        {
            self.record_context_estimate(estimate.input_tokens);
        }
        let window = rune_agent::compaction::Window(
            self.context_limit
                .load(std::sync::atomic::Ordering::Relaxed),
        );
        if let Some(done) =
            rune_agent::compaction::auto_compact(history, plan, self, client, window)?
        {
            self.record_context_estimate(done.after_tokens);
        }
        self.remember_plan(plan);
        Ok(())
    }

    fn compact_now(
        &self,
        history: &mut History,
        client: &dyn rune_net::fetch::Fetch,
        reason: turn::CompactReason,
    ) -> Result<bool> {
        let turn::CompactReason::Overflow { target_tokens, .. } = reason;
        rune_agent::compaction::compact_now(history, self, client, target_tokens)
    }

    fn observed_overflow(&self, model: &str, tokens: u64) {
        if let Some(paths) = &self.usage_paths {
            crate::model_limits::record_observed(
                paths,
                &rune_core::config::provider_key(&self.permission_settings.provider),
                model,
                tokens,
            );
        }
    }

    fn before_compaction(&self, history: &History) {
        self.capture_history(history);
    }

    fn context_replaced(&self, report: &rune_agent::compaction::CompactionReport) {
        // The history prefix changed, so the cache is already lost; this is
        // where a fresh map and fresh instructions cost nothing extra.
        self.rebuild_prefix();
        self.forget_context();
        self.transcript_history_len
            .store(report.remaining_turns, std::sync::atomic::Ordering::Relaxed);
        self.helper_usage
            .record(HelperKind::Compaction, &self.model(), report.usage);
        self.absorb_helper_usage();
    }

    fn compaction_abandoned(&self, usage: &rune_net::stream::Usage) {
        self.helper_usage
            .record(HelperKind::Compaction, &self.model(), *usage);
        self.absorb_helper_usage();
    }

    fn record_child_usage(&self, model: &str, usage: &rune_net::stream::Usage) {
        self.record_usage(usage);
        lock_value(&self.totals).record_model(model, usage, None);
        if let Some(paths) = &self.usage_paths {
            let mut record = UsageRecord::new(now_ms(), model, HelperKind::Main);
            record.input_tokens = usage.input_tokens;
            record.output_tokens = usage.output_tokens;
            record.cache_read_tokens = usage.cache_read_tokens;
            record.cache_write_tokens = usage.cache_write_tokens;
            record.reasoning_tokens = usage.reasoning_tokens;
            if let Err(error) = Ledger::from_paths(paths).append(&record) {
                self.draw_notice(&format!("child usage was not recorded: {error}"));
            }
        }
        if let Some(recorder) = &self.recorder {
            let mut recorder = recorder
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Err(error) = recorder.child_usage(usage) {
                recorder.journal_failed(error);
            }
        }
    }

    fn observe_response(&self, outcome: &rune_net::transport::StreamOutcome) {
        if let Some(recorder) = &self.recorder {
            let mut recorder = recorder
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Err(error) = recorder.response(outcome) {
                recorder.journal_failed(error);
            }
        }
    }

    fn journal(&self, entry: &turn::JournalEntry) {
        if let Some(recorder) = &self.recorder {
            let mut recorder = recorder
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Err(error) = recorder.journal(entry) {
                recorder.journal_failed(error);
            }
        }
    }

    fn emit(&self, event: Event) {
        // Text is accumulated as it arrives and drawn straight away, which is
        // what makes an answer appear while it is being written rather than
        // after the whole response has been received.
        //
        // Each delta is sanitized on its own before it is kept. Model output can
        // carry sequences a terminal acts on, such as a clipboard write or a
        // screen clear, and the rows drawn from this text reach the terminal
        // as they are. A sequence split across two deltas loses its introducer
        // in the first, so what arrives in the second is plain text.
        match &event {
            Event::ProviderRetry {
                next_attempt,
                max_attempts,
                delay,
                ..
            } => {
                *self
                    .provider_retry
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ProviderRetry {
                    attempt: *next_attempt,
                    max_attempts: *max_attempts,
                    delay: Some(*delay),
                });
                self.request_frame();
            }
            Event::ProviderRetryFinished { .. }
            | Event::TurnStarted { .. }
            | Event::Finished { .. } => {
                if matches!(event, Event::TurnStarted { .. })
                    && let Ok(mut started) = self.turn_started.lock()
                {
                    started.get_or_insert_with(std::time::Instant::now);
                }
                if self.clear_provider_retry() {
                    self.request_frame();
                }
            }
            Event::TextDelta { delta } => {
                let delta = transcript::sanitize(delta);
                if !self.journal_text(Some(&delta)) {
                    return;
                }
                if let Ok(mut streaming) = self.streaming.lock() {
                    streaming.answer.push_str(&delta);
                }
                self.request_frame();
            }
            Event::ReasoningDelta { delta } => {
                if let Ok(mut streaming) = self.streaming.lock() {
                    streaming.reasoning.push_str(&transcript::sanitize(delta));
                }
                self.request_frame();
            }
            Event::StepRestarted { .. } => {
                if !self.journal_text(None) {
                    return;
                }
                self.clear_streaming();
                if let Some(retry) = self
                    .provider_retry
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_mut()
                {
                    retry.delay = None;
                }
                self.request_frame();
            }
            Event::ToolStarted { call, activity } => {
                // What the model said before calling a tool is finished, so it
                // moves into scrollback ahead of the call it led to.
                let mut entries = self.take_narration();
                if let Ok(mut running) = self.running.lock() {
                    running.push(Running {
                        id: call.id.clone(),
                        label: self.running_label(call, *activity),
                        started: std::time::Instant::now(),
                    });
                }
                entries.retain(|entry| !entry.text.trim().is_empty());
                self.commit(&entries);
            }
            Event::ToolFinished {
                call,
                output,
                elapsed,
            } => {
                if let Ok(mut running) = self.running.lock() {
                    running.retain(|running| running.id != call.id);
                }
                // A plan change is reported by its own event.
                if call.name == "update_plan" && !output.is_error {
                    self.request_frame();
                } else {
                    let row = self.finished_row(call, output, *elapsed);
                    if self.accessible {
                        self.commit(&[Entry::tool(row)]);
                    } else {
                        let mut lines = vec![self.styled_tool_line(&row, output.is_error)];
                        if !output.is_error {
                            lines.extend(self.change_preview(call, output));
                        }
                        self.commit_lines(&lines);
                    }
                }
            }
            Event::ToolDenied { call, reason } => {
                let mut entries = self.take_narration();
                entries.retain(|entry| !entry.text.trim().is_empty());
                entries.push(Entry::notice(format!("refused {}: {reason}", call.name)));
                self.commit(&entries);
            }
            Event::PlanUpdated { update } => {
                self.draw_notice(&plan_summary(update));
            }
            Event::Notice { text } => self.draw_notice(text),
            Event::ContextPruned {
                results,
                tokens_freed,
            } => self.draw_notice(&format!(
                "replaced {results} old tool results with receipts, about {} tokens freed",
                footer::format_tokens(*tokens_freed)
            )),
            Event::ContextCompacted {
                removed_turns,
                remaining_turns,
                settled,
                ..
            } => self.draw_notice(&format!(
                "compacted {removed_turns} earlier turn(s); {remaining_turns} turn(s) remain{}",
                if *settled { " at a settled point" } else { "" }
            )),
            // A queued message is reported once the turn ends, from the events.
            Event::SteeringApplied { .. } => {}
            // The result ends with a note saying what ran.
            Event::CallRepaired { .. } => {}
            // Said in scrollback, so the reader knows why the turn went on.
            Event::VerificationRequested => {
                let mut entries = self.take_narration();
                entries.retain(|entry| !entry.text.trim().is_empty());
                entries.push(Entry::notice(
                    "files changed with no check since; asked for one before finishing",
                ));
                self.commit(&entries);
            }
        }
        if let Ok(mut events) = self.events.lock() {
            events.push(event);
        }
    }

    fn pre_tool(&self, call: &turn::PreparedCall) -> Result<Option<turn::HookVeto>> {
        crate::pre_tool_hook::run(
            self.pre_tool_hook.as_ref(),
            call,
            &self.context,
            &self.cancellation,
        )
    }

    fn post_tool(&self, result: &turn::CallResult) {
        if let Some(message) = crate::post_tool_hook::notify(
            self.post_tool_hook.as_ref(),
            result,
            &self.context,
            &self.cancellation,
            &self.endpoint.credential,
        ) {
            self.draw_notice(&message);
        }
    }

    fn execute(&self, name: &str, arguments: &serde_json::Value) -> Result<ToolOutput> {
        if let Some(client) = &self.mcp
            && client.tools().iter().any(|tool| tool.name == name)
        {
            return crate::mcp::call_tool(
                client,
                name,
                arguments,
                &self.cancellation,
                &self.context,
            );
        }
        if name == "subagent"
            && let Some(subagent) = &self.subagent
        {
            return subagent.call(self, &self.registry, arguments, &self.context);
        }
        // Captured before the call so the bytes that were there can be put
        // back. A capture failure is not fatal: the call still runs, and `/undo`
        // reports that it had nothing to restore for this file.
        if let Some(path) = mutating_path(name, arguments) {
            self.remember(path);
        }
        if name == "apply_patch"
            && let Some(input) = arguments.get("input").and_then(serde_json::Value::as_str)
        {
            for path in rune_tools::apply_patch::paths(input) {
                self.remember(Utf8Path::new(&path));
            }
        }
        if name == "rename_file"
            && let (Some(source), Some(destination)) = (
                arguments.get("path").and_then(serde_json::Value::as_str),
                arguments
                    .get("new_path")
                    .and_then(serde_json::Value::as_str),
            )
        {
            // Keep the source preimage, but record only successful renames so
            // a refused directory or destination cannot pollute `/undo`.
            let source = rune_tools::resolve(&self.context, source)?.path;
            let destination = rune_tools::resolve(&self.context, destination)?.path;
            let before = match rename_snapshot(&source) {
                Ok(bytes) => Some(bytes),
                Err(error) => return Ok(ToolOutput::from_error(&error)),
            };
            let output = self.registry.call(name, arguments, &self.context)?;
            if !output.is_error {
                let mut undo = self
                    .undo
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                undo.entry(source).or_insert(before);
                undo.entry(destination).or_insert(None);
            }
            return Ok(output);
        }
        self.registry.call(name, arguments, &self.context)
    }

    fn execute_with_context(
        &self,
        name: &str,
        arguments: &serde_json::Value,
        context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        if matches!(name, "web_fetch" | "update_plan" | "read_file") {
            return self.registry.call(name, arguments, context);
        }
        self.execute(name, arguments)
    }

    fn decide_private_network(&self, target: Option<&str>) -> (Outcome, String) {
        // Only a rule explicitly naming this authority at a user-controlled
        // layer can grant it. Broad web grants and project rules cannot.
        let mut rules = RuleSet::new();
        for rule in lock_rules(&self.rules).rules() {
            if rule.tool == "web_fetch_private"
                && (rule.outcome != Outcome::Allow
                    || rule.layer >= rune_policy::decision::Layer::User)
            {
                rules.push(rule.clone());
            }
        }
        let target = target.unwrap_or("web_fetch_private");
        let decision = rules.evaluate("web_fetch_private", target, Outcome::Ask);
        let reason = format!(
            "private-network access for this fetch and its redirects; {}",
            decision.explain()
        );
        if decision.outcome != Outcome::Ask {
            return (decision.outcome, reason);
        }
        self.request_approval("web_fetch_private", target, reason)
    }

    fn decide_quietly(&self, name: &str, target: Option<&str>) -> Outcome {
        crate::mcp::decide_call(
            &lock_rules(&self.rules),
            self.mode(),
            name,
            target,
            self.context.workspace(),
        )
        .0
    }

    fn decide(&self, name: &str, target: Option<&str>) -> (Outcome, String) {
        // Plan mode is enforced here, before any rule or mode, so no rule, mode
        // or approval can let a change through while the session is planning.
        if self.planning.load(std::sync::atomic::Ordering::Relaxed)
            && name != "update_plan"
            && !self
                .registry
                .get(name)
                .is_some_and(rune_tools::Tool::is_read_only)
        {
            let decision = rune_policy::decision::Decision::matched(
                Outcome::Deny,
                rune_policy::decision::Layer::Session,
                "plan mode",
            );
            return (decision.outcome, decision.explain());
        }
        let mode = self.mode();
        let (outcome, reason) = crate::mcp::decide_call(
            &lock_rules(&self.rules),
            mode,
            name,
            target,
            self.context.workspace(),
        );
        if outcome != Outcome::Ask {
            return (outcome, reason);
        }

        // In automatic mode an unresolved action gets one narrow review rather
        // than a person's attention. A caution holds the action and hands the
        // agent guidance. A reviewer that could not answer made no judgment, so
        // the user is asked instead, as if there were no reviewer.
        if mode == PermissionMode::Auto
            && let Some(reviewer) = &self.reviewer
        {
            let action = target.unwrap_or(name).to_owned();
            let request = ReviewRequest::new(action.clone(), vec![action.clone()], name, "");
            let outcome = lock_review(&self.review_session).review(reviewer.as_ref(), &request);
            return match outcome {
                ReviewOutcome::Clear { .. } => {
                    (Outcome::Allow, format!("{reason}; cleared by review"))
                }
                ReviewOutcome::Unavailable { reason: failure } => {
                    let mut entries = self.take_narration();
                    entries.push(Entry::notice(format!(
                        "the safety reviewer could not answer ({failure}); asking you instead"
                    )));
                    self.commit(&entries);
                    self.request_approval(name, target.unwrap_or(name), reason)
                }
                other => (
                    Outcome::Deny,
                    other
                        .reason()
                        .map_or_else(|| String::from("review held the action"), str::to_owned),
                ),
            };
        }

        self.request_approval(name, target.unwrap_or(name), reason)
    }

    fn decide_with_arguments(
        &self,
        name: &str,
        target: Option<&str>,
        arguments: &serde_json::Value,
    ) -> (Outcome, String) {
        let preview = if name == "edit_file" {
            let pairs = edit_pairs(arguments);
            let lines: Vec<(char, &str)> = pairs
                .iter()
                .flat_map(|(old, new)| rune_term::diff::changed_lines(old, new))
                .collect();
            // Plain text: the prompt measures its lines without escapes.
            rune_term::diff::render(
                &lines,
                &rune_term::diff::DiffStyle::default(),
                APPROVAL_PREVIEW_LINES,
                usize::from(self.width()),
                self.ascii,
            )
        } else {
            Vec::new()
        };
        if let Ok(mut slot) = self.approval_preview.lock() {
            *slot = preview;
        }
        let decision = self.decide(name, target);
        if let Ok(mut slot) = self.approval_preview.lock() {
            slot.clear();
        }
        decision
    }

    fn context(&self) -> ExecutionContext {
        // Sharing the cancellation flag is what lets a cancel reach every tool
        // in the turn rather than only the one running when it arrived.
        self.context
            .fork()
            .with_network(self.network.load(std::sync::atomic::Ordering::Relaxed))
    }

    fn limits(&self) -> BudgetSet {
        self.limits.clone()
    }

    fn cancellation(&self) -> Cancellation {
        self.cancellation.clone()
    }

    fn steering(&self) -> &SteeringQueue {
        &self.steering
    }

    fn provider_order(&self) -> Vec<String> {
        self.provider_order.clone()
    }

    fn provider_strict(&self) -> bool {
        self.provider_strict
    }
}

impl SessionHost {
    /// Whether the prompt cache is kept warm while idle: the setting is on,
    /// the dialect is Anthropic and the session is online.
    fn warming_allowed(&self) -> bool {
        self.permission_settings.cache_warming
            && self.dialect.name() == rune_net::anthropic::NAME
            && !self.endpoint.offline
    }

    /// Keeps the request about to be sent, for a cache warming ping to repeat.
    fn remember_plan(&self, plan: &rune_net::provider::RequestPlan) {
        if self.warming_allowed() {
            *self
                .last_plan
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(plan.clone());
        }
    }

    /// Starts keeping the prompt cache warm after a completed turn.
    fn start_warming(&self) -> Option<std::thread::JoinHandle<u32>> {
        if !self.warming_allowed() {
            return None;
        }
        let plan = self
            .last_plan
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()?;
        Some(crate::cache_warming::start(
            &self.warming,
            crate::cache_warming::Warmer {
                plan,
                endpoint: self.endpoint.clone(),
                dialect: Box::new(rune_net::anthropic::Anthropic),
                timeouts: rune_net::transport::RequestTimeouts::from_settings(
                    &self.limits,
                    self.provider_head_timeout_ms(),
                ),
                usage: self.helper_usage.clone(),
                interval: self.warm_interval,
            },
        ))
    }

    /// Ends cache warming; the session is no longer idle on the same prefix.
    fn stop_warming(&self) {
        self.warming.stop();
    }

    /// Queues a line telling the model about a change.
    fn queue_update(&self, line: String) {
        queue(&self.pending_updates, PendingUpdate::other(line));
    }

    /// Takes every queued line, oldest first.
    fn take_updates(&self) -> Vec<String> {
        std::mem::take(&mut *lock_value(&self.pending_updates))
            .into_iter()
            .map(|update| update.line)
            .collect()
    }

    /// Queues a line for each instruction file that changed on disk.
    fn watch_instruction_files(&self) {
        let workspace = self.context.workspace().to_owned();
        for (path, line) in lock_value(&self.watched).file_changes(&workspace) {
            queue(&self.pending_updates, PendingUpdate::file(path, line));
        }
    }

    /// Queues lines for everything a new prompt should hear about.
    fn watch_before_prompt(&self) {
        self.watch_instruction_files();
        let Some(paths) = &self.usage_paths else {
            return;
        };
        let workspace = self.context.workspace().to_owned();
        let mut watched = lock_value(&self.watched);
        for line in watched.skill_changes(&workspace, &paths.config_root) {
            queue(&self.pending_updates, PendingUpdate::skill(line));
        }
        if let Some(line) = watched.test_command_change(&workspace) {
            queue(&self.pending_updates, PendingUpdate::other(line));
        }
    }

    /// Re-derives the instructions, the map and the objective from what is
    /// on disk now, and drops queued news they already carry.
    fn rebuild_prefix(&self) {
        self.mapped.refresh();
        *lock_value(&self.frozen_objective) = FrozenObjective::Unread;
        let Some(paths) = &self.usage_paths else {
            return;
        };
        let workspace = self.context.workspace().to_owned();
        let base = prompt::base_prompt_with(
            &workspace,
            &paths.config_root,
            &self.limits,
            &prompt::PromptOptions {
                variant: self.permission_settings.experiments.prompt,
                tools: &tool_names(&self.tools),
            },
        );
        *lock_value(&self.instruction_sizes) = crate::ask::instruction_sizes(&base);
        *lock_value(&self.instructions) = base.instructions;
        self.stable_instructions_len
            .store(base.stable_len, std::sync::atomic::Ordering::Relaxed);
        *lock_value(&self.nested_rules) = base.nested;
        *lock_value(&self.watched) = context_watch::Watched::of(&workspace, &paths.config_root);
        lock_value(&self.pending_updates).retain(|update| update.kind == UpdateKind::Other);
        self.save_prefix();
    }

    /// Saves the prefix this session's requests open with, so a resume with
    /// the same model sends the same bytes. A failure only costs the cache.
    fn save_prefix(&self) {
        let (Some(paths), Some(recorder)) = (&self.usage_paths, &self.recorder) else {
            return;
        };
        let id = lock_value(recorder).id();
        let base = lock_value(&self.instructions).clone();
        let mapped = self.mapped.get(
            &base,
            self.context.workspace(),
            &crate::ask::map_limits(&self.limits, self.permission_settings.experiments),
        );
        let map = mapped.get(base.len()..).unwrap_or_default().to_owned();
        let prefix = {
            let watched = lock_value(&self.watched);
            crate::session_prefix::SavedPrefix::new(
                self.permission_settings.provider.as_str(),
                &self.model(),
                base,
                self.stable_instructions_len
                    .load(std::sync::atomic::Ordering::Relaxed),
                map,
                self.tools.clone(),
                watched.file_records(),
                watched.skill_names(),
                watched.test_command(),
            )
        };
        if let Err(err) = crate::session_prefix::save(paths, &id, &prefix) {
            self.draw_notice(&format!("the session prefix was not saved: {err}"));
        }
    }

    /// Waits for a terminal answer while allowing cancellation to wake us.
    fn request_approval(&self, tool: &str, target: &str, reason: String) -> (Outcome, String) {
        if self
            .grants
            .lock()
            .is_ok_and(|grants| grants.allows(tool, target))
        {
            return (
                Outcome::Allow,
                format!("{reason}; approved earlier in this session"),
            );
        }
        let Some(requests) = self
            .approval_requests
            .lock()
            .ok()
            .and_then(|slot| slot.clone())
        else {
            return (Outcome::Ask, reason);
        };
        let (answer, response) = mpsc::sync_channel(1);
        let request = ApprovalRequest {
            tool: tool.to_owned(),
            target: target.to_owned(),
            reason: reason.clone(),
            rememberable: rememberable(tool, target),
            preview: self
                .approval_preview
                .lock()
                .map(|mut preview| std::mem::take(&mut *preview))
                .unwrap_or_default(),
            answer,
        };
        if requests.send(request).is_err() {
            return (
                Outcome::Deny,
                format!("{reason}; approval input is unavailable"),
            );
        }
        loop {
            if self.cancellation.is_cancelled() {
                return (Outcome::Deny, format!("{reason}; approval was cancelled"));
            }
            match response.recv_timeout(rune_term::shell::POLL_INTERVAL) {
                Ok(ApprovalOutcome::RunOnce) if !self.cancellation.is_cancelled() => {
                    return (Outcome::Allow, format!("{reason}; approved for this call"));
                }
                Ok(ApprovalOutcome::RememberForSession) if !self.cancellation.is_cancelled() => {
                    if let Ok(mut grants) = self.grants.lock() {
                        grants.insert(SessionGrant::literal(tool, target));
                    }
                    return (
                        Outcome::Allow,
                        format!("{reason}; approved for the rest of this session"),
                    );
                }
                Ok(_) => return (Outcome::Deny, format!("{reason}; approval was denied")),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return (
                        Outcome::Deny,
                        format!("{reason}; approval input was closed"),
                    );
                }
            }
        }
    }

    /// Points the session at another model.
    ///
    /// The next request uses it. A turn already running keeps the model it
    /// started with, because its request has already been sent.
    ///
    /// The context window moves with the model only when it is known: a window
    /// left over from a larger model would understate how full the smaller one
    /// is, and a window invented for a model nothing describes would be a guess
    /// presented as a fact.
    fn set_model(
        &self,
        model: &str,
        context_window: Option<u64>,
        output: Option<u64>,
        prices: Option<rune_net::models_dev::Prices>,
    ) {
        if let Ok(mut current) = self.prices.lock() {
            *current = prices;
        }
        {
            let mut current = lock_model(&self.model);
            model.clone_into(&mut current);
        }
        self.effort_noticed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        // What the previous model could see says nothing about this one; the
        // cache is asked for the new model instead.
        self.vision
            .store(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(window) = context_window.filter(|window| *window > 0) {
            self.context_limit
                .store(window, std::sync::atomic::Ordering::Relaxed);
        }
        // Unlike the window, an unknown ceiling is reset: a ceiling left over
        // from a larger model would be refused by a smaller one.
        self.output_limit.store(
            output.unwrap_or_default(),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Returns the theme the interface is drawn with.
    fn theme(&self) -> Theme {
        lock_value(&self.theme).clone()
    }

    /// Switches the theme, recolouring the reasoning lane and repainting the
    /// live region in full so no row keeps the old colours.
    fn set_theme(&self, theme: Theme) {
        *lock_value(&self.reasoning) = theme.sgr(Slot::Dim, self.depth);
        *lock_value(&self.theme) = theme;
        if let Ok(mut inline) = self.inline.lock() {
            inline.invalidate();
        }
    }

    /// Returns true when the terminal's background is light.
    fn terminal_is_light(&self) -> bool {
        rune_term::theme::terminal_is_light(&self.caps)
    }

    /// Returns how many colors the terminal renders.
    const fn depth(&self) -> ColorDepth {
        self.depth
    }

    /// Returns the model the session is sending to.
    /// Returns the permission mode in force.
    fn mode(&self) -> PermissionMode {
        *self
            .mode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn model_name(&self) -> String {
        lock_model(&self.model).clone()
    }

    /// The requested effort and the level sent for the current model, after
    /// its default and clamping.
    fn effective_effort(&self) -> (Effort, Effort) {
        let requested = *self
            .effort
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (_, traits) = self.compat.resolve(&self.model_name());
        let user_set = self.effort_set.load(std::sync::atomic::Ordering::Relaxed);
        (
            requested,
            rune_net::compat::effective_effort(requested, user_set, &traits),
        )
    }

    /// Returns whether a model has been chosen.
    fn has_model(&self) -> bool {
        !lock_model(&self.model).trim().is_empty()
    }

    /// Adopts a model's real context window, when the endpoint reports one.
    ///
    /// The configured window wins, because a user who declared one is describing
    /// the model they selected and the endpoint may advertise a number that
    /// counts only part of the conversation. When nothing is configured the
    /// endpoint's figure is far better than the compiled default, which
    /// understates a large model so badly that a session looks nearly full
    /// before it has said anything.
    fn adopt_context_window(&self, reported: Option<u64>, configured: bool) {
        if configured {
            return;
        }
        if let Some(window) = reported.filter(|window| *window > 0) {
            self.context_limit
                .store(window, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Reports the running state for the read-only commands.
    ///
    /// Borrowed from live state rather than collected at the start, so a status
    /// line read after `/model` names the model now in effect.
    fn info<'a>(&'a self, provider: &'a str, endpoint: &'a str) -> SessionInfo<'a> {
        SessionInfo {
            model: self.model_name(),
            provider,
            endpoint,
            mode: self.mode(),
            effort: self.effective_effort().1,
            session_id: self.session_id_name(),
            workspace: &self.workspace,
            terminal: self.caps.summary(),
            depth: self.depth,
            context_used: self.context_used.load(std::sync::atomic::Ordering::Relaxed),
            context_source: *self
                .context_source
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            context_limit: self
                .context_limit
                .load(std::sync::atomic::Ordering::Relaxed),
            totals: self
                .totals
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            spend_limit: self.spend_limit.lock().ok().and_then(|limit| *limit),
        }
    }

    /// Adds one turn's usage to the session total.
    fn record_usage(&self, usage: &rune_net::stream::Usage) {
        self.totals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record(usage);
    }

    /// Moves finished helper requests into the session total and the ledger.
    ///
    /// A ledger write must never end a session, so a failure is only reported.
    fn absorb_helper_usage(&self) {
        for (kind, model, usage) in self.helper_usage.drain() {
            let Some(label) = crate::helper_usage::label(kind) else {
                continue;
            };
            let cost = self.usage_paths.as_ref().and_then(|paths| {
                crate::helper_usage::cost(&self.permission_settings, paths, &model, &usage)
            });
            self.totals
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .record_helper(label, &model, &usage, cost);
            if let Some(paths) = &self.usage_paths
                && let Err(error) =
                    crate::helper_usage::append_to_ledger(paths, kind, &model, &usage, cost)
            {
                self.draw_notice(&format!("helper usage was not recorded: {error}"));
            }
        }
    }

    /// Remembers a file's contents before this session changes it.
    ///
    /// Only the first capture for a path is kept, so an undo returns the file
    /// to the state it was in before the session touched it rather than to some
    /// intermediate state that only makes sense in the middle of a chain.
    fn remember(&self, path: &Utf8Path) {
        let mut undo = self
            .undo
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if undo.contains_key(path) {
            return;
        }
        // A file that is absent or unreadable is recorded as absent, so undoing
        // a creation removes the file and an unreadable file is not silently
        // reported as restored.
        undo.insert(path.to_owned(), std::fs::read(path).ok());
    }

    /// Returns whether anything is available to undo.
    fn can_undo(&self) -> bool {
        !self
            .undo
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }

    /// Describes the current-to-original changes without consuming undo records.
    fn preview_undo(&self) -> Vec<String> {
        let undo = self
            .undo
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if undo.is_empty() {
            return vec!["nothing to undo: this session has not changed any file".to_owned()];
        }
        let mut lines = vec!["undo preview (files and mutation journal unchanged)".to_owned()];
        for (path, original) in undo.iter() {
            let current = match preview_snapshot(path) {
                Ok(bytes) => bytes,
                Err(err) => {
                    lines.push(format!("`{path}` could not be previewed: {err}"));
                    continue;
                }
            };
            lines.push(format!(
                "would {} {path}",
                if original.is_some() {
                    "restore"
                } else {
                    "remove"
                }
            ));
            lines.extend(undo_diff(path, current.as_deref(), original.as_deref()));
        }
        lines
    }

    /// Puts every changed file back and forgets the record.
    ///
    /// Returns one line per file, describing what was done, or a failure
    /// naming the file that could not be restored. Files are restored before
    /// the record is cleared, so a failure leaves the rest still undoable.
    fn undo(&self) -> Result<Vec<String>> {
        let mut undo = self
            .undo
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut report = Vec::with_capacity(undo.len());
        let mut done: Vec<Utf8PathBuf> = Vec::with_capacity(undo.len());
        for (path, before) in undo.iter() {
            match before {
                Some(bytes) => {
                    std::fs::write(path, bytes).map_err(|err| {
                        RuneError::new(
                            ErrorCode::Internal,
                            format!("`{path}` could not be restored: {err}"),
                        )
                    })?;
                    report.push(format!("restored {path}"));
                }
                None => {
                    // A file that did not exist before is removed, which is what
                    // undoing its creation means.
                    match std::fs::remove_file(path) {
                        Ok(()) => report.push(format!("removed {path}")),
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                            report.push(format!("{path} was already gone"));
                        }
                        Err(err) => {
                            return Err(RuneError::new(
                                ErrorCode::Internal,
                                format!("`{path}` could not be removed: {err}"),
                            ));
                        }
                    }
                }
            }
            done.push(path.clone());
        }
        for path in done {
            undo.remove(&path);
        }
        Ok(report)
    }

    /// Records how large the conversation has become.
    ///
    /// Takes the largest reading rather than summing them. Each turn resends
    /// the whole conversation, so an input count already includes every earlier
    /// turn: adding them counts the same history once per turn, which is what
    /// made a session appear to fill its window several times over.
    fn record_context_size(&self, used: u64) {
        if used == 0 {
            return;
        }
        let mut source = self
            .context_source
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if source.take().is_some() {
            // The first live reading supersedes the resume estimate, even when
            // the saved per-turn usage overestimated a multi-request turn.
            self.context_used
                .store(used, std::sync::atomic::Ordering::Relaxed);
        } else {
            self.context_used
                .fetch_max(used, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Labels byte-derived readings separately from observed provider usage.
    fn record_context_estimate(&self, used: u64) {
        let mut source = self
            .context_source
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.context_used
            .store(used, std::sync::atomic::Ordering::Relaxed);
        *source = Some("request bytes");
    }

    /// Estimates the current conversation, including instructions and tools.
    fn estimate_context(&self, history: &History) -> Result<()> {
        let mut plan = rune_net::provider::RequestPlan::new(self.model());
        plan.instructions = self.instructions();
        plan.messages = history.to_messages();
        plan.tools = self.tools();
        plan.tool_choice = if plan.tools.is_empty() {
            rune_net::provider::ToolChoice::None
        } else {
            rune_net::provider::ToolChoice::Auto
        };
        plan.parallel_tool_calls = self
            .limits
            .get(rune_core::budget::LimitName::ParallelToolCalls)
            .value()
            != Some(1);
        plan.effort = Host::effort(self);
        plan.fast_mode = self.fast_mode;
        plan.provider_order.clone_from(&self.provider_order);
        plan.provider_strict = self.provider_strict;
        self.record_context_estimate(request_estimate(self, &plan, history.anchor())?.input_tokens);
        Ok(())
    }

    /// Missing counts stay estimates; explicit zero usage is still a report.
    fn record_context_usage(
        &self,
        history: &History,
        usage: &rune_net::stream::Usage,
    ) -> Result<()> {
        if let (Some(input), Some(output)) = (usage.input_tokens, usage.output_tokens) {
            let used = input.saturating_add(output);
            if used == 0 {
                self.forget_context();
            } else {
                self.record_context_size(used);
            }
            Ok(())
        } else {
            self.estimate_context(history)
        }
    }

    /// Captures new turns before compaction can remove them from model context.
    fn capture_history(&self, history: &History) {
        let start = self
            .transcript_history_len
            .load(std::sync::atomic::Ordering::Relaxed)
            .min(history.len());
        self.transcript
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(history_entries(&history.turns()[start..]));
        self.transcript_history_len
            .store(history.len(), std::sync::atomic::Ordering::Relaxed);
    }

    /// Forgets the context reading, for a conversation that has been replaced.
    fn forget_context(&self) {
        *self
            .context_source
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.context_used
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }

    /// Returns the identifier of the session now running.
    fn session_id_name(&self) -> String {
        self.session_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Points the status line at a new session.
    fn set_session_id(&self, id: &str) {
        let mut slot = self
            .session_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        id.clone_into(&mut slot);
    }

    /// Returns the status line for the current state.
    fn status_line(&self, width: usize) -> String {
        let state = FooterState {
            provider_retry: *self
                .provider_retry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            model: lock_model(&self.model).clone(),
            permission_mode: self.mode(),
            planning: self.planning.load(std::sync::atomic::Ordering::Relaxed),
            unsandboxed: self.unsandboxed,
            workspace: self.workspace.clone(),
            context_used: self.context_used.load(std::sync::atomic::Ordering::Relaxed),
            context_source: *self
                .context_source
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            context_limit: self
                .context_limit
                .load(std::sync::atomic::Ordering::Relaxed),
            session_id: self
                .session_id
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            cost: self
                .totals
                .lock()
                .ok()
                .and_then(|totals| totals.cost)
                .map(|micro| format!("${}.{:04}", micro / 1_000_000, (micro % 1_000_000) / 100)),
        };
        let layout = footer::solve(
            (
                u16::try_from(width).unwrap_or(80),
                self.height.load(std::sync::atomic::Ordering::Relaxed),
            ),
            1,
            false,
            footer::DEFAULT_MINIMUM_ROWS,
        );
        footer::render(&state, &layout, &self.theme(), width, self.depth).join("\n")
    }

    /// Returns the line the activity line should show for a finished turn.
    #[must_use]
    pub fn activity_line(outcome: &turn::TurnOutcome) -> Option<String> {
        match outcome.stop_reason {
            StopReason::ObjectiveBudget => {
                Some(String::from("exhausted the objective token budget"))
            }
            StopReason::StepLimit => Some(String::from("reached the step limit")),
            StopReason::Cancelled => Some(String::from("cancelled")),
            _ => None,
        }
    }

    /// Returns the current terminal width.
    fn width(&self) -> u16 {
        self.width.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Takes the terminal's current size, keeping the last known one when the
    /// terminal cannot report it.
    ///
    /// Read every frame rather than once, because a resized window would
    /// otherwise keep a layout measured for the old width.
    fn refresh_size(&self) {
        let Some((cols, rows)) = self.pinned_size.or_else(rune_term::shell::terminal_size) else {
            return;
        };
        if cols == 0 || rows == 0 {
            return;
        }
        self.width.store(cols, std::sync::atomic::Ordering::Relaxed);
        self.height
            .store(rows, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut inline) = self.inline.lock() {
            inline.set_width(cols);
            // One row of slack, so a region that fills the screen still has
            // somewhere to move the caret and never writes into the terminal's
            // last row, which is what makes it scroll.
            inline.set_max_rows(rows.saturating_sub(1));
        }
    }

    /// Draws the live region and returns the bytes the terminal must receive.
    ///
    /// This is the only method that produces output for the interactive path. A
    /// line printed beside it would move the rows the region is drawn at, so
    /// there is exactly one writer: finished lines are handed here and printed
    /// once, and everything after them is redrawn in place.
    fn paint(
        &self,
        settled: &[String],
        activity: Option<&str>,
        prompt: &[String],
        arriving: &[String],
        caret: (u16, u16),
    ) -> Result<Vec<u8>> {
        self.paint_with_menu(settled, activity, prompt, arriving, &[], caret, false)
    }

    /// Draws the live region with a list opened under the input.
    ///
    /// Separate from [`SessionHost::paint`] so the many callers that draw no
    /// menu do not have to pass an empty slice for one. A `paced` frame that
    /// would change nothing on screen returns no bytes.
    #[allow(clippy::too_many_arguments)]
    fn paint_with_menu(
        &self,
        settled: &[String],
        activity: Option<&str>,
        prompt: &[String],
        arriving: &[String],
        menu: &[String],
        caret: (u16, u16),
        paced: bool,
    ) -> Result<Vec<u8>> {
        if self.accessible {
            let mut lines: Vec<String> = settled
                .iter()
                .map(|line| transcript::sanitize(line))
                .collect();
            if let Some(activity) = activity {
                lines.extend(transcript::accessible_lines(&[Entry::notice(activity)]));
            }
            return Ok(transcript::append_lines(&lines));
        }
        self.refresh_size();
        let footer_rows = self.status_rows();
        let mut inline = self
            .inline
            .lock()
            .map_err(|_| RuneError::new(ErrorCode::Internal, "the renderer lock was poisoned"))?;
        let frame = rune_term::inline::Frame {
            settled,
            arriving,
            activity,
            footer: &footer_rows,
            prompt,
            menu,
            caret,
        };
        Ok(if paced {
            inline.frame_if_changed(&frame)
        } else {
            inline.frame(&frame)
        })
    }

    /// Removes the live region, for a clean exit.
    fn clear_region(&self) -> Result<Vec<u8>> {
        if self.accessible {
            return Ok(Vec::new());
        }
        let mut inline = self
            .inline
            .lock()
            .map_err(|_| RuneError::new(ErrorCode::Internal, "the renderer lock was poisoned"))?;
        Ok(inline.clear())
    }

    /// Returns the footer rows for the current state.
    fn status_rows(&self) -> Vec<String> {
        self.status_line(usize::from(self.width()))
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Rows left below the one-line prompt, including menu headings and hints.
    fn menu_room(&self) -> usize {
        self.refresh_size();
        usize::from(self.height.load(std::sync::atomic::Ordering::Relaxed))
            .saturating_sub(1)
            .saturating_sub(self.status_rows().len())
            .saturating_sub(1)
    }

    /// Writes bytes to the session's stream.
    ///
    /// Silently ignores a failure. Presentation must never be able to end a
    /// turn, and a closed stream is not a reason to lose a conversation.
    fn show(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let Ok(slot) = self.live_out.lock() else {
            return;
        };
        let Some(sink) = slot.as_ref() else {
            return;
        };
        if let Ok(mut sink) = sink.lock() {
            let _ = sink.write_all(bytes);
            let _ = sink.flush();
        }
    }

    /// Returns the line being typed and the caret's column within it.
    fn typed_line(&self) -> (String, usize) {
        self.typed
            .lock()
            .map_or_else(|_| (String::new(), 0), |typed| typed.clone())
    }

    /// Draws one frame: the arriving text, a notice, and the line being typed.
    ///
    /// Every writer funnels through here so the line being typed is recorded
    /// once, in one place. Two threads draw during a turn, and a frame drawn
    /// without the current line would put an empty prompt over the correction
    /// the user is halfway through typing.
    ///
    /// `notice` is drawn as the activity line, above the input. A `paced`
    /// frame writes nothing when nothing on screen would change.
    fn draw_frame(&self, notice: Option<&str>, line: &str, column: usize, paced: bool) {
        if self.accessible {
            if let Some(notice) = notice {
                let _frame = self
                    .frame
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                self.show(&transcript::append_lines(&transcript::accessible_lines(&[
                    Entry::notice(notice),
                ])));
            }
            return;
        }
        if let Ok(mut typed) = self.typed.lock() {
            line.clone_into(&mut typed.0);
            typed.1 = column;
        }
        // Cleared before the rows are read, so a delta that lands while this
        // frame is painted marks the next one dirty.
        self.frame_dirty
            .store(false, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut last) = self.last_frame.lock() {
            *last = Some(std::time::Instant::now());
        }
        let mut rows = self.streaming_rows();
        rows.extend(self.live_rows());
        // One lock across painting and writing, so a frame from one thread
        // cannot land inside a frame from the other.
        let Ok(_frame) = self.frame.lock() else {
            return;
        };
        if self
            .transcript_open
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        // Even a frame with nothing in it is painted, because the one before it
        // may have shown a line or a notice that has to be taken off the screen.
        // A frame that changes nothing costs only a caret move.
        let marker = rune_term::shell::prompt();
        let width = usize::from(self.width());
        let (prompt_rows, caret) = transcript::render_draft_at(marker, line, column, width);
        let Ok(painted) = self.paint_with_menu(&[], notice, &prompt_rows, &rows, &[], caret, paced)
        else {
            return;
        };
        self.show(&painted);
    }

    /// Draws the arriving text with the line being typed.
    fn draw_stream_with(&self, line: &str, column: usize) {
        self.draw_frame(None, line, column, false);
    }

    /// Draws the arriving text with the line the user has typed so far.
    fn draw_stream(&self) {
        let (line, column) = self.typed_line();
        self.draw_frame(None, &line, column, false);
    }

    /// Draws the stream frame, at most once per [`FRAME_INTERVAL`].
    ///
    /// A request inside the interval only marks the frame dirty; the input
    /// loop's next idle poll draws it through [`SessionHost::flush_frame`].
    fn request_frame(&self) {
        if self.accessible {
            return;
        }
        if !self.frame_interval_passed() {
            self.frame_dirty
                .store(true, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        let (line, column) = self.typed_line();
        self.draw_frame(None, &line, column, true);
    }

    /// Draws a frame left pending by [`SessionHost::request_frame`], once the
    /// interval has passed.
    fn flush_frame(&self) {
        if self.frame_dirty.load(std::sync::atomic::Ordering::Relaxed)
            && self.frame_interval_passed()
        {
            let (line, column) = self.typed_line();
            self.draw_frame(None, &line, column, true);
        }
    }

    /// Returns whether a stream frame may be written now.
    fn frame_interval_passed(&self) -> bool {
        self.last_frame.lock().map_or(true, |last| {
            last.is_none_or(|last| last.elapsed() >= FRAME_INTERVAL)
        })
    }

    /// Draws one notice above the input, keeping the line being typed.
    fn draw_notice(&self, notice: &str) {
        let (line, column) = self.typed_line();
        self.draw_frame(Some(notice), &line, column, false);
    }

    /// Draws one notice, with the line being typed given explicitly.
    fn draw_notice_with(&self, notice: &str, line: &str, column: usize) {
        self.draw_frame(Some(notice), line, column, false);
    }

    /// Returns the empty input row, with the caret at its start.
    ///
    /// Every frame draws this, so the region is the same shape whether text is
    /// arriving or not and the caret always has a row of its own.
    fn idle_prompt(&self) -> (Vec<String>, (u16, u16)) {
        let marker = rune_term::shell::prompt();
        let row = transcript::render_prompt(marker, "", usize::from(self.width()));
        let caret = rune_term::width::str_width(marker);
        (vec![row], (0, u16::try_from(caret).unwrap_or(u16::MAX)))
    }

    /// Returns the escape that opens the reasoning lane.
    ///
    /// Resolved from the theme once per call and cached on the host, because
    /// `Lanes` holds a `&'static str` and the renderer must not allocate per
    /// line.
    fn reasoning_style(&self) -> String {
        self.reasoning
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Returns the rows the streamed text occupies, newest last.
    fn streaming_rows(&self) -> Vec<String> {
        let Ok(mut streaming) = self.streaming.lock() else {
            return Vec::new();
        };
        let width = usize::from(self.width());
        let dim = self.theme().sgr(Slot::Dim, self.depth);
        let reset = rune_term::engine::Style::RESET;

        // The fields are borrowed separately, because each lane's text and the
        // rows derived from it are written together.
        let StreamingText {
            answer,
            reasoning,
            answer_rows,
            reasoning_rows,
        } = &mut *streaming;

        // Reasoning comes first, drawn apart from the answer, in a secondary
        // colour and indented, so a reader can tell thinking from the reply.
        let mut rows: Vec<String> = Vec::new();
        if !reasoning.trim().is_empty() {
            for line in rows_for(reasoning_rows, reasoning, width) {
                rows.push(format!("{dim}  {line}{reset}"));
            }
        }
        if !answer.trim().is_empty() {
            answer_rows.set_markdown(&self.markdown_style());
            rows.extend(answer_rows.rows_with_ascii(
                answer,
                width.saturating_sub(2).max(1),
                self.ascii,
            ));
        }

        // Every row of the answer is handed over. The renderer owns the region
        // height, because only it knows how tall the region it drew was and
        // therefore which rows it can paint over. Trimming here as well would
        // mean two components each holding a different idea of the limit, and
        // the rows dropped here could not be erased by the frame that follows.
        rows
    }

    /// Saves a delta or retry reset before changing the visible answer.
    fn journal_text(&self, delta: Option<&str>) -> bool {
        let Some(recorder) = &self.recorder else {
            return true;
        };
        let mut recorder = recorder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = match delta {
            Some(text) => recorder.assistant_delta(text),
            None => recorder.reset_assistant(),
        };
        if let Err(error) = result {
            recorder.journal_failed(error);
            self.cancellation.cancel();
            return false;
        }
        true
    }

    /// Clears the streamed text, which a finished step has taken over.
    fn clear_streaming(&self) {
        if let Ok(mut streaming) = self.streaming.lock() {
            streaming.answer.clear();
            streaming.reasoning.clear();
            streaming.answer_rows = transcript::AssistantRows::default();
            streaming.reasoning_rows = LazyRows::default();
        }
    }

    /// Takes the current step's reasoning and narration as transcript entries,
    /// leaving the live text empty.
    fn take_narration(&self) -> Vec<Entry> {
        let Ok(mut streaming) = self.streaming.lock() else {
            return Vec::new();
        };
        let reasoning = std::mem::take(&mut streaming.reasoning);
        let answer = std::mem::take(&mut streaming.answer);
        streaming.answer_rows = transcript::AssistantRows::default();
        streaming.reasoning_rows = LazyRows::default();
        vec![Entry::reasoning(reasoning), Entry::assistant(answer)]
    }

    /// Prints finished entries into scrollback above the live region.
    ///
    /// A turn's tool calls are written as they finish rather than when the
    /// turn ends, so the scrollback reads in the order things happened and a
    /// long turn shows its progress while it runs.
    fn commit(&self, entries: &[Entry]) {
        if entries.is_empty() {
            self.draw_stream();
            return;
        }
        self.commit_lines(&render_entries(entries, self));
    }

    /// Prints finished lines into scrollback above the live region.
    fn commit_lines(&self, lines: &[String]) {
        if self.accessible {
            let _frame = self
                .frame
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.show(&transcript::append_lines(lines));
            return;
        }
        let (line, column) = self.typed_line();
        let mut rows = self.streaming_rows();
        rows.extend(self.live_rows());
        let Ok(_frame) = self.frame.lock() else {
            return;
        };
        if self
            .transcript_open
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let marker = rune_term::shell::prompt();
        let width = usize::from(self.width());
        let (prompt_rows, caret) = transcript::render_draft_at(marker, &line, column, width);
        if let Ok(painted) = self.paint(lines, None, &prompt_rows, &rows, caret) {
            self.show(&painted);
        }
    }

    /// Styles a finished call's row: the status glyph in the success or error
    /// colour, the tool and target as text, and the details after them dimmed.
    fn styled_tool_line(&self, row: &str, failed: bool) -> String {
        let text = transcript::sanitize(row);
        let reset = rune_term::engine::Style::RESET;
        let status = self
            .theme()
            .sgr(if failed { Slot::Error } else { Slot::Success }, self.depth);
        let dim = self.theme().sgr(Slot::Dim, self.depth);
        let (glyph, rest) = text.split_once(' ').unwrap_or((text.as_str(), ""));
        let separator = if self.ascii { " - " } else { " \u{b7} " };
        match rest.split_once(separator) {
            Some((call, details)) => {
                format!("  {status}{glyph}{reset} {call}{dim}{separator}{details}{reset}")
            }
            None => format!("  {status}{glyph}{reset} {rest}"),
        }
    }

    /// Draws the lines a write or an edit removed and added, cut to a few,
    /// under its row. The tool's own preview of the applied change is used
    /// when it sent one; otherwise an edit's arguments are.
    fn change_preview(&self, call: &turn::PreparedCall, output: &ToolOutput) -> Vec<String> {
        let pairs;
        let mut cut = false;
        let lines: Vec<(char, &str)> = if let Some(diff) = &output.diff {
            diff.lines()
                .filter_map(|line| {
                    if line == "..." {
                        cut = true;
                    }
                    let (mark, text) = line.split_at_checked(2)?;
                    match mark {
                        "- " => Some(('-', text)),
                        "+ " => Some(('+', text)),
                        _ => None,
                    }
                })
                .collect()
        } else if call.name == "edit_file" {
            let arguments: serde_json::Value =
                serde_json::from_str(&call.arguments).unwrap_or_default();
            pairs = edit_pairs(&arguments);
            pairs
                .iter()
                .flat_map(|(old, new)| rune_term::diff::changed_lines(old, new))
                .collect()
        } else {
            return Vec::new();
        };
        let sgr = |slot| self.theme().sgr(slot, self.depth);
        let style = rune_term::diff::DiffStyle {
            added: sgr(Slot::Success),
            removed: sgr(Slot::Error),
            dim: sgr(Slot::Dim),
            reset: rune_term::engine::Style::RESET.to_owned(),
        };
        let mut shown = rune_term::diff::render(
            &lines,
            &style,
            EDIT_PREVIEW_LINES,
            usize::from(self.width()),
            self.ascii,
        );
        // The tool cut its preview; say so unless the view already does.
        if cut && lines.len() <= EDIT_PREVIEW_LINES {
            let ellipsis = if self.ascii { "..." } else { "\u{2026}" };
            shown.push(format!("    {}{ellipsis}{}", style.dim, style.reset));
        }
        shown
    }

    /// Returns how Markdown in an answer is drawn with this session's theme.
    ///
    /// Emphasis uses terminal attributes, which a colourless theme still
    /// draws; colours come from the theme, so a theme without them simply
    /// leaves them out.
    fn markdown_style(&self) -> rune_term::markdown::MarkdownStyle {
        let sgr = |slot| self.theme().sgr(slot, self.depth);
        rune_term::markdown::MarkdownStyle {
            enabled: true,
            heading: format!("\u{1b}[1m{}", sgr(Slot::Accent)),
            bold: "\u{1b}[1m".to_owned(),
            italic: "\u{1b}[3m".to_owned(),
            code: sgr(Slot::String),
            link: format!("{}\u{1b}[4m", sgr(Slot::Link)),
            dim: sgr(Slot::Dim),
            reset: rune_term::engine::Style::RESET.to_owned(),
            code_block: rune_term::highlight::CodeStyle {
                keyword: sgr(Slot::Keyword),
                string: sgr(Slot::String),
                number: sgr(Slot::Number),
                comment: sgr(Slot::Comment),
                function: sgr(Slot::Function),
                reset: rune_term::engine::Style::RESET.to_owned(),
            },
        }
    }

    /// Refuses another request once the session has spent its limit.
    ///
    /// Spend is what earlier turns were priced at, so the request that crosses
    /// the limit completes and the one after it is refused.
    fn check_spend(&self) -> Result<()> {
        self.absorb_helper_usage();
        let limit = self.spend_limit.lock().ok().and_then(|limit| *limit);
        let spent = self.totals.lock().ok().and_then(|totals| totals.cost);
        crate::spend::check(spent, limit, "session").map_err(|err| {
            err.with_hint("raise it with /budget <dollars> or session_spend_cents, or start /new")
        })
    }

    /// Returns what a running call is doing, such as `Reading src/app.js`.
    fn running_label(&self, call: &turn::PreparedCall, activity: rune_tools::Activity) -> String {
        let target = self.call_target(call);
        let subject = if target.is_empty() {
            &call.name
        } else {
            &target
        };
        format!("{} {subject}", activity.running_label())
    }

    /// Returns the rows that show work in progress: each running call, or,
    /// while the model has said nothing yet, that it is working.
    ///
    /// Each row carries a turning glyph and the time spent so far, so a long
    /// call or a slow first token is visibly alive rather than a frozen screen.
    fn live_rows(&self) -> Vec<String> {
        let dim = self.theme().sgr(Slot::Dim, self.depth);
        let reset = rune_term::engine::Style::RESET;
        let row = |label: &str, started: std::time::Instant| {
            let elapsed = started.elapsed();
            let frames: &[&str] = if self.ascii {
                &["-", "\\", "|", "/"]
            } else {
                &["\u{25d0}", "\u{25d3}", "\u{25d1}", "\u{25d2}"]
            };
            let step = (elapsed.as_millis() / 150) as usize;
            let frame = frames
                .get(step.checked_rem(frames.len()).unwrap_or_default())
                .copied()
                .unwrap_or_default();
            format!("{dim}  {frame} {label} {}{reset}", format_elapsed(elapsed))
        };
        let running = self
            .running
            .lock()
            .map(|running| {
                running
                    .iter()
                    .map(|call| row(&call.label, call.started))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if !running.is_empty() {
            return running;
        }
        let quiet = self.streaming.lock().is_ok_and(|streaming| {
            streaming.answer.trim().is_empty() && streaming.reasoning.trim().is_empty()
        });
        let started = self.turn_started.lock().ok().and_then(|started| *started);
        match started {
            Some(started) if quiet => vec![row("thinking", started)],
            _ => Vec::new(),
        }
    }

    /// Redraws the live region while a turn is quiet, at most four times a
    /// second, so the spinner and the elapsed time move. The pause between
    /// redraws is long enough that a reader waiting for a quiet terminal,
    /// such as a PTY harness, still sees one while a turn is held.
    fn tick(&self) {
        if self.accessible {
            return;
        }
        let live = self
            .turn_started
            .lock()
            .is_ok_and(|started| started.is_some());
        if !live {
            return;
        }
        let Ok(mut last) = self.last_tick.lock() else {
            return;
        };
        if last.is_some_and(|last| last.elapsed() < TICK_INTERVAL) {
            return;
        }
        *last = Some(std::time::Instant::now());
        drop(last);
        self.request_frame();
    }
    /// Returns the scrollback row for a call that has finished.
    fn finished_row(
        &self,
        call: &turn::PreparedCall,
        output: &ToolOutput,
        elapsed: std::time::Duration,
    ) -> String {
        let glyphs = if self.accessible {
            RowGlyphs::Words
        } else if self.ascii {
            RowGlyphs::Ascii
        } else {
            RowGlyphs::Unicode
        };
        tool_row(&call.name, &self.call_target(call), output, elapsed, glyphs)
    }

    /// Returns what a call acts on, such as a path or a command, cut to fit.
    fn call_target(&self, call: &turn::PreparedCall) -> String {
        let arguments = serde_json::from_str(&call.arguments).unwrap_or_default();
        let target =
            turn::permission_target_for(&self.registry, &call.name, &arguments).unwrap_or_default();
        let width = usize::from(self.width()).saturating_sub(40).clamp(16, 72);
        shorten(target.lines().next().unwrap_or_default(), width, self.ascii)
    }

    /// Clears retry state when a request advances or the turn is finalized.
    fn clear_provider_retry(&self) -> bool {
        self.provider_retry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .is_some()
    }

    /// Returns the sanitized answer currently visible in the live transcript.
    fn partial_answer(&self) -> String {
        self.streaming
            .lock()
            .map(|streaming| streaming.answer.clone())
            .unwrap_or_default()
    }

    /// Drops the events of the turn that just finished.
    fn clear_events(&self) {
        if let Ok(mut events) = self.events.lock() {
            events.clear();
        }
        if let Ok(mut running) = self.running.lock() {
            running.clear();
        }
        if let Ok(mut started) = self.turn_started.lock() {
            *started = None;
        }
    }
}

/// Runs an interactive session.
///
/// Returns the exit code the process should use.
pub fn run<R: BufRead, W: std::io::Write + Send + 'static>(
    mut config: SessionConfig,
    input: R,
    output: W,
) -> Result<u8> {
    // Every width measured from here on, the first frame included, has to agree
    // with how the terminal draws ambiguous characters.
    rune_term::width::set_ambiguous_width(config.settings.ambiguous_width);
    let mut limits = config.settings.limits.clone();
    let model_limits = crate::model_limits::resolve(
        &config.settings,
        &config.paths,
        &config.settings.model,
        None,
    );
    // Only a window someone declared sizes the catalog. The compiled default
    // is a guess, and deriving from it would shrink the catalog for every model
    // that states no window rather than keeping the documented fallback.
    adopt_skill_catalog_budget(&mut limits, config.settings.context_window);
    let connection =
        crate::mcp::connect_interactive(&config.settings, &config.paths, &config.workspace)?;
    let (mcp, mcp_warnings) = match connection {
        Some(connection) => (
            crate::mcp::attach(
                connection.client,
                &config.settings,
                &mut config.registry,
                Arc::clone(&config.catalog),
                config.questions.cancellation.clone(),
            )?,
            connection.warnings,
        ),
        None => (None, Vec::new()),
    };
    let mut tools = inventory::advertisement(&config.registry);
    if let Some(client) = &mcp {
        crate::mcp::append_tools(&mut tools, client)?;
    }
    let mut prompt = build_prompt(
        &config.workspace,
        &config.paths.config_root,
        &limits,
        &tool_names(&tools),
        config.settings.experiments.prompt,
    );
    let nested_rules = std::mem::take(&mut prompt.nested);
    let instruction_sizes = crate::ask::instruction_sizes(&prompt);

    // A resumed session continues its stored conversation; a new one starts
    // empty and writes a fresh log.
    let (recorder, mut history) = if let Some(id) = &config.resume {
        session_log::load(&config.paths, id)?
    } else {
        let id = SessionId::generate();
        let recorder = Recorder::create(&config.paths, &id)?;
        // Recorded before the first turn, so the session is attributable to its
        // workspace even if the run ends immediately.
        recorder.set_workspace(&config.workspace)?;
        (recorder, History::new())
    };
    // Pins added from here on reach the model as updates, not in the prefix.
    history.freeze_pins();
    let results_notice = attach_results(&mut history, &config.paths, &recorder.id());

    // Opened once. A history that failed to load is not fatal: recall is a
    // convenience, and losing it must not cost the session.
    let mut history_file = crate::prompt_history::History::open(&config.paths).ok();

    // Discovered once, because a command file that changes mid-session would
    // otherwise make an invocation mean two different things.
    let commands =
        rune_context::commands::discover(&config.workspace, &Paths::from_process().config_root)
            .unwrap_or_default();

    // Enable raw input before announcing the session. Otherwise input sent in
    // response to the banner can have its Enter translated by canonical mode
    // before the key reader starts, leaving a complete prompt unsubmitted.
    let mut reader = if config.accessible {
        rune_term::input::KeyReader::line_mode()
    } else {
        rune_term::input::KeyReader::new()
    };
    let keyed = reader.is_active();

    let probe = rune_term::probe::run(
        TerminalEnv::from_lookup(|k| std::env::var(k).ok()),
        keyed && !config.accessible && std::env::var("RUNE_NO_PROBE").as_deref() != Ok("1"),
    );
    reader.type_ahead(&probe.typed);

    // Resolved before the host literal because the registry is moved into it,
    // and because the reasoning lane's colour has to be read off the theme
    // before the theme itself moves into the host.
    let (theme, theme_warning) =
        resolve_theme(&config, rune_term::theme::terminal_is_light(&probe.caps));
    let depth = detect_depth(
        &config.settings.color,
        std::io::stdout().is_terminal(),
        |key| std::env::var(key).ok(),
    );
    let reasoning_escape = theme.sgr(Slot::Dim, depth);

    // Copied before the endpoint moves into the host, so `/status` and the
    // picker can name the address and the provider without the credential that
    // travels beside the endpoint.
    let endpoint_url = config.endpoint.base_url.clone();
    let provider_name = config.settings.provider.to_string();

    let (initial_context, context_source) = recorder.resumed_context();
    let (transcript, initial_totals) = if let Some(id) = &config.resume {
        let state = session_log::inspect(&config.paths, id)?;
        (
            resumed_entries(&history, &state),
            Totals::from_saved(&state),
        )
    } else {
        (Vec::new(), Totals::default())
    };
    let recorder = Arc::new(Mutex::new(recorder));
    let mut context = ExecutionContext::new(config.workspace.clone())
        .with_offline(config.settings.offline)
        .with_allow_unsandboxed(config.settings.allow_unsandboxed)
        .with_environment(crate::provider_setup::command_environment(&config.settings))
        .with_cache_roots(crate::sandbox::cache_paths(&config.settings));
    context
        .additional_roots
        .clone_from(&config.settings.additional_directories);
    // A resumed session with the same provider and model opens with the
    // instructions and tools it was started with, so the provider's cached
    // prefix still applies; changes on disk since reach it as updates.
    let saved_prefix = config.resume.as_ref().and_then(|id| {
        crate::session_prefix::load(&config.paths, id).filter(|saved| {
            saved.fits(
                config.settings.provider.as_str(),
                &config.settings.model,
                |name| {
                    tools.iter().any(|tool| tool.name == name)
                        || config.registry.get(name).is_some()
                },
            )
        })
    });
    let reuse_prefix = saved_prefix.is_some();
    let opening = match saved_prefix {
        Some(saved) => Opening {
            mapped: prompt::MappedInstructions::built(format!(
                "{}{}",
                saved.instructions, saved.map
            )),
            watched: context_watch::Watched::seeded(
                &saved.instruction_files,
                saved.skills,
                saved.test_command,
            ),
            instructions: saved.instructions,
            stable_len: saved.stable_len,
            tools: saved.tools,
        },
        None => Opening {
            instructions: prompt.instructions,
            stable_len: prompt.stable_len,
            tools,
            mapped: prompt::MappedInstructions::default(),
            watched: context_watch::Watched::of(&config.workspace, &config.paths.config_root),
        },
    };
    let host = SessionHost {
        cache_key: std::sync::OnceLock::new(),
        usage_paths: Some(config.paths.clone()),
        pre_tool_hook: config.settings.pre_tool_hook.clone(),
        post_tool_hook: config.settings.post_tool_hook.clone(),
        accessible: config.accessible,
        ascii: config.settings.ascii,
        recorder: Some(Arc::clone(&recorder)),
        endpoint: config.endpoint,
        compat: crate::provider_setup::CompatSource::from_settings(&config.settings),
        dialect: config.dialect,
        model: Mutex::new(config.settings.model.clone()),
        instructions: Mutex::new(opening.instructions),
        stable_instructions_len: std::sync::atomic::AtomicUsize::new(opening.stable_len),
        frozen_objective: Mutex::new(FrozenObjective::Unread),
        pending_updates: Mutex::new(Vec::new()),
        watched: Mutex::new(opening.watched),
        nested_rules: Mutex::new(nested_rules),
        instruction_sizes: Mutex::new(instruction_sizes),
        mapped: opening.mapped,
        tools: opening.tools,
        rules: Mutex::new(config.rules),
        mode: Mutex::new(config.settings.permission_mode),
        permission_settings: config.settings.clone(),
        effort: Mutex::new(config.settings.effort),
        effort_set: std::sync::atomic::AtomicBool::new(
            config.settings.source_of("effort") != rune_core::config::Layer::Default,
        ),
        effort_noticed: Mutex::new(std::collections::BTreeSet::new()),
        fast_mode: config.settings.fast_mode,
        limits: limits.clone(),
        context,
        registry: config.registry,
        mcp,
        subagent: config.subagent,
        cancellation: config.questions.cancellation.clone(),
        steering: SteeringQueue::from_limits(&limits),
        events: Arc::new(Mutex::new(Vec::new())),
        goal_judge: std::sync::atomic::AtomicBool::new(false),
        provider_retry: Mutex::new(None),
        context_used: std::sync::atomic::AtomicU64::new(initial_context),
        prices: Mutex::new(None),
        vision: std::sync::atomic::AtomicBool::new(false),
        tool_images: crate::attachments::ToolImages::default(),
        network: std::sync::atomic::AtomicBool::new(config.settings.shell_network),
        spend_limit: Mutex::new(
            config
                .settings
                .limits
                .get(rune_core::budget::LimitName::SessionSpendCents)
                .value()
                .map(|cents| cents.saturating_mul(10_000)),
        ),
        output_limit: std::sync::atomic::AtomicU64::new(
            model_limits.max_output.unwrap_or_default(),
        ),
        context_source: Mutex::new(context_source),
        context_limit: std::sync::atomic::AtomicU64::new(
            model_limits.context_window.unwrap_or_default(),
        ),
        theme: Mutex::new(theme),
        theme_name: Mutex::new(
            config
                .settings
                .theme
                .clone()
                .unwrap_or_else(|| "auto".to_owned()),
        ),
        session_id: Mutex::new(
            recorder
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .id()
                .to_string(),
        ),
        workspace: config.workspace.to_string(),
        caps: probe.caps,
        depth,
        // Seeded from the terminal and refreshed on every frame, so a window
        // resized while the session runs is picked up.
        width: std::sync::atomic::AtomicU16::new(terminal_width()),
        height: std::sync::atomic::AtomicU16::new(terminal_height()),
        pinned_size: None,
        inline: Mutex::new(rune_term::inline::Inline::new(terminal_width())),
        frame: Mutex::new(()),
        transcript: Mutex::new(transcript),
        transcript_history_len: std::sync::atomic::AtomicUsize::new(history.len()),
        transcript_open: std::sync::atomic::AtomicBool::new(false),
        typed: Mutex::new((String::new(), 0)),
        provider_order: config.settings.provider_order.clone(),
        provider_strict: config.settings.provider_strict,
        streaming: Arc::new(Mutex::new(StreamingText::default())),
        running: Mutex::new(Vec::new()),
        last_tick: Mutex::new(None),
        frame_dirty: std::sync::atomic::AtomicBool::new(false),
        last_frame: Mutex::new(None),
        turn_started: Mutex::new(None),
        live_out: Arc::new(Mutex::new(None)),
        reasoning: Mutex::new(reasoning_escape),
        reviewer: crate::auto_review::build(
            &config.settings,
            &config.paths,
            config.helper_usage.clone(),
        )?,
        helper_usage: config.helper_usage.clone(),
        totals: Mutex::new(initial_totals),
        undo: Mutex::new(BTreeMap::new()),
        review_session: Arc::new(Mutex::new(ReviewSession::new(&limits))),
        approval_requests: Mutex::new(None),
        approval_preview: Mutex::new(Vec::new()),
        grants: Mutex::new(Grants::new()),
        last_plan: Mutex::new(None),
        warming: Arc::default(),
        warm_interval: crate::cache_warming::WARM_INTERVAL,
        pending_model: Mutex::new(None),
        ignored_reported: Mutex::new(std::collections::BTreeSet::new()),
        planning: std::sync::atomic::AtomicBool::new(false),
        unsandboxed: config.settings.allow_unsandboxed && !rune_exec::detect().support().is_full(),
        questions: config.questions,
    };
    if !reuse_prefix {
        host.save_prefix();
    }
    if let Ok(mut inline) = host.inline.lock() {
        inline.set_synchronized(host.caps.env.term.as_deref() != Some("dumb"));
    }
    if context_source.is_none() {
        host.estimate_context(&history)?;
    }

    // The stream is shared: the loop writes to it, and the host writes to it
    // while a turn is running so streamed text appears as it arrives. Sharing
    // one lock rather than nesting two is what keeps those writes ordered.
    let out: LiveSink = Arc::new(Mutex::new(output));
    if let Ok(mut slot) = host.live_out.lock() {
        *slot = Some(Arc::clone(&out));
    }

    // The context window is resolved before anything is drawn. The session used
    // to announce itself first and correct the figure a moment later, which
    // meant the status line showed the compiled default for as long as the
    // lookup took and then changed under the reader. A wrong number that
    // corrects itself is worse than a number that arrives late: it is
    // indistinguishable from a right one while it is on screen.
    if !config.settings.offline {
        resolve_context_window(&host, &config.settings, &config.paths);
    }
    crate::provider_setup::refresh_catalog_in_background(&config.settings, &config.paths);

    // The session identifier is announced up front so a resumed-or-new session
    // can be named later without consulting the listing. It goes through the
    // renderer like everything else, so the rows it occupies are known to the
    // component that later redraws over them.
    {
        let banner = format!(
            "session {}",
            recorder
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .id()
        );
        let mut opening = if config.accessible {
            transcript::accessible_lines(&[
                Entry::notice(banner),
                Entry::notice("accessible mode: enter one line per prompt; /exit quits"),
            ])
        } else {
            vec![banner]
        };
        opening.extend(
            theme_warning
                .iter()
                .chain(mcp_warnings.iter())
                .chain(results_notice.iter())
                .flat_map(|warning| render_entries(&[Entry::notice(warning)], &host)),
        );
        // Replay only for an interactive resume. These settled rows enter the
        // terminal's scrollback once, before the composer accepts any input.
        if (keyed || config.accessible) && config.resume.is_some() {
            let entries = host
                .transcript
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            opening.extend(render_entries(&entries, &host));
        }
        // Text typed while the terminal was queried is the first draft.
        let (prompt_row, caret) = if reader.line().is_empty() {
            host.idle_prompt()
        } else {
            transcript::render_draft_at(
                rune_term::shell::prompt(),
                reader.line(),
                reader.column(),
                usize::from(host.width()),
            )
        };
        let painted = host.paint(&opening, None, &prompt_row, &[], caret)?;
        if let Ok(mut sink) = out.lock() {
            sink.write_all(&painted)?;
            sink.flush()?;
        }
    }

    // A resumed session keeps the title it was given.
    let mut is_first_prompt = config.resume.is_none()
        && recorder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .title_is_unset();

    // The Escape gesture spans presses, so it outlives a single read.
    let mut cancellation_gesture = rune_term::shell::EscapeGesture::default();

    // A session with no model asks for one now, before the first turn, because
    // a turn cannot be sent without an identifier. A terminal drives the picker,
    // and a caller that cannot answer is refused here rather than at the
    // endpoint, where an empty model comes back as an opaque protocol error.
    if !config.settings.has_model() {
        if !keyed {
            return Err(rune_core::config::unconfigured_model_error());
        }
        choose_model(&mut reader, &host, &out, &config.settings, &config.paths)?;
        if !host.has_model() {
            // The picker was cancelled, so there is still nothing to send to.
            return Err(rune_core::config::unconfigured_model_error());
        }
    }

    // Held rather than read off the config at the point of use, because the
    // input closure borrows the config and a model chosen mid-session has to be
    // able to build a catalog from the same settings the session started with.
    let settings = config.settings.clone();
    let mut completion_context = ExecutionContext::new(config.workspace.clone());
    for root in &settings.additional_directories {
        completion_context = completion_context.with_root(root.clone());
    }
    let completion_limits = rune_tools::workspace::FileLimits::from_budget(&limits);
    let completions = command_candidates(&commands.commands);

    // The prompts already recorded in this workspace, oldest first, which is
    // the order the up arrow walks backwards through. Refreshed after each
    // turn so a prompt typed now is recallable next, and read from the file
    // rather than kept beside it so there is one source of truth.
    let mut recall = recorded_prompts(history_file.as_ref(), &config.workspace);

    let mut source = rune_term::shell::StdinSource::new(input);

    // One handler for both input paths, so a keystroke and a piped line mean
    // exactly the same thing.
    //
    // The model commands are reported to the caller rather than acted on here,
    // because choosing one needs the input reader to run a picker and the
    // status line to be redrawn, both of which live outside this closure.
    //
    // The recorded prompts are passed in rather than captured, so the loop that
    // owns them can re-read them between submissions for the up arrow.
    let mut handle_input = |input: Input,
                            sink: &mut LockedSink,
                            history_file: &mut Option<crate::prompt_history::History>,
                            reader: &mut rune_term::input::KeyReader,
                            gesture: &mut rune_term::shell::EscapeGesture,
                            line_input: Option<&mut dyn InputSource>|
     -> Result<Step> {
        match input {
            Input::Command { name, arguments } => {
                // Built here rather than captured, so the status a command
                // reports is the state at the moment it runs.
                host.absorb_helper_usage();
                let info = host.info(&provider_name, &endpoint_url);
                // Command output is buffered rather than written straight out,
                // because it has to be committed through the renderer: text
                // written directly lands at the caret, inside the region the
                // session repaints, and the screen then shows a mix of the two.
                let mut captured: Vec<u8> = Vec::new();
                let handled = handle_command(
                    &name,
                    &arguments,
                    &commands,
                    history_file.as_ref(),
                    &config.workspace,
                    &info,
                    &mut captured,
                )?;
                if name == "help" {
                    let _ = writeln!(
                        captured,
                        "\n{}",
                        rune_term::input::render_bindings_help(reader.is_active())
                    );
                }
                let mut note = |text: String| captured.extend_from_slice(text.as_bytes());
                // A model chosen by a command belongs to the prompt right after
                // it, so any other command in between drops it.
                lock_value(&host.pending_model).take();
                if ends_warming(&handled) {
                    host.stop_warming();
                }
                match handled {
                    Handled::Exit => return Ok(Step::Exit),
                    Handled::PickModel => return Ok(Step::PickModel),
                    Handled::Theme(None) => return Ok(Step::PickTheme),
                    Handled::Theme(Some(name)) => {
                        note(set_theme_named(&host, &config.paths, &name));
                    }
                    Handled::Copy if host.accessible => {
                        note("accessible mode: copy the reply from terminal scrollback".to_owned());
                    }
                    Handled::Copy => {
                        // The reply is read from the conversation rather than
                        // from the screen, because what is on screen has been
                        // wrapped and styled and is no longer the text.
                        match last_reply(&history) {
                            Some(reply) => match copy_to_clipboard(&reply) {
                                Ok(()) => note(format!(
                                    "copied {} character(s) to the clipboard",
                                    reply.chars().count()
                                )),
                                Err(err) => note(format!("could not copy: {err}")),
                            },
                            None => note("there is no reply to copy yet".to_owned()),
                        }
                    }
                    Handled::NewSession => {
                        // Handled here because the recorder and the conversation
                        // both live in this closure, and starting a fresh
                        // conversation means replacing both together.
                        let id = SessionId::generate();
                        let fresh = Recorder::create(&config.paths, &id)?;
                        fresh.set_workspace(&config.workspace)?;
                        *recorder
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = fresh;
                        history = History::new();
                        if let Some(notice) = attach_results(&mut history, &config.paths, &id) {
                            note(notice);
                        }
                        host.transcript_history_len
                            .store(0, std::sync::atomic::Ordering::Relaxed);
                        host.transcript
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .clear();
                        host.set_session_id(id.as_str());
                        // Approvals were given for the conversation that ended.
                        if let Ok(mut grants) = host.grants.lock() {
                            grants.clear();
                        }
                        // The context reading belongs to the conversation that
                        // just ended, so it is cleared rather than carried into
                        // a session that has sent nothing.
                        host.forget_context();
                        host.estimate_context(&history)?;
                        note(format!("started session {id}"));
                    }
                    Handled::Fork => {
                        // The copy has the same history, so only the recorder
                        // changes: the turns that follow are saved to the copy,
                        // and the earlier session stays as it was.
                        let mut current = recorder
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let source = current.id();
                        let forked = current.fork(&config.paths).and_then(|id| {
                            let opened = Recorder::open(&config.paths, &id)?;
                            opened.set_workspace(&config.workspace)?;
                            Ok((id, opened))
                        });
                        match forked {
                            Ok((id, opened)) => {
                                *current = opened;
                                drop(current);
                                host.set_session_id(id.as_str());
                                note(format!(
                                    "forked into session {id}; session {source} is unchanged"
                                ));
                            }
                            Err(err) => {
                                drop(current);
                                note(format!("could not fork: {err}"));
                            }
                        }
                    }
                    Handled::Undo => return Ok(Step::Undo),
                    Handled::UndoPreview => {
                        flush_lines(&host, sink, &host.preview_undo())?;
                        return Ok(Step::Continue);
                    }
                    Handled::Reload => {
                        host.rebuild_prefix();
                        history.freeze_pins();
                        host.forget_context();
                        note(
                            "instructions reloaded; the next request rebuilds the cached prefix"
                                .to_owned(),
                        );
                    }
                    Handled::Context => {
                        for row in context_view(&host, &history)? {
                            note(row);
                            note("\n".to_owned());
                        }
                    }
                    Handled::Compact => {
                        // Handled here because this closure already mutably
                        // borrows the conversation, and a second borrower would
                        // not compile for the right reason.
                        return compact_history(&host, &mut history, sink);
                    }
                    Handled::CompactPreview => {
                        return preview_compaction(&host, &history, sink);
                    }
                    Handled::Pin(constraint) => {
                        if let Some(constraint) = constraint {
                            let mut candidate = history.clone();
                            if candidate.pin_constraint(&constraint)? {
                                recorder
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .pin_constraint(&constraint)?;
                                history = candidate;
                                host.queue_update(format!("Pinned constraint: {constraint}"));
                                host.forget_context();
                                host.estimate_context(&history)?;
                            }
                        }
                        if history.pinned_constraints().is_empty() {
                            note("no pinned constraints; usage: /pin <constraint>".to_owned());
                        } else {
                            note(format!(
                                "pinned constraints:\n{}",
                                history.pinned_constraints().join("\n")
                            ));
                        }
                    }
                    Handled::Rename(title) => {
                        // The input loop and stream observer share one recorder;
                        // its lock serializes every write to the session log.
                        recorder
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .set_title(&session_log::derive_title(&title))?;
                        note(format!("renamed this session to {title}"));
                    }
                    Handled::Goal(request) => {
                        let mut recorder = recorder
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let mut refused = false;
                        if let Some(request) = request {
                            if let Some(objective) = &request.objective {
                                if let Some(budget) = request.budget {
                                    recorder.set_budgeted_objective(objective, budget)?;
                                } else {
                                    recorder.set_objective(objective)?;
                                }
                                host.queue_update(format!("Session objective: {objective}"));
                            }
                            refused = request.judge && recorder.objective().is_none();
                            host.goal_judge.store(
                                request.judge && !refused,
                                std::sync::atomic::Ordering::Relaxed,
                            );
                        }
                        if refused {
                            note(GOAL_JUDGE_USAGE.to_owned());
                        } else {
                            note(recorder.objective().map_or_else(
                                || "no active objective".to_owned(),
                                |objective| format!("active objective: {objective}"),
                            ));
                            if let Some((budget, tokens)) = recorder.objective_budget() {
                                note(format!(
                                    "\nobjective tokens: {tokens}/{budget}; {} remaining",
                                    budget.saturating_sub(tokens)
                                ));
                            }
                            if recorder.objective().is_some()
                                && host.goal_judge.load(std::sync::atomic::Ordering::Relaxed)
                            {
                                note("\njudge: on".to_owned());
                            }
                        }
                    }
                    Handled::SetModel(model) => {
                        // Named inline, so no listing describes it; a window no
                        // other source states leaves the one in force.
                        let resolved =
                            crate::model_limits::resolve(&settings, &config.paths, &model, None);
                        host.set_model(&model, resolved.context_window, resolved.max_output, None);
                        note(format!("model set to {}", host.model_name()));
                    }
                    Handled::Effort(requested) => note(set_effort(&host, requested.as_deref())),
                    Handled::Permissions(requested) => {
                        note(set_permissions(&host, requested.as_deref()));
                    }
                    Handled::Budget(requested) => note(set_budget(&host, requested.as_deref())),
                    Handled::Network(requested) => note(set_network(&host, requested.as_deref())),
                    Handled::Plan(argument) => {
                        let leave = argument.as_deref() == Some("off");
                        host.planning
                            .store(!leave, std::sync::atomic::Ordering::Relaxed);
                        if leave {
                            note("plan mode is off; changes and commands may run again".to_owned());
                        } else {
                            note(
                                "plan mode is on: only reads and plan updates run; /plan off to act on the plan"
                                    .to_owned(),
                            );
                            if let Some(task) = argument {
                                reader.replace(&task);
                            }
                        }
                    }
                    Handled::ClearHistory => {
                        // Forgetting is reported, because a silent success would
                        // leave the user unsure whether anything was removed.
                        match history_file.as_mut() {
                            Some(history) => {
                                let _ = history.clear();
                                note("forgot every recorded prompt".to_owned());
                            }
                            None => note("no prompt history is available".to_owned()),
                        }
                    }
                    // Expanded text goes to the composer for review, never
                    // straight to the model: a template with a wrong argument
                    // should be visible before it is sent.
                    Handled::Expand { text, model } => {
                        // Reported once per session, so a command used often
                        // does not repeat what it already said.
                        let ignored = commands
                            .commands
                            .iter()
                            .find(|command| command.name == name)
                            .and_then(rune_context::commands::Command::ignored_notice);
                        if let Some(line) = ignored
                            && lock_value(&host.ignored_reported).insert(name.clone())
                        {
                            host.transcript
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .push(Entry::notice(&line));
                            note(format!("{line}\n"));
                        }
                        match model {
                            Some(model) => {
                                note(format!(
                                    "-- /{name} expanded; edit before sending; the turn uses model {model} --\n{text}"
                                ));
                                *lock_value(&host.pending_model) = Some(model);
                            }
                            None => note(format!(
                                "-- /{name} expanded; edit before sending --\n{text}"
                            )),
                        }
                    }
                    Handled::Continue => {}
                }
                let lines = output_lines(&captured);
                flush_lines(&host, sink, &lines)?;
                Ok(Step::Continue)
            }
            Input::Prompt(text) => {
                // Held for the whole arm, so every way the turn ends puts the
                // session model back.
                let pending = lock_value(&host.pending_model).take();
                let _turn_model = pending.map(|model| TurnModel::apply(&host, &model));
                // The submitted line is committed to the flow before the turn
                // runs, so what the user typed stays on screen once the reply
                // replaces the region it was typed in.
                let echo = if host.accessible {
                    transcript::accessible_lines(&[Entry::user(&text)])
                } else {
                    vec![transcript::render_prompt_with_ascii(
                        rune_term::shell::prompt(),
                        &text,
                        usize::from(host.width()),
                        host.ascii,
                    )]
                };
                // The typed line is committed and an empty input row is drawn
                // under it, so the caret has its own row from the moment the
                // line is submitted rather than only once streaming starts.
                let (prompt_row, caret) = host.idle_prompt();
                let painted = host.paint(&echo, None, &prompt_row, &[], caret)?;
                sink.write_all(&painted)?;
                sink.flush()?;

                if is_first_prompt {
                    recorder
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .set_title(&session_log::derive_title(&text))?;
                    is_first_prompt = false;
                }
                // Recorded before the turn runs, so a prompt that is interrupted
                // is still recallable.
                if let Some(history) = history_file.as_mut() {
                    let entry = crate::prompt_history::Entry::new(text.clone()).located(
                        &config.workspace,
                        recorder
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .id()
                            .as_str(),
                    );
                    let _ = history.record(entry);
                }
                // Changes since the last request ride on this message, so
                // the prefix the provider has cached stays as it was.
                host.watch_before_prompt();
                let updates = host.take_updates();
                recorder
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .user_message(&text)?;
                // The notes are journaled apart from the user's words, so a
                // resumed session rebuilds this exact message.
                let at = u64::try_from(history.len()).unwrap_or(u64::MAX);
                let mut composed = text.clone();
                if !updates.is_empty() {
                    let update = turn::context_update(&updates);
                    composed = format!("{composed}\n\n{update}");
                    host.journal(&turn::JournalEntry::Note {
                        at,
                        placement: turn::NotePlacement::PromptSuffix,
                        text: update,
                    });
                }
                if host.planning.load(std::sync::atomic::Ordering::Relaxed) {
                    composed = format!("{PLAN_MODE_NOTE}\n\n{composed}");
                    host.journal(&turn::JournalEntry::Note {
                        at,
                        placement: turn::NotePlacement::PromptPrefix,
                        text: PLAN_MODE_NOTE.to_owned(),
                    });
                }
                history.push_user(composed);
                host.transcript
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(Entry::user(text));
                // The turn runs on its own thread so this one keeps reading
                // keys. A turn that ran inline made the keyboard dead for as
                // long as the model took, which is the difference between
                // correcting a long turn and waiting it out.
                host.transcript_history_len
                    .store(history.len(), std::sync::atomic::Ordering::Relaxed);
                recorder
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .begin_turn()?;
                let steered = if let Some(input) = line_input {
                    run_turn_accessible(&mut history, &host, input)
                } else {
                    run_turn_steerable(&mut history, &host, reader, gesture)
                };
                host.absorb_helper_usage();
                recorder
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .check_journal()?;
                // A correction the turn took in is part of the conversation the
                // model saw, so a resumed session must see it too.
                for correction in &steered.applied {
                    recorder
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .user_message(correction)?;
                }
                let outcome = match steered.outcome {
                    Ok(outcome) => outcome,
                    // A cancelled or failed turn ends that exchange, not the
                    // session. Cancelling is something the keyboard offers while
                    // a turn runs, so ending the session for it would make the
                    // gesture indistinguishable from quitting. A pipe keeps the
                    // failure, because a script reads it from the exit status.
                    Err(err) => {
                        let partial = host.partial_answer();
                        // Save before clearing the live answer or reporting
                        // the boundary, just as for a completed exchange.
                        if err.code() == ErrorCode::Cancelled {
                            recorder
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .cancelled_turn(&partial)?;
                        } else {
                            recorder
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .failed_turn(&partial, &err)?;
                        }
                        let history_start = host
                            .transcript_history_len
                            .load(std::sync::atomic::Ordering::Relaxed)
                            .min(history.len());
                        retain_partial_answer(&mut history, history_start, &partial);
                        host.capture_history(&history);
                        {
                            let mut transcript = host
                                .transcript
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            if let Some(diagnostic) = &steered.diagnostic {
                                transcript.push(Entry::notice(diagnostic));
                            }
                            transcript.push(Entry::notice(err.message()));
                        }
                        if !reader.is_active() && !host.accessible {
                            return Err(err);
                        }
                        let lines = report_failed_turn(&err, &host, steered.diagnostic.as_deref());
                        host.clear_events();
                        host.clear_streaming();
                        host.clear_provider_retry();
                        close_turn(&host, sink, reader, &lines, None, &steered.unsent)?;
                        return Ok(Step::Continue);
                    }
                };
                // The turn is recorded before it is reported, so a session that
                // dies while rendering still has its exchange on disk.
                recorder
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .turn(&outcome)?;
                host.capture_history(&history);
                // The model is read back rather than captured at start, so a
                // turn that ran after `/model` is billed to the model that ran.
                let cost = host
                    .prices
                    .lock()
                    .ok()
                    .and_then(|prices| *prices)
                    .and_then(|prices| {
                        prices.cost_at(&outcome.usage, rune_net::transport::now_timestamp())
                    });
                record_usage(&config.paths, &host.model(), &outcome, cost);
                if let Ok(mut totals) = host.totals.lock() {
                    if let Some(cost) = cost {
                        totals.cost = Some(totals.cost.unwrap_or_default().saturating_add(cost));
                    }
                    totals.record_model(&host.model(), &outcome.usage, cost);
                }
                host.record_usage(&outcome.usage);
                host.record_context_usage(&history, &outcome.last_request)?;
                let elapsed = host
                    .turn_started
                    .lock()
                    .ok()
                    .and_then(|started| *started)
                    .map(|started| started.elapsed());
                host.clear_events();
                // The streamed text has been superseded by the finished turn,
                // so it is taken before the render that replaces it; only the
                // last step's reasoning is still unprinted.
                let reasoning = host
                    .take_narration()
                    .into_iter()
                    .find(|entry| entry.speaker == transcript::Speaker::Reasoning)
                    .map(|entry| entry.text)
                    .unwrap_or_default();
                // A finished turn is printed once and becomes part of the
                // terminal's own scrollback, so it stays readable with the
                // terminal's search and copy. Nothing else writes to the
                // terminal: the region below is redrawn in place.
                let mut lines = report_turn(&outcome, &host, &reasoning)?;
                if !host.accessible
                    && let Some(summary) =
                        turn_summary(&outcome, plan_progress(&history), elapsed, cost, host.ascii)
                {
                    let dim = host.theme().sgr(Slot::Dim, host.depth);
                    let reset = rune_term::engine::Style::RESET;
                    lines.push(format!("{dim}  {summary}{reset}"));
                }
                let activity = if host.accessible {
                    None
                } else {
                    SessionHost::activity_line(&outcome)
                };
                if outcome.stop_reason != StopReason::Cancelled {
                    host.start_warming();
                }
                close_turn(
                    &host,
                    sink,
                    reader,
                    &lines,
                    activity.as_deref(),
                    &steered.unsent,
                )?;
                Ok(Step::Continue)
            }
            Input::Empty => Ok(Step::Continue),
        }
    };

    let reason = if keyed {
        // Each keystroke redraws the prompt, so the line and the cursor follow
        // what was typed rather than waiting for the terminal to decide the
        // line is finished.
        let mut reason = ExitReason::EndOfInput;
        while let Some(input) = await_submission(
            &completions,
            &mut reader,
            &host,
            &out,
            &recall,
            &completion_context,
            &completion_limits,
        )? {
            let mut sink = LockedSink {
                stream: Arc::clone(&out),
            };
            match handle_input(
                input,
                &mut sink,
                &mut history_file,
                &mut reader,
                &mut cancellation_gesture,
                None,
            )? {
                Step::Exit => {
                    reason = ExitReason::Requested;
                    break;
                }
                Step::PickModel => {
                    choose_model(&mut reader, &host, &out, &settings, &config.paths)?;
                }
                Step::PickTheme => {
                    let line = choose_theme(&mut reader, &host, &out, &config.paths)?;
                    if let Some(line) = line {
                        flush_lines(&host, &mut sink, &[line])?;
                    }
                }
                Step::Undo => {
                    report_undo(&host, &mut sink)?;
                }
                Step::Continue => {}
            }
            // Re-read so a prompt submitted above is recallable with the up
            // arrow. The file is the source of truth, so it is read rather than
            // appended to a copy that could drift from it.
            recall = recorded_prompts(history_file.as_ref(), &config.workspace);
        }
        reason
    } else if config.accessible {
        loop {
            host.show(&transcript::append_lines(&[
                "Input: enter a prompt or slash command".to_owned(),
            ]));
            let Some(line) = source.next_line()? else {
                break ExitReason::EndOfInput;
            };
            let mut sink = LockedSink {
                stream: Arc::clone(&out),
            };
            match handle_input(
                Input::parse(&line),
                &mut sink,
                &mut history_file,
                &mut reader,
                &mut cancellation_gesture,
                Some(&mut source),
            )? {
                Step::Exit => break ExitReason::Requested,
                Step::Undo => report_undo(&host, &mut sink)?,
                Step::PickModel => {
                    flush_lines(
                        &host,
                        &mut sink,
                        &["use /model <id>; rune models lists model ids".to_owned()],
                    )?;
                }
                Step::PickTheme => {
                    flush_lines(&host, &mut sink, &[THEME_USAGE.to_owned()])?;
                }
                Step::Continue => {}
            }
        }
    } else {
        let mut shell = Shell::new(&mut source);
        // A pipe has no keystrokes to drive a picker, so a model change is
        // reported instead of silently doing nothing.
        shell.run(|input| {
            let mut sink = LockedSink {
                stream: Arc::clone(&out),
            };
            match handle_input(input, &mut sink, &mut history_file, &mut reader, &mut cancellation_gesture, None)? {
                Step::Exit => Ok(Action::Exit),
                Step::Continue => Ok(Action::Continue),
                Step::Undo => {
                    report_undo(&host, &mut sink)?;
                    Ok(Action::Continue)
                }
                Step::PickModel => {
                    let _ = writeln!(
                        sink,
                        "a model picker needs a terminal; use /model <id> and run `rune models` to list ids"
                    );
                    Ok(Action::Continue)
                }
                Step::PickTheme => {
                    let _ = writeln!(sink, "{THEME_USAGE}");
                    Ok(Action::Continue)
                }
            }
        })?
    };

    host.stop_warming();
    host.absorb_helper_usage();

    // The region is removed before the session ends, so the shell that started
    // it continues on a screen that is not half a frame.
    let cleared = host.clear_region()?;
    if !cleared.is_empty()
        && let Ok(mut sink) = out.lock()
    {
        let _ = sink.write_all(&cleared);
        let _ = sink.flush();
    }

    Ok(reason.exit_code())
}

/// Returns whether a command changes what the next request starts with, or
/// ends the session, so warming the last request's cache is no longer useful.
fn ends_warming(handled: &Handled) -> bool {
    matches!(
        handled,
        Handled::Exit
            | Handled::NewSession
            | Handled::SetModel(_)
            | Handled::PickModel
            | Handled::Compact
            | Handled::Reload
    )
}

/// Runs with canonical line input, reading only explicit approval and question
/// responses during a turn. Completed replies are settled once by the caller.
fn run_turn_accessible(
    history: &mut History,
    host: &SessionHost,
    input: &mut dyn InputSource,
) -> SteeredTurn {
    host.cancellation.reset();
    let (requests, pending) = mpsc::channel();
    *host
        .approval_requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(requests);
    let (questions, pending_questions) = mpsc::channel();
    *host
        .questions
        .requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(questions);
    let (outcome, diagnostic) = run_on_worker(
        history,
        |taken| turn::run_turn(taken, host).and_then(turn::TurnOutcome::into_strict),
        || {
            if let Ok(request) = pending.try_recv() {
                let mut lines = approval_lines(&request, usize::MAX);
                lines.push(if request.rememberable {
                    "type yes to run once, or always to allow it for this session; any other answer denies"
                        .to_owned()
                } else {
                    "type yes to run once; any other answer denies".to_owned()
                });
                host.show(&transcript::append_lines(&transcript::accessible_lines(&[
                    Entry::notice(lines.join("\n")),
                ])));
                let answer = match input.next_line() {
                    Ok(Some(line)) if line.trim().eq_ignore_ascii_case("yes") => {
                        ApprovalOutcome::RunOnce
                    }
                    Ok(Some(line))
                        if request.rememberable && line.trim().eq_ignore_ascii_case("always") =>
                    {
                        ApprovalOutcome::RememberForSession
                    }
                    _ => ApprovalOutcome::Deny,
                };
                let _ = request.answer.send(answer);
            } else if let Ok(request) = pending_questions.try_recv() {
                let answer = collect_accessible_questions(&request.questions, host, input);
                let _ = request.answer.send(answer);
            } else {
                std::thread::sleep(rune_term::shell::POLL_INTERVAL);
            }
        },
    );
    *host
        .approval_requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    *host
        .questions
        .requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    SteeredTurn {
        outcome,
        diagnostic,
        applied: Vec::new(),
        unsent: Vec::new(),
    }
}

fn collect_accessible_questions(
    questions: &[Question],
    host: &SessionHost,
    input: &mut dyn InputSource,
) -> Result<Answer> {
    let mut answers = Vec::new();
    for question in questions {
        let mut lines = vec![format!("Question: {}", question.text)];
        for (index, option) in question.options.iter().enumerate() {
            lines.push(format!(
                "Option {}: {}",
                index.saturating_add(1),
                option.label
            ));
            if let Some(description) = &option.description {
                lines.push(format!("Description: {description}"));
            }
        }
        lines.push("Answer: enter an option number, or /cancel".to_owned());
        host.show(&transcript::append_lines(&lines));
        loop {
            let Some(line) = input.next_line()? else {
                host.cancellation.cancel();
                return Ok(Answer::Cancelled);
            };
            if line.trim() == "/cancel" {
                host.cancellation.cancel();
                return Ok(Answer::Cancelled);
            }
            if let Some(choice) = line
                .trim()
                .parse::<usize>()
                .ok()
                .and_then(|number| number.checked_sub(1))
                .filter(|index| *index < question.options.len())
            {
                answers.push(choice);
                break;
            }
            host.show(b"Answer: invalid option number; try again or /cancel\n");
        }
    }
    Ok(Answer::Chosen(answers))
}

/// Runs one turn while keeping the keyboard live.
///
/// The turn runs on its own thread because a turn that ran here would block
/// every keystroke until the model finished. What the user types meanwhile is
/// submitted to the steering queue, which the turn drains at the boundaries it
/// already computes; Escape and Control-C reach the cancellation flag the turn
/// already checks.
///
/// Returns the turn's outcome. Cancellation is reported by the turn itself
/// rather than by abandoning it, so the conversation keeps whatever the turn
/// had already recorded.
fn run_turn_steerable(
    history: &mut History,
    host: &SessionHost,
    reader: &mut rune_term::input::KeyReader,
    gesture: &mut rune_term::shell::EscapeGesture,
) -> SteeredTurn {
    if !reader.is_active() {
        // Without a terminal there are no keys to read, so the turn runs here
        // and the steering path is simply unused.
        return SteeredTurn {
            outcome: turn::run_turn(history, host).and_then(turn::TurnOutcome::into_strict),
            diagnostic: None,
            applied: Vec::new(),
            unsent: Vec::new(),
        };
    }

    // Cleared before the turn rather than after, so a cancel that arrived as
    // the previous turn ended cannot stop this one before it starts.
    host.cancellation.reset();
    // The host's copy of the typed line is what every streamed frame draws, so
    // it starts from the reader's line rather than whatever the last turn left.
    if let Ok(mut typed) = host.typed.lock() {
        *typed = (reader.line().to_owned(), reader.column());
    }
    let mut submitted: Vec<String> = Vec::new();

    let (requests, pending) = mpsc::channel();
    if let Ok(mut slot) = host.approval_requests.lock() {
        *slot = Some(requests);
    }
    let (questions, pending_questions) = mpsc::channel();
    if let Ok(mut slot) = host.questions.requests.lock() {
        *slot = Some(questions);
    }

    let (result, diagnostic) = run_on_worker(
        history,
        |taken| turn::run_turn(taken, host).and_then(turn::TurnOutcome::into_strict),
        || {
            if let Ok(request) = pending.try_recv() {
                gesture.disarm();
                let answer = collect_approval(&request, host, reader);
                let _ = request.answer.send(answer);
                host.draw_stream_with(reader.line(), reader.column());
                return;
            }
            if let Ok(request) = pending_questions.try_recv() {
                gesture.disarm();
                let answer = collect_questions(&request.questions, host, reader);
                let _ = request.answer.send(answer);
                host.draw_stream_with(reader.line(), reader.column());
                return;
            }
            let Some(key) = reader.poll_key(rune_term::shell::POLL_INTERVAL) else {
                host.flush_frame();
                host.tick();
                return;
            };
            match key {
                KeyAction::ExternalEditor => {
                    gesture.disarm();
                    edit_draft(reader, host);
                    host.draw_stream_with(reader.line(), reader.column());
                }
                KeyAction::Transcript => {
                    gesture.disarm();
                    let _ = view_transcript(reader, host);
                    host.draw_stream_with(reader.line(), reader.column());
                }
                // Enter submits what has been typed as steering rather than as
                // a new turn, so a correction reaches the running turn instead
                // of queueing behind it.
                KeyAction::Submit => {
                    let text = reader.line().trim().to_owned();
                    reader.clear();
                    gesture.disarm();
                    if text.is_empty() {
                        host.draw_stream_with("", 0);
                        return;
                    }
                    let notice = match host.steering.submit(text.clone()) {
                        Ok(()) => {
                            submitted.push(text);
                            String::from("steering queued for the next boundary")
                        }
                        Err(err) => err.message().to_owned(),
                    };
                    // Drawn with the emptied line, so the submitted text does
                    // not stay on screen as though it were still being typed.
                    host.draw_notice_with(&notice, "", 0);
                }
                // Escape arms on the first press and cancels on the second.
                // The first press also clears a partly typed correction, which
                // is what the key does everywhere else.
                KeyAction::Escape => {
                    if gesture.record() {
                        host.cancellation.cancel();
                        host.draw_notice("cancelling the turn");
                    } else if reader.line().is_empty() {
                        host.draw_notice("press Escape again to cancel");
                    } else {
                        reader.clear();
                        host.draw_stream_with("", 0);
                    }
                }
                // Control-C clears a typed correction first, then cancels. The
                // session is left from the prompt the cancelled turn returns
                // to, so a press here never abandons a turn still writing.
                KeyAction::Cancel => {
                    gesture.disarm();
                    if reader.line().is_empty() {
                        host.cancellation.cancel();
                        host.draw_notice("cancelling the turn");
                    } else {
                        reader.clear();
                        host.draw_stream_with("", 0);
                    }
                }
                // Anything else is an edit, so a half-finished cancel gesture
                // is abandoned rather than left armed, and the line is redrawn
                // with the text that has been typed so far.
                _ => {
                    gesture.disarm();
                    // The reader's line is passed rather than read back from
                    // the host, so the keystroke that just landed is drawn and
                    // not the state before it.
                    host.draw_stream_with(reader.line(), reader.column());
                }
            }
        },
    );
    if let Ok(mut slot) = host.approval_requests.lock() {
        *slot = None;
    }
    if let Ok(mut slot) = host.questions.requests.lock() {
        *slot = None;
    }
    // Anything still queued was typed after the turn's last boundary, so the
    // turn never saw it. The queue is drained in one piece, which makes what is
    // left the newest submissions and everything before them what was applied.
    let unsent: Vec<String> = host
        .steering
        .drain(rune_agent::Boundary::Finalizing)
        .into_iter()
        .map(|message| message.text)
        .collect();
    let applied_count = submitted.len().saturating_sub(unsent.len());
    submitted.truncate(applied_count);
    SteeredTurn {
        outcome: result,
        diagnostic,
        applied: submitted,
        unsent,
    }
}

/// Shows the complete scope in scrollback, then waits on a picker.
///
/// Deny is selected initially, with running once directly above it, so one
/// step up runs the action and remembering it takes a second, deliberate one.
/// Escape, Control-C, and Control-D cancel the turn.
fn collect_approval(
    request: &ApprovalRequest,
    host: &SessionHost,
    reader: &mut rune_term::input::KeyReader,
) -> ApprovalOutcome {
    let choices: Vec<ApprovalOutcome> = if request.rememberable {
        vec![
            ApprovalOutcome::RememberForSession,
            ApprovalOutcome::RunOnce,
            ApprovalOutcome::Deny,
        ]
    } else {
        vec![ApprovalOutcome::RunOnce, ApprovalOutcome::Deny]
    };
    let mut picker = rune_term::picker::Picker::new(
        "permission required",
        choices
            .iter()
            .map(|choice| choice.label().to_owned())
            .collect(),
        choices.len(),
    );
    picker.to(choices.len().saturating_sub(1));
    host.refresh_size();
    let lines = approval_lines(request, usize::from(host.width()));
    if draw_choice(host, &picker, &lines, false).is_err() {
        return ApprovalOutcome::Deny;
    }
    loop {
        if host.cancellation.is_cancelled() {
            return ApprovalOutcome::Deny;
        }
        let key = reader.poll_choice(rune_term::shell::POLL_INTERVAL);
        let idle = key.is_none();
        if let Some(key) = key
            && let Some(answer) = approval_key(key, &mut picker, &host.cancellation)
        {
            return answer;
        }
        // Also refreshes dimensions when no input arrives.
        if draw_choice(host, &picker, &[], idle).is_err() {
            return ApprovalOutcome::Deny;
        }
    }
}

/// Returns true when an approval may be remembered for the session.
///
/// A command that destroys data or publishes is approved one run at a time,
/// and so is reaching a private network address.
fn rememberable(tool: &str, target: &str) -> bool {
    if tool == "web_fetch_private" {
        return false;
    }
    if tool == "shell" {
        // The program's own argv is read when the line needs no shell, which
        // is what lets subcommands such as `git push` be recognized.
        let argv = rune_policy::command::direct_argv(target)
            .unwrap_or_else(|| rune_policy::command::shell_argv(target));
        return rune_policy::command::classify(&argv).kind
            != rune_policy::command::CommandKind::Consequential;
    }
    true
}

/// Collects every answer without editing or submitting the current draft.
fn collect_questions(
    questions: &[Question],
    host: &SessionHost,
    reader: &mut rune_term::input::KeyReader,
) -> Result<Answer> {
    let mut answers = Vec::with_capacity(questions.len());
    for (index, question) in questions.iter().enumerate() {
        host.cancellation.check()?;
        let mut picker = rune_term::picker::Picker::new(
            format!(
                "answer required ({}/{})",
                index.saturating_add(1),
                questions.len()
            ),
            question
                .options
                .iter()
                .map(|option| transcript::sanitize(&option.label))
                .collect(),
            question.options.len(),
        );
        host.refresh_size();
        let lines = question_lines(question, usize::from(host.width()));
        draw_choice(host, &picker, &lines, false)?;
        loop {
            host.cancellation.check()?;
            let key = reader.poll_choice(rune_term::shell::POLL_INTERVAL);
            let idle = key.is_none();
            if let Some(key) = key {
                let chosen = question_key(key, &mut picker, &host.cancellation);
                host.cancellation.check()?;
                if let Some(chosen) = chosen {
                    answers.push(chosen);
                    break;
                }
            }
            draw_choice(host, &picker, &[], idle)?;
        }
    }
    Ok(Answer::Chosen(answers))
}

/// Keeps the full question and option descriptions available in scrollback.
fn question_lines(question: &Question, width: usize) -> Vec<String> {
    std::iter::once(question.text.clone())
        .chain(question.options.iter().map(|option| {
            option.description.as_ref().map_or_else(
                || option.label.clone(),
                |description| format!("{}: {description}", option.label),
            )
        }))
        .flat_map(|line| rune_term::width::wrap(&transcript::sanitize(&line), width.max(1)))
        .collect()
}

fn question_key(
    key: KeyAction,
    picker: &mut rune_term::picker::Picker,
    cancellation: &Cancellation,
) -> Option<usize> {
    match key {
        KeyAction::Submit => picker.selected().map(|_| picker.cursor()),
        KeyAction::Up => {
            picker.up();
            None
        }
        KeyAction::Down => {
            picker.down();
            None
        }
        KeyAction::Escape | KeyAction::Cancel | KeyAction::Interrupt => {
            cancellation.cancel();
            None
        }
        KeyAction::Complete
        | KeyAction::Ignored
        | KeyAction::Transcript
        | KeyAction::HistorySearch
        | KeyAction::ExternalEditor => None,
    }
}

/// Quoting makes control characters visible without changing the approved scope.
fn approval_lines(request: &ApprovalRequest, width: usize) -> Vec<String> {
    [
        format!("Permission request for {:?}", request.tool),
        format!("Reason: {:?}", request.reason),
        format!("Scope: {:?}", request.target),
    ]
    .into_iter()
    .flat_map(|line| rune_term::width::wrap(&line, width.max(1)))
    .chain(request.preview.iter().cloned())
    .collect()
}

fn approval_key(
    key: KeyAction,
    picker: &mut rune_term::picker::Picker,
    cancellation: &Cancellation,
) -> Option<ApprovalOutcome> {
    match key {
        KeyAction::Submit => Some(
            ApprovalOutcome::ALL
                .into_iter()
                .find(|choice| picker.selected() == Some(choice.label()))
                .unwrap_or(ApprovalOutcome::Deny),
        ),
        KeyAction::Up => {
            picker.up();
            None
        }
        KeyAction::Down => {
            picker.down();
            None
        }
        KeyAction::Escape | KeyAction::Cancel | KeyAction::Interrupt => {
            cancellation.cancel();
            Some(ApprovalOutcome::Deny)
        }
        KeyAction::Complete
        | KeyAction::Ignored
        | KeyAction::Transcript
        | KeyAction::HistorySearch
        | KeyAction::ExternalEditor => None,
    }
}

/// Uses the session renderer and reports if the choice cannot be displayed.
///
/// A `paced` redraw that would change nothing on screen writes nothing.
fn draw_choice(
    host: &SessionHost,
    picker: &rune_term::picker::Picker,
    settled: &[String],
    paced: bool,
) -> Result<()> {
    let _frame = host
        .frame
        .lock()
        .map_err(|_| RuneError::new(ErrorCode::Internal, "the frame lock was poisoned"))?;
    let out = host
        .live_out
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .ok_or_else(|| RuneError::new(ErrorCode::InputRequired, "choice output is unavailable"))?;
    let hint = vec![String::from(
        "Up/Down choose, Enter confirm, Esc/Ctrl-C cancel",
    )];
    let menu = picker.rows(&host.theme(), host.depth);
    let painted = host.paint_with_menu(
        settled,
        Some(picker.title()),
        &hint,
        &[],
        &menu,
        (0, 0),
        paced,
    )?;
    if painted.is_empty() {
        return Ok(());
    }
    let mut sink = LockedSink { stream: out };
    sink.write_all(&painted)?;
    sink.flush()?;
    Ok(())
}

/// Runs a turn on its own thread, calling `between` until it finishes.
///
/// The conversation is moved in and handed back: a turn pushes what it learned
/// into the history, and the next turn must see it. Moving it is what lets the
/// worker own it outright rather than sharing it behind a lock the loop would
/// then have to hold.
///
/// A worker that panics hands back the conversation as it stood before the
/// turn. The panic ends that exchange; losing every earlier one with it would
/// turn one defect into the loss of the session. What the worker held is not
/// used, because a turn stopped partway can hold a tool call with no result,
/// and every later request would then be refused.
/// The panic is an internal failure, with its diagnostic returned separately
/// for the closing frame.
fn run_on_worker<T, B>(
    history: &mut History,
    turn: T,
    mut between: B,
) -> (Result<turn::TurnOutcome>, Option<String>)
where
    T: FnOnce(&mut History) -> Result<turn::TurnOutcome> + Send,
    B: FnMut(),
{
    let before = history.clone();
    let mut taken = std::mem::take(history);
    let (returned, result, diagnostic) = std::thread::scope(|scope| {
        let worker = scope.spawn(move || {
            rune_term::input::catch_worker_panic(|| {
                let result = turn(&mut taken);
                (taken, result)
            })
        });
        while !worker.is_finished() {
            between();
        }
        match worker.join().and_then(std::convert::identity) {
            Ok((returned, result)) => (returned, result, None),
            Err(payload) => {
                let message = payload
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| payload.downcast_ref::<&str>().copied())
                    .unwrap_or("non-text panic payload");
                (
                    before,
                    Err(RuneError::new(
                        ErrorCode::Internal,
                        "internal worker failure",
                    )),
                    Some(format!("worker panicked: {message}")),
                )
            }
        }
    });
    *history = returned;
    (result, diagnostic)
}

/// What a steerable turn produced beside its outcome.
struct SteeredTurn {
    /// The turn's own result.
    outcome: Result<turn::TurnOutcome>,
    /// A caught worker panic, settled once through the renderer.
    diagnostic: Option<String>,
    /// Corrections the turn took in, in the order they were typed.
    applied: Vec<String>,
    /// Corrections typed after the turn's last boundary, which it never saw.
    unsent: Vec<String>,
}

/// Draws the frame that closes a turn: its settled lines, then the input row.
///
/// The input row shows whatever the reader holds, because a user who typed
/// during the turn is still typing. A correction the turn ended before taking
/// is put back on the line rather than dropped, where one more Enter sends it.
fn close_turn(
    host: &SessionHost,
    sink: &mut LockedSink,
    reader: &mut rune_term::input::KeyReader,
    lines: &[String],
    activity: Option<&str>,
    unsent: &[String],
) -> Result<()> {
    let mut notice = activity.map(str::to_owned);
    if !unsent.is_empty() {
        let mut restored = unsent.join(" ");
        if !reader.line().trim().is_empty() {
            restored.push(' ');
            restored.push_str(reader.line());
        }
        reader.replace(&restored);
        notice = Some(String::from(
            "the turn ended before this was sent; press Enter to send it",
        ));
    }
    let marker = rune_term::shell::prompt();
    let (prompt_rows, caret) = transcript::render_draft_at(
        marker,
        reader.line(),
        reader.column(),
        usize::from(host.width()),
    );
    if let Ok(mut typed) = host.typed.lock() {
        *typed = (reader.line().to_owned(), reader.column());
    }
    let mut lines = std::borrow::Cow::Borrowed(lines);
    if let Some(recorder) = &host.recorder
        && let Some(warning) = recorder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take_metadata_warning()
    {
        let entry = Entry::notice(format!("warning: {warning}"));
        host.transcript
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(entry.clone());
        lines.to_mut().extend(render_entries(&[entry], host));
    }
    let painted = host.paint(&lines, notice.as_deref(), &prompt_rows, &[], caret)?;
    if !painted.is_empty() {
        sink.write_all(&painted)?;
        sink.flush()?;
    }
    Ok(())
}

/// Restores the main terminal even when rendering fails or unwinds.
struct TranscriptScreen<'a>(&'a SessionHost);

/// Uses the same terminal ownership as the transcript to keep worker output
/// away from the editor and preserve the main screen underneath it.
fn edit_draft(reader: &mut rune_term::input::KeyReader, host: &SessionHost) {
    let screen = {
        let _frame = host
            .frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        host.transcript_open
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let screen = TranscriptScreen(host);
        host.show(b"\x1b[?1049h\x1b[0m\x1b[2J\x1b[H\x1b[?25h");
        screen
    };
    let result = reader.edit_external();
    if let Ok(mut typed) = host.typed.lock() {
        *typed = (reader.line().to_owned(), reader.column());
    }
    drop(screen);
    if let Err(error) = result {
        let _frame = host
            .frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (prompt, caret) = transcript::render_draft_at(
            rune_term::shell::prompt(),
            reader.line(),
            reader.column(),
            usize::from(host.width()),
        );
        let message = format!("external editor failed: {error}");
        if let Ok(painted) =
            host.paint(&[transcript::sanitize(&message)], None, &prompt, &[], caret)
        {
            host.show(&painted);
        }
    }
}

impl Drop for TranscriptScreen<'_> {
    fn drop(&mut self) {
        let _frame = self
            .0
            .frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.0.show(b"\x1b[0m\x1b[?1049l\x1b[?25h");
        self.0
            .transcript_open
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Reads a stable snapshot on the alternate screen. The reader's composer is
/// never replaced, so its caret, recall state and kill buffer survive intact.
fn view_transcript(reader: &rune_term::input::KeyReader, host: &SessionHost) -> Result<()> {
    use rune_term::input::TranscriptAction;

    let mut entries = host
        .transcript
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    entries.extend(event_entries(host));
    let partial = host.partial_answer();
    if !partial.is_empty() {
        entries.push(Entry::assistant(partial));
    }
    host.refresh_size();
    let mut size = (
        host.width(),
        host.height.load(std::sync::atomic::Ordering::Relaxed),
    );
    let mut view =
        rune_term::screen::Transcript::new(full_transcript_rows(&entries, size.0, host.ascii));
    let mut details = transcript::ToolDetails::new(entries);
    let mut surface = rune_term::frame::FrameSurface::new(size.0, size.1)?;
    let _screen = {
        let _frame = host
            .frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        host.transcript_open
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let screen = TranscriptScreen(host);
        host.show(b"\x1b[?1049h\x1b[0m\x1b[2J\x1b[H\x1b[?25l");
        screen
    };

    let mut redraw = true;
    loop {
        host.refresh_size();
        let next_size = (
            host.width(),
            host.height.load(std::sync::atomic::Ordering::Relaxed),
        );
        if next_size != size {
            let top = view.top();
            size = next_size;
            view = rune_term::screen::Transcript::new(
                details
                    .rows(Display {
                        width: usize::from(size.0),
                        ascii: host.ascii,
                        ..Display::default()
                    })
                    .0,
            );
            view.jump(top, usize::from(size.1.saturating_sub(1)));
            // Resizing can leave stale cells anywhere on the alternate screen.
            host.show(b"\x1b[2J");
            redraw = true;
        }
        let height = usize::from(size.1.saturating_sub(1));
        if redraw {
            let mut lines = view.visible(height).to_vec();
            lines.resize(height, String::new());
            let footer = vec![String::from(
                "Transcript | Tab tool Space toggle | Up/Down PgUp/PgDn Home/End | Esc/Ctrl-O close",
            )];
            let target = rune_term::frame::compose(
                &rune_term::frame::Regions::new(&lines, &footer),
                size.0,
                size.1,
            )?;
            let painted = surface.commit(&target)?;
            host.show(&painted.bytes);
        }
        let action = reader.poll_transcript(std::time::Duration::from_millis(50));
        redraw = !matches!(action, Some(TranscriptAction::Ignored) | None);
        match action {
            Some(TranscriptAction::Close) => break,
            Some(TranscriptAction::Up) => view.scroll_up(1),
            Some(TranscriptAction::Down) => view.scroll_down(1, height),
            Some(TranscriptAction::PageUp) => view.page_up(height),
            Some(TranscriptAction::PageDown) => view.page_down(height),
            Some(TranscriptAction::Home) => view.jump(0, height),
            Some(TranscriptAction::End) => view.jump(usize::MAX, height),
            Some(
                action @ (TranscriptAction::NextTool
                | TranscriptAction::PreviousTool
                | TranscriptAction::ToggleTool),
            ) => {
                match action {
                    TranscriptAction::NextTool => details.select(false),
                    TranscriptAction::PreviousTool => details.select(true),
                    _ => details.toggle(),
                }
                let (rows, selected) = details.rows(Display {
                    width: usize::from(size.0),
                    ascii: host.ascii,
                    ..Display::default()
                });
                let top = selected.unwrap_or(view.top());
                view = rune_term::screen::Transcript::new(rows);
                view.jump(top, height);
            }
            Some(TranscriptAction::Ignored) | None => {}
        }
    }
    Ok(())
}

/// Keeps a session's retained tool results in its directory, so a resumed
/// session can still read them. Returns the notice to show when that fails.
pub(crate) fn attach_results(
    history: &mut History,
    paths: &Paths,
    id: &SessionId,
) -> Option<String> {
    history
        .attach_result_directory(paths.session_dir(id).join("results"))
        .err()
        .map(|err| format!("stored tool results will not survive a restart: {err}"))
}

/// Adds process interruption boundaries to the saved transcript without putting
/// those display notices into the conversation sent to the provider.
fn resumed_entries(history: &History, state: &rune_session::store::SessionState) -> Vec<Entry> {
    use rune_session::event::SessionEvent;
    use rune_term::transcript::Speaker;

    let mut boundaries = std::collections::BTreeSet::new();
    let mut messages = 0_usize;
    for frame in rune_session::replay::replay_events(&state.events) {
        match &frame.event {
            SessionEvent::UserMessage { .. } => messages = messages.saturating_add(1),
            SessionEvent::AssistantMessage { text, .. } if !text.is_empty() => {
                messages = messages.saturating_add(1);
            }
            SessionEvent::TurnInterrupted { .. } => {
                boundaries.insert(messages);
            }
            _ => {}
        }
    }
    messages = 0;
    let mut entries = Vec::new();
    for entry in history_entries(history.turns()) {
        if matches!(entry.speaker, Speaker::User | Speaker::Assistant) {
            messages = messages.saturating_add(1);
        }
        entries.push(entry);
        if boundaries.remove(&messages) {
            entries.push(Entry::notice("interrupted"));
        }
    }
    entries
}

/// Projects recorded messages in order, keeping tool arguments and result bodies.
fn history_entries(turns: &[rune_agent::history::Turn]) -> Vec<Entry> {
    use rune_net::message::{ContentPart, Role};
    turns
        .iter()
        .flat_map(|turn| {
            turn.parts.iter().map(|part| match part {
                ContentPart::Text { text } => match turn.role {
                    Role::User => Entry::user(text.clone()),
                    Role::Assistant => Entry::assistant(text.clone()),
                    Role::Tool => Entry::tool(text.clone()),
                    Role::System => Entry::notice(text.clone()),
                },
                ContentPart::Reasoning { text } => Entry::reasoning(text.clone()),
                ContentPart::ToolCall {
                    name, arguments, ..
                } => Entry::tool(format!("{name} {arguments}")),
                ContentPart::ToolResult {
                    name,
                    content,
                    is_error,
                    ..
                } => Entry::tool(format!(
                    "{name}{}:\n{content}",
                    if *is_error { " (error)" } else { "" }
                )),
                ContentPart::Image { image } => {
                    Entry::notice(format!("image {} ({})", image.id, image.media_type))
                }
            })
        })
        .collect()
}

/// Expands every recorded tool line using the same wrapping as the inline view.
fn full_transcript_rows(entries: &[Entry], cols: u16, ascii: bool) -> Vec<String> {
    transcript::render(
        entries,
        Display {
            width: usize::from(cols),
            ascii,
            tool_lines: usize::MAX,
            ..Display::default()
        },
    )
    .lines()
    .map(str::to_owned)
    .collect()
}

/// Waits for a line to be submitted, drawing the prompt as it is typed.
///
/// Returns the submitted input, or `None` when the user asked to leave. The
/// cursor is placed at the editor's caret within the visible draft.
///
/// `recall` supplies earlier prompts for the up and down arrows. Passing an
/// empty slice leaves those keys doing nothing, which is what a session with no
/// recorded history wants.
fn await_submission(
    commands: &[CommandCandidate<'_>],
    reader: &mut rune_term::input::KeyReader,
    host: &SessionHost,
    out: &LiveSink,
    recall: &[String],
    context: &ExecutionContext,
    limits: &rune_tools::workspace::FileLimits,
) -> Result<Option<Input>> {
    let marker = rune_term::shell::prompt();
    // Which completion row is highlighted. While the dropdown is open the
    // arrows move it rather than walking the prompt history, because the list is
    // what the user is looking at.
    let mut selected = 0_usize;
    let mut paths: Option<crate::path_completion::Paths> = None;

    loop {
        let rows = paths.as_ref().map_or_else(
            || {
                completion_rows(
                    commands,
                    reader.line(),
                    selected,
                    &host.theme(),
                    host.depth(),
                    host.menu_room(),
                )
            },
            |paths| {
                path_completion_rows(
                    paths,
                    selected,
                    &host.theme(),
                    host.depth(),
                    host.menu_room(),
                )
            },
        );
        draw_prompt(reader, host, out, &rows, marker)?;

        let draft = reader.line().to_owned();
        let caret = reader.cursor_byte();
        let key = reader.read_key();
        // Resize and other non-edit events still redraw the prompt, but must
        // not reset its completion selection or dismiss an open path menu.
        if key == KeyAction::Ignored && reader.line() == draft && reader.cursor_byte() == caret {
            continue;
        }
        if key == KeyAction::Transcript {
            view_transcript(reader, host)?;
            continue;
        }
        if key == KeyAction::ExternalEditor {
            edit_draft(reader, host);
            selected = 0;
            paths = None;
            continue;
        }
        if key == KeyAction::HistorySearch {
            search_prompt_history(reader, host, out, recall)?;
            selected = 0;
            paths = None;
            continue;
        }
        if path_key(
            commands,
            key,
            reader,
            &mut paths,
            &mut selected,
            context,
            limits,
        ) {
            continue;
        }
        let had_draft = !reader.line().is_empty();
        match idle_key(commands, key, reader, &mut selected, &rows, recall) {
            // Clearing the draft abandons the prompt a command prepared, and
            // the model it chose with it.
            Idle::Stay
                if had_draft
                    && reader.line().is_empty()
                    && matches!(key, KeyAction::Escape | KeyAction::Cancel) =>
            {
                lock_value(&host.pending_model).take();
            }
            Idle::Stay => {}
            Idle::Leave => return Ok(None),
            Idle::Submit(input) => return Ok(Some(input)),
        }
    }
}

/// A filesystem menu is opened only by Tab and discarded when the draft changes.
fn path_key(
    commands: &[CommandCandidate<'_>],
    key: KeyAction,
    reader: &mut rune_term::input::KeyReader,
    paths: &mut Option<crate::path_completion::Paths>,
    selected: &mut usize,
    context: &ExecutionContext,
    limits: &rune_tools::workspace::FileLimits,
) -> bool {
    if let Some(open) = paths {
        match key {
            KeyAction::Submit | KeyAction::Complete => {
                if let Some(chosen) = open.matches.get(*selected) {
                    reader.complete_path(open.range.clone(), chosen);
                }
                *paths = None;
                *selected = 0;
                return true;
            }
            KeyAction::Up | KeyAction::Down => {
                *selected = if key == KeyAction::Up {
                    selected.saturating_sub(1)
                } else {
                    selected
                        .saturating_add(1)
                        .min(open.matches.len().saturating_sub(1))
                };
                return true;
            }
            KeyAction::Escape => {
                *paths = None;
                *selected = 0;
                return true;
            }
            _ => *paths = None,
        }
    }
    if key != KeyAction::Complete
        || completion_matches(commands, reader.line()) > 0
        || matches!(Input::parse(reader.line()), Input::Command { .. })
    {
        return false;
    }
    let choices = crate::path_completion::Paths::collect(
        reader.line(),
        reader.cursor_byte(),
        context,
        limits,
    );
    *selected = 0;
    if choices.matches.len() == 1 {
        if let Some(chosen) = choices.matches.first() {
            reader.complete_path(choices.range, chosen);
        }
    } else if !choices.matches.is_empty() {
        *paths = Some(choices);
    }
    true
}

fn path_completion_rows(
    paths: &crate::path_completion::Paths,
    selected: usize,
    theme: &Theme,
    depth: ColorDepth,
    room: usize,
) -> Vec<String> {
    let count = paths.matches.len();
    let window = completion_window(count, selected, menu_window(count, COMPLETION_WINDOW, room));
    let accent = theme.sgr(Slot::Accent, depth);
    let dim = theme.sgr(Slot::Dim, depth);
    let mut rows: Vec<_> = window
        .clone()
        .filter_map(|index| {
            paths.matches.get(index).map(|path| {
                if index == selected {
                    styled(&accent, &format!("> {path}"))
                } else {
                    styled(&dim, &format!("  {path}"))
                }
            })
        })
        .collect();
    if count > window.len() && rows.len() < room {
        rows.push(styled(
            &dim,
            &format!(
                "  {}-{} of {count}",
                window.start.saturating_add(1),
                window.end
            ),
        ));
    }
    rows
}

/// What a key pressed at the idle prompt leads to.
enum Idle {
    /// Keep reading keys.
    Stay,
    /// Leave the session.
    Leave,
    /// Hand this input to the session.
    Submit(Input),
}

/// Applies one key read at the idle prompt.
///
/// `selected` is the highlighted completion row, and `rows` are the rows drawn
/// for the completions, empty when none are open.
fn idle_key(
    commands: &[CommandCandidate<'_>],
    key: KeyAction,
    reader: &mut rune_term::input::KeyReader,
    selected: &mut usize,
    rows: &[String],
    recall: &[String],
) -> Idle {
    match key {
        KeyAction::Submit => {
            // Enter takes the highlighted completion when the list is open
            // and the command is not yet complete, so a half-typed name is
            // never run. Once the name is complete, Enter runs it.
            if let Some(chosen) = open_completion(commands, reader.line(), rows, *selected)
                && chosen.name != reader.line().trim_start_matches('/')
            {
                reader.replace(&format!("/{}", chosen.name));
                *selected = 0;
                return Idle::Stay;
            }
            let text = reader.line().trim().to_owned();
            reader.clear();
            if text.is_empty() {
                return Idle::Stay;
            }
            Idle::Submit(Input::parse(&text))
        }
        // An empty line is the only thing there is to leave behind, so
        // interrupting it means leaving the session.
        KeyAction::Interrupt => Idle::Leave,
        // Escape only clears the line. While a turn runs, a first Escape
        // asks for a second to cancel, and a turn that ends between the two
        // hands that second press to this prompt, where leaving would end the
        // session the user only meant to stop a turn in.
        KeyAction::Escape => {
            reader.clear();
            *selected = 0;
            Idle::Stay
        }
        KeyAction::Cancel => {
            // Control-C on a partly typed line abandons the line rather
            // than the session, matching what the key does elsewhere.
            if reader.line().is_empty() {
                return Idle::Leave;
            }
            reader.clear();
            *selected = 0;
            Idle::Stay
        }
        response @ (KeyAction::Up | KeyAction::Down) => {
            // The dropdown owns the arrows while it is open. The bound is
            // the number of matches, not the number of rows drawn: the rows
            // include a position line, so clamping on them would strand
            // every command past the first window.
            let matches = completion_matches(commands, reader.line());
            if matches > 0 {
                *selected = if response == KeyAction::Up {
                    selected.saturating_sub(1)
                } else {
                    selected.saturating_add(1).min(matches.saturating_sub(1))
                };
            } else if response == KeyAction::Up {
                reader.recall_previous(recall);
            } else {
                reader.recall_next(recall);
            }
            Idle::Stay
        }
        // Tab completes the highlighted command, which is what every other
        // shell does and what a reader reaches for first.
        KeyAction::Complete => {
            if let Some(chosen) = open_completion(commands, reader.line(), rows, *selected) {
                reader.replace(&format!("/{}", chosen.name));
                *selected = 0;
            }
            Idle::Stay
        }
        KeyAction::Transcript | KeyAction::ExternalEditor | KeyAction::HistorySearch => Idle::Stay,
        KeyAction::Ignored => {
            // Typing narrows the list, so the highlight returns to the top
            // rather than pointing at a row that may no longer exist.
            *selected = 0;
            Idle::Stay
        }
    }
}

/// Returns the command the dropdown is offering, when it is open.
///
/// The rows are already rendered, so the count comes from the table rather than
/// from the text of a row: a row carries styling and a description, and parsing
/// either back out would be the wrong way round.
fn open_completion<'a>(
    commands: &[CommandCandidate<'a>],
    line: &str,
    rows: &[String],
    selected: usize,
) -> Option<CommandCandidate<'a>> {
    if rows.is_empty() {
        return None;
    }
    let matches = matching_commands(commands, line);
    matches.get(selected).copied()
}

/// Draws the prompt row, with any rows that belong below it.
///
/// The visible draft scrolls with the caret so the terminal's cursor is where
/// the next character will go.
fn draw_prompt(
    reader: &rune_term::input::KeyReader,
    host: &SessionHost,
    out: &LiveSink,
    menu: &[String],
    marker: &str,
) -> Result<()> {
    let mut sink = out
        .lock()
        .map_err(|_| RuneError::new(ErrorCode::Internal, "the output lock was poisoned"))?;
    let (rows, caret) = transcript::render_draft_at(
        marker,
        reader.line(),
        reader.column(),
        usize::from(host.width()),
    );
    let painted = host.paint_with_menu(&[], None, &rows, &[], menu, caret, false)?;
    if !painted.is_empty() {
        sink.write_all(&painted)?;
        sink.flush()?;
    }
    Ok(())
}

/// Runs an inline picker and returns the chosen entry.
///
/// The list is drawn under the input line, so the transcript above stays
/// readable while a choice is made. Typing narrows the list, the vertical
/// arrows move the highlight, Enter accepts, and Escape abandons the choice
/// without changing anything. A valid model identifier absent from the list
/// can be typed and accepted verbatim.
///
/// Returns `None` when the user cancelled or asked to leave.
fn run_picker(
    reader: &mut rune_term::input::KeyReader,
    host: &SessionHost,
    out: &LiveSink,
    mut picker: rune_term::picker::Picker,
    catalog: &rune_net::catalog::Catalog,
) -> Result<Option<String>> {
    let marker = rune_term::shell::prompt();
    // Whatever was on the line is put back afterwards, so a half-typed prompt
    // is not lost to a look at the model list.
    let draft = reader.line().to_owned();
    reader.clear();

    let chosen = loop {
        let room = host.menu_room();
        let mut below = picker_menu(&mut picker, &host.theme(), host.depth, room);
        let typed = if picker.selected().is_none() {
            match catalog.choose(picker.query()) {
                rune_net::catalog::ModelChoice::One(id) => Some(id),
                _ => None,
            }
        } else {
            None
        };
        if let Some(id) = &typed
            && let Some(row) = below.get_mut(usize::from(room >= 4))
        {
            *row = format!("> use {id} (not listed)");
        }
        draw_prompt(reader, host, out, &below, marker)?;

        match reader.read_key() {
            // Tab and Enter both accept: the picker is the only thing on
            // screen, so there is no typed argument for Enter to mean.
            KeyAction::Submit | KeyAction::Complete => {
                if let Some(choice) = picker.selected().map(str::to_owned).or(typed) {
                    break Some(choice);
                }
            }
            KeyAction::Interrupt | KeyAction::Cancel | KeyAction::Escape => break None,
            // Both branches fall through to the redraw at the top of the loop.
            response @ (KeyAction::Up | KeyAction::Down) => {
                if response == KeyAction::Up {
                    picker.up();
                } else {
                    picker.down();
                }
            }
            KeyAction::Transcript => {
                view_transcript(reader, host)?;
            }
            KeyAction::ExternalEditor | KeyAction::HistorySearch => {}
            KeyAction::Ignored => {
                picker.set_query(reader.line());
            }
        }
    };

    reader.replace(&draft);
    Ok(chosen)
}

/// Flattens history for a menu without changing the selected prompt.
fn history_preview(row: &str, ascii: bool) -> String {
    transcript::sanitize(row)
        .replace('\n', if ascii { " / " } else { " ↵ " })
        .replace('\t', " ")
}

/// Searches workspace recall, restoring a choice without handing it to the session.
fn search_prompt_history(
    reader: &mut rune_term::input::KeyReader,
    host: &SessionHost,
    out: &LiveSink,
    recall: &[String],
) -> Result<()> {
    let chosen = reader.with_temporary_line(|query| -> Result<Option<String>> {
        let mut picker = history_picker(recall);
        loop {
            let rows = picker_menu(
                &mut picker,
                &Theme::no_color(),
                ColorDepth::None,
                host.menu_room(),
            )
            .into_iter()
            .map(|row| {
                let preview = history_preview(&row, host.ascii);
                let slot = if row.starts_with("> ") {
                    Slot::Accent
                } else {
                    Slot::Dim
                };
                styled(&host.theme().sgr(slot, host.depth), &preview)
            })
            .collect::<Vec<_>>();
            draw_prompt(query, host, out, &rows, rune_term::shell::prompt())?;
            match query.read_key() {
                KeyAction::Submit | KeyAction::Complete => {
                    if let Some(prompt) = picker.selected() {
                        return Ok(Some(prompt.to_owned()));
                    }
                }
                KeyAction::Escape | KeyAction::Cancel | KeyAction::Interrupt => return Ok(None),
                KeyAction::Up => {
                    picker.up();
                }
                KeyAction::Down => {
                    picker.down();
                }
                KeyAction::Ignored => picker.set_query(query.line()),
                KeyAction::Transcript | KeyAction::ExternalEditor | KeyAction::HistorySearch => {}
            }
        }
    })?;
    if let Some(prompt) = chosen {
        reader.restore_prompt(&prompt);
    }
    Ok(())
}

/// Newest first, with case-insensitive substring matching supplied by the picker.
fn history_picker(recall: &[String]) -> rune_term::picker::Picker {
    rune_term::picker::Picker::new(
        "prompt history (Enter restores without submitting)",
        recall.iter().rev().cloned().collect(),
        rune_term::picker::DEFAULT_WINDOW,
    )
}

/// Fits the model menu to the remaining rows before rendering its choices.
fn picker_menu(
    picker: &mut rune_term::picker::Picker,
    theme: &Theme,
    depth: ColorDepth,
    max_rows: usize,
) -> Vec<String> {
    if max_rows == 0 {
        return Vec::new();
    }
    // Keep a choice visible even when the title and key hint cannot fit.
    let decorated = max_rows >= 4;
    let room = max_rows.saturating_sub(if decorated { 2 } else { 0 });
    picker.set_window(menu_window(
        picker.matches().len(),
        rune_term::picker::DEFAULT_WINDOW,
        room,
    ));
    let mut rows = picker.rows(theme, depth);
    rows.truncate(room);
    if decorated {
        rows.insert(0, picker.title().to_owned());
        rows.push(rune_term::picker::Picker::hint().to_owned());
    }
    rows
}

/// Reports the manual compaction plan without changing the conversation.
fn preview_compaction(
    host: &SessionHost,
    history: &History,
    sink: &mut LockedSink,
) -> Result<Step> {
    let (summarized, retained) = rune_agent::compaction::plan(history, &host.limits)
        .map_or((0, history.len()), |plan| {
            (plan.removed_turns, plan.kept_turns)
        });
    report(
        host,
        sink,
        &format!(
            "compaction preview (history unchanged)\n{summarized} record(s) would be summarized\n{retained} record(s) retained verbatim"
        ),
    )
}

/// Summarizes older turns and installs the summary.
///
/// The summary is produced by the model, because nothing else can compress a
/// conversation without discarding what makes it useful. When the request fails
/// the history is left exactly as it was, so a failed compaction costs nothing
/// but the attempt.
fn compact_history(
    host: &SessionHost,
    history: &mut History,
    sink: &mut LockedSink,
) -> Result<Step> {
    let Some(plan) = rune_agent::compaction::plan_for(history, &host.limits, host.dialect.as_ref())
    else {
        return report(
            host,
            sink,
            "nothing to compact: the conversation is short enough to leave as it is",
        );
    };

    if host.endpoint.offline {
        return report(
            host,
            sink,
            "cannot compact while outbound requests are disabled",
        );
    }

    match compact_session(host, history, &plan, &rune_net::transport::agent(), true) {
        Ok(done) => {
            let removed = done.removed_turns;
            // The meter keeps the largest reading, so without a reset it would
            // go on showing the size of the conversation before compaction;
            // `context_replaced` cleared it.
            if let Err(err) = host.estimate_context(history) {
                host.draw_notice(&format!("context size was not estimated: {err}"));
            }
            report(
                host,
                sink,
                &format!(
                    "compacted {removed} earlier turn(s); {} turn(s) remain; originals: rune session {} --record all",
                    history.len(),
                    host.session_id_name()
                ),
            )
        }
        Err(err) => report(host, sink, err.message()),
    }
}

/// Estimates the actual dialect body, including instructions and tool schemas.
fn request_estimate(
    host: &SessionHost,
    plan: &rune_net::provider::RequestPlan,
    anchor: Option<&rune_agent::tokens::Anchor>,
) -> Result<rune_agent::tokens::Estimate> {
    rune_agent::compaction::request_estimate(
        host.dialect.as_ref(),
        plan,
        rune_agent::compaction::Window(
            host.context_limit
                .load(std::sync::atomic::Ordering::Relaxed),
        ),
        anchor,
    )
}

/// Keeps the turns a manual compaction removes in the session's archive.
struct RecorderArchive<'a>(&'a Mutex<Recorder>);

impl rune_agent::compaction::Archive for RecorderArchive<'_> {
    fn archive(&self, removed: &[rune_agent::history::Turn]) -> Result<()> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .archive_messages(removed)
    }
}

/// Runs the shared summarizer for this session, archiving the removed turns
/// first when `archive` is set, and records what it did.
fn compact_session(
    host: &SessionHost,
    history: &mut History,
    plan: &rune_agent::compaction::Plan,
    client: &dyn rune_net::fetch::Fetch,
    archive: bool,
) -> Result<rune_agent::compaction::CompactionReport> {
    let recorder = if archive {
        Some(host.recorder.as_ref().ok_or_else(|| {
            RuneError::new(
                ErrorCode::InvalidState,
                "manual compaction requires a session archive",
            )
        })?)
    } else {
        None
    };
    let archive = recorder.map(|recorder| RecorderArchive(recorder));
    let archive: Option<&dyn rune_agent::compaction::Archive> = archive
        .as_ref()
        .map(|archive| -> &dyn rune_agent::compaction::Archive { archive });
    let report = rune_agent::compaction::compact(history, plan, host, client, archive, &|| false)?;
    host.context_replaced(&report);
    Ok(report)
}

/// Commits one line of command output through the renderer.
fn report(host: &SessionHost, sink: &mut LockedSink, line: &str) -> Result<Step> {
    flush_lines(host, sink, &output_lines(line.as_bytes()))?;
    Ok(Step::Continue)
}

/// Puts back what the session changed and reports each file.
///
/// A file that could not be restored is reported and the rest are still
/// attempted, because leaving the user with half their files back and no
/// explanation is worse than a partial undo they can see.
fn report_undo(host: &SessionHost, sink: &mut LockedSink) -> Result<()> {
    if !host.can_undo() {
        flush_lines(
            host,
            sink,
            &["nothing to undo: this session has not changed any file".to_owned()],
        )?;
        return Ok(());
    }
    match host.undo() {
        Ok(lines) => flush_lines(host, sink, &lines)?,
        Err(err) => {
            let mut lines = vec![err.to_string()];
            if let Some(hint) = err.hint() {
                lines.push(format!("hint: {hint}"));
            }
            flush_lines(host, sink, &lines)?;
        }
    }
    Ok(())
}

/// A unified diff in the direction undo applies, with three context lines.
///
/// One hunk covers the changed region. Common leading and trailing lines are
/// kept as context, so work is linear even for large, unrelated file contents.
fn undo_diff(path: &Utf8Path, current: Option<&[u8]>, original: Option<&[u8]>) -> Vec<String> {
    if current == original {
        return vec!["no byte changes".to_owned()];
    }
    let old_label = current.map_or_else(|| "/dev/null".to_owned(), |_| path.to_string());
    let new_label = original.map_or_else(|| "/dev/null".to_owned(), |_| path.to_string());
    let mut lines = vec![format!("--- {old_label}"), format!("+++ {new_label}")];
    let old_bytes = current.unwrap_or_default();
    let new_bytes = original.unwrap_or_default();
    let text = std::str::from_utf8(old_bytes)
        .ok()
        .zip(std::str::from_utf8(new_bytes).ok());
    let Some((old, new)) = text.filter(|_| !old_bytes.contains(&0) && !new_bytes.contains(&0))
    else {
        lines.push(format!(
            "binary contents differ ({} -> {} bytes)",
            old_bytes.len(),
            new_bytes.len()
        ));
        return lines;
    };
    let old: Vec<_> = old.split_inclusive('\n').collect();
    let new: Vec<_> = new.split_inclusive('\n').collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let start = prefix.saturating_sub(3);
    let context_end = suffix.min(3);
    let old_end = old.len().saturating_sub(suffix);
    let new_end = new.len().saturating_sub(suffix);
    let old_hunk_end = old_end.saturating_add(context_end);
    let new_hunk_end = new_end.saturating_add(context_end);
    let old_count = old_hunk_end.saturating_sub(start);
    let new_count = new_hunk_end.saturating_sub(start);
    lines.push(format!(
        "@@ -{},{} +{},{} @@",
        if old_count == 0 {
            start
        } else {
            start.saturating_add(1)
        },
        old_count,
        if new_count == 0 {
            start
        } else {
            start.saturating_add(1)
        },
        new_count,
    ));
    for (marker, body) in [
        (' ', &old[start..prefix]),
        ('-', &old[prefix..old_end]),
        ('+', &new[prefix..new_end]),
        (' ', &old[old_end..old_hunk_end]),
    ] {
        for line in body {
            lines.push(format!(
                "{marker}{}",
                line.strip_suffix('\n').unwrap_or(line)
            ));
            if !line.ends_with('\n') {
                lines.push("\\ No newline at end of file".to_owned());
            }
        }
    }
    lines
}

/// Returns the model's most recent reply, as plain text.
///
/// Read from the conversation rather than from the screen: what is on screen has
/// been wrapped, indented, and styled, so copying it would paste back the
/// renderer's layout rather than what the model said.
#[must_use]
fn last_reply(history: &History) -> Option<String> {
    history
        .turns()
        .iter()
        .rev()
        .find(|turn| turn.role == rune_net::message::Role::Assistant)
        .map(|turn| {
            // Only text parts are copied. A tool call carries arguments the user
            // did not write and cannot use, so including it would paste JSON
            // into whatever they are working on.
            turn.parts
                .iter()
                .filter_map(|part| match part {
                    rune_net::message::ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<&str>>()
                .join("")
        })
        .filter(|reply| !reply.trim().is_empty())
}

/// Copies text to the terminal's clipboard.
///
/// Uses OSC 52, which the terminal emulator handles itself. Shelling out to a
/// platform clipboard tool would work on a desktop and fail over SSH, where the
/// terminal is the only thing that can reach the user's clipboard. A terminal
/// that does not implement OSC 52 ignores it rather than showing it, so the
/// escape never becomes visible text.
fn copy_to_clipboard(text: &str) -> std::io::Result<()> {
    use std::io::Write as _;

    let encoded = base64_encode(text.as_bytes());
    let mut stdout = std::io::stdout();
    write!(stdout, "\u{1b}]52;c;{encoded}\u{7}")?;
    stdout.flush()
}

/// Encodes bytes as standard base64 with padding.
///
/// Shared by OSC 52 clipboard output and inline vision requests.
#[must_use]
pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3).saturating_mul(4));
    for chunk in bytes.chunks(3) {
        let b0 = chunk.first().copied().unwrap_or(0);
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let triple = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        let indexes = [
            (triple >> 18) & 0x3f,
            (triple >> 12) & 0x3f,
            (triple >> 6) & 0x3f,
            triple & 0x3f,
        ];
        for (position, index) in indexes.iter().enumerate() {
            // A short final chunk emits padding instead of the bytes it does
            // not have, which is what makes the encoding decodable.
            if position > chunk.len() {
                out.push('=');
            } else {
                out.push(char::from(ALPHABET[*index as usize]));
            }
        }
    }
    out
}

/// Closes a styled row.
const RESET: &str = "\u{1b}[0m";

/// How many completions are shown at once.
///
/// A short window keeps the list from swallowing the screen: a bare slash
/// matches every command, and drawing all of them would push the transcript
/// away for a list a reader only ever takes the top few rows of.
pub const COMPLETION_WINDOW: usize = 6;

/// Returns the range of matches on screen for a selection.
///
/// The window scrolls only once the selection reaches its edge and then follows
/// the selection, so growing the selection moves the highlight down a fixed
/// screen until the window has to move, rather than the list jumping on every
/// key.
#[must_use]
pub fn completion_window(count: usize, selected: usize, window: usize) -> std::ops::Range<usize> {
    if window == 0 {
        return 0..0;
    }
    if count <= window {
        return 0..count;
    }
    // The last screenful is the floor, so the list can scroll to its end.
    let last_start = count.saturating_sub(window);
    let selected = selected.min(count.saturating_sub(1));
    // Start far enough back that the selection sits on the last row of the
    // window once it has moved past the first screenful.
    let start = selected
        .saturating_add(1)
        .saturating_sub(window)
        .min(last_start);
    start..start.saturating_add(window)
}

/// Leaves a row for the position indicator whenever a long list has room.
fn menu_window(count: usize, maximum: usize, room: usize) -> usize {
    let window = maximum.min(room);
    let position_row = usize::from(count > window && room > 1);
    window.min(room.saturating_sub(position_row))
}

/// A borrowed view of a builtin or discovered command for the idle menu.
#[derive(Clone, Copy, Debug)]
struct CommandCandidate<'a> {
    name: &'a str,
    arguments: &'a str,
    summary: &'a str,
}

/// Keeps builtin order and appends discovered commands in discovery order.
/// Builtins take precedence in the dispatcher, so do not offer their names twice.
fn command_candidates(custom: &[rune_context::commands::Command]) -> Vec<CommandCandidate<'_>> {
    rune_term::commands::BUILTINS
        .iter()
        .map(|command| CommandCandidate {
            name: command.name,
            arguments: command.arguments,
            summary: command.summary,
        })
        .chain(
            custom
                .iter()
                .filter(|command| rune_term::commands::find(&command.name).is_none())
                .map(|command| CommandCandidate {
                    name: &command.name,
                    arguments: command.argument_hint.as_deref().unwrap_or_default(),
                    summary: &command.description,
                }),
        )
        .collect()
}

/// Rendering, navigation and acceptance all use the same prefix matches.
fn matching_commands<'a>(
    commands: &[CommandCandidate<'a>],
    line: &str,
) -> Vec<CommandCandidate<'a>> {
    let Some(word) = slash_word(line) else {
        return Vec::new();
    };
    commands
        .iter()
        .filter(|command| command.name.starts_with(word))
        .copied()
        .collect()
}

/// Returns the rows showing what can be typed next.
///
/// Drawn while a slash command is being typed, from the builtins and discovered
/// commands also used by help and the dispatcher.
/// The row under the cursor is marked and coloured; every row carries the line
/// that describes the command, which is what makes the list usable without
/// trying each name.
///
/// Only a window of the matches is drawn, and a row saying how far through the
/// list the window is sits under it, so a reader can tell a short list from the
/// top of a long one.
///
/// Returns an empty list when the line is not a command being typed, so a
/// caller can pass every keystroke without checking first.
fn completion_rows(
    commands: &[CommandCandidate<'_>],
    line: &str,
    selected: usize,
    theme: &Theme,
    depth: ColorDepth,
    max_rows: usize,
) -> Vec<String> {
    if max_rows == 0 {
        return Vec::new();
    }
    let matches = matching_commands(commands, line);
    if matches.is_empty() {
        return Vec::new();
    }

    let accent = theme.sgr(Slot::Accent, depth);
    let dim = theme.sgr(Slot::Dim, depth);
    // Wide enough for every match rather than only the visible ones, so the
    // descriptions do not shift sideways as the window scrolls.
    let width = matches
        .iter()
        .map(|entry| {
            entry
                .name
                .len()
                .saturating_add(entry.arguments.len())
                .saturating_add(if entry.arguments.is_empty() { 1 } else { 2 })
        })
        .max()
        .unwrap_or(0);

    let window = completion_window(
        matches.len(),
        selected,
        menu_window(matches.len(), COMPLETION_WINDOW, max_rows),
    );
    let mut rows: Vec<String> = Vec::with_capacity(window.len().saturating_add(1));
    for index in window.clone() {
        let Some(entry) = matches.get(index) else {
            continue;
        };
        let left = if entry.arguments.is_empty() {
            format!("/{}", entry.name)
        } else {
            format!("/{} {}", entry.name, entry.arguments)
        };
        let body = format!("{left:<width$}  {}", entry.summary);
        if index == selected {
            rows.push(styled(&accent, &format!("> {body}")));
        } else {
            rows.push(styled(&dim, &format!("  {body}")));
        }
    }

    // How far through the list this window is, so a reader can tell a list that
    // ends here from one that continues. Shown only when there is more to see.
    if matches.len() > window.len() && rows.len() < max_rows {
        let first = window.start.saturating_add(1);
        let last = window.end;
        rows.push(styled(
            &dim,
            &format!("  {first}-{last} of {}", matches.len()),
        ));
    }
    rows
}

/// Returns how many completions the line offers.
///
/// Counted from the table rather than from the rendered rows, because the rows
/// carry a position line that is not a command.
#[must_use]
fn completion_matches(commands: &[CommandCandidate<'_>], line: &str) -> usize {
    matching_commands(commands, line).len()
}

/// Returns the word after a leading slash, when the line is one.
///
/// A line with a space in it is a command already being given its arguments, so
/// there is nothing left to complete.
fn slash_word(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('/')?;
    if rest.chars().any(char::is_whitespace) {
        return None;
    }
    Some(rest)
}

/// Wraps `text` in `open`, closing it again.
///
/// A theme without color yields an empty sequence, and a row surrounded by two
/// empty strings would carry escapes the terminal has nothing to do with.
fn styled(open: &str, text: &str) -> String {
    if open.is_empty() {
        return text.to_owned();
    }
    format!("{open}{text}{RESET}")
}

/// Wraps finished command output and leaves the prompt under it.
///
/// Every writer must go through the renderer: text written straight to the
/// stream lands at the caret, which is inside the region the session repaints,
/// and the two then disagree about what is on screen. Lines committed here
/// become part of the terminal's own scrollback.
fn flush_lines(host: &SessionHost, sink: &mut LockedSink, lines: &[String]) -> Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    let rendered;
    let lines = if host.accessible {
        rendered = transcript::accessible_lines(&[Entry::notice(lines.join("\n"))]);
        &rendered
    } else {
        // Inline expects display rows and clips anything wider. Captured
        // command output still holds logical lines, so wrap at the terminal's
        // current width before committing it to scrollback.
        host.refresh_size();
        let width = usize::from(host.width()).max(1);
        rendered = lines
            .iter()
            .flat_map(|line| rune_term::width::wrap(line, width))
            .collect::<Vec<_>>();
        &rendered
    };
    let (prompt_row, caret) = host.idle_prompt();
    let painted = host.paint(lines, None, &prompt_row, &[], caret)?;
    if !painted.is_empty() {
        sink.write_all(&painted)?;
        sink.flush()?;
    }
    Ok(())
}

/// Splits captured command output into logical lines for [`flush_lines`].
///
/// A trailing newline produces an empty final line, which would commit a blank
/// row and move the prompt down for no reason, so it is dropped.
fn output_lines(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.strip_suffix('\n').unwrap_or(&text);
    if trimmed.is_empty() {
        return Vec::new();
    }
    trimmed.lines().map(str::to_owned).collect()
}

/// Runs the model picker and applies the choice.
///
/// The list comes from the endpoint when it can be reached and from
/// configuration when it cannot, so a picker still offers something on an
/// offline machine. A chosen model is written back to the configuration, so the
/// next session starts on it, and is applied to this session immediately.
fn choose_model(
    reader: &mut rune_term::input::KeyReader,
    host: &SessionHost,
    out: &LiveSink,
    settings: &Settings,
    paths: &Paths,
) -> Result<()> {
    let catalog = match crate::provider_setup::fetch_catalog(settings, paths, MODEL_PICKER_TIMEOUT)
    {
        Ok(catalog) => catalog,
        // A picker that refuses to open because the endpoint is unreachable
        // leaves the user with nothing to do, so the configured model is
        // offered instead and the reason is shown. It is still enriched from the
        // cached catalog, so the window it reports is the model's own.
        Err(err) => {
            let mut sink = LockedSink {
                stream: Arc::clone(out),
            };
            let mut lines = vec![format!("could not list models: {}", err.message())];
            if let Some(hint) = err.hint() {
                lines.push(format!("hint: {hint}"));
            }
            flush_lines(host, &mut sink, &lines)?;
            let mut catalog = crate::provider_setup::catalog_for(settings);
            crate::provider_setup::enrich_with_capacity(settings, paths, &mut catalog);
            catalog
        }
    };

    let items: Vec<String> = catalog
        .models
        .iter()
        .map(|model| model.id.clone())
        .collect();
    let current = host.model_name();
    let picker = rune_term::picker::Picker::new(
        format!("models from {}", catalog.provider),
        items,
        rune_term::picker::DEFAULT_WINDOW,
    )
    .with_current(Some(current.as_str()));

    let Some(chosen) = run_picker(reader, host, out, picker, &catalog)? else {
        // Cancelling reports nothing: the status line still names the model in
        // effect, which is the answer to the question that was asked.
        return Ok(());
    };

    // The catalog is where a model's capacity is declared, so the window that
    // travels with the choice is the one the endpoint reported for it rather
    // than a number invented for it.
    let entry = catalog.models.iter().find(|model| model.id == chosen);
    let resolved = crate::model_limits::resolve(settings, paths, &chosen, entry);
    host.set_model(
        &chosen,
        resolved.context_window,
        resolved.max_output,
        entry.and_then(|model| model.prices),
    );
    // Written after the change takes effect, so a failed write cannot leave the
    // session on a model the file does not name.
    let selection = crate::provider_setup::Selection {
        provider: rune_core::config::provider_key(&settings.provider),
        model: Some(chosen.clone()),
        base_url: None,
    };
    let saved = crate::provider_setup::save_selection(paths, &selection);
    let mut sink = LockedSink {
        stream: Arc::clone(out),
    };
    report_model_choice(host, &mut sink, &chosen, saved.err().as_ref())
}

/// Reports a model choice through the renderer.
///
/// The picker runs with the terminal in raw mode, where a line written straight
/// to the stream lands at the caret inside the live region: the next frame
/// paints over it, and the status block it pushed down is left behind as a
/// second copy.
fn report_model_choice(
    host: &SessionHost,
    sink: &mut LockedSink,
    chosen: &str,
    unsaved: Option<&RuneError>,
) -> Result<()> {
    let line = match unsaved {
        None => format!("model set to {chosen}"),
        Some(err) => format!(
            "model set to {chosen} for this session; it could not be saved: {}",
            err.message()
        ),
    };
    flush_lines(host, sink, &[line])
}

/// Returns the prompts recorded for a workspace, oldest first.
///
/// A missing history is not an error: recall is a convenience, and a session
/// without it is still usable.
fn recorded_prompts(
    history: Option<&crate::prompt_history::History>,
    workspace: &Utf8Path,
) -> Vec<String> {
    history
        .map(|history| {
            history
                .for_workspace(workspace)
                .iter()
                .map(|entry| entry.text.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Renders the session's current state.
///
/// Reads nothing from disk, because the point of the command is to answer
/// questions about the running session rather than about the machine.
#[must_use]
fn render_status(info: &SessionInfo<'_>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "model          {}", info.model);
    let _ = writeln!(out, "provider       {}", info.provider);
    let _ = writeln!(out, "endpoint       {}", info.endpoint);
    let _ = writeln!(out, "permissions    {}", info.mode.label());
    let _ = writeln!(out, "effort         {}", info.effort);
    let _ = writeln!(out, "session        {}", info.session_id);
    let _ = writeln!(out, "workspace      {}", info.workspace);
    let _ = writeln!(out, "terminal       {}", info.terminal);
    let _ = writeln!(out, "colors         {}", info.depth.name());
    let _ = write!(
        out,
        "context        {}",
        footer::format_tokens(info.context_used)
    );
    if info.context_limit == 0 {
        // A window that is not known is left unnamed rather than shown as a
        // share of a number this program does not have.
        let _ = writeln!(out, " (window unknown)");
    } else {
        let percent = info
            .context_used
            .saturating_mul(100)
            .checked_div(info.context_limit)
            .unwrap_or(0)
            .min(100);
        let _ = writeln!(
            out,
            " of {} ({percent}%)",
            footer::format_tokens(info.context_limit)
        );
    }
    if let Some(source) = info.context_source {
        let _ = writeln!(out, "context source {source} (estimate)");
    } else {
        let _ = writeln!(out, "context source provider-reported");
    }
    out.trim_end().to_owned()
}

/// Renders what this session has spent.
///
/// Counted from saved and live turns rather than read back from the
/// ledger, because the ledger records which model served a request but not
/// which session asked for it, and a command that reported the machine's whole
/// spend would be answering a different question.
///
/// Only tokens are reported. No provider in this build reports a monetary cost,
/// so a currency figure here would be one this program invented.
#[must_use]
fn render_cost(info: &SessionInfo<'_>) -> String {
    let totals = &info.totals;
    if totals.requests == 0 {
        return "no requests have been made in this session yet".to_owned();
    }
    let dollars = |cost: Option<u64>| {
        cost.map_or_else(
            || "-".to_owned(),
            |cost| format!("${}", rune_net::models_dev::dollars(cost)),
        )
    };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "spent {} this session ({} requests, {} in, {} out)",
        dollars(totals.cost),
        totals.requests,
        footer::format_tokens(totals.input_tokens),
        footer::format_tokens(totals.output_tokens)
    );
    let row = |out: &mut String, model: &str, used: &ModelTotals| {
        let percent = (used.cached.saturating_mul(100))
            .checked_div(used.input)
            .unwrap_or(0);
        let _ = writeln!(
            out,
            "  {model}   {} req   in {}  cached {} ({percent}%)  out {}  reasoning {}   {}",
            used.requests,
            footer::format_tokens(used.input),
            footer::format_tokens(used.cached),
            footer::format_tokens(used.output),
            footer::format_tokens(used.reasoning),
            dollars(used.cost)
        );
    };
    for (model, used) in &totals.by_model {
        row(&mut out, model, used);
    }
    if !totals.helper_models.is_empty() {
        out.push_str("helpers\n");
        for (model, used) in &totals.helper_models {
            row(&mut out, model, used);
        }
    }
    if let Some(limit) = info.spend_limit {
        let left = limit.saturating_sub(totals.cost.unwrap_or(0));
        let _ = writeln!(
            out,
            "budget ${}: ${} left",
            rune_net::models_dev::dollars(limit),
            rune_net::models_dev::dollars(left)
        );
    }
    out.trim_end().to_owned()
}

/// Builds the `/context` rows for the request the next turn would send.
fn context_view(host: &SessionHost, history: &History) -> Result<Vec<String>> {
    if history.is_empty() {
        return Ok(vec!["no request has been sent yet".to_owned()]);
    }
    let mut copy = history.clone();
    let plan = turn::request_plan(host, &mut copy);
    let window = host.context_window().unwrap_or(0);
    let breakdown = rune_agent::tokens::context_breakdown(
        host.dialect(),
        &plan,
        Host::instruction_sizes(host),
        history.anchor(),
        rune_agent::tokens::ceiling(window, &host.limits),
    )?;
    Ok(render_context(
        &breakdown,
        host.width(),
        &host.theme(),
        host.depth,
        host.ascii,
    ))
}

/// Letter that marks each context part where colour is not available.
fn context_letter(part: &str) -> char {
    match part {
        "base_prompt" => 'b',
        "tool_schemas" => 's',
        "skill_catalog" => 'k',
        "project_instructions" => 'p',
        "repository_map" => 'm',
        "user_messages" => 'u',
        "assistant_text" => 'a',
        "reasoning" => 'r',
        "tool_results" => 't',
        "checkpoint" => 'c',
        _ => 'o',
    }
}

/// Colour slot of each context part in the bar.
const fn context_slot(letter: char) -> Slot {
    match letter {
        'b' => Slot::Accent,
        's' => Slot::Function,
        'k' => Slot::Keyword,
        'p' => Slot::Heading,
        'm' => Slot::Number,
        'u' => Slot::UserRail,
        'a' => Slot::Fg,
        'r' => Slot::Comment,
        't' => Slot::String,
        'c' => Slot::Warning,
        _ => Slot::Dim,
    }
}

/// Label of a context part in the listing.
fn context_label(part: &str) -> String {
    part.replace('_', " ")
}

/// Splits `cells` bar cells between the parts by their share of `ceiling`.
///
/// A part too small for one cell goes to `other`. The parts' cells and the
/// free cells always add up to `cells`.
fn context_cells(
    parts: &BTreeMap<&'static str, u64>,
    ceiling: u64,
    cells: usize,
) -> (Vec<(char, usize)>, usize) {
    let share = |tokens: u64| -> usize {
        let scaled = u128::from(tokens)
            .saturating_mul(cells as u128)
            .saturating_add(u128::from(ceiling / 2))
            .checked_div(u128::from(ceiling.max(1)))
            .unwrap_or(0);
        usize::try_from(scaled).unwrap_or(usize::MAX)
    };
    let mut other = parts.get("other").copied().unwrap_or(0);
    let mut drawn = Vec::new();
    for (part, tokens) in parts {
        if *part == "other" || *tokens == 0 {
            continue;
        }
        let count = share(*tokens);
        if count == 0 {
            other = other.saturating_add(*tokens);
        } else {
            drawn.push((context_letter(part), count));
        }
    }
    let other_cells = share(other);
    if other_cells > 0 {
        drawn.push(('o', other_cells));
    }
    // Rounding may overshoot a full window: trim the largest parts first.
    let mut used: usize = drawn.iter().map(|(_, count)| *count).sum();
    while used > cells {
        let Some(largest) = drawn.iter_mut().max_by_key(|(_, count)| *count) else {
            break;
        };
        largest.1 = largest.1.saturating_sub(1);
        used = used.saturating_sub(1);
    }
    drawn.retain(|(_, count)| *count > 0);
    (drawn, cells.saturating_sub(used))
}

/// Renders the `/context` view: a bar, the totals, and one row per part.
fn render_context(
    breakdown: &rune_agent::tokens::ContextBreakdown,
    width: u16,
    theme: &Theme,
    depth: ColorDepth,
    ascii: bool,
) -> Vec<String> {
    let reset = rune_term::engine::Style::RESET;
    let letters = depth == ColorDepth::Ansi16 || depth == ColorDepth::None;
    let total = breakdown.total_tokens;
    let estimated = if breakdown.anchored {
        ""
    } else {
        " (estimated)"
    };
    let mut rows = Vec::new();
    match breakdown.ceiling_tokens {
        Some(ceiling) => {
            let cells = usize::from(width.saturating_sub(2)).min(60);
            let (drawn, free) = context_cells(&breakdown.parts, ceiling, cells);
            let mut bar = String::new();
            for (letter, count) in drawn {
                let mark = if letters {
                    letter.to_string()
                } else if ascii {
                    "#".to_owned()
                } else {
                    "\u{2588}".to_owned()
                };
                let _ = write!(
                    bar,
                    "{}{}{reset}",
                    theme.sgr(context_slot(letter), depth),
                    mark.repeat(count)
                );
            }
            let gap = if ascii || letters { "." } else { "\u{2591}" };
            let _ = write!(
                bar,
                "{}{}{reset}",
                theme.sgr(Slot::Dim, depth),
                gap.repeat(free)
            );
            rows.push(bar);
            let percent = total.saturating_mul(100).checked_div(ceiling).unwrap_or(0);
            rows.push(format!(
                "{} of {} tokens ({percent}%){estimated} \u{b7} compaction at {} \u{b7} {} free",
                footer::format_tokens(total),
                footer::format_tokens(ceiling),
                footer::format_tokens(ceiling),
                footer::format_tokens(breakdown.free_tokens.unwrap_or(0))
            ));
        }
        None => rows.push(format!(
            "{} tokens{estimated}, window size unknown",
            footer::format_tokens(total)
        )),
    }
    let mut parts: Vec<(&&str, &u64)> = breakdown
        .parts
        .iter()
        .filter(|(_, tokens)| **tokens > 0)
        .collect();
    parts.sort_by(|left, right| right.1.cmp(left.1).then(left.0.cmp(right.0)));
    let percent_of = |tokens: u64| {
        if total == 0 {
            0.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            let share = tokens as f64 * 100.0 / total as f64;
            share
        }
    };
    for (part, tokens) in parts {
        let letter = context_letter(part);
        let mark = if letters {
            letter.to_string()
        } else if ascii {
            "#".to_owned()
        } else {
            "\u{2588}".to_owned()
        };
        rows.push(format!(
            "{}{mark}{reset} {:<20} {:>8} {:>5.1}%",
            theme.sgr(context_slot(letter), depth),
            context_label(part),
            tokens,
            percent_of(*tokens)
        ));
        if **part == *"tool_results" {
            let mut tools: Vec<(&String, &u64)> = breakdown.tool_results.iter().collect();
            tools.sort_by(|left, right| right.1.cmp(left.1).then(left.0.cmp(right.0)));
            for (tool, tokens) in tools.into_iter().take(5) {
                rows.push(format!(
                    "    {tool:<20} {tokens:>8} {:>5.1}%",
                    percent_of(*tokens)
                ));
            }
        }
    }
    rows
}

/// Shows the spend limit, or sets it from an amount in dollars, or turns it off.
/// Shows or sets whether sandboxed shell commands may use the network.
fn set_network(host: &SessionHost, requested: Option<&str>) -> String {
    use std::sync::atomic::Ordering;
    let state = |on: bool| if on { "on" } else { "off" };
    match requested {
        None => {
            let on = host.network.load(Ordering::Relaxed);
            let offline = if host.endpoint.offline {
                " (offline mode overrides it)"
            } else {
                ""
            };
            format!("shell network: {}{offline}", state(on))
        }
        Some(choice @ ("on" | "off")) => {
            let on = choice == "on";
            if host.network.swap(on, Ordering::Relaxed) != on {
                host.queue_update(
                    if on {
                        "Commands now have network access."
                    } else {
                        "Commands no longer have network access."
                    }
                    .to_owned(),
                );
            }
            format!("shell network: {} for this session", state(on))
        }
        Some(_) => "usage: /network on|off".to_owned(),
    }
}

fn set_budget(host: &SessionHost, requested: Option<&str>) -> String {
    let spent = host
        .totals
        .lock()
        .ok()
        .and_then(|totals| totals.cost)
        .map_or_else(
            || "nothing priced yet".to_owned(),
            |spent| format!("${} spent", rune_net::models_dev::dollars(spent)),
        );
    let Some(requested) = requested else {
        let limit = host.spend_limit.lock().ok().and_then(|limit| *limit);
        return match limit {
            Some(limit) => format!(
                "spend limit ${}; {spent}",
                rune_net::models_dev::dollars(limit)
            ),
            None => format!("no spend limit; {spent}"),
        };
    };
    let limit = if requested == "off" {
        None
    } else {
        let Some(micro) = requested
            .trim_start_matches('$')
            .parse::<f64>()
            .ok()
            .filter(|dollars| dollars.is_finite() && *dollars > 0.0 && *dollars < 1e9)
            .map(|dollars| (dollars * 1_000_000.0).round() as u64)
        else {
            return format!("`{requested}` is not an amount; use /budget 0.50 or /budget off");
        };
        Some(micro)
    };
    if let Ok(mut current) = host.spend_limit.lock() {
        *current = limit;
    }
    match limit {
        Some(limit) => format!(
            "spend limit set to ${}; {spent}",
            rune_net::models_dev::dollars(limit)
        ),
        None => format!("spend limit removed; {spent}"),
    }
}

/// Shows the reasoning effort, or sets it when a level is named.
///
/// Only a level the provider's dialect can express is accepted, so the
/// setting never claims an effort the request cannot carry.
/// Locks the session's rules, which a poisoned lock does not invalidate.
fn lock_rules(rules: &Mutex<RuleSet>) -> std::sync::MutexGuard<'_, RuleSet> {
    rules
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Shows the permission mode, or switches it for the calls that follow.
///
/// The rules are rebuilt from the session's settings for the new mode, so
/// the switch is the same as having started in it. Moving to a mode that
/// allows less also forgets what was approved for the session.
fn set_permissions(host: &SessionHost, requested: Option<&str>) -> String {
    let current = host.mode();
    let Some(requested) = requested else {
        return format!(
            "permissions are {}; choose ask, auto, or full-access",
            current.label()
        );
    };
    let Some(mode) = PermissionMode::from_name(requested) else {
        return format!("`{requested}` is not a permission mode; choose ask, auto, or full-access");
    };
    let mut settings = host.permission_settings.clone();
    settings.permission_mode = mode;
    let rules = match crate::permissions::validated(&settings) {
        Ok(rules) => rules,
        Err(err) => return format!("permissions unchanged: {err}"),
    };
    let rank = |mode: PermissionMode| match mode {
        PermissionMode::Ask => 0,
        PermissionMode::Auto => 1,
        PermissionMode::FullAccess => 2,
    };
    *lock_rules(&host.rules) = rules;
    *host
        .mode
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = mode;
    let name = match mode {
        PermissionMode::Ask => "ask",
        PermissionMode::Auto => "auto",
        PermissionMode::FullAccess => "full-access",
    };
    host.queue_update(format!("Permission mode is now {name}."));
    if rank(mode) < rank(current)
        && let Ok(mut grants) = host.grants.lock()
    {
        grants.clear();
    }
    format!(
        "permissions set to {} for the calls that follow",
        mode.label()
    )
}

fn set_effort(host: &SessionHost, requested: Option<&str>) -> String {
    let model = host.model_name();
    let (_, traits) = host.compat.resolve(&model);
    let accepted = if traits.efforts.is_empty() {
        host.dialect.capabilities().efforts
    } else {
        traits.efforts
    };
    let names = accepted
        .iter()
        .map(|effort| effort.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let Some(requested) = requested else {
        let (_, current) = host.effective_effort();
        return format!("effort is {current}; {model} accepts {names}");
    };
    let Some(level) = rune_net::provider::ALL_EFFORTS
        .iter()
        .copied()
        .find(|effort| effort.as_str() == requested)
    else {
        return format!("`{requested}` is not an effort this provider accepts; choose {names}");
    };
    *host
        .effort
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = level;
    host.effort_set
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let effective = rune_net::compat::clamp_effort(level, accepted);
    if effective == level {
        return format!("effort set to {level} for the requests that follow");
    }
    // The answer says it, so the next turn does not say it again.
    host.effort_noticed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert((model.clone(), level.as_str()));
    format!(
        "effort set to {effective} for the requests that follow ({level} is not accepted by {model})"
    )
}

/// Renders the settings that govern the running session.
#[must_use]
fn render_settings(info: &SessionInfo<'_>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "model          {}", info.model);
    let _ = writeln!(out, "provider       {}", info.provider);
    let _ = writeln!(out, "permissions    {}", info.mode.label());
    let _ = writeln!(out, "effort         {}", info.effort);
    let _ = write!(
        out,
        "context window {}",
        if info.context_limit == 0 {
            "unknown".to_owned()
        } else {
            footer::format_tokens(info.context_limit)
        }
    );
    let _ = writeln!(out);
    out.trim_end().to_owned()
}

/// Adopts the selected model's real context window.
///
/// The endpoint is asked what it serves and the published catalog fills in the
/// capacity it does not state. A failure is not reported and does not stop the
/// session: the window already in force, from the configuration or the compiled
/// default, is what applies, and reminding a user of a lookup they cannot act
/// on would only be noise at startup.
fn resolve_context_window(host: &SessionHost, settings: &Settings, paths: &Paths) {
    let Ok(catalog) = crate::provider_setup::fetch_catalog(settings, paths, CONTEXT_LOOKUP_TIMEOUT)
    else {
        return;
    };
    let current = host.model_name();
    let model = catalog.models.iter().find(|model| model.id == current);
    let resolved = crate::model_limits::resolve(settings, paths, &current, model);
    host.adopt_context_window(
        resolved.context_window,
        stated_before_listing(resolved.context_source),
    );
    host.vision.store(
        model.is_some_and(|model| model.capabilities.vision),
        std::sync::atomic::Ordering::Relaxed,
    );
    if let Some(prices) = model
        .and_then(|model| model.prices)
        .or_else(|| crate::provider_setup::cached_prices(settings, paths))
        && let Ok(mut current) = host.prices.lock()
    {
        *current = Some(prices);
    }
    if !stated_before_listing(resolved.output_source)
        && let Some(output) = resolved.max_output
    {
        host.output_limit
            .store(output, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Returns whether a limit came from a source the session already applied at
/// startup and a listing must not replace.
const fn stated_before_listing(source: crate::model_limits::LimitSource) -> bool {
    matches!(
        source,
        crate::model_limits::LimitSource::Setting
            | crate::model_limits::LimitSource::ConfiguredModel
    )
}

/// How long the session waits for the endpoint's model list at startup.
///
/// Short, because a user is waiting for a prompt: the figure is an improvement
/// on the compiled default rather than something the session cannot run without.
const CONTEXT_LOOKUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// How long the model picker waits for the endpoint's list.
///
/// Shorter than the command line's wait, because a user is watching a list
/// that has not appeared yet and configuration already offers a usable entry.
const MODEL_PICKER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Resolves the theme for a session.
///
/// A terminal that accepts no color gets the colorless theme whatever the
/// configuration names, because the setting expresses a preference and the
/// terminal states a capability.
fn resolve_theme(config: &SessionConfig, light: bool) -> (Theme, Option<String>) {
    resolve_theme_for(config, std::env::var_os("NO_COLOR").is_some(), light)
}

/// Resolves the theme for a stated terminal capability.
///
/// The capability is a parameter because reading it from the environment makes a
/// test that asserts the colorless path pass or fail depending on the shell it
/// happens to run in.
fn resolve_theme_for(
    config: &SessionConfig,
    accepts_no_color: bool,
    light: bool,
) -> (Theme, Option<String>) {
    if accepts_no_color {
        return (Theme::no_color(), None);
    }
    let name = config.settings.theme.as_deref();
    match theme_named(name, light, &config.paths) {
        Ok(theme) => (theme, None),
        Err(err) => {
            let (fallback, polarity) = if light {
                (Theme::light(), "light")
            } else {
                (Theme::dark(), "dark")
            };
            let warning = format!(
                "warning: theme {}: {}; using {polarity}",
                name.unwrap_or_default(),
                err.message()
            );
            (fallback, Some(warning))
        }
    }
}

/// Resolves a theme name against the configuration and data theme folders.
fn theme_named(name: Option<&str>, light: bool, paths: &Paths) -> Result<Theme> {
    Theme::try_resolve(
        name,
        light,
        &paths.config_root.join("themes"),
        &paths.themes_dir(),
    )
}

/// What `/theme` says where no picker can be drawn.
const THEME_USAGE: &str = "use /theme <name>: auto, dark, light, high-contrast, colorblind, ansi, or a file in the themes folder";

/// Lets the user pick a theme, previewing each highlighted one live.
///
/// Enter keeps and saves the highlighted theme; Esc and Ctrl-C put back the
/// theme in force before the picker. Returns the line to report, if any.
fn choose_theme(
    reader: &mut rune_term::input::KeyReader,
    host: &SessionHost,
    out: &LiveSink,
    paths: &Paths,
) -> Result<Option<String>> {
    let before = host.theme();
    let current = lock_value(&host.theme_name).clone();
    let mut picker = rune_term::picker::Picker::new(
        "themes",
        theme_names(paths),
        rune_term::picker::DEFAULT_WINDOW,
    )
    .with_current(Some(current.as_str()));
    let marker = rune_term::shell::prompt();
    let draft = reader.line().to_owned();
    reader.clear();
    let light = host.terminal_is_light();
    let preview = |picker: &rune_term::picker::Picker| {
        if before.base() == rune_term::theme::Base::Mono {
            return;
        }
        if let Some(name) = picker.selected()
            && let Ok(theme) = theme_named(Some(name), light, paths)
        {
            host.set_theme(theme);
        }
    };
    let chosen = loop {
        let room = host.menu_room();
        let below = picker_menu(&mut picker, &host.theme(), host.depth, room);
        draw_prompt(reader, host, out, &below, marker)?;
        match reader.read_key() {
            KeyAction::Submit | KeyAction::Complete => {
                if let Some(choice) = picker.selected().map(str::to_owned) {
                    break Some(choice);
                }
            }
            KeyAction::Interrupt | KeyAction::Cancel | KeyAction::Escape => break None,
            response @ (KeyAction::Up | KeyAction::Down) => {
                if response == KeyAction::Up {
                    picker.up();
                } else {
                    picker.down();
                }
                preview(&picker);
            }
            KeyAction::Transcript => view_transcript(reader, host)?,
            KeyAction::ExternalEditor | KeyAction::HistorySearch => {}
            KeyAction::Ignored => {
                picker.set_query(reader.line());
                preview(&picker);
            }
        }
    };
    reader.replace(&draft);
    if let Some(name) = chosen {
        return Ok(Some(set_theme_named(host, paths, &name)));
    }
    host.set_theme(before);
    Ok(None)
}

/// The built-in theme names the picker offers, before the user's own files.
const BUILTIN_THEME_NAMES: [&str; 6] = [
    "auto",
    "dark",
    "light",
    "high-contrast",
    "colorblind",
    "ansi",
];

/// Returns the names `/theme` offers: the built-ins, then the stems of the
/// TOML files in the configuration themes folder, sorted.
fn theme_names(paths: &Paths) -> Vec<String> {
    let mut own: Vec<String> = std::fs::read_dir(paths.config_root.join("themes"))
        .into_iter()
        .flatten()
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            name.strip_suffix(".toml").map(str::to_owned)
        })
        .collect();
    own.sort();
    BUILTIN_THEME_NAMES
        .iter()
        .map(|name| (*name).to_owned())
        .chain(own)
        .collect()
}

/// Switches to the named theme and saves it, or reports why it cannot be used
/// and keeps the current one.
fn set_theme_named(host: &SessionHost, paths: &Paths, name: &str) -> String {
    match theme_named(Some(name), host.terminal_is_light(), paths) {
        Ok(theme) => {
            // A terminal without colour stays without it, whatever is chosen.
            if host.theme().base() != rune_term::theme::Base::Mono {
                host.set_theme(theme);
            }
            name.clone_into(&mut lock_value(&host.theme_name));
            match crate::provider_setup::save_key(
                paths,
                "theme",
                Some(toml::Value::String(name.to_owned())),
            ) {
                Ok(()) => format!("theme {name}"),
                Err(err) => format!("theme {name} for this session; it was not saved: {err}"),
            }
        }
        Err(err) => format!("theme {name}: {}", err.message()),
    }
}

/// Returns the terminal width to compose for.
///
/// Read from the terminal rather than assumed. A width that disagrees with the
/// real one makes every row either wrap onto a row the renderer did not count,
/// or leave a gap, and both put later content in the wrong place.
fn terminal_width() -> u16 {
    rune_term::shell::terminal_size().map_or(80, |size| size.0)
}

/// Returns the terminal height to compose for.
///
/// A terminal that does not report a size gets a conventional height rather than
/// zero, which would compose a frame with no room for anything.
fn terminal_height() -> u16 {
    rune_term::shell::terminal_size().map_or(24, |size| size.1)
}

/// Returns the context window for the configured model.
///
/// Sizes the skill catalog budget from the model's context window.
///
/// A fixed budget is wrong at both ends: too small for a model with a large
/// window, wasting budget it has, and too large for a small one, crowding out
/// the conversation. A caller that named its own value keeps it, because a
/// limit someone set deliberately is not this program's to overwrite.
fn adopt_skill_catalog_budget(limits: &mut BudgetSet, context_window: Option<u64>) {
    use rune_core::budget::LimitName;
    use rune_core::config::Layer;

    if limits.source(LimitName::SkillCatalogBytes).is_some() {
        return;
    }
    let derived = rune_core::budget::skill_catalog_bytes(context_window);
    // A failure here would mean the derived value is outside the limit's own
    // range, which the derivation already clamps for, so the compiled default
    // stands rather than the session refusing to start. The value is reported
    // as a default, because nobody set it: naming the user file would send a
    // reader to look for a line that is not there.
    let _ = limits.set(
        LimitName::SkillCatalogBytes,
        rune_core::budget::Budget::Bounded(derived),
        Layer::Default,
    );
}

/// Adds a finished turn to the usage ledger.
///
/// A ledger write must never end a session, so a failure is reported and the
/// session continues: losing an accounting record is better than losing the
/// conversation, which is already on disk.
fn record_usage(paths: &Paths, model: &str, outcome: &turn::TurnOutcome, cost: Option<u64>) {
    let mut record = UsageRecord::new(now_ms(), model, HelperKind::Main);
    record.total_cost = cost.map(rune_net::models_dev::dollars);
    record.input_tokens = outcome.usage.input_tokens;
    record.output_tokens = outcome.usage.output_tokens;
    record.cache_read_tokens = outcome.usage.cache_read_tokens;
    record.cache_write_tokens = outcome.usage.cache_write_tokens;
    record.reasoning_tokens = outcome.usage.reasoning_tokens;
    let ledger = Ledger::from_paths(paths);
    if let Err(err) = ledger.append(&record) {
        // Reported on the error stream so a machine consumer reading stdout
        // still sees a clean conversation.
        let _ = writeln!(std::io::stderr(), "usage was not recorded: {err}");
    }
}

/// Sent with a prompt while the session is planning.
///
/// Carried in the user turn rather than the instructions, so entering and
/// leaving plan mode never changes the cached prefix of the request.
const PLAN_MODE_NOTE: &str = "<plan_mode>The session is in plan mode: changes and \
commands are refused. Investigate with reads, then record a step-by-step plan with \
update_plan and explain it. The user will leave plan mode to carry it out.</plan_mode>";

/// A call that is running, as the live region shows it.
#[derive(Clone, Debug)]
struct Running {
    id: String,
    label: String,
    started: std::time::Instant,
}

/// The objective the instructions carry, read once per prefix.
#[derive(Clone, PartialEq, Eq, Debug)]
enum FrozenObjective {
    /// Not read since the prefix was last rebuilt.
    Unread,
    /// Read, and fixed until the next rebuild.
    Fixed(Option<String>),
}

/// What a session's requests open with, saved or newly built.
struct Opening {
    instructions: String,
    stable_len: usize,
    tools: Vec<ToolSpec>,
    mapped: prompt::MappedInstructions,
    watched: context_watch::Watched,
}

/// What a queued update is about, so a rebuild can drop the ones it covers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum UpdateKind {
    /// An instruction file, keyed by its path.
    Instructions,
    /// The skill catalog.
    Skills,
    /// Anything else.
    Other,
}

/// One line the model has not been told yet.
#[derive(Clone, Debug)]
struct PendingUpdate {
    kind: UpdateKind,
    /// For an instruction file, its path: a later change replaces the line.
    path: Option<Utf8PathBuf>,
    line: String,
}

impl PendingUpdate {
    fn other(line: String) -> Self {
        Self {
            kind: UpdateKind::Other,
            path: None,
            line,
        }
    }

    fn skill(line: String) -> Self {
        Self {
            kind: UpdateKind::Skills,
            path: None,
            line,
        }
    }

    fn file(path: Utf8PathBuf, line: String) -> Self {
        Self {
            kind: UpdateKind::Instructions,
            path: Some(path),
            line,
        }
    }
}

/// Queues `update`, replacing an earlier line about the same file.
fn queue(pending: &Mutex<Vec<PendingUpdate>>, update: PendingUpdate) {
    let mut pending = lock_value(pending);
    if update.path.is_some() {
        pending.retain(|queued| queued.path != update.path);
    }
    pending.push(update);
}

/// Locks a mutex, recovering the value when a holder panicked.
fn lock_value<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What handling a slash command decided.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Handled {
    /// Carry on with the session.
    Continue,
    /// End the session.
    Exit,
    /// Place this text in the composer for review, with the model the next
    /// turn runs on when the command names one.
    Expand { text: String, model: Option<String> },
    /// Forget every recorded prompt.
    ClearHistory,
    /// Send later turns to this model.
    SetModel(String),
    /// Show the reasoning effort, or set it for the requests that follow.
    Effort(Option<String>),
    /// Show the permission mode, or switch it for the calls that follow.
    Permissions(Option<String>),
    /// Copy this conversation into a new session and continue in the copy.
    Fork,
    /// Enter plan mode, optionally with a task for the composer, or leave it.
    Plan(Option<String>),
    /// Show the spend limit, or set it in dollars, or turn it off.
    Budget(Option<String>),
    /// Show whether shell commands may use the network, or switch it.
    Network(Option<String>),
    /// Ask the user to choose a model from a list.
    PickModel,
    /// Put back the files the session changed.
    Undo,
    /// Show per-file diffs without changing files or undo records.
    UndoPreview,
    /// Summarize older turns to free the context window.
    Compact,
    /// Show exact summarized and retained record counts without compacting.
    CompactPreview,
    /// Pin an explicit user constraint, or list current pins.
    Pin(Option<String>),
    /// Rebuild the instructions, skills and repository map now.
    Reload,
    /// Switch to the named theme, or choose one.
    Theme(Option<String>),
    /// Show what fills the context window.
    Context,
    /// Put the last reply on the terminal's clipboard.
    Copy,
    /// Start a fresh conversation in this terminal.
    NewSession,
    /// Give the session a new title.
    Rename(String),
    /// Set an explicit objective, or inspect the current one without changes.
    Goal(Option<GoalRequest>),
}

/// What a `/goal` command with arguments asks for.
#[derive(Clone, PartialEq, Eq, Debug)]
struct GoalRequest {
    /// The new objective; absent when the judge is turned on for the active one.
    objective: Option<String>,
    /// The objective's token budget.
    budget: Option<u64>,
    /// Whether the objective is checked before a turn finishes.
    judge: bool,
}

/// Printed for a `/goal --judge` that names no usable objective.
const GOAL_JUDGE_USAGE: &str = "usage: /goal --judge [--budget <positive tokens>] <objective>";

/// Token and request totals for one session.
///
/// Summed here rather than read back from the ledger, which does not record
/// which session a request belonged to.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
struct Totals {
    /// Requests the session has made.
    requests: u64,
    /// Input tokens reported across those requests.
    input_tokens: u64,
    /// Output tokens reported across those requests.
    output_tokens: u64,
    /// Prompt-cache reads reported across those requests.
    cache_read_tokens: u64,
    /// Prompt-cache writes reported across those requests.
    cache_write_tokens: u64,
    /// Reasoning tokens reported across those requests.
    reasoning_tokens: u64,
    /// Estimated cost of the turns run since the session started, in
    /// millionths of a dollar, each priced when it ran. Absent until one is.
    cost: Option<u64>,
    /// The share of these totals each helper accounts for, by its label.
    helpers: BTreeMap<&'static str, HelperTotals>,
    /// The main requests of this session, by the model that served them.
    by_model: BTreeMap<String, ModelTotals>,
    /// The helper requests of this session, by model and helper label.
    helper_models: BTreeMap<String, ModelTotals>,
}

/// What one model has used in a session.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct ModelTotals {
    /// Requests made.
    requests: u64,
    /// Input tokens reported.
    input: u64,
    /// Input tokens read from the prompt cache.
    cached: u64,
    /// Output tokens reported.
    output: u64,
    /// Reasoning tokens reported.
    reasoning: u64,
    /// Estimated cost in millionths of a dollar. Absent until a request is priced.
    cost: Option<u64>,
}

impl ModelTotals {
    /// Adds one request.
    fn add(&mut self, usage: &rune_net::stream::Usage, cost: Option<u64>) {
        self.requests = self.requests.saturating_add(1);
        self.input = self.input.saturating_add(usage.input_tokens.unwrap_or(0));
        self.cached = self
            .cached
            .saturating_add(usage.cache_read_tokens.unwrap_or(0));
        self.output = self.output.saturating_add(usage.output_tokens.unwrap_or(0));
        self.reasoning = self
            .reasoning
            .saturating_add(usage.reasoning_tokens.unwrap_or(0));
        if let Some(cost) = cost {
            self.cost = Some(self.cost.unwrap_or(0).saturating_add(cost));
        }
    }
}

/// What one kind of helper request has used in a session.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct HelperTotals {
    /// Requests made.
    requests: u64,
    /// Input tokens reported across those requests.
    input_tokens: u64,
    /// Output tokens reported across those requests.
    output_tokens: u64,
    /// Prompt-cache reads reported across those requests.
    cache_read_tokens: u64,
    /// Prompt-cache writes reported across those requests.
    cache_write_tokens: u64,
    /// Estimated cost in millionths of a dollar. Absent until a request is priced.
    cost: Option<u64>,
}

impl Totals {
    /// Restores persisted counts without dropping usage covered by compaction.
    fn from_saved(state: &rune_session::store::SessionState) -> Self {
        use std::collections::BTreeSet;

        use rune_session::event::SessionEvent;

        // A legacy final message has no finish marker. Explicit boundaries
        // and streaming journals prevent partial answers from counting as one.
        let journalled: BTreeSet<_> = state
            .events
            .iter()
            .filter_map(|frame| match frame.event {
                SessionEvent::AssistantDelta { turn, .. }
                | SessionEvent::AssistantReset { turn }
                | SessionEvent::TurnFinished { turn }
                | SessionEvent::TurnCancelled { turn }
                | SessionEvent::TurnFailed { turn, .. }
                | SessionEvent::TurnInterrupted { turn } => Some(turn),
                _ => None,
            })
            .collect();
        let mut requests = BTreeSet::new();
        let mut current_turn = None;
        for frame in &state.events {
            match frame.event {
                SessionEvent::TurnStarted { turn } => current_turn = Some(turn),
                SessionEvent::TurnFinished { turn } => {
                    requests.insert(turn);
                }
                SessionEvent::AssistantMessage { turn, .. } if !journalled.contains(&turn) => {
                    requests.insert(turn);
                }
                SessionEvent::UsageRecorded { .. } => {
                    if let Some(turn) = current_turn {
                        requests.insert(turn);
                    }
                }
                _ => {}
            }
        }
        Self {
            requests: u64::try_from(requests.len()).unwrap_or(u64::MAX),
            input_tokens: state.usage.input_tokens,
            output_tokens: state.usage.output_tokens,
            ..Self::default()
        }
    }

    /// Adds one main request to the row of the model that served it.
    fn record_model(&mut self, model: &str, usage: &rune_net::stream::Usage, cost: Option<u64>) {
        self.by_model
            .entry(model.to_owned())
            .or_default()
            .add(usage, cost);
    }

    /// Adds one helper request to the session's totals and to its own label.
    fn record_helper(
        &mut self,
        label: &'static str,
        model: &str,
        usage: &rune_net::stream::Usage,
        cost: Option<u64>,
    ) {
        fn add(total: &mut u64, value: Option<u64>) {
            *total = total.saturating_add(value.unwrap_or(0));
        }
        fn add_cost(total: &mut Option<u64>, cost: u64) {
            *total = Some(total.unwrap_or(0).saturating_add(cost));
        }
        self.helper_models
            .entry(format!("{model} ({label})"))
            .or_default()
            .add(usage, cost);
        self.record(usage);
        let helper = self.helpers.entry(label).or_default();
        helper.requests = helper.requests.saturating_add(1);
        add(&mut helper.input_tokens, usage.input_tokens);
        add(&mut helper.output_tokens, usage.output_tokens);
        add(&mut helper.cache_read_tokens, usage.cache_read_tokens);
        add(&mut helper.cache_write_tokens, usage.cache_write_tokens);
        if let Some(cost) = cost {
            add_cost(&mut helper.cost, cost);
            add_cost(&mut self.cost, cost);
        }
    }

    /// Adds one turn's usage.
    ///
    /// A provider that reports no count leaves that field as it was rather than
    /// adding zero, so an absent figure does not look like a measured zero.
    fn record(&mut self, usage: &rune_net::stream::Usage) {
        self.requests = self.requests.saturating_add(1);
        if let Some(value) = usage.input_tokens {
            self.input_tokens = self.input_tokens.saturating_add(value);
        }
        if let Some(value) = usage.output_tokens {
            self.output_tokens = self.output_tokens.saturating_add(value);
        }
        if let Some(value) = usage.cache_read_tokens {
            self.cache_read_tokens = self.cache_read_tokens.saturating_add(value);
        }
        if let Some(value) = usage.cache_write_tokens {
            self.cache_write_tokens = self.cache_write_tokens.saturating_add(value);
        }
        if let Some(value) = usage.reasoning_tokens {
            self.reasoning_tokens = self.reasoning_tokens.saturating_add(value);
        }
    }
}

/// Returns the single file a content-rewriting tool call will change.
///
/// Renames are captured separately because they affect two paths. A tool that
/// changes something else, such as the shell, is not tracked: what
/// a shell command changed is not knowable from its arguments, and pretending
/// otherwise would offer an undo that does not undo.
fn mutating_path<'a>(name: &str, arguments: &'a serde_json::Value) -> Option<&'a Utf8Path> {
    if name != "write_file" && name != "edit_file" {
        return None;
    }
    arguments
        .get("path")
        .and_then(serde_json::Value::as_str)
        .map(Utf8Path::new)
}

fn preview_snapshot(path: &Utf8Path) -> Result<Option<Vec<u8>>> {
    use std::io::Read as _;
    let cap = rune_tools::mutation::MAX_WRITE_BYTES;
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() {
        return Err(RuneError::new(
            ErrorCode::UnsafePath,
            "undo preview requires a regular file",
        ));
    }
    let file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(RuneError::new(
            ErrorCode::UnsafePath,
            "undo preview requires a regular file",
        ));
    }
    if metadata.len() > cap as u64 {
        return Err(RuneError::too_large(
            "undo preview",
            cap.saturating_add(1),
            cap,
        ));
    }
    let mut bytes = Vec::new();
    file.take((cap as u64).saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() > cap {
        return Err(RuneError::too_large("undo preview", bytes.len(), cap));
    }
    Ok(Some(bytes))
}

/// Refuse a rename before mutation if its undo preimage cannot be captured.
fn rename_snapshot(path: &Utf8Path) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let cap = rune_tools::mutation::MAX_WRITE_BYTES;
    if !std::fs::symlink_metadata(path)?.file_type().is_file() {
        return Err(RuneError::new(
            ErrorCode::UnsafePath,
            "rename source must be a regular file",
        ));
    }
    let file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(RuneError::new(
            ErrorCode::UnsafePath,
            "rename source must be a regular file",
        ));
    }
    if metadata.len() > cap as u64 {
        return Err(RuneError::too_large(
            "rename snapshot",
            metadata.len() as usize,
            cap,
        ));
    }
    let mut bytes = Vec::new();
    file.take((cap as u64).saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() > cap {
        return Err(RuneError::too_large("rename snapshot", bytes.len(), cap));
    }
    Ok(bytes)
}

/// What the read-only commands report about the running session.
///
/// A struct rather than a list of arguments so adding a field does not change
/// every call site, and so a test can build one without a session.
struct SessionInfo<'a> {
    /// Model the session is sending to.
    ///
    /// Owned because it lives behind a lock that can change while the session
    /// runs, so it cannot be handed out as a reference.
    model: String,
    /// Provider name as written in configuration.
    provider: &'a str,
    /// Endpoint requests are sent to.
    endpoint: &'a str,
    /// Permission mode in force.
    mode: PermissionMode,
    /// Reasoning effort in force.
    effort: Effort,
    /// Session identifier.
    ///
    /// Owned because `/new` replaces it while the session runs, so it cannot be
    /// handed out as a reference.
    session_id: String,
    /// Workspace the session runs in.
    workspace: &'a str,
    /// What the terminal reported at startup, as one line.
    terminal: String,
    /// Color depth the session draws with.
    depth: ColorDepth,
    /// Tokens spent from the context window.
    context_used: u64,
    /// Source of an estimate, absent for live provider-reported readings.
    context_source: Option<&'static str>,
    /// Size of the context window, zero when none is known.
    context_limit: u64,
    /// What this session has spent so far.
    totals: Totals,
    /// The spend at which requests stop, in millionths of a dollar, when set.
    spend_limit: Option<u64>,
}

/// What the input loop does after handling one line.
enum Step {
    /// Carry on reading input.
    Continue,
    /// Leave the session.
    Exit,
    /// Ask the user to choose a model before continuing.
    PickModel,
    /// Ask the user to choose a theme.
    PickTheme,
    /// Put back the files the session changed.
    Undo,
}

/// Handles a slash command.
///
/// An unknown command is reported and the loop continues, because a typo should
/// not end a session.
fn handle_command<W: std::io::Write>(
    name: &str,
    arguments: &str,
    commands: &rune_context::commands::Discovery,
    history: Option<&crate::prompt_history::History>,
    self_workspace: &Utf8Path,
    info: &SessionInfo<'_>,
    output: &mut W,
) -> Result<Handled> {
    match name {
        "quit" | "exit" => Ok(Handled::Exit),
        // A model named inline is taken as given rather than checked against a
        // catalog, because an endpoint may serve a model it does not advertise
        // and a typo is visible in the status line right after.
        // `/models` is accepted as a spelling of `/model`, because reaching for
        // the plural is what most people do first and refusing it teaches
        // nothing.
        "model" | "models" => {
            let requested = arguments.trim();
            if requested.is_empty() {
                return Ok(Handled::PickModel);
            }
            Ok(Handled::SetModel(requested.to_owned()))
        }
        "budget" => Ok(Handled::Budget(
            (!arguments.trim().is_empty()).then(|| arguments.trim().to_owned()),
        )),
        "network" => Ok(Handled::Network(
            (!arguments.trim().is_empty()).then(|| arguments.trim().to_owned()),
        )),
        "plan" => Ok(Handled::Plan(
            (!arguments.trim().is_empty()).then(|| arguments.trim().to_owned()),
        )),
        "fork" => Ok(Handled::Fork),
        "permissions" => Ok(Handled::Permissions(
            (!arguments.trim().is_empty()).then(|| arguments.trim().to_owned()),
        )),
        "effort" => Ok(Handled::Effort(
            (!arguments.trim().is_empty()).then(|| arguments.trim().to_ascii_lowercase()),
        )),
        "compact" if arguments.trim() == "preview" => Ok(Handled::CompactPreview),
        "compact" => Ok(Handled::Compact),
        "reload" => Ok(Handled::Reload),
        "context" => Ok(Handled::Context),
        "theme" => Ok(Handled::Theme(
            (!arguments.trim().is_empty()).then(|| arguments.trim().to_owned()),
        )),
        "pin" => Ok(Handled::Pin(
            (!arguments.trim().is_empty()).then(|| arguments.to_owned()),
        )),
        // Read-only view of this saved branch. CLI forks create independent
        // sessions that can be selected with resume.
        "tree" => {
            let state = session_log::inspect(&Paths::from_process(), &info.session_id.parse()?)?;
            let tree = session_log::tree_of(&state);
            let _ = writeln!(output, "{}", session_log::render_tree(&tree, &state));
            Ok(Handled::Continue)
        }
        "copy" => Ok(Handled::Copy),
        "new" => Ok(Handled::NewSession),
        "undo" if arguments.trim() == "preview" => Ok(Handled::UndoPreview),
        "undo" => Ok(Handled::Undo),
        "rename" => {
            let title = arguments.trim();
            if title.is_empty() {
                let _ = writeln!(output, "usage: /rename <title>");
                return Ok(Handled::Continue);
            }
            Ok(Handled::Rename(title.to_owned()))
        }
        "goal" => {
            let arguments = arguments.trim();
            if arguments.is_empty() {
                return Ok(Handled::Goal(None));
            }
            let (first, remainder) = arguments
                .split_once(char::is_whitespace)
                .unwrap_or((arguments, ""));
            let judge = first == "--judge";
            let arguments = if judge { remainder.trim() } else { arguments };
            if judge && arguments.is_empty() {
                return Ok(Handled::Goal(Some(GoalRequest {
                    objective: None,
                    budget: None,
                    judge,
                })));
            }
            let mut words = arguments.splitn(2, char::is_whitespace);
            if words.next() == Some("--budget") {
                let rest = words.next().unwrap_or_default().trim();
                let mut words = rest.splitn(2, char::is_whitespace);
                let budget = words
                    .next()
                    .unwrap_or_default()
                    .parse::<u64>()
                    .ok()
                    .filter(|value| *value > 0);
                let objective = words.next().unwrap_or_default().trim();
                if let Some(budget) = budget.filter(|_| !objective.is_empty()) {
                    return Ok(Handled::Goal(Some(GoalRequest {
                        objective: Some(objective.to_owned()),
                        budget: Some(budget),
                        judge,
                    })));
                }
                let usage = if judge {
                    GOAL_JUDGE_USAGE
                } else {
                    "usage: /goal --budget <positive tokens> <objective>"
                };
                let _ = writeln!(output, "{usage}");
                return Ok(Handled::Continue);
            }
            Ok(Handled::Goal(Some(GoalRequest {
                objective: Some(arguments.to_owned()),
                budget: None,
                judge,
            })))
        }
        "status" => {
            let _ = writeln!(output, "{}", render_status(info));
            Ok(Handled::Continue)
        }
        "cost" => {
            let _ = writeln!(output, "{}", render_cost(info));
            Ok(Handled::Continue)
        }
        "settings" => {
            let _ = writeln!(output, "{}", render_settings(info));
            Ok(Handled::Continue)
        }
        "history" => {
            let Some(history) = history else {
                let _ = writeln!(output, "no prompt history is available");
                return Ok(Handled::Continue);
            };
            // `here` scopes recall to the workspace the session is running in,
            // which is what a composer offers; anything else is read as a
            // session identifier.
            let entries = match arguments.trim() {
                "" => history.entries(),
                "here" => history.for_workspace(self_workspace),
                "clear" => {
                    // Clearing needs the mutable handle, so it is handled by the
                    // caller, which owns it.
                    return Ok(Handled::ClearHistory);
                }
                session => history.for_session(session),
            };
            if entries.is_empty() {
                // The path is named so a user can find or remove the file, and
                // so an empty result is distinguishable from a missing file.
                let _ = writeln!(output, "no prompts recorded in {}", history.path());
            }
            for (index, entry) in entries.iter().enumerate() {
                let _ = writeln!(output, "{:>4}  {}", index.saturating_add(1), entry.text);
            }
            if !entries.is_empty() {
                let _ = writeln!(
                    output,
                    "\n{} prompt(s) recorded in {}",
                    history.len(),
                    history.path()
                );
            }
            Ok(Handled::Continue)
        }
        "help" => {
            // Read from the same table the dropdown offers, so a command that is
            // listed is one that works and the two cannot drift apart. The table
            // is longer than the summary the dropdown shows, so it is printed as
            // a whole rather than as one line.
            let _ = writeln!(output, "{}", rune_term::commands::render_help());
            // `clear` and the session argument are forms of `/history`, which
            // the table lists once; they are named here so they are findable.
            let _ = writeln!(output, "/history clear  forget every recorded prompt");
            let listing = rune_context::commands::render_listing(&commands.commands);
            let _ = writeln!(output, "{listing}");
            for line in commands
                .commands
                .iter()
                .filter_map(rune_context::commands::Command::ignored_notice)
            {
                let _ = writeln!(output, "{line}");
            }
            // A command file that was refused is invisible otherwise, and a
            // silent skip looks like a file that was never read.
            for warning in &commands.warnings {
                let _ = writeln!(output, "skipped {}: {}", warning.path, warning.reason);
            }
            Ok(Handled::Continue)
        }
        other => {
            let Some(command) = commands.commands.iter().find(|c| c.name == other) else {
                let _ = writeln!(output, "unknown command `/{other}`; try /help");
                return Ok(Handled::Continue);
            };
            let expanded = rune_context::commands::split_arguments(arguments)
                .and_then(|parts| command.expand(&parts));
            match expanded {
                Ok(text) => Ok(Handled::Expand {
                    text,
                    model: command.model.clone(),
                }),
                Err(err) => {
                    let _ = writeln!(output, "{err}");
                    if let Some(hint) = err.hint() {
                        let _ = writeln!(output, "hint: {hint}");
                    }
                    Ok(Handled::Continue)
                }
            }
        }
    }
}

/// Returns a human-readable headline for the current plan.
fn plan_summary(update: &rune_tools::PlanUpdate) -> String {
    let completed = update
        .plan
        .iter()
        .filter(|step| step.status == rune_tools::update_plan::Status::Completed)
        .count();
    format!(
        "plan version {}: {completed}/{} steps completed",
        update.version,
        update.plan.len()
    )
}

/// Builds the scrollback row for a finished call: status, tool, target, and
/// what it produced.
///
/// The body is what the model asked for and already has, so only its outcome
/// reaches the conversation; printing it again would bury the answer under the
/// material it was drawn from.
fn tool_row(
    name: &str,
    target: &str,
    output: &ToolOutput,
    elapsed: std::time::Duration,
    glyphs: RowGlyphs,
) -> String {
    let ascii = glyphs != RowGlyphs::Unicode;
    let glyph = match (output.is_error, glyphs) {
        (false, RowGlyphs::Unicode) => "\u{2713}",
        (true, RowGlyphs::Unicode) => "\u{2717}",
        (false, RowGlyphs::Ascii) => "+",
        (true, RowGlyphs::Ascii) => "x",
        (false, RowGlyphs::Words) => "done",
        (true, RowGlyphs::Words) => "failed",
    };
    let mut row = format!("{glyph} {name}");
    if !target.is_empty() {
        row.push(' ');
        row.push_str(target);
    }
    let mut details = Vec::new();
    let result = result_summary(name, output, ascii);
    if !result.is_empty() {
        details.push(result);
    }
    if elapsed >= std::time::Duration::from_millis(100) {
        details.push(format_elapsed(elapsed));
    }
    if !details.is_empty() {
        row.push_str(if ascii { " - " } else { " \u{b7} " });
        row.push_str(&details.join(if ascii { ", " } else { " \u{b7} " }));
    }
    row
}

/// How a finished call's status is marked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RowGlyphs {
    /// A check or a cross.
    Unicode,
    /// Plain characters, for a terminal limited to ASCII.
    Ascii,
    /// Words, for a screen reader, which reads a symbol by its name.
    Words,
}

/// Summarizes what a call produced in a few words.
/// Shortest pause between redraws of a running turn's live region.
const TICK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// Shortest time between two stream frames, about sixty a second.
const FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);

/// Changed lines drawn under an edit's tool row before the rest is counted.
const EDIT_PREVIEW_LINES: usize = 8;

/// Changed lines an approval prompt shows for an edit.
const APPROVAL_PREVIEW_LINES: usize = 12;

/// Returns the old and new text of each replacement an `edit_file` call asks for.
fn edit_pairs(arguments: &serde_json::Value) -> Vec<(String, String)> {
    let text = |value: &serde_json::Value, key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    match arguments.get("edits").and_then(serde_json::Value::as_array) {
        Some(edits) => edits
            .iter()
            .map(|edit| (text(edit, "old_string"), text(edit, "new_string")))
            .collect(),
        None => vec![(text(arguments, "old_string"), text(arguments, "new_string"))],
    }
}

fn result_summary(name: &str, output: &ToolOutput, ascii: bool) -> String {
    let lines = || {
        output
            .text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
    };
    let headline = lines().next().unwrap_or_default();
    if output.is_error {
        return shorten(headline, 96, ascii);
    }
    if output.text.starts_with(turn::REPEAT_MARKER) {
        return "unchanged, not re-run".to_owned();
    }
    match name {
        "shell" => {
            let state = output
                .text
                .lines()
                .rev()
                .map(str::trim)
                .find(|line| line.starts_with('[') && line.ends_with(']'))
                .map(|state| {
                    let state = &state[1..state.len().saturating_sub(1)];
                    state
                        .strip_prefix("exited with status ")
                        .map_or_else(|| shorten(state, 48, ascii), |code| format!("exit {code}"))
                })
                .unwrap_or_default();
            let tests = output
                .text
                .lines()
                .find_map(|line| line.strip_prefix("[tests: ")?.strip_suffix(']'));
            match tests {
                Some(tests) if !state.is_empty() => {
                    format!("{state}{}{tests}", if ascii { ", " } else { " \u{b7} " })
                }
                Some(tests) => tests.to_owned(),
                None => state,
            }
        }
        "read_file" => {
            let count = output
                .text
                .lines()
                .filter(|line| {
                    line.split_once('\t').is_some_and(|(number, _)| {
                        !number.trim().is_empty()
                            && number.trim().bytes().all(|b| b.is_ascii_digit())
                    })
                })
                .count();
            plural(count, "line")
        }
        "grep_files" | "glob_files" => {
            let count = lines().filter(|line| !line.starts_with('[')).count();
            if count == 0 {
                shorten(headline, 48, ascii)
            } else {
                plural(count, if name == "grep_files" { "line" } else { "path" })
            }
        }
        "apply_patch" => shorten(headline.trim_end_matches(':'), 64, ascii),
        // The detail reads like `1 replacement, +3 -1, lines 4-6`; the line
        // counts are the part a reader scans for.
        "edit_file" | "write_file" => headline
            .rsplit_once('(')
            .and_then(|(_, rest)| rest.strip_suffix(')'))
            .map_or_else(
                || shorten(headline, 64, ascii),
                |detail| {
                    let parts: Vec<&str> = detail.split(", ").collect();
                    parts
                        .iter()
                        .position(|part| part.starts_with("new file"))
                        .map(|at| {
                            parts[at..]
                                .iter()
                                .take(2)
                                .copied()
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .or_else(|| {
                            parts
                                .iter()
                                .find(|part| part.starts_with('+'))
                                .map(|part| (*part).to_owned())
                        })
                        .unwrap_or_else(|| detail.to_owned())
                },
            ),
        _ => shorten(headline, 64, ascii),
    }
}

/// Summarizes a finished turn in one line: what it did, what it cost in
/// tokens, and how long it took.
///
/// Omitted for a turn that used no tool and reported no usage, where the
/// answer is the whole story.
/// Completed and total steps of the conversation's plan, when it has one.
fn plan_progress(history: &History) -> Option<(usize, usize)> {
    let plan = history.plan();
    let done = plan
        .plan
        .iter()
        .filter(|step| step.status == rune_tools::update_plan::Status::Completed)
        .count();
    (!plan.plan.is_empty()).then_some((done, plan.plan.len()))
}

fn turn_summary(
    outcome: &turn::TurnOutcome,
    plan: Option<(usize, usize)>,
    elapsed: Option<std::time::Duration>,
    cost: Option<u64>,
    ascii: bool,
) -> Option<String> {
    let executed: Vec<&turn::CallResult> =
        outcome.calls.iter().filter(|call| call.executed).collect();
    let mut parts = Vec::new();
    if !executed.is_empty() {
        parts.push(plural(executed.len(), "call"));
    }
    let mut changed: Vec<String> = executed
        .iter()
        .filter(|call| {
            !call.output.is_error
                && matches!(
                    call.call.name.as_str(),
                    "write_file" | "edit_file" | "apply_patch" | "rename_file"
                )
        })
        .flat_map(|call| {
            let arguments =
                serde_json::from_str::<serde_json::Value>(&call.call.arguments).unwrap_or_default();
            if call.call.name == "apply_patch" {
                return arguments
                    .get("input")
                    .and_then(serde_json::Value::as_str)
                    .map(rune_tools::apply_patch::paths)
                    .unwrap_or_default();
            }
            arguments
                .get("path")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .into_iter()
                .collect()
        })
        .collect();
    changed.sort();
    changed.dedup();
    if !changed.is_empty() {
        parts.push(format!("{} changed", plural(changed.len(), "file")));
    }
    if let Some((done, total)) = plan {
        parts.push(format!("plan {done}/{total}"));
    }
    match outcome.verification {
        turn::Verification::NotApplicable => {}
        turn::Verification::Verified => parts.push("verified".to_owned()),
        turn::Verification::Checked => parts.push("checked".to_owned()),
        turn::Verification::Failed => parts.push("check failed".to_owned()),
        turn::Verification::Unverified => parts.push("unverified".to_owned()),
    }
    if let Some(judge) = &outcome.judge {
        parts.push(format!("judge {}", judge.rounds));
    }
    if let Some(input) = outcome.usage.input_tokens {
        let cached = outcome
            .usage
            .cache_read_tokens
            .filter(|cached| *cached > 0 && input > 0)
            .map(|cached| {
                format!(
                    " ({}% cached)",
                    cached
                        .saturating_mul(100)
                        .checked_div(input)
                        .unwrap_or_default()
                )
            })
            .unwrap_or_default();
        parts.push(format!("{} in{cached}", footer::format_tokens(input)));
    }
    if let Some(output) = outcome.usage.output_tokens {
        parts.push(format!("{} out", footer::format_tokens(output)));
    }
    if executed.is_empty() && parts.is_empty() {
        return None;
    }
    if let Some(cost) = cost {
        parts.push(format!("${}", rune_net::models_dev::dollars(cost)));
    }
    if let Some(elapsed) = elapsed {
        parts.push(format_elapsed(elapsed));
    }
    Some(parts.join(if ascii { ", " } else { " \u{b7} " }))
}

/// Counts something, in the singular when there is one.
fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// Renders a duration the way a person reads it.
fn format_elapsed(elapsed: std::time::Duration) -> String {
    let millis = elapsed.as_millis();
    if millis < 1000 {
        format!("{millis}ms")
    } else if millis < 60_000 {
        format!("{:.1}s", elapsed.as_secs_f64())
    } else {
        format!("{}m{:02}s", millis / 60_000, (millis / 1000) % 60)
    }
}

/// Cuts text to `width` columns, marking the cut.
fn shorten(text: &str, width: usize, ascii: bool) -> String {
    let marker = if ascii { "..." } else { "\u{2026}" };
    if rune_term::width::str_width(text) <= width {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used: usize = 0;
    let room = width.saturating_sub(rune_term::width::str_width(marker));
    for ch in text.chars() {
        let cell = rune_term::width::str_width(ch.encode_utf8(&mut [0; 4]));
        if used.saturating_add(cell) > room {
            break;
        }
        used = used.saturating_add(cell);
        out.push(ch);
    }
    out.push_str(marker);
    out
}

/// Renders what a turn produced, as the lines to print.
///
/// Returns the lines rather than writing them, because the renderer is the only
/// component that writes to the terminal. A line printed beside it would move
/// the rows the live region is drawn at, and in raw mode a bare newline moves
/// down without returning to the first column.
fn report_turn(
    outcome: &turn::TurnOutcome,
    host: &SessionHost,
    reasoning: &str,
) -> Result<Vec<String>> {
    // Earlier steps and their calls were written to scrollback as they
    // finished, so what remains is the last step: its reasoning, shown in its
    // own lane above the answer, and the answer itself.
    let mut entries = event_entries(host);
    if !reasoning.trim().is_empty() {
        entries.push(Entry::reasoning(reasoning.to_owned()));
    }

    if !outcome.text.is_empty() {
        entries.push(Entry::assistant(outcome.text.clone()));
    }

    match outcome.stop_reason {
        StopReason::ObjectiveBudget => {
            entries.push(Entry::notice("exhausted the objective token budget"));
        }
        StopReason::StepLimit => entries.push(Entry::notice("reached the model step limit")),
        StopReason::Cancelled => entries.push(Entry::notice("cancelled")),
        _ => {}
    }

    Ok(render_entries(&entries, host))
}

/// Renders a turn that ended in an error rather than an outcome.
///
/// What streamed before the failure is kept, because the reader watched it
/// arrive and a transcript that dropped it would disagree with the screen.
/// Interrupted answers are also saved and retained in the conversation.
fn report_failed_turn(
    err: &RuneError,
    host: &SessionHost,
    diagnostic: Option<&str>,
) -> Vec<String> {
    let mut entries = event_entries(host);
    let partial = host.partial_answer();
    if !partial.trim().is_empty() {
        entries.push(Entry::assistant(partial));
    }
    if let Some(diagnostic) = diagnostic {
        entries.push(Entry::notice(diagnostic));
    }
    if err.code() == ErrorCode::Cancelled {
        entries.push(Entry::notice("cancelled"));
    } else {
        entries.push(Entry::notice(format!("the turn failed: {}", err.message())));
        if let Some(hint) = err.hint() {
            entries.push(Entry::notice(hint.to_owned()));
        }
    }
    render_entries(&entries, host)
}

/// Keeps only the answer bytes the turn has not already added to history.
///
/// An interruption can follow completed model steps or a tool call, whose
/// assistant text is already in history. The live answer may include that
/// prefix, so appending it all would repeat those steps on the next request.
fn retain_partial_answer(history: &mut History, start: usize, partial: &str) {
    let recorded: String = history.turns()[start..]
        .iter()
        .filter(|turn| turn.role == rune_net::message::Role::Assistant)
        .map(|turn| transcript::sanitize(&turn.text()))
        .collect();
    let unrecorded = partial.strip_prefix(&recorded).unwrap_or(partial);
    if !unrecorded.trim().is_empty() {
        history.push_assistant(vec![rune_net::message::ContentPart::Text {
            text: unrecorded.to_owned(),
        }]);
    }
}

/// Returns the transcript entries for the events the running turn reported.
fn event_entries(host: &SessionHost) -> Vec<Entry> {
    let events = host
        .events
        .lock()
        .map(|events| events.clone())
        .unwrap_or_default();

    // Model and tool output both reach a terminal, so every entry is rendered
    // through the transcript, which strips the control sequences a terminal
    // would act on.
    let mut entries = Vec::new();
    // Calls and refusals reach scrollback as they happen; what is left for
    // the end of the turn is a summary of what was applied between steps.
    for event in &events {
        if let Event::SteeringApplied { count, .. } = event {
            entries.push(Entry::notice(format!("applied {count} queued message(s)")));
        }
    }
    entries
}

/// Renders transcript entries into terminal lines.
fn render_entries(entries: &[Entry], host: &SessionHost) -> Vec<String> {
    if host.accessible {
        return transcript::accessible_lines(entries);
    }
    // Measured against the terminal rather than a fixed width, so a line is
    // wrapped where the reader's own window wraps it instead of mid-word at a
    // column that has nothing to do with this terminal.
    let display = Display {
        width: usize::from(host.width()),
        ascii: host.ascii,
        ..Display::default()
    };
    let lanes = transcript::Lanes {
        reasoning: host.reasoning_style(),
        reset: rune_term::engine::Style::RESET.to_owned(),
        markdown: host.markdown_style(),
    };
    let rendered = transcript::render_lanes(entries, display, &lanes);
    rendered.lines().map(str::to_owned).collect()
}

/// Builds the prompt for a session that offers `tools`.
fn build_prompt(
    workspace: &Utf8Path,
    config_root: &Utf8Path,
    limits: &BudgetSet,
    tools: &[&str],
    variant: rune_core::experiment::PromptVariant,
) -> Prompt {
    prompt::base_prompt_with(
        workspace,
        config_root,
        limits,
        &prompt::PromptOptions { variant, tools },
    )
}

/// Builds the runtime configuration for an interactive session.
///
/// Split from the loop so the command surface can report a configuration problem
/// before a terminal is taken over.
pub fn prepare(
    settings: &Settings,
    paths: &Paths,
    workspace: &Utf8Path,
    resume: Option<SessionId>,
) -> Result<SessionConfig> {
    // A model is not required here. A session that has a terminal can ask for
    // one before its first turn, which is what makes `rune` alone work on a
    // connection that was made without naming a model. The refusal still
    // happens for a caller that cannot be asked, because a turn sent with an
    // empty identifier fails at the endpoint with nothing to act on.
    settings.require_provider()?;
    crate::provider_setup::validate_controls(settings)?;

    let endpoint = crate::provider_setup::configured_endpoint(settings, paths)?;

    let dialect = crate::provider_setup::dialect_for(&settings.provider);

    let questions = Arc::new(TerminalQuestions::default());
    let rules = crate::permissions::validated(settings)?;
    let helper_usage = crate::helper_usage::HelperUsage::default();
    let (registry, subagent, catalog) = session_registry(
        settings,
        paths,
        workspace,
        &rules,
        &questions,
        &helper_usage,
    )?;

    Ok(SessionConfig {
        helper_usage,
        accessible: false,
        settings: settings.clone(),
        paths: paths.clone(),
        resume,
        workspace: workspace.to_owned(),
        endpoint,
        dialect,
        registry,
        subagent: Some(subagent),
        catalog,
        // The rules that ship with the harness, so a fresh install has a usable
        // starting point: reads and in-workspace edits proceed, outbound traffic
        // is refused, and an unknown command resolves to the mode's default
        // rather than to nothing.
        rules,
        questions,
    })
}

/// Returns the names of advertised tools, in order.
pub fn tool_names(tools: &[ToolSpec]) -> Vec<&str> {
    tools.iter().map(|tool| tool.name.as_str()).collect()
}

/// Returns the tool schemas an interactive session in `workspace` advertises.
///
/// Built by the same function a session uses, so `rune prompt --tools` measures
/// what is sent rather than a reconstruction of it.
pub fn advertised_tools(
    settings: &Settings,
    paths: &Paths,
    workspace: &Utf8Path,
) -> Result<Vec<ToolSpec>> {
    let rules = crate::permissions::validated(settings)?;
    let questions = Arc::new(TerminalQuestions::default());
    let helper_usage = crate::helper_usage::HelperUsage::default();
    let (registry, _, _) = session_registry(
        settings,
        paths,
        workspace,
        &rules,
        &questions,
        &helper_usage,
    )?;
    Ok(inventory::advertisement(&registry))
}

/// Builds the tool registry an interactive session advertises.
fn session_registry(
    settings: &Settings,
    paths: &Paths,
    workspace: &Utf8Path,
    rules: &RuleSet,
    questions: &Arc<TerminalQuestions>,
    helper_usage: &crate::helper_usage::HelperUsage,
) -> Result<(
    Registry,
    Arc<crate::subagent::Bridge>,
    Arc<dyn rune_tools::skill::Catalog>,
)> {
    let catalog: Arc<dyn rune_tools::skill::Catalog> = Arc::new(rune_tools::skill::Fixed::new(
        rune_context::skills::discover(
            workspace,
            rune_core::paths::home_directory().as_deref(),
            &paths.config_root,
        )?,
    ));
    let mut registry = inventory::builtin_with_vision(
        &rune_tools::workspace::FileLimits::from_budget(&settings.limits),
        &settings.limits,
        &paths.managed_skills_dir(),
        crate::web_client::backends(settings),
        questions.clone(),
        Arc::clone(&catalog),
        crate::vision_client::backend(settings, paths, Some(helper_usage.clone()))?,
    )?;
    // Chosen once for the session, so the request prefix stays stable after `/model`.
    inventory::tailor(
        &mut registry,
        &inventory::Tailoring {
            workspace,
            interactive: true,
            edit_tool: rune_net::compat::edit_tool(settings.edit_tool, &settings.model),
        },
    );
    let subagent = crate::subagent::register(&mut registry, settings, paths, workspace, rules)?;
    if let Some(text) = rune_tools::tool_text::from_env()? {
        registry.set_tool_text(text)?;
    }
    Ok((registry, subagent, catalog))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune_term::shell::{ScriptedSource, Shell};

    #[test]
    fn a_rebuilt_prefix_keeps_the_tool_guidance_of_the_variant() {
        let root = tempfile::tempdir().expect("tempdir");
        let base = Utf8PathBuf::from_path_buf(root.path().to_owned()).expect("utf8");
        let workspace = base.join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let mut host = test_host();
        host.usage_paths = Some(Paths {
            config_root: base.join("config"),
            state_root: base.join("state"),
            data_root: base.join("data"),
        });
        host.context = ExecutionContext::new(workspace);
        host.permission_settings.experiments.prompt = rune_core::experiment::PromptVariant::V2Min;
        host.tools = ["read_file", "apply_patch"]
            .into_iter()
            .map(|name| ToolSpec {
                name: name.to_owned(),
                description: String::new(),
                input_schema: serde_json::json!({"type": "object"}),
            })
            .collect();
        host.rebuild_prefix();
        let instructions = host.instructions();
        let stable = &instructions[..host.stable_instructions_len()];
        assert!(stable.starts_with(prompt::SYSTEM_PROMPT_V2_MIN), "{stable}");
        assert!(
            stable.contains("Using the tools:\n- Search and read with read_file,"),
            "{stable}"
        );
        assert!(
            stable.ends_with("- and + lines, *** End Patch."),
            "{stable}"
        );
    }

    #[test]
    fn the_active_objective_is_in_model_instructions_after_restart() {
        let root = tempfile::tempdir().expect("tempdir");
        let base = Utf8PathBuf::from_path_buf(root.path().to_owned()).expect("utf8");
        let paths = Paths {
            config_root: base.join("config"),
            state_root: base.join("state"),
            data_root: base.join("data"),
        };
        let id = SessionId::generate();
        let recorder = Recorder::create(&paths, &id).expect("create");
        let mut host = test_host();
        host.instructions = Mutex::new("Workspace instructions".to_owned());
        host.recorder = Some(Arc::new(Mutex::new(recorder)));
        assert_eq!(host.instructions(), "Workspace instructions");
        host.recorder
            .as_ref()
            .expect("recorder")
            .lock()
            .expect("lock")
            .set_objective("Fix café parsing")
            .expect("goal");
        // The prefix is fixed until it is rebuilt, which writes the objective in.
        assert_eq!(host.instructions(), "Workspace instructions");
        host.rebuild_prefix();
        let expected = "Workspace instructions\n\nActive session objective:\nFix café parsing";
        assert_eq!(host.instructions(), expected);
        assert_eq!(host.instructions(), expected);
        host.recorder = None;
        host.recorder = Some(Arc::new(Mutex::new(
            Recorder::open(&paths, &id).expect("restart"),
        )));
        host.rebuild_prefix();
        assert_eq!(host.instructions(), expected);
        host.recorder = None;
        let fresh = Recorder::create(&paths, &SessionId::generate()).expect("new session");
        host.recorder = Some(Arc::new(Mutex::new(fresh)));
        host.rebuild_prefix();
        assert_eq!(host.instructions(), "Workspace instructions");
    }

    #[test]
    fn the_session_reports_its_instruction_sizes() {
        let root = tempfile::tempdir().expect("tempdir");
        let base = Utf8PathBuf::from_path_buf(root.path().to_owned()).expect("utf8");
        let workspace = base.join("work");
        std::fs::create_dir_all(workspace.join(".git")).expect("workspace");
        std::fs::create_dir_all(base.join("config")).expect("config");
        std::fs::write(workspace.join("AGENTS.md"), "A project rule.\n").expect("rules");
        let built = build_prompt(
            &workspace,
            &base.join("config"),
            &BudgetSet::new(),
            &[],
            rune_core::experiment::PromptVariant::default(),
        );
        let mut host = test_host();
        host.instructions = Mutex::new(built.instructions.clone());
        host.instruction_sizes = Mutex::new(crate::ask::instruction_sizes(&built));
        let sizes = Host::instruction_sizes(&host);
        assert!(sizes.base >= prompt::SYSTEM_PROMPT.len(), "{sizes:?}");
        assert!(sizes.project > "A project rule.".len(), "{sizes:?}");
        assert_eq!(sizes.map, 0, "{sizes:?}");
    }

    #[test]
    fn setting_a_goal_leaves_the_instructions_unchanged() {
        let root = tempfile::tempdir().expect("tempdir");
        let base = Utf8PathBuf::from_path_buf(root.path().to_owned()).expect("utf8");
        let paths = Paths {
            config_root: base.join("config"),
            state_root: base.join("state"),
            data_root: base.join("data"),
        };
        let recorder = Recorder::create(&paths, &SessionId::generate()).expect("create");
        let mut host = test_host();
        host.instructions = Mutex::new("Workspace instructions".to_owned());
        host.recorder = Some(Arc::new(Mutex::new(recorder)));
        let before = host.instructions();
        lock_value(host.recorder.as_ref().expect("recorder"))
            .set_objective("Ship it")
            .expect("goal");
        assert_eq!(host.instructions(), before);
    }

    fn appendix_text(host: &SessionHost) -> Option<String> {
        host.step_appendix(&History::new(), &[])
            .map(|appendix| appendix.text)
    }

    #[test]
    fn a_permission_switch_queues_one_update_line() {
        let host = test_host();
        let reply = set_permissions(&host, Some("full-access"));
        assert!(reply.starts_with("permissions set to"), "{reply}");
        assert_eq!(
            appendix_text(&host).as_deref(),
            Some("<context-update>\nPermission mode is now full-access.\n</context-update>")
        );
        assert_eq!(appendix_text(&host), None);
        assert!(set_permissions(&host, Some("nonsense")).contains("is not a permission mode"));
        assert_eq!(appendix_text(&host), None);
    }

    fn watched_workspace(rules: &str) -> (tempfile::TempDir, Utf8PathBuf, SessionHost) {
        let root = tempfile::tempdir().expect("tempdir");
        let base = Utf8PathBuf::from_path_buf(root.path().to_owned()).expect("utf8");
        let workspace = base.join("work");
        std::fs::create_dir_all(workspace.join(".git")).expect("workspace");
        std::fs::create_dir_all(base.join("config")).expect("config");
        std::fs::write(workspace.join("AGENTS.md"), rules).expect("rules");
        let host = test_host();
        *lock_value(&host.watched) = context_watch::Watched::of(&workspace, &base.join("config"));
        (root, workspace, host)
    }

    #[test]
    fn an_edited_instruction_file_is_sent_once_in_full() {
        let (_root, workspace, host) = watched_workspace("Use tabs.\n");
        assert_eq!(appendix_text(&host), None);
        std::fs::write(workspace.join("AGENTS.md"), "Use spaces, four of them.\n").expect("edit");
        let text = appendix_text(&host).expect("an update");
        assert!(
            text.contains("AGENTS.md changed on disk. Its rules are now:"),
            "{text}"
        );
        assert!(text.contains("Use spaces, four of them."), "{text}");
        assert!(!text.contains("Use tabs."), "{text}");
        assert_eq!(appendix_text(&host), None);

        std::fs::remove_file(workspace.join("AGENTS.md")).expect("delete");
        let text = appendix_text(&host).expect("an update");
        assert!(
            text.contains("was deleted; its rules no longer apply."),
            "{text}"
        );
        assert_eq!(appendix_text(&host), None);
    }

    #[test]
    fn a_large_edited_instruction_file_is_named_only() {
        let (_root, workspace, host) = watched_workspace("Short.\n");
        let large = "rule line\n".repeat(1_000);
        std::fs::write(workspace.join("AGENTS.md"), &large).expect("edit");
        let text = appendix_text(&host).expect("an update");
        assert!(
            text.contains("changed on disk; read it before relying on its rules."),
            "{text}"
        );
        assert!(!text.contains("rule line"), "{text}");
    }

    #[test]
    fn two_edits_of_one_file_queue_one_line_with_the_latest_text() {
        let (_root, workspace, host) = watched_workspace("One.\n");
        std::fs::write(workspace.join("AGENTS.md"), "Two two.\n").expect("edit");
        host.watch_instruction_files();
        std::fs::write(workspace.join("AGENTS.md"), "Three three three.\n").expect("edit");
        let text = appendix_text(&host).expect("an update");
        assert_eq!(text.matches("changed on disk").count(), 1, "{text}");
        assert!(text.contains("Three three three."), "{text}");
    }

    fn goal(objective: Option<&str>, budget: Option<u64>, judge: bool) -> Handled {
        Handled::Goal(Some(GoalRequest {
            objective: objective.map(str::to_owned),
            budget,
            judge,
        }))
    }

    #[test]
    fn a_turn_summary_counts_the_judge_rounds() {
        let outcome = turn::TurnOutcome {
            stop_reason: StopReason::Completed,
            text: String::new(),
            narration: Vec::new(),
            reasoning: String::new(),
            usage: rune_net::stream::Usage::default(),
            last_request: rune_net::stream::Usage::default(),
            steps: 2,
            calls: Vec::new(),
            repairs: Vec::new(),
            repaired_calls: 0,
            verification: turn::Verification::NotApplicable,
            checks: rune_agent::verify::CheckSummary::default(),
            error: None,
            judge: Some(turn::JudgeReport {
                rounds: 2,
                ..turn::JudgeReport::default()
            }),
        };
        let summary = turn_summary(&outcome, None, None, None, true).expect("summary");
        assert!(summary.contains("judge 2"), "{summary}");
    }

    #[test]
    fn a_judge_goal_with_a_bad_budget_prints_the_judge_usage() {
        let mut output = Vec::new();
        let handled = handle_command(
            "goal",
            "--judge --budget 0 work",
            &empty_commands(),
            None,
            Utf8Path::new("/w"),
            &test_info(),
            &mut output,
        )
        .expect("command");
        assert_eq!(handled, Handled::Continue);
        assert_eq!(
            String::from_utf8(output).expect("utf8"),
            format!("{GOAL_JUDGE_USAGE}\n")
        );
    }

    #[test]
    fn budgeted_goal_dispatch_requires_an_explicit_positive_budget_and_objective() {
        for (arguments, expected) in [
            ("", Handled::Goal(None)),
            (
                "Fix café parsing",
                goal(Some("Fix café parsing"), None, false),
            ),
            (
                "--budget 30 Fix café parsing",
                goal(Some("Fix café parsing"), Some(30), false),
            ),
            ("--judge", goal(None, None, true)),
            ("--judge Fix it", goal(Some("Fix it"), None, true)),
            (
                "--judge --budget 30 Fix it",
                goal(Some("Fix it"), Some(30), true),
            ),
            ("--judge --budget 0 work", Handled::Continue),
            ("--judge --budget 30", Handled::Continue),
            ("--budget 0 work", Handled::Continue),
            ("--budget -1 work", Handled::Continue),
            ("--budget 18446744073709551616 work", Handled::Continue),
            ("--budget nope work", Handled::Continue),
            ("--budget 30", Handled::Continue),
        ] {
            let mut output = Vec::new();
            assert_eq!(
                handle_command(
                    "goal",
                    arguments,
                    &empty_commands(),
                    None,
                    Utf8Path::new("/w"),
                    &test_info(),
                    &mut output
                )
                .expect("command"),
                expected
            );
            assert_eq!(!output.is_empty(), expected == Handled::Continue);
        }
    }

    #[test]
    fn objective_budget_stops_real_session_requests_after_resume() {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("address");
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().expect("accept");
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .expect("timeout");
                let mut reader = BufReader::new(&mut stream);
                let mut line = String::new();
                let mut length = 0;
                loop {
                    line.clear();
                    reader.read_line(&mut line).expect("header");
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().expect("length");
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).expect("request body");
                requests.push(serde_json::from_slice::<serde_json::Value>(&body).expect("JSON"));
                let response = concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"},\"finish_reason\":\"stop\"}]}\n\n",
                    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n",
                    "data: [DONE]\n\n"
                );
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).expect("response");
            }
            requests
        });
        let root = tempfile::tempdir().expect("root");
        let base = Utf8PathBuf::from_path_buf(root.path().to_owned()).expect("utf8");
        let paths = Paths {
            config_root: base.join("config"),
            state_root: base.join("state"),
            data_root: base.join("data"),
        };
        let id = SessionId::generate();
        let mut recorder = Recorder::create(&paths, &id).expect("create");
        recorder
            .set_budgeted_objective("fixture objective", 30)
            .expect("goal");
        let mut host = test_host();
        host.endpoint = Endpoint::new(format!("http://{address}/v1"), "test-key");
        host.recorder = Some(Arc::new(Mutex::new(recorder)));
        let mut history = History::new();
        for index in 1..=2 {
            let recorder = host.recorder.as_ref().expect("recorder");
            {
                let mut recorder = recorder.lock().expect("lock");
                recorder.user_message("work").expect("user");
                recorder.begin_turn().expect("begin");
            }
            history.push_user("work");
            let outcome = turn::run_turn(&mut history, &host).expect("turn");
            assert_eq!(
                outcome.stop_reason,
                if index == 1 {
                    StopReason::Completed
                } else {
                    StopReason::ObjectiveBudget
                }
            );
            recorder.lock().expect("lock").turn(&outcome).expect("save");
            host.recorder = None;
            let (recorder, resumed) = session_log::load(&paths, &id).expect("resume");
            assert_eq!(recorder.objective_budget(), Some((30, index * 15)));
            host.recorder = Some(Arc::new(Mutex::new(recorder)));
            history = resumed;
        }
        history.push_user("continue");
        let stopped = turn::run_turn(&mut history, &host).expect("stop before fetch");
        assert_eq!(stopped.stop_reason, StopReason::ObjectiveBudget);
        assert_eq!(stopped.steps, 0);
        assert!(
            report_turn(&stopped, &host, "")
                .expect("notice")
                .join("\n")
                .contains("exhausted the objective token budget")
        );
        let requests = server.join().expect("server");
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["max_tokens"], 30);
        assert_eq!(requests[1]["max_tokens"], 15);
        assert!(
            requests
                .iter()
                .all(|body| body.to_string().contains("fixture objective"))
        );
    }

    #[test]
    fn compaction_charges_the_objective_and_refuses_an_exhausted_budget() {
        let (mut host, mut history, _) = compaction_fixture();
        let root = tempfile::tempdir().expect("root");
        let base = Utf8PathBuf::from_path_buf(root.path().to_owned()).expect("utf8");
        let paths = Paths {
            config_root: base.join("config"),
            state_root: base.join("state"),
            data_root: base.join("data"),
        };
        let mut recorder = Recorder::create(&paths, &SessionId::generate()).expect("create");
        recorder.set_budgeted_objective("work", 1).expect("goal");
        host.recorder = Some(Arc::new(Mutex::new(recorder)));
        let client = SummaryFixture::new(CHECKPOINT_REPLY);
        let plan = rune_agent::compaction::plan(&history, &host.limits).expect("plan");
        compact_session(&host, &mut history, &plan, &client, false).expect("summary");
        assert_eq!(host.objective_tokens_remaining(), Some(0));
        let err = compact_session(&host, &mut history, &plan, &client, false)
            .expect_err("blocked summary");
        assert_eq!(
            err.detail().invariant.as_deref(),
            Some("objective_token_budget")
        );
        assert_eq!(client.requests.lock().expect("requests").len(), 1);
        let outcome = turn::run_turn(&mut history, &host).expect("blocked model");
        assert_eq!(outcome.stop_reason, StopReason::ObjectiveBudget);
        assert_eq!(outcome.steps, 0);
    }

    /// A writer that appends to a shared buffer, for asserting on what a draw
    /// actually wrote.
    struct SharedSink(Arc<Mutex<Vec<u8>>>);

    /// Lets the next stream frame draw at once, as though the frame interval
    /// had passed since the last one.
    fn end_frame_interval(host: &SessionHost) {
        *host.last_frame.lock().expect("last frame") = None;
    }

    impl std::io::Write for SharedSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_metadata_write_failure_keeps_showing_durable_text_and_reports_repair() {
        let root = tempfile::tempdir().expect("temp");
        let base = Utf8PathBuf::from_path_buf(root.path().to_owned()).expect("utf8");
        let paths = Paths {
            config_root: base.join("config"),
            state_root: base.join("state"),
            data_root: base.join("data"),
        };
        let id = SessionId::generate();
        let mut recorder = Recorder::create(&paths, &id).expect("created");
        recorder.user_message("slow").expect("prompt");
        recorder.begin_turn().expect("started");
        let metadata = paths.session_dir(&id).join("session.json");
        std::fs::remove_file(&metadata).expect("removed metadata");
        std::fs::create_dir(&metadata).expect("prevent metadata writes");
        let recorder = Arc::new(Mutex::new(recorder));
        let mut host = test_host();
        host.recorder = Some(Arc::clone(&recorder));
        host.emit(Event::TextDelta {
            delta: "durable".to_owned(),
        });
        assert!(!host.cancellation.is_cancelled());
        assert_eq!(host.partial_answer(), "durable");
        host.emit(Event::TextDelta {
            delta: " text".to_owned(),
        });
        assert_eq!(host.partial_answer(), "durable text");
        assert!(!host.cancellation.is_cancelled());
        let state = rune_session::load_read_only(&paths.session_dir(&id)).expect("log");
        assert_eq!(state.len(), 4);
        assert!(matches!(
            &state.events[2].event,
            rune_session::SessionEvent::AssistantDelta { text, .. } if text == "durable"
        ));
        assert!(matches!(
            &state.events[3].event,
            rune_session::SessionEvent::AssistantDelta { text, .. } if text == " text"
        ));
        assert!(recorder.lock().expect("lock").check_journal().is_ok());

        let sink = Arc::new(Mutex::new(Vec::new()));
        let mut locked = LockedSink {
            stream: Arc::new(Mutex::new(SharedSink(Arc::clone(&sink)))),
        };
        let mut reader = rune_term::input::KeyReader::line_mode();
        close_turn(&host, &mut locked, &mut reader, &[], None, &[]).expect("close turn");
        let output = String::from_utf8(sink.lock().expect("lock").clone()).expect("utf8");
        let output = transcript::sanitize(&output)
            .replace(['\r', '\n'], "")
            .replace("][", " ");
        assert!(output.contains("is durable"));
        assert!(output.contains("metadata refresh failed"), "{output:?}");
        assert!(output.contains("do not retry this event"));
        assert!(
            recorder
                .lock()
                .expect("lock")
                .take_metadata_warning()
                .is_none()
        );
    }

    #[test]
    fn a_streamed_delta_is_drawn_before_the_turn_ends() {
        // The point of streaming: text appears while it is being produced. A
        // delta that only reached the screen once the turn finished would look
        // identical on the final screen, so the assertion is that a write
        // happened while the turn was still running.
        let sink: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let host = test_host();
        if let Ok(mut slot) = host.live_out.lock() {
            *slot = Some(Arc::new(Mutex::new(SharedSink(Arc::clone(&sink)))));
        }

        host.emit(Event::TextDelta {
            delta: "streamed".to_owned(),
        });

        let written = String::from_utf8_lossy(&sink.lock().expect("lock")).into_owned();
        assert!(
            written.contains("streamed"),
            "the delta was not drawn when it arrived: {written:?}"
        );
    }

    #[test]
    fn transcript_ownership_suppresses_streaming_frames_and_restores_after_drop() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let host = test_host();
        *host.live_out.lock().expect("lock") =
            Some(Arc::new(Mutex::new(SharedSink(Arc::clone(&bytes)))));
        host.transcript_open
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let screen = TranscriptScreen(&host);
        host.emit(Event::TextDelta {
            delta: String::from("LIVE-ANSWER"),
        });
        assert!(
            bytes.lock().expect("lock").is_empty(),
            "stream painted over the viewer"
        );
        assert_eq!(host.partial_answer(), "LIVE-ANSWER");
        drop(screen);
        assert!(
            !host
                .transcript_open
                .load(std::sync::atomic::Ordering::Relaxed)
        );
        assert!(String::from_utf8_lossy(&bytes.lock().expect("lock")).contains("\x1b[?1049l"));
        bytes.lock().expect("lock").clear();
        host.draw_stream_with("saved draft", 3);
        let written = String::from_utf8_lossy(&bytes.lock().expect("lock")).into_owned();
        assert!(written.contains("LIVE-ANSWER"), "{written:?}");
        assert!(written.contains("saved draft"), "{written:?}");
    }

    #[test]
    fn full_transcript_keeps_recorded_calls_replies_and_every_tool_row() {
        use rune_agent::history::Turn;
        use rune_core::id::ToolCallId;
        use rune_net::message::{ContentPart, Role};

        let mut content = String::new();
        for row in 1..=40 {
            writeln!(content, "TOOL-{row:02}").expect("tool row");
        }
        let turns = vec![
            Turn::user(1, "inspect"),
            Turn::assistant(
                2,
                vec![ContentPart::ToolCall {
                    id: ToolCallId::new("read").expect("tool id"),
                    name: String::from("read_file"),
                    arguments: String::from("{\"path\":\"fixture\"}"),
                }],
            ),
            Turn::new(
                3,
                Role::Tool,
                vec![ContentPart::ToolResult {
                    id: ToolCallId::new("read").expect("tool id"),
                    name: String::from("read_file"),
                    content,
                    is_error: false,
                }],
            ),
            Turn::assistant(
                4,
                vec![ContentPart::Text {
                    text: String::from("REPLY-END\x1b[2J"),
                }],
            ),
        ];
        let entries = history_entries(&turns);
        let rows = full_transcript_rows(&entries, 80, false);
        let rendered = rows.join("\n");
        assert!(rendered.starts_with("> inspect"));
        assert!(rendered.contains("read_file {\"path\":\"fixture\"}"));
        for row in 1..=40 {
            assert!(
                rendered.contains(&format!("TOOL-{row:02}")),
                "missing row {row}"
            );
        }
        assert!(rendered.ends_with("REPLY-END"));
        assert!(!rendered.contains("more line(s)"));
        assert!(!rendered.contains("\x1b[2J"));
        let mut view = rune_term::screen::Transcript::new(rows);
        assert!(view.visible(8).iter().any(|row| row.contains("inspect")));
        view.page_down(8);
        assert!(view.visible(8).iter().any(|row| row.contains("TOOL-10")));
        view.jump(usize::MAX, 8);
        assert!(view.visible(8).iter().any(|row| row.contains("REPLY-END")));
    }

    /// Returns the one-based column of the last caret move in a frame.
    fn last_caret_column(bytes: &[u8]) -> Option<usize> {
        let text = String::from_utf8_lossy(bytes);
        let end = text.rfind('G')?;
        let start = text.get(..end)?.rfind("\u{1b}[")?.saturating_add(2);
        text.get(start..end)?.parse().ok()
    }

    #[test]
    fn deltas_inside_the_frame_interval_mark_the_frame_dirty() {
        let host = test_host();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let out: LiveSink = Arc::new(Mutex::new(SharedSink(Arc::clone(&bytes))));
        *host.live_out.lock().expect("output lock") = Some(out);
        let written = || {
            String::from_utf8_lossy(&std::mem::take(&mut *bytes.lock().expect("bytes")))
                .into_owned()
        };
        let set_last = |at: Option<std::time::Instant>| {
            *host.last_frame.lock().expect("last frame") = at;
        };
        host.emit(Event::TextDelta {
            delta: "first".to_owned(),
        });
        let first = written();
        assert!(first.contains("first"), "{first:?}");
        assert!(
            first.starts_with(rune_term::inline::BEGIN_SYNC),
            "{first:?}"
        );
        // Holds the interval open however slowly this test runs.
        set_last(std::time::Instant::now().checked_add(std::time::Duration::from_secs(60)));
        host.emit(Event::TextDelta {
            delta: " second".to_owned(),
        });
        assert_eq!(written(), "");
        assert!(host.frame_dirty.load(std::sync::atomic::Ordering::Relaxed));
        host.flush_frame();
        assert_eq!(written(), "");
        set_last(std::time::Instant::now().checked_sub(FRAME_INTERVAL));
        host.flush_frame();
        let flushed = written();
        assert!(flushed.contains("first second"), "{flushed:?}");
        assert_eq!(flushed.matches(rune_term::inline::BEGIN_SYNC).count(), 1);
        assert!(!host.frame_dirty.load(std::sync::atomic::Ordering::Relaxed));
        set_last(std::time::Instant::now().checked_sub(FRAME_INTERVAL));
        host.flush_frame();
        assert_eq!(written(), "");
        // A key draws at once, inside the interval, and clears a pending frame.
        set_last(std::time::Instant::now().checked_add(std::time::Duration::from_secs(60)));
        host.emit(Event::TextDelta {
            delta: " third".to_owned(),
        });
        host.draw_stream_with("Z", 1);
        let keyed = written();
        assert!(keyed.contains("first second third"), "{keyed:?}");
        assert!(keyed.contains("> Z"), "{keyed:?}");
        assert!(!host.frame_dirty.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn an_idle_choice_redraw_that_changes_nothing_writes_nothing() {
        let host = test_host();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let out: LiveSink = Arc::new(Mutex::new(SharedSink(Arc::clone(&bytes))));
        *host.live_out.lock().expect("output lock") = Some(out);
        let written = || std::mem::take(&mut *bytes.lock().expect("bytes"));
        let mut picker = rune_term::picker::Picker::new(
            "permission required",
            vec!["run once".to_owned(), "deny".to_owned()],
            2,
        );
        draw_choice(&host, &picker, &["shell ls".to_owned()], false).expect("drawn");
        assert!(String::from_utf8_lossy(&written()).contains("permission required"));
        // The settled lines scrolled the region, so the first redraw repaints it.
        draw_choice(&host, &picker, &[], true).expect("drawn");
        assert!(!written().is_empty());
        for _ in 0..3 {
            draw_choice(&host, &picker, &[], true).expect("drawn");
            assert!(written().is_empty());
        }
        assert!(picker.to(1));
        draw_choice(&host, &picker, &[], true).expect("drawn");
        assert!(!written().is_empty());
    }

    #[test]
    fn accessible_frames_append_settled_lines_and_ignore_live_regions() {
        let mut host = test_host();
        host.accessible = true;
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let out: LiveSink = Arc::new(Mutex::new(SharedSink(Arc::clone(&bytes))));
        *host.live_out.lock().expect("output lock") = Some(out);
        let rows = vec!["live answer".to_owned()];
        for _ in 0..10 {
            host.emit(Event::TextDelta {
                delta: "streamed ".to_owned(),
            });
            assert!(
                host.paint_with_menu(&[], None, &rows, &rows, &rows, (2, 5), false)
                    .expect("paint")
                    .is_empty()
            );
        }
        assert!(bytes.lock().expect("bytes").is_empty());
        let lines = render_entries(&[Entry::assistant("finished\nnext line")], &host);
        assert_eq!(
            host.paint(&lines, None, &[], &[], (0, 0)).expect("paint"),
            b"Assistant: finished\nAssistant: next line\n"
        );
        assert!(host.clear_region().expect("clear").is_empty());
        assert!(!rune_term::input::KeyReader::line_mode().is_active());
    }

    #[test]
    fn a_long_draft_stays_visible_as_a_turn_streams_and_closes() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let out: LiveSink = Arc::new(Mutex::new(SharedSink(Arc::clone(&bytes))));
        let host = test_host();
        *host.live_out.lock().expect("lock") = Some(Arc::clone(&out));
        let mut reader = rune_term::input::KeyReader::new();
        reader.replace(&("a".repeat(160) + "TAIL-END"));
        let mut grid = rune_term::Grid::new(80, 24).expect("grid");
        let check = |grid: &mut rune_term::Grid| {
            let drawn = std::mem::take(&mut *bytes.lock().expect("lock"));
            assert!(last_caret_column(&drawn).expect("caret") <= 80);
            grid.feed(&drawn).expect("feed");
            assert!(grid.text().contains("TAIL-END"), "{}", grid.text());
            assert_eq!(grid.cursor().col, 79);
        };
        host.draw_stream_with(reader.line(), reader.column());
        check(&mut grid);
        end_frame_interval(&host);
        host.emit(Event::TextDelta {
            delta: "answer".to_owned(),
        });
        check(&mut grid);
        host.draw_notice("still working");
        check(&mut grid);
        let mut sink = LockedSink { stream: out };
        close_turn(
            &host,
            &mut sink,
            &mut reader,
            &["answer".to_owned()],
            None,
            &[],
        )
        .expect("closed");
        check(&mut grid);
        assert_eq!(reader.line(), "a".repeat(160) + "TAIL-END");
    }

    #[test]
    fn multiline_caret_survives_streaming_notices_and_turn_close() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let out: LiveSink = Arc::new(Mutex::new(SharedSink(Arc::clone(&bytes))));
        let host = test_host();
        *host.live_out.lock().expect("lock") = Some(Arc::clone(&out));
        let mut reader = rune_term::input::KeyReader::new();
        reader.paste("first 界\nsecond e\u{301}\nthird 👩‍💻");
        let mut grid = rune_term::Grid::new(80, 24).expect("grid");
        let check = |grid: &mut rune_term::Grid, edited_row: u16| {
            let drawn = std::mem::take(&mut *bytes.lock().expect("lock"));
            grid.feed(&drawn).expect("feed");
            let caret = grid.cursor();
            let first = caret.row.saturating_sub(edited_row);
            assert_eq!(grid.row_text(first), "> first 界");
            assert_eq!(grid.row_text(first + 1), "  second e\u{301}");
            assert_eq!(grid.row_text(first + 2), "  third 👩‍💻");
            assert_eq!(caret.col, 10);
        };
        let column =
            rune_term::width::str_width(&rune_term::editor::displayed("first 界\nsecond e\u{301}"));
        host.draw_stream_with(reader.line(), column);
        check(&mut grid, 1);
        host.emit(Event::TextDelta {
            delta: "answer".to_owned(),
        });
        check(&mut grid, 1);
        host.draw_notice("still working");
        check(&mut grid, 1);
        let mut sink = LockedSink { stream: out };
        close_turn(
            &host,
            &mut sink,
            &mut reader,
            &["answer".to_owned()],
            None,
            &[],
        )
        .expect("closed");
        check(&mut grid, 2);
        draw_prompt(&reader, &host, &sink.stream, &[], "> ").expect("idle prompt");
        check(&mut grid, 2);
        reader.clear();
        draw_prompt(&reader, &host, &sink.stream, &[], "> ").expect("cleared");
        grid.feed(&std::mem::take(&mut *bytes.lock().expect("lock")))
            .expect("feed");
        assert!(!grid.text().contains("second"), "{}", grid.text());
        assert!(!grid.text().contains("third"), "{}", grid.text());
    }

    #[test]
    fn a_delta_keeps_the_caret_where_the_reader_left_it() {
        // The caret is placed at the reader's display column. The character
        // count of the line is wrong for a caret moved back into the line or
        // after a wide character, and the caret would jump on every token.
        let sink: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let host = test_host();
        if let Ok(mut slot) = host.live_out.lock() {
            *slot = Some(Arc::new(Mutex::new(SharedSink(Arc::clone(&sink)))));
        }
        let marker = rune_term::width::str_width(rune_term::shell::prompt());

        for (line, column) in [("abcd", 1), ("書書", 4)] {
            host.draw_stream_with(line, column);
            sink.lock().expect("lock").clear();
            end_frame_interval(&host);
            host.emit(Event::TextDelta {
                delta: "token ".to_owned(),
            });
            let drawn = sink.lock().expect("lock").clone();
            assert_eq!(
                last_caret_column(&drawn),
                Some(marker + column + 1),
                "the caret moved for {line:?}: {:?}",
                String::from_utf8_lossy(&drawn)
            );
        }
    }

    #[test]
    fn clearing_the_line_before_anything_streams_takes_it_off_the_screen() {
        // With nothing streamed yet and no notice, the frame for an emptied
        // line still has to be painted, or the cleared text stays on screen.
        let sink: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let host = test_host();
        if let Ok(mut slot) = host.live_out.lock() {
            *slot = Some(Arc::new(Mutex::new(SharedSink(Arc::clone(&sink)))));
        }
        host.draw_stream_with("half a correction", 17);
        host.draw_stream_with("", 0);

        let mut grid = rune_term::Grid::new(80, 24).expect("grid");
        grid.feed(&sink.lock().expect("lock")).expect("feed");
        let screen = grid.text();
        assert!(
            !screen.contains("half a correction"),
            "the cleared line is still on screen:\n{screen}"
        );
    }

    #[test]
    fn a_restarted_step_drops_the_text_of_the_failed_attempt() {
        // The retry streams the answer again from the start, so keeping the
        // failed attempt's text would show the answer's opening twice.
        let host = test_host();
        host.emit(Event::TextDelta {
            delta: "partial".to_owned(),
        });
        host.emit(Event::StepRestarted { step: 1 });
        host.emit(Event::TextDelta {
            delta: "complete".to_owned(),
        });
        let answer = host.streaming.lock().expect("lock").answer.clone();
        assert_eq!(answer, "complete");
    }

    #[test]
    fn retry_frames_replace_one_status_row_and_preserve_the_draft() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let host = test_host();
        *host.live_out.lock().expect("output") =
            Some(Arc::new(Mutex::new(SharedSink(Arc::clone(&bytes)))));
        host.draw_stream_with("saved draft", 5);
        let mut grid = rune_term::Grid::new(80, 24).expect("grid");
        let replay = |grid: &mut rune_term::Grid| {
            grid.feed(&std::mem::take(&mut *bytes.lock().expect("bytes")))
                .expect("feed");
            let screen = grid.text();
            assert_eq!(screen.matches("ctrl-c cancel").count(), 1, "{screen}");
            assert!(screen.contains("> saved draft"), "{screen}");
            assert_eq!(grid.cursor().col, 7);
            screen
        };
        replay(&mut grid);
        for (attempt, delay) in [(2, 250), (3, 500)] {
            end_frame_interval(&host);
            host.emit(Event::ProviderRetry {
                step: 1,
                next_attempt: attempt,
                max_attempts: 3,
                delay: std::time::Duration::from_millis(delay),
                retry_at: std::time::SystemTime::now(),
                kind: "rate_limited",
            });
            let screen = replay(&mut grid);
            assert_eq!(screen.matches("provider retry").count(), 1, "{screen}");
            assert!(
                screen.contains(&format!("provider retry {attempt}/3 in {delay}ms")),
                "{screen}"
            );
            end_frame_interval(&host);
            host.emit(Event::StepRestarted { step: 1 });
            let screen = replay(&mut grid);
            assert!(
                screen.contains(&format!("provider retry {attempt}/3 |")),
                "{screen}"
            );
            assert!(!screen.contains("ms"), "{screen}");
        }
        // A successful request clears the row before tool approval can begin.
        // A new step and finalization also clear it; host error finalization
        // does so before close_turn redraws the failed exchange.
        for event in [
            Event::ProviderRetryFinished { step: 1 },
            Event::TurnStarted { step: 2 },
            Event::Finished {
                reason: StopReason::Cancelled,
                usage: rune_net::stream::Usage::default(),
                last_request: rune_net::stream::Usage::default(),
                steps: 1,
            },
        ] {
            end_frame_interval(&host);
            host.emit(event);
            assert!(!replay(&mut grid).contains("provider retry"));
            end_frame_interval(&host);
            host.emit(Event::ProviderRetry {
                step: 1,
                next_attempt: 2,
                max_attempts: 3,
                delay: std::time::Duration::from_millis(250),
                retry_at: std::time::SystemTime::now(),
                kind: "rate_limited",
            });
            replay(&mut grid);
        }
        host.clear_provider_retry();
        host.draw_stream();
        assert!(!replay(&mut grid).contains("provider retry"));
        assert!(event_entries(&host).is_empty());
    }

    #[test]
    fn a_streamed_delta_cannot_drive_the_terminal() {
        // Streamed rows reach the terminal as they are, so a clipboard write, a
        // screen clear, or a switch to the alternate screen in model output
        // would act on the reader's terminal mid-answer. Both lanes are checked,
        // including a sequence whose introducer arrives in an earlier delta.
        let sink: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let host = test_host();
        if let Ok(mut slot) = host.live_out.lock() {
            *slot = Some(Arc::new(Mutex::new(SharedSink(Arc::clone(&sink)))));
        }

        let deltas = [
            "before \u{1b}]52;c;SGVsbG8=\u{7} after",
            "wiped\u{1b}[2J",
            "\u{1b}[?1049h alternate",
            "split \u{1b}",
            "]52;c;c3BsaXQ=\u{7} tail",
            "\u{9d}52;c;YzE=\u{9c} c1",
        ];
        for delta in deltas {
            host.emit(Event::ReasoningDelta {
                delta: delta.to_owned(),
            });
            host.emit(Event::TextDelta {
                delta: delta.to_owned(),
            });
        }

        let written = String::from_utf8_lossy(&sink.lock().expect("lock")).into_owned();
        for forbidden in ["\u{1b}]", "\u{1b}[2J", "\u{1b}[?1049", "\u{9d}", "\u{9c}"] {
            assert!(
                !written.contains(forbidden),
                "{forbidden:?} reached the terminal: {written:?}"
            );
        }
        let streaming = host.streaming.lock().expect("lock");
        for lane in [&streaming.answer, &streaming.reasoning] {
            assert!(!lane.contains('\u{1b}'), "an escape was kept: {lane:?}");
            assert!(
                lane.contains("after"),
                "the text around it was lost: {lane:?}"
            );
            assert!(
                lane.contains("tail"),
                "the text around it was lost: {lane:?}"
            );
        }
    }

    #[test]
    fn streamed_text_is_drawn_above_the_status_and_input() {
        // The answer sits directly above the status block, under the question it
        // answers, and the input stays at the bottom. Putting it below the input
        // made the input rise as the answer grew, because the region is anchored
        // at the bottom of the screen.
        let host = test_host();
        // Accumulated without drawing, so this test sees one frame rather than
        // the deltas that produced it overlap on the same screen.
        if let Ok(mut streaming) = host.streaming.lock() {
            streaming.answer.push_str("answer");
        }
        let prompt_row = transcript::render_prompt(rune_term::shell::prompt(), "", 80);
        let rows = host.streaming_rows();
        let bytes = host
            .paint(&[], None, std::slice::from_ref(&prompt_row), &rows, (0, 2))
            .expect("painted");
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let input = text.find("> ").expect("the input row");
        let answer = text.find("answer").expect("the streamed answer");
        assert!(
            answer < input,
            "the answer was not drawn above the input: {text:?}"
        );
    }

    #[test]
    fn a_model_choice_is_reported_above_the_input_and_stays_there() {
        // The picker runs in raw mode, where a line written straight to the
        // stream lands at the caret inside the live region. The next frame
        // paints over it and leaves a second status block behind.
        let bytes: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let out: LiveSink = Arc::new(Mutex::new(SharedSink(Arc::clone(&bytes))));
        let host = test_host();
        let (prompt_row, caret) = host.idle_prompt();
        let draw = |host: &SessionHost| {
            let painted = host
                .paint(&[], None, &prompt_row, &[], caret)
                .expect("painted");
            out.lock()
                .expect("lock")
                .write_all(&painted)
                .expect("wrote");
        };

        draw(&host);
        let mut sink = LockedSink {
            stream: Arc::clone(&out),
        };
        report_model_choice(&host, &mut sink, "next-model", None).expect("reported");
        // The prompt the session draws next, as the loop does after the picker,
        // then the frame that commits the next prompt the user sends.
        draw(&host);
        let echo = transcript::render_prompt(rune_term::shell::prompt(), "hi", 80);
        let sent = host
            .paint(std::slice::from_ref(&echo), None, &prompt_row, &[], caret)
            .expect("painted");
        out.lock().expect("lock").write_all(&sent).expect("wrote");

        let mut grid = rune_term::Grid::new(80, 24).expect("grid");
        grid.feed(&bytes.lock().expect("lock")).expect("feed");
        let screen = grid.text();
        assert_eq!(
            screen.matches("model set to next-model").count(),
            1,
            "the report was not kept on screen:\n{screen}"
        );
        for row in ["ctrl-c cancel", "| auto |"] {
            assert_eq!(
                screen.matches(row).count(),
                1,
                "a second status block was left behind:\n{screen}"
            );
        }
        let report = screen.find("model set to").expect("the report");
        let sent = screen.find("> hi").expect("the sent prompt");
        assert!(
            report < sent,
            "the report is not above the prompt:\n{screen}"
        );
    }

    #[test]
    fn history_search_matches_middle_words_in_workspace_prompts_newest_first() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = Paths::resolve(
            Some(root.as_str()),
            Some(root.as_str()),
            Some(root.as_str()),
            Some(root.as_str()),
            None,
        );
        paths.ensure_roots().expect("roots");
        let mut history = crate::prompt_history::History::open(&paths).expect("history");
        for (text, workspace, session) in [
            ("older needle prompt\n界", root.to_owned(), "s1"),
            ("unrelated latest prompt", root.to_owned(), "s2"),
            ("newer needle prompt", root.to_owned(), "s2"),
            ("other needle workspace", root.join("other"), "s3"),
        ] {
            history
                .record(crate::prompt_history::Entry::new(text).located(&workspace, session))
                .expect("record");
        }
        let reopened = crate::prompt_history::History::open(&paths).expect("reopen");
        let mut picker = history_picker(&recorded_prompts(Some(&reopened), root));
        assert_eq!(picker.len(), 3);
        assert_eq!(picker.selected(), Some("newer needle prompt"));
        picker.set_query("NEEDLE");
        assert_eq!(picker.matches().len(), 2);
        picker.down();
        assert_eq!(picker.selected(), Some("older needle prompt\n界"));
        picker.set_query("absent");
        assert_eq!(picker.selected(), None);
        assert_eq!(history_picker(&[]).selected(), None);
    }

    #[test]
    fn escape_at_the_prompt_clears_the_line_and_never_leaves() {
        let commands = command_candidates(&[]);
        // While a turn runs, Escape asks for a second press to cancel. A turn
        // that ends between the two presses hands the second to this prompt,
        // where it must not end the session.
        let mut reader = rune_term::input::KeyReader::new();
        let mut selected = 3;
        reader.replace("a draft");
        let step = idle_key(
            &commands,
            KeyAction::Escape,
            &mut reader,
            &mut selected,
            &[],
            &[],
        );
        assert!(matches!(step, Idle::Stay));
        assert_eq!(reader.line(), "");
        assert_eq!(selected, 0);

        let step = idle_key(
            &commands,
            KeyAction::Escape,
            &mut reader,
            &mut selected,
            &[],
            &[],
        );
        assert!(
            matches!(step, Idle::Stay),
            "escape on an empty line ended the session"
        );

        // Control-C and Control-D on an empty line are still the way out.
        let step = idle_key(
            &commands,
            KeyAction::Cancel,
            &mut reader,
            &mut selected,
            &[],
            &[],
        );
        assert!(matches!(step, Idle::Leave));
        let step = idle_key(
            &commands,
            KeyAction::Interrupt,
            &mut reader,
            &mut selected,
            &[],
            &[],
        );
        assert!(matches!(step, Idle::Leave));
    }

    #[test]
    fn caught_worker_panic_renders_one_diagnostic_without_duplicate_status_rows() {
        // A subprocess makes writes from the default panic hook observable,
        // rather than letting the test harness capture and hide the defect.
        if std::env::var_os("RUNE_R059_PANIC_CHILD").is_none() {
            let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
                .args([
                    "--exact",
                    "session::tests::caught_worker_panic_renders_one_diagnostic_without_duplicate_status_rows",
                    "--nocapture",
                ])
                .env("RUNE_R059_PANIC_CHILD", "1")
                .output()
                .expect("panic fixture subprocess");
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success(), "{stdout}\n{stderr}");
            assert!(stderr.is_empty(), "panic bypassed the renderer: {stderr}");
            assert!(!stdout.contains("panicked at"), "{stdout}");
            assert!(
                !stdout.contains('\u{1b}'),
                "terminal writes bypassed the sink: {stdout}"
            );
            let capture = stdout
                .lines()
                .find_map(|line| line.strip_prefix("R059_CAPTURE:"))
                .expect("renderer capture");
            let bytes: Vec<u8> = serde_json::from_str(capture).expect("frame bytes");
            let mut grid = rune_term::Grid::new(80, 24).expect("grid");
            grid.feed(&bytes).expect("replay");
            let screen = grid.text();
            assert_eq!(
                screen.matches("worker panicked: R059_FIXTURE").count(),
                1,
                "{screen}"
            );
            assert_eq!(screen.matches("ctrl-c cancel").count(), 1, "{screen}");
            assert_eq!(screen.matches("test | ").count(), 1, "{screen}");
            assert_eq!(screen.matches("draft after panic").count(), 1, "{screen}");
            assert!(screen.contains("partial before panic"), "{screen}");
            assert!(screen.contains("next turn answer"), "{screen}");
            return;
        }

        let bytes = Arc::new(Mutex::new(Vec::new()));
        let out: LiveSink = Arc::new(Mutex::new(SharedSink(Arc::clone(&bytes))));
        let host = test_host();
        *host.live_out.lock().expect("output") = Some(Arc::clone(&out));
        let mut reader = rune_term::input::KeyReader::new();
        reader.replace("draft after panic");
        host.draw_stream_with(reader.line(), reader.column());
        let mut history = History::new();
        history.push_user("panic fixture");
        let (result, diagnostic) = run_on_worker(
            &mut history,
            |_| {
                host.emit(Event::TextDelta {
                    delta: "partial before panic".to_owned(),
                });
                // Bounded, debug-only fixture also checks terminal sanitization.
                std::panic::panic_any(String::from("R059_FIXTURE\u{1b}[2J"));
            },
            || std::thread::sleep(std::time::Duration::from_millis(1)),
        );
        let lines = report_failed_turn(
            &result.expect_err("caught panic"),
            &host,
            diagnostic.as_deref(),
        );
        host.clear_events();
        host.clear_streaming();
        let mut sink = LockedSink { stream: out };
        close_turn(&host, &mut sink, &mut reader, &lines, None, &[]).expect("closed");
        // A subsequent worker and frame must still be usable, and redraws must
        // not settle the diagnostic a second time.
        let (_, diagnostic) = run_on_worker(
            &mut history,
            |_| Err(rune_agent::steering::cancelled_error()),
            || {},
        );
        assert!(diagnostic.is_none());
        host.draw_stream_with(reader.line(), reader.column());
        close_turn(
            &host,
            &mut sink,
            &mut reader,
            &["next turn answer".to_owned()],
            None,
            &[],
        )
        .expect("next turn closed");
        println!(
            "R059_CAPTURE:{}",
            serde_json::to_string(&*bytes.lock().expect("bytes")).expect("json")
        );
    }

    #[test]
    fn a_caught_worker_panic_reports_an_internal_failure() {
        let host = test_host();
        let mut history = History::new();
        history.push_user("panic fixture");
        let (result, diagnostic) = run_on_worker(
            &mut history,
            |_| {
                host.emit(Event::TextDelta {
                    delta: "partial before panic".to_owned(),
                });
                panic!("R060_FIXTURE");
            },
            || std::thread::sleep(std::time::Duration::from_millis(1)),
        );

        let error = result.expect_err("caught worker panic");
        assert_eq!(error.code(), ErrorCode::Internal);
        let lines = report_failed_turn(&error, &host, diagnostic.as_deref()).join("\n");
        assert!(
            lines.contains("the turn failed: internal worker failure"),
            "{lines}"
        );
        assert!(lines.contains("partial before panic"), "{lines}");
        assert_eq!(
            lines.matches("worker panicked: R060_FIXTURE").count(),
            1,
            "{lines}"
        );
        assert!(!lines.contains("cancelled"), "{lines}");
    }

    #[test]
    fn a_turn_that_panics_keeps_the_conversation_it_started_with() {
        // A panic inside a turn ends that exchange and nothing more: falling
        // back to an empty history would throw away every earlier turn.
        let mut history = History::new();
        history.push_user("first question");
        history.push_assistant(vec![rune_net::message::ContentPart::Text {
            text: "first answer".to_owned(),
        }]);
        history.push_user("second question");

        let (result, diagnostic) = run_on_worker(
            &mut history,
            |taken| {
                // Stopped partway: an assistant turn holding a call with no
                // result, which every later request would be refused for.
                taken.push_assistant(vec![rune_net::message::ContentPart::ToolCall {
                    id: rune_core::id::ToolCallId::new("call-1").expect("id"),
                    name: "read_file".to_owned(),
                    arguments: "{}".to_owned(),
                }]);
                panic!("the turn failed");
            },
            || std::thread::sleep(std::time::Duration::from_millis(1)),
        );

        assert_eq!(
            result.err().map(|err| err.code()),
            Some(ErrorCode::Internal)
        );
        assert_eq!(
            diagnostic.as_deref(),
            Some("worker panicked: the turn failed")
        );
        assert_eq!(history.len(), 3, "the conversation was not kept");
        assert_eq!(history.turns()[0].text(), "first question");
        assert_eq!(history.turns()[2].text(), "second question");
        history
            .validate()
            .expect("the kept conversation can still be sent");
    }

    #[test]
    fn a_finished_turn_hands_back_what_it_added() {
        let mut history = History::new();
        history.push_user("question");
        let (result, diagnostic) = run_on_worker(
            &mut history,
            |taken| {
                taken.push_assistant(vec![rune_net::message::ContentPart::Text {
                    text: "answer".to_owned(),
                }]);
                Err(rune_agent::steering::cancelled_error())
            },
            || {},
        );
        assert_eq!(
            result.expect_err("cancelled turn").code(),
            ErrorCode::Cancelled
        );
        assert!(diagnostic.is_none());
        assert_eq!(history.len(), 2);
        assert_eq!(history.turns()[1].text(), "answer");
    }

    #[test]
    fn a_cancelled_turn_keeps_what_streamed_and_says_it_was_cancelled() {
        // The reader watched the partial answer arrive, so a transcript that
        // dropped it would disagree with the screen they just saw.
        let host = test_host();
        if let Ok(mut streaming) = host.streaming.lock() {
            streaming.answer.push_str("half an answer");
        }
        let lines =
            report_failed_turn(&rune_agent::steering::cancelled_error(), &host, None).join("\n");
        assert!(lines.contains("half an answer"), "{lines}");
        assert!(lines.contains("cancelled"), "{lines}");
        assert!(
            !lines.contains("failed"),
            "a cancel is not a failure: {lines}"
        );
    }

    #[test]
    fn cancelled_answers_keep_only_text_not_already_in_the_conversation() {
        let mut history = History::new();
        history.push_user("an earlier question");
        history.push_assistant(vec![rune_net::message::ContentPart::Text {
            text: "an earlier answer".to_owned(),
        }]);
        history.push_user("slow");
        let start = history.len();
        history.push_assistant(vec![rune_net::message::ContentPart::Text {
            text: "STREAM-01\n".to_owned(),
        }]);
        retain_partial_answer(&mut history, start, "STREAM-01\nSTREAM-02\nSTREAM-03\n");
        assert_eq!(history.turns()[3].text(), "STREAM-01\n");
        assert_eq!(history.turns()[4].text(), "STREAM-02\nSTREAM-03\n");

        // Cancelling after text is in history, or before any arrives, adds none.
        retain_partial_answer(&mut history, start, "STREAM-01\nSTREAM-02\nSTREAM-03\n");
        retain_partial_answer(&mut history, start, "");
        assert_eq!(history.len(), 5);
        history
            .validate()
            .expect("the next request can use the partial answer");
    }

    #[test]
    fn a_failed_turn_names_the_failure_and_what_to_do() {
        let host = test_host();
        let err = RuneError::new(
            ErrorCode::TransportFailure,
            "the endpoint closed the stream",
        )
        .with_hint("check the provider status");
        let lines = report_failed_turn(&err, &host, None).join("\n");
        assert!(lines.contains("the endpoint closed the stream"), "{lines}");
        assert!(lines.contains("check the provider status"), "{lines}");
    }

    #[test]
    fn the_status_line_is_drawn_above_the_input() {
        // Where a reader looks for it, and where it does not move as an answer
        // arrives.
        let host = test_host();
        let bytes = host
            .paint(&[], None, &["> ".to_owned()], &[], (0, 2))
            .expect("painted");
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let status = text.find("auto").expect("the status line");
        let input = text.find("> ").expect("the input row");
        assert!(
            status < input,
            "the status line was drawn below the input: {text:?}"
        );
    }

    #[test]
    fn a_finished_turn_is_printed_once_above_the_live_region() {
        // Finished lines join the terminal's own scrollback, so they are
        // written in the flow rather than positioned. Everything after them is
        // the region that gets redrawn in place.
        let host = test_host();
        let lines = vec!["a line".to_owned()];
        let bytes = host
            .paint(&lines, None, &[], &[], (0, 0))
            .expect("painted the first screen");
        let text = String::from_utf8_lossy(&bytes).into_owned();
        assert!(
            text.contains("a line\r\n"),
            "the finished line was not printed in the flow: {text:?}"
        );
    }

    #[test]
    fn the_live_region_carries_the_status_line() {
        let host = test_host();
        let bytes = host.paint(&[], None, &[], &[], (0, 0)).expect("painted");
        let text = String::from_utf8_lossy(&bytes).into_owned();
        assert!(
            text.contains("test"),
            "the status line was not drawn: {text:?}"
        );
        assert!(text.contains("auto"), "{text:?}");
    }

    #[test]
    fn the_prompt_and_its_cursor_are_drawn_where_the_caret_is() {
        // This is the whole point of the composer: the caret sits inside what
        // was typed, rather than wherever the terminal would have left it.
        let host = test_host();
        let row = vec!["> hel".to_owned()];
        let bytes = host.paint(&[], None, &row, &[], (0, 5)).expect("painted");
        let text = String::from_utf8_lossy(&bytes).into_owned();
        assert!(text.contains("> hel"), "{text:?}");
        // A one-based column, so a caret after five columns is column six.
        assert!(
            text.contains("\u{1b}[6G"),
            "the caret was not placed at the end of the line: {text:?}"
        );
    }

    #[test]
    fn every_frame_hides_the_cursor_while_it_writes() {
        // A cursor visible mid-frame is seen jumping between rows.
        let host = test_host();
        let bytes = host.paint(&[], None, &[], &[], (0, 0)).expect("painted");
        let text = String::from_utf8_lossy(&bytes).into_owned();
        assert!(text.contains(rune_term::inline::HIDE_CURSOR), "{text:?}");
        assert!(text.contains(rune_term::inline::SHOW_CURSOR), "{text:?}");
    }

    #[test]
    fn leaving_removes_the_live_region() {
        let host = test_host();
        host.paint(&[], None, &[], &[], (0, 0)).expect("painted");
        let cleared = host.clear_region().expect("cleared");
        assert!(!cleared.is_empty(), "the region was left on the screen");
    }

    #[test]
    fn the_width_comes_from_the_terminal_rather_than_a_constant() {
        // A width that disagrees with the real one makes every row wrap or
        // leave a gap, and both put later content in the wrong place.
        let host = test_host();
        let before = host.width();
        host.width.store(132, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(host.width(), 132);
        assert_ne!(before, 132, "the test did not change the width");
        // The status line is measured against the width it now reports.
        let line = host.status_line(usize::from(host.width()));
        assert!(!line.is_empty());
    }

    #[test]
    fn a_prompt_override_replaces_the_built_in_text() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        std::fs::write(
            root.join(rune_core::paths::names::SYSTEM_PROMPT_FILE),
            "You are a terse reviewer.\n",
        )
        .expect("write");

        let prompt = build_prompt(
            root,
            root,
            &BudgetSet::new(),
            &[],
            rune_core::experiment::PromptVariant::V1,
        );
        // The skills installed for this user may follow the override, so the
        // check is on what the prompt opens with rather than on all of it.
        assert!(
            prompt.instructions.starts_with("You are a terse reviewer."),
            "{}",
            prompt.instructions
        );
        let built_in = prompt::SYSTEM_PROMPT.lines().next().unwrap_or_default();
        assert!(
            !prompt.instructions.contains(built_in),
            "the built-in text was kept alongside the override"
        );
    }

    #[test]
    fn no_override_leaves_the_built_in_text_alone() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let prompt = build_prompt(
            root,
            root,
            &BudgetSet::new(),
            &[],
            rune_core::experiment::PromptVariant::V1,
        );
        assert!(prompt.instructions.starts_with(prompt::SYSTEM_PROMPT));
    }

    #[test]
    fn an_empty_override_file_is_ignored() {
        // A truncated or blanked file would otherwise leave the model with no
        // instructions at all.
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        std::fs::write(
            root.join(rune_core::paths::names::SYSTEM_PROMPT_FILE),
            "   \n\n",
        )
        .expect("write");

        let prompt = build_prompt(
            root,
            root,
            &BudgetSet::new(),
            &[],
            rune_core::experiment::PromptVariant::V1,
        );
        assert!(prompt.instructions.starts_with(prompt::SYSTEM_PROMPT));
    }

    /// A reviewer that answers with whatever it was given.
    struct Scripted(ReviewOutcome);

    impl Reviewer for Scripted {
        fn review(&self, _request: &ReviewRequest) -> ReviewOutcome {
            self.0.clone()
        }
    }

    /// Builds a host in automatic mode with the given reviewer.
    fn host_with_reviewer(outcome: ReviewOutcome) -> SessionHost {
        let mut host = test_host();
        *host.mode.get_mut().expect("mode") = PermissionMode::Auto;
        host.reviewer = Some(Box::new(Scripted(outcome)));
        host
    }

    #[test]
    fn an_unresolved_action_is_cleared_by_review_in_automatic_mode() {
        // Automatic mode reviews instead of prompting, so an action the rules
        // do not decide is allowed when the reviewer clears it.
        let host = host_with_reviewer(ReviewOutcome::Clear {
            reviewed_action: "rm -rf /tmp/x".to_owned(),
        });
        let (outcome, reason) = host.decide("shell", Some("rm -rf /tmp/x"));
        assert_eq!(outcome, Outcome::Allow, "{reason}");
        assert!(reason.contains("cleared by review"), "{reason}");
    }

    #[test]
    fn a_cautioning_review_holds_the_action_with_its_reason() {
        let host = host_with_reviewer(ReviewOutcome::Caution {
            reason: "broader than the request".to_owned(),
        });
        let (outcome, reason) = host.decide("shell", Some("rm -rf /tmp/x"));
        assert_eq!(outcome, Outcome::Deny, "{reason}");
        assert_eq!(reason, "broader than the request");
    }

    #[test]
    fn a_failed_review_asks_the_user_with_a_notice() {
        // An unavailable reviewer is not a judgment about the action, so it
        // neither holds nor approves it: the user is asked, and told why.
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let out: LiveSink = Arc::new(Mutex::new(SharedSink(Arc::clone(&bytes))));
        let host = host_with_reviewer(ReviewOutcome::Unavailable {
            reason: "the reviewer could not be reached".to_owned(),
        });
        *host.live_out.lock().expect("lock") = Some(out);
        let (answers, requests) = mpsc::channel::<ApprovalRequest>();
        *host.approval_requests.lock().expect("approvals") = Some(answers);
        let user = std::thread::spawn(move || {
            let request = requests.recv().expect("an approval prompt");
            assert_eq!(request.target, "rm -rf /tmp/x");
            request
                .answer
                .send(ApprovalOutcome::RunOnce)
                .expect("answered");
        });

        let (outcome, reason) = host.decide("shell", Some("rm -rf /tmp/x"));
        user.join().expect("the prompt was answered");
        assert_eq!(outcome, Outcome::Allow, "{reason}");
        assert!(reason.contains("approved for this call"), "{reason}");

        let mut grid = rune_term::Grid::new(80, 24).expect("grid");
        grid.feed(&bytes.lock().expect("bytes")).expect("feed");
        // The notice wraps at the terminal width, so it is read row by row.
        let text = grid.text();
        for part in [
            "the safety reviewer could not answer (the reviewer could not be reached);",
            "asking you instead",
        ] {
            assert!(text.contains(part), "{text}");
        }
    }

    #[test]
    fn a_failed_review_without_a_person_stays_unresolved() {
        // Nobody to ask leaves the action as unresolved as it was, not denied
        // and not approved.
        let host = host_with_reviewer(ReviewOutcome::Unavailable {
            reason: "the reviewer could not be reached".to_owned(),
        });
        let (outcome, reason) = host.decide("shell", Some("rm -rf /tmp/x"));
        assert_eq!(outcome, Outcome::Ask, "{reason}");
    }

    #[test]
    fn an_allowlisted_action_never_reaches_the_reviewer() {
        // A rule that already decided must not cost a review, or the budget
        // would be spent on the commands that were never in question.
        let host = host_with_reviewer(ReviewOutcome::Caution {
            reason: "should not be consulted".to_owned(),
        });
        let (outcome, reason) = host.decide("read_file", Some("src/main.rs"));
        assert_eq!(outcome, Outcome::Allow, "{reason}");
        assert!(!reason.contains("should not be consulted"), "{reason}");
    }

    #[test]
    fn a_reviewer_is_only_consulted_in_automatic_mode() {
        let mut host = host_with_reviewer(ReviewOutcome::Caution {
            reason: "should not be consulted".to_owned(),
        });
        *host.mode.get_mut().expect("mode") = PermissionMode::FullAccess;
        let (outcome, _) = host.decide("shell", Some("rm -rf /tmp/x"));
        assert_eq!(outcome, Outcome::Allow);
    }

    #[test]
    fn an_unresolved_action_is_never_approved_without_a_decision() {
        // Nothing judged it: no rule allowed it and no reviewer saw it. Whether
        // a person is watching is not a decision, so it stays unresolved.
        let mut host = test_host();
        *host.mode.get_mut().expect("mode") = PermissionMode::Auto;
        let (outcome, reason) = host.decide("shell", Some("rm -rf /tmp/x"));
        assert_eq!(outcome, Outcome::Ask, "{reason}");
    }

    #[test]
    fn private_network_access_requires_a_user_answer_in_every_mode() {
        for mode in [
            PermissionMode::Ask,
            PermissionMode::Auto,
            PermissionMode::FullAccess,
        ] {
            let mut host = host_with_reviewer(ReviewOutcome::Clear {
                reviewed_action: "domain:localhost".to_owned(),
            });
            *host.mode.get_mut().expect("mode") = mode;
            host.rules
                .get_mut()
                .expect("rules")
                .push(rune_policy::rules::Rule::allow(
                    "web_fetch",
                    "*",
                    rune_policy::decision::Layer::User,
                ));
            host.rules
                .get_mut()
                .expect("rules")
                .push(rune_policy::rules::Rule::allow(
                    "*",
                    "*",
                    rune_policy::decision::Layer::User,
                ));
            assert_eq!(
                host.decide("web_fetch", Some("domain:localhost")).0,
                Outcome::Allow
            );
            assert_eq!(
                host.decide_private_network(Some("domain:localhost")).0,
                Outcome::Ask
            );
            let (requests, pending) = mpsc::channel();
            *host.approval_requests.lock().expect("lock") = Some(requests);
            std::thread::scope(|scope| {
                let worker = scope.spawn(|| {
                    let first = host.decide_private_network(Some("domain:localhost"));
                    let second = host.decide_private_network(Some("domain:localhost"));
                    (first, second)
                });
                let request = pending
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .expect("separate private-network approval");
                assert_eq!(request.tool, "web_fetch_private");
                assert_eq!(request.target, "domain:localhost");
                assert!(request.reason.contains("private-network access"));
                assert!(request.reason.contains("redirects"));
                request
                    .answer
                    .send(ApprovalOutcome::RunOnce)
                    .expect("approve once");
                let request = pending
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .expect("approval cannot leak to the next call");
                request.answer.send(ApprovalOutcome::Deny).expect("deny");
                let (first, second) = worker.join().expect("worker");
                assert_eq!(first.0, Outcome::Allow);
                assert_eq!(second.0, Outcome::Deny);
            });
        }
    }

    #[test]
    fn only_an_explicit_user_private_network_rule_can_grant_access() {
        use rune_policy::decision::Layer;
        use rune_policy::rules::Rule;
        for layer in [
            Layer::Default,
            Layer::Project,
            Layer::User,
            Layer::Session,
            Layer::Grant,
        ] {
            let mut host = test_host();
            *host.mode.get_mut().expect("mode") = PermissionMode::FullAccess;
            host.rules.get_mut().expect("rules").push(Rule::allow(
                "web_fetch_private",
                "domain:localhost",
                layer,
            ));
            let outcome = host.decide_private_network(Some("domain:localhost")).0;
            assert_eq!(
                outcome,
                if layer >= Layer::User {
                    Outcome::Allow
                } else {
                    Outcome::Ask
                }
            );
            assert_eq!(
                host.decide_private_network(Some("domain:other")).0,
                Outcome::Ask
            );
        }
        let mut host = test_host();
        *host.mode.get_mut().expect("mode") = PermissionMode::FullAccess;
        host.rules.get_mut().expect("rules").push(Rule::deny(
            "web_fetch_private",
            "*",
            Layer::User,
        ));
        let (requests, pending) = mpsc::channel();
        *host.approval_requests.lock().expect("lock") = Some(requests);
        assert_eq!(
            host.decide_private_network(Some("domain:localhost")).0,
            Outcome::Deny
        );
        assert!(
            pending.try_recv().is_err(),
            "denial cannot be overridden by a prompt"
        );
    }

    #[test]
    fn a_terminal_answer_resolves_only_the_displayed_call() {
        let mut host = test_host();
        host.rules
            .get_mut()
            .expect("rules")
            .push(rune_policy::rules::Rule::ask(
                "edit_file",
                "*",
                rune_policy::decision::Layer::Session,
            ));
        let (requests, pending) = mpsc::channel();
        *host.approval_requests.lock().expect("lock") = Some(requests);
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                let first = host.decide("shell", Some("printf AUDIT_SHELL_OK"));
                let second = host.decide("shell", Some("printf AUDIT_SHELL_OK"));
                (first, second)
            });
            let first = pending
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("first approval request");
            assert_eq!(first.tool, "shell");
            assert_eq!(first.target, "printf AUDIT_SHELL_OK");
            assert!(
                approval_lines(&first, 80)
                    .join("\n")
                    .contains(&first.target)
            );
            first
                .answer
                .send(ApprovalOutcome::RunOnce)
                .expect("approve once");
            let second = pending
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("the same command needs another approval");
            second.answer.send(ApprovalOutcome::Deny).expect("deny");
            let (first, second) = worker.join().expect("worker");
            assert_eq!(first.0, Outcome::Allow, "{}", first.1);
            assert_eq!(second.0, Outcome::Deny, "{}", second.1);
        });
    }

    #[test]
    fn cancellation_wakes_a_worker_waiting_for_approval() {
        let mut host = test_host();
        host.rules
            .get_mut()
            .expect("rules")
            .push(rune_policy::rules::Rule::ask(
                "edit_file",
                "*",
                rune_policy::decision::Layer::Session,
            ));
        let (requests, pending) = mpsc::channel();
        *host.approval_requests.lock().expect("lock") = Some(requests);
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| host.decide("shell", Some("printf AUDIT_SHELL_OK")));
            let _request = pending
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("approval request");
            host.cancellation.cancel();
            let (answer, reason) = worker.join().expect("worker");
            assert_eq!(answer, Outcome::Deny);
            assert!(reason.contains("cancelled"), "{reason}");
        });
    }

    #[test]
    fn closed_approval_input_never_allows_a_call() {
        let mut host = test_host();
        host.rules
            .get_mut()
            .expect("rules")
            .push(rune_policy::rules::Rule::ask(
                "edit_file",
                "*",
                rune_policy::decision::Layer::Session,
            ));
        let (requests, pending) = mpsc::channel();
        *host.approval_requests.lock().expect("lock") = Some(requests);
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| host.decide("shell", Some("printf AUDIT_SHELL_OK")));
            let request = pending
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("approval request");
            drop(request);
            let (answer, reason) = worker.join().expect("worker");
            assert_eq!(answer, Outcome::Deny);
            assert!(reason.contains("closed"), "{reason}");
        });
        drop(pending);
        assert_eq!(
            host.decide("shell", Some("printf AUDIT_SHELL_OK")).0,
            Outcome::Deny
        );
    }

    #[test]
    fn terminal_approval_does_not_override_a_rule_or_approve_without_input() {
        let mut host = test_host();
        host.rules
            .get_mut()
            .expect("rules")
            .push(rune_policy::rules::Rule::ask(
                "edit_file",
                "*",
                rune_policy::decision::Layer::Session,
            ));
        host.rules
            .get_mut()
            .expect("rules")
            .push(rune_policy::rules::Rule::deny(
                "read_file",
                ".env",
                rune_policy::decision::Layer::User,
            ));
        assert_eq!(
            host.decide("shell", Some("printf AUDIT_SHELL_OK")).0,
            Outcome::Ask
        );
        let (requests, pending) = mpsc::channel();
        *host.approval_requests.lock().expect("lock") = Some(requests);
        assert_eq!(
            host.decide("read_file", Some("src/main.rs")).0,
            Outcome::Allow
        );
        assert_eq!(host.decide("read_file", Some(".env")).0, Outcome::Deny);
        assert!(
            pending.try_recv().is_err(),
            "settled rules must never prompt"
        );
    }

    #[test]
    fn approval_scope_cannot_hide_terminal_controls_or_clip_long_commands() {
        let (answer, _response) = mpsc::sync_channel(1);
        let request = ApprovalRequest {
            tool: "shell".to_owned(),
            target: "printf 'a\\b'\n\t\u{1b}[2J\u{202e}TAIL-END".to_owned(),
            reason: "no rule allows it".to_owned(),
            rememberable: true,
            preview: Vec::new(),
            answer,
        };
        let lines = approval_lines(&request, 80).join("\n");
        assert!(
            lines.contains("\\n\\t\\u{1b}[2J\\u{202e}TAIL-END"),
            "{lines}"
        );
        assert!(lines.contains("a\\\\b"), "{lines}");
        assert!(!lines.contains('\u{1b}'));
        let narrow = approval_lines(&request, 12);
        assert!(
            narrow
                .iter()
                .all(|line| rune_term::width::str_width(line) <= 12)
        );
        assert!(narrow.join("").ends_with("TAIL-END\""));
    }

    #[test]
    fn an_approval_prompt_that_cannot_be_displayed_denies() {
        let host = test_host();
        let (answer, _response) = mpsc::sync_channel(1);
        let request = ApprovalRequest {
            tool: "shell".to_owned(),
            target: "printf AUDIT_SHELL_OK".to_owned(),
            reason: "no rule allows it".to_owned(),
            rememberable: true,
            preview: Vec::new(),
            answer,
        };
        let mut reader = rune_term::input::KeyReader::new();
        assert_eq!(
            collect_approval(&request, &host, &mut reader),
            ApprovalOutcome::Deny
        );
    }

    fn fixture_question() -> Question {
        Question {
            text: "Which choice?".to_owned(),
            options: vec![
                rune_tools::ask_user::Choice {
                    label: "Alpha".to_owned(),
                    description: None,
                },
                rune_tools::ask_user::Choice {
                    label: "Beta".to_owned(),
                    description: Some("The second choice".to_owned()),
                },
            ],
        }
    }

    #[test]
    fn question_input_unavailable_or_closed_never_selects_an_answer() {
        let answerer = TerminalQuestions::default();
        let context = ExecutionContext::new(Utf8PathBuf::from("/tmp"));
        let questions = [fixture_question()];
        assert_eq!(
            answerer
                .ask(&questions, &context)
                .expect_err("unavailable")
                .code(),
            ErrorCode::InputRequired
        );
        let (requests, pending) = mpsc::channel();
        *answerer.requests.lock().expect("lock") = Some(requests);
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| answerer.ask(&questions, &context));
            let request = pending
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("question request");
            assert_eq!(request.questions, questions);
            drop(request);
            assert_eq!(
                worker.join().expect("worker").expect_err("closed").code(),
                ErrorCode::InputRequired
            );
        });
        drop(pending);
        assert_eq!(
            answerer
                .ask(&questions, &context)
                .expect_err("disconnected")
                .code(),
            ErrorCode::InputRequired
        );
    }

    #[test]
    fn cancellation_wakes_a_worker_waiting_for_question_input() {
        for cancel_context in [false, true] {
            let answerer = TerminalQuestions::default();
            let context = ExecutionContext::new(Utf8PathBuf::from("/tmp"));
            let questions = [fixture_question()];
            let (requests, pending) = mpsc::channel();
            *answerer.requests.lock().expect("lock") = Some(requests);
            std::thread::scope(|scope| {
                let worker = scope.spawn(|| answerer.ask(&questions, &context));
                let _request = pending
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .expect("question request");
                if cancel_context {
                    context.cancellation().cancel();
                } else {
                    answerer.cancellation.cancel();
                }
                assert_eq!(
                    worker
                        .join()
                        .expect("worker")
                        .expect_err("cancelled")
                        .code(),
                    ErrorCode::Cancelled
                );
            });
        }
    }

    #[test]
    fn question_choices_keep_indices_and_sanitize_displayed_text() {
        let mut question = fixture_question();
        question.text.push_str("\u{1b}[2JTAIL-END");
        question.options[0].label = "Beta".to_owned();
        question.options[1].description = Some("\u{1b}]52;c;clipboard\u{7}details".to_owned());
        let lines = question_lines(&question, 12);
        assert!(!lines.join("").contains('\u{1b}'));
        assert!(lines.join("").contains("TAIL-END"));
        assert!(lines.join("").contains("details"));
        assert!(
            lines
                .iter()
                .all(|line| rune_term::width::str_width(line) <= 12)
        );
        let mut picker = rune_term::picker::Picker::new(
            "question",
            question
                .options
                .iter()
                .map(|option| option.label.clone())
                .collect(),
            2,
        );
        let cancellation = Cancellation::new();
        assert_eq!(
            question_key(KeyAction::Down, &mut picker, &cancellation),
            None
        );
        assert_eq!(
            question_key(KeyAction::Ignored, &mut picker, &cancellation),
            None
        );
        assert_eq!(
            question_key(KeyAction::Submit, &mut picker, &cancellation),
            Some(1)
        );
        assert!(!cancellation.is_cancelled());
    }

    #[test]
    fn a_question_that_cannot_be_displayed_requires_input() {
        let host = test_host();
        let mut reader = rune_term::input::KeyReader::new();
        let error = collect_questions(&[fixture_question()], &host, &mut reader)
            .expect_err("cannot display the question");
        assert_eq!(error.code(), ErrorCode::InputRequired);
    }

    #[test]
    fn the_status_line_names_the_model_and_the_mode() {
        let host = test_host();
        let line = host.status_line(120);
        assert!(line.contains("test"), "{line}");
        assert!(line.contains("auto"), "{line}");
        assert!(line.contains("ctx"), "{line}");
    }

    #[test]
    fn the_status_line_reports_the_context_already_spent() {
        let host = test_host();
        let before = host.status_line(120);
        assert!(before.contains("ctx 0%"), "{before}");
        host.record_context_size(64_000);
        let after = host.status_line(120);
        assert!(
            !after.contains("ctx 0%"),
            "usage was not reflected: {after}"
        );
    }

    #[test]
    fn wrapping_incrementally_matches_wrapping_the_whole_text() {
        // The cache exists to make a delta cheap, not to change what is drawn.
        // A word straddling a delta boundary must still break where wrapping the
        // finished text would break it.
        let text = "the quick brown fox jumps over the lazy dog again and again";
        let mut lazy = LazyRows::default();
        let mut prefix = String::new();
        for word in text.split_inclusive(' ') {
            prefix.push_str(word);
            let incremental = rows_for(&mut lazy, &prefix, 20);
            let whole = rune_term::width::wrap(&prefix, 20);
            assert_eq!(
                incremental, whole,
                "incremental wrapping diverged at {prefix:?}"
            );
        }
    }

    #[test]
    fn streamed_fenced_code_matches_the_finished_transcript_across_resize() {
        let host = test_host();
        host.width.store(20, std::sync::atomic::Ordering::Relaxed);
        let mut text = String::new();
        for ch in "```rust\n    call(\"alpha  beta\",  gamma);\n```\nafter".chars() {
            text.push(ch);
            // Accumulate without painting, which refreshes the size from the
            // runner's own terminal rather than this test's chosen width.
            host.streaming.lock().expect("streaming").answer.push(ch);
            assert_eq!(
                host.streaming_rows().join("\n").trim_end(),
                render_entries(&[Entry::assistant(&text)], &host).join("\n"),
                "streamed and finished code disagree at {text:?}"
            );
        }
        assert!(
            host.streaming_rows()
                .iter()
                .any(|row| row.starts_with("    ↪ "))
        );
        for width in [12, 40, 20] {
            host.width
                .store(width, std::sync::atomic::Ordering::Relaxed);
            assert_eq!(
                host.streaming_rows(),
                render_entries(&[Entry::assistant(&text)], &host)
            );
        }
        host.clear_streaming();
        host.streaming
            .lock()
            .expect("streaming")
            .answer
            .push_str("words after the code");
        assert_eq!(
            host.streaming_rows(),
            render_entries(&[Entry::assistant("words after the code")], &host)
        );
    }

    #[test]
    fn wrapping_across_several_lines_keeps_every_line() {
        let mut lazy = LazyRows::default();
        let mut prefix = String::new();
        for part in ["first line\n", "second line\n", "third line"] {
            prefix.push_str(part);
            let incremental = rows_for(&mut lazy, &prefix, 20);
            assert_eq!(incremental, rune_term::width::wrap(&prefix, 20));
        }
        let rows = rows_for(&mut lazy, &prefix, 20);
        assert_eq!(rows.len(), 3, "{rows:?}");
    }

    #[test]
    fn clearing_a_lane_forgets_its_rows() {
        // A cleared lane must not redraw the response that was dropped.
        let mut lazy = LazyRows::default();
        let rows = rows_for(&mut lazy, "an old answer", 20);
        assert!(!rows.is_empty());
        let rows = rows_for(&mut lazy, "", 20);
        assert!(rows.is_empty(), "stale rows survived: {rows:?}");
    }

    #[test]
    fn live_usage_supersedes_the_resume_estimate_and_new_clears_its_source() {
        let host = test_host();
        host.context_used
            .store(1234, std::sync::atomic::Ordering::Relaxed);
        *host.context_source.lock().expect("source") = Some("saved usage");
        assert!(host.status_line(120).contains("ctx ~1.2k (saved usage)"));
        assert!(render_status(&host.info("p", "e")).contains("saved usage (estimate)"));
        host.record_context_size(0);
        assert!(host.status_line(120).contains("saved usage"));
        host.record_context_size(1000);
        let info = host.info("p", "e");
        assert_eq!(info.context_used, 1000);
        assert_eq!(info.context_source, None);
        assert!(!host.status_line(120).contains("saved usage"));
        *host.context_source.lock().expect("source") = Some("history bytes");
        host.forget_context();
        let info = host.info("p", "e");
        assert_eq!(info.context_used, 0);
        assert_eq!(info.context_source, None);
    }

    #[test]
    fn missing_or_partial_usage_is_estimated_and_explicit_zero_is_reported() {
        use rune_net::stream::Usage;

        let host = test_host();
        let mut history = History::new();
        host.estimate_context(&history).expect("initial estimate");
        let initial = host.info("p", "e");
        assert!(initial.context_used > 0);
        assert_eq!(initial.context_source, Some("request bytes"));
        assert!(render_status(&initial).contains("request bytes (estimate)"));
        assert!(host.status_line(120).contains("(request bytes)"));

        history.push_user("fixture question");
        history.push_assistant(vec![rune_net::message::ContentPart::Text {
            text: "fixture answer".to_owned(),
        }]);
        let reported = Usage {
            input_tokens: Some(1234),
            output_tokens: Some(66),
            ..Usage::default()
        };
        host.record_context_usage(&history, &reported)
            .expect("usage");
        assert_eq!(host.info("p", "e").context_used, 1300);
        assert!(render_status(&host.info("p", "e")).contains("context source provider-reported"));
        assert!(host.status_line(120).contains("(provider-reported)"));

        for usage in [
            Usage::default(),
            Usage {
                input_tokens: Some(1234),
                ..Usage::default()
            },
            Usage {
                output_tokens: Some(66),
                ..Usage::default()
            },
        ] {
            host.record_context_usage(&history, &usage)
                .expect("estimate");
            let info = host.info("p", "e");
            assert!(info.context_used > initial.context_used);
            assert_eq!(info.context_source, Some("request bytes"));
            assert!(!host.status_line(120).contains("provider-reported"));
        }
        host.record_context_usage(
            &history,
            &Usage {
                input_tokens: Some(0),
                output_tokens: Some(0),
                ..Usage::default()
            },
        )
        .expect("zero report");
        assert_eq!(host.info("p", "e").context_used, 0);
        assert_eq!(host.info("p", "e").context_source, None);
        assert!(host.status_line(120).contains("(provider-reported)"));
    }

    #[test]
    fn context_usage_is_the_size_of_the_conversation_not_a_running_sum() {
        // Every turn resends the whole conversation, so an input count already
        // contains the earlier turns. Summing them counts the same history once
        // per turn, which is what made a session appear to fill its window
        // several times over.
        let host = test_host();
        host.record_context_size(8_000);
        host.record_context_size(8_400);
        host.record_context_size(8_900);
        assert_eq!(
            host.context_used.load(std::sync::atomic::Ordering::Relaxed),
            8_900,
            "the readings were summed rather than taken as a high-water mark"
        );
    }

    #[test]
    fn a_shrinking_reading_never_lowers_the_reported_size() {
        // A turn that reports fewer input tokens than the last is not the
        // conversation getting smaller; taking the smaller value would hide the
        // history that is still being sent.
        let host = test_host();
        host.record_context_size(9_000);
        host.record_context_size(7_000);
        assert_eq!(
            host.context_used.load(std::sync::atomic::Ordering::Relaxed),
            9_000
        );
    }

    #[test]
    fn a_declared_model_window_is_used_and_an_undeclared_one_is_unknown() {
        // A model that accepts a million tokens must not be budgeted against a
        // hundred and twenty-eight thousand, which reports a nearly empty
        // window as a fifth full.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let paths = Paths::resolve(Some(root.as_str()), None, None, None, Some(root.as_str()));
        let settings = Settings {
            context_window: Some(1_000_000),
            ..Settings::default()
        };
        let limits = crate::model_limits::resolve(&settings, &paths, &settings.model, None);
        assert_eq!(limits.context_window, Some(1_000_000));

        // Without a declaration the window is unknown rather than a guess,
        // because guessing high would let a conversation grow past what the
        // model accepts and guessing low truncates it.
        let undeclared = Settings::default();
        let limits = crate::model_limits::resolve(&undeclared, &paths, &undeclared.model, None);
        assert_eq!(limits.context_window, None);
    }

    #[test]
    fn a_loaded_zero_context_window_cannot_create_a_zero_capacity_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(dir.path().join("config.toml")).expect("utf8");
        std::fs::write(
            &path,
            "provider = 'anthropic'\n[models.anthropic]\nid = 'small'\ncontext_window = 0\n",
        )
        .expect("write");
        let settings = rune_core::config::load(
            None,
            Some(&path),
            &rune_core::config::EnvironmentOverrides::default(),
        );
        assert!(settings.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == ErrorCode::InvalidField
                && diagnostic.key.as_deref() == Some("models.anthropic.context_window")
        }));
        // The zero is dropped, so the window is unknown rather than empty.
        let limits = crate::model_limits::resolve(
            &settings,
            &Paths::resolve(
                Some(dir.path().to_str().expect("utf8")),
                None,
                None,
                None,
                Some(dir.path().to_str().expect("utf8")),
            ),
            &settings.model,
            None,
        );
        assert_eq!(limits.context_window, None);
    }

    #[test]
    fn the_catalog_budget_follows_the_window_it_is_given() {
        // A fixed budget is wrong at both ends, so the session sizes it from
        // the window the model reports.
        use rune_core::budget::LimitName;

        let mut small = BudgetSet::new();
        adopt_skill_catalog_budget(&mut small, Some(8_000));
        let mut large = BudgetSet::new();
        adopt_skill_catalog_budget(&mut large, Some(2_000_000));

        assert!(
            small.get_bytes(LimitName::SkillCatalogBytes)
                < large.get_bytes(LimitName::SkillCatalogBytes),
            "a larger window did not earn a larger catalog budget"
        );
    }

    #[test]
    fn an_undeclared_window_keeps_the_catalog_fallback() {
        // The compiled window is a guess, so a model that states none keeps the
        // documented fallback rather than a budget derived from the guess.
        use rune_core::budget::LimitName;

        let mut limits = BudgetSet::new();
        adopt_skill_catalog_budget(&mut limits, None);
        assert_eq!(
            limits.get_bytes(LimitName::SkillCatalogBytes),
            rune_core::budget::SKILL_CATALOG_FALLBACK_BYTES
        );
    }

    #[test]
    fn a_named_catalog_budget_survives_the_derivation() {
        // A limit someone set on purpose is not this program's to overwrite.
        use rune_core::budget::{Budget, LimitName};
        use rune_core::config::Layer;

        let mut limits = BudgetSet::new();
        limits
            .set(
                LimitName::SkillCatalogBytes,
                Budget::Bounded(1234),
                Layer::User,
            )
            .expect("set");
        adopt_skill_catalog_budget(&mut limits, Some(2_000_000));

        assert_eq!(limits.get_bytes(LimitName::SkillCatalogBytes), 1234);
    }

    #[test]
    fn a_colorless_terminal_cannot_be_forced_into_color() {
        // Capability beats preference: the setting expresses a wish, the
        // terminal states what it accepts, and the terminal wins.
        let config = colorless_config();
        let (theme, _) = resolve_theme_for(&config, true, false);
        assert_eq!(theme.base(), Theme::no_color().base());

        // The same configuration with a terminal that accepts color does take
        // the configured theme, which is what shows the capability is the input
        // rather than the configuration.
        let (with_color, _) = resolve_theme_for(&config, false, false);
        assert_ne!(with_color.base(), Theme::no_color().base());
    }

    #[test]
    fn a_light_terminal_gets_the_light_theme_and_a_broken_one_warns() {
        let mut config = colorless_config();
        let (theme, warning) = resolve_theme_for(&config, false, true);
        assert_eq!(theme, Theme::light());
        assert!(warning.is_none());
        let themes = config.paths.config_root.join("themes");
        std::fs::create_dir_all(&themes).expect("themes");
        std::fs::write(
            themes.join("mine.toml"),
            "base = \"dark\"\ncolour = \"#fff\"\n",
        )
        .expect("theme");
        config.settings.theme = Some("mine".to_owned());
        let (theme, warning) = resolve_theme_for(&config, false, false);
        assert_eq!(theme, Theme::dark());
        let warning = warning.expect("a warning");
        assert!(
            warning.starts_with("warning: theme mine: unknown theme slot `colour`"),
            "{warning}"
        );
        assert!(warning.ends_with("; using dark"), "{warning}");
    }

    fn theme_paths() -> (tempfile::TempDir, Paths) {
        let root = tempfile::tempdir().expect("tempdir");
        let base = Utf8PathBuf::from_path_buf(root.path().to_owned()).expect("utf8");
        let paths = Paths {
            config_root: base.join("config"),
            state_root: base.join("state"),
            data_root: base.join("data"),
        };
        rune_core::paths::create_dir_private(&paths.config_root).expect("config");
        (root, paths)
    }

    #[test]
    fn a_theme_command_switches_and_saves() {
        let (_root, paths) = theme_paths();
        let mut host = test_host();
        host.theme = Mutex::new(Theme::dark());
        let reply = set_theme_named(&host, &paths, "light");
        assert_eq!(reply, "theme light");
        assert_eq!(host.theme(), Theme::light());
        let saved = std::fs::read_to_string(paths.config_file(None)).expect("config");
        assert!(saved.contains("theme = \"light\""), "{saved}");
    }

    #[test]
    fn an_unknown_theme_name_keeps_the_current_theme() {
        let (_root, paths) = theme_paths();
        let mut host = test_host();
        host.theme = Mutex::new(Theme::dark());
        let reply = set_theme_named(&host, &paths, "../elsewhere");
        assert!(reply.contains("is a path, not a theme name"), "{reply}");
        assert_eq!(host.theme(), Theme::dark());
        assert!(!paths.config_file(None).exists());
        // A colourless terminal stays colourless, whatever is named.
        host.theme = Mutex::new(Theme::no_color());
        assert_eq!(set_theme_named(&host, &paths, "ansi"), "theme ansi");
        assert_eq!(host.theme(), Theme::no_color());
    }

    #[test]
    fn no_color_overrides_the_named_high_contrast_theme() {
        let mut config = colorless_config();
        config.settings.theme = Some("high-contrast".to_owned());
        assert_eq!(
            resolve_theme_for(&config, false, false).0,
            Theme::high_contrast()
        );
        let (theme, _) = resolve_theme_for(&config, true, false);
        assert_eq!(theme, Theme::no_color());
        for slot in rune_term::theme::SLOTS {
            for depth in ColorDepth::ALL {
                assert!(theme.sgr(slot, depth).is_empty());
            }
        }
    }

    #[test]
    fn the_shell_reports_an_unknown_command_without_leaving() {
        let mut output = Vec::new();
        let action = handle_command(
            "nope",
            "",
            &empty_commands(),
            None,
            Utf8Path::new("/w"),
            &test_info(),
            &mut output,
        )
        .expect("handled");
        assert_eq!(action, Handled::Continue);
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("unknown command"), "{text}");
        assert!(text.contains("/help"), "{text}");
    }

    #[test]
    fn naming_a_model_inline_asks_for_a_switch() {
        let mut output = Vec::new();
        let action = handle_command(
            "model",
            "vendor/name",
            &empty_commands(),
            None,
            Utf8Path::new("/w"),
            &test_info(),
            &mut output,
        )
        .expect("handled");
        assert_eq!(action, Handled::SetModel("vendor/name".to_owned()));
    }

    #[test]
    fn the_plural_models_spelling_means_the_same_thing() {
        // Reaching for the plural is what most people do first, and refusing it
        // would teach nothing.
        let mut output = Vec::new();
        let action = handle_command(
            "models",
            "",
            &empty_commands(),
            None,
            Utf8Path::new("/w"),
            &test_info(),
            &mut output,
        )
        .expect("handled");
        assert_eq!(action, Handled::PickModel);

        let mut output = Vec::new();
        let action = handle_command(
            "models",
            "beta-1",
            &empty_commands(),
            None,
            Utf8Path::new("/w"),
            &test_info(),
            &mut output,
        )
        .expect("handled");
        assert_eq!(action, Handled::SetModel("beta-1".to_owned()));
    }

    #[test]
    fn a_bare_model_command_asks_for_the_picker() {
        // The whole point of the command without an argument is the list, so a
        // bare invocation must not be read as switching to the empty model.
        let mut output = Vec::new();
        let action = handle_command(
            "model",
            "   ",
            &empty_commands(),
            None,
            Utf8Path::new("/w"),
            &test_info(),
            &mut output,
        )
        .expect("handled");
        assert_eq!(action, Handled::PickModel);
    }

    #[test]
    fn switching_the_model_changes_what_the_session_sends() {
        let host = test_host();
        assert_eq!(host.model(), "test");
        host.set_model("other/model", None, None, None);
        assert_eq!(host.model(), "other/model");
    }

    #[test]
    fn a_reported_window_is_adopted_when_none_was_configured() {
        // Without this the session budgets against the compiled default, which
        // understates a large model so badly that it looks nearly full before
        // anything has been said.
        let host = test_host();
        assert_eq!(
            host.context_limit
                .load(std::sync::atomic::Ordering::Relaxed),
            rune_net::catalog::DEFAULT_CONTEXT_WINDOW
        );
        host.adopt_context_window(Some(1_000_000), false);
        assert_eq!(
            host.context_limit
                .load(std::sync::atomic::Ordering::Relaxed),
            1_000_000
        );
    }

    #[test]
    fn a_configured_window_is_not_overridden_by_the_endpoint() {
        // A user who declared a window is describing the model they selected,
        // and an endpoint may advertise a figure that counts only part of the
        // conversation, so their choice wins.
        let host = test_host();
        let before = host
            .context_limit
            .load(std::sync::atomic::Ordering::Relaxed);
        host.adopt_context_window(Some(9_999_999), true);
        assert_eq!(
            host.context_limit
                .load(std::sync::atomic::Ordering::Relaxed),
            before
        );
    }

    #[test]
    fn an_absent_or_zero_reported_window_leaves_the_limit_alone() {
        let host = test_host();
        let before = host
            .context_limit
            .load(std::sync::atomic::Ordering::Relaxed);
        host.adopt_context_window(None, false);
        host.adopt_context_window(Some(0), false);
        assert_eq!(
            host.context_limit
                .load(std::sync::atomic::Ordering::Relaxed),
            before
        );
    }

    #[test]
    fn switching_the_model_moves_the_context_window_only_when_one_is_known() {
        // A window carried over from a larger model understates how full a
        // smaller one is, but inventing one is worse, so an unknown window is
        // left as it stands.
        let host = test_host();
        let before = host
            .context_limit
            .load(std::sync::atomic::Ordering::Relaxed);
        host.set_model("small", None, None, None);
        assert_eq!(
            host.context_limit
                .load(std::sync::atomic::Ordering::Relaxed),
            before
        );
        host.set_model("large", Some(1_000_000), None, None);
        assert_eq!(
            host.context_limit
                .load(std::sync::atomic::Ordering::Relaxed),
            1_000_000
        );
        // A zero means nothing is known, not a window of nothing.
        host.set_model("zero", Some(0), None, None);
        assert_eq!(
            host.context_limit
                .load(std::sync::atomic::Ordering::Relaxed),
            1_000_000
        );
    }

    #[test]
    fn the_status_command_names_the_model_in_effect() {
        let host = test_host();
        host.set_model("picked/model", None, None, None);
        let info = host.info("chat_completions", "https://example.invalid/v1");
        let rendered = render_status(&info);
        assert!(rendered.contains("picked/model"), "{rendered}");
        assert!(rendered.contains("chat_completions"), "{rendered}");
        assert!(rendered.contains("sessiontest1"), "{rendered}");
    }

    #[test]
    fn the_status_command_names_the_terminal_after_the_workspace() {
        let mut host = test_host();
        host.caps.name = Some("FakeTerm(1.0)".to_owned());
        host.caps.background = Some((0x1e, 0x1e, 0x1e));
        let rendered = render_status(&host.info("p", "e"));
        let rows: Vec<&str> = rendered.lines().collect();
        let at = rows
            .iter()
            .position(|row| row.starts_with("workspace "))
            .expect("workspace row");
        assert_eq!(
            rows[at + 1],
            "terminal       FakeTerm(1.0); background #1e1e1e (dark); synchronized output \
             unknown; kitty keyboard no; tmux no; ssh no"
        );
    }

    #[test]
    fn the_status_command_names_the_colour_depth() {
        for depth in ColorDepth::ALL {
            let mut info = test_info();
            info.depth = depth;
            let rendered = render_status(&info);
            let workspace = rendered.find("workspace      /w").expect("workspace row");
            let colors = rendered
                .find(&format!("colors         {}\n", depth.name()))
                .expect("colors row");
            assert!(workspace < colors, "{rendered}");
        }
    }

    #[test]
    fn no_deeper_escape_than_the_depth_is_written() {
        let themes = [
            Theme::dark(),
            Theme::light(),
            Theme::high_contrast(),
            Theme::no_color(),
        ];
        for theme in themes {
            for depth in ColorDepth::ALL {
                let written = fixture_transcript(&theme, depth);
                let context = format!("{:?} at {depth:?}", theme.base());
                let codes = sgr_codes(&written);
                assert!(!codes.is_empty(), "{context}: no styling at all");
                let colored = codes.iter().any(|code| {
                    code.starts_with("38;") || code.starts_with("48;") || basic_color_code(code)
                });
                match depth {
                    ColorDepth::TrueColor | ColorDepth::Ansi256 | ColorDepth::Ansi16
                        if theme.base() != rune_term::theme::Base::Mono =>
                    {
                        assert!(colored, "{context}: no color written");
                    }
                    _ => {}
                }
                for code in &codes {
                    match depth {
                        ColorDepth::TrueColor => {}
                        ColorDepth::Ansi256 => {
                            assert!(
                                !code.contains("38;2;") && !code.contains("48;2;"),
                                "{context}: {code}"
                            );
                        }
                        ColorDepth::Ansi16 => {
                            for deep in ["38;5;", "48;5;", "38;2;", "48;2;"] {
                                assert!(!code.contains(deep), "{context}: {code}");
                            }
                        }
                        ColorDepth::None => {
                            assert!(
                                !code.contains("38;")
                                    && !code.contains("48;")
                                    && !basic_color_code(code),
                                "{context}: {code}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// Returns the bytes a turn with tool rows, an edit diff, a Markdown answer,
    /// a failure notice and the footer writes with one theme and depth.
    fn fixture_transcript(theme: &Theme, depth: ColorDepth) -> String {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let mut host = test_host();
        host.theme = Mutex::new(theme.clone());
        host.depth = depth;
        *host.live_out.lock().expect("lock") =
            Some(Arc::new(Mutex::new(SharedSink(Arc::clone(&bytes)))));
        host.emit(Event::ToolFinished {
            call: turn::PreparedCall {
                id: "c1".to_owned(),
                name: "read_file".to_owned(),
                arguments: r#"{"path":"src/lib.rs"}"#.to_owned(),
            },
            output: ToolOutput::success("     1\ta\n     2\tb\n"),
            elapsed: std::time::Duration::from_millis(2),
        });
        host.emit(Event::ToolFinished {
            call: turn::PreparedCall {
                id: "c2".to_owned(),
                name: "edit_file".to_owned(),
                arguments: serde_json::json!({
                    "path": "src/lib.rs",
                    "old_string": "keep\nold line",
                    "new_string": "keep\nnew line",
                })
                .to_string(),
            },
            output: ToolOutput::success("edited src/lib.rs (1 replacement, +1 -1, line 2)"),
            elapsed: std::time::Duration::from_millis(3),
        });
        host.emit(Event::ToolFinished {
            call: turn::PreparedCall {
                id: "c3".to_owned(),
                name: "read_file".to_owned(),
                arguments: r#"{"path":"missing.rs"}"#.to_owned(),
            },
            output: ToolOutput::failure("missing.rs does not exist"),
            elapsed: std::time::Duration::from_millis(1),
        });
        let mut written = String::from_utf8_lossy(&bytes.lock().expect("lock")).into_owned();
        let answer = "# Plan\n\nSome **bold** and `code` with a [link](https://a.test).\n\n\
                      ```rust\nfn main() { let x = 1; // note\n}\n```\n\n- one\n- two\n";
        written.push_str(&render_entries(&[Entry::assistant(answer)], &host).join("\r\n"));
        let err = RuneError::new(ErrorCode::InvalidField, "the provider refused the request")
            .with_hint("check the model name");
        written.push_str(&report_failed_turn(&err, &host, None).join("\r\n"));
        written.push_str(&host.status_line(100));
        written
    }

    /// Returns the parameters of every SGR sequence in a byte string.
    fn sgr_codes(text: &str) -> Vec<String> {
        let mut codes = Vec::new();
        let mut rest = text;
        while let Some(start) = rest.find("\u{1b}[") {
            let after = &rest[start.saturating_add(2)..];
            let end = after
                .find(|ch: char| !(ch.is_ascii_digit() || ch == ';' || ch == ':'))
                .unwrap_or(after.len());
            if after[end..].starts_with('m') {
                codes.push(format!("{};", &after[..end]));
            }
            rest = &after[end..];
        }
        codes
    }

    /// Returns true when an SGR parameter list holds a basic color code.
    fn basic_color_code(code: &str) -> bool {
        let params: Vec<u16> = code
            .split(';')
            .filter_map(|param| param.parse().ok())
            .collect();
        let mut index = 0;
        while let Some(param) = params.get(index) {
            match param {
                38 | 48 => {
                    // Skip the extended color's own arguments.
                    let skip = if params.get(index.saturating_add(1)) == Some(&2) {
                        5
                    } else {
                        3
                    };
                    index = index.saturating_add(skip);
                    continue;
                }
                30..=37 | 40..=47 | 90..=97 | 100..=107 => return true,
                _ => {}
            }
            index = index.saturating_add(1);
        }
        false
    }

    #[test]
    fn a_status_line_never_prints_the_credential() {
        // The endpoint carries the credential beside its address, so a status
        // row built from the wrong field would leak the key to the screen.
        let host = test_host();
        let info = host.info("chat_completions", "https://example.invalid/v1");
        let rendered = render_status(&info);
        assert!(!rendered.contains("sk-"), "{rendered}");
    }

    #[test]
    fn the_status_command_names_an_unknown_window_rather_than_a_share_of_it() {
        let mut info = test_info();
        info.context_limit = 0;
        info.context_used = 500;
        let rendered = render_status(&info);
        assert!(rendered.contains("window unknown"), "{rendered}");
        assert!(!rendered.contains('%'), "{rendered}");
    }

    #[test]
    fn the_cost_command_counts_only_what_was_reported() {
        // An absent count must not be added as zero, or a provider that reports
        // nothing looks like a provider that charged nothing.
        let mut totals = Totals::default();
        assert_eq!(
            render_cost(&test_info_with(totals.clone())),
            "no requests have been made in this session yet"
        );
        totals.record(&rune_net::stream::Usage {
            input_tokens: Some(1200),
            output_tokens: Some(450),
            ..rune_net::stream::Usage::default()
        });
        totals.record(&rune_net::stream::Usage {
            input_tokens: None,
            output_tokens: Some(50),
            ..rune_net::stream::Usage::default()
        });
        let rendered = render_cost(&test_info_with(totals));
        assert!(
            rendered.starts_with("spent - this session (2 requests, 1.2k in, 500 out)"),
            "{rendered}"
        );
    }

    /// Answers every POST with a short Anthropic stream and keeps each body.
    fn warming_listener() -> (
        u16,
        Arc<Mutex<Vec<serde_json::Value>>>,
        Arc<std::sync::atomic::AtomicBool>,
    ) {
        use std::io::{BufRead as _, Read as _, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let port = listener.local_addr().expect("addr").port();
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (kept, stop) = (Arc::clone(&bodies), Arc::clone(&done));
        std::thread::spawn(move || {
            let reply = crate::helper_usage::fixture::anthropic_reply("", "max_tokens");
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    continue;
                };
                stream.set_nonblocking(false).expect("blocking");
                let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone"));
                let mut length = 0_usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("header");
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().expect("length");
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).expect("body");
                kept.lock()
                    .expect("bodies")
                    .push(serde_json::from_slice(&body).expect("json body"));
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
            }
        });
        (port, bodies, done)
    }

    /// A Claude session with warming on, whose last request is already sent.
    fn warming_host(port: u16) -> (SessionHost, History) {
        let mut host = test_host();
        host.permission_settings.cache_warming = true;
        host.dialect = Box::new(rune_net::anthropic::Anthropic);
        host.endpoint = Endpoint::new(format!("http://127.0.0.1:{port}"), "k");
        let mut history = History::new();
        history.push_user("first question");
        prepare_next(&host, &mut history);
        (host, history)
    }

    /// Prepares the next request as the loop does before sending it.
    fn prepare_next(host: &SessionHost, history: &mut History) {
        let mut plan = rune_net::provider::RequestPlan::new(host.model());
        plan.instructions = "Be brief.".to_owned();
        plan.messages = history.to_messages();
        host.prepare_request(history, &mut plan, &SummaryFixture::new("unused"))
            .expect("prepared");
    }

    #[test]
    fn warming_pings_at_most_six_times() {
        let (port, bodies, done) = warming_listener();
        let (host, _history) = warming_host(port);
        let sent = host.start_warming().expect("warming starts").join();
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(sent.expect("thread"), 6);
        let bodies = bodies.lock().expect("bodies");
        assert_eq!(bodies.len(), 6);
        let plan = host.last_plan.lock().expect("plan").clone().expect("kept");
        let mut sent_body = rune_net::anthropic::Anthropic
            .build_request(&plan)
            .expect("body");
        sent_body["max_tokens"] = serde_json::json!(1);
        for body in bodies.iter() {
            assert_eq!(body["max_tokens"], 1);
            assert_eq!(body, &sent_body, "only the output ceiling changes");
        }
        let recorded = host.helper_usage.drain();
        assert_eq!(recorded.len(), 6);
        assert!(
            recorded
                .iter()
                .all(|(kind, model, _)| *kind == HelperKind::CacheWarming && model == "test")
        );
    }

    #[test]
    fn a_real_request_stops_warming() {
        let (port, bodies, done) = warming_listener();
        let (mut host, mut history) = warming_host(port);
        host.warm_interval = std::time::Duration::from_millis(300);
        let warming = host.start_warming().expect("warming starts");
        let started = std::time::Instant::now();
        while bodies.lock().expect("bodies").is_empty() {
            assert!(started.elapsed().as_secs() < 10, "no ping arrived");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        history.push_user("second question");
        prepare_next(&host, &mut history);
        assert_eq!(warming.join().expect("thread"), 1);
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(bodies.lock().expect("bodies").len(), 1);

        // Each ending command and quitting stop it too.
        let warming = host.start_warming().expect("warming starts");
        host.stop_warming();
        assert_eq!(warming.join().expect("thread"), 0);
        for handled in [
            Handled::Exit,
            Handled::NewSession,
            Handled::SetModel("other".to_owned()),
            Handled::PickModel,
            Handled::Compact,
            Handled::Reload,
        ] {
            assert!(ends_warming(&handled));
        }
        assert!(!ends_warming(&Handled::Continue));
    }

    #[test]
    fn warming_is_off_by_default_and_for_other_dialects() {
        let (port, bodies, done) = warming_listener();
        let (mut host, mut history) = warming_host(port);
        host.permission_settings.cache_warming = Settings::default().cache_warming;
        *host.last_plan.lock().expect("plan") = None;
        prepare_next(&host, &mut history);
        assert!(host.last_plan.lock().expect("plan").is_none());
        assert!(host.start_warming().is_none());

        host.permission_settings.cache_warming = true;
        host.dialect = Box::new(rune_net::chat_completions::ChatCompletions);
        prepare_next(&host, &mut history);
        assert!(host.last_plan.lock().expect("plan").is_none());
        assert!(host.start_warming().is_none());

        host.dialect = Box::new(rune_net::anthropic::Anthropic);
        host.endpoint.offline = true;
        prepare_next(&host, &mut history);
        assert!(host.start_warming().is_none());

        std::thread::sleep(std::time::Duration::from_millis(200));
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(bodies.lock().expect("bodies").is_empty());
        assert!(host.helper_usage.drain().is_empty());
    }

    #[test]
    fn warming_cost_has_its_own_line() {
        let (port, _bodies, done) = warming_listener();
        let (host, _history) = warming_host(port);
        assert_eq!(
            render_cost(&host.info("p", "e")),
            "no requests have been made in this session yet"
        );
        let sent = host.start_warming().expect("warming starts").join();
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(sent.expect("thread"), 6);
        host.absorb_helper_usage();
        let rendered = render_cost(&host.info("p", "e"));
        assert!(
            rendered.starts_with("spent - this session (6 requests, 66 in, 42 out)\nhelpers\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("(cache warming)   6 req   in 66  cached 0 (0%)  out 42"),
            "{rendered}"
        );
    }

    #[test]
    fn helper_usage_appears_under_its_label_in_cost() {
        let host = test_host();
        host.record_usage(&rune_net::stream::Usage {
            input_tokens: Some(1_000),
            output_tokens: Some(100),
            ..rune_net::stream::Usage::default()
        });
        let usage = |input, output| rune_net::stream::Usage {
            input_tokens: Some(input),
            output_tokens: Some(output),
            ..rune_net::stream::Usage::default()
        };
        host.helper_usage
            .record(HelperKind::PermissionReview, "small", usage(200, 10));
        host.helper_usage
            .record(HelperKind::PermissionReview, "small", usage(300, 20));
        host.helper_usage
            .record(HelperKind::Vision, "eyes", usage(1_500, 90));
        host.absorb_helper_usage();
        host.context_replaced(&rune_agent::compaction::CompactionReport {
            kind: rune_agent::compaction::CompactionKind::Checkpoint,
            pruned_results: 0,
            removed_turns: 1,
            remaining_turns: 1,
            usage: usage(2_000, 400),
        });
        assert_eq!(
            render_cost(&host.info("p", "e")),
            "spent - this session (5 requests, 5.0k in, 620 out)\n\
             helpers\n  \
             eyes (image analysis)   1 req   in 1.5k  cached 0 (0%)  out 90  reasoning 0   -\n  \
             small (permission review)   2 req   in 500  cached 0 (0%)  out 30  reasoning 0   -\n  \
             test (compaction)   1 req   in 2.0k  cached 0 (0%)  out 400  reasoning 0   -"
        );
        assert!(host.helper_usage.drain().is_empty());
    }

    #[test]
    fn a_priced_helper_adds_to_the_session_cost_and_shows_its_own() {
        let mut totals = Totals::default();
        let usage = rune_net::stream::Usage {
            input_tokens: Some(100),
            output_tokens: Some(10),
            ..rune_net::stream::Usage::default()
        };
        totals.record_helper("image analysis", "m", &usage, Some(1_500_000));
        totals.record_helper("image analysis", "m", &usage, Some(500_000));
        totals.record_helper("permission review", "m", &usage, None);
        assert_eq!(totals.cost, Some(2_000_000));
        assert_eq!(totals.requests, 3);
        let rendered = render_cost(&test_info_with(totals));
        let dollars = rune_net::models_dev::dollars(2_000_000);
        assert!(
            rendered.starts_with(&format!(
                "spent ${dollars} this session (3 requests, 300 in, 30 out)\n"
            )),
            "{rendered}"
        );
        assert!(
            rendered.contains(&format!(
                "  m (image analysis)   2 req   in 200  cached 0 (0%)  out 20  reasoning 0   ${dollars}\n"
            )),
            "{rendered}"
        );
        assert!(
            rendered.ends_with(
                "  m (permission review)   1 req   in 100  cached 0 (0%)  out 10  reasoning 0   -"
            ),
            "{rendered}"
        );
    }

    fn parts(entries: &[(&'static str, u64)]) -> BTreeMap<&'static str, u64> {
        rune_agent::tokens::CONTEXT_PARTS
            .iter()
            .map(|key| (*key, 0))
            .chain(entries.iter().copied())
            .collect()
    }

    fn breakdown(
        entries: &[(&'static str, u64)],
        ceiling: Option<u64>,
    ) -> rune_agent::tokens::ContextBreakdown {
        let parts = parts(entries);
        let total = parts.values().sum();
        rune_agent::tokens::ContextBreakdown {
            total_tokens: total,
            anchored: true,
            ceiling_tokens: ceiling,
            free_tokens: ceiling.map(|ceiling: u64| ceiling.saturating_sub(total)),
            parts,
            tool_results: std::iter::once(("read_file".to_owned(), 300)).collect(),
        }
    }

    #[test]
    fn context_bar_cells_sum_to_the_width() {
        for width in [12_u16, 40, 80, 200] {
            let cells = usize::from(width.saturating_sub(2)).min(60);
            let shares = parts(&[
                ("base_prompt", 333),
                ("tool_schemas", 2_000),
                ("tool_results", 4_100),
            ]);
            let (drawn, free) = context_cells(&shares, 10_000, cells);
            let used: usize = drawn.iter().map(|(_, count)| *count).sum();
            assert_eq!(used + free, cells, "{width}");
        }
        // A request over the ceiling still fills the bar exactly.
        let (drawn, free) = context_cells(&parts(&[("tool_results", 30_000)]), 10_000, 40);
        assert_eq!(
            drawn.iter().map(|(_, count)| *count).sum::<usize>() + free,
            40
        );
    }

    #[test]
    fn small_parts_merge_into_other() {
        let shares = parts(&[("tool_results", 9_000), ("reasoning", 40)]);
        let (drawn, _) = context_cells(&shares, 10_000, 38);
        assert!(drawn.iter().all(|(letter, _)| *letter != 'r'), "{drawn:?}");
        assert!(drawn.iter().any(|(letter, _)| *letter == 't'), "{drawn:?}");
    }

    #[test]
    fn ansi16_bar_uses_letters() {
        let view = render_context(
            &breakdown(
                &[("tool_schemas", 2_000), ("tool_results", 4_000)],
                Some(10_000),
            ),
            40,
            &Theme::dark(),
            ColorDepth::Ansi16,
            false,
        );
        let mut grid = rune_term::Grid::new(80, 2).expect("grid");
        grid.feed(view[0].as_bytes()).expect("bar");
        let plain = grid.row_text(0);
        assert!(plain.contains("sss") && plain.contains("ttt"), "{plain}");
        assert!(!plain.contains('\u{2588}'), "{plain}");
        assert_eq!(plain.chars().count(), 38, "{plain}");
        assert!(
            view[1].starts_with("6.0k of 10.0k tokens (60%)"),
            "{}",
            view[1]
        );
        let tools = view
            .iter()
            .position(|row| row.contains("tool results"))
            .expect("row");
        assert!(
            view[tools + 1].trim_start().starts_with("read_file"),
            "{view:?}"
        );
        let unknown = render_context(
            &breakdown(&[("tool_results", 10)], None),
            40,
            &Theme::dark(),
            ColorDepth::Ansi16,
            false,
        );
        assert_eq!(unknown[0], "10 tokens, window size unknown");
    }

    #[test]
    fn cost_lists_each_model_and_helpers() {
        let mut totals = Totals::default();
        let usage = rune_net::stream::Usage {
            input_tokens: Some(1_000),
            output_tokens: Some(100),
            cache_read_tokens: Some(500),
            reasoning_tokens: Some(20),
            ..rune_net::stream::Usage::default()
        };
        for model in ["alpha", "beta"] {
            totals.record(&usage);
            totals.record_model(model, &usage, None);
        }
        totals.record_helper("permission review", "small", &usage, None);
        let rendered = render_cost(&test_info_with(totals));
        assert_eq!(
            rendered,
            "spent - this session (3 requests, 3.0k in, 300 out)\n  \
             alpha   1 req   in 1.0k  cached 500 (50%)  out 100  reasoning 20   -\n  \
             beta   1 req   in 1.0k  cached 500 (50%)  out 100  reasoning 20   -\n\
             helpers\n  \
             small (permission review)   1 req   in 1.0k  cached 500 (50%)  out 100  reasoning 20   -"
        );
    }

    #[test]
    fn budget_line_shows_what_is_left() {
        let mut totals = Totals::default();
        let usage = rune_net::stream::Usage {
            input_tokens: Some(10),
            ..rune_net::stream::Usage::default()
        };
        totals.record(&usage);
        totals.record_model("alpha", &usage, Some(250_000));
        totals.cost = Some(250_000);
        let mut info = test_info_with(totals);
        info.spend_limit = Some(1_000_000);
        let rendered = render_cost(&info);
        let (limit, left) = (
            rune_net::models_dev::dollars(1_000_000),
            rune_net::models_dev::dollars(750_000),
        );
        assert!(
            rendered.ends_with(&format!("budget ${limit}: ${left} left")),
            "{rendered}"
        );
    }

    #[test]
    fn new_usage_accumulates_onto_restored_cost_totals() {
        use rune_session::{SessionEvent, SessionStore};

        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = Paths {
            state_root: root.join("state"),
            config_root: root.join("config"),
            data_root: root.join("data"),
        };
        let id = "restorecost1".parse().expect("id");
        let store = SessionStore::create(&paths, &id).expect("session");
        for turn in 1_u64..=3 {
            store
                .append(SessionEvent::TurnStarted { turn })
                .expect("start");
            store
                .append(SessionEvent::UsageRecorded {
                    input_tokens: turn.saturating_mul(100),
                    output_tokens: turn.saturating_mul(10),
                })
                .expect("usage");
            store
                .append(SessionEvent::TurnFinished { turn })
                .expect("finish");
        }
        drop(store);
        let state = session_log::inspect(&paths, &id).expect("saved state");
        let host = test_host();
        *host.totals.lock().expect("totals") = Totals::from_saved(&state);
        host.record_usage(&rune_net::stream::Usage {
            input_tokens: Some(100),
            output_tokens: Some(10),
            ..rune_net::stream::Usage::default()
        });
        assert_eq!(
            render_cost(&host.info("p", "e")),
            "spent - this session (4 requests, 700 in, 70 out)"
        );
    }

    #[test]
    fn usage_from_a_turn_is_added_to_the_session_total() {
        let host = test_host();
        host.record_usage(&rune_net::stream::Usage {
            input_tokens: Some(10),
            output_tokens: Some(4),
            ..rune_net::stream::Usage::default()
        });
        host.record_usage(&rune_net::stream::Usage {
            input_tokens: Some(5),
            output_tokens: Some(1),
            cache_read_tokens: Some(3),
            ..rune_net::stream::Usage::default()
        });
        let info = host.info("p", "e");
        assert_eq!(info.totals.requests, 2);
        assert_eq!(info.totals.input_tokens, 15);
        assert_eq!(info.totals.output_tokens, 5);
        assert_eq!(info.totals.cache_read_tokens, 3);
    }

    #[test]
    fn the_settings_command_reports_what_is_in_force() {
        let rendered = render_settings(&test_info());
        assert!(rendered.contains("permissions    auto"), "{rendered}");
        assert!(rendered.contains("context window 128.0k"), "{rendered}");
    }

    #[test]
    fn the_undo_command_is_recognized_rather_than_unknown() {
        let mut output = Vec::new();
        let action = handle_command(
            "undo",
            "",
            &empty_commands(),
            None,
            Utf8Path::new("/w"),
            &test_info(),
            &mut output,
        )
        .expect("handled");
        assert_eq!(action, Handled::Undo);
    }

    #[test]
    fn undo_preview_arguments_dispatch_without_applying_undo() {
        for arguments in ["preview", "  preview  "] {
            let mut output = Vec::new();
            assert_eq!(
                handle_command(
                    "undo",
                    arguments,
                    &empty_commands(),
                    None,
                    Utf8Path::new("/w"),
                    &test_info(),
                    &mut output,
                )
                .expect("handled"),
                Handled::UndoPreview,
            );
            assert!(output.is_empty());
        }
    }

    #[test]
    fn undo_preview_shows_file_diffs_and_preserves_bytes_and_journals() {
        for accessible in [false, true] {
            let dir = tempfile::tempdir().expect("temp");
            let root = Utf8Path::from_path(dir.path()).expect("utf8");
            let paths = Paths {
                config_root: root.join("config"),
                state_root: root.join("state"),
                data_root: root.join("data"),
            };
            let id = SessionId::generate();
            let mut host = test_host();
            host.accessible = accessible;
            host.endpoint.offline = true;
            host.context = ExecutionContext::new(root.to_owned());
            host.recorder = Some(Arc::new(Mutex::new(
                Recorder::create(&paths, &id).expect("recorder"),
            )));
            host.registry
                .insert(Box::new(rune_tools::WriteFile::new()))
                .expect("registered");
            let changed = root.join("changed.txt");
            let created = root.join("created.txt");
            std::fs::write(&changed, "original café\n").expect("fixture");
            for (path, content) in [
                (&changed, "intermediate\n"),
                (&changed, "latest 界"),
                (&created, "created\n"),
            ] {
                let output = host
                    .execute(
                        "write_file",
                        &serde_json::json!({"path": path, "content": content}),
                    )
                    .expect("write");
                assert!(!output.is_error, "{}", output.text);
            }
            let journal = host.undo.lock().expect("lock").clone();
            let events = host.events.lock().expect("lock").len();
            let log_path = paths
                .sessions_dir()
                .join(id.as_str())
                .join(rune_session::store::EVENTS_FILE);
            let log = std::fs::read(&log_path).expect("journal");
            let expected = host.preview_undo();
            let captured = Arc::new(Mutex::new(Vec::new()));
            let mut sink = LockedSink {
                stream: Arc::new(Mutex::new(SharedBuffer(Arc::clone(&captured)))),
            };
            for _ in 0..2 {
                let preview = host.preview_undo();
                assert_eq!(
                    preview, expected,
                    "preview consumed or changed undo records"
                );
                flush_lines(&host, &mut sink, &preview).expect("display preview");
            }
            let text = String::from_utf8(captured.lock().expect("lock").clone()).expect("utf8");
            for fragment in [
                "undo preview (files and mutation journal unchanged)",
                changed.as_str(),
                created.as_str(),
                "-latest 界",
                "+original café",
                "-created",
                "+++ /dev/null",
                "\\ No newline at end of file",
            ] {
                assert!(text.contains(fragment), "missing {fragment:?}: {text}");
            }
            assert!(!text.contains("intermediate"));
            assert_eq!(
                std::fs::read(&changed).expect("read"),
                "latest 界".as_bytes()
            );
            assert_eq!(std::fs::read(&created).expect("read"), b"created\n");
            assert_eq!(*host.undo.lock().expect("lock"), journal);
            assert_eq!(host.events.lock().expect("lock").len(), events);
            assert_eq!(std::fs::read(&log_path).expect("journal"), log);
            host.undo().expect("undo remains available");
            assert_eq!(
                std::fs::read(&changed).expect("read"),
                "original café\n".as_bytes()
            );
            assert!(!created.exists());
            assert!(!host.can_undo());
        }
    }

    #[test]
    fn undo_preview_of_a_rename_shows_restoration_and_removal_without_applying_them() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let mut host = test_host();
        host.context = ExecutionContext::new(root.to_owned());
        host.registry
            .insert(Box::new(rune_tools::RenameFile))
            .expect("registered");
        let source = root.join("source");
        let destination = root.join("destination");
        std::fs::write(&source, "body\n").expect("fixture");
        assert!(
            !host
                .execute(
                    "rename_file",
                    &serde_json::json!({"path": "source", "new_path": "destination"}),
                )
                .expect("rename")
                .is_error
        );
        let journal = host.undo.lock().expect("lock").clone();
        let preview = host.preview_undo().join("\n");
        for fragment in [
            format!("would restore {source}\n--- /dev/null\n+++ {source}\n@@ -0,0 +1,1 @@\n+body"),
            format!(
                "would remove {destination}\n--- {destination}\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-body"
            ),
        ] {
            assert!(preview.contains(&fragment), "{preview}");
        }
        assert!(!source.exists());
        assert_eq!(std::fs::read(&destination).expect("read"), b"body\n");
        assert_eq!(*host.undo.lock().expect("lock"), journal);
        host.undo().expect("undo");
        assert_eq!(std::fs::read(&source).expect("read"), b"body\n");
        assert!(!destination.exists());
    }

    #[test]
    fn undo_preview_reports_read_errors_and_keeps_all_records() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let host = test_host();
        assert!(host.preview_undo()[0].contains("nothing to undo"));
        let unreadable = root.join("directory");
        let readable = root.join("readable");
        std::fs::write(&unreadable, "original").expect("fixture");
        host.remember(&unreadable);
        std::fs::remove_file(&unreadable).expect("remove");
        std::fs::create_dir(&unreadable).expect("directory");
        host.remember(&readable);
        std::fs::write(&readable, "body\n").expect("fixture");
        let journal = host.undo.lock().expect("lock").clone();
        let preview = host.preview_undo().join("\n");
        assert!(
            preview.contains(&format!("`{unreadable}` could not be previewed:")),
            "{preview}"
        );
        assert!(
            preview.contains(&format!("would remove {readable}")),
            "{preview}"
        );
        assert!(preview.contains("-body"), "{preview}");
        assert_eq!(*host.undo.lock().expect("lock"), journal);
        assert!(unreadable.is_dir());
        assert_eq!(std::fs::read(&readable).expect("read"), b"body\n");
    }

    #[test]
    fn undo_preview_refuses_large_files_without_consuming_records() {
        let dir = tempfile::tempdir().expect("temp");
        let path = Utf8Path::from_path(dir.path()).expect("utf8").join("large");
        let host = test_host();
        host.remember(&path);
        std::fs::File::create(&path)
            .expect("file")
            .set_len(rune_tools::mutation::MAX_WRITE_BYTES as u64 + 1)
            .expect("length");
        assert!(
            host.preview_undo()
                .join("\n")
                .contains("could not be previewed")
        );
        assert!(host.can_undo());
    }

    #[cfg(unix)]
    #[test]
    fn undo_preview_refuses_fifo() {
        let dir = tempfile::tempdir().expect("temp");
        let path = Utf8Path::from_path(dir.path()).expect("utf8").join("fifo");
        let host = test_host();
        host.remember(&path);
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&path)
                .status()
                .expect("mkfifo")
                .success()
        );
        assert!(host.preview_undo().join("\n").contains("regular file"));
        assert!(host.can_undo());
    }

    #[test]
    fn undo_diffs_cover_context_newlines_binary_empty_and_unchanged_files() {
        let path = Utf8Path::new("file");
        assert_eq!(
            undo_diff(path, Some(b"same\n"), Some(b"same\n")),
            ["no byte changes"]
        );
        assert_eq!(undo_diff(path, None, None), ["no byte changes"]);
        assert_eq!(
            undo_diff(path, Some(b""), None),
            ["--- file", "+++ /dev/null", "@@ -0,0 +0,0 @@"]
        );
        assert_eq!(
            undo_diff(path, None, Some(b"")),
            ["--- /dev/null", "+++ file", "@@ -0,0 +0,0 @@"]
        );
        assert_eq!(
            undo_diff(path, Some(b"line"), Some(b"line\n")),
            [
                "--- file",
                "+++ file",
                "@@ -1,1 +1,1 @@",
                "-line",
                "\\ No newline at end of file",
                "+line",
            ]
        );
        for bytes in [&b"\xff"[..], &b"\0"[..]] {
            assert_eq!(
                undo_diff(path, Some(bytes), Some(b"a")),
                [
                    "--- file",
                    "+++ file",
                    "binary contents differ (1 -> 1 bytes)",
                ]
            );
        }
        let old = b"1\n2\n3\n4\n5\ncurrent\n7\n8\n9\n10\n";
        let new = b"1\n2\n3\n4\n5\noriginal\n7\n8\n9\n10\n";
        assert_eq!(
            undo_diff(path, Some(old), Some(new)),
            [
                "--- file",
                "+++ file",
                "@@ -3,7 +3,7 @@",
                " 3",
                " 4",
                " 5",
                "-current",
                "+original",
                " 7",
                " 8",
                " 9",
            ]
        );
    }

    #[test]
    fn a_bare_rename_asks_for_a_title() {
        // A rename with no title would set the session title to nothing, which
        // is not something the user can undo from the interface.
        let mut output = Vec::new();
        let action = handle_command(
            "rename",
            "  ",
            &empty_commands(),
            None,
            Utf8Path::new("/w"),
            &test_info(),
            &mut output,
        )
        .expect("handled");
        assert_eq!(action, Handled::Continue);
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("/rename <title>"), "{text}");
    }

    #[test]
    fn renaming_a_session_asks_for_a_new_title() {
        let mut output = Vec::new();
        let action = handle_command(
            "rename",
            "fix the parser",
            &empty_commands(),
            None,
            Utf8Path::new("/w"),
            &test_info(),
            &mut output,
        )
        .expect("handled");
        assert_eq!(action, Handled::Rename("fix the parser".to_owned()));
    }

    #[test]
    fn only_the_two_file_rewriting_tools_are_treated_as_mutations() {
        // A shell command changes something, but what it changed cannot be read
        // off its arguments, so offering an undo for it would be a lie.
        let write = serde_json::json!({ "path": "a.txt", "content": "x" });
        assert_eq!(
            mutating_path("write_file", &write).map(Utf8Path::to_owned),
            Some(Utf8PathBuf::from("a.txt"))
        );
        assert_eq!(
            mutating_path("edit_file", &write).map(Utf8Path::to_owned),
            Some(Utf8PathBuf::from("a.txt"))
        );
        assert_eq!(mutating_path("shell", &write), None);
        assert_eq!(mutating_path("read_file", &write), None);
        // A call missing its path is not a mutation this can track.
        assert_eq!(mutating_path("write_file", &serde_json::json!({})), None);
    }

    #[test]
    fn undoing_a_creation_removes_the_file() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let host = test_host();
        let created = root.join("made.txt");

        // Captured before the file exists, which is what the tool boundary does
        // on the call that creates it.
        host.remember(&created);
        std::fs::write(&created, "created by the session").expect("write");
        host.undo().expect("undo");
        assert!(!created.exists(), "the created file was left behind");
    }

    #[test]
    fn oversized_rename_snapshot_refuses_mutation_and_undo_records() {
        let dir = tempfile::tempdir().expect("workspace");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let mut host = test_host();
        host.context = ExecutionContext::new(root.to_owned());
        host.registry
            .insert(Box::new(rune_tools::RenameFile))
            .expect("tool");
        let source = root.join("source");
        let file = std::fs::File::create(&source).expect("source");
        file.set_len(rune_tools::mutation::MAX_WRITE_BYTES as u64 + 1)
            .expect("size");
        let output = host
            .execute(
                "rename_file",
                &serde_json::json!({"path":"source","new_path":"destination"}),
            )
            .expect("result");
        assert!(output.is_error);
        assert!(source.exists());
        assert!(!root.join("destination").exists());
        assert!(!host.can_undo());
    }

    #[cfg(unix)]
    #[test]
    fn fifo_rename_snapshot_is_refused_without_opening_it() {
        let dir = tempfile::tempdir().expect("workspace");
        let path = dir.path().join("fifo");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&path)
                .status()
                .expect("mkfifo")
                .success()
        );
        assert_eq!(
            rename_snapshot(Utf8Path::from_path(&path).expect("utf8"))
                .expect_err("regular files only")
                .code(),
            ErrorCode::UnsafePath
        );
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_rename_source_is_not_recorded_as_absent() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("workspace");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let source = root.join("source");
        std::fs::write(&source, "only copy").expect("source");
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o000))
            .expect("permissions");
        // Privileged test runners can still read mode-000 files.
        if std::fs::File::open(&source).is_ok() {
            return;
        }
        let mut host = test_host();
        host.context = ExecutionContext::new(root.to_owned());
        host.registry
            .insert(Box::new(rune_tools::RenameFile))
            .expect("tool");
        let output = host
            .execute(
                "rename_file",
                &serde_json::json!({"path":"source","new_path":"destination"}),
            )
            .expect("result");
        assert!(output.is_error);
        assert!(source.exists());
        assert!(!root.join("destination").exists());
        assert!(!host.can_undo());
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600))
            .expect("permissions");
        assert_eq!(std::fs::read(&source).expect("retained"), b"only copy");
    }

    #[test]
    fn undoing_a_file_rename_restores_the_source_and_removes_the_destination() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let mut host = test_host();
        host.context = ExecutionContext::new(root.to_owned());
        host.registry
            .insert(Box::new(rune_tools::RenameFile))
            .expect("registered");
        std::fs::write(root.join("source"), [0, 255, 10]).expect("fixture");
        let output = host
            .execute(
                "rename_file",
                &serde_json::json!({"path": "source", "new_path": "destination"}),
            )
            .expect("rename");
        assert!(!output.is_error);
        assert!(!root.join("source").exists());
        assert!(root.join("destination").exists());
        host.undo().expect("undo");
        assert_eq!(
            std::fs::read(root.join("source")).expect("read"),
            [0, 255, 10]
        );
        assert!(!root.join("destination").exists());
    }

    #[test]
    fn a_refused_file_rename_does_not_create_an_undo_record() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let mut host = test_host();
        host.context = ExecutionContext::new(root.to_owned());
        host.registry
            .insert(Box::new(rune_tools::RenameFile))
            .expect("registered");
        std::fs::create_dir(root.join("directory")).expect("fixture");
        let output = host
            .execute(
                "rename_file",
                &serde_json::json!({"path": "directory", "new_path": "destination"}),
            )
            .expect("tool error");
        assert!(output.is_error);
        assert!(!host.can_undo());
        assert!(root.join("directory").is_dir());
    }

    #[test]
    fn undoing_a_change_restores_the_original_bytes() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let host = test_host();
        let changed = root.join("kept.txt");
        std::fs::write(&changed, "original\n").expect("write");

        host.remember(&changed);
        std::fs::write(&changed, "replaced\n").expect("write");
        host.undo().expect("undo");
        assert_eq!(
            std::fs::read_to_string(&changed).expect("read"),
            "original\n"
        );
    }

    #[test]
    fn the_first_state_of_a_file_is_what_undo_returns_to() {
        // A chain of edits must come back to where the session found the file,
        // not to the middle of the chain.
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let host = test_host();
        let path = root.join("chain.txt");
        std::fs::write(&path, "one\n").expect("write");

        host.remember(&path);
        std::fs::write(&path, "two\n").expect("write");
        host.remember(&path);
        std::fs::write(&path, "three\n").expect("write");
        host.undo().expect("undo");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "one\n");
    }

    #[test]
    fn undoing_twice_reports_nothing_left_rather_than_doing_it_again() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let host = test_host();
        let path = root.join("once.txt");
        assert!(!host.can_undo());
        std::fs::write(&path, "body\n").expect("write");
        host.remember(&path);
        assert!(host.can_undo());
        host.undo().expect("undo");
        assert!(!host.can_undo());
    }

    #[test]
    fn compaction_preview_matches_the_applied_plan_and_preserves_history() {
        let (mut host, mut history, _) = compaction_fixture();
        host.endpoint.offline = true;
        let before = serde_json::to_value(&history).expect("history");
        let plan = rune_agent::compaction::plan(&history, &host.limits).expect("plan");
        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut sink = LockedSink {
            stream: Arc::new(Mutex::new(SharedBuffer(Arc::clone(&captured)))),
        };
        preview_compaction(&host, &history, &mut sink).expect("preview");
        let text = String::from_utf8(captured.lock().expect("lock").clone()).expect("utf8");
        for line in [
            "compaction preview (history unchanged)",
            "8 record(s) would be summarized",
            "1 record(s) retained verbatim",
        ] {
            assert!(text.contains(line), "{text}");
        }
        assert_eq!(serde_json::to_value(&history).expect("history"), before);
        assert!(host.events.lock().expect("events").is_empty());
        let retained = history.turns()[plan.removed_turns..].to_vec();
        let removed = rune_agent::compaction::apply(&mut history, &plan, "test summary");
        // Only the newest prompt is kept: four exchanges are summarized.
        assert_eq!(removed, 8);
        assert_eq!(history.len(), 1 + 1);
        // Applying renumbers sequence positions, but retains content verbatim.
        for (after, before) in history.turns()[1..].iter().zip(retained) {
            assert_eq!(after.role, before.role);
            assert_eq!(after.parts, before.parts);
            assert_eq!(after.replay, before.replay);
        }
    }

    #[test]
    fn compact_preview_arguments_dispatch_without_starting_compaction() {
        for (arguments, expected) in [
            ("preview", Handled::CompactPreview),
            ("  preview  ", Handled::CompactPreview),
            ("", Handled::Compact),
        ] {
            let mut output = Vec::new();
            assert_eq!(
                handle_command(
                    "compact",
                    arguments,
                    &empty_commands(),
                    None,
                    Utf8Path::new("/w"),
                    &test_info(),
                    &mut output,
                )
                .expect("handled"),
                expected,
            );
        }
    }

    #[test]
    fn compacting_a_short_conversation_says_so_rather_than_failing() {
        let host = test_host();
        let mut history = History::new();
        history.push_user("hello");
        let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let mut sink = LockedSink {
            stream: Arc::new(Mutex::new(SharedBuffer(Arc::clone(&captured)))),
        };
        compact_history(&host, &mut history, &mut sink).expect("compacted");
        let text = String::from_utf8(captured.lock().expect("lock").clone()).expect("utf8");
        assert!(text.contains("nothing to compact"), "{text}");
        assert_eq!(history.len(), 1, "the conversation was changed anyway");
    }

    /// A summarizer reply in the checkpoint shape.
    const CHECKPOINT_REPLY: &str = "## Goal\n- finish the fixture task\n## Constraints\n- none\n## Decisions\n- none\n## State\n### Done\n- the earlier fixture exchanges established the task and its progress\n### In progress\n- none\n### Blocked\n- none\n## Next steps\n- continue using the retained conversation\n## Files that matter\n- none\n## Facts to keep\n- none";

    struct SummaryFixture {
        reply: String,
        status: u16,
        finish_reason: &'static str,
        requests: Mutex<Vec<serde_json::Value>>,
        events: Option<Arc<Mutex<Vec<Event>>>>,
    }

    impl SummaryFixture {
        fn new(reply: &str) -> Self {
            Self {
                reply: reply.to_owned(),
                status: 200,
                finish_reason: "stop",
                requests: Mutex::new(Vec::new()),
                events: None,
            }
        }
    }

    impl rune_net::fetch::Fetch for SummaryFixture {
        fn send(
            &self,
            request: rune_net::fetch::FetchRequest,
        ) -> rune_net::error::NetResult<rune_net::fetch::FetchResponse> {
            let body: serde_json::Value =
                serde_json::from_slice(&request.body).expect("request JSON");
            if body.to_string().contains("<conversation-checkpoint>") {
                let events = self
                    .events
                    .as_ref()
                    .expect("event observer")
                    .lock()
                    .expect("events");
                assert!(
                    events
                        .iter()
                        .any(|event| matches!(event, Event::ContextCompacted { .. })),
                    "the compaction event must precede the next model request"
                );
            }
            self.requests.lock().expect("requests").push(body);
            let body = format!(
                "data: {}\n\ndata: [DONE]\n\n",
                serde_json::json!({
                    "choices": [{"delta": {"content": self.reply}, "finish_reason": self.finish_reason}]
                })
            );
            Ok(rune_net::fetch::FetchResponse {
                status: self.status,
                content_type: "text/event-stream".to_owned(),
                retry_after: None,
                retry_after_ms: None,
                should_retry: None,
                location: None,
                body: Box::new(std::io::Cursor::new(body.into_bytes())),
            })
        }
    }

    fn compaction_fixture() -> (SessionHost, History, rune_net::provider::RequestPlan) {
        let mut host = test_host();
        for (name, value) in [
            (rune_core::budget::LimitName::MaxToolResultBytes, 16 * 1024),
            (rune_core::budget::LimitName::CompactionTriggerPercent, 50),
            (rune_core::budget::LimitName::CompactionKeepTokens, 0),
        ] {
            host.limits
                .set(
                    name,
                    rune_core::budget::Budget::Bounded(value),
                    rune_core::config::Layer::User,
                )
                .expect("limit");
        }
        let mut history = History::new();
        for index in 0..4 {
            history.push_user(format!("fixture question {index}"));
            history.push_assistant(vec![rune_net::message::ContentPart::Text {
                text: "earlier answer ".repeat(100),
            }]);
        }
        history.push_user("continue the fixture");
        host.capture_history(&history);
        let mut plan = rune_net::provider::RequestPlan::new(host.model());
        plan.messages = history.to_messages();
        let tokens = request_estimate(&host, &plan, None)
            .expect("estimate")
            .input_tokens;
        host.context_limit.store(
            tokens.saturating_mul(100).saturating_div(60),
            std::sync::atomic::Ordering::Relaxed,
        );
        (host, history, plan)
    }

    #[test]
    fn failed_manual_archive_keeps_history_and_previous_records() {
        let (mut host, mut history, _) = compaction_fixture();
        let temp = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(temp.path()).expect("utf8");
        let paths = Paths {
            state_root: root.join("state"),
            config_root: root.join("config"),
            data_root: root.join("data"),
        };
        let id = "archivefail1".parse().expect("id");
        let recorder = Recorder::create(&paths, &id).expect("recorder");
        let archive = paths.session_dir(&id).join("compaction-archive.json");
        rune_core::paths::write_private(&archive, "broken archive").expect("fixture");
        host.recorder = Some(Arc::new(Mutex::new(recorder)));
        let before = history.clone();
        let plan = rune_agent::compaction::plan(&history, &host.limits).expect("plan");
        let fetch = SummaryFixture::new(CHECKPOINT_REPLY);
        let error = compact_session(&host, &mut history, &plan, &fetch, true)
            .expect_err("archive failure must prevent replacement");
        assert_eq!(error.code(), ErrorCode::CorruptRecord);
        assert!(error.message().contains("history unchanged"));
        assert_eq!(history.turns(), before.turns());
        assert_eq!(
            std::fs::read_to_string(archive).expect("old archive"),
            "broken archive"
        );
        assert_eq!(
            *host.transcript.lock().expect("transcript"),
            history_entries(before.turns())
        );
    }

    #[test]
    fn pin_dispatch_preserves_exact_arguments_and_supports_listing() {
        for arguments in ["", "  Keep café and 日本語.\nNever commit.  "] {
            let mut output = Vec::new();
            assert_eq!(
                handle_command(
                    "pin",
                    arguments,
                    &empty_commands(),
                    None,
                    Utf8Path::new("/w"),
                    &test_info(),
                    &mut output,
                )
                .expect("dispatch"),
                Handled::Pin((!arguments.is_empty()).then(|| arguments.to_owned()))
            );
        }
        assert!(rune_term::commands::find("pin").is_some());
    }

    #[test]
    fn automatic_compaction_keeps_three_exact_pins_in_every_following_provider_request() {
        let constraints = [
            "Never commit.",
            "Preserve café and 日本語.\nKeep whitespace.",
            "  No new dependencies.  ",
        ];
        let (host, mut history, mut plan) = compaction_fixture();
        for constraint in constraints {
            history.pin_constraint(constraint).expect("pin");
        }
        let mut fetch = SummaryFixture::new(CHECKPOINT_REPLY);
        fetch.events = Some(Arc::clone(&host.events));
        for round in 0..3 {
            if round > 0 {
                for index in 0..4 {
                    history.push_user(format!("round {round} question {index}"));
                    history.push_assistant(vec![rune_net::message::ContentPart::Text {
                        text: "earlier answer ".repeat(100),
                    }]);
                }
                history.push_user("continue the fixture");
            }
            plan.messages = history.to_messages();
            let before = fetch.requests.lock().expect("requests").len();
            host.prepare_request(&mut history, &mut plan, &fetch)
                .expect("automatic compaction");
            assert_eq!(fetch.requests.lock().expect("requests").len(), before + 1);
            for constraint in constraints {
                let requests = fetch.requests.lock().expect("requests");
                let messages = requests.last().expect("summary request")["messages"]
                    .as_array()
                    .expect("summary messages");
                assert!(messages.iter().any(|message| {
                    message["role"] == "user"
                        && message["content"]
                            .as_str()
                            .is_some_and(|text| text.contains(constraint))
                }));
            }
            assert_eq!(history.pinned_constraints(), constraints);
            rune_net::transport::stream_completion(
                &fetch,
                &host.endpoint,
                host.dialect.as_ref(),
                &plan,
                rune_net::transport::RequestTimeouts::from_settings(&host.limits, None),
                &|| false,
            )
            .expect("following provider request");
            let requests = fetch.requests.lock().expect("requests");
            let messages = requests.last().expect("following request")["messages"]
                .as_array()
                .expect("messages");
            let user_text: Vec<&str> = messages
                .iter()
                .filter(|message| message["role"] == "user")
                .filter_map(|message| message["content"].as_str())
                .collect();
            for constraint in constraints {
                assert!(
                    user_text.iter().any(|text| text.contains(constraint)),
                    "missing exact pin: {constraint}"
                );
            }
        }
    }

    #[test]
    fn pins_that_exceed_retained_capacity_block_compaction_without_dropping_constraints() {
        let (host, mut history, mut plan) = compaction_fixture();
        history
            .pin_constraint("exact pinned text ".repeat(2_000))
            .expect("pin");
        plan.messages = history.to_messages();
        let before = history.to_messages();
        let fetch =
            SummaryFixture::new("An unused summary because the pins cannot fit in the window.");
        assert_eq!(
            host.prepare_request(&mut history, &mut plan, &fetch)
                .expect_err("pins exceed capacity")
                .code(),
            ErrorCode::TooLarge
        );
        assert!(fetch.requests.lock().expect("requests").is_empty());
        assert_eq!(history.to_messages(), before);
        assert_eq!(plan.messages, before);
    }

    #[test]
    fn crossing_the_configured_trigger_compacts_before_the_next_request() {
        let (host, mut history, mut plan) = compaction_fixture();
        let mut fetch = SummaryFixture::new(CHECKPOINT_REPLY);
        fetch.events = Some(Arc::clone(&host.events));
        let before = history.clone();
        let cut = rune_agent::compaction::plan(&history, &host.limits)
            .expect("plan")
            .removed_turns;
        let estimate = request_estimate(&host, &plan, None).expect("estimate");
        assert!(estimate.used_percent() >= 50 && estimate.used_percent() < 80);
        host.prepare_request(&mut history, &mut plan, &fetch)
            .expect("automatic compaction");
        assert_eq!(fetch.requests.lock().expect("requests").len(), 1);
        assert!(
            fetch.requests.lock().expect("requests")[0]
                .to_string()
                .contains("<transcript>")
        );
        assert_eq!(plan.messages, history.to_messages());
        assert!(
            history.turns()[0]
                .text()
                .contains("<conversation-checkpoint>")
        );
        for (retained, original) in history.turns()[1..].iter().zip(&before.turns()[cut..]) {
            assert_eq!(retained.parts, original.parts);
        }
        assert!(matches!(
            host.events.lock().expect("events").last(),
            Some(Event::ContextCompacted { .. })
        ));
        assert!(
            request_estimate(&host, &plan, None)
                .expect("estimate")
                .used_percent()
                < 50
        );
        host.capture_history(&history);
        assert_eq!(
            *host.transcript.lock().expect("transcript"),
            history_entries(before.turns()),
            "compaction must preserve the visible transcript without inserting the handoff"
        );
        history.validate().expect("compacted history is valid");
        rune_net::transport::stream_completion(
            &fetch,
            &host.endpoint,
            host.dialect.as_ref(),
            &plan,
            rune_net::transport::RequestTimeouts::from_settings(&host.limits, None),
            &|| false,
        )
        .expect("next model request");
        assert_eq!(fetch.requests.lock().expect("requests").len(), 2);
    }

    #[test]
    fn an_oversized_removable_prefix_compacts_with_a_small_retained_tail() {
        let (host, _, mut plan) = compaction_fixture();
        let mut history = History::new();
        history.push_user("earlier task");
        history.push_assistant(vec![rune_net::message::ContentPart::Text {
            text: "old answer ".repeat(10_000),
        }]);
        for index in 0..4 {
            history.push_user(format!("recent question {index}"));
            history.push_assistant(vec![rune_net::message::ContentPart::Text {
                text: "recent answer".to_owned(),
            }]);
        }
        history.push_user("continue the fixture");
        plan.messages = history.to_messages();
        plan.instructions = "preserve the current task".to_owned();
        plan.tools = inventory::advertisement(&host.registry);
        plan.max_output_tokens = Some(128);
        host.context_limit
            .store(10_000, std::sync::atomic::Ordering::Relaxed);
        let estimate = request_estimate(&host, &plan, None).expect("estimate");
        assert!(estimate.input_tokens > estimate.capacity);
        let cut = rune_agent::compaction::plan(&history, &host.limits).expect("cut");
        let retained = history.to_messages()[cut.removed_turns..].to_vec();
        let fetch = SummaryFixture::new(CHECKPOINT_REPLY);

        host.prepare_request(&mut history, &mut plan, &fetch)
            .expect("the removable prefix can be compacted");

        assert_eq!(fetch.requests.lock().expect("requests").len(), 1);
        assert_eq!(&history.to_messages()[1..], retained);
        assert_eq!(plan.messages, history.to_messages());
        assert!(
            request_estimate(&host, &plan, None)
                .expect("estimate")
                .input_tokens
                <= estimate.capacity
        );
        assert!(matches!(
            host.events.lock().expect("events").last(),
            Some(Event::ContextCompacted { .. })
        ));
        history.validate().expect("valid retained tail");
    }

    #[test]
    fn oversized_retained_requests_are_refused_before_summarizing() {
        for overhead in ["tail", "instructions", "tools"] {
            let (host, mut history, mut plan) = compaction_fixture();
            plan.max_output_tokens = Some(128);
            let cut = rune_agent::compaction::plan(&history, &host.limits).expect("cut");
            let mut retained_plan = plan.clone();
            retained_plan.messages.drain(..cut.removed_turns);
            let tail_tokens = request_estimate(&host, &retained_plan, None)
                .expect("tail estimate")
                .input_tokens;
            match overhead {
                "instructions" => plan.instructions = "instructions ".repeat(1000),
                "tools" => plan.tools = inventory::advertisement(&host.registry),
                _ => {}
            }
            retained_plan.instructions.clone_from(&plan.instructions);
            retained_plan.tools.clone_from(&plan.tools);
            let retained_tokens = request_estimate(&host, &retained_plan, None)
                .expect("retained request estimate")
                .input_tokens;
            if overhead != "tail" {
                assert!(retained_tokens > tail_tokens);
            }
            host.context_limit.store(
                retained_tokens - 1 + 128,
                std::sync::atomic::Ordering::Relaxed,
            );
            let before = history.to_messages();
            let fetch = SummaryFixture::new("unused");

            let error = host
                .prepare_request(&mut history, &mut plan, &fetch)
                .expect_err("retained request cannot fit");

            assert_eq!(error.code(), ErrorCode::TooLarge, "{overhead}");
            assert!(fetch.requests.lock().expect("requests").is_empty());
            assert_eq!(history.to_messages(), before);
            assert_eq!(plan.messages, before);
            assert!(host.events.lock().expect("events").is_empty());
        }
    }

    #[test]
    fn an_oversized_short_history_without_a_removable_prefix_is_refused() {
        let (host, _, mut plan) = compaction_fixture();
        let mut history = History::new();
        history.push_user("large prompt ".repeat(1000));
        plan.messages = history.to_messages();
        host.context_limit
            .store(1000, std::sync::atomic::Ordering::Relaxed);
        assert!(rune_agent::compaction::plan(&history, &host.limits).is_none());
        let before = history.to_messages();
        let fetch = SummaryFixture::new("unused");

        let error = host
            .prepare_request(&mut history, &mut plan, &fetch)
            .expect_err("no prefix can be removed");

        assert_eq!(error.code(), ErrorCode::TooLarge);
        assert!(fetch.requests.lock().expect("requests").is_empty());
        assert_eq!(history.to_messages(), before);
        assert_eq!(plan.messages, before);
    }

    #[test]
    fn below_the_trigger_no_summary_request_is_sent() {
        let (host, mut history, mut plan) = compaction_fixture();
        host.context_limit
            .store(100_000, std::sync::atomic::Ordering::Relaxed);
        let fetch = SummaryFixture::new("unused");
        let before = history.to_messages();
        host.prepare_request(&mut history, &mut plan, &fetch)
            .expect("fits");
        assert!(fetch.requests.lock().expect("requests").is_empty());
        assert_eq!(history.to_messages(), before);
        assert!(host.events.lock().expect("events").is_empty());
    }

    #[test]
    fn failed_automatic_compaction_preserves_history_and_blocks_the_model_request() {
        for (reply, status) in [("", 200), ("too short", 200), ("provider error", 500)] {
            let (host, mut history, mut plan) = compaction_fixture();
            let mut fetch = SummaryFixture::new(reply);
            fetch.status = status;
            let before = history.to_messages();
            assert!(
                host.prepare_request(&mut history, &mut plan, &fetch)
                    .is_err()
            );
            assert_eq!(history.to_messages(), before);
            assert_eq!(plan.messages, before);
            assert!(host.events.lock().expect("events").is_empty());
        }
    }

    #[test]
    fn max_tokens_compaction_summary_preserves_history_with_an_incomplete_summary_error() {
        for automatic in [false, true] {
            let (host, mut history, mut request_plan) = compaction_fixture();
            let reply =
                "This summary was stopped halfway through its account of the work completed.";
            rune_agent::compaction::validate_summary(reply).expect("otherwise usable summary");
            let mut fetch = SummaryFixture::new(reply);
            fetch.finish_reason = "length";
            let before = history.clone();
            let error = if automatic {
                host.prepare_request(&mut history, &mut request_plan, &fetch)
                    .expect_err("automatic compaction must reject a truncated summary")
            } else {
                let plan = rune_agent::compaction::plan(&history, &host.limits).expect("plan");
                compact_session(&host, &mut history, &plan, &fetch, false)
                    .expect_err("the shared summarizer must reject a truncated summary")
            };
            assert_eq!(error.code(), ErrorCode::InvalidState);
            assert!(error.message().contains("incomplete summary"), "{error}");
            assert!(error.message().contains("token limit"), "{error}");
            assert!(error.message().contains("nothing was changed"), "{error}");
            assert_eq!(history.turns(), before.turns());
            assert_eq!(request_plan.messages, before.to_messages());
            // The cut reply is never kept; the nudge gets the same answer.
            assert_eq!(fetch.requests.lock().expect("requests").len(), 2);
            assert!(host.events.lock().expect("events").is_empty());
            assert_eq!(
                *host.transcript.lock().expect("transcript"),
                history_entries(before.turns()),
            );
        }
    }

    #[test]
    fn compaction_estimates_include_instructions_tools_and_dialect_output_reserve() {
        let mut host = test_host();
        host.dialect = Box::new(rune_net::anthropic::Anthropic);
        host.context_limit
            .store(20_000, std::sync::atomic::Ordering::Relaxed);
        let mut plan = rune_net::provider::RequestPlan::new(host.model());
        plan.messages = rune_net::transport::one_shot_messages("hello");
        let small = request_estimate(&host, &plan, None).expect("estimate");
        plan.instructions = "instructions ".repeat(100);
        plan.tools = inventory::advertisement(&host.registry);
        let full = request_estimate(&host, &plan, None).expect("estimate");
        assert!(full.input_tokens > small.input_tokens);
        assert_eq!(
            full.capacity,
            20_000 - rune_net::anthropic::DEFAULT_MAX_TOKENS
        );
        assert_eq!(
            full.output_tokens, 0,
            "output reserve is subtracted exactly once"
        );
    }

    #[test]
    fn compaction_during_a_turn_keeps_new_transcript_entries_and_partial_answers() {
        let (host, mut history, mut plan) = compaction_fixture();
        history.push_assistant(vec![rune_net::message::ContentPart::Text {
            text: "completed step".to_owned(),
        }]);
        history.push_user("steered correction");
        plan.messages = history.to_messages();
        let fetch = SummaryFixture::new(CHECKPOINT_REPLY);
        host.prepare_request(&mut history, &mut plan, &fetch)
            .expect("compacted");
        let start = host
            .transcript_history_len
            .load(std::sync::atomic::Ordering::Relaxed);
        retain_partial_answer(&mut history, start, "partial next step");
        host.capture_history(&history);
        let entries = host.transcript.lock().expect("transcript");
        for text in ["completed step", "steered correction", "partial next step"] {
            assert_eq!(entries.iter().filter(|entry| entry.text == text).count(), 1);
        }
    }

    /// A sink that appends to a buffer the test can read back.
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn the_last_reply_is_read_from_the_conversation_not_the_screen() {
        let mut history = History::new();
        history.push_user("do a thing");
        assert_eq!(last_reply(&history), None, "a user turn is not a reply");
        history.push_assistant(vec![rune_net::message::ContentPart::Text {
            text: "done".to_owned(),
        }]);
        assert_eq!(last_reply(&history).as_deref(), Some("done"));
    }

    #[test]
    fn the_last_reply_is_the_most_recent_one() {
        let mut history = History::new();
        for text in ["first", "second"] {
            history.push_assistant(vec![rune_net::message::ContentPart::Text {
                text: text.to_owned(),
            }]);
        }
        assert_eq!(last_reply(&history).as_deref(), Some("second"));
    }

    #[test]
    fn a_reply_holds_no_tool_call_arguments() {
        // A tool call carries JSON the user never wrote, so copying it would
        // paste an argument list into whatever they are working on.
        let mut history = History::new();
        history.push_assistant(vec![
            rune_net::message::ContentPart::Text {
                text: "looking".to_owned(),
            },
            rune_net::message::ContentPart::ToolCall {
                id: rune_core::id::ToolCallId::new("c1").expect("id"),
                name: "shell".to_owned(),
                arguments: "{\"command\":\"rm -rf /\"}".to_owned(),
            },
        ]);
        let reply = last_reply(&history).expect("a reply");
        assert_eq!(reply, "looking");
        assert!(!reply.contains("rm -rf"), "{reply}");
    }

    #[test]
    fn base64_matches_the_standard_encoding() {
        // The clipboard is decoded by the terminal, so the alphabet and padding
        // have to be exactly the standard ones.
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(input.as_bytes()), expected, "{input:?}");
        }
    }

    #[test]
    fn base64_encodes_multibyte_text_by_its_bytes() {
        // The text is UTF-8 on the wire, so an emoji is four bytes and must not
        // be encoded as one character.
        assert_eq!(base64_encode("é".as_bytes()), "w6k=");
    }

    #[test]
    fn the_copy_and_new_commands_are_recognized() {
        for (name, expected) in [
            ("copy", Handled::Copy),
            ("new", Handled::NewSession),
            ("fork", Handled::Fork),
        ] {
            let mut output = Vec::new();
            let action = handle_command(
                name,
                "",
                &empty_commands(),
                None,
                Utf8Path::new("/w"),
                &test_info(),
                &mut output,
            )
            .expect("handled");
            assert_eq!(action, expected, "/{name}");
        }
    }

    #[test]
    fn starting_a_new_session_repoints_the_status_line() {
        let host = test_host();
        assert_eq!(host.session_id_name(), "sessiontest1");
        host.set_session_id("another0session");
        assert_eq!(host.session_id_name(), "another0session");
        let info = host.info("p", "e");
        assert_eq!(info.session_id, "another0session");
    }

    #[test]
    fn starting_a_new_session_forgets_the_old_context_reading() {
        let host = test_host();
        host.record_context_size(4_000);
        let info = host.info("p", "e");
        assert_eq!(info.context_used, 4_000);
        host.forget_context();
        let info = host.info("p", "e");
        assert_eq!(info.context_used, 0);
    }

    #[test]
    fn the_dropdown_offers_a_command_being_typed() {
        let commands = command_candidates(&[]);
        let theme = Theme::no_color();
        let rows = completion_rows(
            &commands,
            "/mod",
            0,
            &theme,
            ColorDepth::Ansi256,
            usize::MAX,
        );
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows[0].starts_with("> /model"), "{rows:?}");
        assert!(rows[1].starts_with("  /models"), "{rows:?}");
        // Each row carries what the command does, which is what makes the list
        // usable without trying every name.
        assert!(rows[0].contains("choose a model"), "{rows:?}");
        assert!(rows[1].contains("same as"), "{rows:?}");
    }

    #[test]
    fn discovered_custom_command_completes_and_expands_for_review() {
        let temp = tempfile::tempdir().expect("workspace");
        let root = Utf8PathBuf::from_path_buf(temp.path().to_owned()).expect("UTF-8");
        std::fs::create_dir_all(root.join(".rune/commands")).expect("commands directory");
        std::fs::write(
            root.join(".rune/commands/audit-command.md"),
            "Describe audit command files.",
        )
        .expect("command file");
        let discovery = rune_context::commands::discover(&root, &root.join("config"))
            .expect("discover commands");
        assert!(discovery.warnings.is_empty());
        let commands = command_candidates(&discovery.commands);
        let theme = Theme::no_color();
        let context = ExecutionContext::new(root.clone());
        let limits = rune_tools::workspace::FileLimits::default();

        for key in [KeyAction::Complete, KeyAction::Submit] {
            let mut reader = rune_term::input::KeyReader::new();
            reader.replace("/audit");
            let mut selected = 0;
            let rows = completion_rows(
                &commands,
                reader.line(),
                selected,
                &theme,
                ColorDepth::Ansi256,
                8,
            );
            assert_eq!(rows, ["> /audit-command  Describe audit command files."]);
            assert_eq!(completion_matches(&commands, reader.line()), 1);
            assert!(!path_key(
                &commands,
                key,
                &mut reader,
                &mut None,
                &mut selected,
                &context,
                &limits,
            ));
            assert!(matches!(
                idle_key(&commands, key, &mut reader, &mut selected, &rows, &[]),
                Idle::Stay
            ));
            assert_eq!(reader.line(), "/audit-command");

            let rows = completion_rows(
                &commands,
                reader.line(),
                selected,
                &theme,
                ColorDepth::Ansi256,
                8,
            );
            let Idle::Submit(Input::Command { name, arguments }) = idle_key(
                &commands,
                KeyAction::Submit,
                &mut reader,
                &mut selected,
                &rows,
                &[],
            ) else {
                panic!("a complete custom command must submit");
            };
            assert_eq!(name, "audit-command");
            assert!(arguments.is_empty());
            let result = handle_command(
                &name,
                &arguments,
                &discovery,
                None,
                &root,
                &test_info(),
                &mut Vec::new(),
            )
            .expect("expand command");
            assert_eq!(
                result,
                Handled::Expand {
                    text: "Describe audit command files.".to_owned(),
                    model: None,
                }
            );
        }
    }

    #[test]
    fn custom_completion_navigation_includes_project_and_user_commands() {
        let temp = tempfile::tempdir().expect("workspace");
        let root = Utf8PathBuf::from_path_buf(temp.path().to_owned()).expect("UTF-8");
        std::fs::create_dir_all(root.join(".rune/commands")).expect("project commands");
        std::fs::create_dir_all(root.join("config/commands")).expect("user commands");
        for index in 0..8 {
            std::fs::write(
                root.join(format!(".rune/commands/audit-{index}.md")),
                "Audit the workspace.",
            )
            .expect("project command");
        }
        std::fs::write(root.join(".rune/commands/model.md"), "Shadow the builtin.")
            .expect("conflicting builtin name");
        std::fs::write(
            root.join("config/commands/model-notes.md"),
            "---\ndescription: Describe a model\nargument-hint: <id>\n---\nDescribe $1.",
        )
        .expect("user command");
        let discovery = rune_context::commands::discover(&root, &root.join("config"))
            .expect("discover commands");
        let commands = command_candidates(&discovery.commands);
        assert_eq!(commands.len(), rune_term::commands::BUILTINS.len() + 9);
        let theme = Theme::no_color();
        let rows = completion_rows(&commands, "/mod", 2, &theme, ColorDepth::Ansi256, 8);
        assert_eq!(rows.len(), 3);
        assert!(rows[0].starts_with("  /model [id]"));
        assert!(rows[1].starts_with("  /models"));
        assert!(rows[2].starts_with("> /model-notes <id>"));
        assert!(rows[2].contains("Describe a model"));
        assert_eq!(
            open_completion(&commands, "/mod", &rows, 2)
                .expect("user candidate")
                .name,
            "model-notes"
        );
        for line in ["/model-notes arg", "/audit-0 ", "/unknown", "prompt"] {
            assert!(completion_rows(&commands, line, 0, &theme, ColorDepth::Ansi256, 8).is_empty());
            assert_eq!(completion_matches(&commands, line), 0);
        }

        // Arrows reach every candidate even when only one menu row fits.
        for prefix in ["/", "/audit"] {
            let matches = matching_commands(&commands, prefix);
            for room in [1, 2, 8] {
                let mut reader = rune_term::input::KeyReader::new();
                reader.replace(prefix);
                let mut selected = 0;
                for expected in &matches {
                    let rows = completion_rows(
                        &commands,
                        prefix,
                        selected,
                        &theme,
                        ColorDepth::Ansi256,
                        room,
                    );
                    assert!(rows.len() <= room);
                    assert!(
                        rows.iter()
                            .any(|row| row.starts_with(&format!("> /{}", expected.name)))
                    );
                    assert_eq!(
                        open_completion(&commands, prefix, &rows, selected)
                            .expect("selected candidate")
                            .name,
                        expected.name
                    );
                    assert!(matches!(
                        idle_key(
                            &commands,
                            KeyAction::Down,
                            &mut reader,
                            &mut selected,
                            &rows,
                            &[]
                        ),
                        Idle::Stay
                    ));
                    assert_eq!(reader.line(), prefix);
                }
                assert_eq!(selected, matches.len() - 1);
                let rows = completion_rows(
                    &commands,
                    prefix,
                    selected,
                    &theme,
                    ColorDepth::Ansi256,
                    room,
                );
                assert!(matches!(
                    idle_key(
                        &commands,
                        KeyAction::Up,
                        &mut reader,
                        &mut selected,
                        &rows,
                        &[]
                    ),
                    Idle::Stay
                ));
                assert_eq!(selected, matches.len() - 2);
                let rows = completion_rows(
                    &commands,
                    prefix,
                    selected,
                    &theme,
                    ColorDepth::Ansi256,
                    room,
                );
                let expected = matches[selected].name;
                assert!(matches!(
                    idle_key(
                        &commands,
                        KeyAction::Complete,
                        &mut reader,
                        &mut selected,
                        &rows,
                        &[]
                    ),
                    Idle::Stay
                ));
                assert_eq!(reader.line(), format!("/{expected}"));
                assert_eq!(selected, 0);
            }
        }
    }

    #[test]
    fn path_menus_scroll_with_the_selection_and_keep_within_available_rows() {
        let paths = crate::path_completion::Paths {
            range: 5..7,
            matches: (0..15)
                .map(|index| format!("'fixture {index:02}.txt'"))
                .collect(),
        };
        for selected in 0..paths.matches.len() {
            for room in 1..=8 {
                let rows = path_completion_rows(
                    &paths,
                    selected,
                    &Theme::no_color(),
                    ColorDepth::Ansi256,
                    room,
                );
                assert!(rows.len() <= room);
                assert!(
                    rows.contains(&format!("> {}", paths.matches[selected])),
                    "{rows:?}"
                );
                assert!(rows.iter().all(|row| !row.contains('\u{1b}')));
            }
        }
        assert!(
            path_completion_rows(&paths, 0, &Theme::no_color(), ColorDepth::Ansi256, 0).is_empty()
        );
    }

    #[test]
    fn tab_completes_one_match_without_opening_a_menu_or_changing_slash_commands() {
        let commands = command_candidates(&[]);
        let temp = tempfile::tempdir().expect("workspace");
        let root = Utf8PathBuf::from_path_buf(temp.path().to_owned()).expect("UTF-8");
        std::fs::write(root.join("fixture space.txt"), "fixture").expect("file");
        let context = ExecutionContext::new(root);
        let limits = rune_tools::workspace::FileLimits::default();
        let mut reader = rune_term::input::KeyReader::new();
        let mut paths = None;
        let mut selected = 0;
        reader.replace("read ./fi");
        assert!(path_key(
            &commands,
            KeyAction::Complete,
            &mut reader,
            &mut paths,
            &mut selected,
            &context,
            &limits
        ));
        assert_eq!(reader.line(), "read './fixture space.txt'");
        assert!(paths.is_none());
        for line in ["/mod", "/help fixture", "/zzz"] {
            reader.replace(line);
            assert!(!path_key(
                &commands,
                KeyAction::Complete,
                &mut reader,
                &mut paths,
                &mut selected,
                &context,
                &limits
            ));
            assert_eq!(reader.line(), line);
        }
    }

    #[test]
    fn a_bare_slash_offers_a_window_and_says_how_many_there_are() {
        let commands = command_candidates(&[]);
        // Every command matches a bare slash. Drawing all of them would swallow
        // the screen for a list a reader only ever takes the top few rows of, so
        // a window is shown with a row saying how far through it is.
        let theme = Theme::no_color();
        let rows = completion_rows(&commands, "/", 0, &theme, ColorDepth::Ansi256, usize::MAX);
        let total = rune_term::commands::BUILTINS.len();
        assert_eq!(rows.len(), COMPLETION_WINDOW.saturating_add(1), "{rows:?}");
        assert!(rows[0].starts_with("> /model"), "{rows:?}");
        assert!(
            rows.last().is_some_and(|r| r.contains("1-6 of")),
            "{rows:?}"
        );
        assert!(
            rows.last().is_some_and(|r| r.contains(&total.to_string())),
            "{rows:?}"
        );
    }

    #[test]
    fn a_list_that_fits_carries_no_position_row() {
        let commands = command_candidates(&[]);
        let theme = Theme::no_color();
        let rows = completion_rows(
            &commands,
            "/mod",
            0,
            &theme,
            ColorDepth::Ansi256,
            usize::MAX,
        );
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(!rows.iter().any(|r| r.contains(" of ")), "{rows:?}");
    }

    #[test]
    fn the_window_follows_the_selection_without_the_input_moving() {
        // The highlight moves down a fixed screen until it reaches the last row,
        // and only then does the window scroll. That is what keeps the list from
        // jumping on every key.
        let count = rune_term::commands::BUILTINS.len();
        // Inside the first screenful the window does not move.
        assert_eq!(
            completion_window(count, 0, COMPLETION_WINDOW),
            0..COMPLETION_WINDOW
        );
        assert_eq!(
            completion_window(count, COMPLETION_WINDOW - 1, COMPLETION_WINDOW),
            0..COMPLETION_WINDOW
        );
        // Past it, the selection stays on the last visible row.
        let scrolled = completion_window(count, COMPLETION_WINDOW, COMPLETION_WINDOW);
        assert_eq!(scrolled, 1..COMPLETION_WINDOW.saturating_add(1));
        // And the end of the list is reachable rather than cut off.
        let last = completion_window(count, count.saturating_sub(1), COMPLETION_WINDOW);
        assert_eq!(last.end, count);
    }

    #[test]
    fn a_window_never_looks_past_the_end_of_a_short_list() {
        assert_eq!(completion_window(2, 0, COMPLETION_WINDOW), 0..2);
        assert_eq!(completion_window(0, 0, COMPLETION_WINDOW), 0..0);
        // A selection past the end is clamped rather than panicking.
        let count = rune_term::commands::BUILTINS.len();
        assert!(completion_window(count, 999, COMPLETION_WINDOW).end <= count);
    }

    #[test]
    fn the_highlight_is_always_inside_the_window() {
        let commands = command_candidates(&[]);
        // A highlight scrolled out of view would make the list unusable: the
        // user is moving something they cannot see.
        let theme = Theme::no_color();
        let count = rune_term::commands::BUILTINS.len();
        for selected in 0..count {
            let rows = completion_rows(
                &commands,
                "/",
                selected,
                &theme,
                ColorDepth::Ansi256,
                usize::MAX,
            );
            let marked = rows.iter().filter(|r| r.starts_with('>')).count();
            assert_eq!(marked, 1, "selection {selected} marked {marked} rows");
        }
    }

    #[test]
    fn short_completion_menus_keep_every_command_visible_and_selectable() {
        let commands = command_candidates(&[]);
        let theme = Theme::no_color();
        let count = rune_term::commands::BUILTINS.len();
        for room in 1..=8 {
            for selected in 0..count {
                let rows =
                    completion_rows(&commands, "/", selected, &theme, ColorDepth::Ansi256, room);
                assert!(rows.len() <= room, "{rows:?}");
                let chosen = open_completion(&commands, "/", &rows, selected).expect("completion");
                assert!(
                    rows.iter()
                        .any(|row| row.starts_with(&format!("> /{}", chosen.name))),
                    "selection {selected} is hidden: {rows:?}"
                );
            }
        }
        assert!(completion_rows(&commands, "/", 0, &theme, ColorDepth::Ansi256, 0).is_empty());
    }

    #[test]
    fn model_menus_keep_the_selection_visible_after_resizing_and_narrowing() {
        let theme = Theme::no_color();
        let mut picker = rune_term::picker::Picker::new(
            "models from fixture",
            (1..=15).map(|index| format!("model-{index:02}")).collect(),
            rune_term::picker::DEFAULT_WINDOW,
        );
        picker.to(14);
        for room in [16, 4, 3, 2, 1, 16] {
            let menu = picker_menu(&mut picker, &theme, ColorDepth::Ansi256, room);
            assert!(menu.len() <= room, "{menu:?}");
            assert_eq!(picker.selected(), Some("model-15"));
            assert!(menu.iter().any(|row| row == "> model-15"), "{menu:?}");
        }
        assert!(picker_menu(&mut picker, &theme, ColorDepth::Ansi256, 0).is_empty());
        picker.set_query("model-03");
        let menu = picker_menu(&mut picker, &theme, ColorDepth::Ansi256, 4);
        assert!(menu.iter().any(|row| row == "> model-03"), "{menu:?}");
        assert!(!menu.iter().any(|row| row.contains(" of ")), "{menu:?}");
        picker.set_query("missing");
        let menu = picker_menu(&mut picker, &theme, ColorDepth::Ansi256, 1);
        assert_eq!(menu.len(), 1);
        assert!(menu[0].contains("no match"), "{menu:?}");
    }

    #[test]
    fn accepting_uses_the_selection_the_window_is_showing() {
        let commands = command_candidates(&[]);
        // The position row must not be mistaken for a command.
        let theme = Theme::no_color();
        let rows = completion_rows(&commands, "/", 0, &theme, ColorDepth::Ansi256, usize::MAX);
        let chosen = open_completion(&commands, "/", &rows, 0).expect("a completion");
        assert_eq!(chosen.name, "model");
        // Every command is reachable, including the ones past the first window:
        // clamping on the drawn rows instead of the matches left the tail of the
        // list unselectable.
        let last = rune_term::commands::BUILTINS.len().saturating_sub(1);
        let rows = completion_rows(
            &commands,
            "/",
            last,
            &theme,
            ColorDepth::Ansi256,
            usize::MAX,
        );
        assert!(rows.iter().any(|r| r.starts_with('>')), "{rows:?}");
        let chosen = open_completion(&commands, "/", &rows, last).expect("a completion");
        assert_eq!(chosen.name, "quit");
    }

    #[test]
    fn the_dropdown_is_absent_unless_a_command_is_being_typed() {
        let commands = command_candidates(&[]);
        let theme = Theme::no_color();
        for line in ["", "hello", "/help me", "/model x", "a/b", "/zzz"] {
            assert!(
                completion_rows(&commands, line, 0, &theme, ColorDepth::Ansi256, usize::MAX)
                    .is_empty(),
                "{line:?} offered rows"
            );
        }
        // A whole command name is still offered, so the row does not vanish as
        // the last letter is typed.
        assert!(
            !completion_rows(
                &commands,
                "/help",
                0,
                &theme,
                ColorDepth::Ansi256,
                usize::MAX
            )
            .is_empty()
        );
    }

    #[test]
    fn the_highlighted_row_is_the_one_the_arrows_moved_to() {
        let commands = command_candidates(&[]);
        let theme = Theme::no_color();
        let first = completion_rows(
            &commands,
            "/mod",
            0,
            &theme,
            ColorDepth::Ansi256,
            usize::MAX,
        );
        assert!(first[0].starts_with('>'), "{first:?}");
        let second = completion_rows(
            &commands,
            "/mod",
            1,
            &theme,
            ColorDepth::Ansi256,
            usize::MAX,
        );
        assert!(second[1].starts_with('>'), "{second:?}");
        assert!(second[0].starts_with("  "), "{second:?}");
    }

    #[test]
    fn the_rows_line_up_their_descriptions() {
        let commands = command_candidates(&[]);
        let theme = Theme::no_color();
        let rows = completion_rows(
            &commands,
            "/mod",
            0,
            &theme,
            ColorDepth::Ansi256,
            usize::MAX,
        );
        let summaries = rune_term::commands::matching("mod");
        let columns: Vec<usize> = rows
            .iter()
            .zip(summaries.iter())
            .map(|(row, entry)| {
                row.find(entry.summary)
                    .unwrap_or_else(|| panic!("no summary in {row:?}"))
            })
            .collect();
        assert_eq!(columns.len(), rows.len(), "{rows:?}");
        for column in &columns {
            assert_eq!(*column, columns[0], "the rows are ragged: {rows:?}");
        }
    }

    #[test]
    fn accepting_the_highlighted_row_names_it() {
        let commands = command_candidates(&[]);
        // Tab completes to the highlighted command, and Enter takes it too when
        // the name is not yet whole, so a half-typed command is never run.
        let theme = Theme::no_color();
        let rows = completion_rows(
            &commands,
            "/mod",
            0,
            &theme,
            ColorDepth::Ansi256,
            usize::MAX,
        );
        let chosen = open_completion(&commands, "/mod", &rows, 0).expect("a completion");
        assert_eq!(chosen.name, "model");
        let rows = completion_rows(
            &commands,
            "/mod",
            1,
            &theme,
            ColorDepth::Ansi256,
            usize::MAX,
        );
        let chosen = open_completion(&commands, "/mod", &rows, 1).expect("a completion");
        assert_eq!(chosen.name, "models");
        // With no dropdown there is nothing to accept.
        assert!(open_completion(&commands, "hello", &[], 0).is_none());
    }

    #[test]
    fn a_colorless_theme_emits_no_escapes_in_the_dropdown() {
        let commands = command_candidates(&[]);
        let theme = Theme::no_color();
        let plain = completion_rows(
            &commands,
            "/mod",
            0,
            &theme,
            ColorDepth::Ansi256,
            usize::MAX,
        );
        assert!(!plain.is_empty(), "nothing matched");
        for row in plain {
            assert!(!row.contains('\u{1b}'), "{row:?}");
        }
        // A colored theme does style them.
        let styled_rows = completion_rows(
            &commands,
            "/mod",
            0,
            &Theme::dark(),
            ColorDepth::TrueColor,
            usize::MAX,
        );
        assert!(
            styled_rows.iter().any(|r| r.contains('\u{1b}')),
            "{styled_rows:?}"
        );
    }

    #[test]
    fn the_help_text_names_the_commands_that_exist() {
        // A command that is handled but unlisted is one nobody finds.
        let mut output = Vec::new();
        handle_command(
            "help",
            "",
            &empty_commands(),
            None,
            Utf8Path::new("/w"),
            &test_info(),
            &mut output,
        )
        .expect("handled");
        let text = String::from_utf8_lossy(&output);
        for command in [
            "/model", "/models", "/status", "/cost", "/compact", "/undo", "/copy", "/new",
            "/rename", "/tree",
        ] {
            assert!(text.contains(command), "{command} is missing from: {text}");
        }
    }

    #[test]
    fn the_quit_command_leaves_the_shell() {
        let mut output = Vec::new();
        assert_eq!(
            handle_command(
                "quit",
                "",
                &empty_commands(),
                None,
                Utf8Path::new("/w"),
                &test_info(),
                &mut output
            )
            .expect("handled"),
            Handled::Exit
        );
        assert_eq!(
            handle_command(
                "exit",
                "",
                &empty_commands(),
                None,
                Utf8Path::new("/w"),
                &test_info(),
                &mut output
            )
            .expect("handled"),
            Handled::Exit
        );
    }

    #[test]
    fn a_command_model_applies_to_one_turn() {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("address");
        let requests: Arc<Mutex<Vec<serde_json::Value>>> = Arc::default();
        let seen = Arc::clone(&requests);
        // Not joined: a session that sent fewer requests than expected leaves
        // this thread waiting, and the assertions below report it instead.
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.expect("accept");
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .expect("timeout");
                let mut reader = BufReader::new(&mut stream);
                let mut line = String::new();
                let mut length = 0;
                loop {
                    line.clear();
                    reader.read_line(&mut line).expect("header");
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().expect("length");
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).expect("request body");
                seen.lock()
                    .expect("requests")
                    .push(serde_json::from_slice::<serde_json::Value>(&body).expect("JSON"));
                let response = concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"},\"finish_reason\":\"stop\"}]}\n\n",
                    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n",
                    "data: [DONE]\n\n"
                );
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).expect("response");
            }
        });

        let temp = tempfile::tempdir().expect("root");
        let root = Utf8PathBuf::from_path_buf(temp.path().to_owned()).expect("utf8");
        let workspace = root.join("work");
        std::fs::create_dir_all(workspace.join(".claude/commands")).expect("commands");
        std::fs::write(
            workspace.join(".claude/commands/quick.md"),
            "---\ndescription: Quick answer\nmodel: claude-haiku-4-5\nallowed-tools: Bash(git *)\n---\nAnswer briefly: $ARGUMENTS\n",
        )
        .expect("command file");

        let mut config = colorless_config();
        config.accessible = true;
        config.paths = Paths {
            config_root: root.join("config"),
            state_root: root.join("state"),
            data_root: root.join("data"),
        };
        config.workspace = workspace;
        config.settings.model = "session-model".to_owned();
        config.settings.offline = true;
        config.endpoint = Endpoint::new(format!("http://{address}/v1"), "k").offline(true);

        let output: Arc<Mutex<Vec<u8>>> = Arc::default();
        let input = "/quick \"two words\"\nfirst\n/quick again\n/status\nsecond\n/quit\n";
        let code = run(
            config,
            std::io::Cursor::new(input),
            SharedBuffer(Arc::clone(&output)),
        )
        .expect("session");
        assert_eq!(code, 0);

        let text = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
        assert!(
            text.contains("the turn uses model claude-haiku-4-5"),
            "{text}"
        );
        assert!(text.contains("Answer briefly: two words"), "{text}");
        assert_eq!(
            text.matches("/quick: ignored frontmatter key allowed-tools")
                .count(),
            1,
            "{text}"
        );
        let requests = requests.lock().expect("requests");
        let models: Vec<&str> = requests
            .iter()
            .map(|body| body["model"].as_str().expect("model"))
            .collect();
        // The second command's model was dropped by the command after it.
        assert_eq!(models, ["claude-haiku-4-5", "session-model"], "{text}");
    }

    #[test]
    fn help_lists_ignored_directives_once() {
        let mut discovery = rune_context::commands::Discovery::default();
        for (name, ignored) in [
            (
                "commit",
                vec![
                    "frontmatter key allowed-tools".to_owned(),
                    "!`command` lines (2)".to_owned(),
                ],
            ),
            ("plain", Vec::new()),
        ] {
            discovery.commands.push(rune_context::commands::Command {
                name: name.to_owned(),
                description: "does a thing".to_owned(),
                argument_hint: None,
                body: "Body".to_owned(),
                model: None,
                ignored,
                origin: rune_context::commands::Origin::Project,
                source: None,
            });
        }

        let mut output = Vec::new();
        handle_command(
            "help",
            "",
            &discovery,
            None,
            Utf8Path::new("/w"),
            &test_info(),
            &mut output,
        )
        .expect("handled");
        let text = String::from_utf8_lossy(&output);
        let lines: Vec<&str> = text
            .lines()
            .filter(|line| line.contains("ignored"))
            .collect();
        assert_eq!(
            lines,
            ["/commit: ignored frontmatter key allowed-tools, !`command` lines (2)"],
            "{text}"
        );
        let listing = text.find("/plain").expect("listing");
        let notice = text.find("/commit: ignored").expect("notice");
        assert!(listing < notice, "the notice follows the listing: {text}");
    }

    #[test]
    fn a_command_with_an_unterminated_quote_expands_nothing() {
        let mut discovery = rune_context::commands::Discovery::default();
        discovery.commands.push(rune_context::commands::Command {
            name: "fix".to_owned(),
            description: "Fix it".to_owned(),
            argument_hint: None,
            body: "Fix $ARGUMENTS".to_owned(),
            model: Some("claude-haiku-4-5".to_owned()),
            ignored: Vec::new(),
            origin: rune_context::commands::Origin::Project,
            source: None,
        });

        let mut output = Vec::new();
        let handled = handle_command(
            "fix",
            "\"open",
            &discovery,
            None,
            Utf8Path::new("/w"),
            &test_info(),
            &mut output,
        )
        .expect("handled");
        assert_eq!(handled, Handled::Continue);
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("unterminated quote"), "{text}");
        assert!(text.contains("hint: close the quote"), "{text}");

        let quoted = handle_command(
            "fix",
            "\"two words\" three",
            &discovery,
            None,
            Utf8Path::new("/w"),
            &test_info(),
            &mut Vec::new(),
        )
        .expect("handled");
        assert_eq!(
            quoted,
            Handled::Expand {
                text: "Fix two words three".to_owned(),
                model: Some("claude-haiku-4-5".to_owned()),
            }
        );
    }

    #[test]
    fn a_user_command_expands_for_review_instead_of_running() {
        let mut discovery = rune_context::commands::Discovery::default();
        discovery.commands.push(rune_context::commands::Command {
            name: "fix".to_owned(),
            description: "Fix it".to_owned(),
            argument_hint: None,
            body: "Fix $1 now".to_owned(),
            model: None,
            ignored: Vec::new(),
            origin: rune_context::commands::Origin::Project,
            source: None,
        });

        let mut output = Vec::new();
        let handled = handle_command(
            "fix",
            "parser",
            &discovery,
            None,
            Utf8Path::new("/w"),
            &test_info(),
            &mut output,
        )
        .expect("handled");
        assert_eq!(
            handled,
            Handled::Expand {
                text: "Fix parser now".to_owned(),
                model: None,
            }
        );
    }

    #[test]
    fn a_command_missing_its_argument_reports_instead_of_expanding() {
        let mut discovery = rune_context::commands::Discovery::default();
        discovery.commands.push(rune_context::commands::Command {
            name: "fix".to_owned(),
            description: "Fix it".to_owned(),
            argument_hint: None,
            body: "Fix $1 now".to_owned(),
            model: None,
            ignored: Vec::new(),
            origin: rune_context::commands::Origin::Project,
            source: None,
        });

        let mut output = Vec::new();
        let handled = handle_command(
            "fix",
            "",
            &discovery,
            None,
            Utf8Path::new("/w"),
            &test_info(),
            &mut output,
        )
        .expect("handled");
        assert_eq!(handled, Handled::Continue);
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("$1"), "{text}");
    }

    #[test]
    fn help_reports_a_command_file_that_was_skipped() {
        let mut discovery = rune_context::commands::Discovery::default();
        discovery.warnings.push(rune_context::commands::Warning {
            path: Utf8PathBuf::from("/p/.rune/commands/help.md"),
            reason: "`/help` is a built-in command, so this file was skipped".to_owned(),
        });

        let mut output = Vec::new();
        handle_command(
            "help",
            "",
            &discovery,
            None,
            Utf8Path::new("/w"),
            &test_info(),
            &mut output,
        )
        .expect("handled");
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("skipped"), "{text}");
        assert!(text.contains("built-in"), "{text}");
    }

    #[test]
    fn help_lists_the_commands() {
        let mut output = Vec::new();
        handle_command(
            "help",
            "",
            &empty_commands(),
            None,
            Utf8Path::new("/w"),
            &test_info(),
            &mut output,
        )
        .expect("handled");
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("/help"));
        assert!(text.contains("/quit"));
    }

    #[test]
    fn a_session_driven_by_a_script_runs_every_line() {
        let mut source = ScriptedSource::new(["first", "second", "/quit", "never"]);
        let mut seen = 0_usize;
        {
            let mut shell = Shell::new(&mut source);
            let reason = shell
                .run(|input| {
                    seen = seen.saturating_add(1);
                    match input {
                        Input::Command { name, .. } if name == "quit" => Ok(Action::Exit),
                        _ => Ok(Action::Continue),
                    }
                })
                .expect("ran");
            assert_eq!(reason, ExitReason::Requested);
        }
        // The line after the exit was never read.
        assert_eq!(seen, 3);
    }

    #[test]
    fn a_reported_turn_yields_its_text_as_lines() {
        let host = test_host();
        let outcome = turn::TurnOutcome {
            error: None,
            judge: None,
            stop_reason: StopReason::Completed,
            text: "the answer".to_owned(),
            narration: Vec::new(),
            reasoning: String::new(),
            usage: rune_net::stream::Usage::default(),
            last_request: rune_net::stream::Usage::default(),
            steps: 1,
            verification: turn::Verification::NotApplicable,
            checks: rune_agent::verify::CheckSummary::default(),
            calls: Vec::new(),
            repairs: Vec::new(),
            repaired_calls: 0,
        };
        let lines = report_turn(&outcome, &host, "").expect("reported");
        assert!(
            lines.iter().any(|line| line.contains("the answer")),
            "{lines:?}"
        );
    }

    #[test]
    fn reporting_a_step_limit_names_it() {
        let host = test_host();
        let outcome = turn::TurnOutcome {
            error: None,
            judge: None,
            stop_reason: StopReason::StepLimit,
            text: String::new(),
            narration: Vec::new(),
            reasoning: String::new(),
            usage: rune_net::stream::Usage::default(),
            last_request: rune_net::stream::Usage::default(),
            steps: 40,
            verification: turn::Verification::NotApplicable,
            checks: rune_agent::verify::CheckSummary::default(),
            calls: Vec::new(),
            repairs: Vec::new(),
            repaired_calls: 0,
        };
        let lines = report_turn(&outcome, &host, "").expect("reported");
        assert!(
            lines.iter().any(|line| line.contains("step limit")),
            "{lines:?}"
        );
    }

    #[test]
    fn a_finished_write_shows_the_preview_the_tool_sent() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let host = test_host();
        *host.live_out.lock().expect("lock") =
            Some(Arc::new(Mutex::new(SharedSink(Arc::clone(&bytes)))));
        let mut output = ToolOutput::success("wrote notes.txt (+2 -1, lines 1-2)");
        output.diff = Some("- before\n+ after\n+ more\n...".to_owned());
        host.emit(Event::ToolFinished {
            call: turn::PreparedCall {
                id: "c1".to_owned(),
                name: "write_file".to_owned(),
                arguments: r#"{"path":"notes.txt","content":"after\nmore"}"#.to_owned(),
            },
            output,
            elapsed: std::time::Duration::from_millis(2),
        });
        let shown = String::from_utf8_lossy(&bytes.lock().expect("lock")).into_owned();
        assert!(shown.contains("- before"), "{shown}");
        assert!(shown.contains("+ more"), "{shown}");
        assert!(shown.contains('\u{2026}'), "{shown}");
    }

    #[test]
    fn a_finished_edit_shows_the_lines_it_changed_under_its_row() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let host = test_host();
        *host.live_out.lock().expect("lock") =
            Some(Arc::new(Mutex::new(SharedSink(Arc::clone(&bytes)))));
        host.emit(Event::ToolFinished {
            call: turn::PreparedCall {
                id: "c1".to_owned(),
                name: "edit_file".to_owned(),
                arguments: serde_json::json!({
                    "path": "src/lib.rs",
                    "old_string": "keep\nold line\nkeep too",
                    "new_string": "keep\nnew line\nkeep too",
                })
                .to_string(),
            },
            output: ToolOutput::success("edited src/lib.rs (1 replacement, +1 -1, line 2)"),
            elapsed: std::time::Duration::from_millis(3),
        });
        let shown = String::from_utf8_lossy(&bytes.lock().expect("lock")).into_owned();
        assert!(shown.contains("- old line"), "{shown}");
        assert!(shown.contains("+ new line"), "{shown}");
        assert!(!shown.contains("keep too"), "{shown}");
    }

    #[test]
    fn reporting_a_denied_call_names_the_tool() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let host = test_host();
        *host.live_out.lock().expect("lock") =
            Some(Arc::new(Mutex::new(SharedSink(Arc::clone(&bytes)))));
        host.emit(Event::ToolDenied {
            call: turn::PreparedCall {
                id: "c1".to_owned(),
                name: "shell".to_owned(),
                arguments: "{}".to_owned(),
            },
            reason: "denied by a rule".to_owned(),
        });
        let outcome = turn::TurnOutcome {
            error: None,
            judge: None,
            stop_reason: StopReason::Completed,
            text: String::new(),
            narration: Vec::new(),
            reasoning: String::new(),
            usage: rune_net::stream::Usage::default(),
            last_request: rune_net::stream::Usage::default(),
            steps: 1,
            verification: turn::Verification::NotApplicable,
            checks: rune_agent::verify::CheckSummary::default(),
            calls: Vec::new(),
            repairs: Vec::new(),
            repaired_calls: 0,
        };
        // The refusal reaches scrollback when it happens, and the end of the
        // turn does not print it a second time.
        let shown = String::from_utf8_lossy(&bytes.lock().expect("lock")).into_owned();
        assert!(shown.contains("refused shell"), "{shown}");
        let lines = report_turn(&outcome, &host, "").expect("reported");
        assert!(!lines.join("\n").contains("refused shell"), "{lines:?}");
    }

    #[test]
    fn narration_and_finished_calls_reach_scrollback_in_order() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let host = test_host();
        *host.live_out.lock().expect("lock") =
            Some(Arc::new(Mutex::new(SharedSink(Arc::clone(&bytes)))));
        let call = turn::PreparedCall {
            id: "c1".to_owned(),
            name: "read_file".to_owned(),
            arguments: r#"{"path":"src/session.js"}"#.to_owned(),
        };
        host.emit(Event::TextDelta {
            delta: "Inspecting the module.".to_owned(),
        });
        host.emit(Event::ToolStarted {
            call: call.clone(),
            activity: rune_tools::Activity::Read,
        });
        assert_eq!(host.running.lock().expect("running").len(), 1);
        host.emit(Event::ToolFinished {
            call,
            output: ToolOutput::success("     1\ta\n     2\tb\n"),
            elapsed: std::time::Duration::from_millis(5),
        });
        assert!(host.running.lock().expect("running").is_empty());
        assert!(host.partial_answer().is_empty(), "narration was moved out");

        let shown = String::from_utf8_lossy(&bytes.lock().expect("lock")).into_owned();
        let narration = shown.find("Inspecting the module.").expect("narration");
        let row = shown.find("read_file src/session.js").expect("tool row");
        assert!(narration < row, "{shown}");
        assert!(shown.contains("2 lines"), "{shown}");
    }

    #[test]
    fn network_command_shows_and_switches_shell_network() {
        let host = test_host();
        assert_eq!(set_network(&host, None), "shell network: off");
        assert!(!host.context().network);
        assert_eq!(
            set_network(&host, Some("on")),
            "shell network: on for this session"
        );
        assert!(host.context().network, "the next call's context has it");
        assert_eq!(set_network(&host, None), "shell network: on");
        assert_eq!(
            set_network(&host, Some("off")),
            "shell network: off for this session"
        );
        assert_eq!(set_network(&host, Some("bogus")), "usage: /network on|off");
        assert!(!host.context().network);
    }

    #[test]
    fn a_spent_limit_refuses_the_next_request_until_it_is_raised() {
        let host = test_host();
        assert_eq!(
            set_budget(&host, Some("0.05")),
            "spend limit set to $0.050000; nothing priced yet"
        );
        assert!(host.check_spend().is_ok());
        host.totals.lock().expect("totals").cost = Some(60_000);
        let err = host.check_spend().expect_err("over the limit");
        assert_eq!(err.code(), ErrorCode::LimitExceeded);
        assert!(err.message().contains("$0.060000"), "{}", err.message());
        assert!(set_budget(&host, Some("1")).starts_with("spend limit set to $1.000000"));
        assert!(host.check_spend().is_ok());
        assert!(set_budget(&host, Some("lots")).contains("is not an amount"));
        assert_eq!(
            set_budget(&host, Some("off")),
            "spend limit removed; $0.060000 spent"
        );
    }

    #[test]
    fn plan_mode_refuses_changes_whatever_the_permission_mode() {
        let mut host = test_host();
        *host.mode.get_mut().expect("mode") = PermissionMode::FullAccess;
        host.registry
            .insert(Box::new(rune_tools::WriteFile))
            .expect("write tool");
        host.registry
            .insert(Box::new(rune_tools::UpdatePlan))
            .expect("plan tool");
        host.planning
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let (write, reason) = Host::decide(&host, "write_file", Some("a.txt"));
        assert_eq!(write, Outcome::Deny);
        assert!(reason.contains("plan mode"), "{reason}");
        assert_eq!(Host::decide(&host, "shell", Some("ls")).0, Outcome::Deny);
        assert_eq!(
            Host::decide(&host, "read_file", Some("a.txt")).0,
            Outcome::Allow
        );
        assert_eq!(Host::decide(&host, "update_plan", None).0, Outcome::Allow);
        host.planning
            .store(false, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            Host::decide(&host, "write_file", Some("a.txt")).0,
            Outcome::Allow
        );
    }

    #[test]
    fn permissions_switch_the_mode_and_rules_and_forget_grants_when_narrowed() {
        let host = test_host();
        assert!(set_permissions(&host, None).starts_with("permissions are auto"));
        // Auto allows a routine test command; ask leaves it to the mode default.
        let (before, _) = host.decide("shell", Some("cargo test"));
        assert_eq!(before, Outcome::Allow);
        ApprovalOutcome::RememberForSession.apply(
            &mut host.grants.lock().expect("grants"),
            &rune_policy::approval::Scope::new("shell", "npm run e2e"),
        );
        assert_eq!(
            set_permissions(&host, Some("ask")),
            "permissions set to ask for the calls that follow"
        );
        assert_eq!(host.mode(), PermissionMode::Ask);
        let (after, _) = turn::decide_call_in_workspace(
            &lock_rules(&host.rules),
            host.mode(),
            "shell",
            Some("cargo test"),
            host.context.workspace(),
        );
        assert_ne!(after, Outcome::Allow);
        assert!(
            !host
                .grants
                .lock()
                .expect("grants")
                .allows("shell", "npm run e2e")
        );
        assert!(set_permissions(&host, Some("sideways")).contains("not a permission mode"));
        assert_eq!(host.mode(), PermissionMode::Ask);
        set_permissions(&host, Some("yolo"));
        assert_eq!(host.mode(), PermissionMode::FullAccess);
    }

    #[test]
    fn effort_lists_the_models_levels() {
        let host = test_host();
        "claude-sonnet-4-6".clone_into(&mut lock_model(&host.model));
        assert_eq!(
            set_effort(&host, None),
            "effort is auto; claude-sonnet-4-6 accepts auto, none, minimal, low, medium, high, max"
        );
        assert_eq!(
            set_effort(&host, Some("xhigh")),
            "effort set to max for the requests that follow (xhigh is not accepted by claude-sonnet-4-6)"
        );
        assert_eq!(Host::effort(&host), Effort::Max);
        assert!(
            host.effort_noticed
                .lock()
                .expect("noticed")
                .contains(&("claude-sonnet-4-6".to_owned(), "xhigh"))
        );
    }

    #[test]
    fn effort_is_shown_and_set_only_to_a_level_the_dialect_accepts() {
        let host = test_host();
        assert!(set_effort(&host, None).starts_with("effort is auto"));
        // The test host speaks Chat Completions, which accepts the named levels.
        assert_eq!(
            set_effort(&host, Some("high")),
            "effort set to high for the requests that follow"
        );
        assert_eq!(Host::effort(&host), Effort::High);
        assert!(set_effort(&host, Some("extreme")).contains("is not an effort"));
        assert_eq!(Host::effort(&host), Effort::High);
    }

    #[test]
    fn only_actions_that_can_be_undone_may_be_remembered() {
        assert!(rememberable("shell", "npm run e2e"));
        assert!(rememberable("write_file", "../outside/notes.txt"));
        assert!(!rememberable("shell", "rm -rf build"));
        assert!(!rememberable("shell", "git push --force"));
        assert!(!rememberable("web_fetch_private", "http://10.0.0.1"));
    }

    #[test]
    fn an_edit_awaiting_approval_shows_the_lines_it_would_change() {
        let mut host = test_host();
        host.rules
            .get_mut()
            .expect("rules")
            .push(rune_policy::rules::Rule::ask(
                "edit_file",
                "*",
                rune_policy::decision::Layer::Session,
            ));
        let (sender, requests) = mpsc::channel::<ApprovalRequest>();
        *host.approval_requests.lock().expect("slot") = Some(sender);
        let answerer = std::thread::spawn(move || {
            let request = requests
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("the edit asks");
            let lines = approval_lines(&request, 80).join("\n");
            request.answer.send(ApprovalOutcome::Deny).expect("answer");
            lines
        });
        let arguments = serde_json::json!({
            "path": "src/lib.rs",
            "edits": [{"old_string": "a\nold\nb", "new_string": "a\nnew\nb"}],
        });
        let (outcome, _) =
            Host::decide_with_arguments(&host, "edit_file", Some("src/lib.rs"), &arguments);
        assert_eq!(outcome, Outcome::Deny);
        let lines = answerer.join().expect("answered");
        assert!(lines.contains("    - old"), "{lines}");
        assert!(lines.contains("    + new"), "{lines}");
        assert!(host.approval_preview.lock().expect("slot").is_empty());
    }

    #[test]
    fn a_remembered_approval_covers_the_same_action_and_nothing_else() {
        let host = test_host();
        let (sender, requests) = mpsc::channel::<ApprovalRequest>();
        *host.approval_requests.lock().expect("slot") = Some(sender);
        let answerer = std::thread::spawn(move || {
            let request = requests.recv().expect("one request");
            assert!(request.rememberable);
            request
                .answer
                .send(ApprovalOutcome::RememberForSession)
                .expect("answer");
        });
        let (outcome, _) = host.request_approval("shell", "npm run e2e", "asked".to_owned());
        assert_eq!(outcome, Outcome::Allow);
        answerer.join().expect("answered");

        // With no way left to ask, only the grant can allow anything.
        *host.approval_requests.lock().expect("slot") = None;
        let (again, reason) = host.request_approval("shell", "npm run e2e", "asked".to_owned());
        assert_eq!(again, Outcome::Allow);
        assert!(
            reason.contains("approved earlier in this session"),
            "{reason}"
        );
        let (other, _) = host.request_approval("shell", "npm run e2e -- --all", "asked".to_owned());
        assert_eq!(other, Outcome::Ask);
    }

    #[test]
    fn a_test_run_row_shows_its_exit_status_and_counts() {
        let output = ToolOutput::success(
            "$ npm test\n[tests: 3 passed, 1 failed]\n...\n[exited with status 1]\n",
        );
        assert_eq!(
            result_summary("shell", &output, false),
            "exit 1 \u{b7} 3 passed, 1 failed"
        );
    }

    #[test]
    fn an_edit_row_shows_the_line_counts() {
        let edited = ToolOutput::success("Edited `/w/a.rs` (1 replacement, +3 -1, lines 4-6)");
        assert_eq!(result_summary("edit_file", &edited, false), "+3 -1");
        let created = ToolOutput::success("Wrote `/w/b.rs` (new file, 12 lines, lines 1-12)");
        assert_eq!(
            result_summary("write_file", &created, false),
            "new file, 12 lines"
        );
    }

    #[test]
    fn a_patch_row_shows_its_first_line() {
        let patched = ToolOutput::success("Updated 2 files:\nM a.rs (+1 -1)\nA b.rs (+3)");
        assert_eq!(
            result_summary("apply_patch", &patched, false),
            "Updated 2 files"
        );
    }

    #[test]
    fn a_turn_summary_counts_every_file_a_patch_changed() {
        let patch = turn::CallResult {
            call: turn::PreparedCall {
                id: "p".to_owned(),
                name: "apply_patch".to_owned(),
                arguments: serde_json::json!({
                    "input": "*** Begin Patch\n*** Add File: a.rs\n+x\n*** Delete File: b.rs\n*** End Patch"
                })
                .to_string(),
            },
            output: ToolOutput::success("Updated 2 files:\nA a.rs (+1)\nD b.rs"),
            executed: true,
        };
        let outcome = turn::TurnOutcome {
            error: None,
            judge: None,
            stop_reason: StopReason::Completed,
            text: String::new(),
            narration: Vec::new(),
            reasoning: String::new(),
            usage: rune_net::stream::Usage::default(),
            last_request: rune_net::stream::Usage::default(),
            steps: 1,
            verification: turn::Verification::NotApplicable,
            checks: rune_agent::verify::CheckSummary::default(),
            calls: vec![patch],
            repairs: Vec::new(),
            repaired_calls: 0,
        };
        let summary = turn_summary(&outcome, None, None, None, true).expect("summary");
        assert!(summary.starts_with("1 call, 2 files changed"), "{summary}");
    }

    #[test]
    fn a_turn_summary_shows_plan_progress() {
        let outcome = turn::TurnOutcome {
            error: None,
            judge: None,
            stop_reason: StopReason::Completed,
            text: String::new(),
            narration: Vec::new(),
            reasoning: String::new(),
            usage: rune_net::stream::Usage {
                output_tokens: Some(10),
                ..rune_net::stream::Usage::default()
            },
            last_request: rune_net::stream::Usage::default(),
            steps: 1,
            verification: turn::Verification::NotApplicable,
            checks: rune_agent::verify::CheckSummary::default(),
            calls: Vec::new(),
            repairs: Vec::new(),
            repaired_calls: 0,
        };
        assert_eq!(
            turn_summary(&outcome, Some((1, 3)), None, None, true).expect("summary"),
            "plan 1/3, 10 out"
        );
        assert_eq!(plan_progress(&History::new()), None);
    }

    #[test]
    fn a_turn_summary_names_calls_changes_tokens_and_time() {
        let edit = turn::CallResult {
            call: turn::PreparedCall {
                id: "e".to_owned(),
                name: "edit_file".to_owned(),
                arguments: r#"{"path":"a.rs"}"#.to_owned(),
            },
            output: ToolOutput::success("Edited `a.rs` (1 replacement)"),
            executed: true,
        };
        let outcome = turn::TurnOutcome {
            error: None,
            judge: None,
            stop_reason: StopReason::Completed,
            text: String::new(),
            narration: Vec::new(),
            reasoning: String::new(),
            usage: rune_net::stream::Usage {
                input_tokens: Some(12_000),
                output_tokens: Some(640),
                cache_read_tokens: Some(9_000),
                ..rune_net::stream::Usage::default()
            },
            last_request: rune_net::stream::Usage::default(),
            steps: 2,
            verification: turn::Verification::NotApplicable,
            checks: rune_agent::verify::CheckSummary::default(),
            calls: vec![edit.clone(), edit],
            repairs: Vec::new(),
            repaired_calls: 0,
        };
        let summary = turn_summary(
            &outcome,
            None,
            Some(std::time::Duration::from_millis(9_400)),
            Some(1_523),
            true,
        )
        .expect("summary");
        assert_eq!(
            summary,
            "2 calls, 1 file changed, 12.0k in (75% cached), 640 out, $0.001523, 9.4s"
        );
        for (verification, word) in [
            (turn::Verification::Verified, "verified"),
            (turn::Verification::Checked, "checked"),
            (turn::Verification::Failed, "check failed"),
            (turn::Verification::Unverified, "unverified"),
        ] {
            let judged = turn::TurnOutcome {
                verification,
                ..outcome.clone()
            };
            let summary = turn_summary(&judged, None, None, None, true).expect("summary");
            assert!(
                summary.starts_with(&format!("2 calls, 1 file changed, {word}, ")),
                "{summary}"
            );
        }
    }

    #[test]
    fn preparing_a_session_without_a_provider_fails_before_the_terminal() {
        let settings = Settings::default();
        let paths = Paths::resolve(Some("/tmp"), None, None, None, Some("/tmp/s"));
        let outcome = prepare(&settings, &paths, Utf8Path::new("/tmp"), None);
        let Err(err) = outcome else {
            panic!("a session started with no provider configured");
        };
        assert_eq!(err.code(), ErrorCode::AuthenticationRequired);
        assert!(
            err.hint().is_some(),
            "the failure does not say how to connect a provider"
        );
    }

    #[test]
    fn an_anthropic_session_presents_its_key_the_way_anthropic_reads_it() {
        // Anthropic refuses a bearer token, so the session has to present its
        // key the way the one-shot runner does.
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = Paths::resolve(Some(root.as_str()), None, None, None, None);
        paths.ensure_roots().expect("roots");
        crate::provider_setup::connect(&paths, "anthropic", "sk-ant-test").expect("credential");

        let settings = Settings {
            provider: rune_core::config::Provider::Anthropic,
            base_url: Some("https://example.invalid".to_owned()),
            model: "claude-test".to_owned(),
            ..Settings::default()
        };
        let config =
            prepare(&settings, &paths, Utf8Path::new("/tmp"), None).expect("a session prepares");
        assert_eq!(
            config.endpoint.auth,
            rune_net::transport::AuthStyle::ApiKeyHeader
        );
    }

    #[test]
    fn a_session_runs_with_the_rules_that_ship_with_it() {
        // A session built with an empty rule set resolves every action to the
        // mode default, so in automatic mode nothing is ever allowed and each
        // call comes back as an uncollected approval. The rules a session runs
        // with must be the ones a fresh install describes.
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = Paths::resolve(Some(root.as_str()), None, None, None, None);
        paths.ensure_roots().expect("roots");

        let provider = rune_core::config::parse_provider("chat_completions");
        crate::provider_setup::connect(&paths, provider.as_str(), "sk-test").expect("credential");

        let settings = Settings {
            provider,
            base_url: Some("https://example.invalid/v1".to_owned()),
            model: "test-model".to_owned(),
            ..Settings::default()
        };

        let config =
            prepare(&settings, &paths, Utf8Path::new("/tmp"), None).expect("a session prepares");

        assert!(
            !config.rules.rules().is_empty(),
            "the session runs without any rules, so every action falls to the mode default"
        );
        assert_eq!(
            config
                .rules
                .evaluate("glob_files", "*.md", Outcome::Ask)
                .outcome,
            Outcome::Allow,
            "a read tool would ask for approval it never gets"
        );
        // A fresh session refuses web calls until the user enables them.
        assert_eq!(
            config
                .rules
                .evaluate("web_fetch", "https://example.com", Outcome::Allow)
                .outcome,
            Outcome::Deny,
            "the web tools were allowed without an explicit opt-in"
        );
        // Explicitly enabling web installs the allow above the built-in denial.
        let on = Settings {
            web_tools: true,
            ..settings.clone()
        };
        let on_config =
            prepare(&on, &paths, Utf8Path::new("/tmp"), None).expect("a session prepares");
        assert_eq!(
            on_config
                .rules
                .evaluate("web_fetch", "https://example.com", Outcome::Deny)
                .outcome,
            Outcome::Allow,
            "explicitly enabling the web tools did not allow them"
        );
    }

    /// Builds an empty command set.
    fn empty_commands() -> rune_context::commands::Discovery {
        rune_context::commands::Discovery::default()
    }

    /// Builds a session config whose terminal accepts no color.
    fn colorless_config() -> SessionConfig {
        SessionConfig {
            helper_usage: crate::helper_usage::HelperUsage::default(),
            accessible: false,
            settings: Settings::default(),
            paths: Paths::resolve(Some("/tmp"), None, None, None, Some("/tmp/s")),
            resume: None,
            workspace: Utf8Path::new("/tmp").to_owned(),
            endpoint: Endpoint::new("https://example.invalid", "k"),
            dialect: Box::new(rune_net::chat_completions::ChatCompletions),
            registry: Registry::new(),
            subagent: None,
            catalog: Arc::new(rune_tools::skill::Empty),
            rules: RuleSet::new(),
            questions: Arc::new(TerminalQuestions::default()),
        }
    }

    /// Builds a host with no network, for rendering tests.
    fn test_info() -> SessionInfo<'static> {
        test_info_with(Totals::default())
    }

    /// Builds the same report with a chosen spend, for the cost assertions.
    fn test_info_with(totals: Totals) -> SessionInfo<'static> {
        SessionInfo {
            model: "test".to_owned(),
            provider: "chat_completions",
            endpoint: "https://example.invalid/v1",
            mode: PermissionMode::Auto,
            effort: Effort::Auto,
            session_id: "sessiontest1".to_owned(),
            workspace: "/w",
            terminal: rune_term::probe::TerminalCaps::default().summary(),
            depth: ColorDepth::Ansi256,
            context_used: 0,
            context_source: None,
            context_limit: rune_net::catalog::DEFAULT_CONTEXT_WINDOW,
            totals,
            spend_limit: None,
        }
    }

    #[test]
    fn ascii_session_covers_status_menus_streaming_and_transcript() {
        let commands = command_candidates(&[]);
        let mut host = test_host();
        let mut settings = Settings::default();
        let launch =
            crate::cli::parse(vec![std::ffi::OsString::from("--ascii")], false).expect("parse");
        crate::cli::apply_to_settings(&launch, &mut settings);
        host.ascii = settings.ascii;
        host.width.store(12, std::sync::atomic::Ordering::Relaxed);
        let text = "```rust\n    abcdefghijklmnop\n```";
        host.streaming.lock().expect("streaming").answer = text.to_owned();
        assert!(host.status_line(100).is_ascii());
        for theme in [Theme::no_color(), Theme::dark(), Theme::high_contrast()] {
            for depth in ColorDepth::ALL {
                host.theme = Mutex::new(theme.clone());
                host.depth = depth;
                assert!(host.status_line(100).is_ascii());
                let rows = completion_rows(&commands, "/", 0, &theme, depth, 8);
                assert!(rows.iter().all(|row| row.is_ascii()));
                let paths = crate::path_completion::Paths {
                    // The values are content, while the selection is a decoration.
                    range: 0..0,
                    matches: vec!["one".to_owned(), "two".to_owned()],
                };
                let rows = path_completion_rows(&paths, 0, &theme, depth, 8);
                assert!(rows.iter().all(|row| row.is_ascii()));
            }
        }
        host.theme = Mutex::new(Theme::no_color());
        let streaming = host.streaming_rows();
        let entries = [Entry::assistant(text), Entry::tool("read_file\none\ntwo")];
        let settled = render_entries(&entries, &host);
        assert_eq!(streaming, render_entries(&entries[..1], &host));
        assert!(settled.iter().all(|row| row.is_ascii()));
        assert!(settled.iter().any(|row| row.starts_with("    > ")));
        assert!(
            full_transcript_rows(&entries, 12, host.ascii)
                .iter()
                .all(|row| row.is_ascii())
        );
        assert_eq!(
            history_preview("> first\nsecond\tline", host.ascii),
            "> first / second line"
        );
        assert_eq!(
            history_preview("> 界↵\nsecond", host.ascii),
            "> 界↵ / second"
        );
    }

    #[test]
    fn a_custom_read_tool_keeps_its_activity_in_transcript_summaries() {
        use rune_tools::{Activity, Tool};

        struct CustomRead;

        impl Tool for CustomRead {
            fn name(&self) -> &'static str {
                "custom_read"
            }

            fn description(&self) -> &'static str {
                "Read fixture content."
            }

            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }

            fn activity(&self) -> Activity {
                Activity::Read
            }

            fn call(
                &self,
                _arguments: &serde_json::Value,
                _context: &ExecutionContext,
            ) -> Result<ToolOutput> {
                Ok(ToolOutput::success("fixture content"))
            }
        }

        let mut host = test_host();
        host.registry
            .insert(Box::new(CustomRead))
            .expect("register");
        let activity = host.tool_activity("custom_read").expect("activity");
        assert_eq!(activity, Activity::Read);
        host.emit(Event::ToolStarted {
            call: turn::PreparedCall {
                id: "call_fixture".to_owned(),
                name: "custom_read".to_owned(),
                arguments: "{}".to_owned(),
            },
            activity,
        });

        // A running call is drawn with its own activity, not the generic one.
        let running = host.running.lock().expect("running").clone();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].label, "Reading custom_read");
        let rows = host.live_rows();
        assert!(rows[0].contains("Reading custom_read"), "{rows:?}");
        assert!(
            rows[0].contains("ms"),
            "the row shows the time so far: {rows:?}"
        );
    }

    #[test]
    fn a_quiet_turn_shows_that_the_model_is_working() {
        let host = test_host();
        assert!(host.live_rows().is_empty(), "no turn is running");
        host.emit(Event::TurnStarted { step: 1 });
        let rows = host.live_rows();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].contains("thinking"), "{rows:?}");
        host.emit(Event::TextDelta {
            delta: "Answering.".to_owned(),
        });
        assert!(host.live_rows().is_empty(), "streamed text replaces it");
    }

    fn test_host() -> SessionHost {
        let mut registry = Registry::new();
        registry
            .insert(Box::new(rune_tools::ReadFile::new()))
            .expect("registered");
        let questions = Arc::new(TerminalQuestions::default());
        SessionHost {
            cache_key: std::sync::OnceLock::new(),
            usage_paths: None,
            pre_tool_hook: None,
            post_tool_hook: None,
            accessible: false,
            ascii: false,
            recorder: None,
            endpoint: Endpoint::new("https://example.invalid", "k"),
            compat: crate::provider_setup::CompatSource::default(),
            dialect: Box::new(rune_net::chat_completions::ChatCompletions),
            model: Mutex::new("test".to_owned()),
            instructions: Mutex::new(String::new()),
            mapped: prompt::MappedInstructions::default(),
            stable_instructions_len: std::sync::atomic::AtomicUsize::new(0),
            frozen_objective: Mutex::new(FrozenObjective::Unread),
            pending_updates: Mutex::new(Vec::new()),
            watched: Mutex::new(context_watch::Watched::default()),
            nested_rules: Mutex::new(Vec::new()),
            instruction_sizes: Mutex::new(rune_agent::tokens::InstructionSizes::default()),
            tools: Vec::new(),
            rules: Mutex::new(crate::permissions::validated(&Settings::default()).expect("rules")),
            mode: Mutex::new(PermissionMode::Auto),
            permission_settings: Settings::default(),
            effort: Mutex::new(Effort::Auto),
            effort_set: std::sync::atomic::AtomicBool::new(false),
            effort_noticed: Mutex::new(std::collections::BTreeSet::new()),
            fast_mode: false,
            limits: BudgetSet::new(),
            context: ExecutionContext::new(Utf8PathBuf::from("/tmp")),
            registry,
            mcp: None,
            subagent: None,
            cancellation: questions.cancellation.clone(),
            steering: SteeringQueue::new(4),
            events: Arc::new(Mutex::new(Vec::new())),
            goal_judge: std::sync::atomic::AtomicBool::new(false),
            provider_retry: Mutex::new(None),
            context_used: std::sync::atomic::AtomicU64::new(0),
            prices: Mutex::new(None),
            vision: std::sync::atomic::AtomicBool::new(false),
            tool_images: crate::attachments::ToolImages::default(),
            spend_limit: Mutex::new(None),
            network: std::sync::atomic::AtomicBool::new(false),
            output_limit: std::sync::atomic::AtomicU64::new(0),
            context_source: Mutex::new(None),
            context_limit: std::sync::atomic::AtomicU64::new(
                rune_net::catalog::DEFAULT_CONTEXT_WINDOW,
            ),
            theme: Mutex::new(Theme::no_color()),
            theme_name: Mutex::new("auto".to_owned()),
            session_id: Mutex::new("sessiontest1".to_owned()),
            workspace: "/tmp".to_owned(),
            caps: rune_term::probe::TerminalCaps::default(),
            depth: ColorDepth::Ansi256,
            width: std::sync::atomic::AtomicU16::new(80),
            height: std::sync::atomic::AtomicU16::new(24),
            pinned_size: Some((80, 24)),
            inline: Mutex::new(rune_term::inline::Inline::new(80)),
            frame: Mutex::new(()),
            transcript: Mutex::new(Vec::new()),
            transcript_history_len: std::sync::atomic::AtomicUsize::new(0),
            transcript_open: std::sync::atomic::AtomicBool::new(false),
            typed: Mutex::new((String::new(), 0)),
            provider_order: Vec::new(),
            provider_strict: false,
            streaming: Arc::new(Mutex::new(StreamingText::default())),
            running: Mutex::new(Vec::new()),
            last_tick: Mutex::new(None),
            frame_dirty: std::sync::atomic::AtomicBool::new(false),
            last_frame: Mutex::new(None),
            turn_started: Mutex::new(None),
            live_out: Arc::new(Mutex::new(None)),
            reasoning: Mutex::new("\u{1b}[2m".to_owned()),
            reviewer: None,
            helper_usage: crate::helper_usage::HelperUsage::default(),
            totals: Mutex::new(Totals::default()),
            undo: Mutex::new(BTreeMap::new()),
            review_session: Arc::new(Mutex::new(ReviewSession::new(&BudgetSet::new()))),
            approval_requests: Mutex::new(None),
            approval_preview: Mutex::new(Vec::new()),
            grants: Mutex::new(Grants::new()),
            planning: std::sync::atomic::AtomicBool::new(false),
            unsandboxed: false,
            questions,
            last_plan: Mutex::new(None),
            warming: Arc::default(),
            warm_interval: std::time::Duration::from_millis(50),
            pending_model: Mutex::new(None),
            ignored_reported: Mutex::new(std::collections::BTreeSet::new()),
        }
    }
}

#[cfg(test)]
mod send_probe {
    use super::SessionHost;
    #[test]
    fn session_host_can_cross_a_thread() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SessionHost>();
    }
}
