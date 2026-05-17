use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

#[derive(Debug, Default)]
struct ClientWriterLockMetrics {
    lock_requests: AtomicUsize,
    immediate_locks: AtomicUsize,
    contended_locks: AtomicUsize,
    wait_nanos: AtomicU64,
    max_wait_nanos: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ClientWriterLockMetricsSnapshot {
    pub lock_requests: usize,
    pub immediate_locks: usize,
    pub contended_locks: usize,
    pub wait_nanos: u64,
    pub max_wait_nanos: u64,
}

fn client_writer_lock_metrics_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED
        .get_or_init(|| std::env::var_os("NNTP_PROXY_CLIENT_WRITER_LOCK_METRICS_SECS").is_some())
}

fn client_writer_lock_metrics() -> &'static ClientWriterLockMetrics {
    static METRICS: OnceLock<ClientWriterLockMetrics> = OnceLock::new();
    METRICS.get_or_init(ClientWriterLockMetrics::default)
}

pub(crate) fn client_writer_lock_metrics_snapshot() -> ClientWriterLockMetricsSnapshot {
    let metrics = client_writer_lock_metrics();
    ClientWriterLockMetricsSnapshot {
        lock_requests: metrics.lock_requests.load(Ordering::Relaxed),
        immediate_locks: metrics.immediate_locks.load(Ordering::Relaxed),
        contended_locks: metrics.contended_locks.load(Ordering::Relaxed),
        wait_nanos: metrics.wait_nanos.load(Ordering::Relaxed),
        max_wait_nanos: metrics.max_wait_nanos.load(Ordering::Relaxed),
    }
}

/// Shareable client writer used by per-command routing and backend workers.
#[derive(Clone, Debug)]
pub(crate) struct SharedClientWriter {
    inner: Arc<tokio::sync::Mutex<tokio::net::tcp::OwnedWriteHalf>>,
}

impl SharedClientWriter {
    #[must_use]
    pub(crate) fn new(write_half: tokio::net::tcp::OwnedWriteHalf) -> Self {
        Self {
            inner: Arc::new(tokio::sync::Mutex::new(write_half)),
        }
    }

    pub(crate) async fn lock(
        &self,
    ) -> tokio::sync::MutexGuard<'_, tokio::net::tcp::OwnedWriteHalf> {
        if !client_writer_lock_metrics_enabled() {
            return self.inner.lock().await;
        }

        let metrics = client_writer_lock_metrics();
        metrics.lock_requests.fetch_add(1, Ordering::Relaxed);

        if let Ok(guard) = self.inner.try_lock() {
            metrics.immediate_locks.fetch_add(1, Ordering::Relaxed);
            return guard;
        }

        let start = std::time::Instant::now();
        let guard = self.inner.lock().await;
        let wait_nanos = start.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        metrics.contended_locks.fetch_add(1, Ordering::Relaxed);
        metrics.wait_nanos.fetch_add(wait_nanos, Ordering::Relaxed);
        metrics
            .max_wait_nanos
            .fetch_max(wait_nanos, Ordering::Relaxed);
        guard
    }

    pub(crate) fn try_into_inner(self) -> Result<tokio::net::tcp::OwnedWriteHalf, Self> {
        match Arc::try_unwrap(self.inner) {
            Ok(mutex) => Ok(mutex.into_inner()),
            Err(inner) => Err(Self { inner }),
        }
    }
}
