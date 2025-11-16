//! Lock-free metrics collector

use super::{BackendStats, HealthStatus, MetricsSnapshot, UserStats};
use crate::types::BackendBytes;
use dashmap::DashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

// ============================================================================
// Internal Storage Types
// ============================================================================

/// Backend metrics with atomic storage (zero-allocation concurrent updates)
#[derive(Debug, Default)]
struct BackendMetrics {
    active_connections: AtomicUsize,
    total_commands: AtomicU64,
    bytes_sent: AtomicU64,
    bytes_received: AtomicU64,
    errors: AtomicU64,
    errors_4xx: AtomicU64,
    errors_5xx: AtomicU64,
    article_bytes_total: AtomicU64,
    article_count: AtomicU64,
    ttfb_micros_total: AtomicU64,
    ttfb_count: AtomicU64,
    send_micros_total: AtomicU64,
    recv_micros_total: AtomicU64,
    connection_failures: AtomicU64,
    health_status: AtomicU8,
}

impl BackendMetrics {
    fn to_backend_stats(&self, backend_id: usize) -> BackendStats {
        use super::types::*;
        BackendStats {
            backend_id,
            active_connections: ActiveConnections::new(
                self.active_connections.load(Ordering::Relaxed),
            ),
            total_commands: CommandCount::new(self.total_commands.load(Ordering::Relaxed)),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            bytes_received: self.bytes_received.load(Ordering::Relaxed),
            errors: ErrorCount::new(self.errors.load(Ordering::Relaxed)),
            errors_4xx: ErrorCount::new(self.errors_4xx.load(Ordering::Relaxed)),
            errors_5xx: ErrorCount::new(self.errors_5xx.load(Ordering::Relaxed)),
            article_bytes_total: self.article_bytes_total.load(Ordering::Relaxed),
            article_count: ArticleCount::new(self.article_count.load(Ordering::Relaxed)),
            ttfb_micros_total: TtfbMicros::new(self.ttfb_micros_total.load(Ordering::Relaxed)),
            ttfb_count: self.ttfb_count.load(Ordering::Relaxed),
            send_micros_total: SendMicros::new(self.send_micros_total.load(Ordering::Relaxed)),
            recv_micros_total: RecvMicros::new(self.recv_micros_total.load(Ordering::Relaxed)),
            connection_failures: FailureCount::new(
                self.connection_failures.load(Ordering::Relaxed),
            ),
            health_status: self.health_status.load(Ordering::Relaxed).into(),
        }
    }
}

/// Metrics for a single user (internal storage)
#[derive(Debug, Clone, Default)]
struct UserMetrics {
    username: String,
    active_connections: usize,
    total_connections: u64,
    bytes_sent: u64,
    bytes_received: u64,
    total_commands: u64,
    errors: u64,
}

impl UserMetrics {
    fn new(username: String) -> Self {
        Self {
            username,
            active_connections: 0,
            total_connections: 0,
            bytes_sent: 0,
            bytes_received: 0,
            total_commands: 0,
            errors: 0,
        }
    }

    fn to_user_stats(&self) -> UserStats {
        UserStats {
            username: self.username.clone(),
            active_connections: self.active_connections,
            total_connections: self.total_connections,
            bytes_sent: self.bytes_sent,
            bytes_received: self.bytes_received,
            total_commands: self.total_commands,
            errors: self.errors,
            bytes_sent_per_sec: 0,
            bytes_received_per_sec: 0,
        }
    }
}

// ============================================================================
// Public API
// ============================================================================

/// Thread-safe metrics collector
#[derive(Debug, Clone)]
pub struct MetricsCollector {
    inner: Arc<MetricsInner>,
}

#[derive(Debug)]
struct MetricsInner {
    total_connections: AtomicU64,
    active_connections: AtomicUsize,
    stateful_sessions: AtomicUsize,
    backend_metrics: Vec<BackendMetrics>,
    user_metrics: DashMap<String, UserMetrics>,
    start_time: Instant,
}

impl MetricsCollector {
    #[inline]
    fn backend_get(&self, backend_id: usize) -> Option<&BackendMetrics> {
        self.inner.backend_metrics.get(backend_id)
    }

