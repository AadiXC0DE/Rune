//! Usage of the requests made outside the main loop.
//!
//! The reviewer and the image client run inside tool calls, so they leave what
//! they used here and the host collects it when it next updates its totals.

use std::sync::{Arc, Mutex, PoisonError};

use rune_core::config::Settings;
use rune_core::error::Result;
use rune_core::paths::Paths;
use rune_net::stream::{ProviderEvent, Usage};
use rune_session::usage::{HelperKind, Ledger, UsageRecord, now_ms};

/// Model and usage of each finished helper request, oldest first.
#[derive(Clone, Debug, Default)]
pub struct HelperUsage(Arc<Mutex<Vec<(HelperKind, String, Usage)>>>);

impl HelperUsage {
    /// Records one request.
    pub fn record(&self, kind: HelperKind, model: &str, usage: Usage) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((kind, model.to_owned(), usage));
    }

    /// Records a request from how it ended.
    ///
    /// `completed` is the usage of a request that finished. One that failed
    /// counts only when the provider had already reported usage for it.
    pub fn record_ended(
        &self,
        kind: HelperKind,
        model: &str,
        completed: Option<Usage>,
        reported: Usage,
    ) {
        match completed {
            Some(usage) => self.record(kind, model, usage),
            None if !reported.is_empty() => self.record(kind, model, reported),
            None => {}
        }
    }

    /// Removes and returns everything recorded so far.
    pub fn drain(&self) -> Vec<(HelperKind, String, Usage)> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// Keeps the usage a stream reports, so a request that later fails still counts.
pub fn note_usage(reported: &mut Usage, event: &ProviderEvent) {
    if let ProviderEvent::Usage(usage) = event {
        *reported = reported.merge_max(*usage);
    }
}

/// Returns the name a helper is reported under; a request made for the user has none.
#[must_use]
pub const fn label(kind: HelperKind) -> Option<&'static str> {
    match kind {
        HelperKind::Main => None,
        HelperKind::PermissionReview => Some("permission review"),
        HelperKind::Vision => Some("image analysis"),
        HelperKind::Compaction => Some("compaction"),
        HelperKind::CacheWarming => Some("cache warming"),
    }
}

/// Prices one helper request at the rates of the model that served it.
#[must_use]
pub fn cost(settings: &Settings, paths: &Paths, model: &str, usage: &Usage) -> Option<u64> {
    let mut priced = settings.clone();
    model.clone_into(&mut priced.model);
    crate::provider_setup::cached_prices(&priced, paths)?
        .cost_at(usage, rune_net::transport::now_timestamp())
}

/// Appends one helper request to the usage ledger.
pub fn append_to_ledger(
    paths: &Paths,
    kind: HelperKind,
    model: &str,
    usage: &Usage,
    cost: Option<u64>,
) -> Result<()> {
    let mut record = UsageRecord::new(now_ms(), model, kind);
    record.total_cost = cost.map(rune_net::models_dev::dollars);
    record.input_tokens = usage.input_tokens;
    record.output_tokens = usage.output_tokens;
    record.cache_read_tokens = usage.cache_read_tokens;
    record.cache_write_tokens = usage.cache_write_tokens;
    record.reasoning_tokens = usage.reasoning_tokens;
    Ledger::from_paths(paths).append(&record).map(|_| ())
}

/// A provider stand-in for tests that need the exact request body.
#[cfg(test)]
pub mod fixture {
    use std::io::{BufRead as _, Read as _, Write as _};

    /// Answers one POST with `reply` as an event stream and returns its body.
    pub fn answer_one(reply: String) -> (u16, std::thread::JoinHandle<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .expect("timeout");
            let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone"));
            let mut length = 0_usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("header");
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().expect("length");
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).expect("body");
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            )
            .expect("reply");
            String::from_utf8(body).expect("utf8 body")
        });
        (port, handle)
    }

    /// An Anthropic event stream with these counts, ending the way `stop` names.
    pub fn anthropic_reply(text: &str, stop: &str) -> String {
        use std::fmt::Write as _;

        let events = [
            serde_json::json!({"type": "message_start", "message": {"usage": {"input_tokens": 11, "output_tokens": 1}}}),
            serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
            serde_json::json!({"type": "content_block_stop", "index": 0}),
            serde_json::json!({"type": "message_delta", "delta": {"stop_reason": stop}, "usage": {"output_tokens": 7}}),
            serde_json::json!({"type": "message_stop"}),
        ];
        let mut reply = String::new();
        for event in events {
            let name = event["type"].as_str().unwrap_or("");
            let _ = write!(reply, "event: {name}\ndata: {event}\n\n");
        }
        reply
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_helper_has_a_label_and_a_user_request_has_none() {
        assert_eq!(label(HelperKind::Main), None);
        assert_eq!(
            HelperKind::all()
                .iter()
                .filter_map(|kind| label(*kind))
                .collect::<Vec<_>>(),
            [
                "permission review",
                "image analysis",
                "compaction",
                "cache warming"
            ]
        );
    }

    #[test]
    fn draining_empties_what_a_clone_recorded() {
        let usage = HelperUsage::default();
        usage
            .clone()
            .record(HelperKind::Vision, "m", Usage::default());
        assert_eq!(usage.drain().len(), 1);
        assert!(usage.drain().is_empty());
    }

    #[test]
    fn a_failed_request_counts_only_when_usage_was_reported() {
        let usage = HelperUsage::default();
        usage.record_ended(HelperKind::Vision, "m", None, Usage::default());
        assert!(usage.drain().is_empty());

        let reported = Usage {
            input_tokens: Some(9),
            ..Usage::default()
        };
        usage.record_ended(HelperKind::Vision, "m", None, reported);
        let drained = usage.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].2.input_tokens, Some(9));
    }

    #[test]
    fn a_completed_request_counts_even_without_usage() {
        let usage = HelperUsage::default();
        usage.record_ended(
            HelperKind::Vision,
            "m",
            Some(Usage::default()),
            Usage::default(),
        );
        assert_eq!(usage.drain().len(), 1);
    }

    #[test]
    fn a_ledger_record_carries_its_helper_kind() {
        let dir = tempfile::tempdir().expect("temp");
        let paths = Paths::resolve(
            Some(dir.path().to_str().expect("utf8")),
            None,
            None,
            None,
            None,
        );
        append_to_ledger(
            &paths,
            HelperKind::Compaction,
            "m",
            &Usage {
                output_tokens: Some(3),
                ..Usage::default()
            },
            Some(1_500_000),
        )
        .expect("append");
        let read = Ledger::from_paths(&paths).read().expect("read");
        assert_eq!(read.records.len(), 1);
        assert_eq!(read.records[0].helper, HelperKind::Compaction);
        assert_eq!(read.records[0].output_tokens, Some(3));
        assert!(read.records[0].total_cost.is_some());
    }
}
