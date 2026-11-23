//! Retained tool results.
//!
//! A large result stays out of the model's context but remains fully
//! inspectable. The model receives a bounded preview, the retained byte count,
//! and an opaque handle. The store API reads a byte range or searches for a
//! literal through that handle; a model-callable reader is not yet registered.
//!
//! A handle is scoped to one session and is not guessable from another, so one
//! session cannot read another's retained output.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Write as _;
use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use rune_core::budget::{BudgetSet, LimitName};
use rune_core::error::{ErrorCode, Result, RuneError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::workspace::truncate_to_bytes;

/// Preview size shown to the model when a result is retained.
pub const DEFAULT_PREVIEW_BYTES: usize = 4096;

/// Largest preview a spilled result keeps in the conversation.
pub const SPILL_PREVIEW_BYTES: usize = 16 * 1024;

/// Largest byte range one read may return.
pub const MAX_READ_BYTES: usize = 64 * 1024;

/// Bytes of retained results one session keeps; past this the oldest are dropped.
pub const SESSION_STORE_BYTES: usize = 64 * 1024 * 1024;

/// The file in a store directory that records each insert and eviction in order.
const INDEX_FILE: &str = "index.jsonl";

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
#[derive(Clone, Debug)]
struct Retained {
    handle: Handle,
    tool: String,
    content: Arc<str>,
    is_error: bool,
}

/// One line of a store directory's index.
#[derive(Debug, Serialize, Deserialize)]
struct IndexLine {
    handle: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    is_error: Option<bool>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    evicted: bool,
}

/// What the model receives in place of a full result.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Preview {
    /// The bounded preview text.
    pub text: String,
    /// Total bytes retained.
    pub retained_bytes: u64,
    /// Whether the result was stored and its handle can be read.
    pub is_retained: bool,
    /// A stable identifier, readable only when `is_retained` is true.
    pub handle: Handle,
    /// Whether the tool reported a failure.
    pub is_error: bool,
}

