//! Turns one traced provider exchange into a conformance fixture.
//!
//! `rune --trace` writes numbered `NNNN-request.json` and `NNNN-response.txt`
//! pairs. A fixture keeps the request body and the raw response under
//! `crates/rune-net/tests/fixtures/<dialect>/<name>/`, after a last check that
//! nothing credential shaped is about to be committed.

use std::path::{Path, PathBuf};

/// JSON strings that legitimately hold long opaque runs.
const OPAQUE_FIELDS: &[&str] = &["signature", "encrypted_content", "data", "thoughtSignature"];

/// Shortest run of base64 characters treated as a possible credential.
const MIN_OPAQUE_RUN: usize = 32;

/// `cargo xtask fixture <trace-dir> <name> [--pair NNNN]`.
pub(super) fn run(extra: &[String]) -> Result<(), String> {
    let usage = "usage: cargo xtask fixture <trace-dir> <name> [--pair NNNN]";
    let mut positional = Vec::new();
    let mut pair = None;
    let mut args = extra.iter();
    while let Some(arg) = args.next() {
        if arg == "--pair" {
            let value = args.next().ok_or(usage)?;
            pair = Some(
                value
                    .parse::<u32>()
                    .map_err(|_| format!("`{value}` is not a pair number"))?,
            );
        } else {
            positional.push(arg.as_str());
        }
    }
    let [trace_dir, name] = positional.as_slice() else {
        return Err(usage.to_owned());
    };
    let fixtures = super::workspace_root()?.join("crates/rune-net/tests/fixtures");
    for path in capture(Path::new(trace_dir), name, pair, &fixtures)? {
        println!("wrote {}", path.display());
    }
    Ok(())
}

/// Copies one pair into `<fixtures>/<dialect>/<name>/` and returns the files written.
fn capture(
    trace_dir: &Path,
    name: &str,
    pair: Option<u32>,
    fixtures: &Path,
) -> Result<Vec<PathBuf>, String> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(format!(
            "`{name}` is not a fixture name; use letters, digits, `_` and `-`"
        ));
    }
    let number = match pair {
        Some(number) => number,
        None => highest_pair(trace_dir)?,
    };
    let read = |suffix: &str| {
        let path = trace_dir.join(format!("{number:04}-{suffix}"));
        std::fs::read_to_string(&path).map_err(|err| format!("{}: {err}", path.display()))
    };
    let record: serde_json::Value = serde_json::from_str(&read("request.json")?)
        .map_err(|err| format!("pair {number:04} has an unreadable request: {err}"))?;
    let url = record["url"]
        .as_str()
        .ok_or_else(|| format!("pair {number:04} has no request url"))?;
    let dialect = dialect_for_url(url)?;
    let request =
        serde_json::to_string_pretty(&record["body"]).map_err(|err| err.to_string())? + "\n";
    let response = read("response.txt")?;
    let response = response
        .split_once("\n\n")
        .map(|(_, body)| body.to_owned())
        .ok_or_else(|| format!("pair {number:04} has no response body"))?;

    let dir = fixtures.join(dialect).join(name);
    if dir.exists() {
        return Err(format!("fixture {name} already exists"));
    }
    for (file, text) in [("request.json", &request), ("response.txt", &response)] {
        if let Some(line) = credential_line(text) {
            return Err(format!(
                "{file} line {line} looks like it holds a credential; edit the trace and try again"
            ));
        }
    }
    std::fs::create_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let mut written = Vec::new();
    for (file, text) in [("request.json", &request), ("response.txt", &response)] {
        let path = dir.join(file);
        std::fs::write(&path, text).map_err(|err| format!("{}: {err}", path.display()))?;
        written.push(path);
    }
    Ok(written)
}

/// The highest pair number in a trace directory.
fn highest_pair(trace_dir: &Path) -> Result<u32, String> {
    std::fs::read_dir(trace_dir)
        .map_err(|err| format!("{}: {err}", trace_dir.display()))?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter_map(|name| name.strip_suffix("-request.json")?.parse().ok())
        .max()
        .ok_or_else(|| format!("{} holds no traced request", trace_dir.display()))
}

/// Names the dialect a request was sent in from its URL.
fn dialect_for_url(url: &str) -> Result<&'static str, String> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    if path.ends_with("/chat/completions") {
        Ok("chat_completions")
    } else if path.ends_with("/v1/messages") {
        Ok("anthropic")
    } else if path.ends_with("/responses") {
        Ok("responses")
    } else if path.contains(":streamGenerateContent") {
        Ok("gemini")
    } else {
        Err(format!("no dialect is known for the request url {url}"))
    }
}

/// The first line, counted from one, that looks like it holds a credential.
fn credential_line(text: &str) -> Option<usize> {
    text.lines()
        .position(|line| {
            ["Bearer ", "sk-", "x-api-key"]
                .iter()
                .any(|marker| line.contains(marker))
                || has_opaque_run(line)
        })
        .map(|index| index.saturating_add(1))
}

