//! Retained tool results.
//!
//! A large result stays out of the model's context but remains fully
//! inspectable. The model receives a bounded preview, the retained byte count,
//! and an opaque handle; `read_tool_result` reads a byte range or searches for a
//! literal through that handle.
//!
//! A handle is scoped to one session and is not guessable from another, so one
//! session cannot read another's retained output.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use rune_core::budget::{BudgetSet, LimitName};
use rune_core::error::{ErrorCode, Result, RuneError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Preview size shown to the model when a result is retained.
pub const DEFAULT_PREVIEW_BYTES: usize = 4096;

/// Largest byte range one read may return.
pub const MAX_READ_BYTES: usize = 64 * 1024;

/// Largest number of retained results in one session.
pub const MAX_RETAINED_RESULTS: usize = 256;

/// A handle identifying one retained result.
///
/// Derived from the session, the tool call, and the content, so it is stable for
/// identical content but cannot be guessed for content the caller has not seen.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub struct Handle(String);

impl Handle {
    /// Derives a handle.
    #[must_use]
    pub fn derive(session: &str, tool_call: &str, content: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(session.as_bytes());
        hasher.update(b"\x1f");
        hasher.update(tool_call.as_bytes());
        hasher.update(b"\x1f");
        hasher.update(content.as_bytes());
        let digest = hasher.finalize();
        // Sixteen bytes is enough to make a collision implausible while keeping
        // the handle short enough to copy by hand from a transcript.
        let mut encoded = String::with_capacity(36);
        encoded.push_str("result-");
        for byte in digest.iter().take(12) {
            let _ = write!(encoded, "{byte:02x}");
        }
        Self(encoded)
    }

    /// Returns the handle text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parses a handle supplied by a caller.
    pub fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        if !trimmed.starts_with("result-") {
            return Err(RuneError::invalid_field(
                "handle",
                "does not look like a retained result handle",
            ));
        }
        if trimmed.len() != 31 {
            return Err(RuneError::invalid_field(
                "handle",
                format!("has length {}, expected 31", trimmed.len()),
            ));
        }
        if !trimmed
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(RuneError::invalid_field(
                "handle",
                "contains characters outside the accepted set",
            ));
        }
        Ok(Self(trimmed.to_owned()))
    }
}

impl std::fmt::Display for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One retained result.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Retained {
    handle: Handle,
    tool: String,
    content: String,
    is_error: bool,
}

/// What the model receives in place of a full result.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Preview {
    /// The bounded preview text.
    pub text: String,
    /// Total bytes retained.
    pub retained_bytes: u64,
    /// The handle to read more.
    pub handle: Handle,
    /// Whether the tool reported a failure.
    pub is_error: bool,
}

impl Preview {
    /// Renders the preview as the model-visible string.
    ///
    /// States the retained size and the handle, and says how to read more, so
    /// the model knows the result was truncated and what to do about it.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&self.text);
        if !self.text.ends_with('\n') {
            out.push('\n');
        }
        let _ = write!(
            out,
            "\n[{} bytes retained; read more with read_tool_result and handle {}]",
            self.retained_bytes, self.handle
        );
        out
    }
}

/// The session-scoped store of retained results.
#[derive(Debug, Default)]
pub struct Store {
    session: String,
    entries: BTreeMap<Handle, Retained>,
    /// Total retained bytes, for the session bound.
    total_bytes: usize,
}

