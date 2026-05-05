//! RFC 3977 overview/header command tests (`OVER`, `HDR`, `XHDR`).
//!
//! These commands depend on selected-group reader state and return multiline
//! responses that must preserve framing and ordering.

use anyhow::Result;

use crate::test_helpers::{MockNntpServer, RfcTestClient};
use nntp_proxy::RoutingMode;

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
    let mut client =
        RfcTestClient::spawn(RoutingMode::Stateful, "overview-backend", overview_backend).await?;

    client.expect_status("GROUP alt.test", "211").await?;

    let lines = client.expect_multiline("OVER 1-2", "224").await?;
    let body = lines.concat();
    assert!(
        body.contains("<msg1@example.com>") && body.contains("Subject one"),
        "Expected overview data with message-id and subject, got: {body:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_hdr_and_xhdr_return_multiline_in_hybrid_after_group() -> Result<()> {
    let mut client =
        RfcTestClient::spawn(RoutingMode::Hybrid, "overview-backend", overview_backend).await?;

    client.expect_status("GROUP alt.test", "211").await?;

    let hdr_lines = client.expect_multiline("HDR Subject 1-2", "225").await?;
    assert_eq!(
        hdr_lines,
        vec![
            "1 Subject one\r\n".to_string(),
            "2 Subject two\r\n".to_string()
        ]
    );

    let xhdr_lines = client.expect_multiline("XHDR Subject 1-2", "225").await?;
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
    let mut client = RfcTestClient::spawn(
        RoutingMode::Stateful,
        "overview-backend",
        no_group_over_backend,
    )
    .await?;

    client.expect_status("OVER 1-2", "412").await?;
    client.expect_status("GROUP alt.test", "211").await?;

    Ok(())
}
