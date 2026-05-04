//! Lock-free metrics collector

use super::store::{BackendStore, MetricsStore, UserMetrics};
use super::{BackendHealthStatus, BackendStats, MetricsSnapshot, UserStats};
use crate::types::{BackendId, BackendToClientBytes, ClientToBackendBytes};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::time::Instant;

// ============================================================================
// Internal Storage Types
// ============================================================================

/// Backend metrics wrapper: live gauges only
///
/// Live gauges (`active_connections`, `health_status`) are ephemeral.
/// Cumulative counters are stored in `MetricsStore.backend_stores`[idx].
#[derive(Debug)]
struct BackendMetrics {
    active_connections: AtomicUsize,
    health_status: AtomicU8,
}

impl Default for BackendMetrics {
    fn default() -> Self {
        Self {
            active_connections: AtomicUsize::new(0),
            health_status: AtomicU8::new(BackendHealthStatus::Healthy as u8),
        }
    }
}

impl BackendMetrics {
    /// Convert to immutable snapshot (public for testing)
    #[must_use]
    pub fn to_backend_stats(&self, backend_id: BackendId, store: &BackendStore) -> BackendStats {
        use super::types::{
            ActiveConnections, ArticleCount, CommandCount, ErrorCount, FailureCount, RecvMicros,
            SendMicros, TtfbMicros,
        };
        use crate::types::{ArticleBytesTotal, BytesReceived, BytesSent, TimingMeasurementCount};
        BackendStats {
            backend_id,
            active_connections: ActiveConnections::new(
                self.active_connections.load(Ordering::Relaxed),
            ),
            total_commands: CommandCount::new(store.total_commands.load(Ordering::Relaxed)),
            bytes_sent: BytesSent::new(store.bytes_sent.load(Ordering::Relaxed)),
            bytes_received: BytesReceived::new(store.bytes_received.load(Ordering::Relaxed)),
            errors: ErrorCount::new(store.errors.load(Ordering::Relaxed)),
            errors_4xx: ErrorCount::new(store.errors_4xx.load(Ordering::Relaxed)),
            errors_5xx: ErrorCount::new(store.errors_5xx.load(Ordering::Relaxed)),
            article_bytes_total: ArticleBytesTotal::new(
                store.article_bytes_total.load(Ordering::Relaxed),
            ),
            article_count: ArticleCount::new(store.article_count.load(Ordering::Relaxed)),
            ttfb_micros_total: TtfbMicros::new(store.ttfb_micros_total.load(Ordering::Relaxed)),
            ttfb_count: TimingMeasurementCount::new(store.ttfb_count.load(Ordering::Relaxed)),
            send_micros_total: SendMicros::new(store.send_micros_total.load(Ordering::Relaxed)),
            recv_micros_total: RecvMicros::new(store.recv_micros_total.load(Ordering::Relaxed)),
            connection_failures: FailureCount::new(
                store.connection_failures.load(Ordering::Relaxed),
            ),
            health_status: self.health_status.load(Ordering::Relaxed).into(),
        }
    }
}

// UserMetrics is now imported from store module (no longer duplicated here)

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
    store: MetricsStore,
    active_connections: AtomicUsize,
    stateful_sessions: AtomicUsize,
    backend_metrics: Vec<BackendMetrics>,
    start_time: Instant,
    // Note: client_to_backend_bytes and backend_to_client_bytes are calculated
    // from backend_metrics sums, not stored separately
}

impl MetricsCollector {
    #[inline]
    fn backend_get(&self, backend_id: BackendId) -> Option<&BackendMetrics> {
        self.inner.backend_metrics.get(backend_id.as_index())
    }

    /// Helper for record methods: access backend store by ID
    ///
    /// Out-of-bounds IDs silently no-op (safe for concurrent server addition/removal)
    #[inline]
    fn with_backend_store<F>(&self, backend_id: BackendId, action: F)
    where
        F: FnOnce(&BackendStore),
    {
        let idx = backend_id.as_index();
        if let Some(store) = self.inner.store.backend_stores.get(idx) {
            action(store);
        }
    }

