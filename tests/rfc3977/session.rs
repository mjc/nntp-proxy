//! Integration tests for session functionality
//!
//! Tests authentication, QUIT handling, and message extraction through the public API

use anyhow::Result;

use crate::test_helpers::{
    MockNntpServer, connect_and_read_greeting, send_command_read_line, spawn_single_backend_proxy,
    spawn_single_backend_proxy_with_auth,
};
use nntp_proxy::auth::AuthHandler;
use nntp_proxy::config::RoutingMode;

#[tokio::test]
async fn test_quit_command_integration() -> Result<()> {
    let (proxy_port, _backend) =
        spawn_single_backend_proxy(RoutingMode::Hybrid, "backend", |port| {
            MockNntpServer::new(port)
                .with_name("Backend")
                .on_command("QUIT", "205 Goodbye\r\n")
        })
        .await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;
    let response = send_command_read_line(&mut client, "QUIT").await?;
    assert!(
        response.starts_with("205"),
        "Expected goodbye response, got: {response}"
    );

    Ok(())
}

#[tokio::test]
async fn test_auth_command_integration() -> Result<()> {
    let (proxy_port, _backend) = spawn_single_backend_proxy_with_auth(
        RoutingMode::Hybrid,
        "backend",
        "testuser",
        "testpass",
        |port| {
            MockNntpServer::new(port)
                .with_name("Backend")
                .on_command("DATE", "111 20260505155900\r\n")
        },
    )
    .await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;
    assert!(
        send_command_read_line(&mut client, "DATE")
            .await?
            .starts_with("480")
    );
    assert!(
        send_command_read_line(&mut client, "AUTHINFO USER testuser")
            .await?
            .starts_with("381")
    );
    assert!(
        send_command_read_line(&mut client, "AUTHINFO PASS testpass")
            .await?
            .starts_with("281")
    );
    assert!(
        send_command_read_line(&mut client, "DATE")
            .await?
            .starts_with("111")
    );

    Ok(())
}

#[tokio::test]
async fn test_auth_handler_validate() -> Result<()> {
    let handler = AuthHandler::new(Some("user".to_string()), Some("pass".to_string()))?;

    assert!(handler.is_enabled());
    assert!(handler.validate_credentials("user", "pass"));
    assert!(!handler.validate_credentials("user", "wrong"));
    assert!(!handler.validate_credentials("wrong", "pass"));

    Ok(())
}

#[tokio::test]
async fn test_auth_handler_disabled() -> Result<()> {
    let handler = AuthHandler::new(None, None)?;

    assert!(!handler.is_enabled());

    Ok(())
}

#[tokio::test]
async fn test_concurrent_auth_sessions() -> Result<()> {
    let (proxy_port, _backend) = spawn_single_backend_proxy_with_auth(
        RoutingMode::Hybrid,
        "backend",
        "user",
        "pass",
        |port| {
            MockNntpServer::new(port)
                .with_name("Backend")
                .on_command("DATE", "111 20260505155900\r\n")
        },
    )
    .await?;

    let mut client1 = connect_and_read_greeting(proxy_port).await?;
    let mut client2 = connect_and_read_greeting(proxy_port).await?;

    assert!(
        send_command_read_line(&mut client1, "AUTHINFO USER user")
            .await?
            .starts_with("381")
    );
    assert!(
        send_command_read_line(&mut client2, "AUTHINFO USER user")
            .await?
            .starts_with("381")
    );
    assert!(
        send_command_read_line(&mut client1, "AUTHINFO PASS pass")
            .await?
            .starts_with("281")
    );
    assert!(
        send_command_read_line(&mut client2, "AUTHINFO PASS pass")
            .await?
            .starts_with("281")
    );
    assert!(
        send_command_read_line(&mut client1, "DATE")
            .await?
            .starts_with("111")
    );
    assert!(
        send_command_read_line(&mut client2, "DATE")
            .await?
            .starts_with("111")
    );

    Ok(())
}
