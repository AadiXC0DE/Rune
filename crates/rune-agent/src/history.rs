//! Conversation history.
//!
//! Holds the durable turns of one conversation and enforces the ordering rules a
//! provider request depends on. The `Message` type in `rune-net` is the wire
//! shape; this is the owner of the sequence, and it is what compaction, forking,
//! and session persistence operate on.

use std::sync::Arc;

use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::id::ToolCallId;
use rune_net::message::{ContentPart, Message, Role, validate};
use rune_net::provider::Provider;
use rune_tools::result_store::Store;
use serde::{Deserialize, Serialize};

/// One turn in the conversation.
///
/// A turn is what the user or the model contributed at one point, kept separate
/// from the merged message list so the transcript can be rendered and the
/// history can be forked without re-deriving either.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Turn {
    /// Monotonic position, starting at one.
    pub seq: u64,
    /// Who produced it.
    pub role: Role,
    /// Body parts.
    pub parts: Vec<ContentPart>,
    /// Provider state to replay with this turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<String>,
    /// The `dialect:model` that produced an assistant turn; none for other
    /// turns and for sessions saved before it was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

impl Turn {
    /// Builds a turn at a given position.
    #[must_use]
    pub fn new(seq: u64, role: Role, parts: Vec<ContentPart>) -> Self {
        Self {
            seq,
            role,
            parts,
            replay: None,
            origin: None,
        }
    }

    /// Builds a user turn.
    #[must_use]
    pub fn user(seq: u64, text: impl Into<String>) -> Self {
        Self::new(
            seq,
            Role::User,
            vec![ContentPart::Text { text: text.into() }],
        )
    }

    /// Builds an assistant turn.
    #[must_use]
    pub fn assistant(seq: u64, parts: Vec<ContentPart>) -> Self {
        Self::new(seq, Role::Assistant, parts)
    }

    /// Returns the concatenated text of this turn.
    #[must_use]
    pub fn text(&self) -> String {
        let mut out = String::new();
        for part in &self.parts {
            if let ContentPart::Text { text } = part {
                out.push_str(text);
            }
        }
        out
    }

    /// Returns the tool calls in this turn.
    #[must_use]
    pub fn tool_calls(&self) -> Vec<(&ToolCallId, &str, &str)> {
        self.parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::ToolCall {
                    id,
                    name,
                    arguments,
                } => Some((id, name.as_str(), arguments.as_str())),
                _ => None,
            })
            .collect()
    }

    /// Returns true when the turn carries no content.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// Estimates the byte size of this turn's content.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        let mut total: usize = 0;
        for part in &self.parts {
            total = total.saturating_add(match part {
                ContentPart::Text { text } | ContentPart::Reasoning { text } => text.len(),
                ContentPart::ToolCall {
                    arguments, name, ..
                } => arguments.len().saturating_add(name.len()),
                ContentPart::ToolResult { content, name, .. } => {
                    content.len().saturating_add(name.len())
                }
                ContentPart::Image { .. } => 0,
            });
        }
        total
    }
}

/// The conversation history.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct History {
    /// System instructions, separate from the turn list.
    instructions: String,
    /// Explicit user constraints, retained independently of summarized turns.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pinned_constraints: Vec<String>,
    /// Turns in order.
    turns: Vec<Turn>,
    /// Next sequence number.
    next_seq: u64,
    /// Retained output lives as long as this in-memory conversation. It is not
    /// part of a provider request or a serialized transcript.
    #[serde(skip, default = "new_result_store")]
    result_store: Arc<Store>,
    /// Plan state shares the lifetime of this live conversation, not its registry.
    #[serde(skip)]
    live_plan: Arc<std::sync::Mutex<rune_tools::PlanUpdate>>,
    /// A digest of each file a successful `read_file` call read, by call id,
    /// taken when the result was kept. A later identical read of a file whose
    /// digest is unchanged can point at that result instead of running.
    #[serde(skip)]
    read_digests: std::collections::BTreeMap<String, [u8; 32]>,
    /// Digest of each file as the model last saw it: after its last successful
    /// read, write, edit or patch of that path. Keyed by resolved path. Not
    /// persisted: a resumed session starts with none.
    #[serde(skip)]
    file_views: std::collections::BTreeMap<camino::Utf8PathBuf, [u8; 32]>,
    /// Digests of `write_file` calls refused once by the overwrite guard.
    #[serde(skip)]
    refused_overwrites: std::collections::BTreeSet<[u8; 32]>,
    /// The background generation (`rune_tools::shell::background`) when each
    /// repeatable check command finished, by call id.
    #[serde(skip)]
    check_generations: std::collections::BTreeMap<String, u64>,
    /// Reported input of the latest completed request, for the next estimate.
    #[serde(skip)]
    anchor: Option<crate::tokens::Anchor>,
    /// Files, checks and shells the turn loop saw, kept across compaction.
    #[serde(default)]
    ledger: crate::ledger::Ledger,
    /// Call ids whose result was replaced by a receipt.
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pruned: std::collections::BTreeSet<String>,
    /// The tool-result turn whose checks last settled the ledger.
    #[serde(skip)]
    settled_turn: Option<u64>,
    /// Set once the provider refused reasoning summaries; later requests go
    /// without them. Not persisted: a resumed session asks again.
    #[serde(skip)]
    reasoning_summary_refused: bool,
    /// Set once the user was told their temperature is not sent. Not persisted.
    #[serde(skip)]
    temperature_noticed: bool,
    /// The smallest request a provider rejected as too long, in estimated
    /// tokens, with the model it was sent to.
    #[serde(skip)]
    window_bound: Option<(String, u64)>,
}

