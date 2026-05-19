//! Tests for adaptive prechecking feature
//!
//! Adaptive prechecking allows STAT/HEAD commands to check all backends
//! simultaneously to build accurate availability cache while serving clients efficiently.

use crate::test_helpers::{MockNntpServer, create_test_server_config, wait_for_server};
use anyhow::Result;
use nntp_proxy::{Cache, Config, NntpProxy, RoutingMode};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, Semaphore};
use tokio::time::timeout;

/// Helper to create config with adaptive precheck enabled
fn create_config_with_precheck(backend_port: u16, adaptive_precheck: bool) -> Config {
    Config {
        servers: vec![create_test_server_config(
            "127.0.0.1",
            backend_port,
            "test-backend",
        )],
        cache: Some(Cache {
            store_article_bodies: false,
            adaptive_precheck,
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Helper to setup proxy and return `proxy_port`
async fn setup_proxy_with_config(config: Config, routing_mode: RoutingMode) -> Result<u16> {
    let proxy_listener = TcpListener::bind("127.0.0.1:0").await?;
    let proxy_port = proxy_listener.local_addr()?.port();
    let proxy_addr = format!("127.0.0.1:{proxy_port}");

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

    wait_for_server(&proxy_addr, 20).await?;
    Ok(proxy_port)
}

/// Test STAT precheck returns first successful response (not optimistic)
#[tokio::test]
async fn test_stat_precheck_first_response() -> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();

    let request_count = Arc::new(AtomicUsize::new(0));
    let request_count_clone = request_count.clone();
    let stat_seen = Arc::new(Notify::new());
    let stat_seen_clone = stat_seen.clone();
    let allow_response = Arc::new(Semaphore::new(0));
    let allow_response_clone = allow_response.clone();

    // Create backend that waits for explicit test release before replying to STAT.
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = backend_listener.accept().await {
            let count = request_count_clone.clone();
            let stat_seen = stat_seen_clone.clone();
            let allow_response = allow_response_clone.clone();
            tokio::spawn(async move {
                // Send greeting
                stream.write_all(b"200 Mock Server Ready\r\n").await.ok();

                let mut buf = vec![0u8; 1024];
                loop {
                    match stream.read(&mut buf).await {
                        Ok(n) if n > 0 => {
                            let commands = String::from_utf8_lossy(&buf[..n]);

                            for cmd in commands.split("\r\n").filter(|cmd| !cmd.is_empty()) {
                                if cmd.starts_with("STAT") {
                                    count.fetch_add(1, Ordering::SeqCst);
                                    stat_seen.notify_waiters();
                                    allow_response.acquire().await.unwrap().forget();
                                    stream.write_all(b"223 0 <test@example.com>\r\n").await.ok();
                                } else if cmd.starts_with("QUIT") {
                                    stream.write_all(b"205 Goodbye\r\n").await.ok();
                                    return;
                                } else {
                                    stream.write_all(b"200 OK\r\n").await.ok();
                                }
                            }
                        }
                        Ok(_) | Err(_) => break,
                    }
                }
            });
        }
    });

    let config = create_config_with_precheck(backend_port, true);
    let proxy_port = setup_proxy_with_config(config, RoutingMode::PerCommand).await?;

    // Connect and send STAT command
    let mut client = TcpStream::connect(format!("127.0.0.1:{proxy_port}")).await?;
    let mut buf = vec![0u8; 4096];

    // Read greeting
    timeout(Duration::from_secs(1), client.read(&mut buf)).await??;

    let stat_seen_notification = stat_seen.notified();
    client.write_all(b"STAT <test@example.com>\r\n").await?;

    timeout(Duration::from_secs(1), stat_seen_notification).await?;
    assert!(
        timeout(Duration::from_millis(75), client.read(&mut buf))
            .await
            .is_err(),
        "client should not receive an optimistic STAT response before backend reply"
    );

    allow_response.add_permits(1);

    let n = timeout(Duration::from_secs(1), client.read(&mut buf)).await??;
    let response = String::from_utf8_lossy(&buf[..n]);

    // Should get first backend's response (waits for actual response, not optimistic)
    assert!(
        response.starts_with("223"),
        "Expected 223 from backend, got: {response}"
    );

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

    let backend2_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend2_port = backend2_listener.local_addr()?.port();

    // Backend 1: Fast responder
    let _mock1 = MockNntpServer::new()
        .with_name("fast-backend")
        .on_command(
            "HEAD",
            "221 0 <test@example.com>\r\nSubject: Fast\r\n\r\n.\r\n",
        )
        .spawn_on_listener(backend1_listener);

    // Backend 2: Slow responder
    tokio::spawn(async move {
        while let Ok((stream, _)) = backend2_listener.accept().await {
            tokio::spawn(async move {
                let mut reader = BufReader::new(stream);
                reader
                    .get_mut()
                    .write_all(b"200 Slow Server Ready\r\n")
                    .await
                    .ok();
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            let cmd = line.trim();

                            if cmd.starts_with("HEAD") {
                                for _ in 0..64 {
                                    tokio::task::yield_now().await;
                                }
                                reader
                                    .get_mut()
                                    .write_all(
                                        b"221 0 <test@example.com>\r\nSubject: Slow\r\n\r\n.\r\n",
                                    )
                                    .await
                                    .ok();
                            } else if cmd.starts_with("QUIT") {
                                reader.get_mut().write_all(b"205 Goodbye\r\n").await.ok();
                                break;
                            } else {
                                reader.get_mut().write_all(b"200 OK\r\n").await.ok();
                            }
                        }
                    }
                }
            });
        }
    });

    wait_for_server(&format!("127.0.0.1:{backend1_port}"), 20).await?;
    wait_for_server(&format!("127.0.0.1:{backend2_port}"), 20).await?;

    // Create config with both backends
    let config = Config {
        servers: vec![
            create_test_server_config("127.0.0.1", backend1_port, "fast-backend"),
            create_test_server_config("127.0.0.1", backend2_port, "slow-backend"),
        ],
        cache: Some(Cache {
            store_article_bodies: false,
            adaptive_precheck: true,
            ..Default::default()
        }),
        ..Default::default()
    };

    let proxy_port = setup_proxy_with_config(config, RoutingMode::PerCommand).await?;

    // Connect and send HEAD command
    let mut client = TcpStream::connect(format!("127.0.0.1:{proxy_port}")).await?;
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
        "Response took too long: {elapsed:?}"
    );
    assert!(
        response.starts_with("221"),
        "Expected 221 response, got: {response}"
    );

    Ok(())
}

