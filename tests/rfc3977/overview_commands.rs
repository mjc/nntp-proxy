//! RFC 3977 overview/header command tests (`OVER`, `HDR`, `XHDR`).
//!
//! These commands depend on selected-group reader state and return multiline
//! responses that must preserve framing and ordering.

use anyhow::Result;

use crate::test_helpers::{MockNntpServer, RfcTestClient};
use nntp_proxy::RoutingMode;

fn overview_backend(_port: u16) -> MockNntpServer {
    MockNntpServer::new()
        .with_name("OverviewBackend")
        .on_command("GROUP", "211 2 1 2 alt.test\r\n")
        .on_command(
            "OVER",
            "224 Overview follows\r\n1\tRelease.Title.MixedCase-GROUP [001/010]\tAuthor One\t11 Oct 2026 10:00:00 GMT\t<CaseSensitive@example.com>\t\t123\t9\tXref: news alt.test:1\r\n2\tSecond.Complete.Subject-GROUP\tAuthor Two\t11 Oct 2026 10:01:00 GMT\t<Second@example.com>\t<Parent@example.com> <Root@example.com>\t456\t12\tXref: news alt.test:2 alt.other:99\r\n.\r\n",
        )
        .on_command(
            "XOVER",
            "224 Overview follows\r\n1\tRelease.Title.MixedCase-GROUP [001/010]\tAuthor One\t11 Oct 2026 10:00:00 GMT\t<CaseSensitive@example.com>\t\t123\t9\tXref: news alt.test:1\r\n2\tSecond.Complete.Subject-GROUP\tAuthor Two\t11 Oct 2026 10:01:00 GMT\t<Second@example.com>\t<Parent@example.com> <Root@example.com>\t456\t12\tXref: news alt.test:2 alt.other:99\r\n.\r\n",
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

fn no_group_over_backend(_port: u16) -> MockNntpServer {
    MockNntpServer::new()
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
    assert_eq!(lines, expected_overview_lines());

    Ok(())
}

fn expected_overview_lines() -> Vec<String> {
    vec![
        "1\tRelease.Title.MixedCase-GROUP [001/010]\tAuthor One\t11 Oct 2026 10:00:00 GMT\t<CaseSensitive@example.com>\t\t123\t9\tXref: news alt.test:1\r\n".to_owned(),
        "2\tSecond.Complete.Subject-GROUP\tAuthor Two\t11 Oct 2026 10:01:00 GMT\t<Second@example.com>\t<Parent@example.com> <Root@example.com>\t456\t12\tXref: news alt.test:2 alt.other:99\r\n".to_owned(),
    ]
}

#[tokio::test]
async fn test_over_and_xover_preserve_matching_metadata_in_hybrid() -> Result<()> {
    let mut client =
        RfcTestClient::spawn(RoutingMode::Hybrid, "overview-backend", overview_backend).await?;
    client.expect_status("GROUP alt.test", "211").await?;
    for command in ["OVER 1-2", "XOVER 1-2"] {
        assert_eq!(
            client.expect_multiline(command, "224").await?,
            expected_overview_lines()
        );
    }
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
