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

use crate::test_helpers::{connect_and_read_greeting, spawn_proxy_with_config};
use nntp_proxy::config::{Cache, Config, RoutingMode, Server};
use nntp_proxy::types::Port;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::net::{
    TcpListener,
    tcp::{OwnedReadHalf, OwnedWriteHalf},
};
use tokio::sync::Notify;
use tokio::time::timeout;

async fn read_multiline_response<R>(reader: &mut BufReader<R>) -> String
where
    R: AsyncRead + Unpin,
{
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("220"), "Should get 220 response: {line}");
    let status_line = line.clone();

    loop {
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        if line.trim() == "." {
            break;
        }
    }

    status_line
}

async fn yield_background_work() {
    for _ in 0..128 {
        tokio::task::yield_now().await;
    }
}

/// Count how many times backends are queried
#[derive(Clone)]
struct BackendQueryCounter {
    count: Arc<AtomicU64>,
    updates: Arc<Notify>,
}

impl BackendQueryCounter {
    fn new() -> Self {
        Self {
            count: Arc::new(AtomicU64::new(0)),
            updates: Arc::new(Notify::new()),
        }
    }

    fn increment(&self) {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.updates.notify_waiters();
    }

    fn get(&self) -> u64 {
        self.count.load(Ordering::SeqCst)
    }

    fn reset(&self) {
        self.count.store(0, Ordering::SeqCst);
    }

