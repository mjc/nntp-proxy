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
    let mut client = RfcTestClient::spawn(mode, "reader-backend", |_port| {
        MockNntpServer::new()
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

/// A backend that accepts `XFEATURE COMPRESS GZIP` would switch overview to a
/// zlib stream the proxy cannot frame, so the proxy must answer it locally.
fn gzip_overview_backend(_port: u16) -> MockNntpServer {
    MockNntpServer::new()
        .with_name("GzipOverviewBackend")
        .on_command("XFEATURE", "290 Compression enabled\r\n")
        .on_command("GROUP", "211 1 1 1 alt.test\r\n")
        .on_command("LIST", "215 list follows\r\n.\r\n")
        .on_command(
            "XOVER",
            "224 Overview follows\r\n1\tSubject\tAuthor\t11 Oct 2026 10:00:00 GMT\t<one@example.com>\t\t123\t1\r\n.\r\n",
        )
}

#[tokio::test]
async fn test_xfeature_compress_gzip_rejected_with_503_in_every_routing_mode() -> Result<()> {
    for mode in [
        RoutingMode::PerCommand,
        RoutingMode::Hybrid,
        RoutingMode::Stateful,
    ] {
        let mut client =
            RfcTestClient::spawn(mode, "gzip-overview-backend", gzip_overview_backend).await?;

        for command in [
            "XFEATURE COMPRESS GZIP",
            "XFEATURE COMPRESS GZIP TERMINATOR",
        ] {
            client.expect_status(command, "503").await?;
        }

        if mode == RoutingMode::PerCommand {
            client.expect_multiline("LIST", "215").await?;
        } else {
            // Rejection must also hold once the session owns a stateful backend.
            client.expect_status("GROUP alt.test", "211").await?;
            client
                .expect_status("xfeature compress gzip terminator", "503")
                .await?;
            let overview = client.expect_multiline("XOVER 1-1", "224").await?;
            assert_eq!(
                overview,
                vec![
                    "1\tSubject\tAuthor\t11 Oct 2026 10:00:00 GMT\t<one@example.com>\t\t123\t1\r\n"
                        .to_string()
                ],
                "{mode:?}: overview must stay uncompressed"
            );
        }
    }

    Ok(())
}
