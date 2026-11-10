//! Synchronous, read-only delegation through the live interactive host.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rune_agent::subagent_role::SubagentRole;
use rune_agent::subagent_tool::{ChildOutcome, Delegate};
use rune_agent::turn::{self, Event, Host, StopReason};
use rune_agent::{
    Admission, Authority, Cancellation, History, SteeringQueue, Subagent, SubagentRequest,
};
use rune_core::budget::{Budget, BudgetSet, LimitName};
use rune_core::config::{Effort, Layer};
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::id::SessionId;
use rune_net::message::ToolSpec;
use rune_net::provider::Provider;
use rune_net::transport::Endpoint;
use rune_policy::Outcome;
use rune_tools::{Activity, ExecutionContext, Registry, Tool, ToolOutput};

/// Settings and paths a child resolves its limits from when it runs another model.
pub type LimitInputs = (rune_core::config::Settings, rune_core::paths::Paths);

/// Steps a child may take when its parent is not capped lower.
pub const CHILD_STEPS: u64 = 64;

/// Registers the `subagent` tool over the tools already in `registry`, and
/// returns the bridge that runs its children.
pub fn register(
    registry: &mut Registry,
    settings: &rune_core::config::Settings,
    paths: &rune_core::paths::Paths,
    workspace: &camino::Utf8Path,
    rules: &rune_policy::RuleSet,
) -> Result<Arc<Bridge>> {
    let mut tools: Vec<String> = registry.names().into_iter().map(str::to_owned).collect();
    tools.push("subagent".to_owned());
    let bridge = Bridge::new(
        Authority {
            mode: settings.permission_mode,
            rules: rules.clone(),
            workspace: workspace.to_owned(),
            roots: std::iter::once(workspace.to_owned())
                .chain(settings.additional_directories.iter().cloned())
                .collect(),
            tools,
            mcp_view: None,
            generation: 0,
        },
        Some((settings.clone(), paths.clone())),
    );
    registry.insert(bridge.registered())?;
    Ok(bridge)
}

/// Shared tool state. Execution borrows the live host rather than caching policy.
pub struct Bridge {
    tool: Subagent,
    children: AtomicU64,
    limit_inputs: Option<LimitInputs>,
}

impl Bridge {
    pub fn new(authority: Authority, limit_inputs: Option<LimitInputs>) -> Arc<Self> {
        Arc::new(Self {
            // Direct registry callers have no parent host. SessionHost supplies
            // its live delegate through call_with_delegate below.
            tool: Subagent::unsupported(authority),
            children: AtomicU64::new(0),
            limit_inputs,
        })
    }

    pub fn registered(self: &Arc<Self>) -> Box<dyn Tool> {
        Box::new(Registered(Arc::clone(self)))
    }

    pub fn call(
        &self,
        parent: &(dyn Host + Sync),
        registry: &Registry,
        arguments: &serde_json::Value,
        context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        self.tool.call_with_delegate(
            arguments,
            context,
            &Runner {
                parent,
                registry,
                children: &self.children,
                limit_inputs: self.limit_inputs.as_ref(),
            },
        )
    }
}

struct Registered(Arc<Bridge>);

impl Tool for Registered {
    fn name(&self) -> &'static str {
        "subagent"
    }
    fn description(&self) -> &'static str {
        "Run a read-only child task and return its result. agent selects a role from \
         .rune/agents/<agent>.json."
    }
    fn input_schema(&self) -> serde_json::Value {
        let mut schema = self.0.tool.input_schema();
        schema["properties"]["action"]["enum"] = serde_json::json!(["run"]);
        if let Some(action) = schema["properties"]["action"].as_object_mut() {
            action.remove("description");
        }
        if let Some(properties) = schema["properties"].as_object_mut() {
            properties.remove("message");
            properties["agent"]["description"] = serde_json::json!("Role name.");
        }
        schema
    }
    fn activity(&self) -> Activity {
        Activity::Delegate
    }
    fn call(
        &self,
        arguments: &serde_json::Value,
        context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        self.0.tool.call(arguments, context)
    }
}