impl Store {
    /// Builds a store for one session.
    #[must_use]
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            session: session.into(),
            entries: BTreeMap::new(),
            total_bytes: 0,
        }
    }

    /// Returns the session this store belongs to.
    #[must_use]
    pub fn session(&self) -> &str {
        &self.session
    }

    /// Returns the number of retained results.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns true when nothing is retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the total retained bytes.
    #[must_use]
    pub const fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Returns the preview size for a limit set.
    #[must_use]
    pub fn preview_bytes(limits: &BudgetSet) -> usize {
        let cap = limits.get_usize(LimitName::MaxToolResultBytes);
        cap.min(DEFAULT_PREVIEW_BYTES)
    }

    /// Retains a result and returns its preview.
    ///
    /// A result at or below the preview size is returned whole with no handle,
    /// because retaining something the model already has wastes a slot.
    pub fn retain(
        &mut self,
        limits: &BudgetSet,
        tool: &str,
        tool_call: &str,
        content: &str,
        is_error: bool,
    ) -> Result<Preview> {
        let preview_bytes = Self::preview_bytes(limits);
        let retained_limit = limits.get_usize(LimitName::MaxToolResultBytes);

        if content.len() <= preview_bytes {
            // Small enough to hand over directly. The handle is still derived so
            // a caller has a stable identifier, but nothing is stored.
            return Ok(Preview {
                text: content.to_owned(),
                retained_bytes: u64::try_from(content.len()).unwrap_or(u64::MAX),
                handle: Handle::derive(&self.session, tool_call, content),
                is_error,
            });
        }

        if self.entries.len() >= MAX_RETAINED_RESULTS {
            return Err(RuneError::new(
                ErrorCode::LimitExceeded,
                format!("the session already holds {MAX_RETAINED_RESULTS} retained results"),
            )
            .with_hint("start a new session to retain more"));
        }

        // The retained content is itself bounded, so one enormous result cannot
        // exhaust memory on its own.
        let bounded: String = if content.len() > retained_limit {
            let mut cut = retained_limit;
            while cut > 0 && !content.is_char_boundary(cut) {
                cut = cut.saturating_sub(1);
            }
            let mut text = content.get(..cut).unwrap_or_default().to_owned();
            text.push_str("\n[truncated at the retention limit]");
            text
        } else {
            content.to_owned()
        };

        let handle = Handle::derive(&self.session, tool_call, &bounded);
        let preview = Self::build_preview(&bounded, &handle, is_error, preview_bytes);

        self.total_bytes = self.total_bytes.saturating_add(bounded.len());
        self.entries.insert(
            handle.clone(),
            Retained {
                handle: handle.clone(),
                tool: tool.to_owned(),
                content: bounded,
                is_error,
            },
        );

        Ok(preview)
    }

    /// Builds the preview for a retained body.
    fn build_preview(
        content: &str,
        handle: &Handle,
        is_error: bool,
        preview_bytes: usize,
    ) -> Preview {
        let mut cut = preview_bytes.min(content.len());
        while cut > 0 && !content.is_char_boundary(cut) {
            cut = cut.saturating_sub(1);
        }
        Preview {
            text: content.get(..cut).unwrap_or_default().to_owned(),
            retained_bytes: u64::try_from(content.len()).unwrap_or(u64::MAX),
            handle: handle.clone(),
            is_error,
        }
    }

    /// Reads a byte range from a retained result.
    ///
    /// The range is clamped to the content and to the per-read bound, so a
    /// caller cannot request the whole store in one call.
    pub fn read(&self, handle: &Handle, offset: usize, length: usize) -> Result<ReadOutcome> {
        let entry = self
            .entries
            .get(handle)
            .ok_or_else(|| unknown_handle(handle))?;

        let length = length.clamp(1, MAX_READ_BYTES);
        if offset >= entry.content.len() {
            return Ok(ReadOutcome {
                text: String::new(),
                offset,
                total_bytes: u64::try_from(entry.content.len()).unwrap_or(u64::MAX),
                tool: entry.tool.clone(),
            });
        }

        let mut end = offset.saturating_add(length).min(entry.content.len());
        while end > offset && !entry.content.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        let mut start = offset;
        while start < entry.content.len() && !entry.content.is_char_boundary(start) {
            start = start.saturating_add(1);
        }

        Ok(ReadOutcome {
            text: entry.content.get(start..end).unwrap_or_default().to_owned(),
            offset: start,
            total_bytes: u64::try_from(entry.content.len()).unwrap_or(u64::MAX),
            tool: entry.tool.clone(),
        })
    }

    /// Searches a retained result for a literal string.
    pub fn search(
        &self,
        handle: &Handle,
        needle: &str,
        max_matches: usize,
    ) -> Result<SearchOutcome> {
        let entry = self
            .entries
            .get(handle)
            .ok_or_else(|| unknown_handle(handle))?;
        if needle.is_empty() {
            return Err(RuneError::invalid_field("query", "must not be empty"));
        }

        let mut matches = Vec::new();
        let mut truncated = false;
        for (line_index, line) in entry.content.lines().enumerate() {
            if !line.contains(needle) {
                continue;
            }
            if matches.len() >= max_matches {
                truncated = true;
                break;
            }
            matches.push(LineMatch {
                line: u64::try_from(line_index).unwrap_or(0).saturating_add(1),
                text: line.to_owned(),
            });
        }

        Ok(SearchOutcome {
            matches,
            truncated,
            tool: entry.tool.clone(),
        })
    }

    /// Returns the retained result for a handle, for diagnostics.
    #[must_use]
    pub fn describe(&self, handle: &Handle) -> Option<(&str, u64, bool)> {
        self.entries.get(handle).map(|entry| {
            (
                entry.tool.as_str(),
                u64::try_from(entry.content.len()).unwrap_or(u64::MAX),
                entry.is_error,
            )
        })
    }

    /// Drops every retained result.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.total_bytes = 0;
    }
}

/// One read of a retained result.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ReadOutcome {
    /// The bytes read.
    pub text: String,
    /// Where the read started, after adjusting to a character boundary.
    pub offset: usize,
    /// Total bytes in the retained result.
    pub total_bytes: u64,
    /// The tool that produced it.
    pub tool: String,
}

