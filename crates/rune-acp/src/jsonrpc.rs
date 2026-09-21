//! JSON-RPC 2.0 framing over a byte stream.
//!
//! One message per line of UTF-8 JSON, which is the shape the Agent Client
//! Protocol specifies. Reading is bounded: a peer that streams bytes without ever
//! sending a newline is drained and reported rather than accumulated, so a
//! misbehaving client cannot exhaust the server's memory.

use std::fmt;
use std::io::{BufRead, Write};

use rune_core::error::{ErrorCode, Result, RuneError};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Largest accepted frame, in bytes, excluding the line terminator.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Code reported when a frame is not valid JSON.
pub const PARSE_ERROR: i64 = -32700;
/// Code reported when a frame is not a JSON-RPC 2.0 message.
pub const INVALID_REQUEST: i64 = -32600;
/// Code reported for a method the server does not implement.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// Code reported when a method's parameters are unusable.
pub const INVALID_PARAMS: i64 = -32602;
/// Code reported when a method failed for a reason the client cannot fix.
pub const INTERNAL_ERROR: i64 = -32603;

/// The only protocol version this server speaks.
pub const VERSION: &str = "2.0";

/// Returns the protocol version, for serde defaults.
fn protocol_version() -> String {
    VERSION.to_owned()
}

/// A JSON-RPC identifier.
///
/// Restricted to a number or a string so an identifier can be used as a map key
/// and echoed back exactly. A frame carrying any other shape is rejected.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    /// A numeric identifier.
    Number(i64),
    /// A string identifier.
    Text(String),
}

impl Id {
    /// Reads an identifier from a JSON value.
    #[must_use]
    pub fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Number(number) => number.as_i64().map(Self::Number),
            Value::String(text) => Some(Self::Text(text.clone())),
            _ => None,
        }
    }

    /// Returns the identifier as a JSON value.
    #[must_use]
    pub fn to_value(&self) -> Value {
        match self {
            Self::Number(number) => Value::Number((*number).into()),
            Self::Text(text) => Value::String(text.clone()),
        }
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(number) => write!(f, "{number}"),
            Self::Text(text) => f.write_str(text),
        }
    }
}

/// A structured failure, in either direction.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct RpcError {
    /// One of the JSON-RPC codes, or a server-defined code.
    pub code: i64,
    /// Short description, shown to a user or written to a log.
    pub message: String,
    /// Extra context, when the server has some.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    /// Builds an error.
    #[must_use]
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// Builds a parse error.
    #[must_use]
    pub fn parse(message: impl Into<String>) -> Self {
        Self::new(PARSE_ERROR, message)
    }

    /// Builds an invalid-request error.
    #[must_use]
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(INVALID_REQUEST, message)
    }

    /// Builds a method-not-found error naming the method.
    #[must_use]
    pub fn method_not_found(method: &str) -> Self {
        Self::new(
            METHOD_NOT_FOUND,
            format!("this server does not implement `{method}`"),
        )
    }

    /// Builds an invalid-params error.
    #[must_use]
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, message)
    }

    /// Builds an internal error.
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(INTERNAL_ERROR, message)
    }

    /// Attaches structured context.
    #[must_use]
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    /// Maps a workspace error onto the protocol.
    ///
    /// A failure attributable to the request becomes an invalid-params error, so
    /// the client can correct it; anything else is reported as an internal
    /// fault. The stable workspace code travels in `data`, because the JSON-RPC
    /// codes are too coarse to act on.
    #[must_use]
    pub fn from_rune(error: &RuneError) -> Self {
        let code = match error.code() {
            ErrorCode::Unsupported => METHOD_NOT_FOUND,
            ErrorCode::InvalidField
            | ErrorCode::MissingField
            | ErrorCode::TooLarge
            | ErrorCode::NotFound => INVALID_PARAMS,
            _ => INTERNAL_ERROR,
        };
        let mut data = serde_json::json!({ "code": error.code().as_str() });
        if let Some(hint) = &error.detail().hint {
            data["hint"] = Value::String(hint.clone());
        }
        if let Some(field) = error.field() {
            data["field"] = Value::String(field.to_owned());
        }
        Self::new(code, error.message()).with_data(data)
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

/// A call that expects a response.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Request {
    /// Protocol version.
    #[serde(default = "protocol_version")]
    pub jsonrpc: String,
    /// Identifier echoed back with the response.
    pub id: Id,
    /// Method name.
    pub method: String,
    /// Method parameters.
    #[serde(default)]
    pub params: Value,
}

