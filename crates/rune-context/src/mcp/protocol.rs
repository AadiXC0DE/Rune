//! JSON-RPC 2.0 framing for MCP.
//!
//! Every message is one line of compact JSON. The line cap is what keeps a
//! server from making the client hold an unbounded buffer, and it is enforced
//! while reading rather than after: a frame that exceeds the cap cannot be
//! recovered, because the newline that ends it has already been skipped, so the
//! reader refuses it and the caller discards the connection.

use std::io::BufRead;

use serde_json::{Value, json};

use rune_core::error::{ErrorCode, Result, RuneError};

/// Largest accepted frame, in bytes.
///
/// Sized for a tool call whose result carries a large document. A frame past
/// this is a server defect or an attack, and either way the client refuses it
/// instead of buffering it.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Revision requested at initialization. Every server answers with the revision
/// it will speak, and the answer is what this client accepts.
pub const DEFAULT_VERSION: &str = "2025-11-25";

/// Revisions this client speaks, newest first.
///
/// A server that selects anything outside this ladder is refused: proceeding on
/// an unknown revision would mean guessing at a wire shape, and a wire shape is
/// exactly what must not be guessed.
pub const SUPPORTED_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// Returns true when a revision is one this client speaks.
#[must_use]
pub fn is_supported(version: &str) -> bool {
    SUPPORTED_VERSIONS.contains(&version)
}

/// Renders a request frame, with its trailing newline.
#[must_use]
pub fn request(id: u64, method: &str, params: Option<&Value>) -> String {
    let mut frame = json!({ "jsonrpc": "2.0", "id": id, "method": method });
    if let Some(params) = params
        && let Some(object) = frame.as_object_mut()
    {
        object.insert("params".to_owned(), params.clone());
    }
    let mut text = frame.to_string();
    text.push('\n');
    text
}

/// Renders a notification frame, with its trailing newline.
#[must_use]
pub fn notification(method: &str, params: Option<&Value>) -> String {
    let mut frame = json!({ "jsonrpc": "2.0", "method": method });
    if let Some(params) = params
        && let Some(object) = frame.as_object_mut()
    {
        object.insert("params".to_owned(), params.clone());
    }
    let mut text = frame.to_string();
    text.push('\n');
    text
}

/// A failure reported by the server inside a response.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RpcError {
    /// JSON-RPC error code.
    pub code: i64,
    /// Message the server supplied.
    pub message: String,
    /// Optional structured detail.
    pub data: Option<Value>,
}

impl RpcError {
    /// Converts the failure into the workspace error type.
    ///
    /// The codes that mean something specific get their own mapping; anything
    /// else is a rejected request, which is what an application-level refusal
    /// amounts to.
    #[must_use]
    pub fn to_rune(&self) -> RuneError {
        let code = match self.code {
            -32601 => ErrorCode::Unsupported,
            -32602 => ErrorCode::InvalidField,
            _ => ErrorCode::RequestRejected,
        };
        let mut error = RuneError::new(code, self.message.clone());
        if let Some(text) = self.data.as_ref().and_then(Value::as_str)
            && !text.is_empty()
        {
            error = error.with_observed(text.to_owned());
        }
        error
    }
}

/// An incoming frame, reduced to the part this client acts on.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Incoming {
    /// A response to a request this client sent.
    Response {
        /// Identifier copied from the request.
        id: u64,
        /// Result, or the failure the server reported.
        outcome: std::result::Result<Value, RpcError>,
    },
    /// A frame that needs no reply from this client: a notification, or a
    /// request for a capability the client did not advertise.
    Ignored,
}

/// Parses one frame.
pub fn parse_incoming(text: &str) -> Result<Incoming> {
    let value: Value = serde_json::from_str(text).map_err(|err| {
        RuneError::new(
            ErrorCode::ProtocolViolation,
            format!("a frame is not JSON: {err}"),
        )
    })?;
    let object = value.as_object().ok_or_else(|| {
        RuneError::new(ErrorCode::ProtocolViolation, "a frame is not a JSON object")
    })?;
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(RuneError::new(
            ErrorCode::ProtocolViolation,
            "a frame does not declare JSON-RPC 2.0",
        ));
    }
    let Some(id) = object.get("id").and_then(Value::as_u64) else {
        return Ok(Incoming::Ignored);
    };
    if let Some(error) = object.get("error") {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the server reported an error without a message")
            .to_owned();
        let data = error.get("data").cloned();
        return Ok(Incoming::Response {
            id,
            outcome: Err(RpcError {
                code,
                message,
                data,
            }),
        });
    }
    let Some(result) = object.get("result") else {
        return Err(RuneError::new(
            ErrorCode::ProtocolViolation,
            format!("the response to `{id}` carries neither a result nor an error"),
        ));
    };
    Ok(Incoming::Response {
        id,
        outcome: Ok(result.clone()),
    })
}