struct Runner<'a> {
    parent: &'a (dyn Host + Sync),
    registry: &'a Registry,
    children: &'a AtomicU64,
    limit_inputs: Option<&'a LimitInputs>,
}

/// Returns the context window and output ceiling a child runs `model` with.
///
/// The parent's model keeps the parent's resolved limits; another model is
/// resolved for itself, because the parent's window says nothing about it.
fn child_limits(
    parent: &dyn Host,
    model: &str,
    limit_inputs: Option<&LimitInputs>,
) -> (Option<u64>, Option<u64>) {
    if model == parent.model() {
        return (
            parent.context_window().filter(|window| *window > 0),
            parent.max_output_tokens(),
        );
    }
    limit_inputs.map_or((None, None), |(settings, paths)| {
        let resolved = crate::model_limits::resolve(settings, paths, model, None);
        (resolved.context_window, resolved.max_output)
    })
}

impl Runner<'_> {
    fn tools(&self, admission: &Admission, role: Option<&SubagentRole>) -> Vec<ToolSpec> {
        self.parent
            .tools()
            .into_iter()
            .filter(|spec| {
                admission.tool_names.contains(&spec.name)
                    && role.is_none_or(|role| role.tools.contains(&spec.name))
                    && self
                        .registry
                        .get(&spec.name)
                        .is_some_and(Tool::is_read_only)
            })
            .collect()
    }
}

impl Delegate for Runner<'_> {
    fn delegate(&self, request: &SubagentRequest, admission: &Admission) -> Result<ChildOutcome> {
        self.parent.cancellation().check()?;
        let role = request
            .agent
            .as_deref()
            .map(|name| SubagentRole::load(&admission.workspace, name))
            .transpose()?;
        let mut limits = self.parent.limits();
        let cap = limits
            .get(LimitName::SubagentChildren)
            .value()
            .unwrap_or(u64::MAX);
        let seq = self
            .children
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                count.checked_add(1).filter(|next| *next <= cap)
            })
            .map_err(|_| RuneError::new(ErrorCode::LimitExceeded, "subagent child limit reached"))?
            .saturating_add(1);
        let parent_cap = limits
            .get(LimitName::MaxAgentSteps)
            .value()
            .filter(|cap| *cap > 0);
        let steps = match (self.parent.remaining_steps(), parent_cap) {
            (Some(remaining), _) => remaining.clamp(1, CHILD_STEPS),
            (None, Some(cap)) => cap.min(CHILD_STEPS),
            (None, None) => CHILD_STEPS,
        };
        limits.set(
            LimitName::MaxAgentSteps,
            Budget::Bounded(steps),
            Layer::CommandLine,
        )?;
        let tools = self.tools(admission, role.as_ref());
        let brief = request.instructions.as_deref().unwrap_or(
            "Complete the delegated read task and report a concise result to the parent.",
        );
        let mut instructions = self.parent.instructions();
        if let Some(role) = &role {
            instructions.push_str("\n\n");
            instructions.push_str(&role.instructions);
        }
        instructions.push_str("\n\n");
        instructions.push_str(brief);
        let model = request.model.clone().unwrap_or_else(|| self.parent.model());
        let (context_window, max_output) = child_limits(self.parent, &model, self.limit_inputs);
        let child = ChildHost {
            parent: self.parent,
            tools,
            model,
            context_window,
            max_output,
            effort: request.effort.unwrap_or_else(|| self.parent.effort()),
            instructions,
            context: self.parent.context(),
            steering: SteeringQueue::from_limits(&limits),
            limits,
        };
        let mut history = History::new();
        history.push_user(
            request
                .task
                .clone()
                .ok_or_else(|| RuneError::missing_field("task"))?,
        );
        let outcome =
            turn::run_turn(&mut history, &child).and_then(turn::TurnOutcome::into_strict)?;
        // A child at its step limit returns what it found so far; any other
        // stop short of completion is a failure.
        let capped = outcome.stop_reason == StopReason::StepLimit;
        if outcome.stop_reason != StopReason::Completed && !capped {
            return Err(RuneError::new(
                ErrorCode::LimitExceeded,
                format!("child stopped: {}", outcome.stop_reason.as_str()),
            ));
        }
        let id = rune_agent::ChildId {
            session: SessionId::generate(),
            seq,
        };
        Ok(ChildOutcome {
            id,
            label: format!("child-{seq}"),
            // A role is reusable; it is not the name of a persistent child.
            name: None,
            summary: if capped {
                partial_findings(&outcome.text)
            } else {
                outcome.text
            },
        })
    }
}

