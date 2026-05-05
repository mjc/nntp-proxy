//! RFC 4643 CAPABILITIES compliance tests.
//!
//! These tests exercise the proxy's synthetic CAPABILITIES response rather than
//! just command classification. The proxy must expose AUTHINFO before client
//! authentication and suppress it afterward, while hiding backend-only features
//! that do not reflect the proxy's public behavior.

use anyhow::Result;
use tokio::net::{TcpListener, TcpStream};

use crate::test_helpers::{
    MockNntpServer, connect_and_read_greeting, create_test_config_with_auth,
    send_command_read_line, send_command_read_multiline_response, spawn_proxy_with_config,
};
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

async fn spawn_capabilities_client(mode: RoutingMode) -> Result<TcpStream> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let _backend = MockNntpServer::new(backend_port)
        .with_name("CapsBackend")
        .on_command("CAPABILITIES", BACKEND_CAPABILITIES)
        .spawn_on_listener(backend_listener);

    let config = create_test_config_with_auth(vec![backend_port], "alice", "wonderland");
    let proxy_port = spawn_proxy_with_config(config, mode).await?;
    connect_and_read_greeting(proxy_port).await
}

#[tokio::test]
async fn test_capabilities_before_auth_advertises_authinfo_not_backend_extensions() -> Result<()> {
    let mut client = spawn_capabilities_client(RoutingMode::PerCommand).await?;

    let (status, lines) = send_command_read_multiline_response(&mut client, "CAPABILITIES").await?;
    let body = capabilities_body(&lines);

    assert!(
        status.starts_with("101"),
        "RFC 4643 §3.1: CAPABILITIES must succeed before auth, got: {status:?}"
    );
    assert_proxy_capabilities_are_synthetic(&body, true);

    Ok(())
}

#[tokio::test]
async fn test_capabilities_after_auth_omits_authinfo() -> Result<()> {
    let mut client = spawn_capabilities_client(RoutingMode::Stateful).await?;

    let response = send_command_read_line(&mut client, "AUTHINFO USER alice").await?;
    assert!(
        response.starts_with("381"),
        "Expected 381 after AUTHINFO USER, got: {response:?}"
    );

    let response = send_command_read_line(&mut client, "AUTHINFO PASS wonderland").await?;
    assert!(
        response.starts_with("281"),
        "Expected 281 after AUTHINFO PASS, got: {response:?}"
    );

    let (status, lines) = send_command_read_multiline_response(&mut client, "CAPABILITIES").await?;
    let body = capabilities_body(&lines);

    assert!(
        status.starts_with("101"),
        "RFC 4643 §3.1: CAPABILITIES should still succeed after auth, got: {status:?}"
    );
    assert_proxy_capabilities_are_synthetic(&body, false);

    Ok(())
}
