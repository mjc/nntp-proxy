//! End-to-end request/response benchmarks through the proxy socket.
//!
//! These benches start a real proxy listener and a minimal local NNTP backend,
//! then measure client `ARTICLE` request latency over a persistent connection.
//!
//! Run with: cargo bench --bench `end_to_end_proxy`

use divan::{Bencher, black_box};
use nntp_proxy::config::{Cache, Config, Server};
use nntp_proxy::protocol::{RequestContext, RequestKind, StatusCode};
use nntp_proxy::types::{CacheCapacity, MaxConnections, Port};
use nntp_proxy::{NntpProxy, RoutingMode};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Builder;

fn main() {
    divan::main();
}

const MSG_ID: &str = "<bench@example.com>";
const ARTICLE_64K: usize = 64 * 1024;
const ARTICLE_768K: usize = 768 * 1024;
const ARTICLE_AVG_788K: usize = 788 * 1024;
const ARTICLE_LARGE_1536K: usize = 1536 * 1024;

fn article_response(body_len: usize) -> Vec<u8> {
    let body = "x".repeat(body_len);
    format!(
        "220 42 {MSG_ID}\r\nSubject: Benchmark\r\nFrom: bench@example.com\r\nMessage-ID: {MSG_ID}\r\n\r\n{body}\r\n.\r\n"
    )
    .into_bytes()
}

async fn bind_localhost() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").await.unwrap()
}

fn bench_config(backend_port: u16, cache: Option<Cache>) -> Config {
    Config {
        servers: vec![
            Server::builder("127.0.0.1", Port::try_new(backend_port).unwrap())
                .name("bench-backend")
                .max_connections(MaxConnections::try_new(4).unwrap())
                .build()
                .unwrap(),
        ],
        cache,
        ..Default::default()
    }
}

fn memory_cache() -> Cache {
    Cache {
        article_cache_capacity: CacheCapacity::try_new(32 * 1024 * 1024).unwrap(),
        article_cache_ttl_secs: std::time::Duration::from_secs(300),
        store_article_bodies: true,
        adaptive_precheck: false,
        availability_index_path: None,
        disk: None,
    }
}

fn metadata_only_cache() -> Cache {
    Cache {
        store_article_bodies: false,
        ..memory_cache()
    }
}

fn spawn_backend(listener: TcpListener, response: Arc<[u8]>) {
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            // Without this, Nagle plus delayed ACK holds back the second of
            // several small pipelined replies by about 40 ms.
            stream.set_nodelay(true).unwrap();
            let response = Arc::clone(&response);
            tokio::spawn(async move {
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                let mut line = String::new();

                if write_half
                    .write_all(b"201 bench backend ready\r\n")
                    .await
                    .is_err()
                {
                    return;
                }

                loop {
                    line.clear();
                    let Ok(n) = reader.read_line(&mut line).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }

                    let bytes = line.as_bytes();
                    // Parsed only off the ARTICLE path so the existing benches'
                    // backend does no extra work per request.
                    let kind = || RequestContext::parse(bytes).map(|request| request.kind());
                    let write = if bytes.starts_with(b"ARTICLE ") {
                        write_half.write_all(&response).await
                    } else if kind() == Some(RequestKind::Stat) {
                        write_half
                            .write_all(b"223 42 <bench@example.com>\r\n")
                            .await
                    } else if kind() == Some(RequestKind::Group) {
                        write_half.write_all(b"211 1 42 42 alt.test\r\n").await
                    } else if bytes.starts_with(b"MODE READER") {
                        write_half.write_all(b"200 Reader mode\r\n").await
                    } else if bytes.starts_with(b"DATE") {
                        write_half.write_all(b"111 20260503120000\r\n").await
                    } else if bytes.starts_with(b"QUIT") {
                        let _ = write_half.write_all(b"205 Goodbye\r\n").await;
                        return;
                    } else {
                        write_half.write_all(b"500 Unknown command\r\n").await
                    };

                    if write.is_err() {
                        return;
                    }
                }
            });
        }
    });
}

