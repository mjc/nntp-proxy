//! XOVER edge-case tests for RFC 3977.
//!
//! Verify XOVER behavior in stateful and hybrid modes when a group is selected.

use anyhow::Result;
use tokio::net::TcpListener;

use crate::test_helpers::{
    MockNntpServer, connect_and_read_greeting, create_test_config, send_command_read_line,
    send_command_read_multiline_response, spawn_proxy_with_config,
};
use nntp_proxy::RoutingMode;

async fn spawn_client_with_backend(
    mode: RoutingMode,
    backend: MockNntpServer,
) -> Result<tokio::net::TcpStream> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let _backend = backend.spawn_on_listener(backend_listener);

    let config = create_test_config(vec![(backend_port, "xover-backend")]);
    let proxy_port = spawn_proxy_with_config(config, mode).await?;
    connect_and_read_greeting(proxy_port).await
}

fn xover_backend(port: u16) -> MockNntpServer {
    MockNntpServer::new(port)
        .with_name("XoverBackend")
        .on_command("GROUP", "211 1 1 1 alt.test\r\n")
        .on_command("XOVER 1-0", "501 Syntax error in range\r\n")
        .on_command("XOVER 999-1000", "224 XOVER follows\r\n.\r\n")
        .on_command(
            "XOVER",
            "224 XOVER follows\r\n1\t<msg1@example.com>\tSubject1\tAuthor1\r\n.\r\n",
        )
}

fn no_group_xover_backend(port: u16) -> MockNntpServer {
    MockNntpServer::new(port)
        .with_name("NoGroupXoverBackend")
        .on_command("XOVER", "412 No newsgroup selected\r\n")
        .on_command("GROUP", "211 1 1 1 alt.test\r\n")
}

#[tokio::test]
async fn test_xover_returns_multiline_in_stateful_after_group() -> Result<()> {
    let mut client = spawn_client_with_backend(RoutingMode::Stateful, xover_backend(0)).await?;

    let response = send_command_read_line(&mut client, "GROUP alt.test").await?;
    assert!(
        response.starts_with("211"),
        "Expected 211 for GROUP, got: {response:?}"
    );

    let (status, lines) = send_command_read_multiline_response(&mut client, "XOVER").await?;
    assert!(
        status.starts_with("224"),
        "Expected 224 XOVER response, got: {status:?}"
    );
    let concatenated = lines.concat();
    assert!(
        concatenated.contains("<msg1@example.com>"),
        "XOVER body should include message-id, got: {lines:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_xover_returns_multiline_in_hybrid_after_group() -> Result<()> {
    let mut client = spawn_client_with_backend(RoutingMode::Hybrid, xover_backend(0)).await?;

    let response = send_command_read_line(&mut client, "GROUP alt.test").await?;
    assert!(
        response.starts_with("211"),
        "Expected 211 for GROUP, got: {response:?}"
    );

    let (status, lines) = send_command_read_multiline_response(&mut client, "XOVER").await?;
    assert!(
        status.starts_with("224"),
        "Expected 224 XOVER response, got: {status:?}"
    );
    let concatenated = lines.concat();
    assert!(
        concatenated.contains("<msg1@example.com>"),
        "XOVER body should include message-id, got: {lines:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_xover_without_group_returns_412_and_session_continues() -> Result<()> {
    let mut client =
        spawn_client_with_backend(RoutingMode::Stateful, no_group_xover_backend(0)).await?;

    let response = send_command_read_line(&mut client, "XOVER").await?;
    assert!(
        response.starts_with("412"),
        "RFC 3977 §8.4: XOVER without a selected group should return 412, got: {response:?}"
    );

    let response = send_command_read_line(&mut client, "GROUP alt.test").await?;
    assert!(
        response.starts_with("211"),
        "The session should remain usable after a 412 XOVER response, got: {response:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_xover_invalid_and_empty_ranges_behave_as_expected() -> Result<()> {
    let mut client = spawn_client_with_backend(RoutingMode::Hybrid, xover_backend(0)).await?;

    let response = send_command_read_line(&mut client, "GROUP alt.test").await?;
    assert!(
        response.starts_with("211"),
        "Expected 211 for GROUP, got: {response:?}"
    );

    let response = send_command_read_line(&mut client, "XOVER 1-0").await?;
    assert!(
        response.starts_with("501"),
        "Invalid XOVER ranges should be rejected with 501, got: {response:?}"
    );

    let (status, lines) =
        send_command_read_multiline_response(&mut client, "XOVER 999-1000").await?;
    assert!(
        status.starts_with("224"),
        "Empty XOVER ranges should still return 224 with an empty body, got: {status:?}"
    );
    assert!(
        lines.is_empty(),
        "Expected no overview lines for an empty range"
    );

    Ok(())
}
