//! Bounded, line-numbered file reads.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader};

use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use rune_core::error::{ErrorCode, Result, RuneError};

use crate::contract::{Activity, ExecutionContext, Tool, ToolOutput, validate_closed_arguments};
use crate::workspace::{
    FileLimits, SUMMARY_RESERVE_BYTES, display_path, join_capped, read_line_bounded_checked,
    resolve, string_arg, truncate_line, usize_arg,
};

/// Bytes read to classify a file before any content is returned.
const PROBE_BYTES: usize = 8 * 1024;

/// Width of the line number column.
const NUMBER_WIDTH: usize = 6;

/// Lines shown when a window starts past the end of the file.
const TAIL_LINES: usize = 20;

/// Most paths suggested for a missing one.
const MAX_SUGGESTIONS: usize = 3;

/// Directory entries examined for a name that differs only in case.
const LISTING_LIMIT: usize = 1_000;

/// Reads a bounded window of lines from one text file.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReadFile {
    limits: FileLimits,
}

impl ReadFile {
    /// Builds a reader with the configured caps.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a reader with explicit caps.
    #[must_use]
    pub const fn with_limits(limits: FileLimits) -> Self {
        Self { limits }
    }
}

impl Tool for ReadFile {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn description(&self) -> &'static str {
        "Read a text file as numbered lines. Page a long file with start_line and line_count."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Relative to the workspace root, or absolute.",
                },
                "start_line": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-based.",
                },
                "line_count": {
                    "type": "integer",
                    "minimum": 1,
                },
            },
            "required": ["path"],
            "additionalProperties": false,
        })
    }

    fn activity(&self) -> Activity {
        Activity::Read
    }

    fn permission_target(&self, arguments: &serde_json::Value) -> Option<String> {
        arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    }

    fn call(
        &self,
        arguments: &serde_json::Value,
        context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        context.check_cancelled()?;
        validate_closed_arguments(arguments, &self.input_schema())?;
        let requested = string_arg(arguments, "path")?
            .ok_or_else(|| RuneError::missing_field("path"))?
            .to_owned();
        let start_line = usize_arg(arguments, "start_line")?.unwrap_or(1);
        if start_line == 0 {
            return Err(RuneError::invalid_field(
                "start_line",
                "start_line is 1-based, so 0 is not a line",
            ));
        }

        let count = usize_arg(arguments, "line_count")?
            .unwrap_or(self.limits.read_lines)
            .min(self.limits.read_lines);

        let resolved = resolve(context, &requested)
            .map_err(|err| suggest_outside(context, &requested, err))?;
        let display = display_path(context, &resolved.path);
        context.check_cancelled()?;
        let file = match open(&display, resolved.path.as_path()) {
            Ok(file) => file,
            Err(err) if err.code() == ErrorCode::NotFound => {
                let found = suggestions(context, &resolved.path);
                return Err(with_suggestions(err, &found));
            }
            Err(err) => return Err(err),
        };
        let size = file.metadata().map_or(0, |metadata| metadata.len());

        let mut reader = BufReader::with_capacity(PROBE_BYTES, file);
        let probe = reader.fill_buf().unwrap_or_default().to_vec();
        context.check_cancelled()?;
        if let Some(format) = image_format(&probe) {
            return Ok(ToolOutput::success(image_description(
                &display, format, size,
            )));
        }
        let content = classify(&probe);
        if content == Content::Binary {
            return Ok(ToolOutput::failure(format!(
                "`{display}` is binary ({size} bytes), so its bytes were not returned as text. \
                 Name a text file, or use a tool that decodes this format."
            )));
        }

        // Collection stops at the byte cap, but every line is still counted so
        // the footer can state the true total. The reserve keeps the header and
        // the footer readable even when nothing else fits.
        let cap = self.limits.output_cap(context);
        let byte_budget = cap.saturating_sub(SUMMARY_RESERVE_BYTES);
        let mut window = collect(
            &mut reader,
            start_line,
            count,
            self.limits.line_bytes,
            byte_budget,
            context,
        )?;
        if window.binary {
            return Ok(ToolOutput::failure(format!(
                "`{display}` contains binary data past its first {PROBE_BYTES} bytes, so its \
                 content was not returned as text."
            )));
        }
        window.lossy |= content == Content::Lossy;
        let (body, footer) = render(&display, size, &window, self.limits.line_bytes);
        context.check_cancelled()?;
        Ok(ToolOutput::success(join_capped(body, &footer, cap)))
    }
}

/// Opens a file for reading.
///
/// A directory is refused by name rather than by the read that follows, because
/// `open` succeeds on a directory on some platforms and fails later with an
/// unrelated error code.
fn open(display: &str, path: &Utf8Path) -> Result<std::fs::File> {
    let metadata = std::fs::metadata(path.as_std_path()).map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => {
            RuneError::new(ErrorCode::NotFound, format!("`{display}` does not exist"))
        }
        std::io::ErrorKind::PermissionDenied => RuneError::new(
            ErrorCode::PermissionDenied,
            format!("`{display}` cannot be read"),
        ),
        _ => RuneError::new(
            ErrorCode::InvalidState,
            format!("`{display}` could not be opened: {err}"),
        ),
    })?;
    if !metadata.is_file() {
        return Err(RuneError::invalid_field(
            "path",
            format!("`{display}` is not a regular file"),
        ));
    }
    std::fs::File::open(path.as_std_path()).map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => {
            RuneError::new(ErrorCode::NotFound, format!("`{display}` does not exist"))
        }
        std::io::ErrorKind::PermissionDenied => RuneError::new(
            ErrorCode::PermissionDenied,
            format!("`{display}` cannot be read"),
        ),
        _ => RuneError::new(
            ErrorCode::InvalidState,
            format!("`{display}` could not be opened: {err}"),
        ),
    })
}