fn spawn_proxy(listener: TcpListener, proxy: NntpProxy, mode: RoutingMode) {
    tokio::spawn(async move {
        loop {
            let Ok((stream, addr)) = listener.accept().await else {
                break;
            };
            let proxy = proxy.clone();
            tokio::spawn(async move {
                let _ = match mode {
                    RoutingMode::Stateful => proxy.handle_client(stream, addr.into()).await,
                    RoutingMode::PerCommand | RoutingMode::Hybrid => {
                        proxy
                            .handle_client_per_command_routing(stream, addr.into())
                            .await
                    }
                };
            });
        }
    });
}

struct BenchProxy {
    stream: TcpStream,
    response_buffer: Box<[u8]>,
    response_len: usize,
}

/// Which backend answers the reader's message-ID lookup.
#[derive(Clone, Copy)]
enum ReaderLookup {
    /// The pinned primary has the article.
    PrimaryHit,
    /// The pinned primary answers `430`; a tier-1 secondary has the article.
    /// `stat_missing` is the secondary's setting.
    PrimaryMiss { stat_missing: u8 },
}

/// Whether `[routing] reader_message_id_failover` is on.
#[derive(Clone, Copy)]
enum ReaderFailover {
    Default,
    Enabled,
}

impl BenchProxy {
    /// Hybrid session pinned to the primary by `GROUP`, so message-ID
    /// lookups go through reader recovery when it is enabled.
    async fn start_reader(body_len: usize, failover: ReaderFailover, lookup: ReaderLookup) -> Self {
        let response: Arc<[u8]> = article_response(body_len).into();
        let primary = bind_localhost().await;
        let mut config = bench_config(primary.local_addr().unwrap().port(), None);
        config.servers[0].max_connections = MaxConnections::try_new(1).unwrap();
        config.routing.reader_message_id_failover = matches!(failover, ReaderFailover::Enabled);
        match lookup {
            ReaderLookup::PrimaryHit => spawn_backend(primary, Arc::clone(&response)),
            ReaderLookup::PrimaryMiss { stat_missing } => {
                spawn_backend(primary, Arc::from(&b"430 Missing primary\r\n"[..]));
                // Availability slots are per host, so the secondary needs its
                // own address or the primary's 430 would also mark it missing.
                let secondary = TcpListener::bind("127.0.0.2:0").await.unwrap();
                config.servers.push(
                    Server::builder(
                        "127.0.0.2",
                        Port::try_new(secondary.local_addr().unwrap().port()).unwrap(),
                    )
                    .name("reader-secondary")
                    .max_connections(MaxConnections::try_new(1).unwrap())
                    .stat_missing(stat_missing)
                    .tier(1)
                    .build()
                    .unwrap(),
                );
                spawn_backend(secondary, Arc::clone(&response));
            }
        }

        let proxy_listener = bind_localhost().await;
        let proxy_addr = proxy_listener.local_addr().unwrap();
        let proxy = NntpProxy::new(config, RoutingMode::Hybrid).await.unwrap();
        spawn_proxy(proxy_listener, proxy, RoutingMode::Hybrid);

        let mut stream = TcpStream::connect(proxy_addr).await.unwrap();
        stream.set_nodelay(true).unwrap();
        let mut status = Vec::new();
        read_line(&mut stream, &mut status).await;
        stream.write_all(b"GROUP alt.test\r\n").await.unwrap();
        read_line(&mut stream, &mut status).await;
        assert_eq!(
            StatusCode::parse(&status),
            Some(StatusCode::new(211)),
            "unexpected GROUP reply: {}",
            String::from_utf8_lossy(&status)
        );

        Self {
            stream,
            response_buffer: vec![0; body_len.saturating_mul(2) + 1024].into_boxed_slice(),
            response_len: response.len(),
        }
    }

