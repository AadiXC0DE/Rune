//! Session trees: one immutable history DAG per session, with named branches.
//!
//! A turn is stored once. A branch is a position in the DAG rather than a copy
//! of it, so two branches that share a prefix share the nodes in it and a fork
//! costs one name. That is what makes exploring an alternative line of work
//! cheap, and it is what makes the accounting honest: a shared turn is one
//! stored node, counted once by the tree total and once by every branch whose
//! path runs through it.
//!
//! Nothing here touches the filesystem. Rewinding and switching move a pointer,
//! they never rewrite a node, delete a turn, or reach for a workspace, so
//! re-reading an earlier point of a conversation cannot disturb the work that
//! was done at it.

use std::collections::BTreeMap;
use std::fmt;

use rune_core::error::{ErrorCode, Result, RuneError};
use serde::{Deserialize, Serialize};

/// Schema version written by this build.
pub const SCHEMA_VERSION: u32 = 1;

/// Branch a session starts on.
pub const MAIN_BRANCH: &str = "main";

/// Longest preview kept for one turn, in characters.
pub const MAX_PREVIEW_CHARS: usize = 200;

/// Longest accepted branch name, in bytes.
pub const MAX_BRANCH_NAME: usize = 64;

/// Largest accepted encoded tree, in bytes.
pub const MAX_TREE_BYTES: usize = 64 * 1024 * 1024;

/// Largest ordinal tried when a branch name is derived from another.
pub const MAX_DERIVED_ORDINAL: usize = 1_000;

/// Who produced a turn.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Operator instructions.
    System,
    /// The person using the product.
    User,
    /// The model.
    Assistant,
    /// A tool result.
    Tool,
}

impl Role {
    /// Every role, in a stable order.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[Self::System, Self::User, Self::Assistant, Self::Tool]
    }

    /// Returns the wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One turn of a session, as the tree stores it.
///
/// Position, parent, and branch are owned by the tree: a node built here has no
/// place in a DAG until [`Tree::append`] gives it one.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Node {
    /// Position in the tree, starting at one.
    pub seq: u64,
    /// Turn this one follows, absent only for the root.
    pub parent: Option<u64>,
    /// Branch the turn was recorded on.
    pub branch: String,
    /// Who produced the turn.
    pub role: Role,
    /// One-line preview of the turn.
    pub preview: String,
    /// Encoded size of the turn's payload.
    pub bytes: usize,
    /// Milliseconds since the Unix epoch when the turn was recorded.
    pub created_at_ms: i64,
}

impl Node {
    /// Builds a turn before the tree places it.
    #[must_use]
    pub fn new(role: Role, preview: impl Into<String>, bytes: usize, created_at_ms: i64) -> Self {
        Self {
            seq: 0,
            parent: None,
            branch: String::new(),
            role,
            preview: bounded_preview(preview.into()),
            bytes,
            created_at_ms,
        }
    }

    /// Names the branch this turn joins.
    ///
    /// Without a name the tree decides, which for a turn appended at the tip of
    /// the active branch is that branch.
    #[must_use]
    pub fn with_branch(mut self, branch: impl Into<String>) -> Self {
        self.branch = branch.into();
        self
    }
}

/// A named path through the tree, as a listing shows it.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Branch {
    /// Branch name.
    pub name: String,
    /// Newest turn on the branch, absent while it holds none.
    pub head_seq: Option<u64>,
    /// Turn the branch departs from, absent for the branch that roots the tree.
    pub divergence_seq: Option<u64>,
    /// Turns on the branch's path, the shared prefix included.
    pub turn_count: u64,
    /// Preview of the turn the branch currently ends at.
    pub summary: String,
}

/// Turns and bytes accounted to a branch or to a whole tree.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Turns counted.
    pub turns: u64,
    /// Payload bytes counted.
    pub bytes: u64,
}

/// Where a branch currently sits.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
struct BranchState {
    /// Turn the next append joins, which a rewind moves backward.
    pointer: Option<u64>,
    /// Newest turn on the branch, which only ever moves forward.
    head: Option<u64>,
    /// Turn the branch departs from, absent for the branch that roots the tree.
    divergence: Option<u64>,
}

/// One branch as it is encoded.
#[derive(Serialize, Deserialize)]
struct WireBranch {
    name: String,
    #[serde(flatten)]
    state: BranchState,
}

/// The encoded tree document.
#[derive(Serialize, Deserialize)]
struct WireTree {
    schema_version: u32,
    next_seq: u64,
    root: Option<u64>,
    active: String,
    nodes: Vec<Node>,
    branches: Vec<WireBranch>,
}

/// An immutable turn history with named branches.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Tree {
    nodes: BTreeMap<u64, Node>,
    /// Children in append order, so a tree renders the way it was built.
    children: BTreeMap<u64, Vec<u64>>,
    root: Option<u64>,
    branches: BTreeMap<String, BranchState>,
    active: String,
    next_seq: u64,
}

