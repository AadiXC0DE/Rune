#![cfg(not(target_family = "wasm"))]
#![allow(clippy::expect_used)]

use rune_context::mcp::ServerConfig;
use rune_context::mcp::oauth::ProtectedResourceMetadata;
use rune_core::error::ErrorCode;
use serde_json::{Value, json};

const RESOURCE: &str = "https://mcp.example.com/team/mcp";
const ISSUER: &str = "https://auth.example.com/tenant";

fn config(resource: &str) -> ServerConfig {
    ServerConfig::parse(&json!({
        "name": "remote",
        "transport": { "transport": "http", "url": resource }
    }))
    .expect("fixture configuration")
}

fn resource_document(resource: &str, issuer: &str) -> Value {
    json!({
        "resource": resource,
        "authorization_servers": [issuer],
        "scopes_supported": ["tools:read"],
        "future_extension": true
    })
}

fn authorization_document(issuer: &str) -> Value {
    json!({
        "issuer": issuer,
        "authorization_endpoint": "https://login.example.net/authorize?tenant=team",
        "token_endpoint": "https://tokens.example.net/token",
        "registration_endpoint": "https://clients.example.net/register",
        "response_types_supported": ["code"],
        "future_extension": true
    })
}

#[test]
fn secure_metadata_exposes_bound_resource_and_cross_origin_endpoints() {
    let resource = ProtectedResourceMetadata::validate(
        &config(RESOURCE),
        &resource_document(RESOURCE, ISSUER),
    )
    .expect("resource validated");
    assert_eq!(resource.resource(), RESOURCE);
    assert_eq!(resource.authorization_servers(), &[ISSUER]);
    let authorization = resource
        .validate_authorization_server(ISSUER, &authorization_document(ISSUER))
        .expect("authorization validated");
    assert_eq!(authorization.resource(), RESOURCE);
    assert_eq!(authorization.issuer(), ISSUER);
    assert_eq!(
        authorization.authorization_endpoint(),
        "https://login.example.net/authorize?tenant=team"
    );
    assert_eq!(
        authorization.token_endpoint(),
        "https://tokens.example.net/token"
    );
    assert_eq!(
        authorization.registration_endpoint(),
        Some("https://clients.example.net/register")
    );
    let mut document = authorization_document(ISSUER);
    document
        .as_object_mut()
        .expect("object")
        .remove("registration_endpoint");
    assert!(
        resource
            .validate_authorization_server(ISSUER, &document)
            .expect("optional registration")
            .registration_endpoint()
            .is_none()
    );
}

#[test]
fn mismatched_resource_metadata_never_reaches_authorization_preflight() {
    for mismatch in [
        "https://attacker.example/team/mcp",
        "https://mcp.example.com/other/mcp",
        "https://mcp.example.com/team/mcp/",
        "https://MCP.example.com/team/mcp",
        "https://mcp.example.com:443/team/mcp",
        "https://mcp.example.com/team/%6dcp",
    ] {
        let error = ProtectedResourceMetadata::validate(
            &config(RESOURCE),
            &resource_document(mismatch, ISSUER),
        )
        .expect_err(mismatch);
        assert_eq!(error.code(), ErrorCode::InvalidField);
        assert!(error.message().contains("resource"));
    }
}

#[test]
fn issuer_must_be_advertised_and_match_discovery_exactly() {
    let resource = ProtectedResourceMetadata::validate(
        &config(RESOURCE),
        &resource_document(RESOURCE, ISSUER),
    )
    .expect("resource validated");
    for issuer in [
        "https://attacker.example",
        "https://auth.example.com/other",
        "https://auth.example.com/tenant/",
    ] {
        assert!(
            resource
                .validate_authorization_server(issuer, &authorization_document(issuer))
                .is_err()
        );
        let error = resource
            .validate_authorization_server(ISSUER, &authorization_document(issuer))
            .expect_err("issuer mismatch");
        assert!(error.message().contains("issuer"));
    }
}

