//! Unsupported-command behavior tests for RFC 3977 and exposed NNTP extensions.
//!
//! These end-to-end checks verify that commands the proxy intentionally does not
//! support are rejected with the proxy's documented public semantics while the
//! client session stays usable for subsequent commands.

use anyhow::Result;

use crate::test_helpers::{MockNntpServer, RfcTestClient};
use nntp_proxy::RoutingMode;

async fn assert_command_rejected_but_session_continues(
    mode: RoutingMode,
    command: &str,
    expected_status: &str,
) -> Result<()> {
    let mut client = RfcTestClient::spawn(mode, "reader-backend", |port| {
        MockNntpServer::new(port)
            .with_name("ReaderBackend")
            .on_command("LIST", "215 list follows\r\n.\r\n")
    })
    .await?;

    client.expect_status(command, expected_status).await?;
    client.expect_status("LIST", "215").await?;

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

#[tokio::test]
async fn test_check_rejected_with_503_and_session_continues_in_per_command_mode() -> Result<()> {
    assert_command_rejected_but_session_continues(
        RoutingMode::PerCommand,
        "CHECK <msgid@example>",
        "503",
    )
    .await
}

#[tokio::test]
async fn test_check_rejected_with_503_and_session_continues_in_stateful_mode() -> Result<()> {
    assert_command_rejected_but_session_continues(
        RoutingMode::Stateful,
        "CHECK <msgid@example>",
        "503",
    )
    .await
}

#[tokio::test]
async fn test_takethis_rejected_with_503_and_session_continues_in_per_command_mode() -> Result<()> {
    assert_command_rejected_but_session_continues(
        RoutingMode::PerCommand,
        "TAKETHIS <msgid@example>",
        "503",
    )
    .await
}

#[tokio::test]
async fn test_takethis_rejected_with_503_and_session_continues_in_stateful_mode() -> Result<()> {
    assert_command_rejected_but_session_continues(
        RoutingMode::Stateful,
        "TAKETHIS <msgid@example>",
        "503",
    )
    .await
}

#[tokio::test]
async fn test_starttls_rejected_with_503_and_session_continues_in_per_command_mode() -> Result<()> {
    assert_command_rejected_but_session_continues(RoutingMode::PerCommand, "STARTTLS", "503").await
}

#[tokio::test]
async fn test_starttls_rejected_with_503_and_session_continues_in_stateful_mode() -> Result<()> {
    assert_command_rejected_but_session_continues(RoutingMode::Stateful, "STARTTLS", "503").await
}
