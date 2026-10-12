//! Bounded loopback callback and public-client code exchange. No remote body,
//! callback, state, verifier, or token is included in diagnostics.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use camino::Utf8PathBuf;
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::paths;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ureq::http::Uri;

use super::super::client::http_agent;
use super::AuthorizationMetadata;

const MAX_CALLBACK_BYTES: usize = 8 * 1024;
const MAX_TOKEN_BYTES: u64 = 64 * 1024;
const CALLBACK_PATH: &str = "/oauth/callback";

/// A pending public-client login. The host opens `authorization_url()` in its
/// browser, then calls `complete`. Invalid callbacks may be retried until the
/// original deadline. A state-matched callback consumes the flow before any
/// exchange, including on server rejection or storage failure. Drop closes the
/// listener. This type cannot be cloned or serialized.
pub struct LoopbackAuthorization {
    metadata: AuthorizationMetadata,
    client_id: String,
    listener: Option<TcpListener>,
    redirect_uri: String,
    authorization_url: String,
    state: String,
    verifier: String,
    deadline: Instant,
}

impl fmt::Debug for LoopbackAuthorization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoopbackAuthorization")
            .field("consumed", &self.listener.is_none())
            .finish_non_exhaustive()
    }
}

impl LoopbackAuthorization {
    /// Binds an ephemeral loopback port before constructing the authorization
    /// URL. Metadata must already have passed preflight. The client must be
    /// registered for a loopback redirect with a variable port.
    pub fn start(
        metadata: &AuthorizationMetadata,
        client_id: &str,
        scope: &str,
        timeout: Duration,
    ) -> Result<Self> {
        if client_id.is_empty() || client_id.len() > MAX_CALLBACK_BYTES {
            return Err(oauth_error(
                "client_id",
                "expected a bounded, nonempty client identifier",
            ));
        }
        if scope.len() > MAX_CALLBACK_BYTES {
            return Err(oauth_error("scope", "OAuth scope is too large"));
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .filter(|_| !timeout.is_zero())
            .ok_or_else(|| oauth_error("timeout", "expected a positive, finite OAuth timeout"))?;
        let state = random_secret()?;
        let verifier = random_secret()?;
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .or_else(|_| TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, 0)))
            .map_err(|_| oauth_error("callback", "could not bind an OAuth loopback listener"))?;
        listener.set_nonblocking(true).map_err(|_| callback_io())?;
        let address = listener.local_addr().map_err(|_| callback_io())?;
        let redirect_uri = format!("http://{address}{CALLBACK_PATH}");
        let challenge = challenge(&verifier);
        let parameters = [
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", redirect_uri.as_str()),
            ("resource", metadata.resource()),
            ("scope", scope),
            ("state", state.as_str()),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
        ];
        // Refuse endpoint query parameters that could shadow security fields.
        if let Some(query) = metadata
            .authorization_endpoint()
            .split_once('?')
            .map(|(_, q)| q)
        {
            let existing = parse_form(query)?;
            if parameters
                .iter()
                .any(|(key, _)| existing.contains_key(*key))
            {
                return Err(oauth_error(
                    "authorization_endpoint",
                    "OAuth endpoint query shadows authorization parameters",
                ));
            }
        }
        let separator = if metadata.authorization_endpoint().contains('?') {
            '&'
        } else {
            '?'
        };
        let authorization_url = format!(
            "{}{separator}{}",
            metadata.authorization_endpoint(),
            parameters
                .iter()
                .map(|(k, v)| format!("{k}={}", encode(v)))
                .collect::<Vec<_>>()
                .join("&")
        );
        Ok(Self {
            metadata: metadata.clone(),
            client_id: client_id.to_owned(),
            listener: Some(listener),
            redirect_uri,
            authorization_url,
            state,
            verifier,
            deadline,
        })
    }

    /// Browser navigation URL. Contains state, but never the PKCE verifier.
    pub fn authorization_url(&self) -> &str {
        &self.authorization_url
    }

    /// The exact redirect URI sent in both authorization and token requests.
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Receives one callback, checks state, exchanges its code with the original
    /// verifier, and atomically stores tokens. Neither success nor errors expose
    /// secrets. A second completion after a matched callback is always refused.
    pub fn complete(&mut self, store: &OAuthTokenStore) -> Result<()> {
        let listener = self
            .listener
            .as_ref()
            .ok_or_else(|| oauth_error("callback", "OAuth callback already consumed"))?;
        let (mut stream, _) = loop {
            remaining(self.deadline)?;
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5).min(remaining(self.deadline)?));
                }
                Err(_) => return Err(callback_io()),
            }
        };
        let parameters = self.read_callback(&mut stream);
        let parameters = match parameters {
            Ok(parameters) => parameters,
            Err(error) => {
                respond(&mut stream, false);
                return Err(error);
            }
        };
        if parameters.get("state") != Some(&self.state) {
            respond(&mut stream, false);
            return Err(oauth_error("state", "OAuth callback state does not match"));
        }
        // Consume before exchange. A code must never be sent twice, even if the
        // response is lost or persistence fails after the server issues tokens.
        self.listener.take();
        self.state.clear();
        let result = if parameters.contains_key("error") {
            Err(oauth_error(
                "authorization",
                "OAuth authorization was rejected",
            ))
        } else if let Some(code) = parameters.get("code").filter(|code| !code.is_empty()) {
            self.exchange(code)
                .and_then(|tokens| store.save(&self.metadata, &self.client_id, &tokens))
        } else {
            Err(oauth_error(
                "code",
                "OAuth callback requires an authorization code",
            ))
        };
        self.verifier.clear();
        respond(&mut stream, result.is_ok());
        result
    }

    fn read_callback(&self, stream: &mut TcpStream) -> Result<BTreeMap<String, String>> {
        let mut bytes = Vec::new();
        // Read only a bounded header. Refresh the socket timeout against the
        // absolute deadline so a trickling sender cannot keep the flow alive.
        while !bytes.ends_with(b"\r\n\r\n") {
            if bytes.len() >= MAX_CALLBACK_BYTES {
                return Err(oauth_error("callback", "OAuth callback is too large"));
            }
            stream
                .set_read_timeout(Some(remaining(self.deadline)?))
                .map_err(|_| callback_io())?;
            let mut byte = [0];
            stream.read_exact(&mut byte).map_err(|_| callback_io())?;
            bytes.push(byte[0]);
        }
        let header = std::str::from_utf8(&bytes).map_err(|_| callback_io())?;
        let mut lines = header.split("\r\n");
        let mut request = lines.next().unwrap_or_default().split_whitespace();
        if request.next() != Some("GET") {
            return Err(oauth_error("callback", "OAuth callback requires GET"));
        }
        let target = request.next().ok_or_else(callback_io)?;
        if request.next() != Some("HTTP/1.1") || request.next().is_some() {
            return Err(callback_io());
        }
        let uri: Uri = target.parse().map_err(|_| callback_io())?;
        if uri.scheme().is_some() || uri.authority().is_some() || uri.path() != CALLBACK_PATH {
            return Err(oauth_error(
                "callback",
                "OAuth callback has an unexpected target",
            ));
        }
        let expected_host = self
            .redirect_uri
            .strip_prefix("http://")
            .and_then(|v| v.split_once('/'))
            .map(|(host, _)| host)
            .ok_or_else(callback_io)?;
        let hosts = lines
            .filter_map(|line| line.split_once(':'))
            .filter(|(key, _)| key.eq_ignore_ascii_case("host"))
            .map(|(_, value)| value.trim())
            .collect::<Vec<_>>();
        if hosts.as_slice() != [expected_host] {
            return Err(oauth_error(
                "callback",
                "OAuth callback host does not match the listener",
            ));
        }
        parse_form(uri.query().unwrap_or_default())
    }

    fn exchange(&self, code: &str) -> Result<OAuthTokens> {
        let agent = http_agent();
        let mut response = agent
            .post(self.metadata.token_endpoint())
            .config()
            .timeout_global(Some(remaining(self.deadline)?))
            .max_redirects(0)
            .build()
            .header("Accept", "application/json")
            .send_form([
                ("grant_type", "authorization_code"),
                ("client_id", self.client_id.as_str()),
                ("code", code),
                ("redirect_uri", self.redirect_uri.as_str()),
                ("resource", self.metadata.resource()),
                ("code_verifier", self.verifier.as_str()),
            ])
            .map_err(|_| oauth_error("token_exchange", "OAuth token exchange failed"))?;
        if response.status() != ureq::http::StatusCode::OK {
            return Err(oauth_error("token_exchange", "OAuth token exchange failed"));
        }
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_TOKEN_BYTES)
            .read_to_vec()
            .map_err(|_| {
                oauth_error(
                    "token_response",
                    "could not read bounded OAuth token response",
                )
            })?;
        let tokens = OAuthTokens(
            serde_json::from_slice(&body)
                .map_err(|_| oauth_error("token_response", "invalid OAuth token response"))?,
        );
        tokens.validate()?;
        Ok(tokens)
    }
}

