//! Request tracing.
//!
//! A provider failure can only be diagnosed, or turned into a test, from the
//! bytes that crossed the wire. When tracing is on, every provider request and
//! its raw response are written to a directory as a numbered pair of files,
//! with credentials removed before anything reaches the disk.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use camino::{Utf8Path, Utf8PathBuf};
use rune_core::error::{ErrorCode, Result, RuneError};

use crate::redact;
use crate::transport::MAX_RESPONSE_BYTES;

/// What replaces a credential in a trace.
const REDACTED: &str = "[REDACTED]";

/// A directory that receives one request and response pair per provider call.
#[derive(Debug)]
pub struct TraceSink {
    dir: Utf8PathBuf,
    next: AtomicU32,
    secrets: Mutex<Vec<String>>,
    stopped: AtomicBool,
}

impl TraceSink {
    /// Opens a trace directory, creating it with mode 0700.
    ///
    /// Numbering continues after the highest pair already in the directory, so
    /// a second run adds to a trace rather than overwriting it.
    pub fn open(dir: &Utf8Path, secrets: Vec<String>) -> Result<Arc<Self>> {
        rune_core::paths::create_dir_private(dir).map_err(|err| {
            RuneError::new(
                ErrorCode::IoFailure,
                format!("the trace directory cannot be used: {}", err.message()),
            )
            .with_hint("choose a writable directory for --trace")
        })?;
        let highest = std::fs::read_dir(dir)
            .map_err(|err| {
                RuneError::new(
                    ErrorCode::IoFailure,
                    format!("the trace directory `{dir}` cannot be read: {err}"),
                )
                .with_hint("choose a writable directory for --trace")
            })?
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter_map(|name| pair_number(&name))
            .max()
            .unwrap_or(0);
        Ok(Arc::new(Self {
            dir: dir.to_owned(),
            next: AtomicU32::new(highest.saturating_add(1)),
            secrets: Mutex::new(clean_secrets(secrets)),
            stopped: AtomicBool::new(false),
        }))
    }

    /// The directory pairs are written to.
    #[must_use]
    pub fn dir(&self) -> &Utf8Path {
        &self.dir
    }

    /// Adds credentials to replace, for an endpoint opened after this sink.
    pub fn add_secrets(&self, secrets: Vec<String>) {
        let mut held = self.secrets.lock().unwrap_or_else(PoisonError::into_inner);
        for secret in clean_secrets(secrets) {
            if !held.contains(&secret) {
                held.push(secret);
            }
        }
    }

    /// Records a request about to be sent and returns the entry its response
    /// is written through.
    pub fn begin(
        self: &Arc<Self>,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> TraceEntry {
        let number = self.next.fetch_add(1, Ordering::Relaxed);
        let mut shown = serde_json::Map::new();
        for (name, value) in headers {
            let value = if redact::is_secret_header(name) {
                REDACTED.to_owned()
            } else {
                self.scrub(value)
            };
            shown.insert(name.clone(), serde_json::Value::String(value));
        }
        let mut body = serde_json::from_slice(body).unwrap_or_else(|_| {
            serde_json::Value::String(String::from_utf8_lossy(body).into_owned())
        });
        self.scrub_strings(&mut body);
        let record = serde_json::json!({
            "url": self.scrub(url),
            "headers": shown,
            "body": body,
        });
        match serde_json::to_string_pretty(&record) {
            Ok(text) => self.write(&format!("{number:04}-request.json"), &format!("{text}\n")),
            Err(err) => self.stop(&err.to_string()),
        }
        TraceEntry {
            sink: Arc::clone(self),
            number,
            status: None,
            buffer: Vec::new(),
            cut: false,
            written: false,
        }
    }

    /// Replaces every known credential, then anything credential shaped.
    fn scrub(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for secret in self
            .secrets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            if out.contains(secret.as_str()) {
                out = out.replace(secret.as_str(), REDACTED);
            }
        }
        redact::redact(&out)
    }

