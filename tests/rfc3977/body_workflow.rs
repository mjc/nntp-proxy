//! RFC 3977 BODY workflow tests.
//!
//! These cover end-to-end BODY semantics that differ between message-id and
//! selected-group article-number retrieval, including multiline framing for
//! large bodies and the expected 412/423 error paths.

use anyhow::Result;

use crate::test_helpers::{MockNntpServer, RfcTestClient};
use nntp_proxy::RoutingMode;

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
    let mut client =
        RfcTestClient::spawn(RoutingMode::PerCommand, "body-backend", body_backend).await?;

    let lines = client
        .expect_multiline("BODY <present@example.com>", "222")
        .await?;
    assert_eq!(lines, vec!["Body by message-id\r\n".to_string()]);

    Ok(())
}

#[tokio::test]
async fn test_body_by_number_streams_large_multiline_body_after_group() -> Result<()> {
    let mut client =
        RfcTestClient::spawn(RoutingMode::Hybrid, "body-backend", body_backend).await?;

    client.expect_status("GROUP alt.test", "211").await?;

    let lines = client.expect_multiline("BODY 2", "222 2 ").await?;
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
    let mut client =
        RfcTestClient::spawn(RoutingMode::Stateful, "body-backend", body_backend).await?;

    client.expect_status("BODY missing", "412").await?;
    client.expect_status("GROUP alt.test", "211").await?;

    Ok(())
}

#[tokio::test]
async fn test_body_missing_article_number_returns_423_and_session_continues() -> Result<()> {
    let mut client =
        RfcTestClient::spawn(RoutingMode::Hybrid, "body-backend", body_backend).await?;

    client.expect_status("GROUP alt.test", "211").await?;
    client.expect_status("BODY 999", "423").await?;
    client.expect_status("LAST", "223").await?;

    Ok(())
}