    async fn wait_for_at_least(&self, expected: u64, within: Duration) {
        timeout(within, async {
            loop {
                let notified = self.updates.notified();
                if self.get() >= expected {
                    return;
                }
                notified.await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!("Timed out waiting for backend query count to reach {expected}")
        });
    }

    async fn assert_stays_at_most(&self, max_allowed: u64, within: Duration) {
        assert!(
            self.get() <= max_allowed,
            "Backend query count already exceeded limit: {} > {}",
            self.get(),
            max_allowed
        );

        let result = timeout(within, async {
            loop {
                let notified = self.updates.notified();
                let current = self.get();
                assert!(
                    current <= max_allowed,
                    "Backend query count exceeded limit: {current} > {max_allowed}"
                );
                notified.await;
            }
        })
        .await;

        assert!(
            result.is_err(),
            "Backend query count kept changing while verifying limit <= {max_allowed}"
        );
    }
}

/// Spawn mock server that counts queries
fn spawn_counting_mock_server(
    listener: TcpListener,
    name: &str,
    counter: BackendQueryCounter,
    has_article: bool,
) -> tokio::task::AbortHandle {
    let name = name.to_string();
    let task = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let counter = counter.clone();
            let name = name.clone();

            tokio::spawn(async move {
                // Send greeting
                stream
                    .write_all(format!("200 {name} Ready\r\n").as_bytes())
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

async fn connect_test_client(config: Config) -> (BufReader<OwnedReadHalf>, OwnedWriteHalf) {
    let proxy_port = spawn_proxy_with_config(config, RoutingMode::PerCommand)
        .await
        .unwrap();
    let client = connect_and_read_greeting(proxy_port).await.unwrap();
    let (read_half, write_half) = client.into_split();
    (BufReader::new(read_half), write_half)
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
    let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_port = backend_listener.local_addr().unwrap().port();

    // Create counter to track backend queries
    let counter = BackendQueryCounter::new();

    // Spawn counting mock server (has article)
    let _mock = spawn_counting_mock_server(backend_listener, "TestBackend", counter.clone(), true);

    // Create proxy config with caching and adaptive precheck
    let config = Config {
        servers: vec![
            Server::builder("127.0.0.1", Port::try_new(backend_port).unwrap())
                .name("TestBackend")
                .build()
                .unwrap(),
        ],
        cache: Some(Cache {
            store_article_bodies: true,
            adaptive_precheck: true,
            ..Default::default()
        }),
        ..Default::default()
    };

    let (mut reader, mut write_half) = connect_test_client(config).await;
    let mut line = String::new();

    // Reset counter before test
    counter.reset();

    // FIRST STAT - Cache miss, will query backend
    write_half
        .write_all(b"STAT <test@example.com>\r\n")
        .await
        .unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("223"), "Should get 223 response: {line}");

    counter.wait_for_at_least(1, Duration::from_secs(1)).await;

    let first_queries = counter.get();
    assert!(
        first_queries >= 1,
        "First STAT should query backend at least once, got {first_queries} queries"
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
    assert!(line.starts_with("223"), "Should get 223 response: {line}");

    counter
        .assert_stays_at_most(1, Duration::from_millis(200))
        .await;

    let second_queries = counter.get();

    // CRITICAL ASSERTION: Cache hit MUST NOT query backend
    // Background recheck is allowed but should be minimal
    assert!(
        second_queries <= 1,
        "CRITICAL BUG: Cache hit triggered {second_queries} backend queries! Should be 0 (or 1 for background recheck). \
         This means cache check is happening AFTER adaptive prechecking."
    );

    // Clean up
    write_half.write_all(b"QUIT\r\n").await.unwrap();
}

/// CRITICAL TEST: HEAD command cache hits must not query backends
#[tokio::test]
async fn test_head_cache_hit_zero_backend_queries() {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_port = backend_listener.local_addr().unwrap().port();

    let counter = BackendQueryCounter::new();
    let _mock = spawn_counting_mock_server(backend_listener, "TestBackend", counter.clone(), true);

    let config = Config {
        servers: vec![
            Server::builder("127.0.0.1", Port::try_new(backend_port).unwrap())
                .name("TestBackend")
                .build()
                .unwrap(),
        ],
        cache: Some(Cache {
            store_article_bodies: true,
            adaptive_precheck: true,
            ..Default::default()
        }),
        ..Default::default()
    };

    let (mut reader, mut write_half) = connect_test_client(config).await;
    let mut line = String::new();

    counter.reset();

    // First HEAD - cache miss
    write_half
        .write_all(b"HEAD <test@example.com>\r\n")
        .await
        .unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("221"), "Should get 221 response: {line}");

    // Read multiline response
    loop {
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        if line.trim() == "." {
            break;
        }
    }

    counter.wait_for_at_least(1, Duration::from_secs(1)).await;
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
    assert!(line.starts_with("221"), "Should get 221 response: {line}");

    // Read multiline response
    loop {
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        if line.trim() == "." {
            break;
        }
    }

    counter
        .assert_stays_at_most(1, Duration::from_millis(200))
        .await;
    let second_queries = counter.get();

    assert!(
        second_queries <= 1,
        "CRITICAL BUG: HEAD cache hit triggered {second_queries} backend queries! Should be 0 (or 1 for background recheck)"
    );

    write_half.write_all(b"QUIT\r\n").await.unwrap();
}

/// CRITICAL TEST: ARTICLE command cache hits must not query backends
#[tokio::test]
async fn test_article_cache_hit_zero_backend_queries() {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_port = backend_listener.local_addr().unwrap().port();

    let counter = BackendQueryCounter::new();
    let _mock = spawn_counting_mock_server(backend_listener, "TestBackend", counter.clone(), true);

    let config = Config {
        servers: vec![
            Server::builder("127.0.0.1", Port::try_new(backend_port).unwrap())
                .name("TestBackend")
                .build()
                .unwrap(),
        ],
        cache: Some(Cache {
            store_article_bodies: true,
            adaptive_precheck: true,
            ..Default::default()
        }),
        ..Default::default()
    };

    let (mut reader, mut write_half) = connect_test_client(config).await;

    counter.reset();

    // First ARTICLE - cache miss
    write_half
        .write_all(b"ARTICLE <test@example.com>\r\n")
        .await
        .unwrap();
    read_multiline_response(&mut reader).await;

    let first_queries = counter.get();
    assert_eq!(first_queries, 1, "First ARTICLE should query backend once");

    yield_background_work().await;
    counter.reset();

    // Second ARTICLE - MUST hit cache
    write_half
        .write_all(b"ARTICLE <test@example.com>\r\n")
        .await
        .unwrap();
    read_multiline_response(&mut reader).await;

    let second_queries = counter.get();

    assert_eq!(
        second_queries, 0,
        "CRITICAL BUG: ARTICLE cache hit triggered {second_queries} backend queries. \
         Cache hits must not pick or query a backend."
    );

    write_half.write_all(b"QUIT\r\n").await.unwrap();
}

/// CRITICAL TEST: Batched ARTICLE cache hits must not bypass cache/precheck preparation.
#[tokio::test]
async fn test_batched_article_cache_hits_zero_backend_queries() {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_port = backend_listener.local_addr().unwrap().port();

    let counter = BackendQueryCounter::new();
    let _mock = spawn_counting_mock_server(backend_listener, "TestBackend", counter.clone(), true);

    let config = Config {
        servers: vec![
            Server::builder("127.0.0.1", Port::try_new(backend_port).unwrap())
                .name("TestBackend")
                .build()
                .unwrap(),
        ],
        cache: Some(Cache {
            store_article_bodies: true,
            adaptive_precheck: true,
            ..Default::default()
        }),
        ..Default::default()
    };

    let (mut reader, mut write_half) = connect_test_client(config).await;

    counter.reset();

    write_half
        .write_all(b"ARTICLE <test@example.com>\r\n")
        .await
        .unwrap();
    read_multiline_response(&mut reader).await;
    assert_eq!(counter.get(), 1, "First ARTICLE should query backend once");

    yield_background_work().await;
    counter.reset();

    write_half
        .write_all(b"ARTICLE <test@example.com>\r\nARTICLE <test@example.com>\r\n")
        .await
        .unwrap();
    read_multiline_response(&mut reader).await;
    read_multiline_response(&mut reader).await;

    let second_queries = counter.get();
    assert_eq!(
        second_queries, 0,
        "CRITICAL BUG: batched ARTICLE cache hits triggered {second_queries} backend queries. \
         Batched cache hits must not bypass cache/precheck preparation."
    );

    write_half.write_all(b"QUIT\r\n").await.unwrap();
}

/// Fake-backend test: without article payload caching, ARTICLE requests still go upstream.
#[tokio::test]
async fn test_article_without_payload_cache_queries_backend_each_time() {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_port = backend_listener.local_addr().unwrap().port();

    let counter = BackendQueryCounter::new();
    let _mock = spawn_counting_mock_server(backend_listener, "TestBackend", counter.clone(), true);

    let config = Config {
        servers: vec![
            Server::builder("127.0.0.1", Port::try_new(backend_port).unwrap())
                .name("TestBackend")
                .build()
                .unwrap(),
        ],
        cache: Some(Cache {
            store_article_bodies: false,
            adaptive_precheck: false,
            ..Default::default()
        }),
        ..Default::default()
    };

    let (mut reader, mut write_half) = connect_test_client(config).await;
    let mut line = String::new();

    for expected_queries in 1..=2 {
        write_half
            .write_all(b"ARTICLE <no-payload-cache@example.com>\r\n")
            .await
            .unwrap();
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        assert!(line.starts_with("220"), "Should get 220 response: {line}");

        loop {
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            if line.trim() == "." {
                break;
            }
        }

        assert_eq!(
            counter.get(),
            expected_queries,
            "cache_articles=false should not serve ARTICLE payloads from cache"
        );
    }

    write_half.write_all(b"QUIT\r\n").await.unwrap();
}

/// CRITICAL TEST: Cached 430s must not trigger backend queries
#[tokio::test]
async fn test_cached_430_zero_backend_queries() {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_port = backend_listener.local_addr().unwrap().port();

    let counter = BackendQueryCounter::new();
    // Server doesn't have article (returns 430)
    let _mock = spawn_counting_mock_server(backend_listener, "TestBackend", counter.clone(), false);

    let config = Config {
        servers: vec![
            Server::builder("127.0.0.1", Port::try_new(backend_port).unwrap())
                .name("TestBackend")
                .build()
                .unwrap(),
        ],
        cache: Some(Cache {
            store_article_bodies: true,
            adaptive_precheck: true,
            ..Default::default()
        }),
        ..Default::default()
    };

    let (mut reader, mut write_half) = connect_test_client(config).await;
    let mut line = String::new();

    counter.reset();

    // First STAT - cache miss, backend returns 430
    write_half
        .write_all(b"STAT <missing@example.com>\r\n")
        .await
        .unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("430"), "Should get 430 response: {line}");

    counter.wait_for_at_least(1, Duration::from_secs(1)).await;
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
        "Should still get 430 response: {line}"
    );

    counter
        .assert_stays_at_most(1, Duration::from_millis(200))
        .await;
    let second_queries = counter.get();

    assert!(
        second_queries <= 1,
        "CRITICAL BUG: Cached 430 triggered {second_queries} backend queries! Should be 0 (or 1 for background recheck). \
         430 responses MUST be cached and served instantly."
    );

    write_half.write_all(b"QUIT\r\n").await.unwrap();
}