impl Request {
    /// Builds a request.
    #[must_use]
    pub fn new(id: Id, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: protocol_version(),
            id,
            method: method.into(),
            params,
        }
    }
}

/// A message that expects no response.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Notification {
    /// Protocol version.
    #[serde(default = "protocol_version")]
    pub jsonrpc: String,
    /// Method name.
    pub method: String,
    /// Method parameters.
    #[serde(default)]
    pub params: Value,
}

impl Notification {
    /// Builds a notification.
    #[must_use]
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: protocol_version(),
            method: method.into(),
            params,
        }
    }
}

/// An answer to an earlier request.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Response {
    /// Protocol version.
    #[serde(default = "protocol_version")]
    pub jsonrpc: String,
    /// Identifier of the request being answered.
    ///
    /// Absent, and therefore serialized as null, when the failing frame carried
    /// no identifier that could be recovered.
    #[serde(default)]
    pub id: Option<Id>,
    /// The result, which may be JSON null.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// The failure, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    /// Builds a successful response.
    #[must_use]
    pub fn ok(id: Id, result: Value) -> Self {
        Self {
            jsonrpc: protocol_version(),
            id: Some(id),
            result: Some(result),
            error: None,
        }
    }

    /// Builds a failed response.
    #[must_use]
    pub fn failed(id: Option<Id>, error: RpcError) -> Self {
        Self {
            jsonrpc: protocol_version(),
            id,
            result: None,
            error: Some(error),
        }
    }

    /// Returns the result or the error, whichever the response carries.
    #[must_use]
    pub fn into_outcome(self) -> Option<Result<Value>> {
        match (self.result, self.error) {
            (Some(result), _) => Some(Ok(result)),
            (None, Some(error)) => Some(Err(error)),
            (None, None) => None,
        }
    }
}

/// One framed message.
#[derive(Clone, PartialEq, Debug)]
pub enum Message {
    /// A call expecting a response.
    Request(Request),
    /// A call expecting no response.
    Notification(Notification),
    /// An answer to an earlier call.
    Response(Response),
}

impl Message {
    /// Encodes the message as one JSON line, without its terminator.
    pub fn to_line(&self) -> Result<String> {
        let text = match self {
            Self::Request(request) => serde_json::to_string(request)?,
            Self::Notification(notification) => serde_json::to_string(notification)?,
            Self::Response(response) => serde_json::to_string(response)?,
        };
        Ok(text)
    }

    /// Decodes one JSON line.
    ///
    /// A line that is not valid JSON is a parse error; a line that is valid JSON
    /// but not a JSON-RPC 2.0 message is an invalid request. The distinction
    /// decides the code the client receives.
    pub fn parse(line: &str) -> std::result::Result<Self, FrameError> {
        let value: Value =
            serde_json::from_str(line).map_err(|err| FrameError::Syntax(err.to_string()))?;
        let Some(object) = value.as_object() else {
            return Err(FrameError::Malformed(
                "a message must be a JSON object".to_owned(),
            ));
        };
        match object.get("jsonrpc").and_then(Value::as_str) {
            Some(VERSION) => {}
            Some(other) => {
                return Err(FrameError::Malformed(format!(
                    "unsupported JSON-RPC version `{other}`"
                )));
            }
            None => {
                return Err(FrameError::Malformed(
                    "the `jsonrpc` member is missing".to_owned(),
                ));
            }
        }

        if object.contains_key("method") {
            return parse_call(object);
        }
        if object.contains_key("id")
            || object.contains_key("result")
            || object.contains_key("error")
        {
            return parse_response(object);
        }
        Err(FrameError::Malformed(
            "a message must carry `method`, or `id` with `result` or `error`".to_owned(),
        ))
    }
}

/// Distinguishes a request from a notification, and validates the call shape.
fn parse_call(object: &serde_json::Map<String, Value>) -> std::result::Result<Message, FrameError> {
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        return Err(FrameError::Malformed(
            "`method` must be a string".to_owned(),
        ));
    };
    let params = object.get("params").cloned().unwrap_or(Value::Null);
    let id = match object.get("id") {
        // A null identifier is discouraged by the specification and is read as
        // the notification it behaves like.
        None | Some(Value::Null) => None,
        Some(value) => Some(Id::from_value(value).ok_or_else(|| {
            FrameError::Malformed("`id` must be a number or a string".to_owned())
        })?),
    };
    Ok(match id {
        Some(id) => Message::Request(Request {
            jsonrpc: protocol_version(),
            id,
            method: method.to_owned(),
            params,
        }),
        None => Message::Notification(Notification {
            jsonrpc: protocol_version(),
            method: method.to_owned(),
            params,
        }),
    })
}

