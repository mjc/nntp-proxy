//! HEAD and STAT workflow tests for RFC 3977 §6.2.
//!
//! These cover the end-to-end semantics that parser-only checks miss:
//! message-id retrieval in per-command mode, and selected-group error behavior
//! for article-number requests in stateful/hybrid sessions.

use anyhow::Result;
use nntp_proxy::RoutingMode;
use tokio::net::{TcpListener, TcpStream};

use crate::test_helpers::{
    MockNntpServer, connect_and_read_greeting, create_test_config, send_command_read_line,
    send_command_read_multiline_response, spawn_proxy_with_config,
};

async fn spawn_client_with_backend(
    mode: RoutingMode,
    backend: MockNntpServer,
) -> Result<TcpStream> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let _backend = backend.spawn_on_listener(backend_listener);

    let config = create_test_config(vec![(backend_port, "head-stat-backend")]);
    let proxy_port = spawn_proxy_with_config(config, mode).await?;
    connect_and_read_greeting(proxy_port).await
}

fn head_stat_backend(port: u16) -> MockNntpServer {
    MockNntpServer::new(port)
        .with_name("HeadStatBackend")
        .on_command(
            "HEAD <present@example.com>",
            "221 0 <present@example.com>\r\nSubject: Present\r\nFrom: tester@example.com\r\n.\r\n",
        )
        .on_command(
            "STAT <present@example.com>",
            "223 0 <present@example.com>\r\n",
        )
        .on_command("STAT missing", "412 No newsgroup selected\r\n")
        .on_command("GROUP alt.test", "211 3 1 3 alt.test\r\n")
        .on_command("HEAD 999", "423 No article with that number\r\n")
        .on_command("STAT 999", "423 No article with that number\r\n")
        .on_command("LAST", "223 1 <first@example.com>\r\n")
}

#[tokio::test]
async fn test_head_by_message_id_returns_multiline_headers_in_per_command_mode() -> Result<()> {
    let mut client =
        spawn_client_with_backend(RoutingMode::PerCommand, head_stat_backend(0)).await?;

    let (status, lines) =
        send_command_read_multiline_response(&mut client, "HEAD <present@example.com>").await?;
    assert!(
        status.starts_with("221"),
        "RFC 3977 §6.2.2: HEAD by message-id should return 221, got: {status:?}"
    );
    let headers = lines.concat();
    assert!(headers.contains("Subject: Present"));
    assert!(headers.contains("From: tester@example.com"));

    Ok(())
}

#[tokio::test]
async fn test_stat_by_message_id_returns_223_without_body_in_per_command_mode() -> Result<()> {
    let mut client =
        spawn_client_with_backend(RoutingMode::PerCommand, head_stat_backend(0)).await?;

    let response = send_command_read_line(&mut client, "STAT <present@example.com>").await?;
    assert!(
        response.starts_with("223"),
        "RFC 3977 §6.2.4: STAT by message-id should return 223, got: {response:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_stat_without_group_returns_412_in_stateful_mode() -> Result<()> {
    let mut client = spawn_client_with_backend(RoutingMode::Stateful, head_stat_backend(0)).await?;

    let response = send_command_read_line(&mut client, "STAT missing").await?;
    assert!(
        response.starts_with("412"),
        "RFC 3977 §6.2.4: STAT without a selected group should return 412, got: {response:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_head_and_stat_missing_article_numbers_return_423_and_session_continues() -> Result<()>
{
    let mut client = spawn_client_with_backend(RoutingMode::Hybrid, head_stat_backend(0)).await?;

    let response = send_command_read_line(&mut client, "GROUP alt.test").await?;
    assert!(
        response.starts_with("211"),
        "Expected GROUP to succeed, got: {response:?}"
    );

    let response = send_command_read_line(&mut client, "HEAD 999").await?;
    assert!(
        response.starts_with("423"),
        "RFC 3977 §6.2.2: missing article numbers should return 423 for HEAD, got: {response:?}"
    );

    let response = send_command_read_line(&mut client, "STAT 999").await?;
    assert!(
        response.starts_with("423"),
        "RFC 3977 §6.2.4: missing article numbers should return 423 for STAT, got: {response:?}"
    );

    let response = send_command_read_line(&mut client, "LAST").await?;
    assert!(
        response.starts_with("223"),
        "A 423 response should not terminate the selected-group session, got: {response:?}"
    );

    Ok(())
}
