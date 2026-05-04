//! Callgrind benchmarks for the real cache-miss proxy roundtrip path.
//!
//! These benches drive a client socket through a live per-command proxy with
//! metadata-only cache enabled, so each `ARTICLE` request still streams from the
//! backend while paying the request/cache/routing fixed costs of the miss path.
//!
//! Run with: `cargo bench --bench cache_miss_roundtrip_callgrind`

macro_rules! supported {
    ($($item:item)*) => {
        $(
            #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
            $item
        )*
    };
}

supported! {
    use iai_callgrind::{
        Callgrind, LibraryBenchmarkConfig, library_benchmark, library_benchmark_group, main,
    };
    use nntp_proxy::config::{Cache, Config, Server};
    use nntp_proxy::session::streaming::tail_buffer::{TailBuffer, TerminatorStatus};
    use nntp_proxy::types::{CacheCapacity, MaxConnections, Port};
    use nntp_proxy::{NntpProxy, RoutingMode};
    use std::hint::black_box;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::runtime::Builder;

    const MSG_ID: &str = "<bench@example.com>";
    const ARTICLE_64K: usize = 64 * 1024;
    const ARTICLE_768K: usize = 768 * 1024;

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

    fn bench_config(backend_port: u16) -> Config {
        Config {
            servers: vec![
                Server::builder("127.0.0.1", Port::try_new(backend_port).unwrap())
                    .name("bench-backend")
                    .max_connections(MaxConnections::try_new(4).unwrap())
                    .enable_pipelining(true)
                    .build()
                    .unwrap(),
            ],
            cache: Some(metadata_only_cache()),
            ..Default::default()
        }
    }

    fn metadata_only_cache() -> Cache {
        Cache {
            max_capacity: CacheCapacity::try_new(32 * 1024 * 1024).unwrap(),
            ttl: Duration::from_mins(5),
            cache_articles: false,
            adaptive_precheck: false,
            availability_file: None,
            disk: None,
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

    fn spawn_proxy(listener: TcpListener, proxy: NntpProxy) {
        tokio::spawn(async move {
            loop {
                let Ok((stream, addr)) = listener.accept().await else {
                    break;
                };
                let proxy = proxy.clone();
                tokio::spawn(async move {
                    let _ = proxy
                        .handle_client_per_command_routing(stream, addr.into())
                        .await;
                });
            }
        });
    }

    struct BenchClient {
        stream: TcpStream,
        response_buffer: Box<[u8]>,
    }

    impl BenchClient {
        async fn connect(
            addr: std::net::SocketAddr,
            body_len: usize,
            greeting_prefix: &[u8],
        ) -> Self {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream.set_nodelay(true).unwrap();
            let mut greeting = vec![0_u8; greeting_prefix.len() + 64];
            let greeting_len = read_line_into(&mut stream, &mut greeting).await;
            assert!(
                greeting[..greeting_len].starts_with(greeting_prefix),
                "unexpected greeting: {}",
                String::from_utf8_lossy(&greeting[..greeting_len])
            );

            Self {
                stream,
                response_buffer: vec![0; body_len + 512].into_boxed_slice(),
            }
        }

        async fn start_proxy(body_len: usize) -> Self {
            let backend_listener = bind_localhost().await;
            let backend_port = backend_listener.local_addr().unwrap().port();
            spawn_backend(backend_listener, article_response(body_len).into());

            let proxy_listener = bind_localhost().await;
            let proxy_addr = proxy_listener.local_addr().unwrap();
            let proxy = NntpProxy::new(bench_config(backend_port), RoutingMode::PerCommand)
                .await
                .unwrap();
            spawn_proxy(proxy_listener, proxy);

            Self::connect(proxy_addr, body_len, b"201 ").await
        }

        async fn start_direct_backend(body_len: usize) -> Self {
            let backend_listener = bind_localhost().await;
            let backend_addr = backend_listener.local_addr().unwrap();
            spawn_backend(backend_listener, article_response(body_len).into());

            Self::connect(backend_addr, body_len, b"201 ").await
        }

        async fn article_roundtrip(&mut self) -> usize {
            self.stream.write_all(b"ARTICLE <bench@example.com>\r\n").await.unwrap();
            read_multiline_into(&mut self.stream, &mut self.response_buffer).await
        }
    }

    async fn read_line_into(stream: &mut TcpStream, buffer: &mut [u8]) -> usize {
        let mut len = 0usize;
        loop {
            stream.read_exact(&mut buffer[len..len + 1]).await.unwrap();
            len += 1;
            if buffer[len - 1] == b'\n' {
                return len;
            }
        }
    }

    async fn read_multiline_into(stream: &mut TcpStream, buffer: &mut [u8]) -> usize {
        let mut total = 0usize;
        let mut tail = TailBuffer::default();
        loop {
            let n = stream.read(&mut buffer[total..]).await.unwrap();
            assert_ne!(n, 0, "proxy closed during benchmark response");
            let chunk = &buffer[total..total + n];
            match tail.detect_terminator(chunk) {
                TerminatorStatus::FoundAt(pos) => return total + pos,
                TerminatorStatus::NotFound => {
                    total += n;
                    tail.update(chunk);
                }
            }
        }
    }

    struct BenchHarness {
        rt: tokio::runtime::Runtime,
        client: BenchClient,
    }

    fn setup_proxy_roundtrip(body_len: usize) -> BenchHarness {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        let client = rt.block_on(BenchClient::start_proxy(body_len));
        BenchHarness { rt, client }
    }

    fn setup_direct_backend_roundtrip(body_len: usize) -> BenchHarness {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        let client = rt.block_on(BenchClient::start_direct_backend(body_len));
        BenchHarness { rt, client }
    }

    #[library_benchmark]
    #[bench::article_64k(args = (ARTICLE_64K), setup = setup_proxy_roundtrip)]
    #[bench::article_768k(args = (ARTICLE_768K), setup = setup_proxy_roundtrip)]
    fn run_cache_miss_roundtrip(mut harness: BenchHarness) -> usize {
        black_box(harness.rt.block_on(harness.client.article_roundtrip()))
    }

    #[library_benchmark]
    #[bench::article_64k(args = (ARTICLE_64K), setup = setup_direct_backend_roundtrip)]
    #[bench::article_768k(args = (ARTICLE_768K), setup = setup_direct_backend_roundtrip)]
    fn run_direct_backend_roundtrip(mut harness: BenchHarness) -> usize {
        black_box(harness.rt.block_on(harness.client.article_roundtrip()))
    }

    library_benchmark_group!(
        name = cache_miss_roundtrip;
        benchmarks = run_cache_miss_roundtrip, run_direct_backend_roundtrip
    );

    main!(
        config = LibraryBenchmarkConfig::default().tool(
            Callgrind::with_args([
                "--branch-sim=yes",
                "--cache-sim=yes",
            ])
        );
        library_benchmark_groups = cache_miss_roundtrip
    );
}

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
fn main() {
    eprintln!("cache_miss_roundtrip_callgrind is disabled on this target");
}