/// Reads whose file digest is kept; past this, one is dropped to make room.
const MAX_READ_DIGESTS: usize = 256;

/// Files whose last seen digest is kept; past this, one is dropped to make room.
const MAX_FILE_VIEWS: usize = 1024;

fn new_result_store() -> Arc<Store> {
    Arc::new(Store::new(rune_core::id::SessionId::generate().to_string()))
}

impl Default for History {
    fn default() -> Self {
        Self {
            instructions: String::default(),
            pinned_constraints: Vec::new(),
            turns: Vec::default(),
            next_seq: u64::default(),
            result_store: new_result_store(),
            live_plan: Arc::default(),
            read_digests: std::collections::BTreeMap::new(),
            file_views: std::collections::BTreeMap::new(),
            refused_overwrites: std::collections::BTreeSet::new(),
            check_generations: std::collections::BTreeMap::new(),
            anchor: None,
            ledger: crate::ledger::Ledger::default(),
            pruned: std::collections::BTreeSet::new(),
            settled_turn: None,
            reasoning_summary_refused: false,
            temperature_noticed: false,
            window_bound: None,
        }
    }
}

impl History {
    /// Returns an empty history.
    #[must_use]
    pub fn new() -> Self {
        Self {
            instructions: String::new(),
            pinned_constraints: Vec::new(),
            turns: Vec::new(),
            next_seq: 1,
            result_store: new_result_store(),
            live_plan: Arc::default(),
            read_digests: std::collections::BTreeMap::new(),
            file_views: std::collections::BTreeMap::new(),
            refused_overwrites: std::collections::BTreeSet::new(),
            check_generations: std::collections::BTreeMap::new(),
            anchor: None,
            ledger: crate::ledger::Ledger::default(),
            pruned: std::collections::BTreeSet::new(),
            settled_turn: None,
            reasoning_summary_refused: false,
            temperature_noticed: false,
            window_bound: None,
        }
    }

    /// Whether the provider refused reasoning summaries in this conversation.
    #[must_use]
    pub const fn reasoning_summary_refused(&self) -> bool {
        self.reasoning_summary_refused
    }

    /// Records that the refused temperature was announced, and returns whether
    /// this is the first time.
    pub const fn notice_temperature_once(&mut self) -> bool {
        let first = !self.temperature_noticed;
        self.temperature_noticed = true;
        first
    }

    /// Sends later requests of this conversation without reasoning summaries.
    pub const fn refuse_reasoning_summary(&mut self) {
        self.reasoning_summary_refused = true;
    }

    /// Records that `model` rejected a request of `tokens` as too long.
    ///
    /// The smaller bound is kept for the same model; a bound for another model
    /// is replaced, since it says nothing about this one.
    pub fn learn_window_bound(&mut self, model: &str, tokens: u64) {
        match &mut self.window_bound {
            Some((known, bound)) if known == model => *bound = (*bound).min(tokens),
            _ => self.window_bound = Some((model.to_owned(), tokens)),
        }
    }

    /// Returns the learned upper bound of `model`'s window, when one is known.
    #[must_use]
    pub fn window_bound(&self, model: &str) -> Option<u64> {
        self.window_bound
            .as_ref()
            .filter(|(known, _)| known == model)
            .map(|(_, bound)| *bound)
    }

    /// Records the digest of the file a successful `read_file` call read.
    pub(crate) fn record_read(&mut self, call_id: &str, digest: [u8; 32]) {
        if self.read_digests.len() >= MAX_READ_DIGESTS && !self.read_digests.contains_key(call_id) {
            self.read_digests.pop_first();
        }
        self.read_digests.insert(call_id.to_owned(), digest);
    }

    /// Returns the digest recorded for a `read_file` call, if any.
    pub(crate) fn read_digest(&self, call_id: &str) -> Option<[u8; 32]> {
        self.read_digests.get(call_id).copied()
    }

    /// Records the background generation a check command finished in.
    pub(crate) fn record_check_generation(&mut self, call_id: &str, generation: u64) {
        if self.check_generations.len() >= MAX_READ_DIGESTS
            && !self.check_generations.contains_key(call_id)
        {
            self.check_generations.pop_first();
        }
        self.check_generations
            .insert(call_id.to_owned(), generation);
    }

    /// Returns the background generation recorded for a check command, if any.
    pub(crate) fn check_generation(&self, call_id: &str) -> Option<u64> {
        self.check_generations.get(call_id).copied()
    }

    /// Records a file's digest as the model now knows it.
    pub(crate) fn record_view(&mut self, path: camino::Utf8PathBuf, digest: [u8; 32]) {
        if self.file_views.len() >= MAX_FILE_VIEWS && !self.file_views.contains_key(&path) {
            self.file_views.pop_first();
        }
        self.file_views.insert(path, digest);
    }

    /// Returns the digest of a file as the model last saw it, if it has.
    pub(crate) fn view(&self, path: &camino::Utf8Path) -> Option<[u8; 32]> {
        self.file_views.get(path).copied()
    }

    /// Drops the record of a file, returning it.
    pub(crate) fn forget_view(&mut self, path: &camino::Utf8Path) -> Option<[u8; 32]> {
        self.file_views.remove(path)
    }

