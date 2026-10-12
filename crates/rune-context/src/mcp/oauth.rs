//! OAuth metadata preflight and single-use native loopback authorization.
//!
//! Hosts must validate resource metadata before using its issuers for discovery,
//! then validate authorization server metadata before opening a browser or
//! registering a client. These types expose only validated, immutable values.
//! Resource and issuer comparisons are exact, as required by RFC 9728 section
//! 3.3 and RFC 8414 section 3.3. HTTP is permitted only on loopback endpoints.

use std::net::IpAddr;

use serde::Deserialize;
use serde_json::Value;
use ureq::http::Uri;

use rune_core::error::{Result, RuneError};

use super::config::{ServerConfig, Transport};

mod loopback;
pub use loopback::{LoopbackAuthorization, OAuthTokenStore, OAuthTokens};

/// Protected resource metadata bound to a configured MCP endpoint.
#[derive(Clone, Debug)]
pub struct ProtectedResourceMetadata {
    resource: String,
    authorization_servers: Vec<String>,
}

#[derive(Deserialize)]
struct RawResourceMetadata {
    resource: String,
    authorization_servers: Vec<String>,
}

impl ProtectedResourceMetadata {
    /// Validates a discovery response without contacting any endpoint.
    /// Unknown metadata fields are ignored, as required by RFC 9728.
    pub fn validate(config: &ServerConfig, document: &Value) -> Result<Self> {
        let resource = match &config.transport {
            Transport::Http { url, .. } | Transport::Sse { url } => url,
            Transport::Stdio { .. } => {
                return Err(RuneError::invalid_field(
                    "oauth",
                    "OAuth metadata requires an HTTP or SSE server",
                ));
            }
        };
        check_endpoint("resource", resource)?;
        let raw: RawResourceMetadata = serde_json::from_value(document.clone()).map_err(|_| {
            RuneError::invalid_field(
                "resource_metadata",
                "expected a resource string and an authorization_servers array of strings",
            )
        })?;
        if raw.resource != *resource {
            return Err(RuneError::invalid_field(
                "resource",
                "protected-resource metadata does not match the configured MCP endpoint",
            ));
        }
        if raw.authorization_servers.is_empty() {
            return Err(RuneError::invalid_field(
                "authorization_servers",
                "protected-resource metadata must advertise at least one authorization server",
            ));
        }
        for issuer in &raw.authorization_servers {
            check_issuer("authorization_servers", issuer)?;
        }
        Ok(Self {
            resource: raw.resource,
            authorization_servers: raw.authorization_servers,
        })
    }

    /// Resource identifier to include in subsequent OAuth requests.
    pub fn resource(&self) -> &str {
        &self.resource
    }

    /// Validated issuers from which the host may select a discovery target.
    pub fn authorization_servers(&self) -> &[String] {
        &self.authorization_servers
    }

    /// Binds an authorization server response to the selected discovery issuer.
    /// Call this before launching authorization or client registration. The
    /// selected issuer must come from this resource's validated metadata.
    pub fn validate_authorization_server(
        &self,
        selected_issuer: &str,
        document: &Value,
    ) -> Result<AuthorizationMetadata> {
        if !self
            .authorization_servers
            .iter()
            .any(|issuer| issuer == selected_issuer)
        {
            return Err(RuneError::invalid_field(
                "issuer",
                "selected issuer is not advertised by the protected resource",
            ));
        }
        let raw: RawAuthorizationMetadata =
            serde_json::from_value(document.clone()).map_err(|_| {
                RuneError::invalid_field(
                    "authorization_metadata",
                    "expected issuer, authorization_endpoint and token_endpoint strings, and an optional registration_endpoint string",
                )
            })?;
        if raw.issuer != selected_issuer {
            return Err(RuneError::invalid_field(
                "issuer",
                "authorization metadata issuer does not match the selected discovery issuer",
            ));
        }
        check_endpoint("authorization_endpoint", &raw.authorization_endpoint)?;
        check_endpoint("token_endpoint", &raw.token_endpoint)?;
        if let Some(endpoint) = &raw.registration_endpoint {
            check_endpoint("registration_endpoint", endpoint)?;
        }
        Ok(AuthorizationMetadata {
            resource: self.resource.clone(),
            issuer: raw.issuer,
            authorization_endpoint: raw.authorization_endpoint,
            token_endpoint: raw.token_endpoint,
            registration_endpoint: raw.registration_endpoint,
        })
    }
}

#[derive(Deserialize)]
struct RawAuthorizationMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
}

/// Authorization endpoints exposed only after the entire response passes preflight.
#[derive(Clone, Debug)]
pub struct AuthorizationMetadata {
    resource: String,
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
}

impl AuthorizationMetadata {
    pub fn resource(&self) -> &str {
        &self.resource
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    pub fn authorization_endpoint(&self) -> &str {
        &self.authorization_endpoint
    }

    pub fn token_endpoint(&self) -> &str {
        &self.token_endpoint
    }

    pub fn registration_endpoint(&self) -> Option<&str> {
        self.registration_endpoint.as_deref()
    }
}

fn check_issuer(field: &str, issuer: &str) -> Result<()> {
    check_endpoint(field, issuer)?;
    if issuer.contains('?') {
        return Err(RuneError::invalid_field(
            field,
            "an OAuth issuer cannot contain a query",
        ));
    }
    Ok(())
}

fn check_endpoint(field: &str, endpoint: &str) -> Result<()> {
    let invalid = || {
        RuneError::invalid_field(
            field,
            "expected an absolute HTTPS URL or loopback HTTP URL without credentials or a fragment",
        )
    };
    // Reject characters a browser may reinterpret differently from an HTTP URI.
    if endpoint.contains(['\\', '#'])
        || endpoint
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(invalid());
    }
    let uri: Uri = endpoint.parse().map_err(|_| invalid())?;
    let authority = uri.authority().ok_or_else(invalid)?;
    if authority.as_str().contains('@') {
        return Err(invalid());
    }
    let host = authority.host();
    if host.is_empty() {
        return Err(invalid());
    }
    // Authority accepts some ports that cannot be represented as a TCP port.
    let suffix = authority.as_str().strip_prefix(host).ok_or_else(invalid)?;
    if !suffix.is_empty() && (authority.port_u16().is_none() || authority.port_u16() == Some(0)) {
        return Err(invalid());
    }
    let address = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'));
    if let Some(address) = address {
        if address.parse::<std::net::Ipv6Addr>().is_err() {
            return Err(invalid());
        }
    } else if !host
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'))
    {
        return Err(invalid());
    }
    let loopback = host.eq_ignore_ascii_case("localhost")
        || address
            .unwrap_or(host)
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    match uri.scheme_str() {
        Some("https") => Ok(()),
        Some("http") if loopback => Ok(()),
        _ => Err(invalid()),
    }
}
