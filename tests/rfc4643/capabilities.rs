//! RFC 4643 CAPABILITIES compliance tests.
//!
//! These tests exercise the proxy's synthetic CAPABILITIES response rather than
//! just command classification. The proxy must expose AUTHINFO before client
//! authentication and suppress it afterward, while hiding backend-only features
//! that do not reflect the proxy's public behavior.

use anyhow::Result;

use crate::test_helpers::{MockNntpServer, RfcTestClient};
use nntp_proxy::RoutingMode;

const BACKEND_CAPABILITIES: &str =
    "101 Backend capability list:\r\nVERSION 2\r\nSTARTTLS\r\nCOMPRESS GZIP\r\nSASL PLAIN\r\n.\r\n";

fn capabilities_body(lines: &[String]) -> String {
    lines.concat()
}

fn assert_proxy_capabilities_are_synthetic(body: &str, expect_authinfo: bool) {
    assert_eq!(
        body.contains("AUTHINFO USER PASS\r\n"),
        expect_authinfo,
        "Unexpected AUTHINFO advertisement in CAPABILITIES body: {body:?}"
    );
    assert!(
        !body.contains("STARTTLS\r\n")
            && !body.contains("COMPRESS GZIP\r\n")
            && !body.contains("SASL PLAIN\r\n"),
        "Proxy CAPABILITIES should not leak backend-only extensions, got: {body:?}"
    );
    assert!(body.contains("VERSION 2\r\n"));
    assert!(body.contains("READER\r\n"));
    assert!(body.contains("OVER\r\n"));
    assert!(body.contains("HDR\r\n"));
}

#[tokio::test]
async fn test_capabilities_before_auth_advertises_authinfo_not_backend_extensions() -> Result<()> {
    let mut client = RfcTestClient::spawn_with_auth(
        RoutingMode::PerCommand,
        "caps-backend",
        "alice",
        "wonderland",
        |port| {
            MockNntpServer::new(port)
                .with_name("CapsBackend")
                .on_command("CAPABILITIES", BACKEND_CAPABILITIES)
        },
    )
    .await?;

    let body = capabilities_body(&client.expect_multiline("CAPABILITIES", "101").await?);
    assert_proxy_capabilities_are_synthetic(&body, true);

    Ok(())
}

#[tokio::test]
async fn test_capabilities_after_auth_omits_authinfo() -> Result<()> {
    let mut client = RfcTestClient::spawn_with_auth(
        RoutingMode::Stateful,
        "caps-backend",
        "alice",
        "wonderland",
        |port| {
            MockNntpServer::new(port)
                .with_name("CapsBackend")
                .on_command("CAPABILITIES", BACKEND_CAPABILITIES)
        },
    )
    .await?;

    client.authenticate("alice", "wonderland").await?;

    let body = capabilities_body(&client.expect_multiline("CAPABILITIES", "101").await?);
    assert_proxy_capabilities_are_synthetic(&body, false);

    Ok(())
}