/// Validates a response shape.
fn parse_response(
    object: &serde_json::Map<String, Value>,
) -> std::result::Result<Message, FrameError> {
    let id = match object.get("id") {
        None | Some(Value::Null) => None,
        Some(value) => Some(Id::from_value(value).ok_or_else(|| {
            FrameError::Malformed("`id` must be a number or a string".to_owned())
        })?),
    };
    let has_result = object.contains_key("result");
    let has_error = object.get("error").is_some_and(|value| !value.is_null());
    if has_result && has_error {
        return Err(FrameError::Malformed(
            "a response must not carry both `result` and `error`".to_owned(),
        ));
    }
    if !object.contains_key("result") && !object.contains_key("error") {
        return Err(FrameError::Malformed(
            "a response must carry `result` or `error`".to_owned(),
        ));
    }
    let error =
        match object.get("error") {
            None | Some(Value::Null) => None,
            Some(value) => Some(serde_json::from_value::<RpcError>(value.clone()).map_err(
                |err| FrameError::Malformed(format!("`error` is not a JSON-RPC error: {err}")),
            )?),
        };
    Ok(Message::Response(Response {
        jsonrpc: protocol_version(),
        id,
        result: object.get("result").cloned(),
        error,
    }))
}

/// Why a frame could not be read as a message.
#[derive(Clone, PartialEq, Debug)]
pub enum FrameError {
    /// The line was not valid JSON.
    Syntax(String),
    /// The line was valid JSON but not a JSON-RPC 2.0 message.
    Malformed(String),
    /// The frame exceeded the input cap.
    TooLarge {
        /// Bytes the frame occupied.
        observed: usize,
        /// Largest accepted frame.
        limit: usize,
    },
    /// The stream itself failed.
    Io(RuneError),
}

impl FrameError {
    /// The code reported to the peer.
    #[must_use]
    pub const fn code(&self) -> i64 {
        match self {
            Self::Syntax(_) => PARSE_ERROR,
            Self::Malformed(_) | Self::TooLarge { .. } => INVALID_REQUEST,
            Self::Io(_) => INTERNAL_ERROR,
        }
    }

    /// Whether the connection cannot continue.
    #[must_use]
    pub const fn is_fatal(&self) -> bool {
        matches!(self, Self::Io(_))
    }

    /// Returns the failure as a protocol error.
    #[must_use]
    pub fn to_rpc_error(&self) -> RpcError {
        match self {
            Self::Syntax(message) | Self::Malformed(message) => {
                RpcError::new(self.code(), message.clone())
            }
            Self::TooLarge { observed, limit } => {
                let error =
                    RuneError::too_large("frame", *observed, *limit).with_invariant("frame_size");
                RpcError::from_rune(&error)
            }
            Self::Io(error) => RpcError::from_rune(error),
        }
    }

    /// Returns the failure as a workspace error.
    #[must_use]
    pub fn to_rune_error(&self) -> RuneError {
        match self {
            Self::TooLarge { observed, limit } => {
                RuneError::too_large("frame", *observed, *limit).with_invariant("frame_size")
            }
            Self::Syntax(message) => RuneError::new(ErrorCode::InvalidField, message.clone()),
            Self::Malformed(message) => RuneError::new(ErrorCode::InvalidField, message.clone()),
            Self::Io(error) => error.clone(),
        }
    }
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax(message) | Self::Malformed(message) => f.write_str(message),
            Self::TooLarge { observed, limit } => {
                write!(f, "the frame holds {observed} bytes, limit is {limit}")
            }
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

/// Reads messages from a buffered byte stream.
#[derive(Debug)]
pub struct Reader<R> {
    input: R,
}

impl<R: BufRead> Reader<R> {
    /// Wraps a buffered stream.
    pub const fn new(input: R) -> Self {
        Self { input }
    }

