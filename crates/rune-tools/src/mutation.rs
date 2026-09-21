//! Atomic file mutation.
//!
//! A change is prepared and then applied. Preparing captures everything needed
//! to make the change safe: the resolved path, the file identity, the hash of
//! the bytes that were read, the exact postimage, and a bounded preview.
//! Applying stages the postimage in the target directory, revalidates that the
//! file is still the one that was read, and renames it into place, so a reader
//! sees either the whole old file or the whole new one.
//!
//! Content is never normalized. Matching is byte exact and the postimage is
//! written verbatim, so CRLF and lone CR endings survive a round trip.

use std::fs::{File, OpenOptions, Permissions};
use std::io::{ErrorKind, Read, Write};
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use camino::{Utf8Path, Utf8PathBuf};
use rune_core::error::{ErrorCode, Result, RuneError};
use sha2::{Digest, Sha256};

use crate::contract::ExecutionContext;

/// Largest postimage accepted by one mutation.
pub const MAX_WRITE_BYTES: usize = 4 * 1024 * 1024;
/// Largest combined preimage and postimage accepted by one mutation.
pub const MAX_TOTAL_BYTES: usize = 8 * 1024 * 1024;

/// Lines rendered in one change preview.
const PREVIEW_LINES: usize = 6;
/// Part of the preview budget reserved for removals and for additions.
const PREVIEW_HALF: usize = PREVIEW_LINES / 2;
/// Bytes rendered in one change preview.
const PREVIEW_BYTES: usize = 1024;
/// Characters kept from one previewed line.
const PREVIEW_LINE_CHARS: usize = 200;
/// Prefix of the staging file that apply renames into place.
const STAGE_PREFIX: &str = ".rune-stage-";
/// Attempts made to allocate a unique staging name.
const STAGE_ATTEMPTS: usize = 8;
/// Buffer used while hashing a file that is not read into memory.
const HASH_BUFFER_BYTES: usize = 64 * 1024;

/// Identity of one file on disk.
///
/// The preimage hash proves the bytes are the ones that were read; the identity
/// proves it is still the same file, which catches a replacement that happens
/// to carry identical bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FileIdentity {
    device: u64,
    inode: u64,
}

/// Which occurrence of a match an edit replaces.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Occurrence {
    /// The match must occur exactly once.
    Unique,
    /// Every match is replaced.
    All,
    /// Only the given one-based index is replaced.
    Index(usize),
}

/// One-based inclusive line range a change produced in the resulting file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ChangeSpan {
    /// First changed line.
    pub first_line: usize,
    /// Last changed line, never before `first_line`.
    pub last_line: usize,
}

/// A bounded rendering of what a mutation changes.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Preview {
    /// Rendered lines, removals before additions.
    pub text: String,
    /// Whether the rendering dropped content.
    pub truncated: bool,
}

/// A mutation that has been captured but not yet applied.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Prepared {
    resolved: Utf8PathBuf,
    identity: Option<FileIdentity>,
    hash: [u8; 32],
    preimage_len: usize,
    postimage: Vec<u8>,
    permissions: Option<Permissions>,
    preview: Preview,
    span: Option<ChangeSpan>,
    replacements: usize,
}

impl Prepared {
    /// Returns the resolved path the mutation targets.
    #[must_use]
    pub fn path(&self) -> &Utf8Path {
        self.resolved.as_path()
    }

    /// Returns the hash of the bytes that were read.
    #[must_use]
    pub const fn preimage_hash(&self) -> &[u8; 32] {
        &self.hash
    }

    /// Returns the size of the file that was read, zero when it did not exist.
    #[must_use]
    pub const fn preimage_len(&self) -> usize {
        self.preimage_len
    }

    /// Returns the size of the content that will be written.
    #[must_use]
    pub fn postimage_len(&self) -> usize {
        self.postimage.len()
    }

    /// Returns the bounded preview of the change.
    #[must_use]
    pub const fn preview(&self) -> &Preview {
        &self.preview
    }

    /// Returns the line range the change produces, absent when nothing changes.
    #[must_use]
    pub const fn span(&self) -> Option<ChangeSpan> {
        self.span
    }

    /// Returns how many matches the mutation replaces.
    #[must_use]
    pub const fn replacements(&self) -> usize {
        self.replacements
    }

    /// Returns true when the target does not exist yet.
    #[must_use]
    pub const fn creates_file(&self) -> bool {
        self.identity.is_none()
    }
}

/// The result of applying a mutation.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Applied {
    path: Utf8PathBuf,
    bytes_before: usize,
    bytes_after: usize,
    span: Option<ChangeSpan>,
    replacements: usize,
    created: bool,
}

impl Applied {
    /// Returns the resolved path that was written.
    #[must_use]
    pub fn path(&self) -> &Utf8Path {
        self.path.as_path()
    }

    /// Returns the size of the file before the mutation.
    #[must_use]
    pub const fn bytes_before(&self) -> usize {
        self.bytes_before
    }

    /// Returns the size of the file after the mutation.
    #[must_use]
    pub const fn bytes_after(&self) -> usize {
        self.bytes_after
    }

    /// Returns the signed size change.
    #[must_use]
    pub fn byte_delta(&self) -> i64 {
        let after = i64::try_from(self.bytes_after).unwrap_or(i64::MAX);
        let before = i64::try_from(self.bytes_before).unwrap_or(i64::MAX);
        after.saturating_sub(before)
    }

    /// Returns the line range the change produced, absent when nothing changed.
    #[must_use]
    pub const fn span(&self) -> Option<ChangeSpan> {
        self.span
    }

    /// Returns how many matches were replaced.
    #[must_use]
    pub const fn replacements(&self) -> usize {
        self.replacements
    }

    /// Returns true when the mutation created the file.
    #[must_use]
    pub const fn created(&self) -> bool {
        self.created
    }
}

/// Describes an applied mutation in one bounded phrase.
pub(crate) fn detail(applied: &Applied) -> String {
    let size = if applied.created() {
        format!("new file, {} bytes", applied.bytes_after())
    } else {
        match applied.byte_delta() {
            0 => "0 bytes".to_owned(),
            delta => format!("{delta:+} bytes"),
        }
    };
    match applied.span() {
        None => format!("{size}, no textual change"),
        Some(span) if span.first_line == span.last_line => {
            format!("{size}, line {}", span.first_line)
        }
        Some(span) => format!("{size}, lines {}-{}", span.first_line, span.last_line),
    }
}

