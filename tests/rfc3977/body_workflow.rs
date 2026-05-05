//! RFC 3977 BODY workflow tests.
//!
//! These cover end-to-end BODY semantics that differ between message-id and
//! selected-group article-number retrieval, including multiline framing for
//! large bodies and the expected 412/423 error paths.

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

    let config = create_test_config(vec![(backend_port, "body-backend")]);
    let proxy_port = spawn_proxy_with_config(config, mode).await?;
    connect_and_read_greeting(proxy_port).await
}

fn large_body_response(status: &str) -> String {
    let mut response = String::from(status);
    for idx in 0..2048 {
        response.push_str(&format!("line-{idx:04} {}\r\n", "x".repeat(96)));
    }
    response.push_str(".\r\n");
    response
}

fn body_backend(port: u16) -> MockNntpServer {
    MockNntpServer::new(port)
        .with_name("BodyBackend")
        .on_command(
            "BODY <present@example.com>",
            "222 0 <present@example.com>\r\nBody by message-id\r\n.\r\n",
        )
        .on_command("BODY missing", "412 No newsgroup selected\r\n")
        .on_command("GROUP alt.test", "211 3 1 3 alt.test\r\n")
        .on_command("BODY 999", "423 No article with that number\r\n")
        .on_command(
            "BODY 2",
            large_body_response("222 2 <present@example.com>\r\n"),
        )
        .on_command("LAST", "223 1 <first@example.com>\r\n")
}

#[tokio::test]
async fn test_body_by_message_id_returns_multiline_in_per_command_mode() -> Result<()> {
    let mut client = spawn_client_with_backend(RoutingMode::PerCommand, body_backend(0)).await?;

    let (status, lines) =
        send_command_read_multiline_response(&mut client, "BODY <present@example.com>").await?;
    assert!(
        status.starts_with("222"),
        "RFC 3977 §6.2.3: BODY by message-id should return 222, got: {status:?}"
    );
    assert_eq!(lines, vec!["Body by message-id\r\n".to_string()]);

    Ok(())
}

#[tokio::test]
async fn test_body_by_number_streams_large_multiline_body_after_group() -> Result<()> {
    let mut client = spawn_client_with_backend(RoutingMode::Hybrid, body_backend(0)).await?;

    let response = send_command_read_line(&mut client, "GROUP alt.test").await?;
    assert!(
        response.starts_with("211"),
        "Expected GROUP to succeed before BODY by number, got: {response:?}"
    );

    let (status, lines) = send_command_read_multiline_response(&mut client, "BODY 2").await?;
    assert!(
        status.starts_with("222 2 "),
        "RFC 3977 §6.2.3: BODY by number should return 222, got: {status:?}"
    );
    assert_eq!(lines.len(), 2048, "Expected the full large BODY payload");
    let body = lines.concat();
    assert!(
        body.contains("line-0000") && body.contains("line-2047"),
        "Expected first and last streamed body lines to arrive intact"
    );
    assert!(
        body.len() > 200_000,
        "Expected a large streamed BODY payload, got {} bytes",
        body.len()
    );

    Ok(())
}

#[tokio::test]
async fn test_body_without_group_returns_412_in_stateful_mode() -> Result<()> {
    let mut client = spawn_client_with_backend(RoutingMode::Stateful, body_backend(0)).await?;

    let response = send_command_read_line(&mut client, "BODY missing").await?;
    assert!(
        response.starts_with("412"),
        "RFC 3977 §6.2.3: BODY without a selected group should return 412, got: {response:?}"
    );

    let response = send_command_read_line(&mut client, "GROUP alt.test").await?;
    assert!(
        response.starts_with("211"),
        "Session should remain usable after 412 BODY, got: {response:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_body_missing_article_number_returns_423_and_session_continues() -> Result<()> {
    let mut client = spawn_client_with_backend(RoutingMode::Hybrid, body_backend(0)).await?;

    let response = send_command_read_line(&mut client, "GROUP alt.test").await?;
    assert!(
        response.starts_with("211"),
        "Expected GROUP to succeed before BODY by number, got: {response:?}"
    );

    let response = send_command_read_line(&mut client, "BODY 999").await?;
    assert!(
        response.starts_with("423"),
        "RFC 3977 §6.2.3: missing BODY article numbers should return 423, got: {response:?}"
    );

    let response = send_command_read_line(&mut client, "LAST").await?;
    assert!(
        response.starts_with("223"),
        "A 423 BODY response should not terminate the selected-group session, got: {response:?}"
    );

    Ok(())
}
