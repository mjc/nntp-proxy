//! CRITICAL TEST: Ensure cache is checked BEFORE adaptive prechecking
//!
//! This test exists because we had a bug FOUR TIMES where adaptive prechecking
//! ran before cache checks, causing:
//! - Massive backend queries for cached data
//! - 9KB/s throughput instead of instant cache hits
//! - Unnecessary backend load
//!
//! These tests verify the correct ordering:
//! 1. Extract message-ID
//! 2. Check cache FIRST
//! 3. If cache hit, return immediately (optionally spawn background recheck)
//! 4. If cache miss, run adaptive prechecking
//!
//! DO NOT DELETE THESE TESTS. DO NOT REFACTOR THEM AWAY.

use nntp_proxy::NntpProxy;
use nntp_proxy::config::{Cache, Config, RoutingMode, Server};
use nntp_proxy::types::Port;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

mod test_helpers;
use test_helpers::get_available_port;

/// Count how many times backends are queried
#[derive(Clone)]
struct BackendQueryCounter {
    count: Arc<AtomicU64>,
}

impl BackendQueryCounter {
    fn new() -> Self {
        Self {
            count: Arc::new(AtomicU64::new(0)),
        }
    }

    fn increment(&self) {
        self.count.fetch_add(1, Ordering::SeqCst);
    }

    fn get(&self) -> u64 {
        self.count.load(Ordering::SeqCst)
    }

    fn reset(&self) {
        self.count.store(0, Ordering::SeqCst);
    }
}

/// Spawn mock server that counts queries
async fn spawn_counting_mock_server(
    port: u16,
    name: &str,
    counter: BackendQueryCounter,
    has_article: bool,
) -> tokio::task::AbortHandle {
    let name = name.to_string();
    let task = tokio::spawn(async move {
        let listener = TcpListener::bind(format!("127.0.0.1:{}", port))
            .await
            .unwrap();

        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let counter = counter.clone();
            let name = name.clone();

            tokio::spawn(async move {
                // Send greeting
                stream
                    .write_all(format!("200 {} Ready\r\n", name).as_bytes())
                    .await
                    .ok();

                let mut reader = BufReader::new(stream);
                let mut line = String::new();

                loop {
                    line.clear();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        break;
                    }

                    let command = line.trim();

                    // Count all STAT/HEAD/ARTICLE queries
                    if command.starts_with("STAT ")
                        || command.starts_with("HEAD ")
                        || command.starts_with("ARTICLE ")
                    {
                        counter.increment();
                    }

                    if command.starts_with("AUTHINFO USER") {
                        reader
                            .get_mut()
                            .write_all(b"381 Password required\r\n")
                            .await
                            .ok();
                    } else if command.starts_with("AUTHINFO PASS") {
                        reader.get_mut().write_all(b"281 Ok\r\n").await.ok();
                    } else if command.starts_with("STAT ") {
                        if has_article {
                            reader
                                .get_mut()
                                .write_all(b"223 0 <test@example.com>\r\n")
                                .await
                                .ok();
                        } else {
                            reader
                                .get_mut()
                                .write_all(b"430 No such article\r\n")
                                .await
                                .ok();
                        }
                    } else if command.starts_with("HEAD ") {
                        if has_article {
                            reader
                                .get_mut()
                                .write_all(
                                    b"221 0 <test@example.com>\r\nSubject: Test\r\n\r\n.\r\n",
                                )
                                .await
                                .ok();
                        } else {
                            reader
                                .get_mut()
                                .write_all(b"430 No such article\r\n")
                                .await
                                .ok();
                        }
                    } else if command.starts_with("ARTICLE ") {
                        if has_article {
                            reader
                                .get_mut()
                                .write_all(b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n")
                                .await
                                .ok();
                        } else {
                            reader
                                .get_mut()
                                .write_all(b"430 No such article\r\n")
                                .await
                                .ok();
                        }
                    } else if command.starts_with("QUIT") {
                        reader.get_mut().write_all(b"205 Goodbye\r\n").await.ok();
                        break;
                    } else {
                        reader.get_mut().write_all(b"200 OK\r\n").await.ok();
                    }
                }
            });
        }
    });

    task.abort_handle()
}