/// The result of a child stopped at its step limit.
fn partial_findings(text: &str) -> String {
    if text.trim().is_empty() {
        "child stopped at its step limit; findings so far: none".to_owned()
    } else {
        format!("child stopped at its step limit; findings so far:\n{text}")
    }
}

struct ChildHost<'a> {
    parent: &'a (dyn Host + Sync),
    tools: Vec<ToolSpec>,
    model: String,
    context_window: Option<u64>,
    max_output: Option<u64>,
    effort: Effort,
    instructions: String,
    context: ExecutionContext,
    limits: BudgetSet,
    steering: SteeringQueue,
}

impl ChildHost<'_> {
    fn allowed(&self, name: &str) -> bool {
        self.tools.iter().any(|spec| spec.name == name)
            && self.parent.tools().iter().any(|spec| spec.name == name)
    }
}

impl Host for ChildHost<'_> {
    fn dialect(&self) -> &dyn Provider {
        self.parent.dialect()
    }
    fn endpoint(&self) -> &Endpoint {
        self.parent.endpoint()
    }
    fn fetch(&self) -> Box<dyn rune_net::fetch::Fetch> {
        self.parent.fetch()
    }
    fn observe_response(&self, outcome: &rune_net::transport::StreamOutcome) {
        self.parent.record_child_usage(&self.model, &outcome.usage);
    }
    fn model(&self) -> String {
        self.model.clone()
    }
    fn effort(&self) -> Effort {
        self.effort
    }
    fn max_output_tokens(&self) -> Option<u64> {
        self.max_output
    }
    fn context_window(&self) -> Option<u64> {
        self.context_window
    }
    fn instructions(&self) -> String {
        self.instructions.clone()
    }
    fn tools(&self) -> Vec<ToolSpec> {
        self.tools.clone()
    }
    fn fast_mode(&self) -> bool {
        self.parent.fast_mode()
    }
    fn cache_key(&self) -> Option<String> {
        self.parent.cache_key()
    }
    fn temperature(&self) -> Option<f64> {
        self.parent.temperature()
    }
    fn provider_order(&self) -> Vec<String> {
        self.parent.provider_order()
    }
    fn provider_strict(&self) -> bool {
        self.parent.provider_strict()
    }
    fn compat(
        &self,
        model: &str,
    ) -> (
        rune_net::compat::EndpointCompat,
        rune_net::compat::ModelTraits,
    ) {
        self.parent.compat(model)
    }
    // Child deltas and finish events belong to the child, not the parent's UI
    // or recorder. The parent displays and records the returned tool result.
    fn emit(&self, _event: Event) {}
    fn tool_activity(&self, name: &str) -> Option<Activity> {
        self.parent.tool_activity(name)
    }
    fn tool_permission_target(&self, name: &str, arguments: &serde_json::Value) -> Option<String> {
        self.parent.tool_permission_target(name, arguments)
    }
    fn decide(&self, name: &str, target: Option<&str>) -> (Outcome, String) {
        if !self.allowed(name) {
            return (
                Outcome::Deny,
                "tool is outside the child's inherited allowlist".to_owned(),
            );
        }
        self.parent.decide(name, target)
    }
    fn decide_with_arguments(
        &self,
        name: &str,
        target: Option<&str>,
        arguments: &serde_json::Value,
    ) -> (Outcome, String) {
        if !self.allowed(name) {
            return self.decide(name, target);
        }
        self.parent.decide_with_arguments(name, target, arguments)
    }
    fn pre_tool(&self, call: &turn::PreparedCall) -> Result<Option<turn::HookVeto>> {
        self.parent.pre_tool(call)
    }

    fn post_tool(&self, result: &turn::CallResult) {
        self.parent.post_tool(result);
    }

    fn execute(&self, name: &str, arguments: &serde_json::Value) -> Result<ToolOutput> {
        self.execute_with_context(name, arguments, &self.context)
    }
    fn execute_with_context(
        &self,
        name: &str,
        arguments: &serde_json::Value,
        context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        if !self.allowed(name) {
            return Err(RuneError::new(
                ErrorCode::PermissionDenied,
                "tool is outside the child's inherited allowlist",
            ));
        }
        self.parent.cancellation().check()?;
        context.check_cancelled()?;
        self.parent.execute_with_context(name, arguments, context)
    }
    fn context(&self) -> ExecutionContext {
        self.context.fork()
    }
    fn objective_tokens_remaining(&self) -> Option<u64> {
        self.parent.objective_tokens_remaining()
    }
    fn account_objective_tokens(&self, tokens: u64) -> Result<()> {
        self.parent.account_objective_tokens(tokens)
    }
    fn limits(&self) -> BudgetSet {
        self.limits.clone()
    }
    fn cancellation(&self) -> Cancellation {
        self.parent.cancellation()
    }
    fn steering(&self) -> &SteeringQueue {
        &self.steering
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use rune_core::config::PermissionMode;
    use rune_policy::RuleSet;
    use std::sync::Mutex;

    struct Parent {
        endpoint: Endpoint,
        context: ExecutionContext,
        tools: Mutex<Vec<ToolSpec>>,
        executed: AtomicU64,
        cancellation: Cancellation,
        steering: SteeringQueue,
        limits: BudgetSet,
        objective_remaining: Mutex<Option<u64>>,
    }

    impl Host for Parent {
        fn dialect(&self) -> &dyn Provider {
            &rune_net::chat_completions::ChatCompletions
        }
        fn endpoint(&self) -> &Endpoint {
            &self.endpoint
        }
        fn model(&self) -> String {
            "fixture".to_owned()
        }
        fn context_window(&self) -> Option<u64> {
            Some(50_000)
        }
        fn max_output_tokens(&self) -> Option<u64> {
            Some(5_000)
        }
        fn instructions(&self) -> String {
            String::new()
        }
        fn tools(&self) -> Vec<ToolSpec> {
            self.tools.lock().expect("tools").clone()
        }
        fn emit(&self, _event: Event) {}
        fn execute(&self, _name: &str, _arguments: &serde_json::Value) -> Result<ToolOutput> {
            self.executed.fetch_add(1, Ordering::Relaxed);
            Ok(ToolOutput::success("executed"))
        }
        fn decide(&self, _name: &str, target: Option<&str>) -> (Outcome, String) {
            (
                if target == Some("secret.txt") {
                    Outcome::Deny
                } else {
                    Outcome::Allow
                },
                "parent policy".to_owned(),
            )
        }
        fn context(&self) -> ExecutionContext {
            self.context.fork()
        }
        fn limits(&self) -> BudgetSet {
            self.limits.clone()
        }
        fn objective_tokens_remaining(&self) -> Option<u64> {
            *self.objective_remaining.lock().expect("budget")
        }
        fn account_objective_tokens(&self, tokens: u64) -> Result<()> {
            if let Some(remaining) = self.objective_remaining.lock().expect("budget").as_mut() {
                *remaining = remaining.saturating_sub(tokens);
            }
            Ok(())
        }
        fn cancellation(&self) -> Cancellation {
            self.cancellation.clone()
        }
        fn steering(&self) -> &SteeringQueue {
            &self.steering
        }
    }

    fn parent() -> Parent {
        let mut registry = Registry::new();
        registry
            .insert(Box::new(rune_tools::ReadFile::new()))
            .expect("read tool");
        Parent {
            endpoint: Endpoint::new("https://example.invalid", "fixture"),
            context: ExecutionContext::new(Utf8PathBuf::from("/workspace")).with_offline(true),
            tools: Mutex::new(registry.all_schemas()),
            executed: AtomicU64::new(0),
            cancellation: Cancellation::new(),
            steering: SteeringQueue::new(4),
            limits: BudgetSet::new(),
            objective_remaining: Mutex::new(None),
        }
    }

    fn read_authority(parent: &Parent) -> Authority {
        Authority {
            mode: PermissionMode::Auto,
            rules: RuleSet::new(),
            workspace: parent.context.workspace.clone(),
            roots: vec![parent.context.workspace.clone()],
            tools: vec!["read_file".to_owned()],
            mcp_view: None,
            generation: 0,
        }
    }

    #[test]
    fn a_child_at_its_step_limit_returns_its_findings() {
        // Every step but the last calls a tool; the last one, told it is the
        // last, answers.
        let mut turns: Vec<rune_testkit::scripted::Turn> = (1..CHILD_STEPS)
            .map(|index| {
                rune_testkit::scripted::Turn::call(
                    &format!("call_{index}"),
                    "read_file",
                    &format!(r#"{{"path":"f{index}.rs"}}"#),
                )
            })
            .collect();
        turns.push(rune_testkit::scripted::Turn::text(
            "the parser is in lex.rs",
        ));
        let provider = rune_testkit::scripted::ScriptedProvider::start(turns);
        let mut parent = parent();
        parent.endpoint = Endpoint::new(provider.base_url(), "fixture");
        let mut registry = Registry::new();
        registry
            .insert(Box::new(rune_tools::ReadFile::new()))
            .expect("read");
        let children = AtomicU64::new(0);
        let runner = Runner {
            parent: &parent,
            registry: &registry,
            children: &children,
            limit_inputs: None,
        };
        let request = SubagentRequest::decode(&serde_json::json!({
            "action": "run",
            "task": "find the parser",
        }))
        .expect("request");
        let outcome = runner
            .delegate(&request, &Admission::capture(&read_authority(&parent)))
            .expect("partial findings");
        assert!(
            outcome
                .summary
                .starts_with("child stopped at its step limit; findings so far:\n"),
            "{}",
            outcome.summary
        );
        assert!(outcome.summary.contains("lex.rs"), "{}", outcome.summary);
        assert_eq!(
            u64::try_from(provider.request_count()).expect("count"),
            CHILD_STEPS
        );
        assert_eq!(
            partial_findings("  "),
            "child stopped at its step limit; findings so far: none"
        );
    }

    #[test]
    fn a_child_offers_only_read_only_tools() {
        let parent = parent();
        let mut registry = Registry::new();
        registry
            .insert(Box::new(rune_tools::ReadFile::new()))
            .expect("read");
        registry
            .insert(Box::new(rune_tools::WriteFile::new()))
            .expect("write");
        *parent.tools.lock().expect("tools") = registry.all_schemas();
        let mut authority = read_authority(&parent);
        authority.tools.push("write_file".to_owned());
        let children = AtomicU64::new(0);
        let runner = Runner {
            parent: &parent,
            registry: &registry,
            children: &children,
            limit_inputs: None,
        };
        let names: Vec<String> = runner
            .tools(&Admission::capture(&authority), None)
            .into_iter()
            .map(|spec| spec.name)
            .collect();
        assert_eq!(names, ["read_file"]);
    }

    #[test]
    fn a_child_inherits_limits_for_the_same_model_and_resolves_another() {
        let parent = parent();
        assert_eq!(
            child_limits(&parent, "fixture", None),
            (Some(50_000), Some(5_000))
        );
        // Another model without anything to resolve from is unknown, not the
        // parent's window.
        assert_eq!(child_limits(&parent, "other", None), (None, None));
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_str().expect("utf8");
        let inputs: LimitInputs = (
            rune_core::config::Settings {
                context_window_override: Some(64_000),
                max_output_override: Some(6_400),
                ..rune_core::config::Settings::default()
            },
            rune_core::paths::Paths::resolve(Some(root), None, None, None, Some(root)),
        );
        assert_eq!(
            child_limits(&parent, "other", Some(&inputs)),
            (Some(64_000), Some(6_400))
        );
    }

    #[test]
    fn a_child_charges_the_parent_objective_and_cannot_continue_after_exhaustion() {
        let parent = parent();
        *parent.objective_remaining.lock().expect("budget") = Some(15);
        let child = ChildHost {
            parent: &parent,
            tools: parent.tools(),
            model: parent.model(),
            context_window: None,
            max_output: None,
            effort: Effort::Auto,
            instructions: String::new(),
            context: parent.context(),
            limits: parent.limits(),
            steering: SteeringQueue::new(4),
        };
        assert_eq!(child.objective_tokens_remaining(), Some(15));
        let mut plan = rune_net::provider::RequestPlan::new(child.model());
        let accounting = turn::ObjectiveAccounting::begin(&child, &mut plan).expect("begin");
        accounting
            .finish(
                &child,
                Some(rune_net::stream::Usage {
                    input_tokens: Some(10),
                    output_tokens: Some(5),
                    ..rune_net::stream::Usage::default()
                }),
            )
            .expect("shared charge");
        assert_eq!(parent.objective_tokens_remaining(), Some(0));
        let mut history = History::new();
        history.push_user("continue");
        let outcome = turn::run_turn(&mut history, &child).expect("no provider request");
        assert_eq!(outcome.stop_reason, StopReason::ObjectiveBudget);
        assert_eq!(outcome.steps, 0);
    }

    #[test]
    fn direct_child_execution_enforces_the_allowlist_even_when_parent_policy_allows() {
        let parent = parent();
        let child = ChildHost {
            parent: &parent,
            tools: parent.tools(),
            model: parent.model(),
            context_window: None,
            max_output: None,
            effort: Effort::Auto,
            instructions: String::new(),
            context: parent.context(),
            limits: parent.limits(),
            steering: SteeringQueue::new(4),
        };
        assert!(child.context().offline);
        assert_eq!(
            child.decide("read_file", Some("secret.txt")).0,
            Outcome::Deny
        );
        for name in ["write_file", "shell", "subagent", "unadvertised_read"] {
            assert_eq!(parent.decide(name, None).0, Outcome::Allow);
            assert_eq!(child.decide(name, None).0, Outcome::Deny);
            assert_eq!(
                child
                    .execute(name, &serde_json::json!({}))
                    .expect_err("refused")
                    .code(),
                ErrorCode::PermissionDenied
            );
        }
        assert_eq!(parent.executed.load(Ordering::Relaxed), 0);
        child
            .execute("read_file", &serde_json::json!({}))
            .expect("inherited read");
        assert_eq!(parent.executed.load(Ordering::Relaxed), 1);
        parent.tools.lock().expect("tools").clear();
        assert_eq!(
            child
                .execute("read_file", &serde_json::json!({}))
                .expect_err("removed tool")
                .code(),
            ErrorCode::PermissionDenied
        );
        assert_eq!(parent.executed.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn child_schemas_intersect_admission_parent_advertisement_and_read_only_registry() {
        let parent = parent();
        let mut registry = Registry::new();
        registry
            .insert(Box::new(rune_tools::ReadFile::new()))
            .expect("read");
        registry
            .insert(Box::new(rune_tools::WriteFile::new()))
            .expect("write");
        let mut authority = Authority {
            mode: PermissionMode::Auto,
            rules: RuleSet::new(),
            workspace: parent.context.workspace.clone(),
            roots: vec![parent.context.workspace.clone()],
            tools: vec!["read_file".to_owned(), "write_file".to_owned()],
            mcp_view: None,
            generation: 0,
        };
        let children = AtomicU64::new(0);
        let runner = Runner {
            parent: &parent,
            registry: &registry,
            children: &children,
            limit_inputs: None,
        };
        *parent.tools.lock().expect("tools") = registry.all_schemas();
        let offered = runner.tools(&Admission::capture(&authority), None);
        assert_eq!(
            offered
                .iter()
                .map(|spec| spec.name.as_str())
                .collect::<Vec<_>>(),
            ["read_file"]
        );
        authority.tools.retain(|name| name != "read_file");
        assert!(
            runner
                .tools(&Admission::capture(&authority), None)
                .is_empty()
        );
        authority.tools.push("read_file".to_owned());
        parent
            .tools
            .lock()
            .expect("tools")
            .retain(|spec| spec.name != "read_file");
        assert!(
            runner
                .tools(&Admission::capture(&authority), None)
                .is_empty()
        );
    }

    #[test]
    fn fixture_role_can_only_narrow_inherited_tools_and_keeps_parent_policy() {
        let temp = tempfile::tempdir().expect("workspace");
        let root = Utf8PathBuf::from_path_buf(temp.path().to_owned()).expect("UTF-8");
        std::fs::create_dir_all(root.join(".rune/agents")).expect("roles");
        std::fs::write(
            root.join(".rune/agents/reviewer.json"),
            r#"{"instructions":"Review without changing files.","tools":["read_file","write_file","glob_files","subagent"]}"#,
        )
        .expect("fixture role");
        let role = SubagentRole::load(&root, "reviewer").expect("role");
        let parent = parent();
        let mut registry = Registry::new();
        registry
            .insert(Box::new(rune_tools::ReadFile::new()))
            .expect("read");
        registry
            .insert(Box::new(rune_tools::WriteFile::new()))
            .expect("write");
        registry
            .insert(Box::new(rune_tools::GlobFiles::new()))
            .expect("glob");
        *parent.tools.lock().expect("tools") = registry.all_schemas();
        let authority = Authority {
            mode: PermissionMode::Auto,
            rules: RuleSet::new(),
            workspace: parent.context.workspace.clone(),
            roots: vec![parent.context.workspace.clone()],
            // glob_files is registered and read-only, but not inherited.
            tools: vec!["read_file".to_owned(), "write_file".to_owned()],
            mcp_view: None,
            generation: 0,
        };
        let children = AtomicU64::new(0);
        let runner = Runner {
            parent: &parent,
            registry: &registry,
            children: &children,
            limit_inputs: None,
        };
        let admission = Admission::capture(&authority);
        let tools = runner.tools(&admission, Some(&role));
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["read_file"]
        );
        let child = ChildHost {
            parent: &parent,
            tools,
            model: parent.model(),
            context_window: None,
            max_output: None,
            effort: Effort::Auto,
            instructions: role.instructions.clone(),
            context: parent.context(),
            limits: parent.limits(),
            steering: SteeringQueue::new(4),
        };
        assert_eq!(
            child.decide("read_file", Some("secret.txt")).0,
            Outcome::Deny
        );
        assert_eq!(
            child.decide("read_file", Some("public.txt")).0,
            Outcome::Allow
        );
        for name in ["write_file", "glob_files", "subagent"] {
            assert_eq!(child.decide(name, None).0, Outcome::Deny);
            assert_eq!(
                child
                    .execute(name, &serde_json::json!({}))
                    .expect_err("role cannot grant")
                    .code(),
                ErrorCode::PermissionDenied
            );
        }
        assert_eq!(parent.executed.load(Ordering::Relaxed), 0);
        let empty_role = SubagentRole {
            instructions: "Think only".to_owned(),
            tools: Vec::new(),
        };
        assert!(runner.tools(&admission, Some(&empty_role)).is_empty());
    }

    #[test]
    fn child_limit_and_cancellation_refuse_before_any_provider_request() {
        let mut parent = parent();
        parent
            .limits
            .set(
                LimitName::SubagentChildren,
                Budget::Bounded(0),
                Layer::CommandLine,
            )
            .expect("zero children");
        let bridge = Bridge::new(
            Authority {
                mode: PermissionMode::Auto,
                rules: RuleSet::new(),
                workspace: parent.context.workspace.clone(),
                roots: vec![parent.context.workspace.clone()],
                tools: vec!["subagent".to_owned(), "read_file".to_owned()],
                mcp_view: None,
                generation: 0,
            },
            None,
        );
        let arguments = serde_json::json!({"action": "run", "task": "read"});
        let output = bridge
            .call(&parent, &Registry::new(), &arguments, &parent.context)
            .expect("tool failure");
        assert!(output.is_error);
        assert!(output.text.contains("child limit"), "{}", output.text);
        assert_eq!(bridge.children.load(Ordering::Relaxed), 0);
        parent.cancellation.cancel();
        let output = bridge
            .call(&parent, &Registry::new(), &arguments, &parent.context)
            .expect("tool failure");
        assert!(output.is_error);
        assert!(output.text.contains("cancelled"), "{}", output.text);
        assert_eq!(bridge.children.load(Ordering::Relaxed), 0);
    }
}
