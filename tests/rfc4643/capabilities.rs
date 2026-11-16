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

fn assert_proxy_capabilities_are_synthetic(body: &str, expect_authinfo: bool, mode: RoutingMode) {
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
    // Per-command routing rejects every OVER and HDR form, so advertising them
    // would send clients into a 503 they were told would not happen.
    let expect_group_state = mode != RoutingMode::PerCommand;
    for capability in ["OVER\r\n", "HDR\r\n"] {
        assert_eq!(
            body.contains(capability),
            expect_group_state,
            "{mode:?} advertised {capability:?} incorrectly: {body:?}"
        );
    }
}

#[tokio::test]
async fn test_capabilities_before_auth_advertises_authinfo_not_backend_extensions() -> Result<()> {
    let mut client = RfcTestClient::spawn_with_auth(
        RoutingMode::PerCommand,
        "caps-backend",
        "alice",
        "wonderland",
        |_port| {
            MockNntpServer::new()
                .with_name("CapsBackend")
                .on_command("CAPABILITIES", BACKEND_CAPABILITIES)
        },
    )
    .await?;

    let body = capabilities_body(&client.expect_multiline("CAPABILITIES", "101").await?);
    assert_proxy_capabilities_are_synthetic(&body, true, RoutingMode::PerCommand);

    Ok(())
}

#[tokio::test]
async fn test_capabilities_after_auth_omits_authinfo() -> Result<()> {
    let mut client = RfcTestClient::spawn_with_auth(
        RoutingMode::Stateful,
        "caps-backend",
        "alice",
        "wonderland",
        |_port| {
            MockNntpServer::new()
                .with_name("CapsBackend")
                .on_command("CAPABILITIES", BACKEND_CAPABILITIES)
        },
    )
    .await?;

    client.authenticate("alice", "wonderland").await?;

    let body = capabilities_body(&client.expect_multiline("CAPABILITIES", "101").await?);
    assert_proxy_capabilities_are_synthetic(&body, false, RoutingMode::Stateful);

    Ok(())
}

#[tokio::test]
async fn test_capabilities_match_commands_served_in_each_routing_mode() -> Result<()> {
    for mode in [
        RoutingMode::PerCommand,
        RoutingMode::Hybrid,
        RoutingMode::Stateful,
    ] {
        let mut client = RfcTestClient::spawn(mode, "caps-reader-backend", |_port| {
            MockNntpServer::new()
                .with_name("CapsReaderBackend")
                .on_command("CAPABILITIES", BACKEND_CAPABILITIES)
                .on_command("GROUP", "211 1 1 1 alt.test\r\n")
                .on_command("OVER", "224 Overview follows\r\n.\r\n")
                .on_command("HDR", "225 Headers follow\r\n.\r\n")
                .on_command("STAT", "223 0 <present@example.com>\r\n")
        })
        .await?;

        let body = capabilities_body(&client.expect_multiline("CAPABILITIES", "101").await?);
        assert_proxy_capabilities_are_synthetic(&body, false, mode);

        if mode == RoutingMode::PerCommand {
            for command in ["GROUP alt.test", "OVER 1-1", "HDR Subject 1-1"] {
                client.expect_status(command, "503").await?;
            }
        } else {
            client.expect_status("GROUP alt.test", "211").await?;
            client.expect_multiline("OVER 1-1", "224").await?;
            client.expect_multiline("HDR Subject 1-1", "225").await?;
        }
        client
            .expect_status("STAT <present@example.com>", "223")
            .await?;
    }

    Ok(())
}

#[tokio::test]
async fn test_capabilities_before_auth_follow_routing_mode() -> Result<()> {
    for mode in [
        RoutingMode::PerCommand,
        RoutingMode::Hybrid,
        RoutingMode::Stateful,
    ] {
        let mut client =
            RfcTestClient::spawn_with_auth(mode, "caps-backend", "alice", "wonderland", |_port| {
                MockNntpServer::new()
                    .with_name("CapsBackend")
                    .on_command("CAPABILITIES", BACKEND_CAPABILITIES)
            })
            .await?;

        let body = capabilities_body(&client.expect_multiline("CAPABILITIES", "101").await?);
        assert_proxy_capabilities_are_synthetic(&body, true, mode);
    }

    Ok(())
}