/// One matching line.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LineMatch {
    /// One-based line number.
    pub line: u64,
    /// The matching line.
    pub text: String,
}

/// A search over a retained result.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SearchOutcome {
    /// Matching lines, in order.
    pub matches: Vec<LineMatch>,
    /// Whether more matches exist beyond the bound.
    pub truncated: bool,
    /// The tool that produced the result.
    pub tool: String,
}

/// Builds the error for a handle that is unknown in this session.
///
/// Deliberately does not distinguish "never existed" from "belongs to another
/// session", because the difference is not useful to a caller and naming it
/// would confirm the existence of another session's content.
fn unknown_handle(handle: &Handle) -> RuneError {
    RuneError::new(
        ErrorCode::NotFound,
        format!("no retained result for handle `{handle}`"),
    )
    .with_hint("handles work only within the session that produced them")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::new("session-abc")
    }

    fn limits() -> BudgetSet {
        BudgetSet::new()
    }

    #[test]
    fn a_small_result_is_returned_whole_without_being_stored() {
        let mut store = store();
        let preview = store
            .retain(&limits(), "read_file", "call-1", "small output", false)
            .expect("retained");
        assert_eq!(preview.text, "small output");
        assert!(store.is_empty(), "a small result was stored unnecessarily");
    }

    #[test]
    fn a_large_result_is_retained_and_previewed() {
        let mut store = store();
        let content = "x".repeat(50_000);
        let preview = store
            .retain(&limits(), "shell", "call-1", &content, false)
            .expect("retained");
        assert_eq!(store.len(), 1);
        assert!(preview.text.len() <= DEFAULT_PREVIEW_BYTES);
        assert_eq!(preview.retained_bytes, 50_000);
    }

    #[test]
    fn the_rendered_preview_states_the_size_and_handle() {
        let mut store = store();
        let content = "y".repeat(50_000);
        let preview = store
            .retain(&limits(), "shell", "call-1", &content, false)
            .expect("retained");
        let rendered = preview.render();
        assert!(rendered.contains("50000 bytes retained"), "{rendered}");
        assert!(rendered.contains(preview.handle.as_str()), "{rendered}");
        assert!(rendered.contains("read_tool_result"), "{rendered}");
    }

    #[test]
    fn a_handle_round_trips_through_text() {
        let handle = Handle::derive("s", "c", "content");
        let parsed = Handle::parse(handle.as_str()).expect("parsed");
        assert_eq!(parsed, handle);
    }

    #[test]
    fn a_handle_is_stable_for_identical_input() {
        let first = Handle::derive("s", "c", "content");
        let second = Handle::derive("s", "c", "content");
        assert_eq!(first, second);
    }

    #[test]
    fn a_handle_differs_across_sessions_and_calls() {
        let base = Handle::derive("s1", "c1", "content");
        assert_ne!(base, Handle::derive("s2", "c1", "content"));
        assert_ne!(base, Handle::derive("s1", "c2", "content"));
        assert_ne!(base, Handle::derive("s1", "c1", "other"));
    }

    #[test]
    fn a_malformed_handle_is_rejected() {
        assert!(Handle::parse("").is_err());
        assert!(Handle::parse("not-a-handle").is_err());
        assert!(Handle::parse("result-tooshort").is_err());
        assert!(Handle::parse(&format!("result-{}", "z".repeat(100))).is_err());
    }

    #[test]
    fn reading_a_range_returns_those_bytes() {
        let mut store = store();
        let mut content = String::new();
        for index in 0..2000 {
            let _ = writeln!(content, "line {index}");
        }
        let preview = store
            .retain(&limits(), "shell", "call-1", &content, false)
            .expect("retained");

        let outcome = store.read(&preview.handle, 0, 10).expect("read");
        assert_eq!(outcome.text.len(), 10);
        assert_eq!(outcome.text, content.get(..10).unwrap_or_default());
    }

    #[test]
    fn reading_past_the_end_returns_nothing_rather_than_failing() {
        let mut store = store();
        let content = "z".repeat(50_000);
        let preview = store
            .retain(&limits(), "shell", "call-1", &content, false)
            .expect("retained");
        let outcome = store.read(&preview.handle, 10_000_000, 10).expect("read");
        assert!(outcome.text.is_empty());
        assert_eq!(outcome.total_bytes, 50_000);
    }

    #[test]
    fn a_read_is_bounded() {
        let mut store = store();
        let content = "z".repeat(200_000);
        let preview = store
            .retain(&limits(), "shell", "call-1", &content, false)
            .expect("retained");
        let outcome = store
            .read(&preview.handle, 0, MAX_READ_BYTES * 10)
            .expect("read");
        assert!(outcome.text.len() <= MAX_READ_BYTES);
    }

    #[test]
    fn a_read_lands_on_a_character_boundary() {
        let mut store = store();
        // Multi-byte characters, so a naive slice would panic or split one.
        let content: String = "é".repeat(40_000);
        let preview = store
            .retain(&limits(), "shell", "call-1", &content, false)
            .expect("retained");
        let outcome = store.read(&preview.handle, 1, 3).expect("read");
        assert!(content.contains(&outcome.text) || outcome.text.is_empty());
    }

    #[test]
    fn an_unknown_handle_is_not_found() {
        let store = store();
        let handle = Handle::derive("other-session", "call-1", "content");
        let err = store.read(&handle, 0, 10).expect_err("not found");
        assert_eq!(err.code(), ErrorCode::NotFound);
        assert!(err.detail().hint.is_some());
    }

    #[test]
    fn searching_returns_matching_lines_with_numbers() {
        let mut store = store();
        let mut content = String::new();
        for index in 0..3000 {
            if index % 500 == 0 {
                content.push_str("needle here\n");
            } else {
                let _ = writeln!(content, "filler line {index}");
            }
        }
        let preview = store
            .retain(&limits(), "shell", "call-1", &content, false)
            .expect("retained");

        let outcome = store.search(&preview.handle, "needle", 10).expect("search");
        assert!(!outcome.matches.is_empty());
        assert_eq!(outcome.matches[0].line, 1);
        assert!(outcome.matches[0].text.contains("needle"));
    }

    #[test]
    fn a_search_reports_truncation() {
        let mut store = store();
        let content = "needle\n".repeat(3000);
        let preview = store
            .retain(&limits(), "shell", "call-1", &content, false)
            .expect("retained");
        let outcome = store.search(&preview.handle, "needle", 5).expect("search");
        assert_eq!(outcome.matches.len(), 5);
        assert!(outcome.truncated);
    }

    #[test]
    fn an_empty_search_query_is_rejected() {
        let mut store = store();
        let content = "x".repeat(50_000);
        let preview = store
            .retain(&limits(), "shell", "call-1", &content, false)
            .expect("retained");
        assert!(store.search(&preview.handle, "", 10).is_err());
    }

    #[test]
    fn the_retained_count_is_bounded() {
        let mut store = store();
        let content = "x".repeat(50_000);
        for index in 0..MAX_RETAINED_RESULTS {
            store
                .retain(
                    &limits(),
                    "shell",
                    &format!("call-{index}"),
                    &content,
                    false,
                )
                .expect("retained");
        }
        let err = store
            .retain(&limits(), "shell", "one-more", &content, false)
            .expect_err("bounded");
        assert_eq!(err.code(), ErrorCode::LimitExceeded);
        assert!(err.detail().hint.is_some());
    }

    #[test]
    fn retained_content_is_itself_bounded() {
        let mut limits = limits();
        limits
            .set(
                LimitName::MaxToolResultBytes,
                rune_core::budget::Budget::Bounded(8192),
                rune_core::config::Layer::User,
            )
            .expect("set");

        let mut store = store();
        let content = "x".repeat(100_000);
        let preview = store
            .retain(&limits, "shell", "call-1", &content, false)
            .expect("retained");
        assert!(
            preview.retained_bytes <= 8192 + 64,
            "retained {} bytes",
            preview.retained_bytes
        );
    }

    #[test]
    fn an_error_result_is_marked() {
        let mut store = store();
        let content = "boom".repeat(20_000);
        let preview = store
            .retain(&limits(), "shell", "call-1", &content, true)
            .expect("retained");
        assert!(preview.is_error);
    }

    #[test]
    fn describe_reports_the_tool_and_size() {
        let mut store = store();
        let content = "x".repeat(50_000);
        let preview = store
            .retain(&limits(), "read_file", "call-1", &content, false)
            .expect("retained");
        let (tool, size, is_error) = store.describe(&preview.handle).expect("present");
        assert_eq!(tool, "read_file");
        assert!(size > 0);
        assert!(!is_error);
    }

    #[test]
    fn clearing_drops_everything() {
        let mut store = store();
        let content = "x".repeat(50_000);
        store
            .retain(&limits(), "shell", "call-1", &content, false)
            .expect("retained");
        assert_eq!(store.len(), 1);
        store.clear();
        assert!(store.is_empty());
        assert_eq!(store.total_bytes(), 0);
    }

    #[test]
    fn the_session_is_reported() {
        let store = store();
        assert_eq!(store.session(), "session-abc");
    }

    #[test]
    fn the_preview_size_follows_the_limit_when_it_is_lower() {
        let mut limits = limits();
        limits
            .set(
                LimitName::MaxToolResultBytes,
                rune_core::budget::Budget::Bounded(2048),
                rune_core::config::Layer::User,
            )
            .expect("set");
        assert_eq!(Store::preview_bytes(&limits), 2048);
    }
}