impl Tree {
    /// Builds an empty tree holding one empty branch.
    #[must_use]
    pub fn new() -> Self {
        Self {
            nodes: BTreeMap::new(),
            children: BTreeMap::new(),
            root: None,
            branches: BTreeMap::from([(MAIN_BRANCH.to_owned(), BranchState::default())]),
            active: MAIN_BRANCH.to_owned(),
            next_seq: 1,
        }
    }

    /// Returns the number of turns stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Returns true when no turn is stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Returns the root turn, absent while the tree is empty.
    #[must_use]
    pub const fn root(&self) -> Option<u64> {
        self.root
    }

    /// Returns one turn.
    #[must_use]
    pub fn node(&self, seq: u64) -> Option<&Node> {
        self.nodes.get(&seq)
    }

    /// Returns the turns recorded directly after `seq`, in append order.
    ///
    /// More than one is the point of the DAG: it marks where two branches part.
    #[must_use]
    pub fn children(&self, seq: u64) -> Vec<u64> {
        self.children.get(&seq).cloned().unwrap_or_default()
    }

    /// Returns the branch the next append joins.
    #[must_use]
    pub fn active_branch(&self) -> &str {
        &self.active
    }

    /// Returns the turn the active branch is positioned at.
    #[must_use]
    pub fn pointer(&self) -> Option<u64> {
        self.branches
            .get(&self.active)
            .and_then(|state| state.pointer)
    }

    /// Returns the root-to-`seq` path, empty when the turn is unknown.
    #[must_use]
    pub fn path_to(&self, seq: u64) -> Vec<u64> {
        let mut path = Vec::new();
        let mut cursor = Some(seq);
        while let Some(current) = cursor {
            // The length bound only matters for a tree that never came from
            // `append`, where a parent chain is guaranteed to shorten.
            if path.len() >= self.nodes.len() {
                break;
            }
            let Some(node) = self.nodes.get(&current) else {
                break;
            };
            path.push(current);
            cursor = node.parent;
        }
        path.reverse();
        path
    }

    /// Returns every branch, ordered by name.
    #[must_use]
    pub fn branches(&self) -> Vec<Branch> {
        self.branches
            .iter()
            .map(|(name, state)| self.branch_view(name, *state))
            .collect()
    }

    /// Returns one branch.
    #[must_use]
    pub fn branch(&self, name: &str) -> Option<Branch> {
        self.branches
            .get(name)
            .map(|state| self.branch_view(name, *state))
    }

    /// Appends one turn under `parent`, returning its sequence number.
    ///
    /// The node's own sequence, parent, and branch are replaced: positions
    /// belong to the tree, so two callers cannot disagree about one.
    ///
    /// The branch is resolved in this order:
    ///
    /// 1. A name on the node wins. Naming a branch that is not positioned at
    ///    the parent is refused, because joining it there would make the branch
    ///    two paths instead of one.
    /// 2. With no parent the turn roots the tree on [`MAIN_BRANCH`].
    /// 3. The active branch takes the turn when it is positioned at the parent
    ///    and the parent is its tip.
    /// 4. Otherwise the append cannot join the active branch, and a new branch
    ///    is created beside it. This is what makes a rewind or a repeated fork
    ///    usable: the turn that follows does not land on top of the turn that
    ///    was already there.
    pub fn append(&mut self, parent: Option<u64>, node: Node) -> Result<u64> {
        if let Some(seq) = parent
            && !self.nodes.contains_key(&seq)
        {
            return Err(unknown_turn(seq));
        }
        if parent.is_none() && self.root.is_some() {
            return Err(RuneError::invalid_field(
                "parent",
                "the tree already has a root, so every turn follows another",
            )
            .with_hint("append under the turn this one follows"));
        }

        let branch = self.resolve_branch(parent, &node)?;
        let seq = self.next_seq;
        let mut node = node;
        node.seq = seq;
        node.parent = parent;
        node.branch.clone_from(&branch);
        node.preview = bounded_preview(node.preview);
        self.nodes.insert(seq, node);

        match parent {
            Some(parent) => self.children.entry(parent).or_default().push(seq),
            None => self.root = Some(seq),
        }

        let left_behind = parent
            .and_then(|parent| self.nodes.get(&parent))
            .map(|node| node.branch.clone())
            .filter(|owner| *owner != branch);
        let state = self.branches.entry(branch.clone()).or_insert(BranchState {
            pointer: None,
            head: None,
            divergence: parent,
        });
        state.pointer = Some(seq);
        state.head = Some(seq);
        if let Some(owner) = left_behind
            && let Some(owner_state) = self.branches.get_mut(&owner)
        {
            // The position the new branch left from belongs to the new branch.
            owner_state.pointer = owner_state.head;
        }

        self.active = branch;
        self.next_seq = self.next_seq.saturating_add(1);
        Ok(seq)
    }