    /// Notes a refused overwrite; false when the same call was refused before.
    pub(crate) fn refuse_overwrite(&mut self, digest: [u8; 32]) -> bool {
        self.refused_overwrites.insert(digest)
    }

    /// Returns the full tool results spilled from this live conversation.
    ///
    /// Handles remain readable across turns and compaction, but are not saved
    /// in serialized history or shared with a new conversation.
    #[must_use]
    pub fn result_store(&self) -> &Store {
        &self.result_store
    }

    pub(crate) fn result_store_mut(&mut self) -> &mut Store {
        Arc::make_mut(&mut self.result_store)
    }

    /// Keeps retained results under `directory` as well, and loads what an
    /// earlier run of the same session stored there.
    pub fn attach_result_directory(&mut self, directory: camino::Utf8PathBuf) -> Result<()> {
        self.result_store_mut().attach_directory(directory)
    }

    pub(crate) fn shared_result_store(&self) -> Arc<Store> {
        Arc::clone(&self.result_store)
    }

    /// Returns what the turn loop recorded about files, checks and shells.
    #[must_use]
    pub fn ledger(&self) -> &crate::ledger::Ledger {
        &self.ledger
    }

    pub(crate) fn ledger_mut(&mut self) -> &mut crate::ledger::Ledger {
        &mut self.ledger
    }

    /// Returns the seq of the tool-result turn whose checks last settled the
    /// ledger, if any.
    #[must_use]
    pub fn settled_turn(&self) -> Option<u64> {
        self.settled_turn
    }

    pub(crate) fn set_settled_turn(&mut self, seq: u64) {
        self.settled_turn = Some(seq);
    }

    /// A copy of the conversation's current plan.
    #[must_use]
    pub fn plan(&self) -> rune_tools::PlanUpdate {
        self.live_plan
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn shared_live_plan(&self) -> Arc<std::sync::Mutex<rune_tools::PlanUpdate>> {
        Arc::clone(&self.live_plan)
    }

    /// Sets the system instructions.
    pub fn set_instructions(&mut self, instructions: impl Into<String>) {
        self.instructions = instructions.into();
    }

    /// Returns the system instructions.
    #[must_use]
    pub fn instructions(&self) -> &str {
        &self.instructions
    }

    /// Pins an explicit user constraint verbatim until this conversation ends.
    ///
    /// Empty constraints are rejected. Exact duplicates are a no-op; returns
    /// true when a new constraint was added. No constraints are inferred from
    /// ordinary user messages, assistant responses, or tool results.
    pub fn pin_constraint(&mut self, text: impl Into<String>) -> Result<bool> {
        let text = text.into();
        if text.trim().is_empty() {
            return Err(RuneError::invalid_field(
                "constraint",
                "constraint is empty",
            ));
        }
        if self.pinned_constraints.contains(&text) {
            return Ok(false);
        }
        self.pinned_constraints.push(text);
        Ok(true)
    }

    /// Returns the exact user constraints in insertion order.
    #[must_use]
    pub fn pinned_constraints(&self) -> &[String] {
        &self.pinned_constraints
    }

    /// Returns every turn.
    #[must_use]
    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

    /// Returns the number of turns.
    #[must_use]
    pub fn len(&self) -> usize {
        self.turns.len()
    }

    /// Returns true when there are no turns.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }

    /// Returns the last turn, when there is one.
    #[must_use]
    pub fn last(&self) -> Option<&Turn> {
        self.turns.last()
    }

    /// Appends a user turn.
    ///
    /// Returns the sequence number assigned.
    pub fn push_user(&mut self, text: impl Into<String>) -> u64 {
        let seq = self.take_seq();
        self.turns.push(Turn::user(seq, text));
        seq
    }

    /// Adds a note from the harness to the conversation.
    ///
    /// Joins the last turn when it is a user turn, so two user turns never
    /// follow each other; otherwise it becomes a new user turn.
    pub fn push_harness_note(&mut self, text: &str) {
        match self.turns.last_mut() {
            Some(turn) if turn.role == Role::User => turn.parts.push(ContentPart::Text {
                text: text.to_owned(),
            }),
            _ => {
                self.push_user(text);
            }
        }
    }

    /// Appends an assistant turn.
    ///
    /// Returns the sequence number assigned.
    pub fn push_assistant(&mut self, parts: Vec<ContentPart>) -> u64 {
        self.push_assistant_with_replay(parts, None, None)
    }

    /// Appends an assistant turn together with provider state to replay.
    ///
    /// The replay value is stored with the turn because it must be sent back
    /// with exactly the turn that produced it, and a later turn in the same
    /// conversation must not inherit it.
    pub fn push_assistant_with_replay(
        &mut self,
        parts: Vec<ContentPart>,
        replay: Option<String>,
        origin: Option<String>,
    ) -> u64 {
        let seq = self.take_seq();
        let mut turn = Turn::assistant(seq, parts);
        turn.replay = replay;
        turn.origin = origin;
        self.turns.push(turn);
        seq
    }

    /// Replaces the name and arguments of a recorded tool call, so a call that
    /// ran in a repaired form is shown that way next to its result.
    ///
    /// The newest call with the identifier is changed; the turn's replay
    /// state is left as it is.
    pub fn rewrite_tool_call(&mut self, id: &str, name: &str, arguments: &str) {
        let part = self.turns.iter_mut().rev().find_map(|turn| {
            turn.parts.iter_mut().find(
                |part| matches!(part, ContentPart::ToolCall { id: held, .. } if held.as_str() == id),
            )
        });
        if let Some(ContentPart::ToolCall {
            name: held_name,
            arguments: held_arguments,
            ..
        }) = part
        {
            name.clone_into(held_name);
            arguments.clone_into(held_arguments);
        }
    }

    /// Returns the replay state stored on the last turn, when there is one.
    #[must_use]
    pub fn last_replay(&self) -> Option<&str> {
        self.turns.last().and_then(|turn| turn.replay.as_deref())
    }

    /// Appends a tool result turn.
    ///
    /// Tool results are grouped into one turn so a batch answered together stays
    /// together, which is the shape a provider expects.
    pub fn push_tool_results(&mut self, parts: Vec<ContentPart>) -> u64 {
        let seq = self.take_seq();
        self.turns.push(Turn::new(seq, Role::Tool, parts));
        seq
    }

    /// Reserves the next sequence number.
    fn take_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        seq
    }