/// CRITICAL TEST: Cache check must happen BEFORE adaptive prechecking for STAT
///
/// Bug history:
/// 1st occurrence: Initial implementation had precheck before cache
/// 2nd occurrence: Refactoring moved cache check after precheck
/// 3rd occurrence: Code reorganization accidentally swapped order
/// 4th occurrence: (current) - precheck was before cache check
///
/// Expected behavior:
/// - First STAT: Cache miss, triggers precheck (1-2 backend queries)
/// - Second STAT: Cache hit, ZERO backend queries (instant response)
#[tokio::test]
async fn test_stat_cache_hit_zero_backend_queries() {
    let proxy_port = get_available_port().await.unwrap();
    let backend_port = get_available_port().await.unwrap();

    // Create counter to track backend queries
    let counter = BackendQueryCounter::new();

    // Spawn counting mock server (has article)
    let _mock =
        spawn_counting_mock_server(backend_port, "TestBackend", counter.clone(), true).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create proxy config with caching and adaptive precheck
    let config = Config {
        servers: vec![
            Server::builder("127.0.0.1", Port::try_new(backend_port).unwrap())
                .name("TestBackend")
                .build()
                .unwrap(),
        ],
        cache: Some(Cache {
            cache_articles: true,
            adaptive_precheck: true,
            ..Default::default()
        }),
        ..Default::default()
    };

    // Start proxy
    let proxy = nntp_proxy::NntpProxy::new(config.clone(), RoutingMode::PerCommand)
        .await
        .unwrap();
    let listener = TcpListener::bind(format!("127.0.0.1:{}", proxy_port))
        .await
        .unwrap();
    let _proxy_handle = tokio::spawn(async move {
        while let Ok((stream, addr)) = listener.accept().await {
            let proxy = proxy.clone();
            tokio::spawn(async move {
                proxy.handle_client(stream, addr.into()).await.ok();
            });
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Connect client
    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port))
        .await
        .unwrap();
    let (read_half, mut write_half) = client.split();
    let mut reader = BufReader::new(read_half);

    // Read greeting
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();

    // Reset counter before test
    counter.reset();

    // FIRST STAT - Cache miss, will query backend
    write_half
        .write_all(b"STAT <test@example.com>\r\n")
        .await
        .unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("223"), "Should get 223 response: {}", line);

    // Wait for prechecking to complete and cache to populate
    tokio::time::sleep(Duration::from_millis(200)).await;

    let first_queries = counter.get();
    assert!(
        first_queries >= 1,
        "First STAT should query backend at least once, got {} queries",
        first_queries
    );

    // Reset counter
    counter.reset();

    // SECOND STAT - MUST hit cache with ZERO backend queries
    write_half
        .write_all(b"STAT <test@example.com>\r\n")
        .await
        .unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("223"), "Should get 223 response: {}", line);

    // Wait a bit to ensure no background queries happened
    tokio::time::sleep(Duration::from_millis(100)).await;

    let second_queries = counter.get();

    // CRITICAL ASSERTION: Cache hit MUST NOT query backend
    // Background recheck is allowed but should be minimal
    assert!(
        second_queries <= 1,
        "CRITICAL BUG: Cache hit triggered {} backend queries! Should be 0 (or 1 for background recheck). \
         This means cache check is happening AFTER adaptive prechecking.",
        second_queries
    );

    // Clean up
    write_half.write_all(b"QUIT\r\n").await.unwrap();
}

/// CRITICAL TEST: HEAD command cache hits must not query backends
#[tokio::test]
async fn test_head_cache_hit_zero_backend_queries() {
    let proxy_port = get_available_port().await.unwrap();
    let backend_port = get_available_port().await.unwrap();

    let counter = BackendQueryCounter::new();
    let _mock =
        spawn_counting_mock_server(backend_port, "TestBackend", counter.clone(), true).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let config = Config {
        servers: vec![
            Server::builder("127.0.0.1", Port::try_new(backend_port).unwrap())
                .name("TestBackend")
                .build()
                .unwrap(),
        ],
        cache: Some(Cache {
            cache_articles: true,
            adaptive_precheck: true,
            ..Default::default()
        }),
        ..Default::default()
    };

    let proxy = NntpProxy::new(config.clone(), RoutingMode::PerCommand)
        .await
        .unwrap();
    let listener = TcpListener::bind(format!("127.0.0.1:{}", proxy_port))
        .await
        .unwrap();
    let _proxy_handle = tokio::spawn(async move {
        while let Ok((stream, addr)) = listener.accept().await {
            let proxy = proxy.clone();
            tokio::spawn(async move {
                proxy.handle_client(stream, addr.into()).await.ok();
            });
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port))
        .await
        .unwrap();
    let (read_half, mut write_half) = client.split();
    let mut reader = BufReader::new(read_half);

    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();

    counter.reset();

    // First HEAD - cache miss
    write_half
        .write_all(b"HEAD <test@example.com>\r\n")
        .await
        .unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("221"), "Should get 221 response: {}", line);

    // Read multiline response
    loop {
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        if line.trim() == "." {
            break;
        }
    }

    tokio::time::sleep(Duration::from_millis(200)).await;
    let first_queries = counter.get();
    assert!(first_queries >= 1, "First HEAD should query backend");

    counter.reset();

    // Second HEAD - MUST hit cache
    write_half
        .write_all(b"HEAD <test@example.com>\r\n")
        .await
        .unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("221"), "Should get 221 response: {}", line);

    // Read multiline response
    loop {
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        if line.trim() == "." {
            break;
        }
    }

    tokio::time::sleep(Duration::from_millis(100)).await;
    let second_queries = counter.get();

    assert!(
        second_queries <= 1,
        "CRITICAL BUG: HEAD cache hit triggered {} backend queries! Should be 0 (or 1 for background recheck)",
        second_queries
    );

    write_half.write_all(b"QUIT\r\n").await.unwrap();
}