/// Resolves a tool-supplied path against the workspace.
///
/// Whether the path may be reached at all is a policy decision the caller has
/// already made, so this only turns a relative path into an absolute one and
/// rejects input that cannot name a file.
pub(crate) fn resolve(context: &ExecutionContext, raw: &str) -> Result<Utf8PathBuf> {
    if raw.is_empty() {
        return Err(RuneError::invalid_field("path", "must not be empty"));
    }
    if raw.contains('\0') {
        return Err(RuneError::invalid_field(
            "path",
            "must not contain a NUL byte",
        ));
    }
    let path = Utf8Path::new(raw);
    if path.is_absolute() {
        return Ok(path.to_owned());
    }
    Ok(context.workspace().join(path))
}

/// Reads a required string argument.
pub(crate) fn required_string<'a>(
    tool: &str,
    arguments: &'a serde_json::Value,
    name: &str,
) -> Result<&'a str> {
    let field = format!("{tool} arguments.{name}");
    let value = arguments
        .get(name)
        .ok_or_else(|| RuneError::missing_field(field.clone()))?;
    value
        .as_str()
        .ok_or_else(|| RuneError::invalid_field(field, "expected a string"))
}

/// Prepares a whole-file write.
///
/// `path` is expected to be resolved already. A missing file is not an error:
/// the mutation then requires the path to stay missing until it is applied.
pub fn prepare(path: &Utf8Path, desired: &str) -> Result<Prepared> {
    if desired.len() > MAX_WRITE_BYTES {
        return Err(RuneError::too_large(
            "content",
            desired.len(),
            MAX_WRITE_BYTES,
        ));
    }
    let current = read_preimage(path)?;
    let identity = current.as_ref().map(|snapshot| snapshot.identity);
    let hash = current
        .as_ref()
        .map_or_else(|| hash_of(&[]), |snapshot| snapshot.hash);
    let permissions = current
        .as_ref()
        .map(|snapshot| snapshot.permissions.clone());
    let bytes: &[u8] = current
        .as_ref()
        .map_or(&[][..], |snapshot| snapshot.bytes.as_slice());
    let total = bytes.len().saturating_add(desired.len());
    if total > MAX_TOTAL_BYTES {
        return Err(RuneError::too_large("content", total, MAX_TOTAL_BYTES));
    }
    let before = std::str::from_utf8(bytes).map_err(|_| not_text(path))?;
    let region = changed_region(before, desired);
    Ok(Prepared {
        resolved: path.to_owned(),
        identity,
        hash,
        preimage_len: before.len(),
        postimage: desired.to_owned().into_bytes(),
        permissions,
        preview: preview_of(before, desired, region.as_ref()),
        span: region.as_ref().map(|region| region.span),
        replacements: 1,
    })
}

/// Prepares a replacement of `old` with `new`.
///
/// Matching is byte exact and non-overlapping. `occurrence` decides what
/// happens when the text appears more than once.
pub fn prepare_edit(
    path: &Utf8Path,
    old: &str,
    new: &str,
    occurrence: Occurrence,
) -> Result<Prepared> {
    if old.is_empty() {
        return Err(RuneError::invalid_field("old_string", "must not be empty"));
    }
    if old == new {
        return Err(RuneError::invalid_field(
            "new_string",
            "is identical to `old_string`, so the edit would change nothing",
        )
        .with_hint("pass different text, or leave the file alone"));
    }
    let Some(snapshot) = read_preimage(path)? else {
        return Err(
            RuneError::new(ErrorCode::NotFound, format!("`{path}` does not exist"))
                .with_hint("create the file with `write_file` first"),
        );
    };
    let before = std::str::from_utf8(&snapshot.bytes).map_err(|_| not_text(path))?;
    let count = count_occurrences(before, old);
    if count == 0 {
        return Err(no_match(path));
    }
    let (postimage, replacements) = match occurrence {
        Occurrence::Unique => {
            if count != 1 {
                return Err(ambiguous(path, count));
            }
            let Some(replaced) = replace_one(before, old, new, 1) else {
                return Err(no_match(path));
            };
            (replaced, 1)
        }
        Occurrence::All => (before.replace(old, new), count),
        Occurrence::Index(index) => {
            if index == 0 {
                return Err(index_out_of_range(path, index, count));
            }
            let Some(replaced) = replace_one(before, old, new, index) else {
                return Err(index_out_of_range(path, index, count));
            };
            (replaced, 1)
        }
    };
    if postimage.len() > MAX_WRITE_BYTES {
        return Err(RuneError::too_large(
            "content",
            postimage.len(),
            MAX_WRITE_BYTES,
        ));
    }
    let total = before.len().saturating_add(postimage.len());
    if total > MAX_TOTAL_BYTES {
        return Err(RuneError::too_large("content", total, MAX_TOTAL_BYTES));
    }
    let region = changed_region(before, &postimage);
    let preview = preview_of(before, &postimage, region.as_ref());
    Ok(Prepared {
        resolved: path.to_owned(),
        identity: Some(snapshot.identity),
        hash: snapshot.hash,
        preimage_len: before.len(),
        postimage: postimage.into_bytes(),
        permissions: Some(snapshot.permissions.clone()),
        preview,
        span: region.as_ref().map(|region| region.span),
        replacements,
    })
}

