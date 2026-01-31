//! Tests for stale connection retry logic
//!
//! When pooled connections become stale (backend closed them), the proxy should:
//! 1. Detect the stale connection on first use
//! 2. Remove it from the pool
//! 3. Get a fresh connection and retry
//!
//! This prevents the "overnight 430" bug where all articles fail after idle periods.

use crate::test_helpers::{create_test_server_config, get_available_port, wait_for_server};
use anyhow::Result;
use nntp_proxy::{Cache, Config, NntpProxy, RoutingMode};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::AbortHandle;

/// Mock server that closes connections after a configurable number of commands
///
/// This simulates backend servers that close idle connections.
struct StaleConnectionServer {
    port: u16,
    /// Number of commands to accept before closing the connection
    commands_before_close: usize,
    /// Track how many connections have been made
    connection_count: Arc<AtomicUsize>,
    /// Signal that server is ready
    ready: Arc<Notify>,
}

impl StaleConnectionServer {
    fn new(port: u16, commands_before_close: usize) -> Self {
        Self {
            port,
            commands_before_close,
            connection_count: Arc::new(AtomicUsize::new(0)),
            ready: Arc::new(Notify::new()),
        }
    }

    fn connection_count(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.connection_count)
    }

    fn ready_signal(&self) -> Arc<Notify> {
        Arc::clone(&self.ready)
    }

    fn spawn(self) -> AbortHandle {
        let port = self.port;
        let commands_before_close = self.commands_before_close;
        let connection_count = self.connection_count;
        let ready = self.ready;

        tokio::spawn(async move {
            let listener = TcpListener::bind(format!("127.0.0.1:{}", port))
                .await
                .expect("Failed to bind");

            ready.notify_one();

            while let Ok((mut stream, _)) = listener.accept().await {
                connection_count.fetch_add(1, Ordering::SeqCst);
                let commands_limit = commands_before_close;

                tokio::spawn(async move {
                    // Send greeting
                    if stream.write_all(b"200 StaleTest Ready\r\n").await.is_err() {
                        return;
                    }

                    let mut command_count = 0;
                    let mut buffer = [0u8; 4096];

                    loop {
                        let n = match stream.read(&mut buffer).await {
                            Ok(0) => break,
                            Ok(n) => n,
                            Err(_) => break,
                        };

                        let cmd = String::from_utf8_lossy(&buffer[..n]);
                        let cmd_upper = cmd.trim().to_uppercase();

                        // Handle COMPRESS DEFLATE (connection setup, don't count)
                        if cmd_upper.starts_with("COMPRESS") {
                            let _ = stream.write_all(b"500 Not supported\r\n").await;
                            continue;
                        }
                        // Handle DATE (health check)
                        else if cmd_upper.starts_with("DATE") {
                            let _ = stream.write_all(b"111 20251219120000\r\n").await;
                            command_count += 1;
                        }
                        // Handle QUIT
                        else if cmd_upper.starts_with("QUIT") {
                            let _ = stream.write_all(b"205 Goodbye\r\n").await;
                            break;
                        }
                        // Handle STAT/HEAD/ARTICLE
                        else if cmd_upper.starts_with("STAT")
                            || cmd_upper.starts_with("HEAD")
                            || cmd_upper.starts_with("ARTICLE")
                        {
                            let _ = stream
                                .write_all(b"223 0 <test@example.com> article exists\r\n")
                                .await;
                            command_count += 1;
                        }
                        // Default
                        else {
                            let _ = stream.write_all(b"200 OK\r\n").await;
                            command_count += 1;
                        }

                        // Close connection after limit reached (simulating idle timeout)
                        if commands_limit > 0 && command_count >= commands_limit {
                            // Close abruptly without QUIT response
                            break;
                        }
                    }
                });
            }
        })
        .abort_handle()
    }
}

/// Mock server that fails the first N connections, then works
///
/// Simulates transient connection failures that should be retried.
struct FailFirstNServer {
    port: u16,
    /// Number of connections to fail before accepting
    fail_count: usize,
    connection_count: Arc<AtomicUsize>,
    ready: Arc<Notify>,
}