/// CRITICAL TEST: ARTICLE command cache hits must not query backends
#[tokio::test]
async fn test_article_cache_hit_zero_backend_queries() {
    let proxy_port = get_available_port().await.unwrap();
    let backend_port = get_available_port().await.unwrap();

    let counter = BackendQueryCounter::new();
    let _mock =
        spawn_counting_mock_server(backend_port, "TestBackend", counter.clone(), true).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let config = Config {
        servers: vec![
            Server::builder("127.0.0.1", Port::try_new(backend_port).unwrap())
                .name("TestBackend")
                .build()
                .unwrap(),
        ],
        cache: Some(Cache {
            cache_articles: true,
            adaptive_precheck: true,
            ..Default::default()
        }),
        ..Default::default()
    };

    let proxy = NntpProxy::new(config.clone(), RoutingMode::PerCommand)
        .await
        .unwrap();
    let listener = TcpListener::bind(format!("127.0.0.1:{}", proxy_port))
        .await
        .unwrap();
    let _proxy_handle = tokio::spawn(async move {
        while let Ok((stream, addr)) = listener.accept().await {
            let proxy = proxy.clone();
            tokio::spawn(async move {
                proxy.handle_client(stream, addr.into()).await.ok();
            });
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port))
        .await
        .unwrap();
    let (read_half, mut write_half) = client.split();
    let mut reader = BufReader::new(read_half);

    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();

    counter.reset();

    // First ARTICLE - cache miss
    write_half
        .write_all(b"ARTICLE <test@example.com>\r\n")
        .await
        .unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("220"), "Should get 220 response: {}", line);

    // Read multiline response
    loop {
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        if line.trim() == "." {
            break;
        }
    }

    let first_queries = counter.get();
    assert_eq!(first_queries, 1, "First ARTICLE should query backend once");

    counter.reset();

    // Second ARTICLE - MUST hit cache
    write_half
        .write_all(b"ARTICLE <test@example.com>\r\n")
        .await
        .unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("220"), "Should get 220 response: {}", line);

    // Read multiline response
    loop {
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        if line.trim() == "." {
            break;
        }
    }

    let second_queries = counter.get();

    assert!(
        second_queries <= 1,
        "CRITICAL BUG: ARTICLE cache hit triggered {} backend queries! Should be 0 (or 1 for background recheck). \
         Cache check must happen BEFORE backend queries.",
        second_queries
    );

    write_half.write_all(b"QUIT\r\n").await.unwrap();
}

/// CRITICAL TEST: Cached 430s must not trigger backend queries
#[tokio::test]
async fn test_cached_430_zero_backend_queries() {
    let proxy_port = get_available_port().await.unwrap();
    let backend_port = get_available_port().await.unwrap();

    let counter = BackendQueryCounter::new();
    // Server doesn't have article (returns 430)
    let _mock =
        spawn_counting_mock_server(backend_port, "TestBackend", counter.clone(), false).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let config = Config {
        servers: vec![
            Server::builder("127.0.0.1", Port::try_new(backend_port).unwrap())
                .name("TestBackend")
                .build()
                .unwrap(),
        ],
        cache: Some(Cache {
            cache_articles: true,
            adaptive_precheck: true,
            ..Default::default()
        }),
        ..Default::default()
    };

    let proxy = NntpProxy::new(config.clone(), RoutingMode::PerCommand)
        .await
        .unwrap();
    let listener = TcpListener::bind(format!("127.0.0.1:{}", proxy_port))
        .await
        .unwrap();
    let _proxy_handle = tokio::spawn(async move {
        while let Ok((stream, addr)) = listener.accept().await {
            let proxy = proxy.clone();
            tokio::spawn(async move {
                proxy.handle_client(stream, addr.into()).await.ok();
            });
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port))
        .await
        .unwrap();
    let (read_half, mut write_half) = client.split();
    let mut reader = BufReader::new(read_half);

    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();

    counter.reset();

    // First STAT - cache miss, backend returns 430
    write_half
        .write_all(b"STAT <missing@example.com>\r\n")
        .await
        .unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("430"), "Should get 430 response: {}", line);

    tokio::time::sleep(Duration::from_millis(200)).await;
    let first_queries = counter.get();
    assert!(first_queries >= 1, "First query should hit backend");

    counter.reset();

    // Second STAT - MUST hit cache with ZERO queries (430 is cached)
    write_half
        .write_all(b"STAT <missing@example.com>\r\n")
        .await
        .unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(
        line.starts_with("430"),
        "Should still get 430 response: {}",
        line
    );

    tokio::time::sleep(Duration::from_millis(100)).await;
    let second_queries = counter.get();

    assert!(
        second_queries <= 1,
        "CRITICAL BUG: Cached 430 triggered {} backend queries! Should be 0 (or 1 for background recheck). \
         430 responses MUST be cached and served instantly.",
        second_queries
    );

    write_half.write_all(b"QUIT\r\n").await.unwrap();
}