/// Applies a prepared mutation.
///
/// The postimage is staged next to the target, flushed, revalidated against the
/// preimage, and renamed into place. Every failure path removes the staged file
/// and leaves the target untouched.
pub fn apply(prepared: Prepared) -> Result<Applied> {
    let Prepared {
        resolved,
        identity,
        hash,
        preimage_len,
        postimage,
        permissions,
        span,
        replacements,
        ..
    } = prepared;
    let Some(directory) = resolved
        .parent()
        .filter(|parent| !parent.as_str().is_empty())
    else {
        return Err(RuneError::invalid_field(
            "path",
            format!("`{resolved}` has no parent directory"),
        ));
    };
    let directory = directory.to_owned();
    if !directory.exists() {
        std::fs::create_dir_all(directory.as_std_path())
            .map_err(|err| io_error(&directory, &err))?;
    }
    let (staged, mut file) = create_stage(&directory)?;
    let mut guard = StageGuard::new(staged.clone());
    if let Some(permissions) = permissions {
        file.set_permissions(permissions)
            .map_err(|err| io_error(&staged, &err))?;
    }
    file.write_all(&postimage)
        .map_err(|err| io_error(&staged, &err))?;
    file.sync_all().map_err(|err| io_error(&staged, &err))?;
    drop(file);
    revalidate(&resolved, identity.as_ref(), &hash)?;
    std::fs::rename(staged.as_std_path(), resolved.as_std_path())
        .map_err(|err| io_error(&resolved, &err))?;
    guard.disarm();
    sync_directory(&directory);
    Ok(Applied {
        path: resolved,
        bytes_before: preimage_len,
        bytes_after: postimage.len(),
        span,
        replacements,
        created: identity.is_none(),
    })
}

/// Maps an I/O failure to a typed error that names the path involved.
///
/// The generic conversion cannot know which path failed, and its fallback code
/// describes a network stream rather than a file.
fn io_error(path: &Utf8Path, err: &std::io::Error) -> RuneError {
    let code = match err.kind() {
        ErrorKind::NotFound => ErrorCode::NotFound,
        ErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
        ErrorKind::AlreadyExists => ErrorCode::AlreadyExists,
        ErrorKind::IsADirectory | ErrorKind::NotADirectory | ErrorKind::InvalidInput => {
            ErrorCode::InvalidField
        }
        _ => ErrorCode::Internal,
    };
    RuneError::new(code, format!("`{path}`: {err}")).with_observed(err.to_string())
}

/// Reads the current bytes of a file, or `None` when it does not exist.
///
/// A symlink or a multiply-linked file is refused: the first hides what is
/// actually being replaced, and the second would leave the other link pointing
/// at the old content.
fn read_preimage(path: &Utf8Path) -> Result<Option<Snapshot>> {
    let metadata = match std::fs::symlink_metadata(path.as_std_path()) {
        Ok(metadata) => metadata,
        // A path whose parent is not a directory names no file, which is the
        // same starting state as a path that does not exist yet.
        Err(err) if matches!(err.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            return Ok(None);
        }
        Err(err) => return Err(io_error(path, &err)),
    };
    if metadata.file_type().is_symlink() {
        return Err(
            RuneError::new(ErrorCode::UnsafePath, format!("`{path}` is a symlink"))
                .with_invariant("target_is_not_a_symlink"),
        );
    }
    if !metadata.is_file() {
        return Err(RuneError::invalid_field(
            "path",
            format!("`{path}` is not a regular file"),
        ));
    }
    if is_multilinked(&metadata) {
        return Err(RuneError::new(
            ErrorCode::UnsafePath,
            format!("`{path}` has more than one hard link"),
        )
        .with_invariant("link_count_is_one"));
    }
    let observed = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
    if observed > MAX_TOTAL_BYTES {
        return Err(RuneError::too_large(
            path.as_str(),
            observed,
            MAX_TOTAL_BYTES,
        ));
    }
    let file = File::open(path.as_std_path()).map_err(|err| io_error(path, &err))?;
    let metadata = file.metadata().map_err(|err| io_error(path, &err))?;
    let identity = identity_of(&metadata);
    let permissions = metadata.permissions();
    let limit = u64::try_from(MAX_TOTAL_BYTES.saturating_add(1)).unwrap_or(u64::MAX);
    let mut bytes = Vec::with_capacity(observed.min(MAX_TOTAL_BYTES));
    file.take(limit)
        .read_to_end(&mut bytes)
        .map_err(|err| io_error(path, &err))?;
    if bytes.len() > MAX_TOTAL_BYTES {
        return Err(RuneError::too_large(
            path.as_str(),
            bytes.len(),
            MAX_TOTAL_BYTES,
        ));
    }
    let hash = hash_of(&bytes);
    Ok(Some(Snapshot {
        identity,
        hash,
        permissions,
        bytes,
    }))
}

/// Returns the current identity and content hash, or `None` when the file is
/// gone. The bytes are streamed rather than collected, so a file that grew
/// after being read is still reported as stale instead of oversized.
fn identity_and_hash(path: &Utf8Path) -> Result<Option<(FileIdentity, [u8; 32])>> {
    let mut file = match File::open(path.as_std_path()) {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(io_error(path, &err)),
    };
    let identity = identity_of(&file.metadata().map_err(|err| io_error(path, &err))?);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; HASH_BUFFER_BYTES].into_boxed_slice();
    loop {
        let read = file.read(&mut buffer).map_err(|err| io_error(path, &err))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(Some((identity, hasher.finalize().into())))
}

/// Refuses a mutation whose target is no longer the file that was read.
fn revalidate(path: &Utf8Path, identity: Option<&FileIdentity>, hash: &[u8; 32]) -> Result<()> {
    let current = identity_and_hash(path)?;
    let unchanged = match (&current, identity) {
        (None, None) => true,
        (Some((current_identity, current_hash)), Some(expected)) => {
            current_identity == expected && current_hash == hash
        }
        _ => false,
    };
    if unchanged { Ok(()) } else { Err(stale(path)) }
}

#[cfg(unix)]
fn identity_of(metadata: &std::fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt;

    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

#[cfg(not(unix))]
fn identity_of(metadata: &std::fs::Metadata) -> FileIdentity {
    // Without device and inode numbers the only pair available is length and
    // modification time, which the preimage hash already covers.
    FileIdentity {
        device: 0,
        inode: metadata.len(),
    }
}

#[cfg(unix)]
fn is_multilinked(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    metadata.nlink() > 1
}

#[cfg(not(unix))]
const fn is_multilinked(_metadata: &std::fs::Metadata) -> bool {
    false
}

/// Creates a staging file with a unique name in the target directory.
fn create_stage(directory: &Utf8Path) -> Result<(Utf8PathBuf, File)> {
    for _ in 0..STAGE_ATTEMPTS {
        let candidate = stage_name(directory);
        let opened = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(candidate.as_std_path());
        match opened {
            Ok(file) => return Ok((candidate, file)),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {}
            Err(err) => return Err(io_error(&candidate, &err)),
        }
    }
    Err(RuneError::new(
        ErrorCode::AlreadyExists,
        format!("could not allocate a staging file in `{directory}`"),
    ))
}

/// Builds a staging name that no other mutation in this process will pick.
fn stage_name(directory: &Utf8Path) -> Utf8PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let name = format!(
        "{STAGE_PREFIX}{:x}-{nanos:x}-{sequence:x}",
        std::process::id()
    );
    directory.join(name)
}