    async fn start(body_len: usize, cache: Option<Cache>, mode: RoutingMode) -> Self {
        let backend_listener = bind_localhost().await;
        let backend_port = backend_listener.local_addr().unwrap().port();
        spawn_backend(backend_listener, article_response(body_len).into());

        let proxy_listener = bind_localhost().await;
        let proxy_addr = proxy_listener.local_addr().unwrap();
        let proxy = NntpProxy::new(bench_config(backend_port, cache), mode)
            .await
            .unwrap();
        spawn_proxy(proxy_listener, proxy, mode);

        let mut stream = TcpStream::connect(proxy_addr).await.unwrap();
        stream.set_nodelay(true).unwrap();
        let mut read_buffer = Vec::with_capacity(body_len + 512);
        read_line(&mut stream, &mut read_buffer).await;
        assert!(
            read_buffer.starts_with(b"201 "),
            "unexpected proxy greeting: {}",
            String::from_utf8_lossy(&read_buffer)
        );

        Self {
            stream,
            response_buffer: vec![0; body_len.saturating_mul(2) + 1024].into_boxed_slice(),
            response_len: article_response(body_len).len(),
        }
    }

    async fn article_roundtrip(&mut self) -> usize {
        self.stream
            .write_all(b"ARTICLE <bench@example.com>\r\n")
            .await
            .unwrap();
        read_exact_response_into(
            &mut self.stream,
            &mut self.response_buffer,
            self.response_len,
        )
        .await
    }

    async fn article_window_roundtrip(&mut self) -> usize {
        self.stream
            .write_all(b"ARTICLE <bench@example.com>\r\nARTICLE <bench@example.com>\r\n")
            .await
            .unwrap();
        read_exact_response_into(
            &mut self.stream,
            &mut self.response_buffer,
            self.response_len.saturating_mul(2),
        )
        .await
    }
}

