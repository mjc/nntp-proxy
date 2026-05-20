//! Tests for backend availability tracking edge cases
//!
//! This test suite verifies edge cases in backend availability tracking:
//! - Backend comes back online after being marked unavailable
//! - Cache TTL expiration
//! - All backends exhausted
//! - Availability bitset edge cases

use anyhow::Result;
use nntp_proxy::NntpProxy;
use nntp_proxy::RoutingMode;
use nntp_proxy::config::{BackendSelectionStrategy, Cache, Config};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::test_helpers::{
    connect_and_read_greeting, create_test_server_config, send_article_read_multiline_response,
    setup_proxy_with_backends, spawn_test_proxy_on_random_port, wait_for_server,
};

async fn read_line(stream: &mut tokio::net::TcpStream, context: &str) -> Result<String> {
    let mut bytes = Vec::with_capacity(128);
    let mut byte = [0u8; 1];

    loop {
        let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut byte)).await??;
        if n == 0 {
            anyhow::bail!("Connection closed while reading {context}");
        }

        bytes.push(byte[0]);
        if byte[0] == b'\n' {
            return String::from_utf8(bytes).map_err(Into::into);
        }
    }
}

async fn spawn_counting_backend(
    name: &'static str,
    has_article: bool,
    slow_body: bool,
) -> Result<(u16, Arc<AtomicUsize>, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let requests = Arc::new(AtomicUsize::new(0));
    let request_count = Arc::clone(&requests);

    let handle = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let requests = Arc::clone(&request_count);
            tokio::spawn(async move {
                if stream
                    .write_all(format!("200 {name} Ready\r\n").as_bytes())
                    .await
                    .is_err()
                {
                    return;
                }

                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                    let cmd = line.trim().to_uppercase();
                    let writer = reader.get_mut();

                    if cmd.starts_with("BODY")
                        || cmd.starts_with("ARTICLE")
                        || cmd.starts_with("STAT")
                    {
                        requests.fetch_add(1, Ordering::SeqCst);
                        if !has_article {
                            if writer.write_all(b"430 No such article\r\n").await.is_err() {
                                break;
                            }
                        } else if cmd.starts_with("STAT") {
                            if writer
                                .write_all(
                                    b"223 0 <disconnect-sync@example.com> article exists\r\n",
                                )
                                .await
                                .is_err()
                            {
                                break;
                            }
                        } else {
                            if writer
                                .write_all(b"222 0 <disconnect-sync@example.com> body follows\r\n")
                                .await
                                .is_err()
                            {
                                break;
                            }
                            if slow_body {
                                tokio::time::sleep(Duration::from_millis(25)).await;
                            }
                            for _ in 0..256 {
                                if writer
                                    .write_all(b"body line body line body line body line\r\n")
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                                if slow_body {
                                    tokio::time::sleep(Duration::from_millis(1)).await;
                                }
                            }
                            let _ = writer.write_all(b".\r\n").await;
                        }
                    } else if cmd.starts_with("DATE") {
                        let _ = writer.write_all(b"111 20260520000000\r\n").await;
                    } else if cmd.starts_with("QUIT") {
                        let _ = writer.write_all(b"205 Goodbye\r\n").await;
                        break;
                    } else {
                        let _ = writer.write_all(b"200 OK\r\n").await;
                    }
                    line.clear();
                }
            });
        }
    });

    Ok((port, requests, handle))
}

#[tokio::test]
async fn test_all_backends_exhausted_returns_430() -> Result<()> {
    // When all backends return 430, proxy should return 430 to client
    let (proxy_port, _backend_ports, _mock_handles) = setup_proxy_with_backends(
        vec![
            ("Backend1", false),
            ("Backend2", false),
            ("Backend3", false),
        ],
        RoutingMode::PerCommand,
    )
    .await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;

    let (status, body) =
        send_article_read_multiline_response(&mut client, "<missing@example.com>").await?;

    assert!(
        status.starts_with("430"),
        "Should return 430 when all backends exhausted"
    );
    assert!(body.is_empty(), "430 should have no body");

    Ok(())
}

#[tokio::test]
async fn test_single_backend_immediate_430() -> Result<()> {
    // With only one backend that returns 430, should immediately return 430
    let (proxy_port, _backend_ports, _mock_handles) =
        setup_proxy_with_backends(vec![("OnlyBackend", false)], RoutingMode::PerCommand).await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;

    let (status, body) =
        send_article_read_multiline_response(&mut client, "<only-backend@example.com>").await?;

    assert!(
        status.starts_with("430"),
        "Single backend 430 should return immediately"
    );
    assert!(body.is_empty());

    Ok(())
}

