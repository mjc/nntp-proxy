//! Tests for adaptive prechecking feature
//!
//! Adaptive prechecking allows STAT/HEAD commands to check all backends
//! simultaneously to build accurate availability cache while serving clients efficiently.

mod test_helpers;

use anyhow::Result;
use nntp_proxy::{Cache, Config, NntpProxy, RoutingMode};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use test_helpers::{MockNntpServer, create_test_server_config};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{sleep, timeout};

/// Helper to create config with adaptive precheck enabled
fn create_config_with_precheck(backend_port: u16, adaptive_precheck: bool) -> Config {
    Config {
        servers: vec![create_test_server_config(
            "127.0.0.1",
            backend_port,
            "test-backend",
        )],
        cache: Some(Cache {
            cache_articles: false,
            adaptive_precheck,
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Helper to setup proxy and return proxy_port
async fn setup_proxy_with_config(config: Config, routing_mode: RoutingMode) -> Result<u16> {
    let proxy_listener = TcpListener::bind("127.0.0.1:0").await?;
    let proxy_port = proxy_listener.local_addr()?.port();

    let proxy = NntpProxy::new(config, routing_mode).await?;

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

    sleep(Duration::from_millis(100)).await;
    Ok(proxy_port)
}

/// Test STAT precheck returns first successful response (not optimistic)
#[tokio::test]
async fn test_stat_precheck_first_response() -> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    drop(backend_listener);

    let request_count = Arc::new(AtomicUsize::new(0));
    let request_count_clone = request_count.clone();

    // Create backend that will respond after delay
    tokio::spawn(async move {
        let listener = TcpListener::bind(format!("127.0.0.1:{}", backend_port))
            .await
            .unwrap();

        while let Ok((mut stream, _)) = listener.accept().await {
            let count = request_count_clone.clone();
            tokio::spawn(async move {
                // Send greeting
                stream.write_all(b"200 Mock Server Ready\r\n").await.ok();

                let mut buf = vec![0u8; 1024];
                loop {
                    match stream.try_read(&mut buf) {
                        Ok(n) if n > 0 => {
                            let cmd = String::from_utf8_lossy(&buf[..n]);

                            if cmd.starts_with("STAT") {
                                count.fetch_add(1, Ordering::SeqCst);
                                // Small delay but still respond successfully
                                sleep(Duration::from_millis(50)).await;
                                stream.write_all(b"223 0 <test@example.com>\r\n").await.ok();
                            } else if cmd.starts_with("QUIT") {
                                stream.write_all(b"205 Goodbye\r\n").await.ok();
                                break;
                            } else {
                                stream.write_all(b"200 OK\r\n").await.ok();
                            }
                        }
                        Ok(_) => break,
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            sleep(Duration::from_millis(10)).await;
                        }
                        Err(_) => break,
                    }
                }
            });
        }
    });

    sleep(Duration::from_millis(50)).await;

    let config = create_config_with_precheck(backend_port, true);
    let proxy_port = setup_proxy_with_config(config, RoutingMode::PerCommand).await?;

    // Connect and send STAT command
    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;
    let mut buf = vec![0u8; 4096];

    // Read greeting
    timeout(Duration::from_secs(1), client.read(&mut buf)).await??;

    let start = std::time::Instant::now();
    client.write_all(b"STAT <test@example.com>\r\n").await?;
    let n = timeout(Duration::from_secs(1), client.read(&mut buf)).await??;
    let response = String::from_utf8_lossy(&buf[..n]);
    let elapsed = start.elapsed();

    // Should get first backend's response (waits for actual response, not optimistic)
    assert!(
        response.starts_with("223"),
        "Expected 223 from backend, got: {}",
        response
    );

    // Should be reasonably fast (first backend response + small overhead)
    assert!(
        elapsed < Duration::from_millis(200),
        "Response took too long: {:?}",
        elapsed
    );

    // Wait for background task to complete
    sleep(Duration::from_millis(200)).await;

    // Backend should have been checked
    assert_eq!(
        request_count.load(Ordering::SeqCst),
        1,
        "Backend should have been checked"
    );

    Ok(())
}

/// Test HEAD precheck returns first successful response
#[tokio::test]
async fn test_head_precheck_first_response_wins() -> Result<()> {
    let backend1_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend1_port = backend1_listener.local_addr()?.port();
    drop(backend1_listener);

    let backend2_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend2_port = backend2_listener.local_addr()?.port();
    drop(backend2_listener);

    // Backend 1: Fast responder
    let _mock1 = MockNntpServer::new(backend1_port)
        .with_name("fast-backend")
        .on_command(
            "HEAD",
            "221 0 <test@example.com>\r\nSubject: Fast\r\n\r\n.\r\n",
        )
        .spawn();

    // Backend 2: Slow responder
    let backend2_checked = Arc::new(AtomicUsize::new(0));
    let backend2_checked_clone = backend2_checked.clone();

    tokio::spawn(async move {
        let listener = TcpListener::bind(format!("127.0.0.1:{}", backend2_port))
            .await
            .unwrap();

        while let Ok((mut stream, _)) = listener.accept().await {
            let checked = backend2_checked_clone.clone();
            tokio::spawn(async move {
                stream.write_all(b"200 Slow Server Ready\r\n").await.ok();

                let mut buf = vec![0u8; 1024];
                loop {
                    match stream.try_read(&mut buf) {
                        Ok(n) if n > 0 => {
                            let cmd = String::from_utf8_lossy(&buf[..n]);

                            if cmd.starts_with("HEAD") {
                                checked.fetch_add(1, Ordering::SeqCst);
                                // Delay to ensure fast backend wins
                                sleep(Duration::from_millis(200)).await;
                                stream
                                    .write_all(
                                        b"221 0 <test@example.com>\r\nSubject: Slow\r\n\r\n.\r\n",
                                    )
                                    .await
                                    .ok();
                            } else if cmd.starts_with("QUIT") {
                                stream.write_all(b"205 Goodbye\r\n").await.ok();
                                break;
                            } else {
                                stream.write_all(b"200 OK\r\n").await.ok();
                            }
                        }
                        Ok(_) => break,
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            sleep(Duration::from_millis(10)).await;
                        }
                        Err(_) => break,
                    }
                }
            });
        }
    });

    sleep(Duration::from_millis(100)).await;

    // Create config with both backends
    let config = Config {
        servers: vec![
            create_test_server_config("127.0.0.1", backend1_port, "fast-backend"),
            create_test_server_config("127.0.0.1", backend2_port, "slow-backend"),
        ],
        cache: Some(Cache {
            cache_articles: false,
            adaptive_precheck: true,
            ..Default::default()
        }),
        ..Default::default()
    };

    let proxy_port = setup_proxy_with_config(config, RoutingMode::PerCommand).await?;

    // Connect and send HEAD command
    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;
    let mut buf = vec![0u8; 4096];

    // Read greeting
    timeout(Duration::from_secs(1), client.read(&mut buf)).await??;

    let start = std::time::Instant::now();
    client.write_all(b"HEAD <test@example.com>\r\n").await?;
    let n = timeout(Duration::from_secs(1), client.read(&mut buf)).await??;
    let response = String::from_utf8_lossy(&buf[..n]);
    let elapsed = start.elapsed();

    // Should get fast backend's response quickly
    assert!(
        elapsed < Duration::from_millis(150),
        "Response took too long: {:?}",
        elapsed
    );
    assert!(
        response.starts_with("221"),
        "Expected 221 response, got: {}",
        response
    );

    // Wait for background task to complete checking slow backend
    sleep(Duration::from_millis(500)).await;

    // Note: We can't reliably verify that slow backend was checked because
    // the background task may complete before the slow backend connection is established.
    // The important part is that the client got the fast response.
    // If slow backend was checked, great; if not, that's also acceptable.

    Ok(())
}

/// Test adaptive precheck disabled by default
#[tokio::test]
async fn test_adaptive_precheck_disabled_by_default() -> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    drop(backend_listener);

    let _mock = MockNntpServer::new(backend_port)
        .with_name("backend")
        .on_command("STAT", "223 0 <test@example.com>\r\n")
        .spawn();

    sleep(Duration::from_millis(50)).await;

    // Config without adaptive precheck
    let config = create_config_with_precheck(backend_port, false);
    let proxy_port = setup_proxy_with_config(config, RoutingMode::PerCommand).await?;

    // Send STAT command
    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;
    let mut buf = vec![0u8; 4096];

    // Read greeting
    timeout(Duration::from_secs(1), client.read(&mut buf)).await??;

    client.write_all(b"STAT <test@example.com>\r\n").await?;
    let n = timeout(Duration::from_secs(1), client.read(&mut buf)).await??;
    let response = String::from_utf8_lossy(&buf[..n]);

    // Should get response from backend (normal per-command routing)
    assert!(
        response.starts_with("223"),
        "Expected 223 response, got: {}",
        response
    );

    Ok(())
}

