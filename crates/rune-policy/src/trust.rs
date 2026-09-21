//! Project-scoped authority.
//!
//! A repository can ask for authority: it can declare MCP servers, additional
//! directories, and permission rules. None of that is the user's decision until
//! the user makes it, so a project file starts untrusted on every machine that
//! has not approved that exact workspace.
//!
//! Approval is recorded against the canonical workspace path, never in the
//! repository. That is what makes two repositories holding the same project file
//! separate decisions rather than one: a file can copy its own declarations
//! anywhere, but it cannot copy the record that the user approved them.
//!
//! Before approval nothing happens. No process is started, no endpoint is
//! contacted, and no environment value is read, which is why an unapproved
//! request reports what it was asking for instead of attempting any of it.

use camino::{Utf8Path, Utf8PathBuf};
use rune_core::error::{ErrorCode, Result, RuneError};
use serde::{Deserialize, Serialize};

/// What a project file asks for.
///
/// Every entry is a request, not an authority. The lists carry the values as
/// the file wrote them, because they are what a person is shown before
/// deciding.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct ProjectRequest {
    /// MCP servers the project declares, by name.
    pub servers: Vec<String>,
    /// Directories outside the workspace the project wants to reach.
    pub additional_directories: Vec<Utf8PathBuf>,
    /// How many permission rules the project declares.
    pub permission_rules: usize,
}

impl ProjectRequest {
    /// Builds a request.
    #[must_use]
    pub fn new(
        servers: Vec<String>,
        additional_directories: Vec<Utf8PathBuf>,
        permission_rules: usize,
    ) -> Self {
        Self {
            servers,
            additional_directories,
            permission_rules,
        }
    }

    /// Returns true when the project asks for nothing that widens authority.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
            && self.additional_directories.is_empty()
            && self.permission_rules == 0
    }

    /// Returns the stable identity of each requested entry.
    ///
    /// A decision is recorded against these identities rather than against a
    /// sentence, so rewording a prompt cannot silently invalidate or grant an
    /// approval. The identity keeps the value: approving `server:one` says
    /// nothing about `server:two`.
    #[must_use]
    pub fn entries(&self) -> Vec<String> {
        let mut entries: Vec<String> = self
            .servers
            .iter()
            .map(|server| format!("{SERVER_ENTRY}{server}"))
            .collect();
        entries.extend(
            self.additional_directories
                .iter()
                .map(|directory| format!("{DIRECTORY_ENTRY}{directory}")),
        );
        if self.permission_rules > 0 {
            entries.push(format!("{RULES_ENTRY}{}", self.permission_rules));
        }
        entries
    }

    /// Returns what the project is asking for, one line per entry.
    ///
    /// The prompt names every server and every directory, because the decision
    /// is about those exact entries. A count would let a file swap one server
    /// for another without changing what was approved.
    #[must_use]
    pub fn describe(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .servers
            .iter()
            .map(|server| format!("start the MCP server `{server}`"))
            .collect();
        lines.extend(
            self.additional_directories
                .iter()
                .map(|directory| format!("reach `{directory}`")),
        );
        if self.permission_rules > 0 {
            lines.push(format!(
                "apply {} permission rule(s) of its own",
                self.permission_rules
            ));
        }
        lines
    }

    /// Returns the actions held while the workspace is not approved.
    ///
    /// Each is named with the verb that would have run, so the agent reports
    /// the same list a person would have approved. Nothing in this list has
    /// been attempted.
    #[must_use]
    pub fn blocked_actions(&self) -> Vec<String> {
        let mut blocked: Vec<String> = self
            .servers
            .iter()
            .map(|server| format!("start MCP server `{server}`"))
            .collect();
        blocked.extend(
            self.additional_directories
                .iter()
                .map(|directory| format!("read files or environment for `{directory}`")),
        );
        if self.permission_rules > 0 {
            blocked.push(format!(
                "apply {} project permission rule(s)",
                self.permission_rules
            ));
        }
        blocked
    }
}