    /// Scrubs each string in a request body, so the record stays valid JSON.
    fn scrub_strings(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(text) => *text = self.scrub(text),
            serde_json::Value::Array(items) => {
                for item in items {
                    self.scrub_strings(item);
                }
            }
            serde_json::Value::Object(fields) => {
                for (_, field) in fields.iter_mut() {
                    self.scrub_strings(field);
                }
            }
            _ => {}
        }
    }

    /// Writes one file, stopping the trace on the first failure.
    fn write(&self, name: &str, contents: &str) {
        if self.stopped.load(Ordering::Relaxed) {
            return;
        }
        if let Err(err) = rune_core::paths::write_private_atomic(&self.dir.join(name), contents) {
            self.stop(err.message());
        }
    }

    /// Reports the first write failure once; later ones are ignored.
    fn stop(&self, reason: &str) {
        if !self.stopped.swap(true, Ordering::Relaxed) {
            eprintln!("rune: tracing stopped: {reason}");
        }
    }
}

/// The response half of one traced request.
///
/// Written when finished, or when dropped, so a stream cut by an error still
/// leaves the bytes that arrived.
#[derive(Debug)]
pub struct TraceEntry {
    sink: Arc<TraceSink>,
    number: u32,
    status: Option<u16>,
    buffer: Vec<u8>,
    cut: bool,
    written: bool,
}

impl TraceEntry {
    /// Records the response status.
    pub fn status(&mut self, status: u16) {
        self.status = Some(status);
    }

    /// Appends response bytes as received, up to the response cap.
    pub fn chunk(&mut self, bytes: &[u8]) {
        let cap = usize::try_from(MAX_RESPONSE_BYTES).unwrap_or(usize::MAX);
        let room = cap.saturating_sub(self.buffer.len());
        if bytes.len() > room {
            self.cut = true;
        }
        self.buffer
            .extend_from_slice(bytes.get(..room.min(bytes.len())).unwrap_or_default());
    }

    /// Writes the response file.
    pub fn finish(mut self) {
        self.write();
    }

    fn write(&mut self) {
        if self.written {
            return;
        }
        self.written = true;
        let status = self
            .status
            .map_or_else(|| String::from("none"), |status| status.to_string());
        let mut text = format!(
            "status: {status}\n\n{}",
            String::from_utf8_lossy(&self.buffer)
        );
        if self.cut {
            if !text.ends_with('\n') {
                text.push('\n');
            }
            let _ = writeln!(text, "[trace cut at {MAX_RESPONSE_BYTES} bytes]");
        }
        let text = self.sink.scrub(&text);
        self.sink
            .write(&format!("{:04}-response.txt", self.number), &text);
    }
}

impl Drop for TraceEntry {
    fn drop(&mut self) {
        self.write();
    }
}