impl FailFirstNServer {
    fn new(port: u16, fail_count: usize) -> Self {
        Self {
            port,
            fail_count,
            connection_count: Arc::new(AtomicUsize::new(0)),
            ready: Arc::new(Notify::new()),
        }
    }

    fn connection_count(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.connection_count)
    }

    fn ready_signal(&self) -> Arc<Notify> {
        Arc::clone(&self.ready)
    }

    fn spawn(self) -> AbortHandle {
        let port = self.port;
        let fail_count = self.fail_count;
        let connection_count = self.connection_count;
        let ready = self.ready;

        tokio::spawn(async move {
            let listener = TcpListener::bind(format!("127.0.0.1:{}", port))
                .await
                .expect("Failed to bind");

            ready.notify_one();

            while let Ok((mut stream, _)) = listener.accept().await {
                let conn_num = connection_count.fetch_add(1, Ordering::SeqCst);

                // First N connections fail immediately
                if conn_num < fail_count {
                    // Close without sending anything (simulates connection reset)
                    drop(stream);
                    continue;
                }

                tokio::spawn(async move {
                    // Send greeting
                    if stream.write_all(b"200 FailFirst Ready\r\n").await.is_err() {
                        return;
                    }

                    let mut buffer = [0u8; 4096];

                    loop {
                        let n = match stream.read(&mut buffer).await {
                            Ok(0) => break,
                            Ok(n) => n,
                            Err(_) => break,
                        };

                        let cmd = String::from_utf8_lossy(&buffer[..n]);
                        let cmd_upper = cmd.trim().to_uppercase();

                        if cmd_upper.starts_with("QUIT") {
                            let _ = stream.write_all(b"205 Goodbye\r\n").await;
                            break;
                        } else if cmd_upper.starts_with("DATE") {
                            let _ = stream.write_all(b"111 20251219120000\r\n").await;
                        } else if cmd_upper.starts_with("STAT")
                            || cmd_upper.starts_with("HEAD")
                            || cmd_upper.starts_with("ARTICLE")
                        {
                            let _ = stream
                                .write_all(b"223 0 <test@example.com> article exists\r\n")
                                .await;
                        } else {
                            let _ = stream.write_all(b"200 OK\r\n").await;
                        }
                    }
                });
            }
        })
        .abort_handle()
    }
}

/// Test that precheck retries when pooled connection is stale
#[tokio::test]
async fn test_precheck_retries_on_stale_connection() -> Result<()> {
    let port = get_available_port().await?;
    let proxy_port = get_available_port().await?;

    // Server that closes connection after 2 commands (DATE healthcheck + 1 real command)
    // This simulates stale pool connections after idle period
    let server = StaleConnectionServer::new(port, 2);
    let conn_count = server.connection_count();
    let ready = server.ready_signal();
    let _handle = server.spawn();

    ready.notified().await;

    // Create proxy with adaptive precheck enabled
    let config = Config {
        servers: vec![create_test_server_config("127.0.0.1", port, "StaleTest")],
        cache: Some(Cache {
            adaptive_precheck: true,
            ..Default::default()
        }),
        ..Default::default()
    };

    let proxy = NntpProxy::new(config, RoutingMode::Hybrid).await?;
    let proxy_addr = format!("127.0.0.1:{}", proxy_port);
    let listener = TcpListener::bind(&proxy_addr).await?;

    tokio::spawn({
        let proxy = proxy.clone();
        async move {
            while let Ok((stream, addr)) = listener.accept().await {
                let p = proxy.clone();
                tokio::spawn(async move {
                    let _ = p.handle_client(stream, addr.into()).await;
                });
            }
        }
    });

    wait_for_server(&proxy_addr, 20).await?;

    // Connect client
    let mut client = TcpStream::connect(&proxy_addr).await?;
    let mut reader = BufReader::new(&mut client);
    let mut line = String::new();

    // Read greeting
    reader.read_line(&mut line).await?;
    assert!(line.starts_with("200"), "Expected greeting, got: {}", line);
    line.clear();

    // First STAT command - should work (fresh connection)
    reader
        .get_mut()
        .write_all(b"STAT <test@example.com>\r\n")
        .await?;
    reader.read_line(&mut line).await?;
    assert!(
        line.starts_with("223"),
        "First STAT should succeed, got: {}",
        line
    );
    line.clear();

    // NOTE: No sleep needed here. The StaleConnectionServer will close the connection
    // after 2 commands (DATE healthcheck + this STAT). By the time we send the next
    // command, the pooled connection is already stale (closed by server). The retry
    // logic should detect the closed connection and establish a fresh one.

    // Second STAT - connection is stale (closed by server), should retry with new connection
    reader
        .get_mut()
        .write_all(b"STAT <test2@example.com>\r\n")
        .await?;
    reader.read_line(&mut line).await?;

    // Should succeed (retry logic got fresh connection)
    assert!(
        line.starts_with("223"),
        "Second STAT should succeed after retry, got: {}",
        line
    );

    // Verify multiple connections were made (original + retry)
    let total_conns = conn_count.load(Ordering::SeqCst);
    assert!(
        total_conns >= 2,
        "Should have made at least 2 connections (got {})",
        total_conns
    );

    reader.get_mut().write_all(b"QUIT\r\n").await?;

    Ok(())
}