    /// Update or insert user metrics using a closure (lock-free fast path)
    #[inline]
    fn update_user_metrics<F>(&self, username: Option<&str>, update: F)
    where
        F: Fn(&mut UserMetrics),
    {
        let key = username.unwrap_or(crate::constants::user::ANONYMOUS);

        // Fast path: zero-alloc &str lookup (DashMap String key supports Borrow<str>)
        if let Some(mut entry) = self.inner.store.user_metrics.get_mut(key) {
            update(&mut entry);
            return;
        }

        // Slow path: insert once per user, then update
        let owned = key.to_string();
        self.inner
            .store
            .user_metrics
            .entry(owned)
            .or_insert_with(|| UserMetrics::new(key.to_string()));
        if let Some(mut entry) = self.inner.store.user_metrics.get_mut(key) {
            update(&mut entry);
        }
    }

    /// Create a new metrics collector
    ///
    /// # Arguments
    /// * `num_backends` - Number of backend servers to track
    #[must_use]
    pub fn new(num_backends: usize) -> Self {
        Self::with_store(MetricsStore::new(num_backends))
    }

    /// Create a metrics collector with a restored store (for persistence)
    ///
    /// The store contains all cumulative counters that were saved to disk.
    /// Live gauges (`active_connections`, `health_status`) start at zero.
    pub fn with_store(store: MetricsStore) -> Self {
        let num_backends = store.backend_stores.len();
        let backend_metrics = (0..num_backends)
            .map(|_| BackendMetrics::default())
            .collect();

        Self {
            inner: Arc::new(MetricsInner {
                store,
                active_connections: AtomicUsize::new(0),
                stateful_sessions: AtomicUsize::new(0),
                backend_metrics,
                start_time: Instant::now(),
            }),
        }
    }

    /// Save metrics to disk (for persistence)
    ///
    /// # Errors
    /// Returns any serialization or filesystem error from persisting the metrics
    /// snapshot to `path`.
    pub fn save_to_disk(&self, path: &Path, server_names: &[String]) -> anyhow::Result<()> {
        self.inner.store.save(path, server_names)
    }

    // Connection lifecycle tracking