/// Returns up to three existing paths the model may have meant by a missing one.
///
/// Candidates, in order: entries of the parent directory whose name differs
/// from the requested one only in case, then the requested path's trailing
/// components joined onto the workspace, longest first.
fn suggestions(context: &ExecutionContext, resolved: &Utf8Path) -> Vec<String> {
    let mut found = Vec::new();
    if let (Some(parent), Some(name)) = (resolved.parent(), resolved.file_name()) {
        let wanted = name.to_lowercase();
        let mut same_name = Vec::new();
        if let Ok(entries) = parent.read_dir_utf8() {
            for entry in entries.take(LISTING_LIMIT).flatten() {
                let candidate = entry.file_name();
                if candidate != name && candidate.to_lowercase() == wanted {
                    same_name.push(entry.path().to_owned());
                }
            }
        }
        same_name.sort();
        for path in same_name {
            push_unique(&mut found, path);
        }
    }
    workspace_suffixes(context, resolved, &mut found);
    found
        .iter()
        .take(MAX_SUGGESTIONS)
        .map(|path| display_path(context, path))
        .collect()
}

/// Adds each proper suffix of `path` that names a regular file in the workspace.
fn workspace_suffixes(context: &ExecutionContext, path: &Utf8Path, found: &mut Vec<Utf8PathBuf>) {
    let names = path
        .components()
        .filter_map(|component| match component {
            Utf8Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect::<Vec<_>>();
    let workspace = context.workspace();
    for start in 1..names.len() {
        let candidate = names[start..]
            .iter()
            .fold(workspace.to_owned(), |joined, name| joined.join(name));
        if candidate != path && candidate.is_file() {
            push_unique(found, candidate);
        }
    }
}

/// Appends a path unless it is already listed.
fn push_unique(found: &mut Vec<Utf8PathBuf>, path: Utf8PathBuf) {
    if !found.contains(&path) {
        found.push(path);
    }
}

/// Adds workspace suggestions to the refusal of a missing absolute path.
///
/// An absolute path from another machine or container, such as
/// `/app/src/main.rs`, lies outside every root, so it is refused before it is
/// opened; the workspace file it most likely means is named instead.
fn suggest_outside(context: &ExecutionContext, requested: &str, err: RuneError) -> RuneError {
    let path = Utf8Path::new(requested);
    if err.code() != ErrorCode::PathOutsideWorkspace
        || !path.is_absolute()
        || std::fs::symlink_metadata(path.as_std_path()).is_ok()
    {
        return err;
    }
    let mut found = Vec::new();
    workspace_suffixes(context, path, &mut found);
    let shown = found
        .iter()
        .take(MAX_SUGGESTIONS)
        .map(|path| display_path(context, path))
        .collect::<Vec<_>>();
    with_suggestions(err, &shown)
}

/// Attaches a did-you-mean hint when there is anything to suggest.
fn with_suggestions(err: RuneError, found: &[String]) -> RuneError {
    if found.is_empty() {
        return err;
    }
    let names = found
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ");
    err.with_hint(format!("did you mean: {names}?"))
}

/// The window of lines a read produced, with the true file size.
#[derive(Clone, Debug, Default)]
struct Window {
    /// Collected lines, as a number and the text to show.
    lines: Vec<(usize, String)>,
    /// True line count of the file.
    total: usize,
    /// First line number the caller asked for.
    requested_start: usize,
    /// True when the byte cap stopped collection early.
    byte_capped: bool,
    /// True when at least one line was cut at the per-line cap.
    line_capped: bool,
    /// True when a NUL byte appeared beyond the classification probe.
    binary: bool,
    /// True when some line is not valid UTF-8 and is shown with replacements.
    lossy: bool,
    /// True when the window started past the end and holds the last lines.
    past_end: bool,
}

/// Collects the window while counting every line in the file.
fn collect<R: BufRead>(
    reader: &mut R,
    start_line: usize,
    count: usize,
    line_bytes: usize,
    byte_budget: usize,
    context: &ExecutionContext,
) -> Result<Window> {
    let mut window = Window {
        requested_start: start_line,
        ..Window::default()
    };
    let mut buffer = Vec::new();
    let mut number = 1_usize;
    let mut bytes = 0_usize;
    // The last lines before `start_line`, kept so a window that starts past
    // the end can show the end of the file without a second read.
    let mut tail = VecDeque::with_capacity(TAIL_LINES);

    loop {
        let line = read_line_bounded_checked(reader, &mut buffer, line_bytes, || {
            context.check_cancelled()
        })?;
        if line.consumed == 0 {
            break;
        }
        // Only the leading bytes were classified, so every line is checked
        // before it can reach the result as mojibake.
        if line.has_nul {
            window.binary = true;
            break;
        }
        window.total = window.total.saturating_add(1);
        if !window.lossy && !decodes(&buffer, buffer.len() < line.consumed) {
            window.lossy = true;
        }

        if number < start_line {
            if tail.len() == TAIL_LINES {
                tail.pop_front();
            }
            let (shown, capped) = shown_line(&buffer, line.length, line_bytes);
            tail.push_back((number, shown, capped));
            number = number.saturating_add(1);
            continue;
        }
        if window.lines.len() >= count || window.byte_capped {
            number = number.saturating_add(1);
            continue;
        }

        let (shown, capped) = shown_line(&buffer, line.length, line_bytes);
        window.line_capped |= capped;
        bytes = bytes
            .saturating_add(shown.len())
            .saturating_add(NUMBER_WIDTH + 1);
        if bytes > byte_budget && !window.lines.is_empty() {
            window.byte_capped = true;
            number = number.saturating_add(1);
            continue;
        }
        window.lines.push((number, shown));
        number = number.saturating_add(1);
    }

    if window.lines.is_empty() && window.total > 0 && start_line > window.total {
        window.past_end = true;
        let keep = tail.len().min(count.max(1));
        let mut bytes = 0_usize;
        for (number, shown, capped) in tail.into_iter().rev().take(keep) {
            bytes = bytes
                .saturating_add(shown.len())
                .saturating_add(NUMBER_WIDTH + 1);
            if bytes > byte_budget && !window.lines.is_empty() {
                break;
            }
            window.line_capped |= capped;
            window.lines.push((number, shown));
        }
        window.lines.reverse();
    }
    Ok(window)
}

/// Returns a line as shown, and whether it was cut at the line cap.
fn shown_line(buffer: &[u8], length: usize, line_bytes: usize) -> (String, bool) {
    let text = String::from_utf8_lossy(buffer);
    let text = text.trim_end_matches(['\n', '\r']);
    (truncate_line(text, length, line_bytes), length > line_bytes)
}

/// Returns true when the bytes are valid UTF-8.
///
/// A `cut` line was kept only up to the line cap, so a sequence it ends in the
/// middle of is not counted as invalid.
fn decodes(bytes: &[u8], cut: bool) -> bool {
    match std::str::from_utf8(bytes) {
        Ok(_) => true,
        Err(err) => cut && err.error_len().is_none(),
    }
}

/// Renders the window body and the footer that states what was left out.
fn render(display: &str, size: u64, window: &Window, line_bytes: usize) -> (String, String) {
    let mut body = format!("{display}: {} lines, {size} bytes", window.total);
    if window.lossy {
        body.push_str(
            " [not valid UTF-8: shown with replacement characters; edit it with a command, not \
             edit_file]",
        );
    }
    body.push('\n');
    for (number, text) in &window.lines {
        let _ = writeln!(body, "{number:>NUMBER_WIDTH$}\t{text}");
    }

    let shown = window.lines.len();
    let last = window
        .lines
        .last()
        .map_or(window.requested_start.saturating_sub(1), |(number, _)| {
            *number
        });

    let start = window.requested_start;
    let total = window.total;
    let next = last.saturating_add(1);
    let mut footer = String::new();
    if total == 0 {
        footer.push_str("[the file is empty]\n");
    } else if window.past_end {
        let _ = writeln!(
            footer,
            "[the file has {total} lines; showing the last {shown}]"
        );
    } else if shown == 0 {
        let _ = writeln!(
            footer,
            "[no lines returned: the requested window starts at line {start} but the file has \
             {total} lines]"
        );
    } else if window.byte_capped {
        let _ = writeln!(
            footer,
            "[output truncated at the byte cap: showing lines {start} to {last} of {total}; pass \
             start_line={next} to continue]"
        );
    } else if last < total {
        let _ = writeln!(
            footer,
            "[showing lines {start} to {last} of {total}; pass start_line={next} to continue]"
        );
    } else {
        let _ = writeln!(footer, "[end of file: {total} lines]");
    }
    if window.line_capped {
        let _ = writeln!(
            footer,
            "[some lines were truncated at the {line_bytes}-byte line cap]"
        );
    }
    (body, footer)
}

/// The image formats recognised by magic bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ImageFormat {
    Png,
    Jpeg,
    Gif,
    WebP,
}

impl ImageFormat {
    /// Returns the name shown to the model.
    const fn name(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
            Self::Gif => "GIF",
            Self::WebP => "WebP",
        }
    }

    /// Returns the media type.
    const fn media_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::WebP => "image/webp",
        }
    }
}

