use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static ENABLED: OnceLock<bool> = OnceLock::new();
static STATS: PipelineTimingStats = PipelineTimingStats::new();

struct PipelineTimingStats {
    queue_wait_ns: AtomicU64,
    queue_wait_count: AtomicU64,
    backend_read_ns: AtomicU64,
    backend_read_count: AtomicU64,
    client_await_ns: AtomicU64,
    client_await_count: AtomicU64,
    client_write_ns: AtomicU64,
    client_write_count: AtomicU64,
    payload_bytes: AtomicU64,
}

impl PipelineTimingStats {
    const fn new() -> Self {
        Self {
            queue_wait_ns: AtomicU64::new(0),
            queue_wait_count: AtomicU64::new(0),
            backend_read_ns: AtomicU64::new(0),
            backend_read_count: AtomicU64::new(0),
            client_await_ns: AtomicU64::new(0),
            client_await_count: AtomicU64::new(0),
            client_write_ns: AtomicU64::new(0),
            client_write_count: AtomicU64::new(0),
            payload_bytes: AtomicU64::new(0),
        }
    }
}

#[inline]
pub(crate) fn enabled() -> bool {
    *ENABLED.get_or_init(|| {
        std::env::var_os("NNTP_PROXY_PIPELINE_TIMING").is_some_and(|value| value != "0")
    })
}

#[inline]
pub(crate) fn now_if_enabled() -> Option<Instant> {
    enabled().then(Instant::now)
}

#[inline]
fn nanos(duration: Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

#[inline]
pub(crate) fn record_queue_wait(duration: Duration) {
    if !enabled() {
        return;
    }
    STATS
        .queue_wait_ns
        .fetch_add(nanos(duration), Ordering::Relaxed);
    STATS.queue_wait_count.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub(crate) fn record_backend_read(duration: Duration, bytes: u64) {
    if !enabled() {
        return;
    }
    STATS
        .backend_read_ns
        .fetch_add(nanos(duration), Ordering::Relaxed);
    let count = STATS.backend_read_count.fetch_add(1, Ordering::Relaxed) + 1;
    STATS.payload_bytes.fetch_add(bytes, Ordering::Relaxed);
    maybe_report(count);
}

#[inline]
pub(crate) fn record_client_await(duration: Duration) {
    if !enabled() {
        return;
    }
    STATS
        .client_await_ns
        .fetch_add(nanos(duration), Ordering::Relaxed);
    STATS.client_await_count.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub(crate) fn record_client_write(duration: Duration) {
    if !enabled() {
        return;
    }
    STATS
        .client_write_ns
        .fetch_add(nanos(duration), Ordering::Relaxed);
    STATS.client_write_count.fetch_add(1, Ordering::Relaxed);
}

fn avg_us(total_ns: &AtomicU64, count: &AtomicU64) -> u64 {
    let count = count.load(Ordering::Relaxed);
    if count == 0 {
        return 0;
    }
    total_ns.load(Ordering::Relaxed) / count / 1_000
}

fn maybe_report(completed: u64) {
    if !completed.is_multiple_of(1024) {
        return;
    }

    let bytes = STATS.payload_bytes.load(Ordering::Relaxed);
    eprintln!(
        "pipeline_timing completed={} bytes={} avg_queue_wait_us={} avg_backend_read_us={} avg_client_await_us={} avg_client_write_us={}",
        completed,
        bytes,
        avg_us(&STATS.queue_wait_ns, &STATS.queue_wait_count),
        avg_us(&STATS.backend_read_ns, &STATS.backend_read_count),
        avg_us(&STATS.client_await_ns, &STATS.client_await_count),
        avg_us(&STATS.client_write_ns, &STATS.client_write_count),
    );
}
