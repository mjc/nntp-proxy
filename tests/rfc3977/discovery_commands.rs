//! RFC 3977 discovery command tests (`NEWGROUPS`, `NEWNEWS`).
//!
//! These commands are stateless and return multiline responses that should work
//! without switching the client into stateful reader mode.

use anyhow::Result;

use crate::test_helpers::{MockNntpServer, RfcTestClient};
use nntp_proxy::RoutingMode;

fn discovery_backend(port: u16) -> MockNntpServer {
    MockNntpServer::new(port)
        .with_name("DiscoveryBackend")
        .on_command("MODE READER", "200 Reader mode acknowledged\r\n")
        .on_command(
            "NEWGROUPS",
            "231 List of new newsgroups follows\r\nalt.new 0000000002 0000000001 y\r\n.\r\n",
        )
        .on_command(
            "NEWNEWS",
            "230 List of new articles follows\r\n<new-1@example.com>\r\n.\r\n",
        )
        .on_command("DATE", "111 20260505161700\r\n")
}

#[tokio::test]
async fn test_newgroups_returns_multiline_and_session_continues_in_per_command() -> Result<()> {
    let mut client = RfcTestClient::spawn(
        RoutingMode::PerCommand,
        "discovery-backend",
        discovery_backend,
    )
    .await?;

    let lines = client
        .expect_multiline("NEWGROUPS 20260505 000000 GMT", "231")
        .await?;
    assert_eq!(
        lines,
        vec!["alt.new 0000000002 0000000001 y\r\n".to_string()]
    );

    client.expect_status("DATE", "111").await?;

    Ok(())
}

#[tokio::test]
async fn test_newnews_returns_multiline_in_hybrid_without_stateful_switch() -> Result<()> {
    let mut client =
        RfcTestClient::spawn(RoutingMode::Hybrid, "discovery-backend", discovery_backend).await?;

    let lines = client
        .expect_multiline("NEWNEWS * 20260505 000000 GMT", "230")
        .await?;
    assert_eq!(lines, vec!["<new-1@example.com>\r\n".to_string()]);

    client.expect_status("DATE", "111").await?;

    Ok(())
}

#[tokio::test]
async fn test_mode_reader_is_forwarded_statelessly_and_session_continues() -> Result<()> {
    let mut client = RfcTestClient::spawn(
        RoutingMode::PerCommand,
        "discovery-backend",
        discovery_backend,
    )
    .await?;

    client.expect_status("MODE READER", "200").await?;
    client.expect_status("DATE", "111").await?;

    Ok(())
}
