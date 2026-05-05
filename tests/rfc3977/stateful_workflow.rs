//! RFC 3977 stateful reader workflow tests.
//!
//! These tests cover state-dependent reader semantics that parser-only checks
//! cannot validate: commands that require a selected group, and hybrid-mode
//! switching into a backend session that preserves current-group context.

use anyhow::Result;

use crate::test_helpers::{MockNntpServer, RfcTestClient};
use nntp_proxy::RoutingMode;

fn workflow_backend(port: u16) -> MockNntpServer {
    MockNntpServer::new(port)
        .with_name("WorkflowBackend")
        .on_command("GROUP", "211 3 1 3 alt.test\r\n")
        .on_command("NEXT", "223 2 <second@example.com>\r\n")
        .on_command("LAST", "223 1 <first@example.com>\r\n")
        .on_command(
            "ARTICLE 2",
            "220 2 <second@example.com>\r\nSubject: Second\r\n\r\nBody 2\r\n.\r\n",
        )
}

fn no_such_group_backend(port: u16) -> MockNntpServer {
    MockNntpServer::new(port)
        .with_name("MissingGroupBackend")
        .on_command("GROUP missing.group", "411 No such newsgroup\r\n")
        .on_command("DATE", "111 20260504112233\r\n")
}

fn missing_article_number_backend(port: u16) -> MockNntpServer {
    MockNntpServer::new(port)
        .with_name("MissingArticleBackend")
        .on_command("GROUP", "211 3 1 3 alt.test\r\n")
        .on_command("ARTICLE 999", "423 No article with that number\r\n")
        .on_command("LAST", "223 1 <first@example.com>\r\n")
}

#[tokio::test]
async fn test_next_without_group_returns_412_and_session_continues() -> Result<()> {
    let mut client = RfcTestClient::spawn(RoutingMode::Stateful, "workflow-backend", |port| {
        MockNntpServer::new(port)
            .with_name("NoGroupBackend")
            .on_command("NEXT", "412 No newsgroup selected\r\n")
            .on_command("GROUP", "211 3 1 3 alt.test\r\n")
    })
    .await?;

    client.expect_status("NEXT", "412").await?;
    client.expect_status("GROUP alt.test", "211").await?;

    Ok(())
}

#[tokio::test]
async fn test_hybrid_group_switch_preserves_next_last_and_article_number_workflow() -> Result<()> {
    let mut client =
        RfcTestClient::spawn(RoutingMode::Hybrid, "workflow-backend", workflow_backend).await?;

    client.expect_status("GROUP alt.test", "211").await?;
    client.expect_status("NEXT", "223").await?;
    client.expect_status("LAST", "223").await?;

    let body = client.expect_article("2", "220 2 ").await?;
    assert!(
        body.concat().contains("Body 2\r\n"),
        "Expected ARTICLE body from selected-group context, got: {body:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_missing_group_returns_411_and_session_continues() -> Result<()> {
    let mut client = RfcTestClient::spawn(
        RoutingMode::Stateful,
        "workflow-backend",
        no_such_group_backend,
    )
    .await?;

    client.expect_status("GROUP missing.group", "411").await?;
    client.expect_status("DATE", "111").await?;

    Ok(())
}

#[tokio::test]
async fn test_missing_article_number_returns_423_after_group_selection() -> Result<()> {
    let mut client = RfcTestClient::spawn(
        RoutingMode::Hybrid,
        "workflow-backend",
        missing_article_number_backend,
    )
    .await?;

    client.expect_status("GROUP alt.test", "211").await?;
    client.expect_status("ARTICLE 999", "423").await?;
    client.expect_status("LAST", "223").await?;

    Ok(())
}
