//! XOVER terminator and pipelining edge-case tests
//!
//! Ensure multiline XOVER responses are handled correctly and terminators respected.

use anyhow::Result;
use nntp_proxy::RoutingMode;

use crate::test_helpers::{MockNntpServer, RfcTestClient};

#[tokio::test]
async fn test_xover_multiline_terminator() -> Result<()> {
    let mut client = RfcTestClient::spawn(RoutingMode::Stateful, "xover-backend", |port| {
        MockNntpServer::new(port)
            .with_name("XoverBackend")
            .on_command("GROUP", "211 100 1 100 alt.test\r\n")
            .on_command(
                "XOVER",
                "224 1 Overview follows\r\n1\t<msg1@example.com>\tSubject1\r\n.\r\n",
            )
    })
    .await?;

    client.expect_status("GROUP alt.test", "211").await?;
    let lines = client.expect_multiline("XOVER", "224").await?;
    assert!(
        lines.iter().any(|l| l.contains("<msg1@example.com>")),
        "Expected overview line with msgid"
    );

    Ok(())
}