#[tokio::test]
async fn test_availability_learned_across_requests() -> Result<()> {
    // Test that availability info is preserved across multiple requests
    let (proxy_port, _backend_ports, _mock_handles) = setup_proxy_with_backends(
        vec![
            ("Backend1", false), // Never has articles
            ("Backend2", true),  // Always has articles
        ],
        RoutingMode::PerCommand,
    )
    .await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;

    // First request learns Backend1=430, Backend2=220
    let (status1, _) =
        send_article_read_multiline_response(&mut client, "<first@example.com>").await?;
    assert!(status1.starts_with("220"), "First request should succeed");

    // Second request should use learned availability
    let (status2, _) =
        send_article_read_multiline_response(&mut client, "<second@example.com>").await?;
    assert!(
        status2.starts_with("220"),
        "Second request uses learned availability"
    );

    // Third request for different article
    let (status3, _) =
        send_article_read_multiline_response(&mut client, "<third@example.com>").await?;
    assert!(
        status3.starts_with("220"),
        "Third request uses learned availability"
    );

    Ok(())
}

#[tokio::test]
async fn test_availability_learned_before_client_disconnect_is_persisted() -> Result<()> {
    let (port0, backend0_requests, handle0) =
        spawn_counting_backend("Backend0-430", false, false).await?;
    let (port1, backend1_requests, handle1) =
        spawn_counting_backend("Backend1-430", false, false).await?;
    let (port2, backend2_requests, handle2) =
        spawn_counting_backend("Backend2-Has", true, true).await?;

    wait_for_server(&format!("127.0.0.1:{port0}"), 20).await?;
    wait_for_server(&format!("127.0.0.1:{port1}"), 20).await?;
    wait_for_server(&format!("127.0.0.1:{port2}"), 20).await?;

    let config = Config {
        servers: vec![
            create_test_server_config("127.0.0.1", port0, "Backend0-430"),
            create_test_server_config("127.0.0.1", port1, "Backend1-430"),
            create_test_server_config("127.0.0.1", port2, "Backend2-Has"),
        ],
        cache: Some(Cache {
            adaptive_precheck: false,
            ..Default::default()
        }),
        routing: nntp_proxy::config::Routing {
            backend_selection: BackendSelectionStrategy::WeightedRoundRobin,
            ..Default::default()
        },
        ..Default::default()
    };

    let proxy = NntpProxy::new(config, RoutingMode::PerCommand).await?;
    let proxy_port = spawn_test_proxy_on_random_port(proxy, true).await?;
    let proxy_addr = format!("127.0.0.1:{proxy_port}");
    wait_for_server(&proxy_addr, 20).await?;

    {
        let mut client = TcpStream::connect(&proxy_addr).await?;
        let mut greeting = String::new();
        let mut reader = BufReader::new(&mut client);
        reader.read_line(&mut greeting).await?;

        reader
            .get_mut()
            .write_all(b"BODY <disconnect-sync@example.com>\r\n")
            .await?;
        let mut status = String::new();
        reader.read_line(&mut status).await?;
        assert!(
            status.starts_with("222"),
            "first request should reach backend 2 before disconnect, got: {status}"
        );
    }

    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(backend0_requests.load(Ordering::SeqCst), 1);
    assert_eq!(backend1_requests.load(Ordering::SeqCst), 1);
    assert_eq!(backend2_requests.load(Ordering::SeqCst), 1);

    backend0_requests.store(0, Ordering::SeqCst);
    backend1_requests.store(0, Ordering::SeqCst);
    backend2_requests.store(0, Ordering::SeqCst);

    {
        let mut client = TcpStream::connect(&proxy_addr).await?;
        let mut greeting = String::new();
        let mut reader = BufReader::new(&mut client);
        reader.read_line(&mut greeting).await?;

        reader
            .get_mut()
            .write_all(b"STAT <disconnect-sync@example.com>\r\n")
            .await?;
        let mut status = String::new();
        reader.read_line(&mut status).await?;
        assert!(
            status.starts_with("223"),
            "second request should route directly to backend 2, got: {status}"
        );
    }

    assert_eq!(
        backend0_requests.load(Ordering::SeqCst),
        0,
        "backend 0 miss learned before disconnect should be persisted"
    );
    assert_eq!(
        backend1_requests.load(Ordering::SeqCst),
        0,
        "backend 1 miss learned before disconnect should be persisted"
    );
    assert_eq!(backend2_requests.load(Ordering::SeqCst), 1);

    handle0.abort();
    handle1.abort();
    handle2.abort();

    Ok(())
}

#[tokio::test]
async fn test_partial_backend_availability() -> Result<()> {
    // Test with 3 backends where article exists on middle backend only
    let (proxy_port, _backend_ports, _mock_handles) = setup_proxy_with_backends(
        vec![
            ("Backend1", false),
            ("Backend2", true), // Only this one has it
            ("Backend3", false),
        ],
        RoutingMode::PerCommand,
    )
    .await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;

    let (status, body) =
        send_article_read_multiline_response(&mut client, "<partial@example.com>").await?;

    assert!(status.starts_with("220"), "Should find article on Backend2");
    assert!(!body.is_empty());

    Ok(())
}

#[tokio::test]
async fn test_availability_different_per_article() -> Result<()> {
    // Test that different articles can have different backend availability
    let (proxy_port, _backend_ports, _mock_handles) = setup_proxy_with_backends(
        vec![
            ("Backend1", true), // Has some articles
            ("Backend2", true), // Has some articles
        ],
        RoutingMode::PerCommand,
    )
    .await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;

    // Request multiple articles - each might be on different backends
    for i in 0..5 {
        let msgid = format!("<article-{i}@example.com>");
        let (status, _) = send_article_read_multiline_response(&mut client, &msgid).await?;

        // Each article should either succeed or fail (no partial state)
        assert!(
            status.starts_with("220") || status.starts_with("430"),
            "Article {i} should have valid status"
        );
    }

    Ok(())
}