/// Tokens are deliberately not serializable through the public API and have a
/// redacted Debug representation. Only explicit accessors expose credentials.
pub struct OAuthTokens(StoredTokens);

#[derive(Deserialize, Serialize)]
struct StoredTokens {
    access_token: String,
    token_type: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

impl fmt::Debug for OAuthTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OAuthTokens(<redacted>)")
    }
}

impl OAuthTokens {
    pub fn access_token(&self) -> &str {
        &self.0.access_token
    }
    pub fn refresh_token(&self) -> Option<&str> {
        self.0.refresh_token.as_deref()
    }
    /// Reported lifetime at exchange, not a refreshed remaining lifetime.
    pub fn expires_in(&self) -> Option<u64> {
        self.0.expires_in
    }

    fn validate(&self) -> Result<()> {
        if !self.0.token_type.eq_ignore_ascii_case("bearer")
            || self.0.access_token.is_empty()
            || self
                .0
                .access_token
                .bytes()
                .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
            || self.0.refresh_token.as_ref().is_some_and(String::is_empty)
        {
            return Err(oauth_error(
                "token_response",
                "expected nonempty OAuth bearer credentials",
            ));
        }
        Ok(())
    }
}

/// A host-selected private directory, outside project configuration and session
/// artifacts. Separate atomic files bind tokens to resource, issuer, and client.
#[derive(Debug)]
pub struct OAuthTokenStore {
    directory: Utf8PathBuf,
}

