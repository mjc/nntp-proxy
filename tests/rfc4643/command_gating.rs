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
    RfcTestClient::spawn_with_auth(mode, "auth-gating-backend", USERNAME, PASSWORD, |_port| {
        MockNntpServer::new()
            .with_name("AuthGatingBackend")
            .on_command("DATE", "111 20260504112233\r\n")
            .on_command("GROUP alt.test", "211 100 1 100 alt.test\r\n")
            .on_command("XFOO", "200 extension ok\r\n")
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
async fn test_hybrid_stateful_command_is_gated_until_authentication() -> Result<()> {
    let mut client = spawn_auth_gating_client(RoutingMode::Hybrid).await?;

    client.expect_status("GROUP alt.test", "480").await?;

    client.authenticate(USERNAME, PASSWORD).await?;
    client.expect_status("GROUP alt.test", "211").await?;

    Ok(())
}

#[tokio::test]
async fn test_stateful_command_is_gated_until_authentication_in_stateful_mode() -> Result<()> {
    let mut client = spawn_auth_gating_client(RoutingMode::Stateful).await?;

    client.expect_status("GROUP alt.test", "480").await?;

    client.authenticate(USERNAME, PASSWORD).await?;
    client.expect_status("GROUP alt.test", "211").await?;

    Ok(())
}

#[tokio::test]
async fn test_hybrid_unknown_extension_is_gated_until_authentication() -> Result<()> {
    let mut client = spawn_auth_gating_client(RoutingMode::Hybrid).await?;

    client.expect_status("XFOO arg", "480").await?;

    client.authenticate(USERNAME, PASSWORD).await?;
    client.expect_status("XFOO arg", "200").await?;

    Ok(())
}

#[tokio::test]
async fn test_unsupported_commands_are_gated_before_policy_rejection() -> Result<()> {
    for mode in [
        RoutingMode::PerCommand,
        RoutingMode::Hybrid,
        RoutingMode::Stateful,
    ] {
        let mut client = spawn_auth_gating_client(mode).await?;

        client.expect_status("POST", "480").await?;
        client.expect_status("STARTTLS", "480").await?;

        client.authenticate(USERNAME, PASSWORD).await?;
        client.expect_status("POST", "440").await?;
        client.expect_status("STARTTLS", "503").await?;
    }

    Ok(())
}

#[tokio::test]
async fn test_auth_username_survives_intervening_hybrid_stateful_command() -> Result<()> {
    let mut client = spawn_auth_gating_client(RoutingMode::Hybrid).await?;

    client
        .expect_status(&format!("AUTHINFO USER {USERNAME}"), "381")
        .await?;
    client.expect_status("GROUP alt.test", "480").await?;
    client
        .expect_status(&format!("AUTHINFO PASS {PASSWORD}"), "281")
        .await?;
    client.expect_status("GROUP alt.test", "211").await?;

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