#[tokio::test]
async fn test_retry_stops_after_all_backends_tried() -> Result<()> {
    // Verify retry logic doesn't loop infinitely - stops after trying all backends
    let (proxy_port, _backend_ports, _mock_handles) = setup_proxy_with_backends(
        vec![
            ("Backend1", false),
            ("Backend2", false),
            ("Backend3", false),
            ("Backend4", false),
        ],
        RoutingMode::PerCommand,
    )
    .await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;

    // Should try all 4 backends then return 430
    let start = std::time::Instant::now();
    let (status, _) =
        send_article_read_multiline_response(&mut client, "<retry@example.com>").await?;
    let duration = start.elapsed();

    assert!(
        status.starts_with("430"),
        "Should return 430 after trying all backends"
    );

    // Should complete relatively quickly (not infinite loop)
    assert!(
        duration < Duration::from_secs(5),
        "Should not take more than 5 seconds"
    );

    Ok(())
}

#[tokio::test]
async fn test_pipelined_missing_articles_fall_back_to_430_responses() -> Result<()> {
    let (proxy_port, _backend_ports, _mock_handles) = setup_proxy_with_backends(
        vec![("Backend1", false), ("Backend2", false)],
        RoutingMode::PerCommand,
    )
    .await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;

    let (status, body) =
        send_article_read_multiline_response(&mut client, "<cached-missing@example.com>").await?;
    assert!(status.starts_with("430"));
    assert!(body.is_empty());

    client
        .write_all(
            b"ARTICLE <cached-missing@example.com>\r\nARTICLE <cached-missing@example.com>\r\n",
        )
        .await?;
    client.flush().await?;

    let first = read_line(&mut client, "first pipelined ARTICLE response").await?;
    let second = read_line(&mut client, "second pipelined ARTICLE response").await?;

    assert!(
        first.starts_with("430"),
        "Expected 430 for first pipelined cached-missing ARTICLE, got: {first}"
    );
    assert!(
        second.starts_with("430"),
        "Expected 430 for second pipelined cached-missing ARTICLE, got: {second}"
    );

    client.write_all(b"QUIT\r\n").await?;

    Ok(())
}

#[tokio::test]
async fn test_availability_works_in_hybrid_mode() -> Result<()> {
    // Test that backend availability tracking works in hybrid mode too
    let (proxy_port, _backend_ports, _mock_handles) = setup_proxy_with_backends(
        vec![("Backend1", false), ("Backend2", true)],
        RoutingMode::Hybrid,
    )
    .await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;

    // Hybrid mode should still use availability tracking for stateless commands
    let (status, body) =
        send_article_read_multiline_response(&mut client, "<hybrid@example.com>").await?;

    assert!(
        status.starts_with("220"),
        "Hybrid mode should find article after retry"
    );
    assert!(!body.is_empty());

    Ok(())
}

#[tokio::test]
async fn test_maximum_backends_bitset_limit() -> Result<()> {
    // Test with maximum supported backends (8 = sizeof(u8) bits)
    // This tests the bitset limit
    let (proxy_port, _backend_ports, _mock_handles) = setup_proxy_with_backends(
        vec![
            ("Backend1", false),
            ("Backend2", false),
            ("Backend3", false),
            ("Backend4", false),
            ("Backend5", false),
            ("Backend6", false),
            ("Backend7", false),
            ("Backend8", true), // Only last one has it
        ],
        RoutingMode::PerCommand,
    )
    .await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;

    let (status, body) =
        send_article_read_multiline_response(&mut client, "<max-backends@example.com>").await?;

    assert!(
        status.starts_with("220"),
        "Should find article on 8th backend"
    );
    assert!(!body.is_empty());

    Ok(())
}

#[tokio::test]
async fn test_concurrent_requests_same_article() -> Result<()> {
    // Test multiple concurrent requests for the same article
    let (proxy_port, _backend_ports, _mock_handles) = setup_proxy_with_backends(
        vec![("Backend1", false), ("Backend2", true)],
        RoutingMode::PerCommand,
    )
    .await?;

    // Create multiple clients
    let mut clients = Vec::new();
    for _ in 0..3 {
        let client = connect_and_read_greeting(proxy_port).await?;
        clients.push(client);
    }

    let msgid = "<concurrent@example.com>";

    // Send requests from all clients concurrently
    let mut tasks = Vec::new();
    for mut client in clients {
        let msgid = msgid.to_string();
        tasks.push(tokio::spawn(async move {
            send_article_read_multiline_response(&mut client, &msgid).await
        }));
    }

    // Wait for all to complete
    for task in tasks {
        let result = task.await??;
        assert!(
            result.0.starts_with("220"),
            "All concurrent requests should succeed"
        );
    }

    Ok(())
}