    /// Returns the number of the next turn to be appended.
    #[must_use]
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Projects the history into the message list a provider expects.
    ///
    /// The system lane is returned separately rather than as a message, because
    /// every dialect places instructions in its own field. Pinned constraints
    /// precede the turns as user content and are never replaced by a summary.
    pub fn to_messages(&self) -> Vec<Message> {
        let pinned = (!self.pinned_constraints.is_empty()).then(|| Message {
            role: Role::User,
            parts: std::iter::once(ContentPart::Text {
                text: "Explicit user constraints (preserve verbatim across context compaction):\n"
                    .to_owned(),
            })
            .chain(self.pinned_constraints.iter().cloned().flat_map(|text| {
                [
                    ContentPart::Text { text },
                    ContentPart::Text {
                        text: "\n".to_owned(),
                    },
                ]
            }))
            .collect(),
            replay: None,
            origin: None,
        });
        let mut messages: Vec<Message> = self
            .turns
            .iter()
            .map(|turn| Message {
                role: turn.role,
                parts: turn.parts.clone(),
                replay: turn.replay.clone(),
                origin: turn.origin.clone(),
            })
            .collect();
        if let Some(pinned) = pinned {
            let index = messages
                .iter()
                .take_while(|message| message.role == Role::System)
                .count();
            messages.insert(index, pinned);
        }
        messages
    }

    /// Validates the ordering rules a request depends on.
    pub fn validate(&self) -> Result<()> {
        validate(&self.to_messages())
    }