    #[inline]
    fn normalize_username(username: Option<&str>) -> String {
        username.unwrap_or("<anonymous>").to_string()
    }

    #[inline]
    fn with_backend<F>(&self, backend_id: usize, action: F)
    where
        F: FnOnce(&BackendMetrics),
    {
        if let Some(backend) = self.backend_get(backend_id) {
            action(backend);
        }
    }

    /// Update or insert user metrics using a closure (lock-free fast path)
    #[inline]
    fn update_user_metrics<F>(&self, username: Option<&str>, update: F)
    where
        F: Fn(&mut UserMetrics),
    {
        let key = Self::normalize_username(username);

        // Fast path: try lock-free update if entry exists
        if let Some(mut entry) = self.inner.user_metrics.get_mut(&key) {
            update(&mut entry);
            return;
        }

        // Slow path: insert new entry (only happens once per user)
        let new_metrics = UserMetrics::new(key.clone());
        self.inner.user_metrics.insert(key.clone(), new_metrics);
        if let Some(mut entry) = self.inner.user_metrics.get_mut(&key) {
            update(&mut entry);
        }
    }

    /// Create a new metrics collector
    ///
    /// # Arguments
    /// * `num_backends` - Number of backend servers to track
    pub fn new(num_backends: usize) -> Self {
        let backend_metrics = (0..num_backends)
            .map(|_| BackendMetrics::default())
            .collect();

        Self {
            inner: Arc::new(MetricsInner {
                total_connections: AtomicU64::new(0),
                active_connections: AtomicUsize::new(0),
                stateful_sessions: AtomicUsize::new(0),
                backend_metrics,
                user_metrics: DashMap::new(),
                start_time: Instant::now(),
            }),
        }
    }

    // Connection lifecycle tracking