/// Returns the image format a file starts with, when it is an image.
fn image_format(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some(ImageFormat::Png);
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(ImageFormat::Jpeg);
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(ImageFormat::Gif);
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some(ImageFormat::WebP);
    }
    None
}

/// Describes an image instead of returning its bytes.
fn image_description(display: &str, format: ImageFormat, size: u64) -> String {
    format!(
        "{display} is a {} image ({}), {size} bytes. Image bytes are not returned as text.",
        format.name(),
        format.media_type()
    )
}

/// What the leading bytes of a file say about its content.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Content {
    /// Valid UTF-8 text.
    Text,
    /// Text that is not valid UTF-8, shown with replacement characters.
    Lossy,
    /// Not text: refused.
    Binary,
}

/// Classifies the probe bytes.
///
/// A NUL byte is decisive. Otherwise invalid UTF-8 makes the text lossy, except
/// an incomplete sequence cut by the end of a full probe on a longer file.
fn classify(bytes: &[u8]) -> Content {
    if bytes.contains(&0) {
        return Content::Binary;
    }
    match std::str::from_utf8(bytes) {
        Ok(_) => Content::Text,
        Err(err) if err.error_len().is_none() && bytes.len() >= PROBE_BYTES => Content::Text,
        Err(_) => Content::Lossy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::Cancellation;
    use crate::workspace::fixture::{Repo, binary};

    /// Cancels at a known byte boundary without relying on thread scheduling.
    struct CancellingReader<R> {
        inner: R,
        cancellation: Cancellation,
        cancel_after: usize,
        consumed: usize,
        fills: usize,
    }

    impl<R: std::io::Read> std::io::Read for CancellingReader<R> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.inner.read(buffer)
        }
    }

    impl<R: BufRead> BufRead for CancellingReader<R> {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            self.fills = self.fills.checked_add(1).expect("fill count");
            self.inner.fill_buf()
        }

        fn consume(&mut self, amount: usize) {
            self.inner.consume(amount);
            self.consumed = self.consumed.checked_add(amount).expect("consumed bytes");
            if self.consumed >= self.cancel_after {
                self.cancellation.cancel();
            }
        }
    }

    #[test]
    fn a_cancelled_call_stops_before_accessing_the_file() {
        let repo = Repo::new();
        let path = repo.path().join("large.txt");
        std::fs::File::create(&path)
            .expect("create")
            .set_len(128 * 1024 * 1024)
            .expect("large sparse file");
        let context = repo.context();
        context.cancellation().cancel();

        for path in ["large.txt", "absent.txt"] {
            let error = ReadFile::new()
                .call(&serde_json::json!({ "path": path }), &context)
                .expect_err("cancelled before file access");
            assert_eq!(error.code(), ErrorCode::Cancelled);
        }
    }

    #[test]
    fn a_cancelled_collection_does_no_io() {
        let repo = Repo::new();
        let context = repo.context();
        context.cancellation().cancel();
        let mut reader = CancellingReader {
            inner: std::io::Cursor::new(b"text\n"),
            cancellation: context.cancellation(),
            cancel_after: usize::MAX,
            consumed: 0,
            fills: 0,
        };

        let error =
            collect(&mut reader, 1, 1, 32, 4096, &context).expect_err("cancelled before reading");
        assert_eq!(error.code(), ErrorCode::Cancelled);
        assert_eq!(reader.consumed, 0);
        assert_eq!(reader.fills, 0);
    }

    #[test]
    fn cancellation_bounds_work_while_skipping_collecting_and_counting_lines() {
        let repo = Repo::new();
        let body = "short text line\n".repeat(64 * 1024);
        for (start, count, budget) in [
            (100_000, 1, 4096),       // Skip to a requested page.
            (1, 100_000, usize::MAX), // Collect a large window.
            (1, 1, 4096),             // Count the tail after the line cap.
            (1, 100_000, 1),          // Count the tail after the byte cap.
        ] {
            let context = repo.context();
            let mut reader = CancellingReader {
                inner: BufReader::with_capacity(PROBE_BYTES, body.as_bytes()),
                cancellation: context.cancellation(),
                cancel_after: 3 * PROBE_BYTES,
                consumed: 0,
                fills: 0,
            };

            let error = collect(&mut reader, start, count, 32, budget, &context)
                .expect_err("cancelled during a paged read");
            assert_eq!(error.code(), ErrorCode::Cancelled);
            assert!(reader.consumed >= reader.cancel_after);
            assert!(reader.consumed <= reader.cancel_after + PROBE_BYTES);
            assert!(reader.consumed < body.len());
        }
    }

    #[test]
    fn cancellation_bounds_work_within_one_oversized_line() {
        let repo = Repo::new();
        let context = repo.context();
        let body = "x".repeat(8 * 1024 * 1024);
        let mut reader = CancellingReader {
            inner: BufReader::with_capacity(PROBE_BYTES, body.as_bytes()),
            cancellation: context.cancellation(),
            cancel_after: 3 * PROBE_BYTES,
            consumed: 0,
            fills: 0,
        };

        let error = collect(&mut reader, 1, 1, 32, 4096, &context)
            .expect_err("cancelled while discarding a long line");
        assert_eq!(error.code(), ErrorCode::Cancelled);
        assert_eq!(reader.consumed, reader.cancel_after);
        assert_eq!(reader.fills, 3);
    }

    #[test]
    fn cancellation_on_the_final_chunk_does_not_return_a_partial_window() {
        let repo = Repo::new();
        for body in [b"last line\n".as_slice(), b"last line".as_slice()] {
            let context = repo.context();
            let mut reader = CancellingReader {
                inner: BufReader::with_capacity(PROBE_BYTES, body),
                cancellation: context.cancellation(),
                cancel_after: body.len(),
                consumed: 0,
                fills: 0,
            };

            let error = collect(&mut reader, 1, 1, 32, 4096, &context)
                .expect_err("cancelled at end of file");
            assert_eq!(error.code(), ErrorCode::Cancelled);
            assert_eq!(reader.consumed, body.len());
            assert_eq!(reader.fills, 1);
        }
    }

    /// Builds `count` numbered lines under a prefix.
    fn numbered(prefix: &str, first: usize, last: usize) -> String {
        let mut body = String::new();
        for n in first..=last {
            let _ = writeln!(body, "{prefix}{n}");
        }
        body
    }

    /// Builds a minimal PNG header, which is all the detector reads.
    fn png_bytes() -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x0D]);
        bytes.extend_from_slice(b"IHDR");
        bytes
    }

    #[cfg(unix)]
    #[test]
    fn a_file_reached_through_a_link_that_leads_outside_is_refused() {
        let repo = Repo::new();
        let elsewhere = tempfile::tempdir().expect("temp dir");
        std::fs::write(elsewhere.path().join("secret.txt"), "confidential\n").expect("write");
        std::os::unix::fs::symlink(elsewhere.path(), repo.path().join("link")).expect("symlink");

        let err = ReadFile::default()
            .call(
                &serde_json::json!({ "path": "link/secret.txt" }),
                &repo.context(),
            )
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::PathOutsideWorkspace);
        assert!(!err.message().contains("confidential"), "{}", err.message());
    }

    #[test]
    fn lines_are_numbered_from_one() {
        let repo = Repo::new();
        let output = ReadFile::default()
            .call(&serde_json::json!({ "path": "README.md" }), &repo.context())
            .expect("call");
        assert!(!output.is_error);
        assert!(
            output.text.contains("     1\tRune fixture"),
            "{}",
            output.text
        );
        assert!(
            output.text.contains("     2\tneedle one"),
            "{}",
            output.text
        );
        assert!(
            output.text.contains("[end of file: 2 lines]"),
            "{}",
            output.text
        );
    }

    #[test]
    fn a_misspelled_optional_argument_is_rejected_before_reading() {
        let repo = Repo::new();
        let arguments = serde_json::json!({ "path": "README.md", "start_lien": 2 });
        let err = ReadFile::new()
            .call(&arguments, &repo.context())
            .expect_err("unknown argument refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.detail().field.as_deref(), Some("start_lien"));
        assert!(err.message().contains("start_lien"));

        let mut registry = crate::registry::Registry::new();
        registry
            .insert(Box::new(ReadFile::new()))
            .expect("registered");
        let output = registry
            .call("read_file", &arguments, &repo.context())
            .expect("model-visible schema error");
        assert!(output.is_error);
        assert!(output.text.contains("start_lien"));
        assert!(!output.text.contains("Rune fixture"));
    }

    #[test]
    fn a_window_reports_the_true_total_and_how_to_continue() {
        let repo = Repo::new();
        let body = numbered("line ", 1, 40);
        repo.write("long/window.txt", &body);

        let output = ReadFile::default()
            .call(
                &serde_json::json!({ "path": "long/window.txt", "start_line": 3, "line_count": 4 }),
                &repo.context(),
            )
            .expect("call");
        assert!(
            output.text.contains("long/window.txt: 40 lines"),
            "{}",
            output.text
        );
        assert!(output.text.contains("     3\tline 3"), "{}", output.text);
        assert!(output.text.contains("     6\tline 6"), "{}", output.text);
        assert!(!output.text.contains("line 7"), "{}", output.text);
        assert!(
            output
                .text
                .contains("showing lines 3 to 6 of 40; pass start_line=7"),
            "{}",
            output.text
        );
    }

    #[test]
    fn a_start_line_past_the_end_returns_the_last_lines() {
        let repo = Repo::new();
        // Line 3 is exactly one past the end, and counts as past it.
        for start in [99, 3] {
            let output = ReadFile::default()
                .call(
                    &serde_json::json!({ "path": "README.md", "start_line": start }),
                    &repo.context(),
                )
                .expect("call");
            assert!(!output.is_error);
            assert!(
                output.text.contains("README.md: 2 lines"),
                "{}",
                output.text
            );
            assert!(
                output.text.contains("     1\tRune fixture"),
                "{}",
                output.text
            );
            assert!(
                output.text.contains("     2\tneedle one"),
                "{}",
                output.text
            );
            assert!(
                output
                    .text
                    .contains("[the file has 2 lines; showing the last 2]"),
                "{}",
                output.text
            );
            assert!(
                !output.text.contains("no lines returned"),
                "{}",
                output.text
            );
        }

        repo.write("long/forty.txt", &numbered("line ", 1, 40));
        let output = ReadFile::default()
            .call(
                &serde_json::json!({ "path": "long/forty.txt", "start_line": 41 }),
                &repo.context(),
            )
            .expect("call");
        assert!(!output.text.contains("line 20\n"), "{}", output.text);
        assert!(output.text.contains("    21\tline 21"), "{}", output.text);
        assert!(output.text.contains("    40\tline 40"), "{}", output.text);
        assert!(
            output
                .text
                .contains("[the file has 40 lines; showing the last 20]"),
            "{}",
            output.text
        );

        let output = ReadFile::default()
            .call(
                &serde_json::json!({ "path": "empty.txt", "start_line": 99 }),
                &repo.context(),
            )
            .expect("call");
        assert!(
            output.text.contains("[the file is empty]"),
            "{}",
            output.text
        );
    }

    #[test]
    fn latin1_text_is_shown_with_replacement_characters() {
        let repo = Repo::new();
        repo.write_bytes("data/latin1.txt", b"caf\xe9\nplain\n");
        let output = ReadFile::default()
            .call(
                &serde_json::json!({ "path": "data/latin1.txt" }),
                &repo.context(),
            )
            .expect("call");
        assert!(!output.is_error, "{}", output.text);
        assert!(
            output.text.starts_with(
                "data/latin1.txt: 2 lines, 11 bytes [not valid UTF-8: shown with replacement \
                 characters; edit it with a command, not edit_file]\n"
            ),
            "{}",
            output.text
        );
        assert!(
            output.text.contains("     1\tcaf\u{FFFD}\n"),
            "{}",
            output.text
        );
        assert!(output.text.contains("     2\tplain"), "{}", output.text);
    }

    #[test]
    fn an_invalid_byte_past_the_probe_marks_the_read_lossy() {
        let repo = Repo::new();
        let mut bytes = "text line\n".repeat(2000).into_bytes();
        bytes.extend_from_slice(b"caf\xe9\n");
        assert!(bytes.len() > PROBE_BYTES);
        repo.write_bytes("data/late.txt", &bytes);
        let output = ReadFile::default()
            .call(
                &serde_json::json!({ "path": "data/late.txt", "line_count": 2 }),
                &repo.context(),
            )
            .expect("call");
        assert!(!output.is_error, "{}", output.text);
        assert!(
            output
                .text
                .contains("2001 lines, 20005 bytes [not valid UTF-8"),
            "{}",
            output.text
        );
        assert!(output.text.contains("     1\ttext line"), "{}", output.text);

        let clean = "text line\n".repeat(2000);
        repo.write("data/clean.txt", &clean);
        let output = ReadFile::default()
            .call(
                &serde_json::json!({ "path": "data/clean.txt", "line_count": 2 }),
                &repo.context(),
            )
            .expect("call");
        assert!(!output.text.contains("not valid UTF-8"), "{}", output.text);
    }

    #[test]
    fn a_wrong_case_name_suggests_the_real_file() {
        let repo = Repo::new();
        assert_eq!(
            suggestions(&repo.context(), &repo.path().join("src/Main.rs")),
            vec!["src/main.rs".to_owned()]
        );
        let result = ReadFile::default().call(
            &serde_json::json!({ "path": "src/Main.rs" }),
            &repo.context(),
        );
        if repo.path().join("src/Main.rs").exists() {
            // A case-insensitive file system opens the real file directly.
            let output = result.expect("read through the other case");
            assert!(output.text.contains("fn main()"), "{}", output.text);
            return;
        }
        let err = result.expect_err("missing");
        assert_eq!(err.code(), ErrorCode::NotFound);
        assert_eq!(err.message(), "`src/Main.rs` does not exist");
        assert_eq!(err.hint(), Some("did you mean: `src/main.rs`?"));
    }

    #[test]
    fn a_foreign_absolute_path_suggests_the_workspace_file() {
        let repo = Repo::new();
        let elsewhere = tempfile::tempdir().expect("temp dir");
        let foreign = Utf8Path::from_path(elsewhere.path())
            .expect("a UTF-8 temporary path")
            .join("app/src/main.rs");
        let arguments = serde_json::json!({ "path": foreign.as_str() });

        let err = ReadFile::default()
            .call(&arguments, &repo.context())
            .expect_err("outside the workspace");
        assert_eq!(err.code(), ErrorCode::PathOutsideWorkspace);
        assert_eq!(err.hint(), Some("did you mean: `src/main.rs`?"));

        let err = ReadFile::default()
            .call(&arguments, &repo.context().with_external_access(true))
            .expect_err("missing");
        assert_eq!(err.code(), ErrorCode::NotFound);
        assert_eq!(err.hint(), Some("did you mean: `src/main.rs`?"));
    }

    #[test]
    fn at_most_three_suggestions_are_given() {
        let repo = Repo::new();
        for path in ["x.txt", "d/x.txt", "c/d/x.txt", "b/c/d/x.txt"] {
            repo.write(path, "x\n");
        }
        let elsewhere = tempfile::tempdir().expect("temp dir");
        let foreign = Utf8Path::from_path(elsewhere.path())
            .expect("a UTF-8 temporary path")
            .join("a/b/c/d/x.txt");
        let err = ReadFile::default()
            .call(
                &serde_json::json!({ "path": foreign.as_str() }),
                &repo.context().with_external_access(true),
            )
            .expect_err("missing");
        assert_eq!(err.code(), ErrorCode::NotFound);
        let expected = ["b/c/d/x.txt", "c/d/x.txt", "d/x.txt"]
            .map(|path| {
                format!(
                    "`{}`",
                    display_path(&repo.context(), &repo.path().join(path))
                )
            })
            .join(", ");
        assert_eq!(
            err.hint(),
            Some(format!("did you mean: {expected}?").as_str())
        );
    }

    #[test]
    fn the_late_binary_message_has_no_run_of_spaces() {
        let repo = Repo::new();
        let mut bytes = "text line\n".repeat(2000).into_bytes();
        bytes.extend_from_slice(&[0x00, 0x01]);
        repo.write_bytes("data/late.bin", &bytes);
        let output = ReadFile::default()
            .call(
                &serde_json::json!({ "path": "data/late.bin" }),
                &repo.context(),
            )
            .expect("call");
        assert!(output.is_error);
        assert_eq!(
            output.text,
            "`data/late.bin` contains binary data past its first 8192 bytes, so its content was \
             not returned as text."
        );
        assert!(!output.text.contains("  "), "{}", output.text);
    }

    #[test]
    fn a_large_file_returns_a_bounded_window_and_the_true_total() {
        let repo = Repo::new();
        // Roughly ten MiB of an easy-to-count shape: 10485 lines of 1000 bytes.
        let mut body = String::with_capacity(10 * 1024 * 1024);
        for n in 1..=10_485 {
            let _ = writeln!(body, "{n:0>7}{}", "p".repeat(999));
        }
        repo.write("huge/ten.txt", &body);
        assert!(body.len() > 10 * 1024 * 1024, "{}", body.len());

        let output = ReadFile::default()
            .call(
                &serde_json::json!({ "path": "huge/ten.txt" }),
                &repo.context(),
            )
            .expect("call");
        let limits = FileLimits::default();
        assert!(
            output.text.contains("huge/ten.txt: 10485 lines"),
            "{}",
            &output.text[..200.min(output.text.len())]
        );
        // The window is bounded by the byte cap well before the line cap, and
        // the result is a fraction of the file rather than all of it.
        // Content lines carry a number column and are about a thousand bytes;
        // the footer is short, so width separates them without parsing.
        let shown = output.text.lines().filter(|line| line.len() > 512).count();
        assert!(shown > 0 && shown < limits.read_lines, "{shown}");
        assert!(output.text.len() < 128 * 1024, "{}", output.text.len());
        let tail = &output.text[output.text.len().saturating_sub(300)..];
        assert!(
            tail.contains(&format!(
                "showing lines 1 to {shown} of 10485; pass start_line={}",
                shown + 1
            )),
            "{tail}"
        );
    }

    #[test]
    fn an_empty_file_is_not_an_error() {
        let repo = Repo::new();
        let output = ReadFile::default()
            .call(&serde_json::json!({ "path": "empty.txt" }), &repo.context())
            .expect("call");
        assert!(!output.is_error);
        assert!(
            output.text.contains("empty.txt: 0 lines, 0 bytes"),
            "{}",
            output.text
        );
        assert!(
            output.text.contains("[the file is empty]"),
            "{}",
            output.text
        );
    }

    #[test]
    fn the_line_count_is_capped_and_the_remainder_is_reported() {
        let repo = Repo::new();
        let body = numbered("line ", 1, 50);
        repo.write("long/many.txt", &body);
        let output = ReadFile::default()
            .call(
                &serde_json::json!({ "path": "long/many.txt", "line_count": 5000 }),
                &repo.context(),
            )
            .expect("call");
        assert!(
            output.text.contains("long/many.txt: 50 lines"),
            "{}",
            output.text
        );
        assert!(
            output.text.contains("[end of file: 50 lines]"),
            "{}",
            output.text
        );
    }

    #[test]
    fn a_long_line_is_truncated_with_a_marker() {
        let repo = Repo::new();
        let output = ReadFile::default()
            .call(
                &serde_json::json!({ "path": "long/line.txt" }),
                &repo.context(),
            )
            .expect("call");
        assert!(output.text.contains("-byte line cap"), "{}", output.text);
        assert!(
            output
                .text
                .contains(&format!("the line is {} bytes", 7 + 5000)),
            "{}",
            output.text
        );
        assert!(output.text.len() < FileLimits::default().line_bytes + 1024);
    }

    #[test]
    fn a_crlf_line_loses_its_carriage_return() {
        let repo = Repo::new();
        let output = ReadFile::default()
            .call(
                &serde_json::json!({ "path": "crlf/win.txt" }),
                &repo.context(),
            )
            .expect("call");
        assert!(output.text.contains("     2\tbeta\n"), "{}", output.text);
        assert!(
            output.text.contains("crlf/win.txt: 3 lines"),
            "{}",
            output.text
        );
        assert!(!output.text.contains("beta\r"), "{:?}", output.text);
    }

    #[test]
    fn a_binary_file_is_refused_rather_than_mojibake() {
        let repo = Repo::new();
        let output = ReadFile::default()
            .call(
                &serde_json::json!({ "path": "data/blob.bin" }),
                &repo.context(),
            )
            .expect("call");
        assert!(output.is_error);
        assert!(output.text.contains("is binary"), "{}", output.text);
        assert!(!output.text.contains("needle"), "{}", output.text);
    }

    #[test]
    fn one_enormous_line_is_cut_and_named_by_its_true_length() {
        let repo = Repo::new();
        let mut body = "y".repeat(3 * 1024 * 1024);
        body.push_str("\nsecond\n");
        repo.write("long/huge.txt", &body);
        let output = ReadFile::default()
            .call(
                &serde_json::json!({ "path": "long/huge.txt" }),
                &repo.context(),
            )
            .expect("call");
        assert!(
            output
                .text
                .contains(&format!("the line is {} bytes", 3 * 1024 * 1024)),
            "{}",
            &output.text[output.text.len().saturating_sub(400)..]
        );
        assert!(output.text.contains("     2\tsecond"));
    }

    #[test]
    fn a_nul_past_the_kept_part_of_a_long_line_is_still_refused() {
        let repo = Repo::new();
        let mut bytes = b"text\n".to_vec();
        bytes.extend(std::iter::repeat_n(b'z', 64 * 1024));
        bytes.push(0);
        bytes.extend_from_slice(b"\n");
        repo.write_bytes("data/long.bin", &bytes);
        let output = ReadFile::default()
            .call(
                &serde_json::json!({ "path": "data/long.bin" }),
                &repo.context(),
            )
            .expect("call");
        assert!(output.is_error, "{}", output.text);
    }

    #[test]
    fn binary_content_past_the_probe_is_still_refused() {
        let repo = Repo::new();
        let mut bytes = "text line\n".repeat(2000).into_bytes();
        bytes.extend_from_slice(&[0x00, 0x01, 0x02]);
        bytes.extend_from_slice(b"needle\n");
        repo.write_bytes("data/late.bin", &bytes);

        let output = ReadFile::default()
            .call(
                &serde_json::json!({ "path": "data/late.bin" }),
                &repo.context(),
            )
            .expect("call");
        assert!(output.is_error);
        assert!(
            output.text.contains("contains binary data past"),
            "{}",
            output.text
        );
        assert!(!output.text.contains("text line"), "{}", output.text);
    }

    #[test]
    fn an_image_is_described_by_its_magic_bytes() {
        let repo = Repo::new();
        repo.write_bytes("data/pixel.png", &png_bytes());
        let output = ReadFile::default()
            .call(
                &serde_json::json!({ "path": "data/pixel.png" }),
                &repo.context(),
            )
            .expect("call");
        assert!(!output.is_error);
        assert!(
            output.text.contains("is a PNG image (image/png)"),
            "{}",
            output.text
        );
        assert!(output.text.contains("data/pixel.png"), "{}", output.text);
    }

    #[test]
    fn every_supported_image_format_is_detected() {
        assert_eq!(image_format(&png_bytes()), Some(ImageFormat::Png));
        assert_eq!(
            image_format(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some(ImageFormat::Jpeg)
        );
        assert_eq!(image_format(b"GIF89a____"), Some(ImageFormat::Gif));
        assert_eq!(image_format(b"GIF87a____"), Some(ImageFormat::Gif));
        assert_eq!(
            image_format(b"RIFF\x00\x00\x00\x00WEBPVP8 "),
            Some(ImageFormat::WebP)
        );
        assert_eq!(image_format(b"RIFF\x00\x00\x00\x00WAVEfmt "), None);
        assert_eq!(image_format(b"plain text"), None);
    }

    #[test]
    fn a_utf8_file_without_a_nul_is_read_as_text() {
        let repo = Repo::new();
        repo.write_bytes(
            "data/utf8.txt",
            "cafe\u{301} and \u{4e2d}\u{6587}\n".as_bytes(),
        );
        let output = ReadFile::default()
            .call(
                &serde_json::json!({ "path": "data/utf8.txt" }),
                &repo.context(),
            )
            .expect("call");
        assert!(!output.is_error, "{}", output.text);
        assert!(output.text.contains("cafe"), "{}", output.text);
    }

    #[test]
    fn a_nul_byte_still_refuses_the_file() {
        assert_eq!(classify(b"plain text with a newline\n"), Content::Text);
        assert_eq!(
            classify("tabs\tand accents: cafe\u{301}\n".as_bytes()),
            Content::Text
        );
        assert_eq!(classify(&binary()), Content::Binary);
        assert_eq!(classify(&[0x00]), Content::Binary);
        assert_eq!(classify(&[0xC3, 0x28]), Content::Lossy);
        // A sequence cut by the end of a full probe is not evidence of anything.
        let mut cut = vec![b'a'; PROBE_BYTES - 1];
        cut.push(0xC3);
        assert_eq!(classify(&cut), Content::Text);
        assert_eq!(classify(&cut[PROBE_BYTES - 4..]), Content::Lossy);
    }

    #[test]
    fn a_relative_and_an_absolute_path_name_the_same_file() {
        let repo = Repo::new();
        let context = repo.context();
        let relative = ReadFile::default()
            .call(&serde_json::json!({ "path": "src/main.rs" }), &context)
            .expect("call");
        let absolute = ReadFile::default()
            .call(
                &serde_json::json!({ "path": repo.path().join("src/main.rs").as_str() }),
                &context,
            )
            .expect("call");
        assert_eq!(relative.text, absolute.text);
        assert!(relative.text.starts_with("src/main.rs: 2 lines"));
    }

    #[test]
    fn an_escaping_path_is_refused() {
        let repo = Repo::new();
        let err = ReadFile::default()
            .call(
                &serde_json::json!({ "path": "../outside.txt" }),
                &repo.context(),
            )
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::PathOutsideWorkspace);
    }

    #[test]
    fn a_missing_file_names_the_path() {
        let repo = Repo::new();
        let err = ReadFile::default()
            .call(
                &serde_json::json!({ "path": "absent.txt" }),
                &repo.context(),
            )
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::NotFound);
        assert!(err.message().contains("absent.txt"), "{}", err.message());
        assert_eq!(err.hint(), None);
    }

    #[test]
    fn a_wrongly_typed_line_count_is_refused() {
        let repo = Repo::new();
        let err = ReadFile::default()
            .call(
                &serde_json::json!({ "path": "README.md", "line_count": "many" }),
                &repo.context(),
            )
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.field(), Some("line_count"));
    }

    #[test]
    fn a_directory_is_refused_as_a_non_file() {
        let repo = Repo::new();
        let err = ReadFile::default()
            .call(&serde_json::json!({ "path": "src" }), &repo.context())
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert!(
            err.message().contains("is not a regular file"),
            "{}",
            err.message()
        );
    }

    #[test]
    fn a_zero_start_line_is_refused() {
        let repo = Repo::new();
        let err = ReadFile::default()
            .call(
                &serde_json::json!({ "path": "README.md", "start_line": 0 }),
                &repo.context(),
            )
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
    }

    #[test]
    fn the_output_stays_within_the_context_byte_cap() {
        let repo = Repo::new();
        let body = numbered("line ", 1, 400);
        repo.write("long/wide.txt", &body);
        let context = repo.context().with_output_cap(4096);
        let capped = ReadFile::default()
            .call(&serde_json::json!({ "path": "long/wide.txt" }), &context)
            .expect("call");
        assert!(capped.text.len() <= 4096, "{}", capped.text.len());
        assert!(
            capped.text.contains("output truncated at the byte cap"),
            "{}",
            capped.text
        );
        assert!(capped.text.contains("of 400"), "{}", capped.text);
    }

    #[test]
    fn the_declared_schema_names_only_the_accepted_arguments() {
        let schema = ReadFile::default().input_schema();
        assert_eq!(schema["required"][0], "path");
        assert_eq!(schema["additionalProperties"], false);
        assert!(schema["properties"].get("start_line").is_some());
        assert!(schema["properties"].get("line_count").is_some());
    }
}