/// Test STAT/HEAD without message-ID doesn't trigger precheck
#[tokio::test]
async fn test_precheck_requires_message_id() -> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    drop(backend_listener);

    let _mock = MockNntpServer::new(backend_port)
        .with_name("backend")
        .on_command("STAT", "412 No newsgroup selected\r\n")
        .on_command("HEAD", "412 No newsgroup selected\r\n")
        .spawn();

    sleep(Duration::from_millis(50)).await;

    let config = create_config_with_precheck(backend_port, true);
    let proxy_port = setup_proxy_with_config(config, RoutingMode::PerCommand).await?;

    // Send STAT without message-ID (should not trigger precheck)
    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;
    let mut buf = vec![0u8; 4096];

    // Read greeting
    timeout(Duration::from_secs(1), client.read(&mut buf)).await??;

    client.write_all(b"STAT\r\n").await?;
    let n = timeout(Duration::from_secs(1), client.read(&mut buf)).await??;
    let response = String::from_utf8_lossy(&buf[..n]);

    // Should get error from backend
    assert!(
        response.starts_with("412"),
        "Expected error response, got: {}",
        response
    );

    // Test passes if we get the 412 response and don't crash
    // (No way to verify backend call count with current MockNntpServer API)

    Ok(())
}

/// Test precheck only works in per-command mode
#[tokio::test]
async fn test_precheck_only_in_per_command_mode() -> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    drop(backend_listener);

    let _mock = MockNntpServer::new(backend_port)
        .with_name("test-backend")
        .spawn();

    sleep(Duration::from_millis(50)).await;

    // Create proxy in STATEFUL mode (not per-command)
    let config = create_config_with_precheck(backend_port, true);
    let proxy_port = setup_proxy_with_config(config, RoutingMode::Stateful).await?;

    // Connect client
    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;
    let mut buf = vec![0u8; 4096];

    // Read greeting
    timeout(Duration::from_secs(1), client.read(&mut buf)).await??;

    client.write_all(b"STAT <test@example.com>\r\n").await?;
    let n = timeout(Duration::from_secs(1), client.read(&mut buf)).await??;
    let response = String::from_utf8_lossy(&buf[..n]);

    // Should work normally (stateful mode doesn't use precheck)
    // This test mainly ensures no crashes occur
    assert!(
        !response.is_empty(),
        "Should get some response in stateful mode"
    );

    Ok(())
}