/// Reads one frame, returning `None` at a clean end of input.
///
/// Blank lines are skipped: a server that separates frames with an empty line
/// is still speaking a legal stream, and the empty line carries no message.
/// Input that ends inside a frame is an error rather than a frame, because a
/// truncated message would otherwise be read as a complete one.
pub fn read_frame<R: BufRead>(reader: &mut R, cap: usize) -> Result<Option<String>> {
    loop {
        let mut buffer: Vec<u8> = Vec::new();
        loop {
            let available = reader.fill_buf().map_err(frame_io_error)?;
            if available.is_empty() {
                if buffer.is_empty() {
                    return Ok(None);
                }
                return Err(RuneError::new(
                    ErrorCode::IncompleteStream,
                    "the server closed its output inside a frame",
                ));
            }
            let (chunk, consumed, terminated) =
                match available.iter().position(|byte| *byte == b'\n') {
                    Some(index) => (&available[..index], index.saturating_add(1), true),
                    None => (available, available.len(), false),
                };
            if buffer.len().saturating_add(chunk.len()) > cap {
                let observed = buffer.len().saturating_add(chunk.len());
                reader.consume(consumed);
                return Err(RuneError::too_large("frame", observed, cap).with_hint(
                    "the server sent one message larger than the frame cap; a stream cannot be resynchronised after it",
                ));
            }
            buffer.extend_from_slice(chunk);
            reader.consume(consumed);
            if terminated {
                break;
            }
        }
        if buffer.is_empty() {
            continue;
        }
        return Ok(Some(decode(&buffer)?));
    }
}

/// Decodes one frame as UTF-8 text.
fn decode(bytes: &[u8]) -> Result<String> {
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|err| {
            RuneError::new(
                ErrorCode::ProtocolViolation,
                format!("a frame is not valid UTF-8: {err}"),
            )
        })
}

/// Maps a read failure onto the taxonomy.
fn frame_io_error(err: std::io::Error) -> RuneError {
    if err.kind() == std::io::ErrorKind::InvalidData {
        return RuneError::new(ErrorCode::ProtocolViolation, err.to_string());
    }
    RuneError::from(err)
}

/// Extracts the response to `id` from a body.
///
/// A streamable HTTP reply is either one JSON document or a short event stream
/// whose payloads are the equivalent frames, so both shapes are accepted.
pub fn response_from_body(body: &str, id: u64) -> Result<Value> {
    if let Ok(value) = serde_json::from_str::<Value>(body)
        && let Some(object) = value.as_object()
        && object.contains_key("jsonrpc")
    {
        return match parse_incoming(body)? {
            Incoming::Response { id: got, outcome } if got == id => {
                outcome.map_err(|e| e.to_rune())
            }
            _ => Err(unmatched(id)),
        };
    }
    for line in body.lines() {
        let Some(payload) = data_line(line) else {
            continue;
        };
        let Ok(Incoming::Response { id: got, outcome }) = parse_incoming(payload) else {
            continue;
        };
        if got == id {
            return outcome.map_err(|error| error.to_rune());
        }
    }
    Err(unmatched(id))
}

/// Returns the payload of an event stream data line.
fn data_line(line: &str) -> Option<&str> {
    let line = line.trim_end_matches('\r');
    let payload = line
        .strip_prefix("data:")
        .or_else(|| line.strip_prefix("data :"))?
        .trim();
    (!payload.is_empty()).then_some(payload)
}

