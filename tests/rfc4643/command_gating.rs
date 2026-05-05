//! RFC 4643 command-gating integration tests.
//!
//! These tests verify end-to-end authentication gating on ordinary commands:
//! unauthenticated sessions must receive 480 responses, and successful
//! authentication must unlock the same command flow across routing modes.

use anyhow::Result;
use tokio::net::{TcpListener, TcpStream};

use crate::test_helpers::{
    MockNntpServer, connect_and_read_greeting, create_test_config_with_auth,
    send_command_read_line, spawn_proxy_with_config,
};
use nntp_proxy::RoutingMode;

const USERNAME: &str = "alice";
const PASSWORD: &str = "wonderland";

async fn spawn_auth_gating_client(mode: RoutingMode) -> Result<TcpStream> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let _backend = MockNntpServer::new(backend_port)
        .with_name("AuthGatingBackend")
        .on_command("DATE", "111 20260504112233\r\n")
        .spawn_on_listener(backend_listener);

    let config = create_test_config_with_auth(vec![backend_port], USERNAME, PASSWORD);
    let proxy_port = spawn_proxy_with_config(config, mode).await?;
    connect_and_read_greeting(proxy_port).await
}

async fn authenticate(stream: &mut TcpStream) -> Result<()> {
    for (command, expected) in [
        (format!("AUTHINFO USER {USERNAME}"), "381"),
        (format!("AUTHINFO PASS {PASSWORD}"), "281"),
    ] {
        let response = send_command_read_line(stream, &command).await?;
        assert!(
            response.starts_with(expected),
            "Expected {expected} during authentication for {command:?}, got: {response:?}"
        );
    }

    Ok(())
}

#[tokio::test]
async fn test_unauthenticated_date_is_gated_until_authentication_across_modes() -> Result<()> {
    for mode in [
        RoutingMode::PerCommand,
        RoutingMode::Hybrid,
        RoutingMode::Stateful,
    ] {
        let mut client = spawn_auth_gating_client(mode).await?;

        let response = send_command_read_line(&mut client, "DATE").await?;
        assert!(
            response.starts_with("480"),
            "RFC 4643 §2.2: DATE should be gated before auth in {mode:?}, got: {response:?}"
        );

        authenticate(&mut client).await?;

        let response = send_command_read_line(&mut client, "DATE").await?;
        assert!(
            response.starts_with("111"),
            "DATE should succeed after auth in {mode:?}, got: {response:?}"
        );
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

        let response =
            send_command_read_line(&mut client, &format!("AUTHINFO USER {USERNAME}")).await?;
        assert!(
            response.starts_with("381"),
            "Expected 381 after AUTHINFO USER in {mode:?}, got: {response:?}"
        );

        let response = send_command_read_line(&mut client, "DATE").await?;
        assert!(
            response.starts_with("480"),
            "RFC 4643 §2.2: DATE should stay gated before PASS in {mode:?}, got: {response:?}"
        );

        let response =
            send_command_read_line(&mut client, &format!("AUTHINFO PASS {PASSWORD}")).await?;
        assert!(
            response.starts_with("281"),
            "Stored AUTHINFO USER state should survive the gated command in {mode:?}, got: {response:?}"
        );

        let response = send_command_read_line(&mut client, "DATE").await?;
        assert!(
            response.starts_with("111"),
            "DATE should succeed after delayed PASS in {mode:?}, got: {response:?}"
        );
    }

    Ok(())
}