    #[inline]
    pub fn connection_opened(&self) {
        self.inner.total_connections.fetch_add(1, Ordering::Relaxed);
        self.inner
            .active_connections
            .fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn connection_closed(&self) {
        self.inner
            .active_connections
            .fetch_sub(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn stateful_session_started(&self) {
        self.inner.stateful_sessions.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn stateful_session_ended(&self) {
        self.inner.stateful_sessions.fetch_sub(1, Ordering::Relaxed);
    }

    // Backend metrics recording (using with_backend pattern for cleaner code)

    #[inline]
    pub fn record_command(&self, backend_id: usize) {
        self.with_backend(backend_id, |b| {
            b.total_commands.fetch_add(1, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn record_connection_failure(&self, backend_id: usize) {
        self.with_backend(backend_id, |b| {
            b.connection_failures.fetch_add(1, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn record_error(&self, backend_id: usize) {
        self.with_backend(backend_id, |b| {
            b.errors.fetch_add(1, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn record_client_to_backend_bytes_for(&self, backend_id: usize, bytes: u64) {
        self.with_backend(backend_id, |b| {
            b.bytes_sent.fetch_add(bytes, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn record_backend_to_client_bytes_for(&self, backend_id: usize, bytes: u64) {
        self.with_backend(backend_id, |b| {
            b.bytes_received.fetch_add(bytes, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn backend_connection_opened(&self, backend_id: usize) {
        self.with_backend(backend_id, |b| {
            b.active_connections.fetch_add(1, Ordering::Relaxed);
        });
    }

    /// Type-safe recording: consumes unrecorded bytes and returns recorded marker
    #[inline]
    pub fn record_client_to_backend(
        &self,
        bytes: crate::types::MetricsBytes<crate::types::Unrecorded>,
    ) -> crate::types::MetricsBytes<crate::types::Recorded> {
        crate::types::MetricsBytes::new(bytes.into_u64()).mark_recorded()
    }

    /// Type-safe recording: consumes unrecorded bytes and returns recorded marker
    #[inline]
    pub fn record_backend_to_client(
        &self,
        bytes: crate::types::MetricsBytes<crate::types::Unrecorded>,
    ) -> crate::types::MetricsBytes<crate::types::Recorded> {
        crate::types::MetricsBytes::new(bytes.into_u64()).mark_recorded()
    }

    #[inline]
    pub fn backend_connection_closed(&self, backend_id: usize) {
        self.with_backend(backend_id, |b| {
            b.active_connections.fetch_sub(1, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn record_error_4xx(&self, backend_id: usize) {
        self.with_backend(backend_id, |b| {
            b.errors_4xx.fetch_add(1, Ordering::Relaxed);
            b.errors.fetch_add(1, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn record_error_5xx(&self, backend_id: usize) {
        self.with_backend(backend_id, |b| {
            b.errors_5xx.fetch_add(1, Ordering::Relaxed);
            b.errors.fetch_add(1, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn record_article(&self, backend_id: usize, bytes: u64) {
        self.with_backend(backend_id, |b| {
            b.article_bytes_total.fetch_add(bytes, Ordering::Relaxed);
            b.article_count.fetch_add(1, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn record_ttfb_micros(&self, backend_id: usize, micros: u64) {
        self.with_backend(backend_id, |b| {
            b.ttfb_micros_total.fetch_add(micros, Ordering::Relaxed);
            b.ttfb_count.fetch_add(1, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn record_send_recv_micros(&self, backend_id: usize, send_micros: u64, recv_micros: u64) {
        self.with_backend(backend_id, |b| {
            b.send_micros_total
                .fetch_add(send_micros, Ordering::Relaxed);
            b.recv_micros_total
                .fetch_add(recv_micros, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn set_backend_health(&self, backend_id: usize, health: HealthStatus) {
        self.with_backend(backend_id, |b| {
            b.health_status.store(health.into(), Ordering::Relaxed);
        });
    }

    // User metrics recording (functional update pattern)

    pub fn user_connection_opened(&self, username: Option<&str>) {
        self.update_user_metrics(username, |m| {
            m.active_connections += 1;
            m.total_connections += 1;
        });
    }

    pub fn user_connection_closed(&self, username: Option<&str>) {
        let key = Self::normalize_username(username);
        if let Some(mut m) = self.inner.user_metrics.get_mut(&key) {
            m.active_connections = m.active_connections.saturating_sub(1);
        }
    }

    pub fn user_bytes_sent(&self, username: Option<&str>, bytes: u64) {
        self.update_user_metrics(username, |m| m.bytes_sent += bytes);
    }

    pub fn user_bytes_received(&self, username: Option<&str>, bytes: u64) {
        self.update_user_metrics(username, |m| m.bytes_received += bytes);
    }

    pub fn user_command(&self, username: Option<&str>) {
        self.update_user_metrics(username, |m| m.total_commands += 1);
    }

    pub fn user_error(&self, username: Option<&str>) {
        self.update_user_metrics(username, |m| m.errors += 1);
    }

    // Snapshot creation (pure functional transformations)

    /// Create a snapshot of current metrics (functional pipeline)
    ///
    /// Returns cumulative counters - no rate calculations.
    /// Use `MetricsSnapshot::with_pool_status()` to add pool utilization data.
    pub fn snapshot(&self) -> MetricsSnapshot {
        let backend_stats: Vec<BackendStats> = self
            .inner
            .backend_metrics
            .iter()
            .enumerate()
            .map(|(id, metrics)| metrics.to_backend_stats(id))
            .collect();

        let user_stats: Vec<UserStats> = self
            .inner
            .user_metrics
            .iter()
            .map(|entry| entry.value().to_user_stats())
            .collect();

        let total_sent: u64 = backend_stats.iter().map(|b| b.bytes_sent).sum();
        let total_received: u64 = backend_stats.iter().map(|b| b.bytes_received).sum();

        MetricsSnapshot {
            total_connections: self.inner.total_connections.load(Ordering::Relaxed),
            active_connections: self.inner.active_connections.load(Ordering::Relaxed),
            stateful_sessions: self.inner.stateful_sessions.load(Ordering::Relaxed),
            client_to_backend_bytes: BackendBytes::new(total_sent),
            backend_to_client_bytes: BackendBytes::new(total_received),
            uptime: self.inner.start_time.elapsed(),
            backend_stats: Arc::new(backend_stats),
            user_stats,
        }
    }

    /// Get the number of backends being tracked
    #[must_use]
    #[inline]
    pub fn num_backends(&self) -> usize {
        self.inner.backend_metrics.len()
    }
}
