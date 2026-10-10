//! Bounded byte pages from the live conversation's retained tool output.

use rune_core::budget::{BudgetSet, LimitName};
use rune_core::error::{ErrorCode, Result, RuneError};

use crate::contract::{Activity, ExecutionContext, Tool, ToolOutput};
use crate::result_store::{Handle, MAX_READ_BYTES};
use crate::workspace::{string_arg, truncate_to_bytes, usize_arg};

/// Reads retained output without granting access to files or other sessions.
#[derive(Clone, Copy, Debug)]
pub struct ReadToolResult {
    output_cap: usize,
}

impl Default for ReadToolResult {
    fn default() -> Self {
        Self::new(&BudgetSet::new())
    }
}

impl ReadToolResult {
    /// Builds a reader respecting both configured response limits.
    #[must_use]
    pub fn new(budget: &BudgetSet) -> Self {
        Self {
            output_cap: budget
                .get_usize(LimitName::MaxToolResultBytes)
                .min(budget.get_usize(LimitName::MaxTurnResultBytes))
                .min(MAX_READ_BYTES),
        }
    }

    fn read(&self, arguments: &serde_json::Value, context: &ExecutionContext) -> Result<String> {
        let handle = Handle::parse(
            string_arg(arguments, "handle")?.ok_or_else(|| RuneError::missing_field("handle"))?,
        )?;
        let offset = usize_arg(arguments, "offset")?.unwrap_or(0);
        let length = usize_arg(arguments, "length")?.unwrap_or(MAX_READ_BYTES);
        if length == 0 {
            return Err(RuneError::invalid_field("length", "must be positive"));
        }
        let store = context.result_store().ok_or_else(|| {
            RuneError::new(ErrorCode::InvalidState, "no live conversation result store")
        })?;
        let cap = self.output_cap.min(context.max_output_bytes);
        let page = store.read(&handle, offset, length.min(cap).max(1))?;
        if page.offset != offset {
            return Err(RuneError::invalid_field(
                "offset",
                "must be a UTF-8 character boundary; use the previous page's next_offset",
            ));
        }

        // Reserve metadata with the largest possible next offset and the
        // longer EOF spelling. The final metadata can only be shorter.
        let metadata = serde_json::json!({
            "offset": page.offset,
            "next_offset": page.offset.saturating_add(page.text.len()),
            "total_bytes": page.total_bytes,
            "eof": false,
            "text": "",
        });
        let mut available = cap.saturating_sub(metadata.to_string().len());
        let mut end: usize = 0;
        for ch in page.text.chars() {
            let escaped_bytes = match ch {
                '"' | '\\' | '\x08' | '\x0c' | '\n' | '\r' | '\t' => 2,
                '\x00'..='\x1f' => 6,
                _ => ch.len_utf8(),
            };
            if escaped_bytes > available {
                break;
            }
            available = available.saturating_sub(escaped_bytes);
            end = end.saturating_add(ch.len_utf8());
        }
        let text = page.text.get(..end).unwrap_or_default();
        let next_offset = page.offset.saturating_add(text.len());
        let eof = u64::try_from(next_offset).unwrap_or(u64::MAX) >= page.total_bytes;
        if text.is_empty() && !eof {
            return Err(RuneError::new(
                ErrorCode::LimitExceeded,
                "page cannot fit a character; increase length or the response budget",
            ));
        }
        let rendered = serde_json::json!({
            "offset": page.offset,
            "next_offset": next_offset,
            "total_bytes": page.total_bytes,
            "eof": eof,
            "text": text,
        })
        .to_string();
        if rendered.len() > cap {
            return Err(RuneError::new(
                ErrorCode::LimitExceeded,
                "response budget cannot fit page metadata",
            ));
        }
        Ok(rendered)
    }
}