impl OAuthTokenStore {
    pub fn new(directory: impl Into<Utf8PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    /// Reads only a bounded, regular private file, refusing widened permissions
    /// and links. Hosts must opt in to using its explicit credential accessors.
    pub fn load(
        &self,
        metadata: &AuthorizationMetadata,
        client_id: &str,
    ) -> Result<Option<OAuthTokens>> {
        if !self.directory.try_exists()? {
            return Ok(None);
        }
        paths::create_dir_private(&self.directory)?;
        let Some(text) = paths::read_private(&self.path(metadata, client_id), MAX_TOKEN_BYTES)?
        else {
            return Ok(None);
        };
        let tokens = OAuthTokens(
            serde_json::from_str(&text)
                .map_err(|_| oauth_error("token_store", "invalid stored OAuth tokens"))?,
        );
        tokens.validate()?;
        Ok(Some(tokens))
    }

    fn path(&self, metadata: &AuthorizationMetadata, client_id: &str) -> Utf8PathBuf {
        let binding =
            serde_json::json!([metadata.resource(), metadata.issuer(), client_id]).to_string();
        self.directory
            .join(format!("{:x}.json", Sha256::digest(binding.as_bytes())))
    }

    fn save(
        &self,
        metadata: &AuthorizationMetadata,
        client_id: &str,
        tokens: &OAuthTokens,
    ) -> Result<()> {
        paths::create_dir_private(&self.directory)?;
        let path = self.path(metadata, client_id);
        let _lock = paths::lock_private(&path.with_extension("lock"))?;
        let encoded = serde_json::to_string(&tokens.0)
            .map_err(|_| oauth_error("token_store", "could not encode OAuth tokens"))?;
        if encoded.len() as u64 > MAX_TOKEN_BYTES {
            return Err(oauth_error(
                "token_store",
                "stored OAuth tokens are too large",
            ));
        }
        paths::write_private_atomic(&path, &encoded)
    }
}

fn oauth_error(field: &str, message: &str) -> RuneError {
    RuneError::invalid_field(field, message)
}
fn callback_io() -> RuneError {
    oauth_error("callback", "could not read OAuth loopback callback")
}
fn remaining(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|v| !v.is_zero())
        .ok_or_else(|| RuneError::new(ErrorCode::Timeout, "OAuth authorization deadline expired"))
}
fn respond(stream: &mut TcpStream, success: bool) {
    let status = if success { "200 OK" } else { "400 Bad Request" };
    if stream
        .set_write_timeout(Some(Duration::from_millis(100)))
        .is_err()
    {
        return;
    }
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Length: 0\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n"
    );
}
fn random_secret() -> Result<String> {
    let mut bytes = [0; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|_| oauth_error("entropy", "OS entropy unavailable for OAuth"))?;
    Ok(base64url(&bytes))
}
fn challenge(verifier: &str) -> String {
    base64url(&Sha256::digest(verifier.as_bytes()).into())
}
fn base64url(bytes: &[u8; 32]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(43);
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = chunk[1]; // A 32-byte input has only 3-byte or 2-byte chunks.
        let c = chunk.get(2).copied().unwrap_or(0);
        out.push(char::from(ALPHABET[usize::from(a >> 2)]));
        out.push(char::from(ALPHABET[usize::from(((a & 3) << 4) | (b >> 4))]));
        out.push(char::from(
            ALPHABET[usize::from(((b & 15) << 2) | (c >> 6))],
        ));
        if chunk.len() == 3 {
            out.push(char::from(ALPHABET[usize::from(c & 63)]));
        }
    }
    out
}
fn encode(value: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}
fn decode(value: &str) -> Result<String> {
    let mut out = Vec::new();
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        out.push(match byte {
            b'+' => b' ',
            b'%' => {
                let high = bytes
                    .next()
                    .and_then(|v| char::from(v).to_digit(16))
                    .ok_or_else(callback_io)?;
                let low = bytes
                    .next()
                    .and_then(|v| char::from(v).to_digit(16))
                    .ok_or_else(callback_io)?;
                ((high << 4) | low) as u8
            }
            _ => byte,
        });
    }
    String::from_utf8(out).map_err(|_| callback_io())
}
fn parse_form(query: &str) -> Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if values.insert(decode(key)?, decode(value)?).is_some() {
            return Err(oauth_error("callback", "duplicate OAuth parameter"));
        }
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_rfc7636_appendix_b() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}
