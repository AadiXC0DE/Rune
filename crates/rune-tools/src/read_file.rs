//! Bounded, line-numbered file reads.

use std::fmt::Write as _;
use std::io::{BufRead, BufReader};

use camino::Utf8Path;
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
        "Read a text file as numbered lines. Returns a bounded window: pass start_line and \
         line_count to page through a long file. An image is described rather than decoded, and \
         binary content is refused instead of returned."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File to read, relative to the workspace root or absolute.",
                },
                "start_line": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "First line to return, 1-based. Defaults to the first line.",
                },
                "line_count": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Lines to return, capped by the read_file_lines limit.",
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

        let resolved = resolve(context, &requested)?;
        let display = display_path(context, &resolved.path);
        context.check_cancelled()?;
        let file = open(&display, resolved.path.as_path())?;
        let size = file.metadata().map_or(0, |metadata| metadata.len());

        let mut reader = BufReader::with_capacity(PROBE_BYTES, file);
        let probe = reader.fill_buf().unwrap_or_default().to_vec();
        context.check_cancelled()?;
        if let Some(format) = image_format(&probe) {
            return Ok(ToolOutput::success(image_description(
                &display, format, size,
            )));
        }
        if is_binary(&probe) {
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
        let window = collect(
            &mut reader,
            start_line,
            count,
            self.limits.line_bytes,
            byte_budget,
            context,
        )?;
        if window.binary {
            return Ok(ToolOutput::failure(format!(
                "`{display}` contains binary data past its first {PROBE_BYTES} bytes, so its                  content was not returned as text."
            )));
        }
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

        if number < start_line || window.lines.len() >= count || window.byte_capped {
            number = number.saturating_add(1);
            continue;
        }

        let text = String::from_utf8_lossy(&buffer);
        let text = text.trim_end_matches(['\n', '\r']);
        if line.length > line_bytes {
            window.line_capped = true;
        }
        let shown = truncate_line(text, line.length, line_bytes);
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
    Ok(window)
}

/// Renders the window body and the footer that states what was left out.
fn render(display: &str, size: u64, window: &Window, line_bytes: usize) -> (String, String) {
    let mut body = format!("{display}: {} lines, {size} bytes\n", window.total);
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

/// Returns true when the bytes are not text.
///
/// A NUL byte is decisive. Otherwise the probe must decode as UTF-8, allowing
/// an incomplete sequence at the end only when the probe is the whole file.
fn is_binary(bytes: &[u8]) -> bool {
    if bytes.contains(&0) {
        return true;
    }
    match std::str::from_utf8(bytes) {
        Ok(_) => false,
        Err(err) => err.error_len().is_some() || bytes.len() < PROBE_BYTES,
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
    fn reading_past_the_end_returns_an_empty_window_with_the_true_count() {
        let repo = Repo::new();
        let output = ReadFile::default()
            .call(
                &serde_json::json!({ "path": "README.md", "start_line": 99 }),
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
            output
                .text
                .contains("no lines returned: the requested window starts at line 99"),
            "{}",
            output.text
        );
        assert!(!output.text.contains("Rune fixture"), "{}", output.text);
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
    fn binary_detection_accepts_text_and_rejects_nul() {
        assert!(!is_binary(b"plain text with a newline\n"));
        assert!(!is_binary("tabs\tand accents: cafe\u{301}\n".as_bytes()));
        assert!(is_binary(&binary()));
        assert!(is_binary(&[0x00]));
        // A short invalid file is decided by the invalid sequence, not its size.
        assert!(is_binary(&[0xC3, 0x28]));
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
