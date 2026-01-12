//! Lock-free metrics collector

use super::{BackendHealthStatus, BackendStats, MetricsSnapshot, UserStats};
use crate::types::{BackendId, BackendToClientBytes, ClientToBackendBytes};
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
    /// Convert to immutable snapshot (public for testing)
    #[must_use]
    pub fn to_backend_stats(&self, backend_id: BackendId) -> BackendStats {
        use super::types::*;
        use crate::types::{ArticleBytesTotal, BytesReceived, BytesSent, TimingMeasurementCount};
        BackendStats {
            backend_id,
            active_connections: ActiveConnections::new(
                self.active_connections.load(Ordering::Relaxed),
            ),
            total_commands: CommandCount::new(self.total_commands.load(Ordering::Relaxed)),
            bytes_sent: BytesSent::new(self.bytes_sent.load(Ordering::Relaxed)),
            bytes_received: BytesReceived::new(self.bytes_received.load(Ordering::Relaxed)),
            errors: ErrorCount::new(self.errors.load(Ordering::Relaxed)),
            errors_4xx: ErrorCount::new(self.errors_4xx.load(Ordering::Relaxed)),
            errors_5xx: ErrorCount::new(self.errors_5xx.load(Ordering::Relaxed)),
            article_bytes_total: ArticleBytesTotal::new(
                self.article_bytes_total.load(Ordering::Relaxed),
            ),
            article_count: ArticleCount::new(self.article_count.load(Ordering::Relaxed)),
            ttfb_micros_total: TtfbMicros::new(self.ttfb_micros_total.load(Ordering::Relaxed)),
            ttfb_count: TimingMeasurementCount::new(self.ttfb_count.load(Ordering::Relaxed)),
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
        use crate::metrics::types::{CommandCount, ErrorCount};
        use crate::types::{BytesPerSecondRate, BytesReceived, BytesSent, TotalConnections};
        UserStats {
            username: self.username.clone(),
            active_connections: self.active_connections,
            total_connections: TotalConnections::new(self.total_connections),
            bytes_sent: BytesSent::new(self.bytes_sent),
            bytes_received: BytesReceived::new(self.bytes_received),
            total_commands: CommandCount::new(self.total_commands),
            errors: ErrorCount::new(self.errors),
            bytes_sent_per_sec: BytesPerSecondRate::ZERO,
            bytes_received_per_sec: BytesPerSecondRate::ZERO,
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
    // Note: client_to_backend_bytes and backend_to_client_bytes are calculated
    // from backend_metrics sums, not stored separately
}

impl MetricsCollector {
    #[inline]
    fn backend_get(&self, backend_id: BackendId) -> Option<&BackendMetrics> {
        self.inner.backend_metrics.get(backend_id.as_index())
    }

    #[inline]
    fn normalize_username(username: Option<&str>) -> String {
        username
            .unwrap_or(crate::constants::user::ANONYMOUS)
            .to_string()
    }

    #[inline]
    fn with_backend<F>(&self, backend_id: BackendId, action: F)
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
    pub fn record_command(&self, backend_id: BackendId) {
        self.with_backend(backend_id, |b| {
            b.total_commands.fetch_add(1, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn record_connection_failure(&self, backend_id: BackendId) {
        self.with_backend(backend_id, |b| {
            b.connection_failures.fetch_add(1, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn record_error(&self, backend_id: BackendId) {
        self.with_backend(backend_id, |b| {
            b.errors.fetch_add(1, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn record_client_to_backend_bytes_for(&self, backend_id: BackendId, bytes: u64) {
        self.with_backend(backend_id, |b| {
            b.bytes_sent.fetch_add(bytes, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn record_backend_to_client_bytes_for(&self, backend_id: BackendId, bytes: u64) {
        self.with_backend(backend_id, |b| {
            b.bytes_received.fetch_add(bytes, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn backend_connection_opened(&self, backend_id: BackendId) {
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

    /// Record command execution with per-backend and global byte tracking
    ///
    /// Convenience method that combines the common pattern:
    /// - Record command count for backend
    /// - Record per-backend bytes sent/received
    /// - Mark global bytes as recorded (type-state transition)
    ///
    /// Returns recorded byte markers for compile-time tracking.
    #[inline]
    pub fn record_command_execution(
        &self,
        backend_id: BackendId,
        client_to_backend: crate::types::MetricsBytes<crate::types::Unrecorded>,
        backend_to_client: crate::types::MetricsBytes<crate::types::Unrecorded>,
    ) -> (
        crate::types::MetricsBytes<crate::types::Recorded>,
        crate::types::MetricsBytes<crate::types::Recorded>,
    ) {
        // Record command count
        self.record_command(backend_id);

        // Record per-backend bytes
        self.record_client_to_backend_bytes_for(backend_id, client_to_backend.peek());
        self.record_backend_to_client_bytes_for(backend_id, backend_to_client.peek());

        // Mark global bytes as recorded (type-state transition)
        let sent_recorded = self.record_client_to_backend(client_to_backend);
        let recv_recorded = self.record_backend_to_client(backend_to_client);

        (sent_recorded, recv_recorded)
    }

    #[inline]
    pub fn backend_connection_closed(&self, backend_id: BackendId) {
        self.with_backend(backend_id, |b| {
            b.active_connections.fetch_sub(1, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn record_error_4xx(&self, backend_id: BackendId) {
        self.with_backend(backend_id, |b| {
            b.errors_4xx.fetch_add(1, Ordering::Relaxed);
            b.errors.fetch_add(1, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn record_error_5xx(&self, backend_id: BackendId) {
        self.with_backend(backend_id, |b| {
            b.errors_5xx.fetch_add(1, Ordering::Relaxed);
            b.errors.fetch_add(1, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn record_article(&self, backend_id: BackendId, bytes: u64) {
        self.with_backend(backend_id, |b| {
            b.article_bytes_total.fetch_add(bytes, Ordering::Relaxed);
            b.article_count.fetch_add(1, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn record_ttfb_micros(&self, backend_id: BackendId, micros: u64) {
        self.with_backend(backend_id, |b| {
            b.ttfb_micros_total.fetch_add(micros, Ordering::Relaxed);
            b.ttfb_count.fetch_add(1, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn record_send_recv_micros(
        &self,
        backend_id: BackendId,
        send_micros: u64,
        recv_micros: u64,
    ) {
        self.with_backend(backend_id, |b| {
            b.send_micros_total
                .fetch_add(send_micros, Ordering::Relaxed);
            b.recv_micros_total
                .fetch_add(recv_micros, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn set_backend_health(&self, backend_id: BackendId, health: BackendHealthStatus) {
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
    pub fn snapshot(&self, cache: Option<&crate::cache::UnifiedCache>) -> MetricsSnapshot {
        let cache_stats = cache.map(|c| c as &dyn crate::cache::CacheStatsProvider);
        self.snapshot_with_cache(cache_stats)
    }

    /// Create a snapshot with any cache type that implements CacheStatsProvider
    ///
    /// Returns cumulative counters - no rate calculations.
    /// Use `MetricsSnapshot::with_pool_status()` to add pool utilization data.
    pub fn snapshot_with_cache(
        &self,
        cache: Option<&dyn crate::cache::CacheStatsProvider>,
    ) -> MetricsSnapshot {
        let backend_stats: Vec<BackendStats> = self
            .inner
            .backend_metrics
            .iter()
            .enumerate()
            .map(|(id, metrics)| metrics.to_backend_stats(BackendId::from_index(id)))
            .collect();

        let user_stats: Vec<UserStats> = self
            .inner
            .user_metrics
            .iter()
            .map(|entry| entry.value().to_user_stats())
            .collect();

        let total_sent: u64 = backend_stats.iter().map(|b| b.bytes_sent.as_u64()).sum();
        let total_received: u64 = backend_stats
            .iter()
            .map(|b| b.bytes_received.as_u64())
            .sum();

        let (cache_entries, cache_size_bytes, cache_hit_rate, disk_cache) = cache
            .map(|c| {
                let stats = c.display_stats();
                let disk = stats.disk.map(|d| super::snapshot::DiskCacheStats {
                    disk_hits: d.disk_hits,
                    disk_hit_rate: d.disk_hit_rate,
                    disk_capacity: d.capacity,
                    bytes_written: d.bytes_written,
                    bytes_read: d.bytes_read,
                    write_ios: d.write_ios,
                    read_ios: d.read_ios,
                });
                (stats.entry_count, stats.size_bytes, stats.hit_rate, disk)
            })
            .unwrap_or((0, 0, 0.0, None));

        MetricsSnapshot {
            total_connections: self.inner.total_connections.load(Ordering::Relaxed),
            active_connections: self.inner.active_connections.load(Ordering::Relaxed),
            stateful_sessions: self.inner.stateful_sessions.load(Ordering::Relaxed),
            client_to_backend_bytes: ClientToBackendBytes::new(total_sent),
            backend_to_client_bytes: BackendToClientBytes::new(total_received),
            uptime: self.inner.start_time.elapsed(),
            backend_stats: Arc::new(backend_stats),
            user_stats,
            cache_entries,
            cache_size_bytes,
            cache_hit_rate,
            disk_cache,
        }
    }

    /// Get the number of backends being tracked
    #[must_use]
    #[inline]
    pub fn num_backends(&self) -> usize {
        self.inner.backend_metrics.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backend_metrics_default() {
        let metrics = BackendMetrics::default();
        let stats = metrics.to_backend_stats(BackendId::from_index(0));

        assert_eq!(stats.total_commands.get(), 0);
        assert_eq!(stats.bytes_sent.as_u64(), 0);
        assert_eq!(stats.bytes_received.as_u64(), 0);
        assert_eq!(stats.errors.get(), 0);
    }

    #[test]
    fn test_backend_metrics_to_backend_stats() {
        let metrics = BackendMetrics::default();
        metrics.total_commands.store(42, Ordering::Relaxed);
        metrics.bytes_sent.store(1024, Ordering::Relaxed);
        metrics.bytes_received.store(2048, Ordering::Relaxed);
        metrics.errors.store(3, Ordering::Relaxed);

        let stats = metrics.to_backend_stats(BackendId::from_index(5));

        assert_eq!(stats.backend_id.as_index(), 5);
        assert_eq!(stats.total_commands.get(), 42);
        assert_eq!(stats.bytes_sent.as_u64(), 1024);
        assert_eq!(stats.bytes_received.as_u64(), 2048);
        assert_eq!(stats.errors.get(), 3);
    }

    #[test]
    fn test_user_metrics_new() {
        let metrics = UserMetrics::new("testuser".to_string());
        assert_eq!(metrics.username, "testuser");
        assert_eq!(metrics.total_commands, 0);
        assert_eq!(metrics.bytes_sent, 0);
        assert_eq!(metrics.bytes_received, 0);
    }

    #[test]
    fn test_user_metrics_to_user_stats() {
        let mut metrics = UserMetrics::new("alice".to_string());
        metrics.total_commands = 100;
        metrics.bytes_sent = 5000;
        metrics.bytes_received = 10000;
        metrics.errors = 2;

        let stats = metrics.to_user_stats();
        assert_eq!(stats.username, "alice");
        assert_eq!(stats.total_commands.get(), 100);
        assert_eq!(stats.bytes_sent.as_u64(), 5000);
        assert_eq!(stats.bytes_received.as_u64(), 10000);
        assert_eq!(stats.errors.get(), 2);
    }

    #[test]
    fn test_metrics_collector_new() {
        let collector = MetricsCollector::new(3);
        assert_eq!(collector.num_backends(), 3);
    }

    #[test]
    fn test_metrics_collector_record_command() {
        let collector = MetricsCollector::new(2);
        let backend_id = BackendId::from_index(0);

        collector.record_command(backend_id);
        collector.record_command(backend_id);

        let snapshot = collector.snapshot(None);
        assert_eq!(snapshot.backend_stats[0].total_commands.get(), 2);
    }

    #[test]
    fn test_metrics_collector_record_bytes() {
        let collector = MetricsCollector::new(1);
        let backend_id = BackendId::from_index(0);

        collector.record_client_to_backend_bytes_for(backend_id, 1024);
        collector.record_backend_to_client_bytes_for(backend_id, 2048);

        let snapshot = collector.snapshot(None);
        assert_eq!(snapshot.backend_stats[0].bytes_sent.as_u64(), 1024);
        assert_eq!(snapshot.backend_stats[0].bytes_received.as_u64(), 2048);
    }

    #[test]
    fn test_metrics_collector_record_errors() {
        let collector = MetricsCollector::new(1);
        let backend_id = BackendId::from_index(0);

        collector.record_error(backend_id);
        collector.record_error(backend_id);
        collector.record_error(backend_id);

        let snapshot = collector.snapshot(None);
        assert_eq!(snapshot.backend_stats[0].errors.get(), 3);
    }

    #[test]
    fn test_metrics_collector_connection_lifecycle() {
        let collector = MetricsCollector::new(1);

        assert_eq!(collector.snapshot(None).active_connections, 0);
        assert_eq!(collector.snapshot(None).total_connections, 0);

        collector.connection_opened();
        assert_eq!(collector.snapshot(None).active_connections, 1);
        assert_eq!(collector.snapshot(None).total_connections, 1);

        collector.connection_opened();
        assert_eq!(collector.snapshot(None).active_connections, 2);
        assert_eq!(collector.snapshot(None).total_connections, 2);

        collector.connection_closed();
        assert_eq!(collector.snapshot(None).active_connections, 1);
        assert_eq!(collector.snapshot(None).total_connections, 2);
    }

    #[test]
    fn test_metrics_collector_stateful_sessions() {
        let collector = MetricsCollector::new(1);

        collector.stateful_session_started();
        assert_eq!(collector.snapshot(None).stateful_sessions, 1);

        collector.stateful_session_started();
        assert_eq!(collector.snapshot(None).stateful_sessions, 2);

        collector.stateful_session_ended();
        assert_eq!(collector.snapshot(None).stateful_sessions, 1);
    }

    #[test]
    fn test_metrics_collector_user_metrics() {
        let collector = MetricsCollector::new(1);

        collector.user_connection_opened(Some("alice"));
        collector.user_bytes_sent(Some("alice"), 1000);
        collector.user_bytes_received(Some("alice"), 2000);
        collector.user_command(Some("alice"));

        let snapshot = collector.snapshot(None);
        let alice_stats = snapshot.user_stats.iter().find(|s| s.username == "alice");
        assert!(alice_stats.is_some());

        let stats = alice_stats.unwrap();
        assert_eq!(stats.bytes_sent.as_u64(), 1000);
        assert_eq!(stats.bytes_received.as_u64(), 2000);
        assert_eq!(stats.total_commands.get(), 1);
    }

    #[test]
    fn test_metrics_collector_multiple_users() {
        let collector = MetricsCollector::new(1);

        collector.user_connection_opened(Some("alice"));
        collector.user_bytes_sent(Some("alice"), 100);

        collector.user_connection_opened(Some("bob"));
        collector.user_bytes_sent(Some("bob"), 200);

        let snapshot = collector.snapshot(None);
        assert_eq!(snapshot.user_stats.len(), 2);

        let alice = snapshot.user_stats.iter().find(|s| s.username == "alice");
        let bob = snapshot.user_stats.iter().find(|s| s.username == "bob");

        assert!(alice.is_some());
        assert!(bob.is_some());
        assert_eq!(alice.unwrap().bytes_sent.as_u64(), 100);
        assert_eq!(bob.unwrap().bytes_sent.as_u64(), 200);
    }

    #[test]
    fn test_metrics_collector_snapshot_aggregation() {
        let collector = MetricsCollector::new(2);

        // Backend 0
        collector.record_client_to_backend_bytes_for(BackendId::from_index(0), 1000);
        collector.record_backend_to_client_bytes_for(BackendId::from_index(0), 2000);

        // Backend 1
        collector.record_client_to_backend_bytes_for(BackendId::from_index(1), 500);
        collector.record_backend_to_client_bytes_for(BackendId::from_index(1), 1500);

        let snapshot = collector.snapshot(None);

        // Total should be sum of both backends
        assert_eq!(snapshot.client_to_backend_bytes.as_u64(), 1500);
        assert_eq!(snapshot.backend_to_client_bytes.as_u64(), 3500);
    }

    #[test]
    fn test_metrics_collector_clone() {
        let collector1 = MetricsCollector::new(1);
        collector1.record_command(BackendId::from_index(0));

        let collector2 = collector1.clone();
        collector2.record_command(BackendId::from_index(0));

        // Both should see the same data (Arc shared)
        assert_eq!(
            collector1.snapshot(None).backend_stats[0]
                .total_commands
                .get(),
            2
        );
        assert_eq!(
            collector2.snapshot(None).backend_stats[0]
                .total_commands
                .get(),
            2
        );
    }
}
