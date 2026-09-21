//! Server-sent event framing.
//!
//! Framing is done here rather than by a crate because the streaming path needs
//! incremental delivery of partial JSON to a dialect reducer, and because the
//! byte and count bounds must be enforced where the bytes arrive.
//!
//! The grammar handled is the subset model endpoints actually emit: one field
//! per line, `data` payloads, comments ignored, and a blank line dispatching the
//! accumulated event. A byte-order mark on the first line is skipped, and any of
//! the three line terminators is accepted.

use rune_core::error::{ErrorCode, Result, RuneError};

use crate::stream::Limit;

/// One decoded event.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Event {
    /// The event name, when the stream declared one. Model endpoints rarely do.
    pub name: Option<String>,
    /// The concatenated data payload, without the trailing newline.
    pub data: String,
}

impl Event {
    /// Returns true when the payload terminates the stream.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.data.trim() == "[DONE]"
    }

    /// Returns true when the event carries no data.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

/// Incremental decoder over a byte stream.
#[derive(Debug)]
pub struct Decoder {
    /// Bytes of the line currently being assembled.
    line: Vec<u8>,
    /// Event name of the event currently being assembled.
    name: Option<String>,
    /// Data lines of the event currently being assembled.
    data: Vec<u8>,
    /// Whether the first line has been seen, which is when a byte-order mark
    /// is possible.
    started: bool,
    /// Whether the current event has accumulated any field.
    saw_field: bool,
    /// Set when a carriage return was consumed, so a following line feed does
    /// not produce a spurious empty line.
    pending_line_feed: bool,
    /// Total line bytes accepted.
    total_bytes: usize,
    /// Events emitted.
    event_count: usize,
    /// Whether a `[DONE]` payload has been seen.
    done: bool,
    limits: Limit,
}

impl Decoder {
    /// Creates a decoder with the given bounds.
    #[must_use]
    pub fn new(limits: Limit) -> Self {
        Self {
            line: Vec::new(),
            name: None,
            data: Vec::new(),
            started: false,
            saw_field: false,
            pending_line_feed: false,
            total_bytes: 0,
            event_count: 0,
            done: false,
            limits,
        }
    }

    /// Returns true once a `[DONE]` payload has been seen.
    ///
    /// A stream that has said it is done is not read further, even when the
    /// connection stays open, because some gateways keep it alive.
    #[must_use]
    pub const fn is_done(&self) -> bool {
        self.done
    }

    /// Returns the number of events emitted so far.
    #[must_use]
    pub const fn event_count(&self) -> usize {
        self.event_count
    }

