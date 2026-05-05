//! LIST variants tests (NEWSGROUPS/LISTGROUP)

use anyhow::Result;
use nntp_proxy::RoutingMode;

use crate::test_helpers::{MockNntpServer, RfcTestClient};

#[tokio::test]
async fn test_list_newsgroups_multiline() -> Result<()> {
    let mut client = RfcTestClient::spawn(RoutingMode::PerCommand, "list-backend", |port| {
        MockNntpServer::new(port)
            .with_name("ListBackend")
            .on_command(
                "LIST NEWSGROUPS",
                "215 list follows\r\nalt.test 100 1 y\r\n.\r\n",
            )
    })
    .await?;

    let lines = client.expect_multiline("LIST NEWSGROUPS", "215").await?;
    assert!(
        lines.iter().any(|l| l.contains("alt.test")),
        "Expected alt.test in LIST output"
    );

    Ok(())
}

#[tokio::test]
async fn test_list_active_multiline() -> Result<()> {
    let mut client = RfcTestClient::spawn(RoutingMode::PerCommand, "list-active-backend", |port| {
        MockNntpServer::new(port)
            .with_name("ListActiveBackend")
            .on_command("LIST ACTIVE", "215 list follows\r\nalt.test 2 1 y\r\n.\r\n")
    })
    .await?;

    let lines = client.expect_multiline("LIST ACTIVE", "215").await?;
    assert_eq!(lines, vec!["alt.test 2 1 y\r\n".to_string()]);

    Ok(())
}

#[tokio::test]
async fn test_list_distributions_multiline() -> Result<()> {
    let mut client = RfcTestClient::spawn(
        RoutingMode::PerCommand,
        "list-distributions-backend",
        |port| {
            MockNntpServer::new(port)
                .with_name("ListDistributionsBackend")
                .on_command(
                    "LIST DISTRIBUTIONS",
                    "215 list follows\r\nworld Global distribution\r\n.\r\n",
                )
        },
    )
    .await?;

    let lines = client.expect_multiline("LIST DISTRIBUTIONS", "215").await?;
    assert_eq!(lines, vec!["world Global distribution\r\n".to_string()]);

    Ok(())
}

#[tokio::test]
async fn test_listgroup_multiline_uses_211_and_preserves_body() -> Result<()> {
    let mut client = RfcTestClient::spawn(RoutingMode::Stateful, "listgroup-backend", |port| {
        MockNntpServer::new(port)
            .with_name("ListgroupBackend")
            .on_command("LISTGROUP", "211 2 1 2 alt.test\r\n1\r\n2\r\n.\r\n")
    })
    .await?;

    let lines = client.expect_multiline("LISTGROUP", "211").await?;
    assert_eq!(lines, vec!["1\r\n".to_string(), "2\r\n".to_string()]);

    Ok(())
}
