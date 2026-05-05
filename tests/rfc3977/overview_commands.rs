//! RFC 3977 overview/header command tests (`OVER`, `HDR`, `XHDR`).
//!
//! These commands depend on selected-group reader state and return multiline
//! responses that must preserve framing and ordering.

use anyhow::Result;
use tokio::net::{TcpListener, TcpStream};

use crate::test_helpers::{
    MockNntpServer, connect_and_read_greeting, create_test_config, send_command_read_line,
    send_command_read_multiline_response, spawn_proxy_with_config,
};
use nntp_proxy::RoutingMode;

async fn spawn_client_with_backend(
    mode: RoutingMode,
    backend: MockNntpServer,
) -> Result<TcpStream> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let _backend = backend.spawn_on_listener(backend_listener);

    let config = create_test_config(vec![(backend_port, "overview-backend")]);
    let proxy_port = spawn_proxy_with_config(config, mode).await?;
    connect_and_read_greeting(proxy_port).await
}

fn overview_backend(port: u16) -> MockNntpServer {
    MockNntpServer::new(port)
        .with_name("OverviewBackend")
        .on_command("GROUP", "211 2 1 2 alt.test\r\n")
        .on_command(
            "OVER",
            "224 Overview follows\r\n1\t<msg1@example.com>\tSubject one\tAuthor One\r\n.\r\n",
        )
        .on_command(
            "HDR SUBJECT",
            "225 Headers follow\r\n1 Subject one\r\n2 Subject two\r\n.\r\n",
        )
        .on_command(
            "XHDR SUBJECT",
            "225 Headers follow\r\n1 Subject one\r\n2 Subject two\r\n.\r\n",
        )
}

fn no_group_over_backend(port: u16) -> MockNntpServer {
    MockNntpServer::new(port)
        .with_name("NoGroupOverviewBackend")
        .on_command("OVER", "412 No newsgroup selected\r\n")
        .on_command("GROUP", "211 2 1 2 alt.test\r\n")
}

#[tokio::test]
async fn test_over_returns_multiline_in_stateful_after_group() -> Result<()> {
    let mut client = spawn_client_with_backend(RoutingMode::Stateful, overview_backend(0)).await?;

    let response = send_command_read_line(&mut client, "GROUP alt.test").await?;
    assert!(
        response.starts_with("211"),
        "Expected 211 for GROUP, got: {response:?}"
    );

    let (status, lines) = send_command_read_multiline_response(&mut client, "OVER 1-2").await?;
    assert!(
        status.starts_with("224"),
        "RFC 3977 §8.4: OVER should return 224 on success, got: {status:?}"
    );
    let body = lines.concat();
    assert!(
        body.contains("<msg1@example.com>") && body.contains("Subject one"),
        "Expected overview data with message-id and subject, got: {body:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_hdr_and_xhdr_return_multiline_in_hybrid_after_group() -> Result<()> {
    let mut client = spawn_client_with_backend(RoutingMode::Hybrid, overview_backend(0)).await?;

    let response = send_command_read_line(&mut client, "GROUP alt.test").await?;
    assert!(
        response.starts_with("211"),
        "Expected 211 for GROUP, got: {response:?}"
    );

    let (hdr_status, hdr_lines) =
        send_command_read_multiline_response(&mut client, "HDR Subject 1-2").await?;
    assert!(
        hdr_status.starts_with("225"),
        "RFC 3977 §8.4: HDR should return 225 on success, got: {hdr_status:?}"
    );
    assert_eq!(
        hdr_lines,
        vec![
            "1 Subject one\r\n".to_string(),
            "2 Subject two\r\n".to_string()
        ]
    );

    let (xhdr_status, xhdr_lines) =
        send_command_read_multiline_response(&mut client, "XHDR Subject 1-2").await?;
    assert!(
        xhdr_status.starts_with("225"),
        "RFC 3977 §8.4: XHDR should return 225 on success, got: {xhdr_status:?}"
    );
    assert_eq!(
        xhdr_lines,
        vec![
            "1 Subject one\r\n".to_string(),
            "2 Subject two\r\n".to_string()
        ]
    );

    Ok(())
}

#[tokio::test]
async fn test_over_without_group_returns_412_and_session_continues() -> Result<()> {
    let mut client =
        spawn_client_with_backend(RoutingMode::Stateful, no_group_over_backend(0)).await?;

    let response = send_command_read_line(&mut client, "OVER 1-2").await?;
    assert!(
        response.starts_with("412"),
        "RFC 3977 §8.4: OVER without a selected group should return 412, got: {response:?}"
    );

    let response = send_command_read_line(&mut client, "GROUP alt.test").await?;
    assert!(
        response.starts_with("211"),
        "Session should remain usable after a 412 OVER response, got: {response:?}"
    );

    Ok(())
}
