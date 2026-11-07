//! Memory held by cache-miss payload captures under realistic read geometry.
//!
//! TLS backends return at most ~16 KiB per read even though each pooled read
//! buffer is 1 MiB, so per-read buffer ownership can make a capture hold far
//! more memory than the article it contains. Wall time alone cannot show that,
//! so this harness reports the peak live heap of the whole proxy alongside the
//! pool fallback counters from `hot_path_allocation_metrics_snapshot`, and
//! the client's time to first byte, which shows whether a cache-miss capture
//! holds the response back until the backend finishes it.
//!
//! Run with:
//! `cargo bench --features framing-bench --bench capture_retention`
//!
//! Options (all `--key=value`): `geometry=sparse|dense`, `concurrency`,
//! `requests` (per client), `size` (article bytes), `segment` (sparse backend
//! write size), `pause-us` (sparse pause between writes), `capture-pool-size`,
//! `capture-pool-count`, `buffer-pool-count`. Without options the default
//! matrix runs.

use nntp_proxy::config::{Cache, Config, Memory, Server};
use nntp_proxy::pool::hot_path_allocation_metrics_snapshot;
use nntp_proxy::protocol::StatusCode;
use nntp_proxy::session::benchmark_article_response_into;
use nntp_proxy::types::{CacheCapacity, MaxConnections, Port};
use nntp_proxy::{NntpProxy, RoutingMode};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// Counts live heap bytes so the peak reflects every retained buffer,
/// including pool fallbacks that never return to a pool.
struct PeakTracking;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn track_alloc(size: usize) {
    let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

// SAFETY: every method forwards to `System` with the caller's layout; the
// counters do not affect allocation behaviour.
unsafe impl GlobalAlloc for PeakTracking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged to the system allocator.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            track_alloc(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged to the system allocator.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            track_alloc(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` was allocated by `System` with this layout.
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: `ptr` was allocated by `System` with this layout.
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            track_alloc(new_size);
        }
        new
    }
}

#[global_allocator]
static ALLOCATOR: PeakTracking = PeakTracking;

const MIB: f64 = 1024.0 * 1024.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Geometry {
    /// Small paced backend writes, like TLS records arriving over a WAN.
    Sparse,
    /// One backend write per article, so reads fill large buffers.
    Dense,
}

#[derive(Clone, Copy, Debug)]
struct Scenario {
    geometry: Geometry,
    concurrency: usize,
    requests: usize,
    size: usize,
    segment: usize,
    pause: Duration,
    memory: MemoryShape,
}

#[derive(Clone, Copy, Debug)]
struct MemoryShape {
    capture_pool_size: usize,
    capture_pool_count: usize,
    buffer_pool_count: usize,
}

impl Default for MemoryShape {
    fn default() -> Self {
        let memory = Memory::default();
        Self {
            capture_pool_size: memory.capture_pool_size,
            capture_pool_count: memory.capture_pool_count,
            buffer_pool_count: memory.buffer_pool_count,
        }
    }
}

impl Scenario {
    fn new(geometry: Geometry, concurrency: usize) -> Self {
        Self {
            geometry,
            concurrency,
            requests: 16,
            size: 788 * 1024,
            segment: 16 * 1024,
            pause: Duration::from_micros(200),
            memory: MemoryShape::default(),
        }
    }

    fn apply(&mut self, key: &str, value: &str) {
        let number = || {
            value
                .parse::<usize>()
                .unwrap_or_else(|_| panic!("--{key} expects an integer, got {value:?}"))
        };
        match key {
            "geometry" => {
                self.geometry = match value {
                    "sparse" => Geometry::Sparse,
                    "dense" => Geometry::Dense,
                    _ => panic!("--geometry expects sparse or dense"),
                }
            }
            "concurrency" => self.concurrency = number(),
            "requests" => self.requests = number(),
            "size" => self.size = number(),
            "segment" => self.segment = number(),
            "pause-us" => self.pause = Duration::from_micros(number() as u64),
            "capture-pool-size" => self.memory.capture_pool_size = number(),
            "capture-pool-count" => self.memory.capture_pool_count = number(),
            "buffer-pool-count" => self.memory.buffer_pool_count = number(),
            _ => panic!("unknown option --{key}"),
        }
    }
}

fn spawn_backend(listener: TcpListener, scenario: Scenario) {
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            stream.set_nodelay(true).unwrap();
            tokio::spawn(serve_backend(stream, scenario));
        }
    });
}

async fn serve_backend(stream: TcpStream, scenario: Scenario) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    if write_half.write_all(b"200 bench ready\r\n").await.is_err() {
        return;
    }
    let mut line = String::new();
    let mut response = Vec::with_capacity(scenario.size);
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let command = line.trim_end();
        let written = if let Some(id) = command.strip_prefix("ARTICLE ") {
            benchmark_article_response_into(&mut response, id, scenario.size);
            match scenario.geometry {
                Geometry::Dense => write_half.write_all(&response).await,
                Geometry::Sparse => write_paced(&mut write_half, &response, scenario).await,
            }
        } else if command == "QUIT" {
            let _ = write_half.write_all(b"205 bye\r\n").await;
            return;
        } else if command == "DATE" {
            write_half.write_all(b"111 20260101000000\r\n").await
        } else if command == "MODE READER" {
            write_half.write_all(b"200 reader\r\n").await
        } else {
            write_half.write_all(b"500 unknown\r\n").await
        };
        if written.is_err() {
            return;
        }
    }
}

async fn write_paced(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    response: &[u8],
    scenario: Scenario,
) -> std::io::Result<()> {
    for segment in response.chunks(scenario.segment) {
        writer.write_all(segment).await?;
        tokio::time::sleep(scenario.pause).await;
    }
    Ok(())
}

fn spawn_proxy(listener: TcpListener, proxy: NntpProxy) {
    tokio::spawn(async move {
        while let Ok((stream, addr)) = listener.accept().await {
            let proxy = proxy.clone();
            tokio::spawn(async move {
                let _ = proxy
                    .handle_client_per_command_routing(stream, addr.into())
                    .await;
            });
        }
    });
}

/// Bytes received and per-request time to first byte for one client.
struct ClientResult {
    bytes: usize,
    first_byte: Vec<Duration>,
}

async fn run_client(
    address: std::net::SocketAddr,
    client: usize,
    scenario: Scenario,
) -> anyhow::Result<ClientResult> {
    let mut stream = TcpStream::connect(address).await?;
    stream.set_nodelay(true)?;
    let mut reader = BufReader::new(&mut stream);
    let mut greeting = String::new();
    reader.read_line(&mut greeting).await?;
    anyhow::ensure!(
        StatusCode::parse(greeting.as_bytes()).is_some_and(|status| status.is_success()),
        "greeting {greeting:?}"
    );
    drop(reader);

    let mut expected = Vec::with_capacity(scenario.size);
    let mut response = Vec::with_capacity(scenario.size);
    let mut received = 0;
    let mut first_byte = Vec::with_capacity(scenario.requests);
    for request in 0..scenario.requests {
        let id = format!("<c{client}-r{request}@capture.bench>");
        benchmark_article_response_into(&mut expected, &id, scenario.size);
        stream
            .write_all(format!("ARTICLE {id}\r\n").as_bytes())
            .await?;
        let sent = Instant::now();
        response.resize(expected.len(), 0);
        stream.read_exact(&mut response[..1]).await?;
        first_byte.push(sent.elapsed());
        stream.read_exact(&mut response[1..]).await?;
        anyhow::ensure!(response == expected, "response for {id} differs");
        received += response.len();
    }
    Ok(ClientResult {
        bytes: received,
        first_byte,
    })
}

fn percentile_ms(sorted: &[Duration], percentile: usize) -> f64 {
    let index = (sorted.len() * percentile / 100).min(sorted.len() - 1);
    sorted[index].as_secs_f64() * 1000.0
}

async fn run(scenario: Scenario) -> anyhow::Result<()> {
    let backend = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend.local_addr()?.port();
    spawn_backend(backend, scenario);

    let config = Config {
        servers: vec![
            Server::builder("127.0.0.1", Port::try_new(backend_port)?)
                .name("capture-bench")
                .max_connections(MaxConnections::try_new(scenario.concurrency)?)
                .build()?,
        ],
        memory: Memory {
            capture_pool_size: scenario.memory.capture_pool_size,
            capture_pool_count: scenario.memory.capture_pool_count,
            buffer_pool_count: scenario.memory.buffer_pool_count,
            ..Memory::default()
        },
        // A small payload cache keeps cached copies from dominating the peak;
        // every request is a distinct message ID, so each one is a miss that
        // captures the full response.
        cache: Some(Cache {
            article_cache_capacity: CacheCapacity::try_new(16 * 1024 * 1024)?,
            article_cache_ttl_secs: Duration::from_secs(300),
            store_article_bodies: true,
            adaptive_precheck: false,
            availability_index_path: None,
            disk: None,
        }),
        ..Default::default()
    };
    let proxy = NntpProxy::new(config, RoutingMode::PerCommand).await?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    spawn_proxy(listener, proxy);

    // Warm one connection so startup allocations are part of the baseline.
    run_client(
        address,
        usize::MAX,
        Scenario {
            requests: 1,
            ..scenario
        },
    )
    .await?;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let baseline = LIVE.load(Ordering::Relaxed);
    PEAK.store(baseline, Ordering::Relaxed);
    let before = hot_path_allocation_metrics_snapshot();
    let started = Instant::now();

    let mut clients = tokio::task::JoinSet::new();
    for client in 0..scenario.concurrency {
        clients.spawn(run_client(address, client, scenario));
    }
    let mut bytes = 0;
    let mut first_byte = Vec::with_capacity(scenario.concurrency * scenario.requests);
    while let Some(result) = clients.join_next().await {
        let client = result??;
        bytes += client.bytes;
        first_byte.extend(client.first_byte);
    }
    first_byte.sort_unstable();

    let elapsed = started.elapsed();
    let peak = PEAK.load(Ordering::Relaxed);
    let after = hot_path_allocation_metrics_snapshot();
    let articles = scenario.concurrency * scenario.requests;
    println!(
        "{:<6} c={:<3} articles={:<4} size={}KiB capture_pool={}x{}KiB | peak_heap_above_baseline={:>8.1} MiB per_capture={:>6.2} MiB | regular_fallbacks={:<6} capture_fallbacks={:<6} metadata_spills={:<5} | ttfb p50={:>6.2} ms p99={:>6.2} ms | {:>7.1} ms {:>7.1} MiB/s",
        format!("{:?}", scenario.geometry).to_lowercase(),
        scenario.concurrency,
        articles,
        scenario.size / 1024,
        scenario.memory.capture_pool_count,
        scenario.memory.capture_pool_size / 1024,
        peak.saturating_sub(baseline) as f64 / MIB,
        peak.saturating_sub(baseline) as f64 / MIB / scenario.concurrency as f64,
        after.regular_pool_fallback_allocations - before.regular_pool_fallback_allocations,
        after.capture_pool_fallback_allocations - before.capture_pool_fallback_allocations,
        after.chunked_response_metadata_spills - before.chunked_response_metadata_spills,
        percentile_ms(&first_byte, 50),
        percentile_ms(&first_byte, 99),
        elapsed.as_secs_f64() * 1000.0,
        bytes as f64 / MIB / elapsed.as_secs_f64(),
    );
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let options: Vec<(String, String)> = std::env::args()
        .skip(1)
        .filter(|arg| arg != "--bench")
        .map(|arg| {
            let arg = arg
                .strip_prefix("--")
                .unwrap_or_else(|| panic!("unexpected argument {arg:?}"));
            let (key, value) = arg
                .split_once('=')
                .unwrap_or_else(|| panic!("expected --key=value, got --{arg}"));
            (key.to_owned(), value.to_owned())
        })
        .collect();

    let scenarios = if options.is_empty() {
        vec![
            Scenario::new(Geometry::Sparse, 8),
            Scenario::new(Geometry::Sparse, 32),
            Scenario::new(Geometry::Dense, 8),
            Scenario::new(Geometry::Dense, 32),
        ]
    } else {
        let mut scenario = Scenario::new(Geometry::Sparse, 8);
        for (key, value) in &options {
            scenario.apply(key, value);
        }
        vec![scenario]
    };

    for scenario in scenarios {
        // A fresh runtime per scenario releases the previous proxy's pools.
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()?
            .block_on(run(scenario))?;
    }
    Ok(())
}