    #[inline]
    pub fn connection_opened(&self) {
        self.inner
            .store
            .total_connections
            .fetch_add(1, Ordering::Relaxed);
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

    // Backend metrics recording (store access via with_backend_store, live gauges via backend_get)

    #[inline]
    pub fn record_command(&self, backend_id: BackendId) {
        self.with_backend_store(backend_id, |store| {
            store.total_commands.fetch_add(1, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn record_connection_failure(&self, backend_id: BackendId) {
        self.with_backend_store(backend_id, |store| {
            store.connection_failures.fetch_add(1, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn record_error(&self, backend_id: BackendId) {
        self.with_backend_store(backend_id, |store| {
            store.errors.fetch_add(1, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn record_client_to_backend_bytes_for(&self, backend_id: BackendId, bytes: u64) {
        self.with_backend_store(backend_id, |store| {
            store.bytes_sent.fetch_add(bytes, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn record_backend_to_client_bytes_for(&self, backend_id: BackendId, bytes: u64) {
        self.with_backend_store(backend_id, |store| {
            store.bytes_received.fetch_add(bytes, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn backend_connection_opened(&self, backend_id: BackendId) {
        if let Some(backend) = self.backend_get(backend_id) {
            backend.active_connections.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Type-safe recording: consumes unrecorded bytes and returns recorded marker
    #[inline]
    #[must_use]
    pub const fn record_client_to_backend(
        &self,
        bytes: crate::types::MetricsBytes<crate::types::Unrecorded>,
    ) -> crate::types::MetricsBytes<crate::types::Recorded> {
        crate::types::MetricsBytes::new(bytes.into_u64()).mark_recorded()
    }

    /// Type-safe recording: consumes unrecorded bytes and returns recorded marker
    #[inline]
    #[must_use]
    pub const fn record_backend_to_client(
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
    #[must_use]
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
        if let Some(backend) = self.backend_get(backend_id) {
            backend.active_connections.fetch_sub(1, Ordering::Relaxed);
        }
    }

    #[inline]
    pub fn record_error_4xx(&self, backend_id: BackendId) {
        self.with_backend_store(backend_id, |store| {
            store.errors_4xx.fetch_add(1, Ordering::Relaxed);
            store.errors.fetch_add(1, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn record_error_5xx(&self, backend_id: BackendId) {
        self.with_backend_store(backend_id, |store| {
            store.errors_5xx.fetch_add(1, Ordering::Relaxed);
            store.errors.fetch_add(1, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn record_article(&self, backend_id: BackendId, bytes: u64) {
        self.with_backend_store(backend_id, |store| {
            store
                .article_bytes_total
                .fetch_add(bytes, Ordering::Relaxed);
            store.article_count.fetch_add(1, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn record_ttfb_micros(&self, backend_id: BackendId, micros: u64) {
        self.with_backend_store(backend_id, |store| {
            store.ttfb_micros_total.fetch_add(micros, Ordering::Relaxed);
            store.ttfb_count.fetch_add(1, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn record_send_recv_micros(
        &self,
        backend_id: BackendId,
        send_micros: u64,
        recv_micros: u64,
    ) {
        self.with_backend_store(backend_id, |store| {
            store
                .send_micros_total
                .fetch_add(send_micros, Ordering::Relaxed);
            store
                .recv_micros_total
                .fetch_add(recv_micros, Ordering::Relaxed);
        });
    }

    #[inline]
    pub fn set_backend_health(&self, backend_id: BackendId, health: BackendHealthStatus) {
        if let Some(backend) = self.backend_get(backend_id) {
            backend
                .health_status
                .store(health.into(), Ordering::Relaxed);
        }
    }

    // User metrics recording (functional update pattern)

    pub fn user_connection_opened(&self, username: Option<&str>) {
        self.update_user_metrics(username, |m| {
            m.active_connections += 1;
            m.total_connections += 1;
        });
    }

    pub fn user_connection_closed(&self, username: Option<&str>) {
        let key = username.unwrap_or(crate::constants::user::ANONYMOUS);
        if let Some(mut m) = self.inner.store.user_metrics.get_mut(key) {
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

    // Pipeline metrics

    /// Record a pipelined batch execution.
    ///
    /// `batch_size` is the number of commands in the batch. Only called for
    /// batches with more than 1 command (single-command batches use the
    /// existing sequential path and are not counted).
    #[inline]
    pub fn record_pipeline_batch(&self, batch_size: u64) {
        self.inner
            .store
            .pipeline_batches
            .fetch_add(1, Ordering::Relaxed);
        self.inner
            .store
            .pipeline_commands
            .fetch_add(batch_size, Ordering::Relaxed);
    }

    /// Record a request being enqueued to a pipeline queue
    #[inline]
    pub fn record_pipeline_enqueue(&self) {
        self.inner
            .store
            .pipeline_requests_queued
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Record a request being completed via pipeline
    #[inline]
    pub fn record_pipeline_complete(&self) {
        self.inner
            .store
            .pipeline_requests_completed
            .fetch_add(1, Ordering::Relaxed);
    }

    // Snapshot creation (pure functional transformations)

    /// Create a snapshot of current metrics (functional pipeline)
    ///
    /// Returns cumulative counters - no rate calculations.
    /// Use `MetricsSnapshot::with_pool_status()` to add pool utilization data.
    #[must_use]
    pub fn snapshot(&self, cache: Option<&crate::cache::UnifiedCache>) -> MetricsSnapshot {
        let cache_stats = cache.map(|c| c as &dyn crate::cache::CacheStatsProvider);
        self.snapshot_with_cache(cache_stats)
    }

    /// Create a snapshot with any cache type that implements `CacheStatsProvider`
    ///
    /// Returns cumulative counters - no rate calculations.
    /// Use `MetricsSnapshot::with_pool_status()` to add pool utilization data.
    #[must_use]
    pub fn snapshot_with_cache(
        &self,
        cache: Option<&dyn crate::cache::CacheStatsProvider>,
    ) -> MetricsSnapshot {
        let backend_stats: Vec<BackendStats> = self
            .inner
            .backend_metrics
            .iter()
            .enumerate()
            .map(|(id, metrics)| {
                let backend_id = BackendId::from_index(id);
                let store = &self.inner.store.backend_stores[id];
                metrics.to_backend_stats(backend_id, store)
            })
            .collect();

        let user_stats: Vec<UserStats> = self
            .inner
            .store
            .user_metrics
            .iter()
            .map(|entry| entry.value().to_user_stats())
            .collect();

        let total_sent: u64 = backend_stats.iter().map(|b| b.bytes_sent.as_u64()).sum();
        let total_received: u64 = backend_stats
            .iter()
            .map(|b| b.bytes_received.as_u64())
            .sum();

        let (cache_entries, cache_size_bytes, cache_hit_rate, disk_cache) =
            cache.map_or((0, 0, 0.0, None), |c| {
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
            });

        MetricsSnapshot {
            total_connections: self.inner.store.total_connections.load(Ordering::Relaxed),
            active_connections: self.inner.active_connections.load(Ordering::Relaxed),
            stateful_sessions: self.inner.stateful_sessions.load(Ordering::Relaxed),
            client_to_backend_bytes: ClientToBackendBytes::new(total_sent),
            backend_to_client_bytes: BackendToClientBytes::new(total_received),
            uptime: self.inner.start_time.elapsed(),
            backend_stats: backend_stats.into(),
            user_stats,
            cache_entries,
            cache_size_bytes,
            cache_hit_rate,
            disk_cache,
            pipeline_batches: self.inner.store.pipeline_batches.load(Ordering::Relaxed),
            pipeline_commands: self.inner.store.pipeline_commands.load(Ordering::Relaxed),
            pipeline_requests_queued: self
                .inner
                .store
                .pipeline_requests_queued
                .load(Ordering::Relaxed),
            pipeline_requests_completed: self
                .inner
                .store
                .pipeline_requests_completed
                .load(Ordering::Relaxed),
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
        let store = BackendStore::default();
        let stats = metrics.to_backend_stats(BackendId::from_index(0), &store);

        assert_eq!(stats.total_commands.get(), 0);
        assert_eq!(stats.bytes_sent.as_u64(), 0);
        assert_eq!(stats.bytes_received.as_u64(), 0);
        assert_eq!(stats.errors.get(), 0);
    }

    #[test]
    fn test_backend_metrics_to_backend_stats() {
        let metrics = BackendMetrics::default();
        let store = BackendStore::default();
        store.total_commands.store(42, Ordering::Relaxed);
        store.bytes_sent.store(1024, Ordering::Relaxed);
        store.bytes_received.store(2048, Ordering::Relaxed);
        store.errors.store(3, Ordering::Relaxed);

        let stats = metrics.to_backend_stats(BackendId::from_index(5), &store);

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

    #[test]
    fn test_metrics_collector_with_store_records_to_correct_store() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let stats_path = temp_dir.path().join("stats.json");
        let server_names = vec!["backend0".to_string(), "backend1".to_string()];

        // Create initial store and save it
        let initial_store = MetricsStore::new(2);
        initial_store.backend_stores[0]
            .total_commands
            .store(100, Ordering::Relaxed);
        initial_store.backend_stores[1]
            .total_commands
            .store(200, Ordering::Relaxed);
        initial_store.save(&stats_path, &server_names).unwrap();

        // Load the store
        let loaded_store = MetricsStore::load(&stats_path, &server_names)
            .unwrap()
            .expect("Should load store");

        // CRITICAL: Create collector with loaded store (this is the bug fix)
        let collector = MetricsCollector::with_store(loaded_store);

        // Record additional commands
        collector.record_command(BackendId::from_index(0)); // Should go to 101
        collector.record_command(BackendId::from_index(1)); // Should go to 201
        collector.record_command(BackendId::from_index(0)); // Should go to 102

        // Verify snapshot shows the correct totals (loaded + new)
        let snapshot = collector.snapshot(None);
        assert_eq!(
            snapshot.backend_stats[0].total_commands.get(),
            102,
            "Backend 0 should have loaded 100 + recorded 2 = 102"
        );
        assert_eq!(
            snapshot.backend_stats[1].total_commands.get(),
            201,
            "Backend 1 should have loaded 200 + recorded 1 = 201"
        );

        // Save again
        collector.save_to_disk(&stats_path, &server_names).unwrap();

        // Load a second time and verify persistence
        let final_store = MetricsStore::load(&stats_path, &server_names)
            .unwrap()
            .expect("Should load store");
        assert_eq!(
            final_store.backend_stores[0]
                .total_commands
                .load(Ordering::Relaxed),
            102,
            "Backend 0 should have 102 persisted"
        );
        assert_eq!(
            final_store.backend_stores[1]
                .total_commands
                .load(Ordering::Relaxed),
            201,
            "Backend 1 should have 201 persisted"
        );
    }

    #[test]
    fn test_out_of_bounds_backend_id_is_noop() {
        let collector = MetricsCollector::new(2);
        let bad_id = BackendId::from_index(99);

        // All these should silently no-op (not panic)
        collector.record_command(bad_id);
        collector.record_connection_failure(bad_id);
        collector.record_error(bad_id);
        collector.record_client_to_backend_bytes_for(bad_id, 1000);
        collector.record_backend_to_client_bytes_for(bad_id, 2000);
        collector.record_error_4xx(bad_id);
        collector.record_error_5xx(bad_id);
        collector.record_article(bad_id, 5000);
        collector.record_ttfb_micros(bad_id, 100);
        collector.record_send_recv_micros(bad_id, 50, 75);

        // Verify nothing changed
        let snapshot = collector.snapshot(None);
        assert_eq!(snapshot.backend_stats[0].total_commands.get(), 0);
        assert_eq!(snapshot.backend_stats[1].total_commands.get(), 0);
    }

    #[test]
    fn test_record_error_4xx_increments_both_counters() {
        let collector = MetricsCollector::new(1);
        let backend_id = BackendId::from_index(0);

        collector.record_error_4xx(backend_id);
        collector.record_error_4xx(backend_id);

        let snapshot = collector.snapshot(None);
        assert_eq!(
            snapshot.backend_stats[0].errors_4xx.get(),
            2,
            "errors_4xx should increment by 2"
        );
        assert_eq!(
            snapshot.backend_stats[0].errors.get(),
            2,
            "errors total should also increment by 2"
        );
    }

    #[test]
    fn test_record_error_5xx_increments_both_counters() {
        let collector = MetricsCollector::new(1);
        let backend_id = BackendId::from_index(0);

        collector.record_error_5xx(backend_id);
        collector.record_error_5xx(backend_id);
        collector.record_error_5xx(backend_id);

        let snapshot = collector.snapshot(None);
        assert_eq!(
            snapshot.backend_stats[0].errors_5xx.get(),
            3,
            "errors_5xx should increment by 3"
        );
        assert_eq!(
            snapshot.backend_stats[0].errors.get(),
            3,
            "errors total should also increment by 3"
        );
    }

    #[test]
    fn test_mixed_error_types_compose_correctly() {
        let collector = MetricsCollector::new(1);
        let backend_id = BackendId::from_index(0);

        // Mix different error types
        collector.record_error(backend_id); // Plain error (errors: 1)
        collector.record_error_4xx(backend_id); // 4xx (errors_4xx: 1, errors: 2)
        collector.record_error_5xx(backend_id); // 5xx (errors_5xx: 1, errors: 3)
        collector.record_error(backend_id); // Plain error again (errors: 4)

        let snapshot = collector.snapshot(None);
        assert_eq!(
            snapshot.backend_stats[0].errors.get(),
            4,
            "errors total: 2 plain + 1 from 4xx + 1 from 5xx = 4"
        );
        assert_eq!(
            snapshot.backend_stats[0].errors_4xx.get(),
            1,
            "errors_4xx should be 1"
        );
        assert_eq!(
            snapshot.backend_stats[0].errors_5xx.get(),
            1,
            "errors_5xx should be 1"
        );
    }

    #[test]
    fn test_concurrent_recording() {
        use std::sync::Arc;
        use std::thread;

        let collector = Arc::new(MetricsCollector::new(1));
        let backend_id = BackendId::from_index(0);
        let mut handles = vec![];

        // 4 threads, each recording 1000 commands
        for _ in 0..4 {
            let collector = Arc::clone(&collector);
            let handle = thread::spawn(move || {
                for _ in 0..1000 {
                    collector.record_command(backend_id);
                }
            });
            handles.push(handle);
        }

        // Wait for all threads
        for handle in handles {
            handle.join().unwrap();
        }

        // Should have exactly 4000 commands (4 threads * 1000)
        let snapshot = collector.snapshot(None);
        assert_eq!(
            snapshot.backend_stats[0].total_commands.get(),
            4000,
            "4 threads x 1000 iterations = 4000 total commands"
        );
    }
}