    /// Reads one message.
    ///
    /// Returns `None` at the end of the stream, or a rejected frame. A rejected
    /// frame leaves the reader positioned at the start of the next line, so an
    /// oversized or malformed frame costs one error response and nothing else.
    pub fn read_message(&mut self) -> std::result::Result<Option<Message>, FrameError> {
        loop {
            let mut frame: Vec<u8> = Vec::new();
            let mut observed = 0_usize;
            let mut oversized = false;
            let mut complete = false;
            while !complete {
                let (taken, end_of_line, at_eof) = {
                    let available = self
                        .input
                        .fill_buf()
                        .map_err(|err| FrameError::Io(err.into()))?;
                    if available.is_empty() {
                        (0_usize, false, true)
                    } else {
                        let (copy, terminator) =
                            match available.iter().position(|byte| *byte == b'\n') {
                                Some(index) => (index, 1_usize),
                                None => (available.len(), 0_usize),
                            };
                        observed = observed.saturating_add(copy);
                        if !oversized {
                            if frame.len().saturating_add(copy) > MAX_FRAME_BYTES {
                                oversized = true;
                            } else {
                                frame.extend_from_slice(&available[..copy]);
                            }
                        }
                        (copy.saturating_add(terminator), terminator == 1, false)
                    }
                };
                if at_eof {
                    break;
                }
                self.input.consume(taken);
                complete = end_of_line;
            }

            if oversized {
                return Err(FrameError::TooLarge {
                    observed,
                    limit: MAX_FRAME_BYTES,
                });
            }
            if !complete {
                if frame.iter().all(u8::is_ascii_whitespace) {
                    return Ok(None);
                }
                return Err(FrameError::Malformed(
                    "the stream ended inside a frame".to_owned(),
                ));
            }

            while frame.last() == Some(&b'\r') {
                frame.pop();
            }
            if frame.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let text = std::str::from_utf8(&frame)
                .map_err(|err| FrameError::Syntax(format!("the frame is not UTF-8: {err}")))?;
            return Message::parse(text).map(Some);
        }
    }
}

/// Writes messages to a byte stream.
#[derive(Debug)]
pub struct Writer<W> {
    output: W,
}

impl<W: Write> Writer<W> {
    /// Wraps a stream.
    pub const fn new(output: W) -> Self {
        Self { output }
    }

    /// Writes one framed message and flushes it.
    pub fn write_message(&mut self, message: &Message) -> Result<()> {
        let mut line = message.to_line()?;
        line.push('\n');
        self.output.write_all(line.as_bytes())?;
        self.output.flush()?;
        Ok(())
    }