    /// Creates a branch that shares the history through `at_seq`.
    ///
    /// No turn is copied or moved: the new branch starts where the tree already
    /// is, and returns that turn as its head.
    pub fn fork(&mut self, at_seq: u64, name: &str) -> Result<u64> {
        let name = checked_branch_name(name)?;
        if !self.nodes.contains_key(&at_seq) {
            return Err(unknown_turn(at_seq));
        }
        if self.branches.contains_key(name) {
            return Err(RuneError::new(
                ErrorCode::AlreadyExists,
                format!("branch `{name}` already exists"),
            )
            .with_hint("switch to the branch, or fork under another name"));
        }

        self.branches.insert(
            name.to_owned(),
            BranchState {
                pointer: Some(at_seq),
                head: Some(at_seq),
                divergence: Some(at_seq),
            },
        );
        name.clone_into(&mut self.active);
        Ok(at_seq)
    }

    /// Moves the active branch's position back to `to_seq`, deleting nothing.
    ///
    /// Only a turn on the active branch's own path can be rewound to; reaching
    /// a turn that belongs to another branch is a switch, not a rewind.
    pub fn rewind(&mut self, to_seq: u64) -> Result<()> {
        if !self.nodes.contains_key(&to_seq) {
            return Err(unknown_turn(to_seq));
        }
        let name = self.active.clone();
        let head = self.branches.get(&name).and_then(|state| state.head);
        let Some(head) = head else {
            return Err(RuneError::new(
                ErrorCode::InvalidState,
                format!("branch `{name}` holds no turn to rewind"),
            ));
        };
        if !self.path_to(head).contains(&to_seq) {
            return Err(RuneError::invalid_field(
                "to_seq",
                format!("turn {to_seq} is not on branch `{name}`"),
            )
            .with_hint("switch to the branch that holds it"));
        }
        if let Some(state) = self.branches.get_mut(&name) {
            state.pointer = Some(to_seq);
        }
        Ok(())
    }

    /// Makes `branch` active, restoring the position it was left at.
    pub fn switch(&mut self, branch: &str) -> Result<()> {
        if !self.branches.contains_key(branch) {
            return Err(RuneError::new(
                ErrorCode::NotFound,
                format!("branch `{branch}` is not in the tree"),
            )
            .with_hint("list the branches to see the names in use"));
        }
        branch.clone_into(&mut self.active);
        Ok(())
    }

    /// Returns the turns and bytes of one branch's path.
    pub fn usage_for(&self, branch: &str) -> Result<Usage> {
        let state = self.branches.get(branch).ok_or_else(|| {
            RuneError::new(
                ErrorCode::NotFound,
                format!("branch `{branch}` is not in the tree"),
            )
            .with_hint("list the branches to see the names in use")
        })?;
        let Some(head) = state.head else {
            return Ok(Usage::default());
        };
        Ok(self.usage_of(&self.path_to(head)))
    }

    /// Returns the turns and bytes stored in the tree, counting each turn once.
    #[must_use]
    pub fn usage_total(&self) -> Usage {
        let mut usage = Usage::default();
        for node in self.nodes.values() {
            usage.turns = usage.turns.saturating_add(1);
            usage.bytes = usage.bytes.saturating_add(as_u64(node.bytes));
        }
        usage
    }

    /// Encodes the tree as one JSON document.
    pub fn encode(&self) -> Result<String> {
        let wire = WireTree {
            schema_version: SCHEMA_VERSION,
            next_seq: self.next_seq,
            root: self.root,
            active: self.active.clone(),
            nodes: self.nodes.values().cloned().collect(),
            branches: self
                .branches
                .iter()
                .map(|(name, state)| WireBranch {
                    name: name.clone(),
                    state: *state,
                })
                .collect(),
        };
        let text = serde_json::to_string_pretty(&wire)?;
        if text.len() > MAX_TREE_BYTES {
            return Err(
                RuneError::too_large("session_tree", text.len(), MAX_TREE_BYTES)
                    .with_invariant("tree_size"),
            );
        }
        Ok(text)
    }

    /// Decodes a tree, rejecting a schema this build cannot read.
    pub fn decode(text: &str) -> Result<Self> {
        if text.len() > MAX_TREE_BYTES {
            return Err(
                RuneError::too_large("session_tree", text.len(), MAX_TREE_BYTES)
                    .with_invariant("tree_size"),
            );
        }
        let wire: WireTree = serde_json::from_str(text).map_err(|cause| {
            RuneError::invariant(
                "tree_encoding",
                format!("a session tree could not be read: {cause}"),
            )
        })?;
        if wire.schema_version != SCHEMA_VERSION {
            return Err(RuneError::new(
                ErrorCode::UnsupportedVersion,
                format!(
                    "a session tree uses schema version {}, this build reads {SCHEMA_VERSION}",
                    wire.schema_version
                ),
            )
            .with_invariant("schema_version")
            .with_observed(format!("schema_version {}", wire.schema_version))
            .with_hint("upgrade Rune to read this tree, or rebuild it from the session log"));
        }
        Self::assemble(wire)
    }