#[test]
fn every_authorization_endpoint_is_checked_before_any_are_exposed() {
    let resource = ProtectedResourceMetadata::validate(
        &config(RESOURCE),
        &resource_document(RESOURCE, ISSUER),
    )
    .expect("resource validated");
    for field in [
        "authorization_endpoint",
        "token_endpoint",
        "registration_endpoint",
    ] {
        for endpoint in [
            "http://auth.example.com/flow",
            "http://192.168.1.10/flow",
            "http://0.0.0.0/flow",
            "http://[::]/flow",
            "http://localhost.attacker.example/flow",
            "http://127.0.0.1.attacker.example/flow",
            "http://127.1/flow",
            "http://2130706433/flow",
            "http://0x7f000001/flow",
            "http://[::ffff:127.0.0.1]/flow",
            "https://user:secret@auth.example.com/flow",
            "http://localhost@attacker.example/flow",
            "http://attacker.example@localhost/flow",
            "http://localhost\\@attacker.example/flow",
            "http://%6cocalhost/flow",
            "https://auth.example.com/flow#fragment",
            "https://auth.example.com:99999/flow",
            "https://auth.example.com:abc/flow",
            "https://auth.example.com:80:90/flow",
            "https://auth.example.com:/flow",
            "https://auth.example.com:0/flow",
            "https://[not-an-ip]/flow",
            "https:///flow",
            "https://",
            "//auth.example.com/flow",
            "/flow",
            "javascript:alert(1)",
            "https://auth.example.com/flow\n",
        ] {
            let mut document = authorization_document(ISSUER);
            document[field] = json!(endpoint);
            let error = resource
                .validate_authorization_server(ISSUER, &document)
                .expect_err(endpoint);
            assert_eq!(error.code(), ErrorCode::InvalidField);
            assert_eq!(error.field(), Some(field), "{field}: {error}");
            assert!(!error.to_string().contains("secret"));
        }
    }
}

#[test]
fn loopback_http_is_accepted_for_each_url_role() {
    for host in ["localhost", "LOCALHOST", "127.0.0.1", "127.0.0.2", "[::1]"] {
        let endpoint = format!("http://{host}:8765/mcp");
        let resource = ProtectedResourceMetadata::validate(
            &config(&endpoint),
            &resource_document(&endpoint, &endpoint),
        )
        .expect("loopback resource and issuer");
        let document = json!({
            "issuer": endpoint,
            "authorization_endpoint": endpoint,
            "token_endpoint": endpoint,
            "registration_endpoint": endpoint
        });
        let authorization = resource
            .validate_authorization_server(&endpoint, &document)
            .expect("loopback endpoints");
        assert_eq!(authorization.authorization_endpoint(), endpoint);
    }
}

#[test]
fn insecure_resources_and_any_unsafe_advertised_issuer_are_rejected() {
    let endpoint = "http://mcp.example.com/mcp";
    // Plain HTTP remains supported for existing non-OAuth MCP transports.
    let plain_http = config(endpoint);
    assert!(
        ProtectedResourceMetadata::validate(&plain_http, &resource_document(endpoint, ISSUER))
            .is_err()
    );
    for issuer in [
        "http://auth.example.com",
        "https://auth.example.com?tenant=x",
        "https://auth.example.com#fragment",
        "https://user:secret@auth.example.com",
    ] {
        let mut document = resource_document(RESOURCE, ISSUER);
        document["authorization_servers"] = json!([ISSUER, issuer]);
        assert!(ProtectedResourceMetadata::validate(&config(RESOURCE), &document).is_err());
    }
}

#[test]
fn missing_empty_and_malformed_required_metadata_are_rejected() {
    for document in [
        json!({}),
        json!({"resource": RESOURCE}),
        json!({"resource": RESOURCE, "authorization_servers": []}),
        json!({"resource": RESOURCE, "authorization_servers": ISSUER}),
        json!({"resource": RESOURCE, "authorization_servers": [null]}),
        json!({"resource": null, "authorization_servers": [ISSUER]}),
    ] {
        assert!(ProtectedResourceMetadata::validate(&config(RESOURCE), &document).is_err());
    }
    let resource = ProtectedResourceMetadata::validate(
        &config(RESOURCE),
        &resource_document(RESOURCE, ISSUER),
    )
    .expect("resource validated");
    for field in ["issuer", "authorization_endpoint", "token_endpoint"] {
        for replacement in [None, Some(json!(null)), Some(json!(42)), Some(json!(""))] {
            let mut document = authorization_document(ISSUER);
            if let Some(replacement) = replacement {
                document[field] = replacement;
            } else {
                document.as_object_mut().expect("object").remove(field);
            }
            assert!(
                resource
                    .validate_authorization_server(ISSUER, &document)
                    .is_err()
            );
        }
    }
}

#[test]
fn sse_can_validate_metadata_but_stdio_cannot() {
    let sse = ServerConfig::parse(
        &json!({"name": "events", "transport": {"transport": "sse", "url": RESOURCE}}),
    )
    .expect("sse");
    assert!(
        ProtectedResourceMetadata::validate(&sse, &resource_document(RESOURCE, ISSUER)).is_ok()
    );
    let stdio = ServerConfig::parse(
        &json!({"name": "local", "transport": {"transport": "stdio", "command": ["server"]}}),
    )
    .expect("stdio");
    assert!(
        ProtectedResourceMetadata::validate(&stdio, &resource_document(RESOURCE, ISSUER)).is_err()
    );
}