/// Prefix identifying a requested MCP server.
const SERVER_ENTRY: &str = "server:";
/// Prefix identifying a requested additional directory.
const DIRECTORY_ENTRY: &str = "directory:";
/// Prefix identifying the project's permission rules.
const RULES_ENTRY: &str = "rules:";

/// What the trust state decides about a project.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TrustDecision {
    /// The workspace is not approved, so nothing the project asked for happens.
    Untrusted {
        /// The question a person is asked, one line per requested entry.
        prompt: Vec<String>,
        /// Every action held until approval, named individually.
        blocked_actions: Vec<String>,
    },
    /// The workspace is approved, so the project's requests apply.
    Trusted,
    /// Only part of what the project asked for is approved, and the rest is
    /// still pending. Exactly the approved entries take effect.
    PartiallyTrusted {
        /// The approved entries, as a person reads them.
        approved: Vec<String>,
        /// The entries still waiting for a decision.
        pending: Vec<String>,
    },
}

impl TrustDecision {
    /// Returns true when nothing the project asked for takes effect.
    #[must_use]
    pub const fn is_untrusted(&self) -> bool {
        matches!(self, Self::Untrusted { .. })
    }

    /// Returns the prompt a person is shown, when there is one.
    #[must_use]
    pub fn prompt(&self) -> &[String] {
        match self {
            Self::Untrusted { prompt, .. } => prompt,
            Self::Trusted | Self::PartiallyTrusted { .. } => &[],
        }
    }

    /// Returns the entries this decision holds back.
    #[must_use]
    pub fn blocked_actions(&self) -> &[String] {
        match self {
            Self::Untrusted {
                blocked_actions, ..
            } => blocked_actions,
            Self::Trusted => &[],
            Self::PartiallyTrusted { pending, .. } => pending,
        }
    }
}

/// What a user decided about one workspace.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceTrust {
    /// The project's requests apply.
    Approved,
    /// The project's requests do not apply.
    Rejected,
}

/// One workspace's recorded decision.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
struct TrustRecord {
    path: String,
    decision: WorkspaceTrust,
    /// The entries approved individually, or `None` when the whole project file
    /// was approved or rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    entries: Option<Vec<String>>,
}

/// The trust records for every workspace the user has decided about.
///
/// Records are keyed by the canonical workspace path, so a symlink, a trailing
/// separator, or a relative path cannot name a second workspace whose approval
/// was never given. The store is the user's, so it lives in user state: a
/// project cannot write it, and a clone of the repository carries no approval.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TrustStore {
    entries: Vec<TrustRecord>,
}

impl TrustStore {
    /// Returns a store with no decisions.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records approval of a workspace's whole project file.
    pub fn approve(&mut self, workspace: &Utf8Path) {
        self.record(
            workspace,
            TrustRecord {
                path: workspace.as_str().to_owned(),
                decision: WorkspaceTrust::Approved,
                entries: None,
            },
        );
    }

    /// Records approval of individual entries of a workspace's project file.
    ///
    /// The entries are identities from [`ProjectRequest::entries`]. Approving
    /// one leaves every other entry pending, which is what makes a later entry
    /// in the same file a fresh decision rather than a covered one.
    pub fn approve_entries(&mut self, workspace: &Utf8Path, entries: &[String]) {
        let approved = entries.to_vec();
        self.record(
            workspace,
            TrustRecord {
                path: workspace.as_str().to_owned(),
                decision: WorkspaceTrust::Approved,
                entries: Some(approved),
            },
        );
    }

    /// Records refusal of a workspace's whole project file.
    pub fn reject(&mut self, workspace: &Utf8Path) {
        self.record(
            workspace,
            TrustRecord {
                path: workspace.as_str().to_owned(),
                decision: WorkspaceTrust::Rejected,
                entries: None,
            },
        );
    }

    /// Records approval of the whole project file for several workspaces.
    ///
    /// Nothing outside the named workspaces is touched.
    pub fn approve_all(&mut self, workspaces: &[Utf8PathBuf]) {
        for workspace in workspaces {
            self.approve(workspace);
        }
    }