/// Flushes the directory entry so the rename survives a crash.
fn sync_directory(directory: &Utf8Path) {
    // The rename is already committed, so a failed flush would only widen the
    // crash window and must not be reported as a failed mutation.
    #[cfg(unix)]
    if let Ok(handle) = File::open(directory.as_std_path()) {
        let _ = handle.sync_all();
    }
    #[cfg(not(unix))]
    let _ = directory;
}

/// The changed region of two texts, in bytes and in lines.
struct ChangedRegion {
    before: Range<usize>,
    after: Range<usize>,
    span: ChangeSpan,
}

/// Locates the changed region of two texts.
///
/// The comparison is over raw bytes, terminator included, so a line that only
/// changed its terminator counts as changed. The span is taken from the
/// resulting text because that is what the caller shows.
fn changed_region(before: &str, after: &str) -> Option<ChangedRegion> {
    if before == after {
        return None;
    }
    let max = before.len().min(after.len());
    let mut prefix = 0usize;
    while prefix < max && before.as_bytes().get(prefix) == after.as_bytes().get(prefix) {
        prefix = prefix.saturating_add(1);
    }
    let mut suffix = 0usize;
    while suffix < max.saturating_sub(prefix) {
        let left = before
            .as_bytes()
            .get(before.len().saturating_sub(1).saturating_sub(suffix));
        let right = after
            .as_bytes()
            .get(after.len().saturating_sub(1).saturating_sub(suffix));
        if left != right || left.is_none() {
            break;
        }
        suffix = suffix.saturating_add(1);
    }
    let before_range =
        floor_boundary(before, prefix)..ceil_boundary(before, before.len().saturating_sub(suffix));
    let after_range =
        floor_boundary(after, prefix)..ceil_boundary(after, after.len().saturating_sub(suffix));
    let start_line = terminators_in(&after[..after_range.start]).saturating_add(1);
    let after_lines = fragment_lines(&after[after_range.clone()]);
    let last_line = if after_lines == 0 {
        start_line
    } else {
        start_line.saturating_add(after_lines).saturating_sub(1)
    };
    Some(ChangedRegion {
        before: before_range,
        after: after_range,
        span: ChangeSpan {
            first_line: start_line,
            last_line,
        },
    })
}

/// Renders a bounded preview of the changed region.
fn preview_of(before: &str, after: &str, region: Option<&ChangedRegion>) -> Preview {
    let Some(region) = region else {
        return Preview::default();
    };
    let removed = &before[region.before.clone()];
    let added = &after[region.after.clone()];
    let (removed_cap, added_cap) = if removed.is_empty() {
        (0, PREVIEW_LINES)
    } else if added.is_empty() {
        (PREVIEW_LINES, 0)
    } else {
        (PREVIEW_HALF, PREVIEW_LINES.saturating_sub(PREVIEW_HALF))
    };
    let mut text = String::new();
    // A cut line is as much of an omission as a dropped one, so both mark the
    // preview truncated.
    let mut truncated = false;
    let complete = render_side(&mut text, '-', removed, removed_cap, &mut truncated)
        && render_side(&mut text, '+', added, added_cap, &mut truncated);
    if complete {
        return Preview { text, truncated };
    }
    let cut = floor_boundary(&text, PREVIEW_BYTES);
    text.truncate(cut);
    Preview {
        text,
        truncated: true,
    }
}

/// Appends up to `cap` lines, reporting whether every line fit.
fn render_side(
    text: &mut String,
    marker: char,
    body: &str,
    cap: usize,
    truncated: &mut bool,
) -> bool {
    if body.is_empty() {
        return true;
    }
    let mut lines = body.split_inclusive('\n');
    let mut kept = 0usize;
    while kept < cap {
        let Some(line) = lines.next() else {
            return true;
        };
        if !push_line(text, marker, line, truncated) {
            return false;
        }
        kept = kept.saturating_add(1);
    }
    lines.next().is_none()
}

/// Appends one marked line, reporting false once the byte budget is spent.
fn push_line(text: &mut String, marker: char, line: &str, truncated: &mut bool) -> bool {
    if text.len() >= PREVIEW_BYTES {
        return false;
    }
    let body = line.strip_suffix('\n').unwrap_or(line);
    let body = body.strip_suffix('\r').unwrap_or(body);
    if !text.is_empty() {
        text.push('\n');
    }
    text.push(marker);
    text.push(' ');
    let clipped = truncate_chars(body, PREVIEW_LINE_CHARS);
    if clipped.len() < body.len() {
        *truncated = true;
    }
    text.push_str(clipped);
    true
}

/// Moves an offset back to a character boundary.
///
/// Two texts can differ inside one multi-byte character, and every offset used
/// for slicing has to fall on a boundary.
fn floor_boundary(text: &str, offset: usize) -> usize {
    let mut index = offset.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index = index.saturating_sub(1);
    }
    index
}

/// Moves an offset forward to a character boundary.
fn ceil_boundary(text: &str, offset: usize) -> usize {
    let mut index = offset.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index = index.saturating_add(1);
    }
    index
}

/// Counts the line terminators in a fragment.
///
/// A line ends at a line feed, or at a carriage return that does not begin a
/// CRLF pair. Counting lone carriage returns is what makes the reported range
/// honest for a file that uses them as its line ending.
fn terminators_in(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut count = 0usize;
    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            b'\n' => count = count.saturating_add(1),
            b'\r' if bytes.get(index.saturating_add(1)) != Some(&b'\n') => {
                count = count.saturating_add(1);
            }
            _ => {}
        }
    }
    count
}