    /// Returns the total line bytes accepted so far.
    ///
    /// Counts every byte that reached a line, including field names and
    /// terminators, because the bound exists to cap work rather than to measure
    /// payload.
    #[must_use]
    pub const fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Feeds a chunk of bytes, appending any completed events.
    ///
    /// Chunk boundaries are arbitrary, including mid-line.
    pub fn push(&mut self, chunk: &[u8], out: &mut Vec<Event>) -> Result<()> {
        if self.done {
            return Ok(());
        }

        for byte in chunk {
            match *byte {
                b'\n' => {
                    if self.pending_line_feed {
                        // Second half of a carriage-return line feed pair.
                        self.pending_line_feed = false;
                        continue;
                    }
                    self.end_line(out)?;
                }
                b'\r' => {
                    self.end_line(out)?;
                    self.pending_line_feed = true;
                }
                other => {
                    self.pending_line_feed = false;
                    self.total_bytes = self.total_bytes.saturating_add(1);
                    self.limits.check_total(self.total_bytes)?;
                    self.line.push(other);
                }
            }

            if self.done {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Signals end of stream, emitting any final unterminated event.
    ///
    /// A well-formed stream ends with a blank line, but a truncated one may not.
    /// Emitting the partial event is what lets the reducer report the truncation
    /// rather than the transport inventing a completion.
    pub fn finish(&mut self, out: &mut Vec<Event>) -> Result<()> {
        if self.done {
            return Ok(());
        }
        if !self.line.is_empty() {
            self.end_line(out)?;
        }
        if self.saw_field {
            self.dispatch(out)?;
        }
        Ok(())
    }

    /// Completes the current line.
    fn end_line(&mut self, out: &mut Vec<Event>) -> Result<()> {
        let mut line = std::mem::take(&mut self.line);

        if !self.started {
            self.started = true;
            if line.starts_with(&[0xEF, 0xBB, 0xBF]) {
                line.drain(..3);
            }
        }

        self.limits.check_event(line.len())?;

        if line.is_empty() {
            if self.saw_field {
                self.dispatch(out)?;
            }
            return Ok(());
        }

        // A line beginning with a colon is a comment.
        if line.first() == Some(&b':') {
            return Ok(());
        }

        let (field, value) = split_field(&line);

        match field.as_slice() {
            b"data" => {
                self.saw_field = true;
                if !self.data.is_empty() {
                    self.data.push(b'\n');
                }
                self.data.extend_from_slice(&value);
            }
            b"event" => {
                self.saw_field = true;
                self.name = Some(String::from_utf8_lossy(&value).into_owned());
            }
            b"" => {
                // A colon-less line is a field with no name and no value. It is
                // only meaningful as a dispatch trigger when it is blank, which
                // the empty case above already handled.
                self.saw_field = true;
            }
            _ => {
                // `id` and `retry` are recognized and ignored. Unknown fields are
                // ignored too, because a new field must not break an old client.
                self.saw_field = true;
            }
        }

        Ok(())
    }

    /// Emits the accumulated event.
    fn dispatch(&mut self, out: &mut Vec<Event>) -> Result<()> {
        self.saw_field = false;
        let data = String::from_utf8(std::mem::take(&mut self.data)).map_err(|err| {
            RuneError::new(
                ErrorCode::ProtocolViolation,
                format!("stream event is not valid UTF-8: {err}"),
            )
        })?;
        let name = self.name.take();

        self.event_count = self.event_count.saturating_add(1);
        self.limits.check_events(self.event_count)?;

        let event = Event { name, data };
        if event.is_done() {
            self.done = true;
        }
        out.push(event);
        Ok(())
    }
}

/// Splits a line into its field name and value.
///
/// A single space following the colon is part of the syntax and is removed. A
/// line with no colon is a field with an empty value.
fn split_field(line: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let Some(index) = line.iter().position(|byte| *byte == b':') else {
        return (line.to_vec(), Vec::new());
    };
    let field = line.get(..index).unwrap_or_default().to_vec();
    let mut value = line
        .get(index.saturating_add(1)..)
        .unwrap_or_default()
        .to_vec();
    if value.first() == Some(&b' ') {
        value.remove(0);
    }
    (field, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(chunks: &[&[u8]]) -> Vec<Event> {
        let mut decoder = Decoder::new(Limit::default());
        let mut out = Vec::new();
        for chunk in chunks {
            decoder.push(chunk, &mut out).expect("push");
        }
        decoder.finish(&mut out).expect("finish");
        out
    }

    fn payloads(events: &[Event]) -> Vec<&str> {
        events.iter().map(|event| event.data.as_str()).collect()
    }

    #[test]
    fn a_single_event_is_decoded() {
        let events = decode_all(&[b"data: hello\n\n"]);
        assert_eq!(payloads(&events), vec!["hello"]);
    }

    #[test]
    fn several_events_are_decoded_in_order() {
        let events = decode_all(&[b"data: one\n\ndata: two\n\ndata: three\n\n"]);
        assert_eq!(payloads(&events), vec!["one", "two", "three"]);
    }

    #[test]
    fn a_space_after_the_colon_is_removed_but_not_more() {
        let events = decode_all(&[b"data:  two spaces\n\n"]);
        assert_eq!(payloads(&events), vec![" two spaces"]);
    }

    #[test]
    fn a_field_without_a_colon_has_an_empty_value() {
        let events = decode_all(&[b"data\n\n"]);
        assert_eq!(payloads(&events), vec![""]);
    }

    #[test]
    fn multiple_data_lines_join_with_a_newline() {
        let events = decode_all(&[b"data: first\ndata: second\n\n"]);
        assert_eq!(payloads(&events), vec!["first\nsecond"]);
    }

    #[test]
    fn comments_are_ignored() {
        let events = decode_all(&[b": keepalive\ndata: real\n\n"]);
        assert_eq!(payloads(&events), vec!["real"]);
    }

    #[test]
    fn an_event_name_is_captured() {
        let events = decode_all(&[b"event: message\ndata: body\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name.as_deref(), Some("message"));
        assert_eq!(events[0].data, "body");
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let events = decode_all(&[b"id: 42\nretry: 100\nnotreal: x\ndata: body\n\n"]);
        assert_eq!(payloads(&events), vec!["body"]);
    }

    #[test]
    fn carriage_return_line_feeds_are_handled() {
        let events = decode_all(&[b"data: one\r\n\r\ndata: two\r\n\r\n"]);
        assert_eq!(payloads(&events), vec!["one", "two"]);
    }

    #[test]
    fn bare_carriage_returns_are_handled() {
        let events = decode_all(&[b"data: one\r\rdata: two\r\r"]);
        assert_eq!(payloads(&events), vec!["one", "two"]);
    }

    #[test]
    fn a_byte_order_mark_is_skipped_on_the_first_line() {
        let events = decode_all(&[b"\xEF\xBB\xBFdata: hello\n\n"]);
        assert_eq!(payloads(&events), vec!["hello"]);
    }

    #[test]
    fn a_byte_like_sequence_later_in_the_stream_is_not_stripped() {
        let events = decode_all(&[b"data: a\n\ndata: \xEF\xBB\xBFb\n\n"]);
        assert_eq!(payloads(&events), vec!["a", "\u{feff}b"]);
    }

    #[test]
    fn the_done_payload_terminates_decoding() {
        let mut decoder = Decoder::new(Limit::default());
        let mut out = Vec::new();
        decoder.push(b"data: [DONE]\n\n", &mut out).expect("push");
        assert!(decoder.is_done());
        assert_eq!(payloads(&out), vec!["[DONE]"]);

        // Anything after the terminator is ignored.
        decoder.push(b"data: ignored\n\n", &mut out).expect("push");
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn done_is_recognized_with_surrounding_whitespace() {
        assert!(
            Event {
                name: None,
                data: " [DONE] ".to_owned()
            }
            .is_done()
        );
    }

    #[test]
    fn arbitrary_chunk_boundaries_produce_identical_events() {
        let stream = b"data: alpha\n\ndata: beta\ndata: gamma\n\ndata: [DONE]\n\n";
        let expected = decode_all(&[stream]);

        for split in 0..stream.len() {
            let (left, right) = stream.split_at(split);
            let produced = decode_all(&[left, right]);
            assert_eq!(produced, expected, "split at {split}");
        }
    }

    #[test]
    fn a_single_byte_at_a_time_produces_identical_events() {
        let stream = b"event: x\ndata: body\n\n: comment\ndata: two\n\n";
        let expected = decode_all(&[stream]);
        let chunks: Vec<&[u8]> = stream.chunks(1).collect();
        let produced = decode_all(&chunks);
        assert_eq!(produced, expected);
    }

    #[test]
    fn a_truncated_final_event_is_still_emitted() {
        // No trailing blank line. The event must reach the reducer so it can
        // report the truncation rather than the transport reporting success.
        let events = decode_all(&[b"data: partial"]);
        assert_eq!(payloads(&events), vec!["partial"]);
    }

    #[test]
    fn an_oversized_event_is_rejected_naming_the_bound() {
        let mut decoder = Decoder::new(Limit {
            max_event_bytes: 8,
            ..Limit::default()
        });
        let mut out = Vec::new();
        let err = decoder
            .push(b"data: this payload is far too long\n\n", &mut out)
            .expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert_eq!(err.field(), Some("stream.event"));
    }

    #[test]
    fn exceeding_the_total_bound_is_rejected() {
        let mut decoder = Decoder::new(Limit {
            max_total_bytes: 4,
            ..Limit::default()
        });
        let mut out = Vec::new();
        let err = decoder
            .push(b"data: abcdefgh\n\n", &mut out)
            .expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert_eq!(err.field(), Some("stream.total"));
    }

    #[test]
    fn exceeding_the_event_count_is_rejected() {
        let mut decoder = Decoder::new(Limit {
            max_events: 2,
            ..Limit::default()
        });
        let mut out = Vec::new();
        decoder
            .push(b"data: a\n\ndata: b\n\n", &mut out)
            .expect("push");
        let err = decoder
            .push(b"data: c\n\n", &mut out)
            .expect_err("rejected");
        assert_eq!(err.field(), Some("stream.events"));
    }

    #[test]
    fn invalid_utf8_in_a_payload_is_a_protocol_error() {
        let mut decoder = Decoder::new(Limit::default());
        let mut out = Vec::new();
        let err = decoder
            .push(b"data: \xFF\xFE\n\n", &mut out)
            .expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::ProtocolViolation);
    }

    #[test]
    fn an_empty_chunk_is_harmless() {
        let events = decode_all(&[b"", b"data: x\n\n", b""]);
        assert_eq!(payloads(&events), vec!["x"]);
    }

    #[test]
    fn counters_track_the_stream() {
        let mut decoder = Decoder::new(Limit::default());
        let mut out = Vec::new();
        decoder.push(b"data: abc\n\n", &mut out).expect("push");
        assert_eq!(decoder.event_count(), 1);
        // Nine bytes reach the line: the field name, the separator, and the value.
        assert_eq!(decoder.total_bytes(), 9);
        assert!(!decoder.is_done());
    }

    #[test]
    fn a_final_event_without_a_blank_line_is_dispatched_on_finish() {
        let mut decoder = Decoder::new(Limit::default());
        let mut out = Vec::new();
        decoder.push(b"data: x", &mut out).expect("push");
        assert!(out.is_empty());
        decoder.finish(&mut out).expect("finish");
        assert_eq!(payloads(&out), vec!["x"]);
    }

    #[test]
    fn finish_is_idempotent() {
        let mut decoder = Decoder::new(Limit::default());
        let mut out = Vec::new();
        decoder.push(b"data: x\n\n", &mut out).expect("push");
        decoder.finish(&mut out).expect("finish");
        decoder.finish(&mut out).expect("finish");
        assert_eq!(out.len(), 1);
    }
}