/// Test that per-command routing retries on stale connection
#[tokio::test]
async fn test_per_command_retries_on_stale_connection() -> Result<()> {
    let port = get_available_port().await?;
    let proxy_port = get_available_port().await?;

    // Server that closes after 1 command
    let server = StaleConnectionServer::new(port, 1);
    let conn_count = server.connection_count();
    let ready = server.ready_signal();
    let _handle = server.spawn();

    ready.notified().await;

    // Disable precheck so we test the per_command path
    let config = Config {
        servers: vec![create_test_server_config("127.0.0.1", port, "StaleTest")],
        cache: Some(Cache {
            adaptive_precheck: false,
            ..Default::default()
        }),
        ..Default::default()
    };

    let proxy = NntpProxy::new(config, RoutingMode::PerCommand).await?;
    let proxy_addr = format!("127.0.0.1:{}", proxy_port);
    let listener = TcpListener::bind(&proxy_addr).await?;

    tokio::spawn({
        let proxy = proxy.clone();
        async move {
            while let Ok((stream, addr)) = listener.accept().await {
                let p = proxy.clone();
                tokio::spawn(async move {
                    let _ = p
                        .handle_client_per_command_routing(stream, addr.into())
                        .await;
                });
            }
        }
    });

    wait_for_server(&proxy_addr, 20).await?;

    let mut client = TcpStream::connect(&proxy_addr).await?;
    let mut reader = BufReader::new(&mut client);
    let mut line = String::new();

    // Read greeting
    reader.read_line(&mut line).await?;
    assert!(line.starts_with("200"));
    line.clear();

    // First command
    reader.get_mut().write_all(b"STAT <msg1@test>\r\n").await?;
    reader.read_line(&mut line).await?;
    assert!(line.starts_with("223"), "First should work: {}", line);
    line.clear();

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Second command - stale connection
    reader.get_mut().write_all(b"STAT <msg2@test>\r\n").await?;
    reader.read_line(&mut line).await?;
    assert!(line.starts_with("223"), "Retry should work: {}", line);

    // Check connections
    let total = conn_count.load(Ordering::SeqCst);
    assert!(total >= 2, "Need at least 2 connections, got {}", total);

    reader.get_mut().write_all(b"QUIT\r\n").await?;
    Ok(())
}