    /// Drops the decision recorded for a workspace.
    ///
    /// Only this workspace is cleared. Another workspace holding an identical
    /// project file keeps its own decision.
    pub fn reset(&mut self, workspace: &Utf8Path) {
        let key = workspace.as_str();
        self.entries.retain(|record| record.path != key);
    }

    /// Returns true when a workspace's whole project file is approved.
    ///
    /// A workspace approved entry by entry is not trusted as a whole, because
    /// the parts nobody decided about are still pending.
    #[must_use]
    pub fn is_trusted(&self, path: &Utf8Path) -> bool {
        self.record_for(path).is_some_and(|record| {
            record.decision == WorkspaceTrust::Approved && record.entries.is_none()
        })
    }

    /// Returns the recorded decision for a workspace.
    #[must_use]
    pub fn decision(&self, path: &Utf8Path) -> Option<WorkspaceTrust> {
        self.record_for(path).map(|record| record.decision)
    }

    /// Returns the entries approved individually for a workspace.
    #[must_use]
    pub fn approved_entries(&self, path: &Utf8Path) -> &[String] {
        self.record_for(path)
            .and_then(|record| record.entries.as_deref())
            .unwrap_or(&[])
    }

    /// Returns the number of recorded decisions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns true when no decision is recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns every recorded workspace path with its decision.
    #[must_use]
    pub fn entries(&self) -> Vec<(&str, WorkspaceTrust)> {
        self.entries
            .iter()
            .map(|record| (record.path.as_str(), record.decision))
            .collect()
    }

    fn record_for(&self, path: &Utf8Path) -> Option<&TrustRecord> {
        let key = path.as_str();
        self.entries.iter().find(|record| record.path == key)
    }

    fn record(&mut self, workspace: &Utf8Path, record: TrustRecord) {
        let key = workspace.as_str();
        if let Some(existing) = self.entries.iter_mut().find(|held| held.path == key) {
            *existing = record;
            return;
        }
        self.entries.push(record);
    }
}

/// Decides what a project's request is worth in a workspace.
///
/// The decision rests on the recorded approval alone. An unapproved workspace
/// produces a decision naming what is held, and the caller performs none of it:
/// no process, no endpoint, no environment read. A rejected workspace is
/// reported the same way, because the effect is identical and the prompt is the
/// useful answer.
#[must_use]
pub fn decide(
    request: &ProjectRequest,
    approved: &TrustStore,
    workspace: &Utf8Path,
) -> TrustDecision {
    if request.is_empty() {
        // A project that asks for nothing widens nothing, so there is nothing
        // to approve and nothing to hold.
        return TrustDecision::Trusted;
    }
    if approved.is_trusted(workspace) {
        return TrustDecision::Trusted;
    }
    let recorded = approved.approved_entries(workspace);
    if recorded.is_empty() {
        return TrustDecision::Untrusted {
            prompt: request.describe(),
            blocked_actions: request.blocked_actions(),
        };
    }
    let mut granted = Vec::new();
    let mut pending = Vec::new();
    for (identity, line, blocked) in request
        .entries()
        .into_iter()
        .zip(request.describe())
        .zip(request.blocked_actions())
        .map(|((identity, line), blocked)| (identity, line, blocked))
    {
        if recorded.contains(&identity) {
            granted.push(line);
        } else {
            pending.push(blocked);
        }
    }
    if pending.is_empty() {
        return TrustDecision::Trusted;
    }
    TrustDecision::PartiallyTrusted {
        approved: granted,
        pending,
    }
}

/// Returns the canonical form of a workspace path.
///
/// Approval is recorded against this form. A path that cannot be resolved is
/// returned unchanged, so a decision can still be recorded before the directory
/// exists.
#[must_use]
pub fn canonical_workspace(workspace: &Utf8Path) -> Utf8PathBuf {
    workspace
        .canonicalize_utf8()
        .unwrap_or_else(|_| workspace.to_owned())
}

