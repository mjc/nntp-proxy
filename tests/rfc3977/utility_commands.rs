//! RFC 3977 utility command tests (`HELP`, bare `LIST`).

use anyhow::Result;

use crate::test_helpers::{MockNntpServer, RfcTestClient};
use nntp_proxy::RoutingMode;

fn utility_backend(port: u16) -> MockNntpServer {
    MockNntpServer::new(port)
        .with_name("UtilityBackend")
        .on_command(
            "HELP",
            "100 Help text follows\r\nThis server supports NNTP basics\r\n.\r\n",
        )
        .on_command(
            "LIST",
            "215 list follows\r\nalt.test 2 1 y\r\nalt.misc 5 1 y\r\n.\r\n",
        )
        .on_command("DATE", "111 20260505163500\r\n")
}

#[tokio::test]
async fn test_help_returns_multiline_and_session_continues() -> Result<()> {
    let mut client =
        RfcTestClient::spawn(RoutingMode::PerCommand, "utility-backend", utility_backend).await?;

    let lines = client.expect_multiline("HELP", "100").await?;
    assert_eq!(
        lines,
        vec!["This server supports NNTP basics\r\n".to_string()]
    );

    client.expect_status("DATE", "111").await?;

    Ok(())
}

#[tokio::test]
async fn test_list_returns_multiline_group_listing() -> Result<()> {
    let mut client =
        RfcTestClient::spawn(RoutingMode::Hybrid, "utility-backend", utility_backend).await?;

    let lines = client.expect_multiline("LIST", "215").await?;
    assert_eq!(
        lines,
        vec![
            "alt.test 2 1 y\r\n".to_string(),
            "alt.misc 5 1 y\r\n".to_string()
        ]
    );

    client.expect_status("DATE", "111").await?;

    Ok(())
}
