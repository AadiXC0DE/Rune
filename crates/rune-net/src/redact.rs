//! Secret redaction.
//!
//! Applied to every error body, log line, and diagnostic that could carry a
//! credential. A credential reaching standard output or a log file is treated as
//! a defect, so this runs on the way out rather than at each call site.

/// Shortest run of credential-looking characters replaced wholesale.
const MIN_SECRET_RUN: usize = 16;

/// Environment variable names whose values must never be printed.
const SECRET_NAMES: &[&str] = &[
    "api_key",
    "api-key",
    "apikey",
    "authorization",
    "auth_token",
    "bearer",
    "client_secret",
    "credential",
    "id_token",
    "password",
    "refresh_token",
    "secret",
    "access_token",
    "session_token",
];

/// Prefixes that mark a token regardless of the field name.
const SECRET_PREFIXES: &[&str] = &[
    "sk-",
    "sk_",
    "pk-",
    "rk-",
    "ghp_",
    "gho_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "AKIA",
    "AIza",
    "eyJ",
];

/// Redacts credential-shaped content from a string.
///
/// Covers the three shapes that actually leak: a `NAME=value` assignment, a
/// bearer token in a header, and a bare token with a recognizable prefix or
/// enough entropy to be one.
#[must_use]
pub fn redact(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for line in input.split_inclusive('\n') {
        out.push_str(&redact_line(line));
    }
    out
}

/// Redacts one line, preserving its terminator.
fn redact_line(line: &str) -> String {
    let (body, terminator) = match line.strip_suffix('\n') {
        Some(rest) => (rest, "\n"),
        None => (line, ""),
    };

    if let Some(redacted) = redact_assignment(body) {
        return format!("{redacted} {terminator}");
    }
    if let Some(redacted) = redact_header(body) {
        return format!("{redacted} {terminator}");
    }

    format!("{}{terminator}", redact_tokens(body))
}

/// Redacts a `NAME=value` or `"name": "value"` assignment.
fn redact_assignment(line: &str) -> Option<String> {
    let (name, value) = line.split_once('=')?;
    if value.is_empty() {
        return None;
    }
    if !looks_secret_name(name) {
        return None;
    }
    Some(format!("{}={}", name.trim_end(), placeholder()))
}

/// Redacts an `Authorization: Bearer <token>` style header.
fn redact_header(line: &str) -> Option<String> {
    let (name, value) = line.split_once(':')?;
    if !looks_secret_name(name) {
        return None;
    }

    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    // The scheme is preserved so a reader can still tell what kind of
    // credential was there, while the credential itself is removed.
    let redacted = match strip_prefix_ci(value, "bearer ") {
        Some(_) => format!("Bearer {}", placeholder()),
        None => placeholder().to_owned(),
    };

    Some(format!("{}: {redacted}", name.trim_end()))
}

/// Replaces bare tokens recognized by prefix or by length and shape.
fn redact_tokens(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut current = String::new();

    for character in line.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | '+') {
            current.push(character);
            continue;
        }
        flush_token(&mut current, &mut out);
        out.push(character);
    }
    flush_token(&mut current, &mut out);
    out
}

/// Appends the current token, replaced when it looks like a credential.
fn flush_token(current: &mut String, out: &mut String) {
    if current.is_empty() {
        return;
    }
    if looks_like_token(current) {
        out.push_str(placeholder());
    } else {
        out.push_str(current);
    }
    current.clear();
}

/// Returns true when a bare token looks like a credential.
fn looks_like_token(token: &str) -> bool {
    if SECRET_PREFIXES
        .iter()
        .any(|prefix| token.starts_with(prefix))
    {
        return true;
    }
    // A long unbroken run mixing letters and digits with no word structure is
    // the shape of an opaque key.
    token.len() >= MIN_SECRET_RUN
        && token.bytes().any(|byte| byte.is_ascii_digit())
        && token.bytes().any(|byte| byte.is_ascii_alphabetic())
}

/// Returns true when a field name suggests its value is a secret.
fn looks_secret_name(name: &str) -> bool {
    let normalized = name
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .to_ascii_lowercase();
    SECRET_NAMES
        .iter()
        .any(|candidate| normalized.contains(candidate))
}

/// Removes a case-insensitive prefix.
fn strip_prefix_ci<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    if value.len() < prefix.len() {
        return None;
    }
    let (head, tail) = value.split_at(prefix.len());
    head.eq_ignore_ascii_case(prefix).then_some(tail)
}

