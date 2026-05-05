//! HEAD and STAT workflow tests for RFC 3977 §6.2.
//!
//! These cover the end-to-end semantics that parser-only checks miss:
//! message-id retrieval in per-command mode, and selected-group error behavior
//! for article-number requests in stateful/hybrid sessions.

use anyhow::Result;
use nntp_proxy::RoutingMode;

use crate::test_helpers::{MockNntpServer, RfcTestClient};

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
    let mut client = RfcTestClient::spawn(
        RoutingMode::PerCommand,
        "head-stat-backend",
        head_stat_backend,
    )
    .await?;

    let lines = client
        .expect_multiline("HEAD <present@example.com>", "221")
        .await?;
    let headers = lines.concat();
    assert!(headers.contains("Subject: Present"));
    assert!(headers.contains("From: tester@example.com"));

    Ok(())
}

#[tokio::test]
async fn test_stat_by_message_id_returns_223_without_body_in_per_command_mode() -> Result<()> {
    let mut client = RfcTestClient::spawn(
        RoutingMode::PerCommand,
        "head-stat-backend",
        head_stat_backend,
    )
    .await?;
    client
        .expect_status("STAT <present@example.com>", "223")
        .await?;

    Ok(())
}

#[tokio::test]
async fn test_stat_without_group_returns_412_in_stateful_mode() -> Result<()> {
    let mut client = RfcTestClient::spawn(
        RoutingMode::Stateful,
        "head-stat-backend",
        head_stat_backend,
    )
    .await?;
    client.expect_status("STAT missing", "412").await?;

    Ok(())
}

#[tokio::test]
async fn test_head_and_stat_missing_article_numbers_return_423_and_session_continues() -> Result<()>
{
    let mut client =
        RfcTestClient::spawn(RoutingMode::Hybrid, "head-stat-backend", head_stat_backend).await?;
    client.expect_status("GROUP alt.test", "211").await?;
    client.expect_status("HEAD 999", "423").await?;
    client.expect_status("STAT 999", "423").await?;
    client.expect_status("LAST", "223").await?;

    Ok(())
}