/// Returns true when the fragment ends on a line terminator.
fn ends_with_terminator(text: &str) -> bool {
    text.ends_with('\n') || text.ends_with('\r')
}

/// Counts the lines a fragment covers, terminator included.
///
/// A fragment that ends on a terminator closes a line rather than opening an
/// empty one, which is what makes a replaced line report one line.
fn fragment_lines(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let closed = terminators_in(text);
    if ends_with_terminator(text) {
        closed
    } else {
        closed.saturating_add(1)
    }
}

/// Keeps at most `max` characters.
fn truncate_chars(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((index, _)) => &text[..index],
        None => text,
    }
}

/// Counts non-overlapping occurrences of `needle`.
fn count_occurrences(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    haystack.match_indices(needle).count()
}

/// Replaces only the occurrence at the given one-based index.
fn replace_one(haystack: &str, needle: &str, replacement: &str, index: usize) -> Option<String> {
    let mut seen = 0usize;
    for (offset, _) in haystack.match_indices(needle) {
        seen = seen.saturating_add(1);
        if seen == index {
            let mut out = String::with_capacity(
                haystack
                    .len()
                    .saturating_sub(needle.len())
                    .saturating_add(replacement.len()),
            );
            out.push_str(&haystack[..offset]);
            out.push_str(replacement);
            out.push_str(&haystack[offset.saturating_add(needle.len())..]);
            return Some(out);
        }
    }
    None
}