    /// Rebuilds the maps and checks that every reference resolves.
    fn assemble(wire: WireTree) -> Result<Self> {
        let mut nodes: BTreeMap<u64, Node> = BTreeMap::new();
        for node in wire.nodes {
            if node.seq == 0 {
                return Err(
                    RuneError::invariant("tree_sequence", "a tree turn has sequence 0")
                        .with_observed("seq 0"),
                );
            }
            if nodes.insert(node.seq, node).is_some() {
                return Err(RuneError::invariant(
                    "tree_sequence",
                    "a tree holds the same sequence twice",
                ));
            }
        }

        let mut children: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        let mut root = None;
        for node in nodes.values() {
            match node.parent {
                // A parent below the child is what keeps a path finite; a tree
                // that broke it could not be rendered or walked.
                Some(parent) if parent >= node.seq => {
                    return Err(RuneError::invariant(
                        "tree_parent",
                        format!("turn {} follows turn {parent}", node.seq),
                    )
                    .with_observed(format!("seq {}", node.seq)));
                }
                Some(parent) if !nodes.contains_key(&parent) => {
                    return Err(RuneError::invariant(
                        "tree_parent",
                        format!(
                            "turn {} follows turn {parent}, which is not in the tree",
                            node.seq
                        ),
                    )
                    .with_observed(format!("seq {}", node.seq)));
                }
                Some(parent) => children.entry(parent).or_default().push(node.seq),
                None if root.is_some() => {
                    return Err(RuneError::invariant(
                        "tree_root",
                        "a tree holds more than one root",
                    ));
                }
                None => root = Some(node.seq),
            }
        }

        if let Some(last) = nodes.keys().next_back()
            && *last >= wire.next_seq
        {
            return Err(RuneError::invariant(
                "tree_sequence",
                "the next sequence is behind a turn",
            )
            .with_observed(format!("next_seq {}", wire.next_seq)));
        }

        let mut branches: BTreeMap<String, BranchState> = BTreeMap::new();
        for branch in wire.branches {
            let name = checked_branch_name(&branch.name)?.to_owned();
            for reference in [
                branch.state.pointer,
                branch.state.head,
                branch.state.divergence,
            ]
            .into_iter()
            .flatten()
            {
                if !nodes.contains_key(&reference) {
                    return Err(RuneError::invariant(
                        "tree_branch",
                        format!(
                            "branch `{name}` refers to turn {reference}, which is not in the tree"
                        ),
                    )
                    .with_observed(format!("branch {name}")));
                }
            }
            branches.insert(name, branch.state);
        }
        for node in nodes.values() {
            if !branches.contains_key(&node.branch) {
                return Err(RuneError::invariant(
                    "tree_branch",
                    format!(
                        "turn {} is on branch `{}`, which is not declared",
                        node.seq, node.branch
                    ),
                )
                .with_observed(format!("seq {}", node.seq)));
            }
        }
        if !branches.contains_key(&wire.active) {
            return Err(RuneError::invariant(
                "tree_branch",
                format!("the active branch `{}` is not declared", wire.active),
            )
            .with_observed(format!("branch {}", wire.active)));
        }

        Ok(Self {
            nodes,
            children,
            root,
            branches,
            active: wire.active,
            next_seq: wire.next_seq,
        })
    }

    /// Resolves the branch an append joins.
    fn resolve_branch(&self, parent: Option<u64>, node: &Node) -> Result<String> {
        let explicit = node.branch.trim();
        if !explicit.is_empty() {
            let name = checked_branch_name(explicit)?.to_owned();
            if let Some(state) = self.branches.get(&name)
                && state.pointer != parent
            {
                return Err(RuneError::invalid_field(
                    "branch",
                    format!(
                        "branch `{name}` is positioned at {}, not at {}",
                        position(state.pointer),
                        position(parent)
                    ),
                )
                .with_hint("switch to the branch, or append where it is positioned"));
            }
            return Ok(name);
        }

        let Some(parent) = parent else {
            return Ok(MAIN_BRANCH.to_owned());
        };
        let active = self.branches.get(&self.active).copied().unwrap_or_default();
        if active.pointer == Some(parent) && (active.head == Some(parent) || active.head.is_none())
        {
            return Ok(self.active.clone());
        }
        let owner = self
            .nodes
            .get(&parent)
            .map_or(MAIN_BRANCH, |node| node.branch.as_str());
        self.derived_name(owner)
    }