    /// Returns the wrapped stream.
    pub fn into_inner(self) -> W {
        self.output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn reader(text: &str) -> Reader<Cursor<Vec<u8>>> {
        Reader::new(Cursor::new(text.as_bytes().to_vec()))
    }

    #[test]
    fn a_request_round_trips() {
        let line = r#"{"jsonrpc":"2.0","id":7,"method":"session/prompt","params":{"a":1}}"#;
        let message = Message::parse(line).expect("parse");
        let Message::Request(request) = &message else {
            panic!("expected a request");
        };
        assert_eq!(request.id, Id::Number(7));
        assert_eq!(request.method, "session/prompt");
        assert_eq!(message.to_line().expect("encode"), line);
    }

    #[test]
    fn a_call_without_an_id_is_a_notification() {
        let message =
            Message::parse(r#"{"jsonrpc":"2.0","method":"session/cancel"}"#).expect("parse");
        let Message::Notification(notification) = &message else {
            panic!("expected a notification");
        };
        assert_eq!(notification.method, "session/cancel");
        assert_eq!(notification.params, Value::Null);
    }

    #[test]
    fn a_null_id_is_read_as_a_notification() {
        let message = Message::parse(r#"{"jsonrpc":"2.0","id":null,"method":"session/cancel"}"#)
            .expect("parse");
        assert!(matches!(message, Message::Notification(_)));
    }

    #[test]
    fn a_string_id_is_preserved_verbatim() {
        let message =
            Message::parse(r#"{"jsonrpc":"2.0","id":"abc","method":"initialize"}"#).expect("parse");
        let Message::Request(request) = &message else {
            panic!("expected a request");
        };
        assert_eq!(request.id, Id::Text("abc".to_owned()));
    }

    #[test]
    fn a_response_carries_a_null_id_when_none_was_recoverable() {
        let response = Response::failed(None, RpcError::parse("boom"));
        let line = Message::Response(response).to_line().expect("encode");
        assert!(line.contains(r#""id":null"#), "{line}");
        let Message::Response(parsed) = Message::parse(&line).expect("parse") else {
            panic!("expected a response");
        };
        assert_eq!(parsed.id, None);
        assert_eq!(parsed.error.expect("error").code, PARSE_ERROR);
    }

    #[test]
    fn a_null_result_is_preserved() {
        let line = Message::Response(Response::ok(Id::Number(1), Value::Null))
            .to_line()
            .expect("encode");
        assert!(line.contains(r#""result":null"#), "{line}");
    }

    #[test]
    fn a_reader_yields_one_message_per_line() {
        let mut reader = reader(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/list\"}\n",
        );
        let first = reader.read_message().expect("first").expect("a message");
        assert!(matches!(first, Message::Request(_)));
        let second = reader.read_message().expect("second").expect("a message");
        let Message::Request(request) = second else {
            panic!("expected a request");
        };
        assert_eq!(request.id, Id::Number(2));
        assert!(reader.read_message().expect("end").is_none());
    }

    #[test]
    fn a_frame_larger_than_the_cap_is_rejected() {
        let payload = "x".repeat(MAX_FRAME_BYTES.saturating_add(1));
        let line = format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"{payload}\"}}\n");
        let mut reader = reader(&line);
        let error = reader.read_message().expect_err("rejected");
        assert_eq!(
            error,
            FrameError::TooLarge {
                observed: line.len().saturating_sub(1),
                limit: MAX_FRAME_BYTES,
            }
        );
        assert_eq!(error.to_rune_error().code(), ErrorCode::TooLarge);
    }

    #[test]
    fn the_reader_resynchronizes_after_an_oversized_frame() {
        let payload = "x".repeat(MAX_FRAME_BYTES.saturating_add(1));
        let text = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"{payload}\"}}\n{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"initialize\"}}\n"
        );
        let mut reader = reader(&text);
        assert!(reader.read_message().is_err());
        let next = reader.read_message().expect("next").expect("a message");
        let Message::Request(request) = next else {
            panic!("expected a request");
        };
        assert_eq!(request.method, "initialize");
    }

    #[test]
    fn a_line_without_a_terminator_is_not_a_message() {
        let mut reader = reader(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#);
        let error = reader.read_message().expect_err("rejected");
        assert!(matches!(error, FrameError::Malformed(_)));
    }

    #[test]
    fn invalid_json_is_a_parse_error() {
        let error = Message::parse("{not json").expect_err("rejected");
        assert_eq!(error.code(), PARSE_ERROR);
    }

    #[test]
    fn a_missing_version_is_an_invalid_request() {
        let error = Message::parse(r#"{"id":1,"method":"initialize"}"#).expect_err("rejected");
        assert_eq!(error.code(), INVALID_REQUEST);
    }

    #[test]
    fn a_wrong_version_is_an_invalid_request() {
        let error =
            Message::parse(r#"{"jsonrpc":"1.0","id":1,"method":"x"}"#).expect_err("rejected");
        assert_eq!(error.code(), INVALID_REQUEST);
    }

    #[test]
    fn a_response_carrying_both_result_and_error_is_rejected() {
        let error = Message::parse(r#"{"jsonrpc":"2.0","id":1,"result":{},"error":{"code":1}}"#)
            .expect_err("rejected");
        assert_eq!(error.code(), INVALID_REQUEST);
    }

    #[test]
    fn a_writer_terminates_every_frame() {
        let mut writer = Writer::new(Vec::new());
        writer
            .write_message(&Message::Notification(Notification::new(
                "session/update",
                serde_json::json!({ "a": 1 }),
            )))
            .expect("write");
        let text = String::from_utf8(writer.into_inner()).expect("utf8");
        assert!(text.ends_with("\n"), "{text}");
        assert_eq!(text.matches('\n').count(), 1);
    }

    #[test]
    fn an_unsupported_method_is_reported_with_its_code() {
        let error = RpcError::method_not_found("bogus/method");
        assert_eq!(error.code, METHOD_NOT_FOUND);
        assert!(error.message.contains("bogus/method"));
    }

    #[test]
    fn a_request_error_maps_to_invalid_params() {
        let error = RpcError::from_rune(&RuneError::missing_field("sessionId"));
        assert_eq!(error.code, INVALID_PARAMS);
        assert_eq!(error.data.expect("data")["code"], "missing_field");
    }

    #[test]
    fn an_unsupported_error_maps_to_method_not_found() {
        let error = RpcError::from_rune(&RuneError::new(ErrorCode::Unsupported, "no"));
        assert_eq!(error.code, METHOD_NOT_FOUND);
    }
}
