//! Regression tests for RFC 3977 / RFC 4643 protocol format compliance bugs.
//!
//! These tests were written *before* the corresponding fixes so they confirm
//! each bug exists, then verify the fix closes it.

mod test_helpers;
use test_helpers::*;

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{Duration, timeout};

use nntp_proxy::{NntpProxy, RoutingMode};

// ---------------------------------------------------------------------------
// Bug 3 + Bug 4: overlong first command in pipeline
//
// RFC 3977 §3.2 — server SHOULD return 501 (syntax error) for commands that
// exceed the 512-byte limit and continue serving the connection.
//
// Before fix:
//   • The first oversized command in a batch returned `Err(...)` from
//     `read_command_batch`, which closed the session.
//   • The error response used status code 500 instead of 501.
// ---------------------------------------------------------------------------

/// Bug 3 + 4: Send a command > 512 bytes as the *first* command in a batch.
/// The proxy must reply with 501, keep the connection open, and process the
/// next command normally.
#[tokio::test]
async fn test_overlong_first_command_returns_501_and_continues() -> Result<()> {
    // Bind backend and proxy listeners up front (prevents port races)
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await?;
    let proxy_port = proxy_listener.local_addr()?.port();

    // Start a minimal mock backend
    let _backend = MockNntpServer::new(backend_port)
        .with_name("TestBackend")
        .on_command("LIST", "215 list follows\r\n.\r\n")
        .spawn_on_listener(backend_listener);

    tokio::time::sleep(Duration::from_millis(50)).await;

    let config = create_test_config(vec![(backend_port, "TestBackend")]);
    let proxy = NntpProxy::new(config, RoutingMode::PerCommand).await?;

    tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = proxy_listener.accept().await {
                let proxy_clone = proxy.clone();
                tokio::spawn(async move {
                    let _ = proxy_clone
                        .handle_client_per_command_routing(stream, addr.into())
                        .await;
                });
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut client = TcpStream::connect(format!("127.0.0.1:{proxy_port}")).await?;

    // Read and discard the greeting
    let mut buf = vec![0u8; 1024];
    let n = timeout(Duration::from_secs(2), client.read(&mut buf)).await??;
    let greeting = String::from_utf8_lossy(&buf[..n]);
    assert!(
        greeting.starts_with("201"),
        "Expected 200 greeting, got: {greeting}"
    );

    // Send a command that is just over the 512-byte RFC 3977 limit.
    // We use "LIST " + 510 'A's + "\r\n" = 518 bytes total.
    let overlong = format!("LIST {}\r\n", "A".repeat(510));
    assert!(overlong.len() > 512, "test command must be > 512 bytes");
    client.write_all(overlong.as_bytes()).await?;

    // The proxy must reply with 501 (syntax error), NOT close the connection
    let n = timeout(Duration::from_secs(2), client.read(&mut buf)).await??;
    let response = String::from_utf8_lossy(&buf[..n]);
    assert!(
        response.starts_with("501"),
        "Bug 3+4: expected 501 for overlong command, got: {response:?}"
    );

    // Connection must still be alive — send a normal LIST command
    client.write_all(b"LIST\r\n").await?;
    let n = timeout(Duration::from_secs(2), client.read(&mut buf)).await??;
    let follow_up = String::from_utf8_lossy(&buf[..n]);
    assert!(
        follow_up.starts_with("215"),
        "Connection should remain open after 501; got: {follow_up:?}"
    );

    Ok(())
}

/// Verify the error code is exactly 501 (not 500).
///
/// RFC 3977 §3.2.1: 500 = unknown command; 501 = syntax error.
/// An oversized command is a *syntax* error, not an unknown command.
#[tokio::test]
async fn test_overlong_command_uses_501_not_500() -> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await?;
    let proxy_port = proxy_listener.local_addr()?.port();

    let _backend = MockNntpServer::new(backend_port)
        .with_name("TestBackend501")
        .spawn_on_listener(backend_listener);

    tokio::time::sleep(Duration::from_millis(50)).await;

    let config = create_test_config(vec![(backend_port, "TestBackend501")]);
    let proxy = NntpProxy::new(config, RoutingMode::PerCommand).await?;

    tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = proxy_listener.accept().await {
                let proxy_clone = proxy.clone();
                tokio::spawn(async move {
                    let _ = proxy_clone
                        .handle_client_per_command_routing(stream, addr.into())
                        .await;
                });
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut client = TcpStream::connect(format!("127.0.0.1:{proxy_port}")).await?;

    let mut buf = vec![0u8; 1024];
    let n = timeout(Duration::from_secs(2), client.read(&mut buf)).await??;
    let greeting = String::from_utf8_lossy(&buf[..n]);
    assert!(
        greeting.starts_with("201"),
        "Expected 200 greeting: {greeting}"
    );

    // Overlong command as a pipelined trailing command (> 512 bytes)
    let overlong = format!("STAT {}\r\n", "B".repeat(510));
    client.write_all(overlong.as_bytes()).await?;

    let n = timeout(Duration::from_secs(2), client.read(&mut buf)).await??;
    let response = String::from_utf8_lossy(&buf[..n]);

    assert!(
        response.starts_with("501"),
        "Bug 4: response must be 501 (not 500) for overlong command; got: {response:?}"
    );
    assert!(
        !response.starts_with("500"),
        "Bug 4: response must NOT be 500; got: {response:?}"
    );

    Ok(())
}
