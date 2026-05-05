//! RFC 4643 command-gating integration tests.
//!
//! These tests verify end-to-end authentication gating on ordinary commands:
//! unauthenticated sessions must receive 480 responses, and successful
//! authentication must unlock the same command flow across routing modes.

use anyhow::Result;

use crate::test_helpers::{MockNntpServer, RfcTestClient};
use nntp_proxy::RoutingMode;

const USERNAME: &str = "alice";
const PASSWORD: &str = "wonderland";

async fn spawn_auth_gating_client(mode: RoutingMode) -> Result<RfcTestClient> {
    RfcTestClient::spawn_with_auth(mode, "auth-gating-backend", USERNAME, PASSWORD, |port| {
        MockNntpServer::new(port)
            .with_name("AuthGatingBackend")
            .on_command("DATE", "111 20260504112233\r\n")
    })
    .await
}

#[tokio::test]
async fn test_unauthenticated_date_is_gated_until_authentication_across_modes() -> Result<()> {
    for mode in [
        RoutingMode::PerCommand,
        RoutingMode::Hybrid,
        RoutingMode::Stateful,
    ] {
        let mut client = spawn_auth_gating_client(mode).await?;

        client.expect_status("DATE", "480").await?;

        client.authenticate(USERNAME, PASSWORD).await?;
        client.expect_status("DATE", "111").await?;
    }

    Ok(())
}

#[tokio::test]
async fn test_auth_username_survives_intervening_gated_command() -> Result<()> {
    for mode in [
        RoutingMode::PerCommand,
        RoutingMode::Hybrid,
        RoutingMode::Stateful,
    ] {
        let mut client = spawn_auth_gating_client(mode).await?;

        client
            .expect_status(&format!("AUTHINFO USER {USERNAME}"), "381")
            .await?;
        client.expect_status("DATE", "480").await?;
        client
            .expect_status(&format!("AUTHINFO PASS {PASSWORD}"), "281")
            .await?;
        client.expect_status("DATE", "111").await?;
    }

    Ok(())
}