    /// Estimates the total byte size of the conversation.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        let pinned_bytes = self
            .pinned_constraints
            .iter()
            .fold(0_usize, |total, text| total.saturating_add(text.len()));
        self.turns.iter().fold(pinned_bytes, |total, turn| {
            total.saturating_add(turn.byte_len())
        })
    }

    /// Returns the reported input of the latest completed request.
    #[must_use]
    pub fn anchor(&self) -> Option<&crate::tokens::Anchor> {
        self.anchor.as_ref()
    }

    /// Records the reported input of a completed request.
    pub fn set_anchor(&mut self, anchor: crate::tokens::Anchor) {
        self.anchor = Some(anchor);
    }

    /// Forgets the anchor, after earlier turns changed.
    pub fn clear_anchor(&mut self) {
        self.anchor = None;
    }

    /// Returns true when a retained turn carries replay state the dialect
    /// reports as signed, which makes any edit of earlier turns invalid.
    #[must_use]
    pub fn is_signed(&self, dialect: &dyn Provider) -> bool {
        self.turns.iter().any(|turn| {
            turn.replay
                .as_deref()
                .is_some_and(|replay| dialect.replay_is_signed(replay))
        })
    }

    /// Returns true when the result of this call was replaced by a receipt.
    #[must_use]
    pub fn is_pruned(&self, id: &str) -> bool {
        self.pruned.contains(id)
    }

    /// Replaces the content of a tool result with a receipt. Returns false
    /// when no result answers the call.
    pub(crate) fn replace_result(&mut self, id: &str, receipt: String) -> bool {
        let part = self.turns.iter_mut().find_map(|turn| {
            turn.parts.iter_mut().find_map(|part| match part {
                ContentPart::ToolResult {
                    id: held, content, ..
                } if held.as_str() == id => Some(content),
                _ => None,
            })
        });
        let Some(content) = part else {
            return false;
        };
        *content = receipt;
        self.pruned.insert(id.to_owned());
        self.clear_anchor();
        true
    }

    /// Truncates to the first `count` turns.
    ///
    /// Used by compaction, which replaces an old prefix with a summary.
    pub fn truncate_to(&mut self, count: usize) {
        self.clear_anchor();
        self.turns.truncate(count);
    }

    /// Keeps only turns from `start` onward, renumbering from one.
    ///
    /// Used when a compaction summary replaces everything before `start`.
    pub fn keep_from(&mut self, start: u64) {
        self.clear_anchor();
        self.turns.retain(|turn| turn.seq >= start);
        self.renumber();
    }

    /// Drops turns matching a sequence number.
    pub fn drop_seq(&mut self, seq: u64) {
        self.clear_anchor();
        self.turns.retain(|turn| turn.seq != seq);
        self.renumber();
    }

    /// Renumbers turns contiguously from one.
    ///
    /// Sequence numbers are positions, not identities, so they stay contiguous
    /// after a history is modified. A gap would look like a corrupt log.
    fn renumber(&mut self) {
        for (index, turn) in self.turns.iter_mut().enumerate() {
            turn.seq = u64::try_from(index).unwrap_or(0).saturating_add(1);
        }
        self.next_seq = u64::try_from(self.turns.len())
            .unwrap_or(0)
            .saturating_add(1);
    }

    /// Inserts a turn at the front, shifting the rest.
    ///
    /// Used to place a compaction summary before the retained tail.
    pub fn prepend(&mut self, role: Role, parts: Vec<ContentPart>) {
        self.clear_anchor();
        self.turns.insert(0, Turn::new(0, role, parts));
        self.renumber();
    }

    /// Replaces the history with a summary followed by a retained tail.
    ///
    /// This is the shape a compaction produces: one summary turn, then the
    /// recent turns verbatim.
    pub fn replace_with_summary(&mut self, summary: impl Into<String>, keep_from_seq: u64) {
        self.clear_anchor();
        let mut retained: Vec<Turn> = self
            .turns
            .iter()
            .filter(|turn| turn.seq >= keep_from_seq)
            .cloned()
            .collect();

        let mut turns = vec![Turn::new(
            1,
            Role::User,
            vec![ContentPart::Text {
                text: summary.into(),
            }],
        )];
        turns.append(&mut retained);

        self.turns = turns;
        self.renumber();
    }

    /// Returns the first sequence number of the tail a compaction keeps.
    ///
    /// The tail is the newest `keep_tokens` estimated tokens, extended back to
    /// the newest user turn so it opens with a request. When the only user
    /// turn opens the conversation, it is extended back to an assistant turn
    /// instead, which keeps each call with its results. It never starts on a
    /// tool result, because a summary followed by an orphaned result cannot
    /// form a request. Returns `None` when nothing would be summarized, which
    /// includes a history smaller than `keep_tokens`.
    #[must_use]
    pub fn tail_start(&self, keep_tokens: u64) -> Option<u64> {
        let newest = self.turns.len().checked_sub(1)?;
        let reached = if keep_tokens == 0 {
            newest
        } else {
            let mut sum: u64 = 0;
            self.turns
                .iter()
                .enumerate()
                .rev()
                .find_map(|(index, turn)| {
                    let bytes = u64::try_from(turn.byte_len()).unwrap_or(u64::MAX);
                    sum = sum.saturating_add(crate::tokens::estimate_tokens(bytes));
                    (sum >= keep_tokens).then_some(index)
                })?
        };
        let reach = || {
            self.turns
                .iter()
                .take(reached.saturating_add(1))
                .rev()
                .filter(|turn| turn.seq > 1)
        };
        reach()
            .find(|turn| turn.role == Role::User)
            .or_else(|| reach().find(|turn| turn.role == Role::Assistant))
            .map(|turn| turn.seq)
    }

    /// Returns a copy of the history containing only turns up to `seq`.
    ///
    /// Used to build a branch without disturbing the original.
    #[must_use]
    pub fn branch_at(&self, seq: u64) -> Self {
        let turns: Vec<Turn> = self
            .turns
            .iter()
            .filter(|turn| turn.seq <= seq)
            .cloned()
            .collect();
        let next_seq = u64::try_from(turns.len()).unwrap_or(0).saturating_add(1);
        Self {
            instructions: self.instructions.clone(),
            pinned_constraints: self.pinned_constraints.clone(),
            turns,
            next_seq,
            result_store: self.result_store.clone(),
            live_plan: Arc::default(),
            read_digests: self.read_digests.clone(),
            file_views: self.file_views.clone(),
            refused_overwrites: self.refused_overwrites.clone(),
            check_generations: self.check_generations.clone(),
            anchor: None,
            ledger: self.ledger.clone(),
            pruned: self.pruned.clone(),
            settled_turn: None,
            reasoning_summary_refused: self.reasoning_summary_refused,
            temperature_noticed: self.temperature_noticed,
            window_bound: self.window_bound.clone(),
        }
    }

    /// Returns the tool call identifiers awaiting a result.
    ///
    /// A non-empty result means the last assistant turn asked for tools whose
    /// results have not been appended, which is exactly the state a resumed
    /// interrupted turn is in.
    #[must_use]
    pub fn pending_tool_calls(&self) -> Vec<ToolCallId> {
        let mut pending: Vec<ToolCallId> = Vec::new();
        for turn in &self.turns {
            match turn.role {
                Role::Assistant => {
                    pending.clear();
                    for (id, _, _) in turn.tool_calls() {
                        pending.push(id.clone());
                    }
                }
                Role::Tool => {
                    for part in &turn.parts {
                        if let ContentPart::ToolResult { id, .. } = part {
                            pending.retain(|candidate| candidate != id);
                        }
                    }
                }
                _ => {}
            }
        }
        pending
    }

    /// Returns the number of assistant turns that asked for tool calls.
    #[must_use]
    pub fn tool_turn_count(&self) -> usize {
        self.turns
            .iter()
            .filter(|turn| !turn.tool_calls().is_empty())
            .count()
    }
}

