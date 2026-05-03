//! End-to-end request/response benchmarks through the proxy socket.
//!
//! These benches start a real proxy listener and a minimal local NNTP backend,
//! then measure client `ARTICLE` request latency over a persistent connection.
//!
//! Run with: cargo bench --bench `end_to_end_proxy`

use divan::{Bencher, black_box};
use nntp_proxy::config::{Cache, Config, Server};
use nntp_proxy::types::{CacheCapacity, MaxConnections, Port};
use nntp_proxy::{NntpProxy, RoutingMode};
use std::sync::Arc;
use std::time::Duration;
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
                .enable_pipelining(true)
                .build()
                .unwrap(),
        ],
        cache,
        ..Default::default()
    }
}

fn memory_cache() -> Cache {
    Cache {
        max_capacity: CacheCapacity::try_new(32 * 1024 * 1024).unwrap(),
        ttl: Duration::from_mins(5),
        cache_articles: true,
        adaptive_precheck: false,
        availability_file: None,
        disk: None,
    }
}

fn metadata_only_cache() -> Cache {
    Cache {
        cache_articles: false,
        ..memory_cache()
    }
}

fn spawn_backend(listener: TcpListener, response: Arc<[u8]>) {
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
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
                    let write = if bytes.starts_with(b"ARTICLE ") {
                        write_half.write_all(&response).await
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
}

impl BenchProxy {
    async fn start(body_len: usize, cache: Option<Cache>) -> Self {
        let backend_listener = bind_localhost().await;
        let backend_port = backend_listener.local_addr().unwrap().port();
        spawn_backend(backend_listener, article_response(body_len).into());

        let proxy_listener = bind_localhost().await;
        let proxy_addr = proxy_listener.local_addr().unwrap();
        let proxy = NntpProxy::new(bench_config(backend_port, cache), RoutingMode::PerCommand)
            .await
            .unwrap();
        spawn_proxy(proxy_listener, proxy, RoutingMode::PerCommand);

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
            response_buffer: vec![0; body_len + 512].into_boxed_slice(),
        }
    }

    async fn article_roundtrip(&mut self) -> usize {
        self.stream
            .write_all(b"ARTICLE <bench@example.com>\r\n")
            .await
            .unwrap();
        read_multiline_into(&mut self.stream, &mut self.response_buffer).await
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

async fn read_multiline_into(stream: &mut TcpStream, buffer: &mut [u8]) -> usize {
    let mut total = 0;
    loop {
        let n = stream.read(&mut buffer[total..]).await.unwrap();
        assert_ne!(n, 0, "proxy closed during benchmark response");
        total += n;
        if buffer[..total].ends_with(b"\r\n.\r\n") {
            return total;
        }
    }
}

fn bench_roundtrip(bencher: Bencher, body_len: usize, cache: Option<Cache>, warm_cache: bool) {
    let rt = Builder::new_current_thread().enable_all().build().unwrap();
    let mut proxy = rt.block_on(BenchProxy::start(body_len, cache));
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