/// Hashes bytes with SHA-256.
fn hash_of(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// A mutation whose target is no longer the file that was read.
fn stale(path: &Utf8Path) -> RuneError {
    RuneError::new(
        ErrorCode::StalePreimage,
        format!("`{path}` changed after it was read"),
    )
    .with_invariant("preimage_unchanged")
    .with_hint("read the file again and retry the change")
}

/// The text to replace does not appear in the target.
fn no_match(path: &Utf8Path) -> RuneError {
    RuneError::new(
        ErrorCode::NotFound,
        format!("`old_string` was not found in `{path}`"),
    )
    .with_hint("read the file again to see its current contents")
}

/// The text to replace appears more than once and no selector was given.
fn ambiguous(path: &Utf8Path, count: usize) -> RuneError {
    RuneError::new(
        ErrorCode::AmbiguousMatch,
        format!("the requested text occurs {count} times in `{path}`"),
    )
    .with_observed(count.to_string())
    .with_hint("pass `occurrence` to pick one, or `replace_all` to change every one")
}

/// The requested occurrence does not exist in the target.
fn index_out_of_range(path: &Utf8Path, index: usize, count: usize) -> RuneError {
    RuneError::invalid_field(
        "occurrence",
        format!("{index} is outside the {count} occurrences in `{path}`"),
    )
    .with_observed(count.to_string())
    .with_hint("pass an index of 1 or more and at most the occurrence count")
}

/// The target is not a text file, so a text mutation cannot describe it.
fn not_text(path: &Utf8Path) -> RuneError {
    RuneError::new(
        ErrorCode::InvalidState,
        format!("`{path}` is not valid UTF-8"),
    )
    .with_invariant("preimage_is_text")
    .with_hint("this tool changes text files; use a command for binary content")
}

/// The bytes that were read before a mutation.
struct Snapshot {
    identity: FileIdentity,
    hash: [u8; 32],
    permissions: Permissions,
    bytes: Vec<u8>,
}

/// Removes a staging file whose rename never happened.
struct StageGuard {
    path: Option<Utf8PathBuf>,
}

impl StageGuard {
    fn new(path: Utf8PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for StageGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            // The mutation already failed, so a failed cleanup must not replace
            // the error the caller needs to see.
            let _ = std::fs::remove_file(path.as_std_path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> tempfile::TempDir {
        tempfile::TempDir::new().expect("temp dir")
    }

    fn file_in(dir: &tempfile::TempDir, name: &str) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(dir.path().join(name)).expect("utf8 path")
    }

    fn staging_files(dir: &tempfile::TempDir) -> Vec<String> {
        std::fs::read_dir(dir.path())
            .expect("read dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|name| name.starts_with(STAGE_PREFIX))
            .collect()
    }

    #[test]
    fn a_write_creates_a_missing_file() {
        let dir = dir();
        let path = file_in(&dir, "new.txt");
        let prepared = prepare(&path, "hello\n").expect("prepare");
        assert!(prepared.creates_file());
        assert_eq!(prepared.preimage_len(), 0);
        assert_eq!(
            prepared.span(),
            Some(ChangeSpan {
                first_line: 1,
                last_line: 1
            })
        );
        let applied = apply(prepared).expect("apply");
        assert!(applied.created());
        assert_eq!(applied.byte_delta(), 6);
        assert_eq!(std::fs::read(&path).expect("read"), b"hello\n");
        assert!(staging_files(&dir).is_empty());
    }

    #[test]
    fn a_write_creates_missing_parent_directories() {
        let dir = dir();
        let path = file_in(&dir, "nested/deeper/new.txt");
        let applied = apply(prepare(&path, "x").expect("prepare")).expect("apply");
        assert!(applied.created());
        assert_eq!(std::fs::read(&path).expect("read"), b"x");
    }

    #[test]
    fn a_write_replaces_an_existing_file_and_reports_the_delta() {
        let dir = dir();
        let path = file_in(&dir, "note.txt");
        std::fs::write(&path, "one\ntwo\n").expect("seed");
        let prepared = prepare(&path, "one\ntwo\nthree\n").expect("prepare");
        assert!(!prepared.creates_file());
        assert_eq!(prepared.preimage_len(), 8);
        assert_eq!(
            prepared.span(),
            Some(ChangeSpan {
                first_line: 3,
                last_line: 3
            })
        );
        let applied = apply(prepared).expect("apply");
        assert_eq!(applied.byte_delta(), 6);
        assert_eq!(applied.bytes_before(), 8);
        assert_eq!(applied.bytes_after(), 14);
        assert!(staging_files(&dir).is_empty());
    }

    #[test]
    fn a_changed_file_is_refused_and_left_alone() {
        let dir = dir();
        let path = file_in(&dir, "note.txt");
        std::fs::write(&path, "one\n").expect("seed");
        let prepared = prepare(&path, "two\n").expect("prepare");
        std::fs::write(&path, "changed elsewhere\n").expect("interfere");

        let err = apply(prepared).expect_err("stale");
        assert_eq!(err.code(), ErrorCode::StalePreimage);
        assert!(err.message().contains("note.txt"), "{}", err.message());
        assert_eq!(std::fs::read(&path).expect("read"), b"changed elsewhere\n");
        assert!(staging_files(&dir).is_empty());
    }

    #[test]
    fn a_replacement_carrying_identical_bytes_is_refused() {
        let dir = dir();
        let path = file_in(&dir, "swap.txt");
        std::fs::write(&path, "same\n").expect("seed");
        let prepared = prepare(&path, "same\n").expect("prepare");

        let other = file_in(&dir, "other.txt");
        std::fs::write(&other, "same\n").expect("seed");
        std::fs::rename(&other, &path).expect("swap");

        let err = apply(prepared).expect_err("stale");
        assert_eq!(err.code(), ErrorCode::StalePreimage);
        assert_eq!(std::fs::read(&path).expect("read"), b"same\n");
        assert!(staging_files(&dir).is_empty());
    }

    #[test]
    fn a_prepared_creation_is_refused_when_the_file_appears() {
        let dir = dir();
        let path = file_in(&dir, "race.txt");
        let prepared = prepare(&path, "mine\n").expect("prepare");
        std::fs::write(&path, "theirs\n").expect("interfere");
        let err = apply(prepared).expect_err("stale");
        assert_eq!(err.code(), ErrorCode::StalePreimage);
        assert_eq!(std::fs::read(&path).expect("read"), b"theirs\n");
        assert!(staging_files(&dir).is_empty());
    }

    #[test]
    fn crlf_content_round_trips_without_normalization() {
        let dir = dir();
        let path = file_in(&dir, "crlf.txt");
        std::fs::write(&path, "alpha\r\nbeta\r\n").expect("seed");
        let prepared = prepare(&path, "alpha\r\nBETA\r\ngamma\r\n").expect("prepare");
        apply(prepared).expect("apply");
        assert_eq!(
            std::fs::read(&path).expect("read"),
            b"alpha\r\nBETA\r\ngamma\r\n"
        );
    }

    #[test]
    fn a_lone_carriage_return_survives_a_write() {
        let dir = dir();
        let path = file_in(&dir, "cr.txt");
        std::fs::write(&path, "alpha\rbeta\rgamma").expect("seed");
        let prepared = prepare(&path, "alpha\rBETA\rgamma").expect("prepare");
        apply(prepared).expect("apply");
        assert_eq!(std::fs::read(&path).expect("read"), b"alpha\rBETA\rgamma");
    }

    #[test]
    fn an_edit_preserves_every_byte_it_did_not_match() {
        let dir = dir();
        let path = file_in(&dir, "crlf.txt");
        std::fs::write(&path, "alpha\r\nbeta\r\ngamma\r\n").expect("seed");
        let prepared = prepare_edit(&path, "beta", "BETA", Occurrence::Unique).expect("prepare");
        assert_eq!(
            prepared.span(),
            Some(ChangeSpan {
                first_line: 2,
                last_line: 2
            })
        );
        let applied = apply(prepared).expect("apply");
        assert_eq!(applied.replacements(), 1);
        assert_eq!(
            std::fs::read(&path).expect("read"),
            b"alpha\r\nBETA\r\ngamma\r\n"
        );
    }

    #[test]
    fn a_lone_carriage_return_file_keeps_its_ending() {
        let dir = dir();
        let path = file_in(&dir, "cr.txt");
        std::fs::write(&path, "alpha\rbeta\rgamma").expect("seed");
        let prepared = prepare_edit(&path, "beta", "BETA", Occurrence::Unique).expect("prepare");
        apply(prepared).expect("apply");
        assert_eq!(std::fs::read(&path).expect("read"), b"alpha\rBETA\rgamma");
    }

    #[test]
    fn replace_all_counts_every_occurrence() {
        let dir = dir();
        let path = file_in(&dir, "many.txt");
        std::fs::write(&path, "a b a b a\n").expect("seed");
        let prepared = prepare_edit(&path, "a", "x", Occurrence::All).expect("prepare");
        assert_eq!(prepared.replacements(), 3);
        let applied = apply(prepared).expect("apply");
        assert_eq!(applied.replacements(), 3);
        assert_eq!(std::fs::read(&path).expect("read"), b"x b x b x\n");
    }

    #[test]
    fn an_ambiguous_match_names_the_count() {
        let dir = dir();
        let path = file_in(&dir, "many.txt");
        std::fs::write(&path, "a b a b a\n").expect("seed");
        let err = prepare_edit(&path, "a", "x", Occurrence::Unique).expect_err("ambiguous");
        assert_eq!(err.code(), ErrorCode::AmbiguousMatch);
        assert!(err.message().contains('3'), "{}", err.message());
        assert_eq!(std::fs::read(&path).expect("read"), b"a b a b a\n");
    }

    #[test]
    fn a_single_match_needs_no_selector() {
        let dir = dir();
        let path = file_in(&dir, "one.txt");
        std::fs::write(&path, "a b c\n").expect("seed");
        let prepared = prepare_edit(&path, "b", "B", Occurrence::Unique).expect("prepare");
        apply(prepared).expect("apply");
        assert_eq!(std::fs::read(&path).expect("read"), b"a B c\n");
    }

    #[test]
    fn an_occurrence_outside_the_range_names_the_count() {
        let dir = dir();
        let path = file_in(&dir, "many.txt");
        std::fs::write(&path, "a b a\n").expect("seed");
        let err = prepare_edit(&path, "a", "x", Occurrence::Index(5)).expect_err("range");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert!(err.message().contains('2'), "{}", err.message());
        assert_eq!(err.field(), Some("occurrence"));
        assert_eq!(std::fs::read(&path).expect("read"), b"a b a\n");
    }

    #[test]
    fn a_named_occurrence_replaces_only_that_one() {
        let dir = dir();
        let path = file_in(&dir, "many.txt");
        std::fs::write(&path, "a b a\n").expect("seed");
        let prepared = prepare_edit(&path, "a", "x", Occurrence::Index(2)).expect("prepare");
        apply(prepared).expect("apply");
        assert_eq!(std::fs::read(&path).expect("read"), b"a b x\n");
    }

    #[test]
    fn a_missing_match_is_refused() {
        let dir = dir();
        let path = file_in(&dir, "one.txt");
        std::fs::write(&path, "a b c\n").expect("seed");
        let err = prepare_edit(&path, "zzz", "x", Occurrence::Unique).expect_err("missing");
        assert_eq!(err.code(), ErrorCode::NotFound);
    }

    #[test]
    fn a_no_op_edit_is_refused() {
        let dir = dir();
        let path = file_in(&dir, "one.txt");
        std::fs::write(&path, "a b c\n").expect("seed");
        let err = prepare_edit(&path, "b", "b", Occurrence::Unique).expect_err("no op");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.field(), Some("new_string"));
    }

    #[test]
    fn an_empty_search_string_is_refused() {
        let dir = dir();
        let path = file_in(&dir, "one.txt");
        std::fs::write(&path, "a\n").expect("seed");
        let err = prepare_edit(&path, "", "x", Occurrence::All).expect_err("empty");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.field(), Some("old_string"));
    }

    #[test]
    fn editing_a_missing_file_is_refused() {
        let dir = dir();
        let path = file_in(&dir, "absent.txt");
        let err = prepare_edit(&path, "a", "b", Occurrence::Unique).expect_err("missing");
        assert_eq!(err.code(), ErrorCode::NotFound);
    }

    #[test]
    fn a_directory_target_is_refused() {
        let dir = dir();
        let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let err = prepare(&path, "x").expect_err("directory");
        assert_eq!(err.code(), ErrorCode::InvalidField);
    }

    #[test]
    fn content_over_the_per_write_cap_is_refused() {
        let dir = dir();
        let path = file_in(&dir, "big.txt");
        let content = "x".repeat(MAX_WRITE_BYTES.saturating_add(1));
        let err = prepare(&path, &content).expect_err("too large");
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert_eq!(err.field(), Some("content"));
        assert!(!path.exists());
    }

    #[test]
    fn a_write_pair_over_the_total_cap_is_refused() {
        let dir = dir();
        let path = file_in(&dir, "big.txt");
        std::fs::write(&path, "y".repeat(7 * 1024 * 1024)).expect("seed");
        let content = "x".repeat(2 * 1024 * 1024);
        let err = prepare(&path, &content).expect_err("too large");
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert_eq!(
            std::fs::metadata(&path).expect("meta").len(),
            7 * 1024 * 1024
        );
    }

    #[test]
    fn a_preimage_over_the_total_cap_is_refused() {
        let dir = dir();
        let path = file_in(&dir, "big.txt");
        std::fs::write(&path, "y".repeat(MAX_TOTAL_BYTES.saturating_add(1))).expect("seed");
        let err = prepare(&path, "x").expect_err("too large");
        assert_eq!(err.code(), ErrorCode::TooLarge);
    }

    #[test]
    fn the_preview_is_bounded_to_a_few_lines() {
        let dir = dir();
        let path = file_in(&dir, "big.txt");
        std::fs::write(&path, "a\nb\nc\n").expect("seed");
        let content = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
        let prepared = prepare(&path, content).expect("prepare");
        let preview = prepared.preview();
        assert!(preview.truncated, "{}", preview.text);
        assert!(
            preview.text.lines().count() <= PREVIEW_LINES,
            "{}",
            preview.text
        );
        assert!(preview.text.len() <= PREVIEW_BYTES);
    }

    #[test]
    fn a_preview_that_fits_is_not_marked_truncated() {
        let dir = dir();
        let path = file_in(&dir, "small.txt");
        std::fs::write(&path, "a\nb\n").expect("seed");
        let prepared = prepare(&path, "a\nB\n").expect("prepare");
        assert!(!prepared.preview().truncated);
        assert_eq!(prepared.preview().text, "- b\n+ B");
    }

    #[test]
    fn an_unchanged_write_reports_no_span() {
        let dir = dir();
        let path = file_in(&dir, "same.txt");
        std::fs::write(&path, "a\n").expect("seed");
        let prepared = prepare(&path, "a\n").expect("prepare");
        assert_eq!(prepared.span(), None);
        assert_eq!(prepared.preview().text, "");
    }

    #[test]
    fn a_deletion_collapses_the_span_onto_the_remaining_line() {
        let dir = dir();
        let path = file_in(&dir, "del.txt");
        std::fs::write(&path, "a\nb\nc\n").expect("seed");
        let prepared = prepare(&path, "a\nc\n").expect("prepare");
        assert_eq!(
            prepared.span(),
            Some(ChangeSpan {
                first_line: 2,
                last_line: 2
            })
        );
    }

    #[test]
    fn a_binary_preimage_is_refused() {
        let dir = dir();
        let path = file_in(&dir, "blob.bin");
        std::fs::write(&path, [0xffu8, 0xfe, 0x00, 0x01]).expect("seed");
        let err = prepare(&path, "text\n").expect_err("binary");
        assert_eq!(err.code(), ErrorCode::InvalidState);
        assert_eq!(
            std::fs::read(&path).expect("read"),
            [0xffu8, 0xfe, 0x00, 0x01]
        );
    }

    #[test]
    fn a_relative_path_resolves_against_the_workspace() {
        let dir = dir();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let context = ExecutionContext::new(workspace);
        let resolved = resolve(&context, "nested/note.txt").expect("resolve");
        assert_eq!(
            resolved,
            Utf8PathBuf::from_path_buf(dir.path().join("nested/note.txt")).expect("utf8")
        );
        assert!(resolve(&context, "").is_err());
    }

    #[test]
    fn a_missing_argument_names_the_tool_and_field() {
        let err =
            required_string("write_file", &serde_json::json!({}), "path").expect_err("missing");
        assert_eq!(err.code(), ErrorCode::MissingField);
        assert!(err.message().contains("write_file arguments.path"));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_target_is_refused() {
        let dir = dir();
        let real = file_in(&dir, "real.txt");
        std::fs::write(&real, "content\n").expect("seed");
        let link = file_in(&dir, "link.txt");
        std::os::unix::fs::symlink(real.as_std_path(), link.as_std_path()).expect("symlink");
        let err = prepare(&link, "x").expect_err("symlink");
        assert_eq!(err.code(), ErrorCode::UnsafePath);
        assert_eq!(std::fs::read(&real).expect("read"), b"content\n");
    }

    #[cfg(unix)]
    #[test]
    fn a_hard_linked_target_is_refused() {
        let dir = dir();
        let first = file_in(&dir, "first.txt");
        std::fs::write(&first, "content\n").expect("seed");
        let second = file_in(&dir, "second.txt");
        std::fs::hard_link(first.as_std_path(), second.as_std_path()).expect("link");
        let err = prepare(&first, "x").expect_err("hard link");
        assert_eq!(err.code(), ErrorCode::UnsafePath);
        assert_eq!(std::fs::read(&first).expect("read"), b"content\n");
    }

    #[test]
    fn the_staging_file_sits_beside_its_target() {
        // A staging file on another filesystem could not be renamed into place,
        // so the same-directory property is what makes the apply atomic.
        let dir = dir();
        let path = file_in(&dir, "note.txt");
        let staged = stage_name(path.parent().expect("parent"));
        assert_eq!(staged.parent(), path.parent());
        assert!(staged.file_name().expect("name").starts_with(STAGE_PREFIX));
        assert_ne!(stage_name(path.parent().expect("parent")), staged);
    }

    #[test]
    fn a_failed_apply_leaves_no_staging_file_and_leaves_the_target_alone() {
        // The staging directory cannot be created because a file already holds
        // that name, so apply fails before it can rename anything.
        let dir = dir();
        let blocker = file_in(&dir, "blocker");
        std::fs::write(&blocker, "original\n").expect("seed");
        let target = file_in(&dir, "blocker/note.txt");
        let prepared = prepare(&target, "replacement\n").expect("prepare");

        let err = apply(prepared).expect_err("unwritable parent");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert!(err.message().contains("blocker"), "{}", err.message());
        assert_eq!(std::fs::read(&blocker).expect("read"), b"original\n");
        assert!(!target.exists());
        assert!(staging_files(&dir).is_empty());
    }

    #[test]
    fn an_edit_over_the_per_write_cap_is_refused() {
        let dir = dir();
        let path = file_in(&dir, "note.txt");
        std::fs::write(&path, "seed\n").expect("seed");
        let growth = "x".repeat(MAX_WRITE_BYTES.saturating_add(1));
        let err = prepare_edit(&path, "seed", &growth, Occurrence::Unique).expect_err("too large");
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert_eq!(std::fs::read(&path).expect("read"), b"seed\n");
        assert!(staging_files(&dir).is_empty());
    }

    #[test]
    fn residue_from_a_crashed_apply_is_left_alone_by_the_next_apply() {
        // A staging file abandoned by a killed process must not be reused or
        // renamed over the target: staging names are unique per attempt.
        let dir = dir();
        let residue = file_in(&dir, ".rune-stage-deadbeef");
        std::fs::write(&residue, "abandoned\n").expect("seed");
        let target = file_in(&dir, "note.txt");
        std::fs::write(&target, "original\n").expect("seed");

        let applied = apply(prepare(&target, "replacement\n").expect("prepare")).expect("apply");

        assert_eq!(applied.bytes_after(), 12);
        assert_eq!(std::fs::read(&target).expect("read"), b"replacement\n");
        assert_eq!(
            std::fs::read(&residue).expect("read"),
            b"abandoned\n",
            "the pre-existing staging-named file was disturbed"
        );
    }

    #[test]
    fn a_single_enormous_line_is_truncated_in_the_preview() {
        let dir = dir();
        let path = file_in(&dir, "long.txt");
        std::fs::write(&path, "seed\n").expect("seed");
        let content = "y".repeat(8 * 1024);
        let prepared = prepare(&path, &content).expect("prepare");
        let preview = prepared.preview();
        assert!(
            preview.text.len() <= PREVIEW_BYTES,
            "{}",
            preview.text.len()
        );
        assert!(preview.truncated);
    }

    #[test]
    fn a_cr_only_file_reports_the_line_of_the_change() {
        let dir = dir();
        let path = file_in(&dir, "cr.txt");
        std::fs::write(&path, "alpha\rbeta\rgamma").expect("seed");
        let prepared = prepare_edit(&path, "beta", "BETA", Occurrence::Unique).expect("prepare");
        assert_eq!(
            prepared.span(),
            Some(ChangeSpan {
                first_line: 2,
                last_line: 2
            })
        );
    }

    #[test]
    fn a_crlf_insertion_reports_the_line_it_lands_on() {
        let dir = dir();
        let path = file_in(&dir, "dos.txt");
        std::fs::write(&path, "alpha\r\nbeta\r\ngamma\r\n").expect("seed");
        let prepared =
            prepare_edit(&path, "beta", "beta\r\nbeta", Occurrence::Unique).expect("prepare");
        // The added line is the third one; `gamma` shifts to the fourth.
        assert_eq!(
            prepared.span(),
            Some(ChangeSpan {
                first_line: 3,
                last_line: 3
            })
        );
        apply(prepared).expect("apply");
        assert_eq!(
            std::fs::read(&path).expect("read"),
            b"alpha\r\nbeta\r\nbeta\r\ngamma\r\n"
        );
    }

    #[test]
    fn a_repeated_edit_reports_the_new_span_after_the_first_change() {
        let dir = dir();
        let path = file_in(&dir, "note.txt");
        std::fs::write(&path, "one\ntwo\n").expect("seed");
        apply(prepare_edit(&path, "two", "TWO", Occurrence::Unique).expect("prepare"))
            .expect("apply");
        let second = prepare_edit(&path, "TWO", "two\nthree", Occurrence::Unique).expect("prepare");
        assert_eq!(
            second.span(),
            Some(ChangeSpan {
                first_line: 2,
                last_line: 3
            })
        );
        apply(second).expect("apply");
        assert_eq!(std::fs::read(&path).expect("read"), b"one\ntwo\nthree\n");
    }
}
