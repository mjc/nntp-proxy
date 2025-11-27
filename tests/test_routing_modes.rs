//! Integration tests for different routing modes
//!
//! Tests the three routing modes: Standard, PerCommand, and Hybrid
//! These tests exercise the session handler code paths that have low coverage.

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{Duration, timeout};

mod config_helpers;
mod test_helpers;

use config_helpers::create_test_server_config;
use nntp_proxy::{Config, NntpProxy, RoutingMode};
use test_helpers::MockNntpServer;

/// Helper to setup proxy with specific routing mode
async fn setup_proxy_with_mode(
    routing_mode: RoutingMode,
) -> Result<(u16, u16, tokio::task::AbortHandle)> {
    // Find available ports
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    drop(backend_listener);

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await?;
    let proxy_port = proxy_listener.local_addr()?.port();

    // Start mock backend
    let mock = MockNntpServer::new(backend_port)
        .with_name("Test Backend")
        .on_command("HELP", "100 Help text\r\n.\r\n")
        .on_command("DATE", "111 20251120120000\r\n")
        .on_command("LIST", "215 List follows\r\n.\r\n")
        .on_command("QUIT", "205 Goodbye\r\n")
        .on_command("GROUP alt.test", "211 100 1 100 alt.test\r\n")
        .spawn();

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create and start proxy
    let config = Config {
        servers: vec![create_test_server_config(
            "127.0.0.1",
            backend_port,
            "backend",
        )],
        ..Default::default()
    };

    let proxy = NntpProxy::new(config, routing_mode)?;

    tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = proxy_listener.accept().await {
                let proxy_clone = proxy.clone();
                tokio::spawn(async move {
                    let _ = proxy_clone.handle_client(stream, addr).await;
                });
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    Ok((proxy_port, backend_port, mock))
}

#[tokio::test]
async fn test_stateful_mode_basic() -> Result<()> {
    let (proxy_port, _, _mock) = setup_proxy_with_mode(RoutingMode::Stateful).await?;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;

    // Read greeting
    let mut buffer = vec![0u8; 4096];
    let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
    let greeting = String::from_utf8_lossy(&buffer[..n]);
    assert!(greeting.contains("200"));

    // Send HELP command
    client.write_all(b"HELP\r\n").await?;
    let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
    let response = String::from_utf8_lossy(&buffer[..n]);
    assert!(response.contains("100"));

    // Send QUIT
    client.write_all(b"QUIT\r\n").await?;
    let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
    let response = String::from_utf8_lossy(&buffer[..n]);
    assert!(response.contains("205") || response.contains("Goodbye"));

    Ok(())
}

#[tokio::test]
async fn test_stateful_mode_group_command() -> Result<()> {
    let (proxy_port, _, _mock) = setup_proxy_with_mode(RoutingMode::Stateful).await?;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;

    // Read greeting
    let mut buffer = vec![0u8; 4096];
    let _ = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;

    // Send GROUP command (stateful)
    client.write_all(b"GROUP alt.test\r\n").await?;
    let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
    let response = String::from_utf8_lossy(&buffer[..n]);
    assert!(response.contains("211") || response.contains("alt.test"));

    Ok(())
}

#[tokio::test]
async fn test_per_command_mode_basic() -> Result<()> {
    let (proxy_port, _, _mock) = setup_proxy_with_mode(RoutingMode::PerCommand).await?;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;

    // Read greeting
    let mut buffer = vec![0u8; 4096];
    let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
    let greeting = String::from_utf8_lossy(&buffer[..n]);
    assert!(greeting.contains("200"));

    // Send HELP command (stateless)
    client.write_all(b"HELP\r\n").await?;
    let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
    let response = String::from_utf8_lossy(&buffer[..n]);
    assert!(response.contains("100"));

    // Send DATE command (stateless)
    client.write_all(b"DATE\r\n").await?;
    let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
    let response = String::from_utf8_lossy(&buffer[..n]);
    assert!(response.contains("111"));

    Ok(())
}

#[tokio::test]
async fn test_per_command_mode_multiple_commands() -> Result<()> {
    let (proxy_port, _, _mock) = setup_proxy_with_mode(RoutingMode::PerCommand).await?;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;

    // Read greeting
    let mut buffer = vec![0u8; 4096];
    let _ = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;

    // Send multiple commands - each should route independently
    client.write_all(b"HELP\r\n").await?;
    let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
    assert!(n > 0);

    client.write_all(b"DATE\r\n").await?;
    let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
    assert!(n > 0);

    client.write_all(b"LIST\r\n").await?;
    let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
    assert!(n > 0);

    Ok(())
}

#[tokio::test]
async fn test_hybrid_mode_starts_per_command() -> Result<()> {
    let (proxy_port, _, _mock) = setup_proxy_with_mode(RoutingMode::Hybrid).await?;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;

    // Read greeting
    let mut buffer = vec![0u8; 4096];
    let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
    let greeting = String::from_utf8_lossy(&buffer[..n]);
    assert!(greeting.contains("200"));

    // Should handle stateless commands in per-command mode initially
    client.write_all(b"HELP\r\n").await?;
    let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
    let response = String::from_utf8_lossy(&buffer[..n]);
    assert!(response.contains("100"));

    client.write_all(b"DATE\r\n").await?;
    let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
    let response = String::from_utf8_lossy(&buffer[..n]);
    assert!(response.contains("111"));

    Ok(())
}

#[tokio::test]
async fn test_hybrid_mode_switches_to_stateful() -> Result<()> {
    let (proxy_port, _, _mock) = setup_proxy_with_mode(RoutingMode::Hybrid).await?;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;

    // Read greeting
    let mut buffer = vec![0u8; 4096];
    let _ = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;

    // Start with stateless command
    client.write_all(b"HELP\r\n").await?;
    let _ = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;

    // Send stateful command - should trigger switch to stateful mode
    client.write_all(b"GROUP alt.test\r\n").await?;
    let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
    let response = String::from_utf8_lossy(&buffer[..n]);
    assert!(response.contains("211") || response.contains("alt.test"));

    // Should remain in stateful mode for subsequent commands
    client.write_all(b"LIST\r\n").await?;
    let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
    let response = String::from_utf8_lossy(&buffer[..n]);
    assert!(response.contains("215"));

    Ok(())
}

#[tokio::test]
async fn test_multiple_clients_stateful() -> Result<()> {
    let (proxy_port, _, _mock) = setup_proxy_with_mode(RoutingMode::Stateful).await?;

    // Connect multiple clients simultaneously
    let mut client1 = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;
    let mut client2 = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;

    let mut buffer1 = vec![0u8; 4096];
    let mut buffer2 = vec![0u8; 4096];

    // Both should get greetings
    let n1 = timeout(Duration::from_secs(1), client1.read(&mut buffer1)).await??;
    let n2 = timeout(Duration::from_secs(1), client2.read(&mut buffer2)).await??;

    assert!(n1 > 0);
    assert!(n2 > 0);

    // Both should be able to send commands independently
    client1.write_all(b"HELP\r\n").await?;
    client2.write_all(b"DATE\r\n").await?;

    let n1 = timeout(Duration::from_secs(1), client1.read(&mut buffer1)).await??;
    let n2 = timeout(Duration::from_secs(1), client2.read(&mut buffer2)).await??;

    assert!(n1 > 0);
    assert!(n2 > 0);

    Ok(())
}

#[tokio::test]
async fn test_multiple_clients_per_command() -> Result<()> {
    let (proxy_port, _, _mock) = setup_proxy_with_mode(RoutingMode::PerCommand).await?;

    // Connect multiple clients simultaneously
    let mut client1 = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;
    let mut client2 = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;

    let mut buffer1 = vec![0u8; 4096];
    let mut buffer2 = vec![0u8; 4096];

    // Both should get greetings
    let _ = timeout(Duration::from_secs(1), client1.read(&mut buffer1)).await??;
    let _ = timeout(Duration::from_secs(1), client2.read(&mut buffer2)).await??;

    // Both should share backend connections in per-command mode
    client1.write_all(b"HELP\r\n").await?;
    client2.write_all(b"DATE\r\n").await?;

    let n1 = timeout(Duration::from_secs(1), client1.read(&mut buffer1)).await??;
    let n2 = timeout(Duration::from_secs(1), client2.read(&mut buffer2)).await??;

    assert!(n1 > 0);
    assert!(n2 > 0);

    Ok(())
}

#[tokio::test]
async fn test_quit_command_closes_connection() -> Result<()> {
    let (proxy_port, _, _mock) = setup_proxy_with_mode(RoutingMode::Hybrid).await?;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;

    let mut buffer = vec![0u8; 4096];
    let _ = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;

    // Send QUIT
    client.write_all(b"QUIT\r\n").await?;
    let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
    let response = String::from_utf8_lossy(&buffer[..n]);
    assert!(response.contains("205"));

    // Connection should close after QUIT
    let result = timeout(Duration::from_secs(1), client.read(&mut buffer)).await;
    // Should either timeout or read 0 bytes (connection closed)
    if let Ok(Ok(n)) = result {
        assert_eq!(n, 0, "Connection should be closed after QUIT");
    }

    Ok(())
}

#[tokio::test]
async fn test_sequential_commands_stateful() -> Result<()> {
    let (proxy_port, _, _mock) = setup_proxy_with_mode(RoutingMode::Stateful).await?;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;

    let mut buffer = vec![0u8; 4096];
    let _ = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;

    // Send sequence of commands
    let commands = vec!["HELP\r\n", "DATE\r\n", "LIST\r\n"];

    for cmd in commands {
        client.write_all(cmd.as_bytes()).await?;
        let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
        assert!(n > 0, "Should receive response for: {}", cmd.trim());
    }

    Ok(())
}

#[tokio::test]
async fn test_connection_error_handling() -> Result<()> {
    let (proxy_port, _, _mock) = setup_proxy_with_mode(RoutingMode::Hybrid).await?;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;

    let mut buffer = vec![0u8; 4096];
    let _ = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;

    // Send a command
    client.write_all(b"HELP\r\n").await?;
    let _ = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;

    // Abruptly close connection (simulates network error)
    drop(client);

    // Proxy should handle this gracefully (no panic)
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Should be able to connect a new client
    let mut client2 = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;
    let n = timeout(Duration::from_secs(1), client2.read(&mut buffer)).await??;
    assert!(n > 0);

    Ok(())
}

/// Test that per-command mode properly releases pending counts after successful execution
///
/// This test exercises the bug where `complete_command()` was not called after successful
/// command execution, causing the pending count to leak and eventually exhaust the connection pool.
///
/// The bug manifests as:
/// - Pending count increments on each command
/// - Never decrements on success (only on 430 errors)
/// - After N commands, pool shows N pending but all connections are idle
/// - New requests fail with "failed to acquire connection after 20 attempts"
#[tokio::test]
async fn test_per_command_pending_count_release() -> Result<()> {
    use std::sync::Arc;

    // Setup mock backend
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    drop(backend_listener);

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await?;
    let proxy_port = proxy_listener.local_addr()?.port();

    // Mock that responds to ARTICLE commands
    let mock = MockNntpServer::new(backend_port)
        .with_name("Test Backend")
        .on_command(
            "ARTICLE <test1@example.com>",
            "220 0 <test1@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n",
        )
        .on_command(
            "ARTICLE <test2@example.com>",
            "220 0 <test2@example.com>\r\nSubject: Test2\r\n\r\nBody2\r\n.\r\n",
        )
        .on_command(
            "ARTICLE <test3@example.com>",
            "220 0 <test3@example.com>\r\nSubject: Test3\r\n\r\nBody3\r\n.\r\n",
        )
        .on_command("DATE", "111 20251120120000\r\n")
        .on_command("QUIT", "205 Goodbye\r\n")
        .spawn();

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create proxy with small connection pool to make leak obvious
    let config = Config {
        servers: vec![
            config_helpers::create_test_server_config_with_max_connections(
                "127.0.0.1",
                backend_port,
                "backend",
                3, // Small pool to trigger leak faster
            ),
        ],
        ..Default::default()
    };

    let proxy = NntpProxy::new(config, RoutingMode::PerCommand)?;
    let proxy_clone = Arc::new(proxy.clone());
    let proxy_for_check = proxy_clone.clone();

    tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = proxy_listener.accept().await {
                let proxy_clone = proxy_clone.clone();
                tokio::spawn(async move {
                    let _ = proxy_clone.handle_client(stream, addr).await;
                });
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connect client
    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;
    let mut buffer = vec![0u8; 8192];

    // Read greeting
    let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
    let greeting = String::from_utf8_lossy(&buffer[..n]);
    assert!(
        greeting.contains("200"),
        "Expected greeting, got: {}",
        greeting
    );

    // Execute multiple successful commands and check pending count after each
    // Before the fix, pending count would increment on each command
    // After the fix, pending count should return to 0 after each command
    for i in 1..=5 {
        let msg_id = format!("test{}@example.com", if i <= 3 { i } else { (i % 3) + 1 });
        let command = format!("ARTICLE <{}>\r\n", msg_id);

        client.write_all(command.as_bytes()).await?;
        let n = timeout(Duration::from_secs(2), client.read(&mut buffer)).await??;
        let response = String::from_utf8_lossy(&buffer[..n]);

        // Should succeed
        assert!(
            response.contains("220"),
            "Command {} should succeed, got: {}",
            i,
            response
        );

        // Wait for command to fully complete
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Check pending count - should be 0 after command completes
        // BUG: Without the fix, pending count will be `i` (leaked count)
        // FIX: With the fix, pending count should be 0
        let backend_id = nntp_proxy::types::BackendId::from_index(0);
        if let Some(load) = proxy_for_check.backend_load(backend_id) {
            assert_eq!(
                load, 0,
                "Pending count should be 0 after command {}, but got {} (LEAKED!)",
                i, load
            );
        }
    }

    // If we got here, pending counts were properly released

    client.write_all(b"QUIT\r\n").await?;
    let _ = timeout(Duration::from_secs(1), client.read(&mut buffer)).await;

    drop(mock);
    Ok(())
}
