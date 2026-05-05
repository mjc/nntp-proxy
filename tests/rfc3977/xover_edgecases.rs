//! XOVER edge-case tests for RFC 3977.
//!
//! Verify XOVER behavior in stateful and hybrid modes when a group is selected.

use anyhow::Result;

use crate::test_helpers::{MockNntpServer, RfcTestClient};
use nntp_proxy::RoutingMode;

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
    let mut client =
        RfcTestClient::spawn(RoutingMode::Stateful, "xover-backend", xover_backend).await?;

    client.expect_status("GROUP alt.test", "211").await?;
    let lines = client.expect_multiline("XOVER", "224").await?;
    let concatenated = lines.concat();
    assert!(
        concatenated.contains("<msg1@example.com>"),
        "XOVER body should include message-id, got: {lines:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_xover_returns_multiline_in_hybrid_after_group() -> Result<()> {
    let mut client =
        RfcTestClient::spawn(RoutingMode::Hybrid, "xover-backend", xover_backend).await?;

    client.expect_status("GROUP alt.test", "211").await?;
    let lines = client.expect_multiline("XOVER", "224").await?;
    let concatenated = lines.concat();
    assert!(
        concatenated.contains("<msg1@example.com>"),
        "XOVER body should include message-id, got: {lines:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_xover_without_group_returns_412_and_session_continues() -> Result<()> {
    let mut client = RfcTestClient::spawn(
        RoutingMode::Stateful,
        "xover-backend",
        no_group_xover_backend,
    )
    .await?;

    client.expect_status("XOVER", "412").await?;
    client.expect_status("GROUP alt.test", "211").await?;

    Ok(())
}

#[tokio::test]
async fn test_xover_invalid_and_empty_ranges_behave_as_expected() -> Result<()> {
    let mut client =
        RfcTestClient::spawn(RoutingMode::Hybrid, "xover-backend", xover_backend).await?;

    client.expect_status("GROUP alt.test", "211").await?;
    client.expect_status("XOVER 1-0", "501").await?;
    assert!(
        client
            .expect_multiline("XOVER 999-1000", "224")
            .await?
            .is_empty()
    );

    Ok(())
}
