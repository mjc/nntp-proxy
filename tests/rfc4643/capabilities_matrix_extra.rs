//! Additional CAPABILITIES matrix tests (RFC 4643)
//!
//! Ensure CAPABILITIES injection/stripping is correct across routing modes.

use anyhow::Result;
use tokio::net::TcpListener;

use crate::test_helpers::{
    MockNntpServer, connect_and_read_greeting, create_test_config_with_auth,
    send_command_read_line, send_command_read_multiline_response, spawn_proxy_with_config,
};
use nntp_proxy::RoutingMode;

const BACKEND_CAPS_VARIANT: &str =
    "101 Backend CAPABILITIES\r\nVERSION 2\r\nSTARTTLS\r\nSASL PLAIN\r\nCOMPRESS GZIP\r\n.\r\n";

async fn spawn_caps_client_with_mode(mode: RoutingMode) -> Result<tokio::net::TcpStream> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let _back = MockNntpServer::new(port)
        .with_name("CapsVariantBackend")
        .on_command("CAPABILITIES", BACKEND_CAPS_VARIANT)
        .spawn_on_listener(listener);

    let config = create_test_config_with_auth(vec![port], "alice", "wonderland");
    let proxy_port = spawn_proxy_with_config(config, mode).await?;
    connect_and_read_greeting(proxy_port).await
}

#[tokio::test]
async fn test_capabilities_in_hybrid_before_auth_injects_authinfo() -> Result<()> {
    let mut client = spawn_caps_client_with_mode(RoutingMode::Hybrid).await?;

    let (status, lines) = send_command_read_multiline_response(&mut client, "CAPABILITIES").await?;
    let body = lines.concat();
    assert!(status.starts_with("101"), "Expected 101 CAPABILITIES status, got: {status:?}");
    assert!(body.contains("AUTHINFO USER PASS"), "Proxy should inject AUTHINFO when configured for client auth and before auth, got: {body:?}");
    assert!(!body.contains("STARTTLS"), "Proxy must not leak STARTTLS, got: {body:?}");
    Ok(())
}

#[tokio::test]
async fn test_capabilities_in_per_command_after_auth_strips_authinfo() -> Result<()> {
    let mut client = spawn_caps_client_with_mode(RoutingMode::PerCommand).await?;

    let resp = send_command_read_line(&mut client, "AUTHINFO USER alice").await?;
    assert!(resp.starts_with("381"), "Expected 381 password required, got: {resp:?}");
    let resp = send_command_read_line(&mut client, "AUTHINFO PASS wonderland").await?;
    assert!(resp.starts_with("281"), "Expected 281 auth accepted, got: {resp:?}");

    let (status, lines) = send_command_read_multiline_response(&mut client, "CAPABILITIES").await?;
    let body = lines.concat();
    assert!(status.starts_with("101"), "Expected 101 CAPABILITIES after auth, got: {status:?}");
    assert!(!body.contains("AUTHINFO"), "Proxy must not advertise AUTHINFO after client auth, got: {body:?}");
    Ok(())
}