impl Tool for ReadToolResult {
    fn name(&self) -> &'static str {
        "read_tool_result"
    }

    fn description(&self) -> &'static str {
        "Read a byte page of retained tool output using its handle. Pass offset (default 0) and \
         length (default 65536, capped). Returns JSON with text, offset, next_offset, total_bytes \
         and eof. Continue at next_offset until eof to reconstruct the full text. Both response \
         budgets include JSON escaping and metadata. Handles belong to the live conversation."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "handle": {"type": "string", "description": "Handle from a retained result preview."},
                "offset": {"type": "integer", "minimum": 0, "description": "Byte offset, at a UTF-8 character boundary. Defaults to 0."},
                "length": {"type": "integer", "minimum": 1, "description": "Requested bytes, clamped to the response budget and 65536 bytes."},
            },
            "required": ["handle"],
            "additionalProperties": false,
        })
    }

    fn activity(&self) -> Activity {
        Activity::Read
    }

    fn validate(&self, _arguments: &serde_json::Value) -> Result<()> {
        // Decode inside call so validation errors also respect the byte cap.
        Ok(())
    }

    fn call(
        &self,
        arguments: &serde_json::Value,
        context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        context.check_cancelled()?;
        let cap = self.output_cap.min(context.max_output_bytes);
        Ok(match self.read(arguments, context) {
            Ok(text) => ToolOutput::success(text),
            Err(err) => ToolOutput::failure(truncate_to_bytes(err.message(), cap)),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rune_core::budget::Budget;
    use rune_core::config::Layer;
    use serde_json::{Value, json};

    use super::*;
    use crate::inventory;
    use crate::result_store::Store;

    fn fixture(content: &str) -> (Arc<Store>, Handle) {
        let mut store = Store::new("reader-test");
        let preview = store
            .spill("shell", "fixture", content.to_owned(), false, 0)
            .expect("retained");
        (Arc::new(store), preview.handle)
    }

    fn context(store: Arc<Store>, cap: usize) -> ExecutionContext {
        ExecutionContext::new(camino::Utf8PathBuf::from("/tmp"))
            .with_output_cap(cap)
            .with_result_store(store)
    }

    #[test]
    fn registered_reader_reconstructs_escaped_unicode_bytes_under_both_configured_caps() {
        let content = format!("{}TAIL", "é🙂\n\"\\\0\u{0008}\u{000c}\t\r".repeat(50));
        let (store, handle) = fixture(&content);
        for limit in [LimitName::MaxToolResultBytes, LimitName::MaxTurnResultBytes] {
            let mut budget = BudgetSet::new();
            budget
                .set(limit, Budget::Bounded(128), Layer::User)
                .expect("cap");
            let registry = inventory::builtin(
                &crate::FileLimits::default(),
                &budget,
                camino::Utf8Path::new("/skills"),
            )
            .expect("registry");
            let context = context(Arc::clone(&store), 96).fork();
            let mut reconstructed = String::new();
            loop {
                let output = registry
                    .call(
                        "read_tool_result",
                        &json!({"handle": handle.as_str(), "offset": reconstructed.len(), "length": u64::MAX}),
                        &context,
                    )
                    .expect("read");
                assert!(!output.is_error, "{}", output.text);
                assert!(output.text.len() <= 96);
                let page: Value = serde_json::from_str(&output.text).expect("JSON page");
                assert_eq!(page["offset"], reconstructed.len());
                assert_eq!(page["total_bytes"], content.len());
                let text = page["text"].as_str().expect("text");
                assert!(!text.is_empty());
                reconstructed.push_str(text);
                assert_eq!(page["next_offset"], reconstructed.len());
                if page["eof"] == true {
                    break;
                }
            }
            assert_eq!(reconstructed.as_bytes(), content.as_bytes());
        }
    }

    #[test]
    fn configured_cap_and_hard_cap_apply_with_a_larger_context_budget() {
        let (store, handle) = fixture(&"x".repeat(MAX_READ_BYTES * 2));
        for limit in [LimitName::MaxToolResultBytes, LimitName::MaxTurnResultBytes] {
            for cap in [128, MAX_READ_BYTES * 2] {
                let mut budget = BudgetSet::new();
                budget
                    .set(limit, Budget::Bounded(cap as u64), Layer::User)
                    .expect("cap");
                let output = ReadToolResult::new(&budget)
                    .call(
                        &json!({"handle": handle.as_str()}),
                        &context(Arc::clone(&store), usize::MAX),
                    )
                    .expect("read");
                assert!(!output.is_error);
                assert!(output.text.len() <= cap.min(MAX_READ_BYTES));
                let page: Value = serde_json::from_str(&output.text).expect("JSON");
                assert!(page["text"].as_str().expect("text").len() > cap.min(MAX_READ_BYTES) - 128);
                assert_eq!(page["eof"], false);
            }
        }
    }

    #[test]
    fn invalid_arguments_and_unknown_handles_return_bounded_errors() {
        let (store, handle) = fixture("éTAIL");
        let registry = inventory::builtin_default().expect("registry");
        let context = context(store, 96);
        for arguments in [
            json!({}),
            json!({"handle": 4}),
            json!({"handle": "invalid"}),
            json!({"handle": handle.as_str(), "length": 0}),
            json!({"handle": handle.as_str(), "length": -1}),
            json!({"handle": handle.as_str(), "offset": -1}),
            json!({"handle": handle.as_str(), "offset": 1.5}),
            json!({"handle": handle.as_str(), "offset": 1}),
            json!({"handle": handle.as_str(), "length": 1}),
            json!({"handle": Handle::derive("another-session", "fixture", "éTAIL").as_str()}),
        ] {
            let output = registry
                .call("read_tool_result", &arguments, &context)
                .expect("tool error");
            assert!(output.is_error, "{arguments}: {}", output.text);
            assert!(output.text.len() <= 96);
        }
        let unavailable = registry
            .call(
                "read_tool_result",
                &json!({"handle": handle.as_str()}),
                &ExecutionContext::new(camino::Utf8PathBuf::from("/tmp")),
            )
            .expect("unavailable");
        assert!(unavailable.is_error);
        assert!(unavailable.text.contains("no live conversation"));
    }

    #[test]
    fn eof_and_extreme_offsets_do_not_overflow_or_claim_missing_bytes() {
        let (store, handle) = fixture("éTAIL");
        for offset in [7, usize::MAX] {
            let output = ReadToolResult::default()
                .call(
                    &json!({"handle": handle.as_str(), "offset": offset}),
                    &context(Arc::clone(&store), 256),
                )
                .expect("EOF");
            assert!(!output.is_error);
            let page: Value = serde_json::from_str(&output.text).expect("JSON");
            assert_eq!(page["offset"], offset);
            assert_eq!(page["next_offset"], offset);
            assert_eq!(page["total_bytes"], 6);
            assert_eq!(page["text"], "");
            assert_eq!(page["eof"], true);
        }
    }

    #[test]
    fn tiny_and_exhausted_budgets_return_bounded_errors_and_cancellation_propagates() {
        let (store, handle) = fixture("éTAIL");
        for cap in [0, 1, 64] {
            let output = ReadToolResult::default()
                .call(
                    &json!({"handle": handle.as_str()}),
                    &context(Arc::clone(&store), cap),
                )
                .expect("bounded error");
            assert!(output.is_error);
            assert!(output.text.len() <= cap);
        }
        let context = context(store, 128);
        context.cancellation().cancel();
        let error = ReadToolResult::default()
            .call(&json!({"handle": handle.as_str()}), &context)
            .expect_err("cancelled");
        assert_eq!(error.code(), ErrorCode::Cancelled);
    }
}