/// Whether a line has a long base64 run outside a field that may hold one.
fn has_opaque_run(line: &str) -> bool {
    let is_base64 = |byte: &u8| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=');
    let bytes = line.as_bytes();
    let mut start = 0;
    while start < bytes.len() {
        let Some(offset) = bytes
            .get(start..)
            .and_then(|rest| rest.iter().position(is_base64))
        else {
            return false;
        };
        let begin = start.saturating_add(offset);
        let length = bytes.get(begin..).map_or(0, |rest| {
            rest.iter().take_while(|byte| is_base64(byte)).count()
        });
        if length >= MIN_OPAQUE_RUN && !in_opaque_field(line.get(..begin).unwrap_or_default()) {
            return true;
        }
        start = begin.saturating_add(length.max(1));
    }
    false
}

/// Whether the text before a run opens a JSON string named in [`OPAQUE_FIELDS`].
fn in_opaque_field(before: &str) -> bool {
    let Some(quote) = before.rfind('"') else {
        return false;
    };
    let key = before
        .get(..quote)
        .unwrap_or_default()
        .trim_end()
        .strip_suffix(':')
        .map(str::trim_end);
    key.is_some_and(|key| {
        OPAQUE_FIELDS
            .iter()
            .any(|field| key.ends_with(&format!("\"{field}\"")))
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;

    #[test]
    fn fixture_names_its_dialect_from_the_url() {
        let cases = [
            ("http://127.0.0.1:9/v1/chat/completions", "chat_completions"),
            ("https://api.anthropic.com/v1/messages", "anthropic"),
            ("https://api.openai.com/v1/responses", "responses"),
            (
                "https://generativelanguage.googleapis.com/v1beta/models/g:streamGenerateContent?alt=sse",
                "gemini",
            ),
        ];
        for (url, dialect) in cases {
            assert_eq!(dialect_for_url(url), Ok(dialect), "{url}");
        }
        let err = dialect_for_url("http://127.0.0.1:9/v1/models").expect_err("unknown");
        assert!(err.contains("http://127.0.0.1:9/v1/models"), "{err}");
    }

    #[test]
    fn fixture_refuses_a_file_with_a_bearer_token() {
        let text = "{\n  \"model\": \"m\",\n  \"note\": \"Bearer abc\"\n}\n";
        assert_eq!(credential_line(text), Some(3));
        assert_eq!(credential_line("{\n  \"key\": \"sk-abc\"\n}\n"), Some(2));
        assert_eq!(credential_line("x-api-key: k\n"), Some(1));
        assert_eq!(credential_line("{\"model\": \"m\"}\n"), None);
    }

    #[test]
    fn fixture_allows_long_runs_only_in_opaque_fields() {
        let run = "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVo0NTY3ODkw";
        for field in OPAQUE_FIELDS {
            let line = format!("data: {{\"type\":\"x\",\"{field}\": \"{run}\"}}");
            assert_eq!(credential_line(&line), None, "{field}");
        }
        let line = format!("data: {{\"text\":\"{run}\"}}");
        assert_eq!(credential_line(&line), Some(1));
        assert_eq!(
            credential_line("data: {\"text\":\"short words only\"}"),
            None
        );
    }

    #[test]
    fn fixture_copies_a_pair_and_refuses_to_overwrite_it() {
        let root = std::env::temp_dir().join(format!("xtask-fixture-{}", std::process::id()));
        let trace = root.join("trace");
        let fixtures = root.join("fixtures");
        std::fs::create_dir_all(&trace).expect("trace dir");
        std::fs::write(
            trace.join("0001-request.json"),
            r#"{"url":"http://127.0.0.1:9/v1/chat/completions","headers":{},"body":{"model":"old"}}"#,
        )
        .expect("old request");
        std::fs::write(trace.join("0001-response.txt"), "status: 200\n\nold\n").expect("old");
        std::fs::write(
            trace.join("0002-request.json"),
            r#"{"url":"http://127.0.0.1:9/v1/chat/completions","headers":{"authorization":"[REDACTED]"},"body":{"model":"m"}}"#,
        )
        .expect("request");
        std::fs::write(
            trace.join("0002-response.txt"),
            "status: 200\n\ndata: {\"choices\":[]}\n\ndata: [DONE]\n\n",
        )
        .expect("response");

        let written = capture(&trace, "basic", None, &fixtures).expect("captured");
        let dir = fixtures.join("chat_completions").join("basic");
        assert_eq!(
            written,
            [dir.join("request.json"), dir.join("response.txt")]
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("request.json")).expect("request"),
            "{\n  \"model\": \"m\"\n}\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("response.txt")).expect("response"),
            "data: {\"choices\":[]}\n\ndata: [DONE]\n\n"
        );
        assert_eq!(
            capture(&trace, "basic", Some(1), &fixtures),
            Err(String::from("fixture basic already exists"))
        );
        capture(&trace, "older", Some(1), &fixtures).expect("chosen pair");
        assert!(
            std::fs::read_to_string(fixtures.join("chat_completions/older/request.json"))
                .expect("older")
                .contains("old")
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