/// The error used when a reply carries no response to the request sent.
fn unmatched(id: u64) -> RuneError {
    RuneError::new(
        ErrorCode::ProtocolViolation,
        format!("the reply carries no response to request {id}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune_core::error::ErrorCode;
    use std::io::Cursor;

    fn frames(text: &str, cap: usize) -> Vec<Result<Option<String>>> {
        let mut reader = Cursor::new(text.as_bytes().to_vec());
        let mut out = Vec::new();
        loop {
            let next = read_frame(&mut reader, cap);
            let done = matches!(next, Ok(None));
            out.push(next);
            if done || out.last().is_some_and(std::result::Result::is_err) {
                break;
            }
        }
        out
    }

    #[test]
    fn a_request_frame_carries_the_jsonrpc_version_and_id() {
        let frame = request(
            7,
            "initialize",
            Some(&json!({ "protocolVersion": DEFAULT_VERSION })),
        );
        assert!(frame.ends_with('\n'));
        let value: Value = serde_json::from_str(&frame).expect("parsed");
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], 7);
        assert_eq!(value["method"], "initialize");
        assert_eq!(value["params"]["protocolVersion"], DEFAULT_VERSION);
    }

    #[test]
    fn a_request_without_params_omits_them() {
        let value: Value = serde_json::from_str(&request(1, "tools/list", None)).expect("parsed");
        assert!(value.get("params").is_none());
    }

    #[test]
    fn a_notification_frame_carries_no_id() {
        let value: Value =
            serde_json::from_str(&notification("notifications/initialized", None)).expect("parsed");
        assert!(value.get("id").is_none());
        assert_eq!(value["method"], "notifications/initialized");
    }

    #[test]
    fn the_default_version_is_the_newest_supported_one() {
        assert_eq!(SUPPORTED_VERSIONS[0], DEFAULT_VERSION);
        assert!(is_supported("2024-11-05"));
        assert!(!is_supported("2026-07-28"));
    }

    #[test]
    fn frames_are_read_one_line_at_a_time() {
        let read = frames("{\"a\":1}\n{\"b\":2}\n", MAX_FRAME_BYTES);
        assert_eq!(read.len(), 3);
        assert_eq!(
            read[0].as_ref().expect("frame").as_deref(),
            Some("{\"a\":1}")
        );
        assert_eq!(
            read[1].as_ref().expect("frame").as_deref(),
            Some("{\"b\":2}")
        );
        assert!(matches!(read[2], Ok(None)));
    }

    #[test]
    fn a_trailing_line_without_a_newline_is_incomplete_rather_than_a_frame() {
        let mut reader = Cursor::new(b"{\"half\":true".to_vec());
        let error = read_frame(&mut reader, MAX_FRAME_BYTES).expect_err("refused");
        assert_eq!(error.code(), ErrorCode::IncompleteStream);
    }

    #[test]
    fn blank_lines_are_skipped() {
        let read = frames("\n\n{\"a\":1}\n", MAX_FRAME_BYTES);
        assert_eq!(
            read[0].as_ref().expect("frame").as_deref(),
            Some("{\"a\":1}")
        );
    }

    #[test]
    fn a_frame_over_the_cap_is_refused() {
        let text = format!("{}\n", "a".repeat(64));
        let error = frames(&text, 32)
            .into_iter()
            .find_map(std::result::Result::err)
            .expect("refused");
        assert_eq!(error.code(), ErrorCode::TooLarge);
        assert_eq!(error.field(), Some("frame"));
    }

    #[test]
    fn a_frame_of_exactly_the_cap_is_accepted() {
        let body = "a".repeat(32);
        let mut reader = Cursor::new(format!("{body}\n").into_bytes());
        let frame = read_frame(&mut reader, 32).expect("read").expect("frame");
        assert_eq!(frame.len(), 32);
    }

    #[test]
    fn a_frame_that_is_not_utf8_is_refused() {
        let mut reader = Cursor::new(vec![0xff, 0xfe, b'\n']);
        let error = read_frame(&mut reader, MAX_FRAME_BYTES).expect_err("refused");
        assert_eq!(error.code(), ErrorCode::ProtocolViolation);
    }

    #[test]
    fn a_result_response_is_decoded() {
        let text = r#"{"jsonrpc":"2.0","id":3,"result":{"tools":[]}}"#;
        let Incoming::Response { id, outcome } = parse_incoming(text).expect("parsed") else {
            panic!("expected a response");
        };
        assert_eq!(id, 3);
        assert_eq!(outcome.expect("result")["tools"], json!([]));
    }

    #[test]
    fn an_error_response_carries_the_server_code_and_message() {
        let text = r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"no such method"}}"#;
        let Incoming::Response { outcome, .. } = parse_incoming(text).expect("parsed") else {
            panic!("expected a response");
        };
        let error = outcome.expect_err("error");
        assert_eq!(error.code, -32601);
        assert_eq!(error.to_rune().code(), ErrorCode::Unsupported);
        assert_eq!(error.to_rune().message(), "no such method");
    }

    #[test]
    fn a_notification_is_ignored() {
        let text = r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#;
        assert_eq!(parse_incoming(text).expect("parsed"), Incoming::Ignored);
    }

    #[test]
    fn a_frame_without_the_version_is_refused() {
        let text = r#"{"id":1,"result":{}}"#;
        assert_eq!(
            parse_incoming(text).expect_err("refused").code(),
            ErrorCode::ProtocolViolation
        );
    }

    #[test]
    fn a_response_with_neither_result_nor_error_is_refused() {
        let text = r#"{"jsonrpc":"2.0","id":1}"#;
        assert_eq!(
            parse_incoming(text).expect_err("refused").code(),
            ErrorCode::ProtocolViolation
        );
    }

    #[test]
    fn a_reply_body_is_accepted_as_one_document() {
        let body = r#"{"jsonrpc":"2.0","id":2,"result":{"ok":true}}"#;
        assert_eq!(
            response_from_body(body, 2).expect("parsed")["ok"],
            json!(true)
        );
    }

    #[test]
    fn a_reply_body_is_accepted_as_an_event_stream() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"ok\":true}}\n\n";
        assert_eq!(
            response_from_body(body, 2).expect("parsed")["ok"],
            json!(true)
        );
    }

    #[test]
    fn a_reply_without_the_requested_id_is_refused() {
        let body = r#"{"jsonrpc":"2.0","id":9,"result":{}}"#;
        assert_eq!(
            response_from_body(body, 2).expect_err("refused").code(),
            ErrorCode::ProtocolViolation
        );
    }
}