    /// Names a new branch beside `base`.
    ///
    /// The search is bounded so a caller cannot spin here; a name that cannot
    /// be found within the bound is reported rather than silently reused, which
    /// would put two lines of history on one branch.
    fn derived_name(&self, base: &str) -> Result<String> {
        for ordinal in 1..=MAX_DERIVED_ORDINAL {
            let candidate = format!("{base}-{ordinal}");
            if !self.branches.contains_key(&candidate) {
                return Ok(candidate);
            }
        }
        Err(RuneError::new(
            ErrorCode::LimitExceeded,
            format!("every branch name derived from `{base}` is taken"),
        )
        .with_hint("name the branch explicitly"))
    }

    /// Builds the listing view of one branch.
    fn branch_view(&self, name: &str, state: BranchState) -> Branch {
        let path = state.head.map_or_else(Vec::new, |seq| self.path_to(seq));
        let summary = state
            .head
            .and_then(|seq| self.nodes.get(&seq))
            .map_or_else(String::new, |node| node.preview.clone());
        Branch {
            name: name.to_owned(),
            head_seq: state.head,
            divergence_seq: state.divergence,
            turn_count: as_u64(path.len()),
            summary,
        }
    }

    /// Sums the turns and bytes of a path.
    fn usage_of(&self, path: &[u64]) -> Usage {
        let mut usage = Usage::default();
        for seq in path {
            if let Some(node) = self.nodes.get(seq) {
                usage.turns = usage.turns.saturating_add(1);
                usage.bytes = usage.bytes.saturating_add(as_u64(node.bytes));
            }
        }
        usage
    }
}

impl Default for Tree {
    fn default() -> Self {
        Self::new()
    }
}

/// Returns the error reported for a turn the tree does not hold.
fn unknown_turn(seq: u64) -> RuneError {
    RuneError::new(
        ErrorCode::NotFound,
        format!("turn {seq} is not in the tree"),
    )
    .with_hint("append to or fork from a turn the tree already holds")
}

/// Describes a position in a message.
fn position(seq: Option<u64>) -> String {
    seq.map_or_else(|| "the start".to_owned(), |seq| format!("turn {seq}"))
}

/// Rejects a branch name that is empty, oversized, or outside the alphabet.
fn checked_branch_name(raw: &str) -> Result<&str> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(RuneError::invalid_field("branch", "must not be empty"));
    }
    if name.len() > MAX_BRANCH_NAME {
        return Err(RuneError::invalid_field(
            "branch",
            format!("`{name}` exceeds {MAX_BRANCH_NAME} bytes"),
        ));
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(RuneError::invalid_field(
            "branch",
            format!("`{name}` contains characters outside the accepted set"),
        )
        .with_hint("branch names use letters, digits, `-`, `_`, and `.`"));
    }
    // A name of only dots is a relative path component. It passes the character
    // check but would resolve somewhere other than a branch directory, so it is
    // refused rather than normalized.
    if name.bytes().all(|byte| byte == b'.') {
        return Err(RuneError::invalid_field(
            "branch",
            format!("`{name}` is a relative path component"),
        )
        .with_hint("use at least one letter or digit"));
    }
    Ok(name)
}

/// Truncates a preview to the bound, leaving a short one alone.
fn bounded_preview(text: String) -> String {
    if text.chars().count() <= MAX_PREVIEW_CHARS {
        return text;
    }
    text.chars().take(MAX_PREVIEW_CHARS).collect()
}