/// Test that connection failures on first attempt are retried
#[tokio::test]
async fn test_retry_on_immediate_connection_failure() -> Result<()> {
    let port = get_available_port().await?;
    let proxy_port = get_available_port().await?;

    // Server that fails first 2 connections
    let server = FailFirstNServer::new(port, 2);
    let conn_count = server.connection_count();
    let ready = server.ready_signal();
    let _handle = server.spawn();

    ready.notified().await;

    let config = Config {
        servers: vec![create_test_server_config("127.0.0.1", port, "FailFirst")],
        cache: Some(Cache {
            adaptive_precheck: false,
            ..Default::default()
        }),
        ..Default::default()
    };

    let proxy = NntpProxy::new(config, RoutingMode::PerCommand).await?;
    let proxy_addr = format!("127.0.0.1:{}", proxy_port);
    let listener = TcpListener::bind(&proxy_addr).await?;

    tokio::spawn({
        let proxy = proxy.clone();
        async move {
            while let Ok((stream, addr)) = listener.accept().await {
                let p = proxy.clone();
                tokio::spawn(async move {
                    let _ = p
                        .handle_client_per_command_routing(stream, addr.into())
                        .await;
                });
            }
        }
    });

    wait_for_server(&proxy_addr, 20).await?;

    let mut client = TcpStream::connect(&proxy_addr).await?;
    let mut reader = BufReader::new(&mut client);
    let mut line = String::new();

    // Read greeting
    reader.read_line(&mut line).await?;
    assert!(line.starts_with("200"));
    line.clear();

    // Try command - first 2 connections fail, third should work
    reader
        .get_mut()
        .write_all(b"STAT <test@example.com>\r\n")
        .await?;
    reader.read_line(&mut line).await?;

    // With retry logic, this should eventually succeed
    // The proxy will try, fail, retry with fresh connection
    let total = conn_count.load(Ordering::SeqCst);
    eprintln!("Connection count: {}, response: {}", total, line.trim());

    // Should have made multiple connection attempts
    assert!(total >= 2, "Should have retried connections, got {}", total);

    reader.get_mut().write_all(b"QUIT\r\n").await?;
    Ok(())
}

/// Test that merge_from respects NNTP semantics: 430 is authoritative
///
/// NNTP servers NEVER give false negatives (430 is always truthful).
/// NNTP servers CAN give false positives (2xx might be corrupt/incomplete).
/// Therefore: 430 (missing) ALWAYS overrides previous "has" state.
#[test]
fn test_availability_merge_430_overrides_has() {
    use nntp_proxy::cache::ArticleAvailability;
    use nntp_proxy::types::BackendId;

    let mut cache_state = ArticleAvailability::new();
    let b0 = BackendId::from_index(0);
    let b1 = BackendId::from_index(1);

    // Backend 0 previously returned success (might be false positive)
    cache_state.record_has(b0);
    assert!(!cache_state.is_missing(b0));

    // Later, backend 0 returns 430 (AUTHORITATIVE - article is NOT there)
    let mut new_result = ArticleAvailability::new();
    new_result.record_missing(b0);
    new_result.record_missing(b1);

    // Merge new result into cache
    cache_state.merge_from(&new_result);

    // CRITICAL: 430 is authoritative, so backend 0 MUST be marked missing
    // The previous "has" state was unreliable (possible false positive)
    assert!(
        cache_state.is_missing(b0),
        "Backend 0 MUST be marked missing - 430 is authoritative"
    );

    // Backend 1 should also be marked missing
    assert!(
        cache_state.is_missing(b1),
        "Backend 1 should be marked missing"
    );
}

/// Test that 430 cache entries are authoritative and persist
///
/// NNTP SEMANTICS: 430 is authoritative, so cache entries with 430 should NOT
/// be overridden by subsequent "has" results (which might be false positives).
/// The cache correctly maintains 430 state until TTL expiry.
#[tokio::test]
async fn test_430_cache_is_authoritative() -> Result<()> {
    use nntp_proxy::cache::{ArticleAvailability, ArticleCache};
    use nntp_proxy::router::BackendCount;
    use nntp_proxy::types::{BackendId, MessageId};
    use std::time::Duration;

    let cache = ArticleCache::new(1024 * 1024, Duration::from_secs(300), false);
    let msg_id = MessageId::from_borrowed("<auth430@test.com>")?;

    // Record backends as 430 using the public API
    cache
        .record_backend_missing(msg_id.clone(), BackendId::from_index(0))
        .await;
    cache
        .record_backend_missing(msg_id.clone(), BackendId::from_index(1))
        .await;

    // Verify all exhausted
    let cached = cache.get(&msg_id).await.unwrap();
    assert!(cached.all_backends_exhausted(BackendCount::new(2)));

    // Try to claim backend 0 "has" the article (unreliable 2xx response)
    let mut avail = ArticleAvailability::new();
    avail.record_has(BackendId::from_index(0));

    cache.sync_availability(msg_id.clone(), &avail).await;

    // CRITICAL: 430 is authoritative, so the cache should STILL show backend 0 as missing
    // The "has" from sync_availability is unreliable and should NOT override 430
    let updated = cache.get(&msg_id).await.unwrap();
    assert!(
        updated.all_backends_exhausted(BackendCount::new(2)),
        "430 is authoritative - 'has' should NOT override cached 430"
    );
    assert!(
        !updated.should_try_backend(BackendId::from_index(0)),
        "Backend 0 should STILL be marked missing (430 is authoritative)"
    );

    Ok(())
}