/// Returns the error for acting on an unapproved project.
pub fn unapproved_error(workspace: &Utf8Path, decision: &TrustDecision) -> RuneError {
    let blocked = decision.blocked_actions();
    let detail = if blocked.is_empty() {
        "nothing the project asks for is approved".to_owned()
    } else {
        blocked.join("; ")
    };
    RuneError::new(
        ErrorCode::PermissionDenied,
        format!("`{workspace}` is not approved: {detail}"),
    )
    .with_hint("approve the workspace to let the project's requests apply")
}

/// Returns the error for entries that are still pending.
pub fn pending_error(workspace: &Utf8Path, pending: &[String]) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    Err(RuneError::new(
        ErrorCode::InputRequired,
        format!(
            "`{workspace}` is approved for part of what it asks for; still pending: {}",
            pending.join("; ")
        ),
    )
    .with_hint("approve the remaining entries before they take effect"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::TempDir;

    use super::*;

    fn tempdir() -> TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    fn utf8(path: &Path) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(path.to_path_buf()).expect("utf8 path")
    }

    /// Builds a workspace holding an `.mcp.json`, returning its canonical path.
    fn workspace_with_mcp_json(dir: &TempDir, body: &str) -> Utf8PathBuf {
        std::fs::write(dir.path().join(".mcp.json"), body).expect("write");
        utf8(dir.path()).canonicalize_utf8().expect("resolve")
    }

    const MCP_JSON: &str = r#"{"mcpServers":{"project-local":{"command":"npx"}}}"#;

    fn request() -> ProjectRequest {
        ProjectRequest::new(
            vec!["project-local".to_owned(), "project-remote".to_owned()],
            vec![Utf8PathBuf::from("/opt/data")],
            2,
        )
    }

    #[test]
    fn an_unapproved_workspace_blocks_every_requested_entry() {
        let dir = tempdir();
        let workspace = workspace_with_mcp_json(&dir, MCP_JSON);
        let request = request();

        let decision = decide(&request, &TrustStore::new(), &workspace);
        assert!(decision.is_untrusted());
        let blocked = decision.blocked_actions();
        assert!(blocked.iter().any(|entry| entry.contains("project-local")));
        assert!(blocked.iter().any(|entry| entry.contains("project-remote")));
        assert!(blocked.iter().any(|entry| entry.contains("/opt/data")));
        assert!(
            blocked
                .iter()
                .any(|entry| entry.contains("2 project permission"))
        );
        assert_eq!(blocked.len(), 4, "every entry is named: {blocked:?}");
    }

    #[test]
    fn the_prompt_names_every_server_and_directory() {
        let dir = tempdir();
        let workspace = workspace_with_mcp_json(&dir, MCP_JSON);
        let decision = decide(&request(), &TrustStore::new(), &workspace);
        let prompt = decision.prompt();
        assert!(prompt.iter().any(|line| line.contains("`project-local`")));
        assert!(prompt.iter().any(|line| line.contains("`project-remote`")));
        assert!(prompt.iter().any(|line| line.contains("`/opt/data`")));
        assert_eq!(prompt.len(), 4);
    }

    #[test]
    fn a_request_that_widens_nothing_needs_no_approval() {
        let dir = tempdir();
        let workspace = workspace_with_mcp_json(&dir, MCP_JSON);
        let empty = ProjectRequest::default();
        assert!(empty.is_empty());
        assert_eq!(
            decide(&empty, &TrustStore::new(), &workspace),
            TrustDecision::Trusted
        );
    }

    #[test]
    fn an_approved_workspace_applies_the_request() {
        let dir = tempdir();
        let workspace = workspace_with_mcp_json(&dir, MCP_JSON);
        let mut store = TrustStore::new();
        store.approve(&workspace);
        assert!(store.is_trusted(&workspace));
        assert_eq!(
            decide(&request(), &store, &workspace),
            TrustDecision::Trusted
        );
    }

    #[test]
    fn approving_one_workspace_does_not_approve_another_with_the_same_file() {
        let first_dir = tempdir();
        let second_dir = tempdir();
        let first = workspace_with_mcp_json(&first_dir, MCP_JSON);
        let second = workspace_with_mcp_json(&second_dir, MCP_JSON);
        assert_ne!(first, second);
        assert_eq!(
            std::fs::read_to_string(first.join(".mcp.json")).expect("first"),
            std::fs::read_to_string(second.join(".mcp.json")).expect("second"),
            "the two project files are identical"
        );

        let mut store = TrustStore::new();
        store.approve(&first);

        assert!(store.is_trusted(&first));
        assert!(!store.is_trusted(&second));
        let decision = decide(&request(), &store, &second);
        assert!(decision.is_untrusted());
        assert_eq!(decision.blocked_actions().len(), 4);
    }

    #[test]
    fn approval_does_not_follow_a_copy_of_the_project_file() {
        let first_dir = tempdir();
        let second_dir = tempdir();
        let first = workspace_with_mcp_json(&first_dir, MCP_JSON);
        let second = workspace_with_mcp_json(&second_dir, MCP_JSON);
        std::fs::copy(first.join(".mcp.json"), second.join(".mcp.json")).expect("copy");

        let mut store = TrustStore::new();
        store.approve(&first);
        let decision = decide(&request(), &store, &second);
        assert!(decision.is_untrusted(), "a copied file carries no approval");
    }

    #[test]
    fn a_symlink_to_an_approved_workspace_resolves_to_it() {
        let dir = tempdir();
        let link_dir = tempdir();
        let workspace = workspace_with_mcp_json(&dir, MCP_JSON);
        let link = utf8(link_dir.path()).join("linked");
        std::os::unix::fs::symlink(&workspace, &link).expect("symlink");

        let mut store = TrustStore::new();
        store.approve(&workspace);
        assert!(store.is_trusted(&canonical_workspace(&link)));
    }

    #[test]
    fn a_relative_workspace_is_not_the_approved_one() {
        let dir = tempdir();
        let workspace = workspace_with_mcp_json(&dir, MCP_JSON);
        let mut store = TrustStore::new();
        store.approve(&workspace);
        assert!(!store.is_trusted(Utf8Path::new(".")));
        assert_ne!(
            canonical_workspace(Utf8Path::new(".")),
            Utf8PathBuf::new(),
            "a relative path still resolves to a real workspace"
        );
    }

    #[test]
    fn a_path_that_does_not_exist_is_recorded_as_given() {
        let missing = Utf8Path::new("/nonexistent/workspace/rune-test");
        let mut store = TrustStore::new();
        store.approve(missing);
        assert!(store.is_trusted(missing));
        assert_eq!(canonical_workspace(missing), Utf8PathBuf::from(missing));
    }

    #[test]
    fn a_rejected_workspace_stays_untrusted_and_still_names_everything() {
        let dir = tempdir();
        let workspace = workspace_with_mcp_json(&dir, MCP_JSON);
        let mut store = TrustStore::new();
        store.reject(&workspace);
        assert!(!store.is_trusted(&workspace));
        assert_eq!(store.decision(&workspace), Some(WorkspaceTrust::Rejected));
        let decision = decide(&request(), &store, &workspace);
        assert!(decision.is_untrusted());
        assert_eq!(decision.blocked_actions().len(), 4);
    }

    #[test]
    fn responding_again_replaces_the_earlier_decision() {
        let dir = tempdir();
        let workspace = workspace_with_mcp_json(&dir, MCP_JSON);
        let mut store = TrustStore::new();
        store.reject(&workspace);
        store.approve(&workspace);
        assert_eq!(store.len(), 1, "a workspace holds one decision");
        assert!(store.is_trusted(&workspace));
    }

    #[test]
    fn approving_all_covers_each_named_workspace() {
        let first_dir = tempdir();
        let second_dir = tempdir();
        let first = workspace_with_mcp_json(&first_dir, MCP_JSON);
        let second = workspace_with_mcp_json(&second_dir, MCP_JSON);
        let mut store = TrustStore::new();
        store.approve_all(&[first.clone(), second.clone()]);
        assert!(store.is_trusted(&first));
        assert!(store.is_trusted(&second));
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn reset_clears_only_the_named_workspace() {
        let first_dir = tempdir();
        let second_dir = tempdir();
        let first = workspace_with_mcp_json(&first_dir, MCP_JSON);
        let second = workspace_with_mcp_json(&second_dir, MCP_JSON);
        let mut store = TrustStore::new();
        store.approve(&first);
        store.approve(&second);

        store.reset(&first);
        assert!(!store.is_trusted(&first));
        assert!(
            store.is_trusted(&second),
            "the other workspace is untouched"
        );
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn entry_approval_leaves_the_rest_pending() {
        let dir = tempdir();
        let workspace = workspace_with_mcp_json(&dir, MCP_JSON);
        let request = request();
        let mut store = TrustStore::new();
        store.approve_entries(&workspace, &["server:project-local".to_owned()]);

        assert!(
            !store.is_trusted(&workspace),
            "the file is not approved whole"
        );
        let TrustDecision::PartiallyTrusted { approved, pending } =
            decide(&request, &store, &workspace)
        else {
            panic!("part of the request is decided");
        };
        assert_eq!(
            approved,
            ["start the MCP server `project-local`".to_owned()]
        );
        assert_eq!(pending.len(), 3);
        assert!(pending.iter().any(|entry| entry.contains("project-remote")));
        assert!(pending.iter().any(|entry| entry.contains("/opt/data")));
    }

    #[test]
    fn approving_every_entry_trusts_the_request() {
        let dir = tempdir();
        let workspace = workspace_with_mcp_json(&dir, MCP_JSON);
        let request = request();
        let mut store = TrustStore::new();
        store.approve_entries(&workspace, &request.entries());
        assert_eq!(decide(&request, &store, &workspace), TrustDecision::Trusted);
    }

    #[test]
    fn a_pending_decision_blocks_the_entries_nobody_approved() {
        let dir = tempdir();
        let workspace = workspace_with_mcp_json(&dir, MCP_JSON);
        let decision = TrustDecision::PartiallyTrusted {
            approved: vec!["start the MCP server `project-local`".to_owned()],
            pending: vec!["start the MCP server `project-remote`".to_owned()],
        };
        assert!(!decision.is_untrusted());
        assert_eq!(
            decision.blocked_actions(),
            ["start the MCP server `project-remote`".to_owned()]
        );
        assert!(decision.prompt().is_empty());

        let err = pending_error(&workspace, decision.blocked_actions()).expect_err("pending");
        assert_eq!(err.code(), ErrorCode::InputRequired);
        assert!(err.message().contains("project-remote"));
        assert!(err.detail().hint.is_some());
        pending_error(&workspace, &[]).expect("nothing pending");
    }

    #[test]
    fn the_unapproved_error_names_every_blocked_action() {
        let dir = tempdir();
        let workspace = workspace_with_mcp_json(&dir, MCP_JSON);
        let decision = decide(&request(), &TrustStore::new(), &workspace);
        let err = unapproved_error(&workspace, &decision);
        assert_eq!(err.code(), ErrorCode::PermissionDenied);
        for blocked in decision.blocked_actions() {
            assert!(
                err.message().contains(blocked),
                "`{blocked}` is named in: {}",
                err.message()
            );
        }
        assert!(err.detail().hint.is_some());
    }

    #[test]
    fn an_empty_request_describes_nothing() {
        let request = ProjectRequest::default();
        assert!(request.entries().is_empty());
        assert!(request.describe().is_empty());
        assert!(request.blocked_actions().is_empty());
    }

    #[test]
    fn the_store_round_trips_through_json() {
        let dir = tempdir();
        let workspace = workspace_with_mcp_json(&dir, MCP_JSON);
        let mut store = TrustStore::new();
        store.approve_entries(&workspace, &["server:project-local".to_owned()]);
        store.reject(Utf8Path::new("/elsewhere"));

        let encoded = serde_json::to_string(&store).expect("encode");
        let decoded: TrustStore = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(
            decoded.approved_entries(&workspace),
            ["server:project-local".to_owned()]
        );
        assert_eq!(
            decoded.decision(Utf8Path::new("/elsewhere")),
            Some(WorkspaceTrust::Rejected)
        );
        assert_eq!(decoded.entries().len(), 2);
    }
}