impl Preview {
    /// Renders the preview as the model-visible string.
    ///
    /// For stored results, states the retained size and readable handle.
    /// Results returned whole without retention render as their original text.
    #[must_use]
    pub fn render(&self) -> String {
        if !self.is_retained {
            return self.text.clone();
        }
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
#[derive(Clone, Debug)]
pub struct Store {
    session: String,
    entries: BTreeMap<Handle, Retained>,
    /// Total retained bytes, for the session bound.
    total_bytes: usize,
    /// Bytes kept before the oldest results are dropped.
    budget: usize,
    /// Handles in insertion order, oldest first.
    order: VecDeque<Handle>,
    /// Handles dropped to keep the store under its budget.
    evicted: BTreeSet<Handle>,
    /// Where results are also kept on disk, when attached.
    directory: Option<Utf8PathBuf>,
}

impl Default for Store {
    fn default() -> Self {
        Self::new(String::new())
    }
}

impl Store {
    /// Builds a store for one session.
    #[must_use]
    pub fn new(session: impl Into<String>) -> Self {
        Self::with_budget(session, SESSION_STORE_BYTES)
    }

    /// Builds a store that keeps at most `budget` bytes.
    #[must_use]
    fn with_budget(session: impl Into<String>, budget: usize) -> Self {
        Self {
            session: session.into(),
            entries: BTreeMap::new(),
            total_bytes: 0,
            budget,
            order: VecDeque::new(),
            evicted: BTreeSet::new(),
            directory: None,
        }
    }

    /// Keeps every retained result under `directory` as well as in memory, and loads
    /// what an earlier run of the same session stored there.
    pub fn attach_directory(&mut self, directory: Utf8PathBuf) -> Result<()> {
        rune_core::paths::create_dir_private(&directory)?;
        let index = rune_core::paths::read_private(&directory.join(INDEX_FILE), u64::MAX)?
            .unwrap_or_default();
        // A line cut short by an interrupted write is ended, so the next line
        // appended starts on its own.
        if !index.is_empty() && !index.ends_with('\n') {
            append_to_index(&directory, "\n");
        }

        // The final state of each handle the index names, then the order the
        // survivors were last inserted in.
        let mut recorded: BTreeMap<Handle, (String, bool)> = BTreeMap::new();
        let mut evicted = BTreeSet::new();
        let mut inserted = Vec::new();
        for line in index.lines() {
            let Ok(record) = serde_json::from_str::<IndexLine>(line) else {
                continue;
            };
            let Ok(handle) = Handle::parse(&record.handle) else {
                continue;
            };
            if record.evicted {
                recorded.remove(&handle);
                evicted.insert(handle);
                continue;
            }
            let Some(tool) = record.tool else {
                continue;
            };
            evicted.remove(&handle);
            if recorded
                .insert(handle.clone(), (tool, record.is_error.unwrap_or(false)))
                .is_none()
            {
                inserted.push(handle);
            }
        }
        let mut seen = BTreeSet::new();
        let mut loaded_order: Vec<Handle> = inserted
            .into_iter()
            .rev()
            .filter(|handle| recorded.contains_key(handle) && seen.insert(handle.clone()))
            .collect();
        loaded_order.reverse();

        let mut on_disk = BTreeSet::new();
        let mut order = VecDeque::new();
        for handle in loaded_order {
            let Some((tool, is_error)) = recorded.remove(&handle) else {
                continue;
            };
            let budget = u64::try_from(self.budget).unwrap_or(u64::MAX);
            let Ok(Some(content)) =
                rune_core::paths::read_private(&content_path(&directory, &handle), budget)
            else {
                continue;
            };
            on_disk.insert(handle.clone());
            order.push_back(handle.clone());
            self.entries.entry(handle.clone()).or_insert(Retained {
                handle,
                tool,
                content: content.into(),
                is_error,
            });
        }
        for handle in std::mem::take(&mut self.order) {
            if !on_disk.contains(&handle) {
                order.push_back(handle);
            }
        }
        self.order = order;
        evicted.extend(std::mem::take(&mut self.evicted));
        self.evicted = evicted
            .into_iter()
            .filter(|handle| !self.entries.contains_key(handle))
            .collect();
        self.total_bytes = self.entries.values().map(|entry| entry.content.len()).sum();
        self.directory = Some(directory);

        self.enforce_budget();
        let unsaved: Vec<Handle> = self
            .order
            .iter()
            .filter(|handle| !on_disk.contains(*handle))
            .cloned()
            .collect();
        for handle in unsaved {
            if let Some(entry) = self.entries.get(&handle) {
                self.persist(entry);
            }
        }
        Ok(())
    }

    /// Returns true when the handle's result can be read.
    #[must_use]
    pub fn is_available(&self, handle: &Handle) -> bool {
        self.entries.contains_key(handle)
    }

    /// Stores an entry, keeping the place of a handle already present, then
    /// drops the oldest entries until the store is within its budget.
    fn insert(&mut self, entry: Retained) {
        let handle = entry.handle.clone();
        let bytes = entry.content.len();
        self.persist(&entry);
        match self.entries.insert(handle.clone(), entry) {
            Some(replaced) => {
                self.total_bytes = self.total_bytes.saturating_sub(replaced.content.len());
            }
            None => self.order.push_back(handle.clone()),
        }
        self.evicted.remove(&handle);
        self.total_bytes = self.total_bytes.saturating_add(bytes);
        self.enforce_budget();
    }

    /// Drops the oldest entries while the store holds more than its budget.
    fn enforce_budget(&mut self) {
        while self.total_bytes > self.budget {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(entry) = self.entries.remove(&oldest) {
                self.total_bytes = self.total_bytes.saturating_sub(entry.content.len());
                if let Some(directory) = &self.directory {
                    let _ = std::fs::remove_file(content_path(directory, &oldest));
                    append_index(
                        directory,
                        &IndexLine {
                            handle: oldest.to_string(),
                            tool: None,
                            is_error: None,
                            evicted: true,
                        },
                    );
                }
                self.evicted.insert(oldest);
            }
        }
    }

    /// Writes an entry to the attached directory. A failure is ignored: the
    /// entry stays in memory and only a later resume loses it.
    fn persist(&self, entry: &Retained) {
        let Some(directory) = &self.directory else {
            return;
        };
        if rune_core::paths::write_private(&content_path(directory, &entry.handle), &entry.content)
            .is_ok()
        {
            append_index(
                directory,
                &IndexLine {
                    handle: entry.handle.to_string(),
                    tool: Some(entry.tool.clone()),
                    is_error: Some(entry.is_error),
                    evicted: false,
                },
            );
        }
    }

    /// The error for a result that cannot be stored.
    fn full() -> RuneError {
        RuneError::new(
            ErrorCode::LimitExceeded,
            "the session's retained tool result store is full",
        )
        .with_hint("start a new session to retain more")
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

    /// Spills a full result that would otherwise be cut from the conversation.
    ///
    /// Unlike `retain`, the model's per-result cap does not cut the stored body.
    /// Older results are dropped to keep the session within its byte budget. A
    /// result larger than the whole budget is refused and leaves the store
    /// unchanged, so the caller can report that it could not be retained.
    pub fn spill(
        &mut self,
        tool: &str,
        tool_call: &str,
        content: String,
        is_error: bool,
        preview_bytes: usize,
    ) -> Result<Preview> {
        if content.len() > self.budget {
            return Err(Self::full());
        }
        let handle = Handle::derive(&self.session, tool_call, &content);
        let preview = Self::build_preview(
            &content,
            &handle,
            is_error,
            preview_bytes.min(SPILL_PREVIEW_BYTES),
        );
        self.insert(Retained {
            handle,
            tool: tool.to_owned(),
            content: content.into(),
            is_error,
        });
        Ok(preview)
    }

    /// Retains a result and returns its preview.
    ///
    /// A result at or below the preview size is returned whole without retention
    /// metadata when rendered, because retaining it would waste a slot. Its
    /// stable identifier is not a readable handle.
    /// Stored content includes any truncation marker within the result cap;
    /// caps smaller than the marker retain only the marker prefix that fits.
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
                retained_bytes: 0,
                is_retained: false,
                handle: Handle::derive(&self.session, tool_call, content),
                is_error,
            });
        }

