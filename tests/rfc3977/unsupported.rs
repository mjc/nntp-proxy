//! RFC 3977 unsupported-command behavior tests.
//!
//! These end-to-end checks verify that commands the proxy intentionally does not
//! support are rejected with the proxy's documented public semantics while the
//! client session stays usable for subsequent commands.

use anyhow::Result;
use tokio::net::TcpListener;

use crate::test_helpers::{
    MockNntpServer, connect_and_read_greeting, create_test_config, send_command_read_line,
    spawn_proxy_with_config,
};
use nntp_proxy::RoutingMode;

async fn spawn_reader_client(mode: RoutingMode) -> Result<tokio::net::TcpStream> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let _backend = MockNntpServer::new(backend_port)
        .with_name("ReaderBackend")
        .on_command("LIST", "215 list follows\r\n.\r\n")
        .spawn_on_listener(backend_listener);

    let config = create_test_config(vec![(backend_port, "reader-backend")]);
    let proxy_port = spawn_proxy_with_config(config, mode).await?;
    connect_and_read_greeting(proxy_port).await
}

async fn assert_command_rejected_but_session_continues(
    mode: RoutingMode,
    command: &str,
    expected_status: &str,
) -> Result<()> {
    let mut client = spawn_reader_client(mode).await?;

    let response = send_command_read_line(&mut client, command).await?;
    assert!(
        response.starts_with(expected_status),
        "Expected {expected_status} for {command:?} in {mode:?}, got: {response:?}"
    );

    let response = send_command_read_line(&mut client, "LIST").await?;
    assert!(
        response.starts_with("215"),
        "{command:?} rejection should not terminate the session in {mode:?}, got: {response:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_post_rejected_with_440_and_session_continues_in_per_command_mode() -> Result<()> {
    assert_command_rejected_but_session_continues(RoutingMode::PerCommand, "POST", "440").await
}

#[tokio::test]
async fn test_post_rejected_with_440_and_session_continues_in_stateful_mode() -> Result<()> {
    assert_command_rejected_but_session_continues(RoutingMode::Stateful, "POST", "440").await
}

#[tokio::test]
async fn test_ihave_rejected_with_503_and_session_continues_in_per_command_mode() -> Result<()> {
    assert_command_rejected_but_session_continues(
        RoutingMode::PerCommand,
        "IHAVE <msgid@example>",
        "503",
    )
    .await
}

#[tokio::test]
async fn test_ihave_rejected_with_503_and_session_continues_in_stateful_mode() -> Result<()> {
    assert_command_rejected_but_session_continues(
        RoutingMode::Stateful,
        "IHAVE <msgid@example>",
        "503",
    )
    .await
}