/// The number of a trace file named `NNNN-...`, when it has one.
fn pair_number(name: &str) -> Option<u32> {
    let (digits, _) = name.split_once('-')?;
    if digits.len() < 4 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Drops empty credentials, which would otherwise match everywhere.
fn clean_secrets(secrets: Vec<String>) -> Vec<String> {
    secrets
        .into_iter()
        .filter(|secret| !secret.trim().is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> (tempfile::TempDir, Utf8PathBuf) {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = Utf8PathBuf::from_path_buf(temp.path().join("trace")).expect("utf8");
        (temp, dir)
    }

    fn read(dir: &Utf8Path, name: &str) -> String {
        std::fs::read_to_string(dir.join(name)).expect("trace file")
    }

    #[test]
    fn a_trace_pair_is_written_and_redacted() {
        let (_temp, dir) = temp_dir();
        let sink = TraceSink::open(&dir, vec![String::from("plainsecretvalue")]).expect("open");
        let headers = vec![
            (
                String::from("authorization"),
                String::from("Bearer plainsecretvalue"),
            ),
            (
                String::from("content-type"),
                String::from("application/json"),
            ),
        ];
        let body =
            br#"{"model":"m","messages":[{"role":"user","content":"key plainsecretvalue"}]}"#;
        let mut entry = sink.begin("http://127.0.0.1:9/v1/chat/completions", &headers, body);
        entry.status(401);
        entry.chunk(br#"{"error":"invalid key plainsecretvalue"}"#);
        entry.finish();

        let request = read(&dir, "0001-request.json");
        let response = read(&dir, "0001-response.txt");
        assert!(!request.contains("plainsecretvalue"), "{request}");
        assert!(!response.contains("plainsecretvalue"), "{response}");
        let parsed: serde_json::Value = serde_json::from_str(&request).expect("json");
        assert_eq!(parsed["url"], "http://127.0.0.1:9/v1/chat/completions");
        assert_eq!(parsed["headers"]["authorization"], REDACTED);
        assert_eq!(parsed["headers"]["content-type"], "application/json");
        assert_eq!(parsed["body"]["model"], "m");
        assert_eq!(parsed["body"]["messages"][0]["content"], "key [REDACTED]");
        assert!(response.starts_with("status: 401\n\n"), "{response}");
        assert!(response.contains("invalid key [REDACTED]"), "{response}");
    }

    #[test]
    fn numbering_continues_after_existing_files() {
        let (_temp, dir) = temp_dir();
        rune_core::paths::create_dir_private(&dir).expect("dir");
        std::fs::write(dir.join("0007-request.json"), "{}").expect("old request");
        std::fs::write(dir.join("0007-response.txt"), "status: 200\n\n").expect("old response");
        std::fs::write(dir.join("notes.txt"), "kept").expect("other file");

        let sink = TraceSink::open(&dir, Vec::new()).expect("open");
        let first = sink.begin("http://127.0.0.1:9/v1/models", &[], b"");
        let second = sink.begin("http://127.0.0.1:9/v1/models", &[], b"");
        drop(first);
        drop(second);
        for name in [
            "0008-request.json",
            "0008-response.txt",
            "0009-request.json",
            "0009-response.txt",
        ] {
            assert!(dir.join(name).exists(), "{name} is missing");
        }

        let reopened = TraceSink::open(&dir, Vec::new()).expect("reopen");
        drop(reopened.begin("http://127.0.0.1:9/v1/models", &[], b""));
        assert!(dir.join("0010-request.json").exists());
    }

    #[test]
    fn a_cut_stream_still_writes_its_bytes() {
        let (_temp, dir) = temp_dir();
        let sink = TraceSink::open(&dir, Vec::new()).expect("open");
        {
            let mut entry = sink.begin("http://127.0.0.1:9/v1/chat/completions", &[], b"{}");
            entry.status(200);
            entry.chunk(b"data: {\"choices\":[]}\n\n");
            entry.chunk(b"data: {\"cho");
            // Dropped without finishing, as an interrupted stream is.
        }
        let response = read(&dir, "0001-response.txt");
        assert_eq!(
            response,
            "status: 200\n\ndata: {\"choices\":[]}\n\ndata: {\"cho"
        );
    }

    #[test]
    fn a_response_over_the_cap_is_cut_with_a_marker() {
        let (_temp, dir) = temp_dir();
        let sink = TraceSink::open(&dir, Vec::new()).expect("open");
        let mut entry = sink.begin("http://127.0.0.1:9/v1/chat/completions", &[], b"{}");
        entry.status(200);
        let piece = vec![b'a'; 8 * 1024 * 1024];
        for _ in 0..5 {
            entry.chunk(&piece);
        }
        entry.finish();
        let response = read(&dir, "0001-response.txt");
        assert!(
            response.ends_with("\n[trace cut at 33554432 bytes]\n"),
            "the cut marker is missing"
        );
        let body = response
            .strip_prefix("status: 200\n\n")
            .expect("status line");
        let kept = body.len() - "\n[trace cut at 33554432 bytes]\n".len();
        assert_eq!(kept, 32 * 1024 * 1024);
    }

    #[test]
    fn an_unusable_trace_directory_names_the_fix() {
        let (_temp, dir) = temp_dir();
        std::fs::create_dir_all(dir.parent().expect("parent")).expect("parent");
        std::fs::write(&dir, "a file, not a directory").expect("file");
        let err = TraceSink::open(&dir, Vec::new()).expect_err("refused");
        assert_eq!(err.hint(), Some("choose a writable directory for --trace"));
    }

    #[test]
    fn concurrent_requests_get_distinct_numbers() {
        let (_temp, dir) = temp_dir();
        let sink = TraceSink::open(&dir, Vec::new()).expect("open");
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let sink = Arc::clone(&sink);
                std::thread::spawn(move || {
                    sink.begin("http://127.0.0.1:9/v1/models", &[], b"")
                        .finish();
                })
            })
            .collect();
        for thread in threads {
            thread.join().expect("thread");
        }
        let count = std::fs::read_dir(&dir).expect("dir").count();
        assert_eq!(count, 16);
    }
}
