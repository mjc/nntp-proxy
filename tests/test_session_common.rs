//! Integration tests for session functionality
//!
//! Tests authentication, QUIT handling, and message extraction through the public API

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{Duration, timeout};

mod config_helpers;
mod test_helpers;

use config_helpers::create_test_server_config;
use nntp_proxy::NntpProxy;
use nntp_proxy::auth::AuthHandler;
use nntp_proxy::config::{ClientAuth, Config, RoutingMode};
use test_helpers::MockNntpServer;

#[tokio::test]
async fn test_quit_command_integration() -> Result<()> {
    // Setup proxy
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    drop(backend_listener);

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await?;
    let proxy_port = proxy_listener.local_addr()?.port();

    let _mock = MockNntpServer::new(backend_port)
        .with_name("Backend")
        .on_command("QUIT", "205 Goodbye\r\n")
        .spawn();

    tokio::time::sleep(Duration::from_millis(100)).await;

    let config = Config {
        servers: vec![create_test_server_config(
            "127.0.0.1",
            backend_port,
            "backend",
        )],
        ..Default::default()
    };

    let proxy = NntpProxy::new(config, RoutingMode::Hybrid)?;

    tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = proxy_listener.accept().await {
                let proxy_clone = proxy.clone();
                tokio::spawn(async move {
                    let _ = proxy_clone.handle_client(stream, addr.into()).await;
                });
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connect and test QUIT
    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;

    let mut buffer = vec![0u8; 1024];
    let _ = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;

    // Send QUIT
    client.write_all(b"QUIT\r\n").await?;
    let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
    let response = String::from_utf8_lossy(&buffer[..n]);

    // Should get goodbye response
    assert!(
        response.contains("205") || response.contains("Goodbye"),
        "Expected goodbye response, got: {}",
        response
    );

    Ok(())
}

#[tokio::test]
async fn test_auth_command_integration() -> Result<()> {
    // Setup proxy with auth
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    drop(backend_listener);

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await?;
    let proxy_port = proxy_listener.local_addr()?.port();

    let _mock = MockNntpServer::new(backend_port)
        .with_name("Backend")
        .on_command("HELP", "100 Help\r\n")
        .spawn();

    tokio::time::sleep(Duration::from_millis(100)).await;

    let config = Config {
        servers: vec![create_test_server_config(
            "127.0.0.1",
            backend_port,
            "backend",
        )],
        client_auth: ClientAuth {
            users: vec![],
            username: Some("testuser".to_string()),
            password: Some("testpass".to_string()),
            greeting: None,
        },
        ..Default::default()
    };

    let proxy = NntpProxy::new(config, RoutingMode::Hybrid)?;

    tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = proxy_listener.accept().await {
                let proxy_clone = proxy.clone();
                tokio::spawn(async move {
                    let _ = proxy_clone.handle_client(stream, addr.into()).await;
                });
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connect and try to send command without auth
    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;

    let mut buffer = vec![0u8; 1024];
    let _ = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;

    // Try HELP without auth - should get 480
    client.write_all(b"HELP\r\n").await?;
    let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
    let response = String::from_utf8_lossy(&buffer[..n]);
    assert!(
        response.starts_with("480"),
        "Expected auth required, got: {}",
        response
    );

    // Authenticate
    client.write_all(b"AUTHINFO USER testuser\r\n").await?;
    let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
    let response = String::from_utf8_lossy(&buffer[..n]);
    assert!(
        response.starts_with("381"),
        "Expected password required, got: {}",
        response
    );

    client.write_all(b"AUTHINFO PASS testpass\r\n").await?;
    let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
    let response = String::from_utf8_lossy(&buffer[..n]);
    assert!(
        response.starts_with("281"),
        "Expected auth success, got: {}",
        response
    );

    // Now HELP should work
    client.write_all(b"HELP\r\n").await?;
    let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
    let response = String::from_utf8_lossy(&buffer[..n]);
    assert!(
        response.contains("100"),
        "Expected help response, got: {}",
        response
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
    // Setup proxy with auth
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    drop(backend_listener);

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await?;
    let proxy_port = proxy_listener.local_addr()?.port();

    let _mock = MockNntpServer::new(backend_port)
        .with_name("Backend")
        .on_command("HELP", "100 Help\r\n")
        .spawn();

    tokio::time::sleep(Duration::from_millis(100)).await;

    let config = Config {
        servers: vec![create_test_server_config(
            "127.0.0.1",
            backend_port,
            "backend",
        )],
        client_auth: ClientAuth {
            users: vec![],
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            greeting: None,
        },
        ..Default::default()
    };

    let proxy = NntpProxy::new(config, RoutingMode::Hybrid)?;

    tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = proxy_listener.accept().await {
                let proxy_clone = proxy.clone();
                tokio::spawn(async move {
                    let _ = proxy_clone.handle_client(stream, addr.into()).await;
                });
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connect two clients and auth both
    let mut client1 = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;
    let mut client2 = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;

    let mut buffer1 = vec![0u8; 1024];
    let mut buffer2 = vec![0u8; 1024];

    // Read greetings
    let _ = timeout(Duration::from_secs(1), client1.read(&mut buffer1)).await??;
    let _ = timeout(Duration::from_secs(1), client2.read(&mut buffer2)).await??;

    // Auth both clients
    client1.write_all(b"AUTHINFO USER user\r\n").await?;
    client2.write_all(b"AUTHINFO USER user\r\n").await?;

    let _ = timeout(Duration::from_secs(1), client1.read(&mut buffer1)).await??;
    let _ = timeout(Duration::from_secs(1), client2.read(&mut buffer2)).await??;

    client1.write_all(b"AUTHINFO PASS pass\r\n").await?;
    client2.write_all(b"AUTHINFO PASS pass\r\n").await?;

    let n1 = timeout(Duration::from_secs(1), client1.read(&mut buffer1)).await??;
    let n2 = timeout(Duration::from_secs(1), client2.read(&mut buffer2)).await??;

    assert!(n1 > 0);
    assert!(n2 > 0);

    // Both should be authenticated
    client1.write_all(b"HELP\r\n").await?;
    client2.write_all(b"HELP\r\n").await?;

    let n1 = timeout(Duration::from_secs(1), client1.read(&mut buffer1)).await??;
    let n2 = timeout(Duration::from_secs(1), client2.read(&mut buffer2)).await??;

    assert!(n1 > 0);
    assert!(n2 > 0);

    Ok(())
}
