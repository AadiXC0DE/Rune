//! Conversation history.
//!
//! Holds the durable turns of one conversation and enforces the ordering rules a
//! provider request depends on. The `Message` type in `rune-net` is the wire
//! shape; this is the owner of the sequence, and it is what compaction, forking,
//! and session persistence operate on.

use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::id::ToolCallId;
use rune_net::message::{ContentPart, Message, Role, validate};
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
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct History {
    /// System instructions, separate from the turn list.
    instructions: String,
    /// Turns in order.
    turns: Vec<Turn>,
    /// Next sequence number.
    next_seq: u64,
}

impl History {
    /// Returns an empty history.
    #[must_use]
    pub fn new() -> Self {
        Self {
            instructions: String::new(),
            turns: Vec::new(),
            next_seq: 1,
        }
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

    /// Appends an assistant turn.
    ///
    /// Returns the sequence number assigned.
    pub fn push_assistant(&mut self, parts: Vec<ContentPart>) -> u64 {
        self.push_assistant_with_replay(parts, None)
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
    ) -> u64 {
        let seq = self.take_seq();
        let mut turn = Turn::assistant(seq, parts);
        turn.replay = replay;
        self.turns.push(turn);
        seq
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
    /// every dialect places instructions in its own field.
    pub fn to_messages(&self) -> Vec<Message> {
        self.turns
            .iter()
            .map(|turn| Message {
                role: turn.role,
                parts: turn.parts.clone(),
                replay: turn.replay.clone(),
            })
            .collect()
    }

    /// Validates the ordering rules a request depends on.
    pub fn validate(&self) -> Result<()> {
        validate(&self.to_messages())
    }

    /// Estimates the total byte size of the conversation.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.turns
            .iter()
            .fold(0_usize, |total, turn| total.saturating_add(turn.byte_len()))
    }

    /// Truncates to the first `count` turns.
    ///
    /// Used by compaction, which replaces an old prefix with a summary.
    pub fn truncate_to(&mut self, count: usize) {
        self.turns.truncate(count);
    }

    /// Keeps only turns from `start` onward, renumbering from one.
    ///
    /// Used when a compaction summary replaces everything before `start`.
    pub fn keep_from(&mut self, start: u64) {
        self.turns.retain(|turn| turn.seq >= start);
        self.renumber();
    }

    /// Drops turns matching a sequence number.
    pub fn drop_seq(&mut self, seq: u64) {
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
        self.turns.insert(0, Turn::new(0, role, parts));
        self.renumber();
    }

    /// Replaces the history with a summary followed by a retained tail.
    ///
    /// This is the shape a compaction produces: one summary turn, then the
    /// recent turns verbatim.
    pub fn replace_with_summary(&mut self, summary: impl Into<String>, keep_from_seq: u64) {
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

    /// Returns the sequence number where a summary should cut.
    ///
    /// Keeps at least `keep_turns` recent turns and always cuts at a user turn,
    /// because a summary that begins mid-exchange leaves an orphaned tool result.
    #[must_use]
    pub fn compaction_cut(&self, keep_turns: usize) -> Option<u64> {
        if self.turns.len() <= keep_turns {
            return None;
        }
        let target = self.turns.len().saturating_sub(keep_turns);
        // Walk forward to the next user turn so the cut is at a boundary.
        self.turns
            .iter()
            .skip(target)
            .find(|turn| turn.role == Role::User)
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
            turns,
            next_seq,
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

    #[test]
    fn the_compaction_cut_lands_on_a_user_turn() {
        let mut history = History::new();
        history.push_user("one");
        history.push_assistant(vec![call("c1", "a")]);
        history.push_tool_results(vec![result("c1", "a")]);
        history.push_user("two");
        history.push_assistant(vec![ContentPart::Text {
            text: "answer".to_owned(),
        }]);

        // Cutting mid-exchange would orphan a tool result, so the cut walks
        // forward to the next user turn.
        let cut = history.compaction_cut(2).expect("a cut");
        let turn = history
            .turns()
            .iter()
            .find(|candidate| candidate.seq == cut)
            .expect("present");
        assert_eq!(turn.role, Role::User);
        assert_eq!(turn.text(), "two");
    }

    #[test]
    fn no_compaction_cut_exists_when_the_history_is_short() {
        let mut history = History::new();
        history.push_user("only");
        assert_eq!(history.compaction_cut(5), None);
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