/// The text substituted for a secret.
fn placeholder() -> &'static str {
    "[redacted]"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_api_key_assignment_is_redacted() {
        let redacted = redact("OPENROUTER_API_KEY=abcdef1234567890");
        assert!(!redacted.contains("abcdef1234567890"), "{redacted}");
        assert!(redacted.contains("[redacted]"));
    }

    #[test]
    fn a_bearer_header_is_redacted() {
        let redacted = redact("authorization: Bearer abcdef1234567890");
        assert!(!redacted.contains("abcdef1234567890"), "{redacted}");
    }

    #[test]
    fn a_bearer_header_is_redacted_case_insensitively() {
        let redacted = redact("Authorization: BEARER abcdef1234567890");
        assert!(!redacted.contains("abcdef1234567890"), "{redacted}");
    }

    #[test]
    fn a_prefixed_token_is_redacted_anywhere_in_a_line() {
        for token in [
            "sk-abcdefghijklmnop",
            "ghp_abcdefghijklmnop",
            "AKIAIOSFODNN7EXAMPLE",
            "xoxb-1234567890-abcdef",
        ] {
            let redacted = redact(&format!("failed with {token} attached"));
            assert!(!redacted.contains(token), "leaked {token}: {redacted}");
        }
    }

    #[test]
    fn a_json_credential_field_is_redacted() {
        let redacted = redact(r#"{"api_key": "abcdef1234567890xyz"}"#);
        assert!(!redacted.contains("abcdef1234567890xyz"), "{redacted}");
    }

    #[test]
    fn a_refresh_token_field_is_redacted() {
        let redacted = redact("refresh_token=abcdefghijklmnopqrst");
        assert!(!redacted.contains("abcdefghijklmnopqrst"), "{redacted}");
    }

    #[test]
    fn ordinary_text_is_left_alone() {
        let input = "the model returned an error while reading config.toml";
        assert_eq!(redact(input), input);
    }

    #[test]
    fn a_short_identifier_is_not_mistaken_for_a_secret() {
        // A path segment or short name mixes letters and digits but is not long
        // enough to be an opaque key.
        let input = "could not open src/v2_config.rs";
        assert_eq!(redact(input), input);
    }

    #[test]
    fn ordinary_words_with_digits_survive() {
        let input = "HTTP 429 means rate limited";
        assert_eq!(redact(input), input);
    }

    #[test]
    fn line_structure_is_preserved() {
        let input = "line one\nAPI_KEY=abcdef1234567890\nline three\n";
        let redacted = redact(input);
        assert_eq!(redacted.lines().count(), 3);
        assert!(redacted.starts_with("line one\n"));
        assert!(redacted.ends_with("line three\n"));
    }

    #[test]
    fn a_windows_line_ending_is_preserved() {
        let input = "a\r\nAPI_KEY=abcdef1234567890\r\n";
        let redacted = redact(input);
        assert!(!redacted.contains("abcdef1234567890"));
        assert!(redacted.contains("\r\n"));
    }

    #[test]
    fn several_secrets_in_one_line_are_all_redacted() {
        let input = "first=sk-aaaaaaaaaaaaaaaa second=sk-bbbbbbbbbbbbbbbb";
        let redacted = redact(input);
        assert!(!redacted.contains("sk-aaaaaaaaaaaaaaaa"), "{redacted}");
        assert!(!redacted.contains("sk-bbbbbbbbbbbbbbbb"), "{redacted}");
    }

    #[test]
    fn an_empty_value_is_left_alone() {
        assert_eq!(redact("api_key="), "api_key=");
    }

    #[test]
    fn a_json_web_token_is_redacted_by_prefix() {
        let token = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.payload.signature";
        let redacted = redact(&format!("token was {token}"));
        assert!(
            !redacted.contains("eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9"),
            "{redacted}"
        );
    }

    #[test]
    fn redaction_is_idempotent() {
        let once = redact("api_key=abcdef1234567890");
        assert_eq!(redact(&once), once);
    }

    #[test]
    fn a_url_with_a_userinfo_password_is_reduced() {
        let redacted = redact("https://user:abcdef1234567890@example.com/path");
        assert!(!redacted.contains("abcdef1234567890"), "{redacted}");
    }
}