async fn read_line(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> usize {
    buffer.clear();
    let mut byte = [0_u8; 1];
    loop {
        stream.read_exact(&mut byte).await.unwrap();
        buffer.push(byte[0]);
        if buffer.ends_with(b"\r\n") {
            return buffer.len();
        }
    }
}

async fn read_exact_response_into(
    stream: &mut TcpStream,
    buffer: &mut [u8],
    expected: usize,
) -> usize {
    let mut total = 0;
    while total < expected {
        let n = stream.read(&mut buffer[total..]).await.unwrap();
        assert_ne!(n, 0, "proxy closed during benchmark response");
        total += n;
    }
    total
}

fn bench_roundtrip(bencher: Bencher, body_len: usize, cache: Option<Cache>, warm_cache: bool) {
    let rt = Builder::new_current_thread().enable_all().build().unwrap();
    let mut proxy = rt.block_on(BenchProxy::start(body_len, cache, RoutingMode::PerCommand));
    if warm_cache {
        let bytes = rt.block_on(proxy.article_roundtrip());
        assert!(bytes > body_len);
    }
    bencher
        .counter(divan::counter::BytesCount::new(body_len))
        .bench_local(|| {
            let bytes = rt.block_on(proxy.article_roundtrip());
            black_box(bytes)
        });
}

fn bench_window(bencher: Bencher, body_len: usize, mode: RoutingMode) {
    let rt = Builder::new_current_thread().enable_all().build().unwrap();
    let mut proxy = rt.block_on(BenchProxy::start(body_len, None, mode));
    bencher
        .counter(divan::counter::BytesCount::new(body_len.saturating_mul(2)))
        .bench_local(|| {
            let bytes = rt.block_on(proxy.article_window_roundtrip());
            black_box(bytes)
        });
}

fn bench_stateful_window(bencher: Bencher, body_len: usize) {
    bench_window(bencher, body_len, RoutingMode::Stateful);
}

fn bench_reader_lookup(
    bencher: Bencher,
    body_len: usize,
    failover: ReaderFailover,
    lookup: ReaderLookup,
) {
    let rt = Builder::new_current_thread().enable_all().build().unwrap();
    let mut proxy = rt.block_on(BenchProxy::start_reader(body_len, failover, lookup));
    bencher
        .counter(divan::counter::BytesCount::new(body_len))
        .bench_local(|| black_box(rt.block_on(proxy.article_roundtrip())));
}

/// Message-ID lookups in a hybrid session pinned by `GROUP`: the cost of
/// `reader_message_id_failover` on a primary hit, and of recovering a primary
/// `430` from a tier-1 secondary, with and without its STAT probe.
mod reader_message_id_failover {
    use super::{
        ARTICLE_64K, ARTICLE_AVG_788K, Bencher, ReaderFailover, ReaderLookup, bench_reader_lookup,
    };

    const MISS: ReaderLookup = ReaderLookup::PrimaryMiss { stat_missing: 0 };
    const MISS_WITH_PROBE: ReaderLookup = ReaderLookup::PrimaryMiss { stat_missing: 1 };

    #[divan::bench(sample_count = 50, sample_size = 10)]
    fn default_primary_hit_64k(bencher: Bencher) {
        bench_reader_lookup(
            bencher,
            ARTICLE_64K,
            ReaderFailover::Default,
            ReaderLookup::PrimaryHit,
        );
    }

    #[divan::bench(sample_count = 50, sample_size = 10)]
    fn enabled_primary_hit_64k(bencher: Bencher) {
        bench_reader_lookup(
            bencher,
            ARTICLE_64K,
            ReaderFailover::Enabled,
            ReaderLookup::PrimaryHit,
        );
    }

    #[divan::bench(sample_count = 50, sample_size = 10)]
    fn enabled_primary_miss_64k(bencher: Bencher) {
        bench_reader_lookup(bencher, ARTICLE_64K, ReaderFailover::Enabled, MISS);
    }

    #[divan::bench(sample_count = 50, sample_size = 10)]
    fn enabled_primary_miss_with_probe_64k(bencher: Bencher) {
        bench_reader_lookup(
            bencher,
            ARTICLE_64K,
            ReaderFailover::Enabled,
            MISS_WITH_PROBE,
        );
    }

    #[divan::bench(sample_count = 50, sample_size = 5)]
    fn default_primary_hit_788k(bencher: Bencher) {
        bench_reader_lookup(
            bencher,
            ARTICLE_AVG_788K,
            ReaderFailover::Default,
            ReaderLookup::PrimaryHit,
        );
    }

    #[divan::bench(sample_count = 50, sample_size = 5)]
    fn enabled_primary_hit_788k(bencher: Bencher) {
        bench_reader_lookup(
            bencher,
            ARTICLE_AVG_788K,
            ReaderFailover::Enabled,
            ReaderLookup::PrimaryHit,
        );
    }

    #[divan::bench(sample_count = 50, sample_size = 5)]
    fn enabled_primary_miss_788k(bencher: Bencher) {
        bench_reader_lookup(bencher, ARTICLE_AVG_788K, ReaderFailover::Enabled, MISS);
    }

    #[divan::bench(sample_count = 50, sample_size = 5)]
    fn enabled_primary_miss_with_probe_788k(bencher: Bencher) {
        bench_reader_lookup(
            bencher,
            ARTICLE_AVG_788K,
            ReaderFailover::Enabled,
            MISS_WITH_PROBE,
        );
    }
}

fn bench_per_command_pair(bencher: Bencher, body_len: usize) {
    bench_window(bencher, body_len, RoutingMode::PerCommand);
}

mod backend_roundtrip {
    use super::{ARTICLE_64K, ARTICLE_768K, ARTICLE_AVG_788K, ARTICLE_LARGE_1536K};
    use super::{Bencher, bench_roundtrip};

    #[divan::bench(sample_count = 100, sample_size = 20)]
    fn article_64k(bencher: Bencher) {
        bench_roundtrip(bencher, ARTICLE_64K, None, false);
    }

    #[divan::bench(sample_count = 100, sample_size = 10)]
    fn article_768k(bencher: Bencher) {
        bench_roundtrip(bencher, ARTICLE_768K, None, false);
    }

    #[divan::bench(sample_count = 100, sample_size = 10)]
    fn article_avg_788k(bencher: Bencher) {
        bench_roundtrip(bencher, ARTICLE_AVG_788K, None, false);
    }

    #[divan::bench(sample_count = 50, sample_size = 5)]
    fn article_large_1536k(bencher: Bencher) {
        bench_roundtrip(bencher, ARTICLE_LARGE_1536K, None, false);
    }
}

mod cache_hit_roundtrip {
    use super::{ARTICLE_64K, ARTICLE_768K, ARTICLE_AVG_788K, ARTICLE_LARGE_1536K};
    use super::{Bencher, bench_roundtrip, memory_cache};

    #[divan::bench(sample_count = 100, sample_size = 20)]
    fn article_64k(bencher: Bencher) {
        bench_roundtrip(bencher, ARTICLE_64K, Some(memory_cache()), true);
    }

    #[divan::bench(sample_count = 100, sample_size = 10)]
    fn article_768k(bencher: Bencher) {
        bench_roundtrip(bencher, ARTICLE_768K, Some(memory_cache()), true);
    }

    #[divan::bench(sample_count = 100, sample_size = 10)]
    fn article_avg_788k(bencher: Bencher) {
        bench_roundtrip(bencher, ARTICLE_AVG_788K, Some(memory_cache()), true);
    }

    #[divan::bench(sample_count = 50, sample_size = 5)]
    fn article_large_1536k(bencher: Bencher) {
        bench_roundtrip(bencher, ARTICLE_LARGE_1536K, Some(memory_cache()), true);
    }
}

mod cache_miss_roundtrip {
    use super::{ARTICLE_64K, ARTICLE_768K, ARTICLE_AVG_788K, ARTICLE_LARGE_1536K};
    use super::{Bencher, bench_roundtrip, metadata_only_cache};

    #[divan::bench(sample_count = 100, sample_size = 20)]
    fn article_64k(bencher: Bencher) {
        bench_roundtrip(bencher, ARTICLE_64K, Some(metadata_only_cache()), false);
    }

    #[divan::bench(sample_count = 100, sample_size = 10)]
    fn article_768k(bencher: Bencher) {
        bench_roundtrip(bencher, ARTICLE_768K, Some(metadata_only_cache()), false);
    }

    #[divan::bench(sample_count = 100, sample_size = 10)]
    fn article_avg_788k(bencher: Bencher) {
        bench_roundtrip(
            bencher,
            ARTICLE_AVG_788K,
            Some(metadata_only_cache()),
            false,
        );
    }

    #[divan::bench(sample_count = 50, sample_size = 5)]
    fn article_large_1536k(bencher: Bencher) {
        bench_roundtrip(
            bencher,
            ARTICLE_LARGE_1536K,
            Some(metadata_only_cache()),
            false,
        );
    }
}

mod stateful_upstream_window {
    use super::{ARTICLE_64K, Bencher, bench_per_command_pair, bench_stateful_window};

    /// Sixteen pipelined tiny `ARTICLE` replies in one client write: the
    /// per-reply overhead of the per-command upstream window.
    #[divan::bench(sample_count = 50, sample_size = 10)]
    fn per_command_sixteen_small_replies(bencher: Bencher) {
        use super::{BenchProxy, Builder, RoutingMode, black_box, read_exact_response_into};
        use tokio::io::AsyncWriteExt;

        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        let mut proxy = rt.block_on(BenchProxy::start(0, None, RoutingMode::PerCommand));
        let requests = b"ARTICLE <bench@example.com>\r\n".repeat(16);
        let response_len = proxy.response_len * 16;
        proxy.response_buffer = vec![0; response_len].into_boxed_slice();
        bencher
            .counter(divan::counter::BytesCount::new(response_len))
            .bench_local(|| {
                black_box(rt.block_on(async {
                    proxy.stream.write_all(&requests).await.unwrap();
                    read_exact_response_into(
                        &mut proxy.stream,
                        &mut proxy.response_buffer,
                        response_len,
                    )
                    .await
                }))
            });
    }

    #[divan::bench(sample_count = 50, sample_size = 5)]
    fn article_pair_64k(bencher: Bencher) {
        bench_stateful_window(bencher, ARTICLE_64K);
    }

    #[divan::bench(sample_count = 50, sample_size = 5)]
    fn per_command_pair_64k(bencher: Bencher) {
        bench_per_command_pair(bencher, ARTICLE_64K);
    }
}