/// Test adaptive precheck disabled by default
#[tokio::test]
async fn test_adaptive_precheck_disabled_by_default() -> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();

    let _mock = MockNntpServer::new()
        .with_name("backend")
        .on_command("STAT", "223 0 <test@example.com>\r\n")
        .spawn_on_listener(backend_listener);

    // Config without adaptive precheck
    let config = create_config_with_precheck(backend_port, false);
    let proxy_port = setup_proxy_with_config(config, RoutingMode::PerCommand).await?;

    // Send STAT command
    let mut client = TcpStream::connect(format!("127.0.0.1:{proxy_port}")).await?;
    let mut buf = vec![0u8; 4096];

    // Read greeting
    timeout(Duration::from_secs(1), client.read(&mut buf)).await??;

    client.write_all(b"STAT <test@example.com>\r\n").await?;
    let n = timeout(Duration::from_secs(1), client.read(&mut buf)).await??;
    let response = String::from_utf8_lossy(&buf[..n]);

    // Should get response from backend (normal per-command routing)
    assert!(
        response.starts_with("223"),
        "Expected 223 response, got: {response}"
    );

    Ok(())
}

/// Test STAT/HEAD without message-ID doesn't trigger precheck
#[tokio::test]
async fn test_precheck_requires_message_id() -> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();

    let _mock = MockNntpServer::new()
        .with_name("backend")
        .on_command("STAT", "412 No newsgroup selected\r\n")
        .on_command("HEAD", "412 No newsgroup selected\r\n")
        .spawn_on_listener(backend_listener);

    let config = create_config_with_precheck(backend_port, true);
    let proxy_port = setup_proxy_with_config(config, RoutingMode::PerCommand).await?;

    // Send STAT without message-ID (should not trigger precheck)
    let mut client = TcpStream::connect(format!("127.0.0.1:{proxy_port}")).await?;
    let mut buf = vec![0u8; 4096];

    // Read greeting
    timeout(Duration::from_secs(1), client.read(&mut buf)).await??;

    client.write_all(b"STAT\r\n").await?;
    let n = timeout(Duration::from_secs(1), client.read(&mut buf)).await??;
    let response = String::from_utf8_lossy(&buf[..n]);

    // Should get error from backend
    assert!(
        response.starts_with("412"),
        "Expected error response, got: {response}"
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

    let _mock = MockNntpServer::new()
        .with_name("test-backend")
        .spawn_on_listener(backend_listener);

    // Create proxy in STATEFUL mode (not per-command)
    let config = create_config_with_precheck(backend_port, true);
    let proxy_port = setup_proxy_with_config(config, RoutingMode::Stateful).await?;

    // Connect client
    let mut client = TcpStream::connect(format!("127.0.0.1:{proxy_port}")).await?;
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