/// Converts a length to the type used in totals.
fn as_u64(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    use camino::{Utf8Path, Utf8PathBuf};
    use sha2::{Digest, Sha256};

    fn turn(role: Role, preview: &str) -> Node {
        Node::new(role, preview, preview.len(), 1_000)
    }

    fn user(preview: &str) -> Node {
        turn(Role::User, preview)
    }

    /// Builds a tree with three turns on `main`.
    fn seeded() -> Tree {
        let mut tree = Tree::new();
        let mut parent = None;
        for preview in ["first", "second", "third"] {
            parent = Some(tree.append(parent, user(preview)).expect("append"));
        }
        tree
    }

    #[test]
    fn a_fork_shares_the_prefix_and_each_branch_keeps_its_own_history() {
        let mut tree = seeded();
        assert_eq!(tree.fork(3, "alpha").expect("fork"), 3);
        assert_eq!(tree.append(Some(3), user("alpha work")).expect("append"), 4);
        assert_eq!(tree.fork(3, "beta").expect("fork"), 3);
        assert_eq!(tree.append(Some(3), user("beta work")).expect("append"), 5);
        assert_eq!(tree.append(Some(5), user("beta more")).expect("append"), 6);

        // A branch is a path through the DAG: the shared prefix is the same
        // three turns in both paths, not a copy in either.
        assert_eq!(tree.path_to(4), vec![1, 2, 3, 4]);
        assert_eq!(tree.path_to(6), vec![1, 2, 3, 5, 6]);
        assert_eq!(tree.path_to(4)[..3], tree.path_to(6)[..3]);
        assert_eq!(tree.len(), 6);
        assert_eq!(tree.node(4).expect("node").branch, "alpha");
        assert_eq!(tree.node(6).expect("node").branch, "beta");
        assert_eq!(tree.children(3), vec![4, 5]);

        let names: Vec<String> = tree.branches().into_iter().map(|b| b.name).collect();
        assert_eq!(names, vec!["alpha", "beta", "main"]);
        assert_eq!(tree.active_branch(), "beta");
        assert_eq!(tree.pointer(), Some(6));

        tree.switch("alpha").expect("switch");
        assert_eq!(tree.active_branch(), "alpha");
        assert_eq!(tree.pointer(), Some(4));
        assert_eq!(tree.append(Some(4), user("alpha more")).expect("append"), 7);
        assert_eq!(tree.path_to(7), vec![1, 2, 3, 4, 7]);

        // Each branch reports the path it stands for, shared prefix included.
        let alpha = tree.branch("alpha").expect("branch");
        assert_eq!(alpha.head_seq, Some(7));
        assert_eq!(alpha.divergence_seq, Some(3));
        assert_eq!(alpha.turn_count, 5);
        assert_eq!(alpha.summary, "alpha more");
        let main = tree.branch("main").expect("branch");
        assert_eq!(main.head_seq, Some(3));
        assert_eq!(main.divergence_seq, None);
        assert_eq!(main.turn_count, 3);
        assert_eq!(main.summary, "third");
    }

    #[test]
    fn a_rewind_moves_the_pointer_and_the_next_append_starts_a_branch() {
        let mut tree = seeded();
        tree.rewind(2).expect("rewind");
        assert_eq!(tree.pointer(), Some(2));
        assert_eq!(tree.node(3), Some(&tree.node(3).expect("node").clone()));
        assert_eq!(tree.path_to(3), vec![1, 2, 3]);

        let seq = tree.append(Some(2), user("instead")).expect("append");
        assert_eq!(seq, 4);
        assert_eq!(tree.active_branch(), "main-1");
        assert_eq!(tree.path_to(4), vec![1, 2, 4]);

        // The turn that was already under this parent is untouched, and the
        // branch it belongs to is back at its own tip.
        assert_eq!(tree.node(3).expect("node").preview, "third");
        assert_eq!(tree.node(3).expect("node").branch, "main");
        assert_eq!(tree.branch("main").expect("branch").head_seq, Some(3));
        assert_eq!(tree.pointer(), Some(4));

        tree.switch("main").expect("switch");
        assert_eq!(tree.pointer(), Some(3));
        assert_eq!(tree.append(Some(3), user("after")).expect("append"), 5);
        assert_eq!(tree.path_to(5), vec![1, 2, 3, 5]);
        assert_eq!(tree.path_to(4), vec![1, 2, 4]);
    }

    #[test]
    fn rewinding_to_another_branch_is_refused() {
        let mut tree = seeded();
        tree.fork(2, "alpha").expect("fork");
        tree.append(Some(2), user("alpha work")).expect("append");
        tree.switch("main").expect("switch");

        let err = tree.rewind(4).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.field(), Some("to_seq"));

        tree.rewind(3).expect("the branch's own tip");
        assert_eq!(tree.pointer(), Some(3));
    }

    #[test]
    fn usage_counts_a_shared_turn_once_in_the_total_and_once_per_path() {
        let mut tree = seeded();
        tree.fork(3, "alpha").expect("fork");
        tree.append(Some(3), user("abcdef")).expect("append");

        let total = tree.usage_total();
        assert_eq!(total.turns, 4);
        assert_eq!(total.bytes, "firstsecondthirdabcdef".len() as u64);

        let main = tree.usage_for("main").expect("usage");
        assert_eq!(main.turns, 3);
        assert_eq!(main.bytes, "firstsecondthird".len() as u64);

        let alpha = tree.usage_for("alpha").expect("usage");
        assert_eq!(alpha.turns, 4);
        assert_eq!(alpha.bytes, "firstsecondthirdabcdef".len() as u64);

        let err = tree.usage_for("missing").expect_err("missing branch");
        assert_eq!(err.code(), ErrorCode::NotFound);
    }

    #[test]
    fn a_switch_and_a_rewind_leave_the_workspace_untouched() {
        let root = tempfile::tempdir().expect("tempdir");
        let workspace = Utf8PathBuf::from_path_buf(root.path().join("workspace")).expect("utf8");
        std::fs::create_dir_all(workspace.join("nested")).expect("create");
        std::fs::write(workspace.join("a.txt"), "alpha\n").expect("write");
        std::fs::write(workspace.join("nested/b.txt"), "beta\n").expect("write");
        let before = hash_tree(&workspace);

        let mut tree = seeded();
        tree.fork(3, "alpha").expect("fork");
        tree.append(Some(3), user("alpha work")).expect("append");
        tree.rewind(1).expect("rewind");
        tree.switch("main").expect("switch");
        tree.append(Some(3), user("more")).expect("append");
        let encoded = tree.encode().expect("encode");
        let decoded = Tree::decode(&encoded).expect("decode");

        // The workspace is the subject, so the tree work has to be real: two
        // branches and five turns means the fork, the append on the branch, the
        // rewind, the switch, and the append after it all happened.
        assert_eq!(decoded.branches().len(), 2, "the test did nothing");
        assert_eq!(decoded.len(), 5, "the test did nothing");
        assert_eq!(hash_tree(&workspace), before);
        assert!(workspace.join("nested/b.txt").is_file());
    }

    #[test]
    fn a_tree_round_trips_through_json() {
        let mut tree = seeded();
        tree.fork(2, "alpha").expect("fork");
        tree.append(Some(2), user("work")).expect("append");
        tree.rewind(2).expect("rewind");

        let encoded = tree.encode().expect("encode");
        assert!(encoded.contains("\"schema_version\": 1"), "{encoded}");
        let decoded = Tree::decode(&encoded).expect("decode");
        assert_eq!(decoded, tree);
        assert_eq!(decoded.active_branch(), tree.active_branch());
        assert_eq!(decoded.pointer(), Some(2));
    }

    #[test]
    fn an_unreadable_schema_version_is_refused() {
        let text = Tree::new()
            .encode()
            .expect("encode")
            .replace("\"schema_version\": 1", "\"schema_version\": 7");
        let err = Tree::decode(&text).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::UnsupportedVersion);
        assert_eq!(err.detail().invariant.as_deref(), Some("schema_version"));
        assert!(err.detail().hint.is_some());
    }

    #[test]
    fn a_dangling_reference_is_refused() {
        let mut tree = seeded();
        tree.fork(3, "alpha").expect("fork");
        let encoded = tree.encode().expect("encode");
        let document: serde_json::Value = serde_json::from_str(&encoded).expect("parsed");

        // Mutating the parsed document rather than replacing text keeps the case
        // tied to the semantics under test instead of to the serializer's
        // spacing.
        let mut missing_parent = document.clone();
        if let Some(node) = missing_parent["nodes"]
            .as_array_mut()
            .and_then(|nodes| nodes.iter_mut().find(|node| node["seq"] == 2))
        {
            node["parent"] = serde_json::json!(9);
        }
        let err = Tree::decode(&missing_parent.to_string()).expect_err("refused");
        assert_eq!(err.detail().invariant.as_deref(), Some("tree_parent"));

        // A branch reference to a turn that is not in the tree fails only once
        // the node graph itself is sound, so this case is reached after the two
        // node cases above.
        let mut missing_branch = document.clone();
        if let Some(branch) = missing_branch["branches"]
            .as_array_mut()
            .and_then(|branches| branches.iter_mut().find(|branch| branch["name"] == "alpha"))
        {
            branch["head"] = serde_json::json!(99);
        }
        let err = Tree::decode(&missing_branch.to_string()).expect_err("refused");
        assert_eq!(err.detail().invariant.as_deref(), Some("tree_branch"));

        // A parent at or above the child would make a path non-terminating.
        let mut forward = document.clone();
        if let Some(node) = forward["nodes"]
            .as_array_mut()
            .and_then(|nodes| nodes.iter_mut().find(|node| node["seq"] == 1))
        {
            node["parent"] = serde_json::json!(3);
        }
        let err = Tree::decode(&forward.to_string()).expect_err("refused");
        assert_eq!(err.detail().invariant.as_deref(), Some("tree_parent"));

        assert!(Tree::decode(&encoded).is_ok());
    }

    #[test]
    fn an_append_is_refused_when_it_cannot_join_the_named_branch() {
        let mut tree = seeded();
        tree.fork(2, "alpha").expect("fork");
        assert_eq!(tree.append(Some(2), user("work")).expect("append"), 4);

        let err = tree
            .append(Some(3), user("elsewhere").with_branch("alpha"))
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.field(), Some("branch"));

        let err = tree
            .append(Some(9), user("nowhere"))
            .expect_err("unknown parent");
        assert_eq!(err.code(), ErrorCode::NotFound);
    }

    #[test]
    fn an_empty_tree_has_one_empty_branch_and_one_root() {
        let mut tree = Tree::new();
        assert!(tree.is_empty());
        assert_eq!(tree.root(), None);
        assert_eq!(tree.branches().len(), 1);
        let main = tree.branch(MAIN_BRANCH).expect("branch");
        assert_eq!(main.head_seq, None);
        assert_eq!(main.turn_count, 0);
        assert!(main.summary.is_empty());
        assert_eq!(tree.usage_total(), Usage::default());

        let first = tree.append(None, user("root")).expect("append");
        assert_eq!(first, 1);
        assert_eq!(tree.root(), Some(1));
        assert_eq!(tree.branch(MAIN_BRANCH).expect("branch").head_seq, Some(1));

        let err = tree.append(None, user("second root")).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.field(), Some("parent"));

        let err = tree.switch("missing").expect_err("refused");
        assert_eq!(err.code(), ErrorCode::NotFound);
        let err = tree.fork(9, "alpha").expect_err("refused");
        assert_eq!(err.code(), ErrorCode::NotFound);
        let err = tree.fork(1, "main").expect_err("refused");
        assert_eq!(err.code(), ErrorCode::AlreadyExists);
        let err = tree.fork(1, "not a name").expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
    }

    #[test]
    fn a_preview_is_bounded_and_a_turn_is_stored_where_it_was_appended() {
        let long = "x".repeat(MAX_PREVIEW_CHARS.saturating_add(50));
        let mut tree = Tree::new();
        let seq = tree
            .append(None, Node::new(Role::Assistant, long, 0, -5))
            .expect("append");

        let node = tree.node(seq).expect("node");
        assert_eq!(node.preview.chars().count(), MAX_PREVIEW_CHARS);
        assert_eq!(node.created_at_ms, -5);
        assert_eq!(node.role, Role::Assistant);
        assert_eq!(node.parent, None);

        // A caller cannot smuggle a position or a branch in: a node carrying
        // either is refused rather than silently corrected, because accepting it
        // would mean the stored tree differs from what the caller believed it
        // appended.
        let err = tree
            .append(
                Some(seq),
                Node {
                    seq: 99,
                    parent: None,
                    branch: "not the active branch".to_owned(),
                    ..Node::new(Role::User, "next", 4, 0)
                },
            )
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);

        // A node built by the constructor, which carries no position, is
        // assigned one by the tree.
        let assigned = tree
            .append(Some(seq), Node::new(Role::User, "next", 4, 0))
            .expect("append");
        assert_eq!(assigned, 2);
        assert!(tree.node(99).is_none());
        assert_eq!(tree.node(2).expect("node").parent, Some(1));
    }

    #[test]
    fn a_branch_name_is_accepted_by_the_shape_the_wire_writes() {
        assert_eq!(Role::all().len(), 4);
        for role in Role::all() {
            let json = serde_json::to_string(role).expect("serialize");
            assert_eq!(json, format!("\"{}\"", role.as_str()));
            assert_eq!(role.to_string(), role.as_str());
        }
        assert!(checked_branch_name("feature-1").is_ok());
        assert!(checked_branch_name("").is_err());
        assert!(checked_branch_name(&"x".repeat(MAX_BRANCH_NAME.saturating_add(1))).is_err());
        assert!(checked_branch_name("no spaces").is_err());
    }

    #[test]
    fn a_derived_name_never_collides_with_an_existing_branch() {
        let mut tree = seeded();
        // A branch named the way a derived name would be is taken, so the next
        // derived name has to skip it rather than overwrite it.
        tree.fork(1, "main-1").expect("fork");
        for _ in 0..MAX_DERIVED_ORDINAL {
            tree.fork(1, "extra").ok();
        }
        let derived = tree.fork(1, "derived").expect("fork");
        assert_eq!(derived, 1);
        assert!(tree.branch("main-1").is_some());
        assert!(tree.branch("derived").is_some());
    }

    #[test]
    fn a_branch_name_is_rejected_when_it_cannot_be_a_path_component() {
        // A branch name becomes part of a stored path, so anything that is not a
        // usable path component has to be refused rather than accepted and
        // normalized later.
        let mut tree = Tree::new();
        let first = tree.append(None, user("first")).expect("the first turn");

        for name in ["", "with/slash", "with\\slash", "..", "."] {
            let err = tree.fork(first, name).expect_err("refused");
            assert_eq!(
                err.code(),
                ErrorCode::InvalidField,
                "branch name `{name}` was accepted"
            );
        }

        // A plain identifier is accepted.
        assert!(tree.fork(first, "review").is_ok());
    }

    /// Hashes every file under a directory, path and contents together.
    fn hash_tree(root: &Utf8Path) -> String {
        let mut files: Vec<Utf8PathBuf> = Vec::new();
        collect(root, &mut files);
        files.sort();
        let mut hasher = Sha256::new();
        for path in files {
            hasher.update(path.as_str().as_bytes());
            hasher.update(std::fs::read(&path).expect("read"));
        }
        format!("{:x}", hasher.finalize())
    }

    /// Collects the files below a directory.
    fn collect(dir: &Utf8Path, out: &mut Vec<Utf8PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read dir") {
            let path = Utf8PathBuf::from_path_buf(entry.expect("entry").path()).expect("utf8");
            if path.is_dir() {
                collect(&path, out);
            } else {
                out.push(path);
            }
        }
    }
}
