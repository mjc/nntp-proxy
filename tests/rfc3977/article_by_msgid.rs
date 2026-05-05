//! ARTICLE by message-id tests (RFC 3977)
//!
//! Verify ARTICLE <message-id> behavior in per-command routing.

use anyhow::Result;

use crate::test_helpers::{MockNntpServer, RfcTestClient};
use nntp_proxy::RoutingMode;

fn article_backend(port: u16, message_id: &str) -> MockNntpServer {
    // Avoid double-wrapping angle brackets if the caller already provided them.
    let display = message_id.trim_matches(|c| c == '<' || c == '>');
    let resp_id = if message_id.starts_with('<') {
        message_id.to_string()
    } else {
        format!("<{display}>")
    };

    MockNntpServer::new(port)
        .with_name("ArticleBackend")
        .on_command(
            format!("ARTICLE {message_id}"),
            format!(
                "220 0 {}\r\nSubject: Test\r\n\r\nBody for {}\r\n.\r\n",
                resp_id, display
            ),
        )
}

#[tokio::test]
async fn test_article_by_message_id_in_per_command_mode() -> Result<()> {
    let msgid = "<msgid-123@example.com>";
    let mut client = RfcTestClient::spawn(RoutingMode::PerCommand, "article-backend", |port| {
        article_backend(port, msgid)
    })
    .await?;

    let concatenated = client.expect_article(msgid, "220").await?.concat();
    assert!(
        concatenated.contains("Body for"),
        "ARTICLE body should be returned in per-command mode, got: {concatenated:?}"
    );
    Ok(())
}