        // The retained content is itself bounded, so one enormous result cannot
        // exhaust memory on its own.
        let bounded: String = if content.len() > retained_limit {
            let marker = truncate_to_bytes("\n[truncated at the retention limit]", retained_limit);
            let mut text =
                truncate_to_bytes(content, retained_limit.saturating_sub(marker.len())).to_owned();
            text.push_str(marker);
            text
        } else {
            content.to_owned()
        };

        if bounded.len() > self.budget {
            return Err(Self::full());
        }
        let handle = Handle::derive(&self.session, tool_call, &bounded);
        let preview = Self::build_preview(&bounded, &handle, is_error, preview_bytes);
        self.insert(Retained {
            handle,
            tool: tool.to_owned(),
            content: bounded.into(),
            is_error,
        });
        Ok(preview)
    }

    /// Builds the preview for a retained body.
    fn build_preview(
        content: &str,
        handle: &Handle,
        is_error: bool,
        preview_bytes: usize,
    ) -> Preview {
        Preview {
            text: preview_text(content, preview_bytes),
            retained_bytes: u64::try_from(content.len()).unwrap_or(u64::MAX),
            is_retained: true,
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
            .ok_or_else(|| self.unknown_handle(handle))?;
        let content: &str = &entry.content;

        let length = length.clamp(1, MAX_READ_BYTES);
        if offset >= content.len() {
            return Ok(ReadOutcome {
                text: String::new(),
                offset,
                total_bytes: u64::try_from(content.len()).unwrap_or(u64::MAX),
                tool: entry.tool.clone(),
            });
        }

        let mut end = offset.saturating_add(length).min(content.len());
        while end > offset && !content.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        let mut start = offset;
        while start < content.len() && !content.is_char_boundary(start) {
            start = start.saturating_add(1);
        }

        Ok(ReadOutcome {
            text: content.get(start..end).unwrap_or_default().to_owned(),
            offset: start,
            total_bytes: u64::try_from(content.len()).unwrap_or(u64::MAX),
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
            .ok_or_else(|| self.unknown_handle(handle))?;
        if needle.is_empty() {
            return Err(RuneError::invalid_field("query", "must not be empty"));
        }
        let content: &str = &entry.content;

        let mut matches = Vec::new();
        let mut truncated = false;
        for (line_index, line) in content.lines().enumerate() {
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
        self.order.clear();
        self.total_bytes = 0;
    }

    /// Builds the error for a handle this store cannot read.
    ///
    /// A handle dropped to keep the session under its budget says so. Any other
    /// handle does not distinguish "never existed" from "belongs to another
    /// session", because the difference is not useful to a caller and naming it
    /// would confirm the existence of another session's content.
    fn unknown_handle(&self, handle: &Handle) -> RuneError {
        if self.evicted.contains(handle) {
            return RuneError::new(
                ErrorCode::NotFound,
                format!(
                    "the result for handle `{handle}` was dropped to keep this session's stored results under 64 MiB"
                ),
            )
            .with_hint("run the command or read the file again");
        }
        RuneError::new(
            ErrorCode::NotFound,
            format!("no retained result for handle `{handle}`"),
        )
        .with_hint("handles work only within the session that produced them")
    }
}

/// Path of one result's content in a store directory.
fn content_path(directory: &Utf8Path, handle: &Handle) -> Utf8PathBuf {
    directory.join(format!("{handle}.txt"))
}

/// Appends one line to a store directory's index. A failure is ignored, as for
/// the content files.
fn append_index(directory: &Utf8Path, line: &IndexLine) {
    if let Ok(mut text) = serde_json::to_string(line) {
        text.push('\n');
        append_to_index(directory, &text);
    }
}

/// Appends text to a store directory's index, creating it private.
fn append_to_index(directory: &Utf8Path, text: &str) {
    use std::io::Write as _;

    let path = directory.join(INDEX_FILE);
    if std::fs::symlink_metadata(&path).is_ok_and(|meta| !meta.is_file()) {
        return;
    }
    let mut options = std::fs::OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    if let Ok(mut file) = options.open(&path) {
        let _ = file.write_all(text.as_bytes());
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

/// Bytes a preview keeps for the marker naming the range it leaves out.
const OMISSION_MARKER_ROOM: usize = 96;

/// Returns at most `budget` bytes of `content` for a preview.
///
/// A long result keeps its start and its end and names the byte range between
/// them, because the end is often the part that matters, such as a test run's
/// failures, and the range is exactly what `read_tool_result` takes to fetch
/// the rest.
fn preview_text(content: &str, budget: usize) -> String {
    if content.len() <= budget {
        return content.to_owned();
    }
    if budget < OMISSION_MARKER_ROOM.saturating_mul(4) {
        return truncate_to_bytes(content, budget).to_owned();
    }
    let (start, end) =
        crate::workspace::head_and_tail(content, budget.saturating_sub(OMISSION_MARKER_ROOM));
    let resume = content.len().saturating_sub(end.len());
    format!(
        "{start}\n[bytes {} to {resume} omitted; read_tool_result reads them]\n{end}",
        start.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_preview_keeps_both_ends_and_names_the_omitted_range() {
        let content: String = (0..2000).fold(String::new(), |mut text, index| {
            use std::fmt::Write as _;
            writeln!(text, "line {index}").expect("format");
            text
        });
        let preview = preview_text(&content, 4096);
        assert!(preview.len() <= 4096, "{} bytes", preview.len());
        assert!(preview.starts_with("line 0\n"));
        assert!(preview.ends_with("line 1999\n"));
        let (from, to) = preview
            .split_once("[bytes ")
            .and_then(|(_, rest)| rest.split_once(" omitted"))
            .and_then(|(range, _)| range.split_once(" to "))
            .map(|(from, to)| {
                (
                    from.parse::<usize>().expect("from"),
                    to.parse::<usize>().expect("to"),
                )
            })
            .expect("an omitted range");
        // The range is exact: what precedes it and what follows it are the
        // content's own bytes, so reading it restores the whole result.
        assert_eq!(&content[..from], &preview[..from]);
        assert!(preview.ends_with(&content[to..]));

        assert_eq!(preview_text("short", 4096), "short");
    }

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
    fn unretained_results_render_without_read_instructions() {
        let limits = limits();
        let mut store = store();
        let at_preview_limit = "é".repeat(Store::preview_bytes(&limits) / 2);
        for content in ["", "tiny", "tiny\n", at_preview_limit.as_str()] {
            for is_error in [false, true] {
                let preview = store
                    .retain(&limits, "shell", "call-1", content, is_error)
                    .expect("preview");

                assert_eq!(preview.render(), content);
                assert!(!preview.render().contains("read_tool_result"));
                assert!(!preview.is_retained);
                assert_eq!(preview.retained_bytes, 0);
                assert_eq!(preview.is_error, is_error);
                assert_eq!(
                    store
                        .read(&preview.handle, 0, 100)
                        .expect_err("not stored")
                        .code(),
                    ErrorCode::NotFound
                );
            }
        }
        assert!(store.is_empty());
        assert_eq!(store.total_bytes(), 0);
    }

    #[test]
    fn small_spilled_results_advertise_a_readable_handle() {
        for content in ["", "tiny"] {
            let mut store = store();
            let preview = store
                .spill(
                    "shell",
                    "call-1",
                    content.to_owned(),
                    false,
                    DEFAULT_PREVIEW_BYTES,
                )
                .expect("spilled");

            assert!(preview.is_retained);
            assert!(preview.render().contains("read_tool_result"));
            assert!(preview.render().contains(preview.handle.as_str()));
            assert_eq!(
                store.read(&preview.handle, 0, 100).expect("stored").text,
                content
            );
        }
    }

    #[test]
    fn a_spill_previews_up_to_sixteen_kib() {
        let mut store = store();
        let content = format!("HEAD\n{}TAIL", "line of output\n".repeat(40 * 1024 / 15));
        let preview = store
            .spill("shell", "call-1", content.clone(), false, 32 * 1024)
            .expect("spilled");
        assert!(preview.text.len() <= SPILL_PREVIEW_BYTES);
        assert!(
            preview.text.len() > SPILL_PREVIEW_BYTES - 512,
            "{}",
            preview.text.len()
        );
        assert!(preview.text.starts_with("HEAD\n"));
        assert!(preview.text.ends_with("TAIL"));

        // A smaller cap still bounds the preview.
        let preview = store
            .spill("shell", "call-2", content, false, 6_000)
            .expect("spilled");
        assert!(preview.text.len() <= 6_000);
    }

    #[test]
    fn a_large_result_is_retained_and_previewed() {
        let mut store = store();
        let content = "x".repeat(30_000);
        let preview = store
            .retain(&limits(), "shell", "call-1", &content, false)
            .expect("retained");
        assert_eq!(store.len(), 1);
        assert!(preview.text.len() <= DEFAULT_PREVIEW_BYTES);
        assert_eq!(preview.retained_bytes, 30_000);
    }

    #[test]
    fn retaining_the_same_handle_counts_its_bytes_once() {
        let mut store = store();
        let content = "é".repeat(2500);
        let other = "y".repeat(6000);
        store
            .retain(&limits(), "shell", "other-call", &other, false)
            .expect("other result retained");
        let first = store
            .retain(&limits(), "shell", "call-1", &content, false)
            .expect("first retain");
        let second = store
            .retain(&limits(), "read_file", "call-1", &content, true)
            .expect("replacement");

        assert_eq!(first.handle, second.handle);
        assert_eq!(store.len(), 2);
        assert_eq!(store.total_bytes(), other.len() + content.len());
        assert_eq!(
            store
                .read(&second.handle, 0, MAX_READ_BYTES)
                .expect("read")
                .text,
            content
        );
        assert_eq!(
            store.describe(&second.handle),
            Some(("read_file", content.len() as u64, true))
        );
    }

    #[test]
    fn replacing_a_truncated_result_counts_the_stored_bytes_once() {
        let mut limits = limits();
        limits
            .set(
                LimitName::MaxToolResultBytes,
                rune_core::budget::Budget::Bounded(8192),
                rune_core::config::Layer::User,
            )
            .expect("set");
        let mut store = store();
        let prefix = "x".repeat(8192);
        let first = store
            .retain(&limits, "shell", "call-1", &format!("{prefix}first"), false)
            .expect("first retain");
        let second = store
            .retain(
                &limits,
                "shell",
                "call-1",
                &format!("{prefix}second"),
                false,
            )
            .expect("replacement");
        let stored = store.read(&second.handle, 0, MAX_READ_BYTES).expect("read");

        assert_eq!(first.handle, second.handle);
        assert_eq!(store.len(), 1);
        assert_eq!(store.total_bytes(), stored.text.len());
        assert_eq!(second.retained_bytes, stored.total_bytes);
    }

    #[test]
    fn spilling_keeps_the_full_body_even_with_a_zero_byte_preview() {
        let mut store = store();
        let content = format!("{}TAIL", "é".repeat(40_000));
        let preview = store
            .spill("shell", "spill", content.clone(), true, 0)
            .expect("spilled");
        assert!(preview.text.is_empty());
        assert_eq!(preview.retained_bytes, content.len() as u64);
        assert_eq!(store.total_bytes(), content.len());
        assert_eq!(
            store
                .read(&preview.handle, content.len() - 4, 4)
                .expect("tail")
                .text,
            "TAIL"
        );
        assert_eq!(
            store.describe(&preview.handle),
            Some(("shell", content.len() as u64, true))
        );
    }

    #[test]
    fn spilling_refuses_a_result_larger_than_the_budget_without_losing_existing_results() {
        let mut store = Store::with_budget("session-abc", 1024);
        let mut first = None;
        for index in 0..4 {
            let preview = store
                .spill("shell", &index.to_string(), "kept".to_owned(), false, 4)
                .expect("spilled");
            first.get_or_insert(preview.handle);
        }
        let err = store
            .spill("shell", "overflow", "x".repeat(1025), false, 4)
            .expect_err("larger than the budget");
        assert_eq!(err.code(), ErrorCode::LimitExceeded);
        assert_eq!(store.len(), 4);
        assert_eq!(store.total_bytes(), 16);
        assert_eq!(
            store
                .read(&first.expect("first"), 0, 4)
                .expect("existing output")
                .text,
            "kept"
        );
    }

    #[test]
    fn spilled_bytes_cannot_exceed_the_session_budget() {
        let mut store = store();
        let preview = store
            .spill("shell", "full", "x".repeat(SESSION_STORE_BYTES), false, 8)
            .expect("at the budget");
        let err = store
            .spill(
                "shell",
                "overflow",
                "x".repeat(SESSION_STORE_BYTES + 1),
                false,
                1,
            )
            .expect_err("past the budget");
        assert_eq!(err.code(), ErrorCode::LimitExceeded);
        assert_eq!(store.len(), 1);
        assert_eq!(store.total_bytes(), SESSION_STORE_BYTES);
        assert_eq!(
            store
                .read(&preview.handle, SESSION_STORE_BYTES - 1, 1)
                .expect("existing tail")
                .text,
            "x"
        );

        // One more byte keeps the store within the budget by dropping the oldest.
        store
            .spill("shell", "next", "y".to_owned(), false, 1)
            .expect("spilled");
        assert_eq!(store.len(), 1);
        assert_eq!(store.total_bytes(), 1);
        assert!(!store.is_available(&preview.handle));
    }

    #[test]
    fn more_than_256_results_are_kept() {
        let mut store = store();
        let content = "r".repeat(1024);
        let handles: Vec<Handle> = (0..300)
            .map(|index| {
                store
                    .spill("shell", &format!("call-{index}"), content.clone(), false, 0)
                    .expect("spilled")
                    .handle
            })
            .collect();
        assert_eq!(store.len(), 300);
        assert_eq!(store.total_bytes(), 300 * 1024);
        for handle in &handles {
            assert_eq!(store.read(handle, 0, 2048).expect("kept").text, content);
        }
    }

    #[test]
    fn the_oldest_results_are_dropped_past_the_byte_budget() {
        let mut store = Store::with_budget("session-abc", 64 * 1024);
        let handles: Vec<Handle> = (0..65)
            .map(|index| {
                store
                    .spill(
                        "shell",
                        &format!("call-{index}"),
                        format!("{index:04}{}", "z".repeat(1020)),
                        false,
                        0,
                    )
                    .expect("spilled")
                    .handle
            })
            .collect();
        assert_eq!(store.len(), 64);
        assert_eq!(store.total_bytes(), 64 * 1024);
        assert!(!store.is_available(&handles[0]));
        for handle in &handles[1..] {
            assert!(store.is_available(handle));
        }

        // Storing an existing handle again keeps its place, so the next drop
        // still takes the oldest.
        store
            .spill(
                "shell",
                "call-1",
                format!("{:04}{}", 1, "z".repeat(1020)),
                false,
                0,
            )
            .expect("again");
        store
            .spill("shell", "call-65", "w".repeat(1024), false, 0)
            .expect("spilled");
        assert!(!store.is_available(&handles[1]));
        assert!(store.is_available(&handles[2]));
        assert!(store.total_bytes() <= 64 * 1024);
    }

    #[test]
    fn a_dropped_handle_says_it_was_dropped() {
        let mut store = Store::with_budget("session-abc", 8);
        let dropped = store
            .spill("shell", "first", "12345678".to_owned(), false, 0)
            .expect("spilled")
            .handle;
        store
            .spill("shell", "second", "abcdefgh".to_owned(), false, 0)
            .expect("spilled");

        let err = store.read(&dropped, 0, 8).expect_err("dropped");
        assert_eq!(err.code(), ErrorCode::NotFound);
        assert_eq!(
            err.message(),
            format!(
                "the result for handle `{dropped}` was dropped to keep this session's stored results under 64 MiB"
            )
        );
        assert_eq!(
            err.detail().hint.as_deref(),
            Some("run the command or read the file again")
        );
        let err = store.search(&dropped, "1", 8).expect_err("dropped");
        assert!(err.message().contains("was dropped"), "{err}");

        let unknown = Handle::derive("session-abc", "never", "stored");
        let err = store.read(&unknown, 0, 8).expect_err("unknown");
        assert_eq!(
            err.message(),
            format!("no retained result for handle `{unknown}`")
        );
    }

    fn directory(temp: &tempfile::TempDir) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(temp.path().join("results")).expect("utf8")
    }

    #[test]
    fn results_written_to_a_directory_load_in_a_new_store() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut store = store();
        store.attach_directory(directory(&temp)).expect("attached");
        let first = store
            .spill("shell", "first", "first body".to_owned(), false, 0)
            .expect("spilled")
            .handle;
        let second = store
            .spill("read_file", "second", "second body".to_owned(), true, 0)
            .expect("spilled")
            .handle;

        let mut resumed = Store::new("another-session-string");
        resumed.attach_directory(directory(&temp)).expect("loaded");
        assert_eq!(resumed.len(), 2);
        assert_eq!(resumed.total_bytes(), 21);
        assert_eq!(
            resumed.read(&first, 0, 64).expect("first").text,
            "first body"
        );
        assert_eq!(
            resumed.read(&second, 0, 64).expect("second").text,
            "second body"
        );
        assert_eq!(resumed.describe(&second), Some(("read_file", 11, true)));
    }

    #[test]
    fn an_eviction_is_replayed_from_the_index() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut store = Store::with_budget("session-abc", 16);
        store.attach_directory(directory(&temp)).expect("attached");
        let dropped = store
            .spill("shell", "first", "a".repeat(10), false, 0)
            .expect("spilled")
            .handle;
        let kept = store
            .spill("shell", "second", "b".repeat(10), false, 0)
            .expect("spilled")
            .handle;
        assert!(!directory(&temp).join(format!("{dropped}.txt")).exists());

        let mut resumed = Store::with_budget("session-def", 16);
        resumed.attach_directory(directory(&temp)).expect("loaded");
        assert_eq!(resumed.len(), 1);
        assert!(resumed.is_available(&kept));
        let err = resumed.read(&dropped, 0, 8).expect_err("dropped");
        assert!(err.message().contains("was dropped"), "{err}");
    }

    #[test]
    fn a_truncated_last_index_line_is_skipped() {
        use std::io::Write as _;

        let temp = tempfile::tempdir().expect("tempdir");
        let mut store = store();
        store.attach_directory(directory(&temp)).expect("attached");
        let kept = store
            .spill("shell", "first", "first body".to_owned(), false, 0)
            .expect("spilled")
            .handle;
        std::fs::OpenOptions::new()
            .append(true)
            .open(directory(&temp).join(INDEX_FILE))
            .expect("index")
            .write_all(b"{\"handle\":\"result-")
            .expect("torn line");

        let mut resumed = store_in(&temp);
        assert_eq!(resumed.len(), 1);
        assert_eq!(resumed.read(&kept, 0, 64).expect("kept").text, "first body");
        let next = resumed
            .spill("shell", "next", "next body".to_owned(), false, 0)
            .expect("spilled")
            .handle;
        let again = store_in(&temp);
        assert_eq!(again.read(&kept, 0, 64).expect("kept").text, "first body");
        assert_eq!(again.read(&next, 0, 64).expect("next").text, "next body");
    }

    fn store_in(temp: &tempfile::TempDir) -> Store {
        let mut store = Store::new("session-xyz");
        store.attach_directory(directory(temp)).expect("loaded");
        store
    }

    #[test]
    fn results_held_before_attaching_are_written_to_the_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut store = store();
        let early = store
            .spill("shell", "early", "early body".to_owned(), false, 0)
            .expect("spilled")
            .handle;
        store.attach_directory(directory(&temp)).expect("attached");
        assert_eq!(
            store_in(&temp).read(&early, 0, 64).expect("early").text,
            "early body"
        );
    }

    #[test]
    fn the_rendered_preview_states_the_size_and_handle() {
        let mut store = store();
        let content = "y".repeat(30_000);
        let preview = store
            .retain(&limits(), "shell", "call-1", &content, false)
            .expect("retained");
        let rendered = preview.render();
        assert!(rendered.contains("30000 bytes retained"), "{rendered}");
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
        let content = "z".repeat(30_000);
        let preview = store
            .retain(&limits(), "shell", "call-1", &content, false)
            .expect("retained");
        let outcome = store.read(&preview.handle, 10_000_000, 10).expect("read");
        assert!(outcome.text.is_empty());
        assert_eq!(outcome.total_bytes, 30_000);
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
        let content = "x".repeat(30_000);
        let preview = store
            .retain(&limits(), "shell", "call-1", &content, false)
            .expect("retained");
        assert!(store.search(&preview.handle, "", 10).is_err());
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
            preview.retained_bytes <= 8192,
            "retained {} bytes",
            preview.retained_bytes
        );
    }

    #[test]
    fn retention_cap_includes_the_truncation_marker() {
        let marker = "\n[truncated at the retention limit]";
        for cap in [0, 1, 16, 34, 35, 36, 2048, 4096, 8192] {
            let mut limits = limits();
            limits
                .set(
                    LimitName::MaxToolResultBytes,
                    rune_core::budget::Budget::Bounded(cap as u64),
                    rune_core::config::Layer::User,
                )
                .expect("set");
            let mut store = store();
            let content = "x".repeat(cap + 1);
            let preview = store
                .retain(&limits, "shell", "call-1", &content, false)
                .expect("retained");
            let stored = store
                .read(&preview.handle, 0, MAX_READ_BYTES)
                .expect("read");

            assert!(stored.text.len() <= cap, "cap {cap}: {stored:?}");
            assert_eq!(stored.text.len(), cap);
            assert_eq!(stored.total_bytes, cap as u64);
            assert_eq!(preview.retained_bytes, stored.total_bytes);
            assert_eq!(store.total_bytes(), cap);
            assert_eq!(
                store.describe(&preview.handle),
                Some(("shell", cap as u64, false))
            );
            if cap >= marker.len() {
                assert_eq!(
                    stored.text,
                    format!("{}{marker}", "x".repeat(cap - marker.len()))
                );
            } else {
                assert_eq!(stored.text, &marker[..cap]);
            }
        }
    }

    #[test]
    fn truncated_retention_preserves_utf8_within_the_cap() {
        let mut limits = limits();
        limits
            .set(
                LimitName::MaxToolResultBytes,
                rune_core::budget::Budget::Bounded(8192),
                rune_core::config::Layer::User,
            )
            .expect("set");
        let mut store = store();
        let content = format!("{}x", "é".repeat(4096));
        let preview = store
            .retain(&limits, "shell", "call-1", &content, false)
            .expect("retained");
        let stored = store
            .read(&preview.handle, 0, MAX_READ_BYTES)
            .expect("read");

        assert_eq!(stored.text.len(), 8191);
        assert_eq!(
            stored.text,
            format!("{}\n[truncated at the retention limit]", "é".repeat(4078))
        );
        assert_eq!(preview.retained_bytes, stored.total_bytes);
        assert_eq!(store.total_bytes(), stored.text.len());
    }

    #[test]
    fn retention_keeps_content_at_or_below_the_cap_unchanged() {
        let mut limits = limits();
        limits
            .set(
                LimitName::MaxToolResultBytes,
                rune_core::budget::Budget::Bounded(8192),
                rune_core::config::Layer::User,
            )
            .expect("set");
        for content in ["x".repeat(8191), "é".repeat(4096)] {
            let mut store = store();
            let preview = store
                .retain(&limits, "shell", "call-1", &content, false)
                .expect("retained");
            let stored = store
                .read(&preview.handle, 0, MAX_READ_BYTES)
                .expect("read");

            assert_eq!(stored.text, content);
            assert_eq!(preview.retained_bytes, content.len() as u64);
            assert_eq!(store.total_bytes(), content.len());
        }
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
        let content = "x".repeat(30_000);
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
        let content = "x".repeat(30_000);
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