/// Test that availability information remains accurate after clearing idle connections
///
/// This is critical: when we clear stale pools, the BackendId → server mapping must
/// remain consistent. Backend 0 after clearing must still be the same server that
/// returned 430 before clearing.
///
/// The test:
/// 1. Set up two backends with different article availability
/// 2. Learn that backend 0 returns 430 for an article, backend 1 has it
/// 3. Clear all connection pools (simulating idle timeout)
/// 4. Verify the availability cache still correctly identifies which backend has the article
/// 5. Make a request and verify it routes to the correct backend (backend 1)
#[tokio::test]
async fn test_availability_survives_pool_clearing() -> Result<()> {
    // Backend 0: Returns 430 for our test article
    let port0 = get_available_port().await?;
    let backend0_requests = Arc::new(AtomicUsize::new(0));
    let backend0_requests_clone = backend0_requests.clone();

    let handle0 = tokio::spawn(async move {
        let listener = TcpListener::bind(format!("127.0.0.1:{}", port0))
            .await
            .unwrap();

        while let Ok((mut stream, _)) = listener.accept().await {
            let requests = backend0_requests_clone.clone();
            tokio::spawn(async move {
                stream.write_all(b"200 Backend0 Ready\r\n").await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();

                while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                    let cmd = line.trim().to_uppercase();
                    let writer = reader.get_mut();

                    if cmd.starts_with("ARTICLE") || cmd.starts_with("STAT") {
                        requests.fetch_add(1, Ordering::SeqCst);
                        // Backend 0 does NOT have this article
                        writer.write_all(b"430 No such article\r\n").await.unwrap();
                    } else if cmd.starts_with("DATE") {
                        writer.write_all(b"111 20251220120000\r\n").await.unwrap();
                    } else if cmd.starts_with("QUIT") {
                        writer.write_all(b"205 Goodbye\r\n").await.unwrap();
                        break;
                    } else {
                        writer.write_all(b"200 OK\r\n").await.unwrap();
                    }
                    line.clear();
                }
            });
        }
    });

    // Backend 1: Has the article
    let port1 = get_available_port().await?;
    let backend1_requests = Arc::new(AtomicUsize::new(0));
    let backend1_requests_clone = backend1_requests.clone();

    let handle1 = tokio::spawn(async move {
        let listener = TcpListener::bind(format!("127.0.0.1:{}", port1))
            .await
            .unwrap();

        while let Ok((mut stream, _)) = listener.accept().await {
            let requests = backend1_requests_clone.clone();
            tokio::spawn(async move {
                stream.write_all(b"200 Backend1 Ready\r\n").await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();

                while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                    let cmd = line.trim().to_uppercase();
                    let writer = reader.get_mut();

                    if cmd.starts_with("ARTICLE") || cmd.starts_with("STAT") {
                        requests.fetch_add(1, Ordering::SeqCst);
                        // Backend 1 HAS this article
                        writer
                            .write_all(b"223 0 <pool-test@example.com> article exists\r\n")
                            .await
                            .unwrap();
                    } else if cmd.starts_with("DATE") {
                        writer.write_all(b"111 20251220120000\r\n").await.unwrap();
                    } else if cmd.starts_with("QUIT") {
                        writer.write_all(b"205 Goodbye\r\n").await.unwrap();
                        break;
                    } else {
                        writer.write_all(b"200 OK\r\n").await.unwrap();
                    }
                    line.clear();
                }
            });
        }
    });

    wait_for_server(&format!("127.0.0.1:{}", port0), 20).await?;
    wait_for_server(&format!("127.0.0.1:{}", port1), 20).await?;

    // Create proxy with both backends - MUST enable cache for availability tracking!
    let config = Config {
        servers: vec![
            create_test_server_config("127.0.0.1", port0, "Backend0-430"),
            create_test_server_config("127.0.0.1", port1, "Backend1-Has"),
        ],
        cache: Some(Cache {
            adaptive_precheck: false, // Don't precheck, let the request go through normally
            ..Default::default()
        }),
        ..Default::default()
    };

    let proxy = Arc::new(NntpProxy::new(config, RoutingMode::PerCommand).await?);

    // Phase 1: Learn availability by making a request
    // The proxy will try backend 0 first, get 430, then try backend 1, get success
    let proxy_port = get_available_port().await?;
    let proxy_addr = format!("127.0.0.1:{}", proxy_port);
    let listener = TcpListener::bind(&proxy_addr).await?;

    // Spawn proxy accept loop that handles multiple connections
    tokio::spawn({
        let proxy = proxy.clone();
        async move {
            while let Ok((stream, addr)) = listener.accept().await {
                let p = proxy.clone();
                tokio::spawn(async move {
                    let _ = p
                        .handle_client_per_command_routing(stream, addr.into())
                        .await;
                });
            }
        }
    });

    wait_for_server(&proxy_addr, 20).await?;

    {
        let mut client = TcpStream::connect(&proxy_addr).await?;
        let mut reader = BufReader::new(&mut client);
        let mut line = String::new();

        // Read greeting
        reader.read_line(&mut line).await?;
        line.clear();

        // Request article - this learns availability
        reader
            .get_mut()
            .write_all(b"STAT <pool-test@example.com>\r\n")
            .await?;
        reader.read_line(&mut line).await?;
        assert!(
            line.starts_with("223"),
            "Should get article from backend 1 after backend 0 returns 430"
        );

        reader.get_mut().write_all(b"QUIT\r\n").await?;
    }

    // Verify backend 0 was tried (got 430)
    assert!(
        backend0_requests.load(Ordering::SeqCst) >= 1,
        "Backend 0 should have been tried"
    );
    let backend1_before_clear = backend1_requests.load(Ordering::SeqCst);
    assert!(
        backend1_before_clear >= 1,
        "Backend 1 should have been tried and succeeded"
    );

    // Phase 2: Clear all connection pools (simulating idle timeout)
    // This is what happens after 5 minutes of inactivity
    for provider in proxy.connection_providers() {
        provider.clear_idle_connections();
    }

    // Reset request counters to track new requests
    backend0_requests.store(0, Ordering::SeqCst);
    backend1_requests.store(0, Ordering::SeqCst);

    // Phase 3: Make another request for the SAME article
    // The availability cache should remember that backend 0 returned 430
    // So it should go DIRECTLY to backend 1 without trying backend 0 again
    {
        let mut client = TcpStream::connect(&proxy_addr).await?;
        let mut reader = BufReader::new(&mut client);
        let mut line = String::new();

        // Read greeting
        reader.read_line(&mut line).await?;
        line.clear();

        // Request the SAME article again
        reader
            .get_mut()
            .write_all(b"STAT <pool-test@example.com>\r\n")
            .await?;
        reader.read_line(&mut line).await?;

        // CRITICAL: Should still get 223 (success) from backend 1
        assert!(
            line.starts_with("223"),
            "After pool clear, should still get article from correct backend. Got: {}",
            line.trim()
        );

        reader.get_mut().write_all(b"QUIT\r\n").await?;
    }

    // CRITICAL ASSERTION: Backend 0 should NOT have been tried again
    // because the availability cache remembers it returned 430
    let backend0_after_clear = backend0_requests.load(Ordering::SeqCst);
    assert_eq!(
        backend0_after_clear, 0,
        "Backend 0 should NOT be tried again - availability cache should remember it returned 430"
    );

    // Backend 1 should have been tried (and succeeded)
    let backend1_after_clear = backend1_requests.load(Ordering::SeqCst);
    assert!(
        backend1_after_clear >= 1,
        "Backend 1 should have been tried after pool clear"
    );

    handle0.abort();
    handle1.abort();

    Ok(())
}