/// Builds the error used when a history cannot form a valid request.
#[must_use]
pub fn invalid_history(err: &RuneError) -> RuneError {
    RuneError::new(
        ErrorCode::InvalidState,
        format!("the conversation cannot form a request: {}", err.message()),
    )
    .with_hint("this is a defect; the session log records the exact sequence")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_bound_keeps_the_smaller_value_per_model() {
        let mut history = History::new();
        assert_eq!(history.window_bound("a"), None);
        history.learn_window_bound("a", 100_000);
        history.learn_window_bound("a", 120_000);
        assert_eq!(history.window_bound("a"), Some(100_000));
        history.learn_window_bound("a", 85_000);
        assert_eq!(history.window_bound("a"), Some(85_000));
        assert_eq!(history.window_bound("b"), None);
        history.learn_window_bound("b", 90_000);
        assert_eq!(history.window_bound("b"), Some(90_000));
        assert_eq!(history.window_bound("a"), None);
    }

    #[test]
    fn retained_store_snapshots_and_branches_remain_independent_on_a_spill() {
        let mut history = History::new();
        history.push_user("retain");
        let first = history
            .result_store_mut()
            .spill("shell", "first", "first body".to_owned(), false, 0)
            .expect("spill");
        let snapshot = history.shared_result_store();
        let branch = history.branch_at(1);
        let second = history
            .result_store_mut()
            .spill("shell", "second", "second body".to_owned(), false, 0)
            .expect("spill");
        assert_eq!(history.result_store().len(), 2);
        for store in [snapshot.as_ref(), branch.result_store()] {
            assert_eq!(store.len(), 1);
            assert_eq!(
                store.read(&first.handle, 0, 64).expect("first").text,
                "first body"
            );
            assert!(store.read(&second.handle, 0, 64).is_err());
        }
    }

    fn call(id: &str, name: &str) -> ContentPart {
        ContentPart::ToolCall {
            id: ToolCallId::new(id).expect("id"),
            name: name.to_owned(),
            arguments: "{}".to_owned(),
        }
    }

    fn result(id: &str, name: &str) -> ContentPart {
        ContentPart::ToolResult {
            id: ToolCallId::new(id).expect("id"),
            name: name.to_owned(),
            content: "ok".to_owned(),
            is_error: false,
        }
    }

    #[test]
    fn rewriting_the_history_clears_the_anchor() {
        let anchor = crate::tokens::Anchor {
            dialect: "chat_completions".to_owned(),
            model: "model-a".to_owned(),
            input_tokens: 100,
            measure: crate::tokens::Measure {
                text_bytes: 400,
                images: 0,
            },
        };
        let mut history = History::new();
        history.push_user("one");
        history.push_assistant(vec![ContentPart::Text {
            text: "two".to_owned(),
        }]);
        history.push_user("three");

        history.set_anchor(anchor.clone());
        assert_eq!(history.anchor(), Some(&anchor));
        history.replace_with_summary("summary", 3);
        assert_eq!(history.anchor(), None);

        history.set_anchor(anchor);
        history.keep_from(2);
        assert_eq!(history.anchor(), None);
    }

    #[test]
    fn a_new_history_is_empty_and_starts_at_one() {
        let history = History::new();
        assert!(history.is_empty());
        assert_eq!(history.len(), 0);
        assert_eq!(history.next_seq(), 1);
        assert!(history.instructions().is_empty());
    }

    #[test]
    fn turns_are_numbered_contiguously() {
        let mut history = History::new();
        assert_eq!(history.push_user("one"), 1);
        assert_eq!(history.push_assistant(vec![]), 2);
        assert_eq!(history.push_user("two"), 3);
        assert_eq!(history.next_seq(), 4);
        let seqs: Vec<u64> = history.turns().iter().map(|turn| turn.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
    }

    #[test]
    fn instructions_are_held_separately_from_the_turns() {
        let mut history = History::new();
        history.set_instructions("be concise");
        history.push_user("hi");
        assert_eq!(history.instructions(), "be concise");
        assert_eq!(history.len(), 1);
        // The message list never carries a system message.
        let messages = history.to_messages();
        assert!(messages.iter().all(|message| message.role != Role::System));
    }

    #[test]
    fn a_tool_exchange_projects_into_a_valid_request() {
        let mut history = History::new();
        history.push_user("read a file");
        history.push_assistant(vec![call("c1", "read_file")]);
        history.push_tool_results(vec![result("c1", "read_file")]);
        history.validate().expect("valid");
    }

    #[test]
    fn an_unanswered_tool_call_is_detected_before_it_becomes_a_request() {
        let mut history = History::new();
        history.push_user("read a file");
        history.push_assistant(vec![call("c1", "read_file")]);
        let err = history.validate().expect_err("invalid");
        assert_eq!(
            err.detail().invariant.as_deref(),
            Some("tool_result_pairing")
        );
    }

    #[test]
    fn pending_tool_calls_reports_the_unanswered_set() {
        let mut history = History::new();
        history.push_assistant(vec![call("c1", "a"), call("c2", "b")]);
        assert_eq!(history.pending_tool_calls().len(), 2);
        history.push_tool_results(vec![result("c1", "a")]);
        let pending = history.pending_tool_calls();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].as_str(), "c2");
        history.push_tool_results(vec![result("c2", "b")]);
        assert!(history.pending_tool_calls().is_empty());
    }

    #[test]
    fn a_new_assistant_turn_replaces_the_pending_set() {
        let mut history = History::new();
        history.push_assistant(vec![call("c1", "a")]);
        assert_eq!(history.pending_tool_calls().len(), 1);
        // A second assistant turn cannot arrive before the first is answered in
        // a valid conversation, but the accounting must still track the latest.
        history.truncate_to(0);
        history.push_assistant(vec![call("c9", "z")]);
        let pending = history.pending_tool_calls();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].as_str(), "c9");
    }

    #[test]
    fn the_tool_turn_count_counts_assistant_turns_that_called_tools() {
        let mut history = History::new();
        history.push_user("go");
        history.push_assistant(vec![call("c1", "a")]);
        history.push_tool_results(vec![result("c1", "a")]);
        history.push_assistant(vec![ContentPart::Text {
            text: "done".to_owned(),
        }]);
        assert_eq!(history.tool_turn_count(), 1);
    }

    #[test]
    fn byte_length_sums_the_content() {
        let mut history = History::new();
        history.push_user("abcd");
        history.push_assistant(vec![ContentPart::Text {
            text: "ef".to_owned(),
        }]);
        assert_eq!(history.byte_len(), 6);
    }

    #[test]
    fn truncating_keeps_the_leading_turns() {
        let mut history = History::new();
        history.push_user("one");
        history.push_user("two");
        history.push_user("three");
        history.truncate_to(2);
        assert_eq!(history.len(), 2);
        assert_eq!(history.turns()[0].text(), "one");
    }

    #[test]
    fn keeping_from_a_sequence_drops_earlier_turns_and_renumbers() {
        let mut history = History::new();
        history.push_user("one");
        history.push_user("two");
        history.push_user("three");
        history.keep_from(2);
        assert_eq!(history.len(), 2);
        assert_eq!(history.turns()[0].text(), "two");
        // Renumbering keeps the sequence contiguous, because a gap would look
        // like a damaged log to anything reading it.
        assert_eq!(history.turns()[0].seq, 1);
        assert_eq!(history.turns()[1].seq, 2);
        assert_eq!(history.next_seq(), 3);
    }

    #[test]
    fn dropping_a_sequence_removes_it_and_renumbers() {
        let mut history = History::new();
        history.push_user("one");
        history.push_user("two");
        history.drop_seq(1);
        assert_eq!(history.len(), 1);
        assert_eq!(history.turns()[0].text(), "two");
        assert_eq!(history.turns()[0].seq, 1);
    }

    #[test]
    fn replacing_with_a_summary_prepends_it_and_keeps_the_tail() {
        let mut history = History::new();
        history.push_user("old one");
        history.push_user("old two");
        history.push_user("recent");
        history.replace_with_summary("summary of the earlier work", 3);
        assert_eq!(history.len(), 2);
        assert_eq!(history.turns()[0].text(), "summary of the earlier work");
        assert_eq!(history.turns()[1].text(), "recent");
        assert_eq!(history.turns()[0].seq, 1);
        assert_eq!(history.turns()[1].seq, 2);
    }

    #[test]
    fn prepending_shifts_every_existing_turn() {
        let mut history = History::new();
        history.push_user("existing");
        history.prepend(
            Role::User,
            vec![ContentPart::Text {
                text: "summary".to_owned(),
            }],
        );
        assert_eq!(history.len(), 2);
        assert_eq!(history.turns()[0].text(), "summary");
        assert_eq!(history.turns()[1].text(), "existing");
        assert_eq!(history.turns()[1].seq, 2);
    }

    fn sized_result(id: &str, bytes: usize) -> ContentPart {
        ContentPart::ToolResult {
            id: ToolCallId::new(id).expect("id"),
            name: "a".to_owned(),
            content: "r".repeat(bytes),
            is_error: false,
        }
    }

    fn role_at(history: &History, seq: u64) -> Role {
        history
            .turns()
            .iter()
            .find(|turn| turn.seq == seq)
            .expect("present")
            .role
    }

    #[test]
    fn the_tail_is_measured_in_tokens_and_starts_at_a_user_turn() {
        // Each exchange: a 100-token question, a call, a 1,001-token result
        // and a 100-token answer, starting at seq 4i + 1.
        let mut history = History::new();
        for index in 0..5 {
            let id = format!("c{index}");
            history.push_user("q".repeat(400));
            history.push_assistant(vec![call(&id, "a")]);
            history.push_tool_results(vec![sized_result(&id, 4000)]);
            history.push_assistant(vec![ContentPart::Text {
                text: "a".repeat(400),
            }]);
        }

        // The newest exchange is 1,202 tokens; 1,500 is reached at the result
        // of the exchange before it, whose question is seq 13.
        let start = history.tail_start(1500).expect("a tail");
        assert_eq!(start, 13);
        assert_eq!(role_at(&history, start), Role::User);

        history.replace_with_summary("summary", start);
        assert_eq!(history.len(), 9);
        history.validate().expect("still valid");
    }

    #[test]
    fn a_long_single_prompt_keeps_a_tail_that_starts_at_an_assistant_step() {
        // The only user turn opens the conversation. Calls are at seq 2i + 2
        // and their 101-token results at 2i + 3.
        let mut history = History::new();
        history.push_user("do everything");
        for index in 0..10 {
            let id = format!("c{index}");
            history.push_assistant(vec![call(&id, "a")]);
            history.push_tool_results(vec![sized_result(&id, 400)]);
        }
        history.push_assistant(vec![ContentPart::Text {
            text: "done".to_owned(),
        }]);

        // 250 tokens are reached at the result at seq 17; the tail starts at
        // its call.
        let start = history.tail_start(250).expect("a tail");
        assert_eq!(start, 16);
        assert_eq!(role_at(&history, start), Role::Assistant);

        history.replace_with_summary("summary", start);
        assert_eq!(history.len(), 8);
        history.validate().expect("still valid");
    }

    #[test]
    fn a_zero_keep_budget_keeps_only_the_newest_exchange() {
        let mut history = History::new();
        history.push_user("one");
        history.push_assistant(vec![call("c1", "a")]);
        history.push_tool_results(vec![result("c1", "a")]);
        history.push_user("two");
        history.push_assistant(vec![ContentPart::Text {
            text: "answer".to_owned(),
        }]);

        let start = history.tail_start(0).expect("a tail");
        assert_eq!(start, 4);
        assert_eq!(role_at(&history, start), Role::User);

        history.replace_with_summary("summary", start);
        assert_eq!(history.len(), 3);
        assert_eq!(history.turns()[1].text(), "two");
        history.validate().expect("still valid");
    }

    #[test]
    fn a_large_newest_result_keeps_its_call() {
        let mut history = History::new();
        history.push_user("first");
        history.push_assistant(vec![ContentPart::Text {
            text: "first answer".to_owned(),
        }]);
        history.push_user("second");
        history.push_assistant(vec![call("c1", "a")]);
        // 30,000 tokens on its own, past the default tail.
        history.push_tool_results(vec![sized_result("c1", 120_000)]);

        let start = history.tail_start(20_000).expect("a tail");
        assert_eq!(start, 3);
        assert_eq!(role_at(&history, start), Role::User);

        history.replace_with_summary("summary", start);
        assert_eq!(history.len(), 4);
        history.validate().expect("still valid");
    }

    #[test]
    fn no_compaction_cut_exists_when_the_history_is_short() {
        let mut history = History::new();
        history.push_user("only");
        assert_eq!(history.tail_start(0), None);
        assert_eq!(history.tail_start(20_000), None);

        // A conversation smaller than the tail is kept whole.
        history.push_assistant(vec![ContentPart::Text {
            text: "answer".to_owned(),
        }]);
        history.push_user("again");
        assert_eq!(history.tail_start(0), Some(3));
        assert_eq!(history.tail_start(20_000), None);
    }

    #[test]
    fn branching_at_a_sequence_keeps_only_the_prefix() {
        let mut history = History::new();
        history.push_user("one");
        history.push_user("two");
        history.push_user("three");
        let branch = history.branch_at(2);
        assert_eq!(branch.len(), 2);
        assert_eq!(branch.turns()[1].text(), "two");
        // The original is untouched.
        assert_eq!(history.len(), 3);
    }

    #[test]
    fn a_branch_carries_the_instructions() {
        let mut history = History::new();
        history.set_instructions("be helpful");
        history.push_user("one");
        let branch = history.branch_at(1);
        assert_eq!(branch.instructions(), "be helpful");
    }

    #[test]
    fn history_round_trips_through_json() {
        let mut history = History::new();
        history.set_instructions("instructions");
        history.push_user("hello");
        history.push_assistant(vec![call("c1", "read_file")]);
        history.push_tool_results(vec![result("c1", "read_file")]);

        let text = serde_json::to_string(&history).expect("serialize");
        let parsed: History = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed.instructions(), "instructions");
        parsed.validate().expect("still valid");
    }

    #[test]
    fn a_turn_remembers_the_model_that_produced_it() {
        let mut history = History::new();
        history.push_user("hello");
        history.push_assistant_with_replay(
            vec![ContentPart::Text {
                text: "hi".to_owned(),
            }],
            None,
            Some("anthropic:claude-opus-5-5".to_owned()),
        );
        history.push_assistant(vec![ContentPart::Text {
            text: "again".to_owned(),
        }]);

        let text = serde_json::to_string(&history).expect("serialize");
        let parsed: History = serde_json::from_str(&text).expect("deserialize");
        let messages = parsed.to_messages();
        assert_eq!(
            messages[1].origin.as_deref(),
            Some("anthropic:claude-opus-5-5")
        );
        assert_eq!(messages[2].origin, None);

        let old = r#"{"seq":1,"role":"assistant","parts":[]}"#;
        let turn: Turn = serde_json::from_str(old).expect("an old turn");
        assert_eq!(turn.origin, None);
        assert!(
            !serde_json::to_string(&turn)
                .expect("json")
                .contains("origin")
        );
    }

    #[test]
    fn a_turn_reports_its_call_identifiers() {
        let turn = Turn::assistant(1, vec![call("c1", "a"), call("c2", "b")]);
        let calls = turn.tool_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0.as_str(), "c1");
        assert_eq!(calls[0].1, "a");
    }

    #[test]
    fn an_empty_turn_is_reported_as_empty() {
        assert!(Turn::assistant(1, vec![]).is_empty());
        assert!(!Turn::user(1, "x").is_empty());
    }

    #[test]
    fn the_invalid_history_error_names_the_cause() {
        let inner = RuneError::invariant("tool_result_pairing", "unpaired");
        let wrapped = invalid_history(&inner);
        assert_eq!(wrapped.code(), ErrorCode::InvalidState);
        assert!(wrapped.message().contains("unpaired"));
        assert!(wrapped.detail().hint.is_some());
    }
}
