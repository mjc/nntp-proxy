//! Real-time metrics collection for the NNTP proxy
//!
//! This module provides lock-free, thread-safe metrics tracking using atomic operations.
//! Metrics are designed to be updated frequently from hot paths with minimal overhead.

mod connection_stats;

pub use connection_stats::ConnectionStatsAggregator;

use crate::types::BackendBytes;
use dashmap::DashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Backend health status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    /// Backend is healthy and responding normally
    Healthy,
    /// Backend is degraded (high error rate or slow)
    Degraded,
    /// Backend is down or unreachable
    Down,
}

impl From<u8> for HealthStatus {
    fn from(value: u8) -> Self {
        match value {
            0 => Self::Healthy,
            1 => Self::Degraded,
            2 => Self::Down,
            _ => Self::Healthy,
        }
    }
}

impl From<HealthStatus> for u8 {
    fn from(status: HealthStatus) -> Self {
        match status {
            HealthStatus::Healthy => 0,
            HealthStatus::Degraded => 1,
            HealthStatus::Down => 2,
        }
    }
}

/// Thread-safe metrics collector for the entire proxy
///
/// Uses atomic operations for lock-free updates from multiple threads.
/// All methods are safe to call concurrently from any thread.
#[derive(Debug, Clone)]
pub struct MetricsCollector {
    inner: Arc<MetricsInner>,
}

#[derive(Debug)]
struct MetricsInner {
    // Connection metrics
    total_connections: AtomicU64,
    active_connections: AtomicUsize,

    // Hybrid mode tracking - count of sessions currently in stateful mode
    stateful_sessions: AtomicUsize,

    // Per-backend metrics (indexed by backend ID)
    backend_metrics: Vec<BackendMetrics>,

    // Per-user metrics (lock-free concurrent HashMap)
    // Uses Arc<str> keys to avoid String allocations
    // DashMap provides concurrent access without locks
    user_metrics: DashMap<Arc<str>, UserMetrics>,

    // Start time for uptime calculation
    start_time: Instant,
}

/// Metrics for a single backend server
#[derive(Debug)]
struct BackendMetrics {
    active_connections: AtomicUsize,
    total_commands: AtomicU64,
    bytes_sent: AtomicU64,
    bytes_received: AtomicU64,
    errors: AtomicU64,
    // Error classification
    errors_4xx: AtomicU64,
    errors_5xx: AtomicU64,
    // Article size tracking (for averaging)
    article_bytes_total: AtomicU64,
    article_count: AtomicU64,
    // Latency tracking (for averaging)
    latency_micros_total: AtomicU64,
    latency_count: AtomicU64,
    // Connection failures
    connection_failures: AtomicU64,
    // Health status (0 = healthy, 1 = degraded, 2 = down)
    health_status: AtomicU8,
}

/// Metrics for a single user (internal storage)
#[derive(Debug, Clone)]
struct UserMetrics {
    username: Arc<str>, // Share username across snapshots to avoid cloning
    active_connections: usize,
    total_connections: u64,
    bytes_sent: u64,
    bytes_received: u64,
    total_commands: u64,
    errors: u64,
}

/// Snapshot of current metrics (for display/reporting)
#[derive(Debug, Clone)]
pub struct MetricsSnapshot {
    pub total_connections: u64,
    pub active_connections: usize,
    pub stateful_sessions: usize,
    // Traffic flows
    pub client_to_backend_bytes: BackendBytes, // Commands from client to backend
    pub backend_to_client_bytes: BackendBytes, // Article data from backend to client
    pub uptime: Duration,
    pub backend_stats: Vec<BackendStats>,
    pub user_stats: Vec<UserStats>,
}

/// Statistics for a single backend
#[derive(Debug, Clone)]
pub struct BackendStats {
    pub backend_id: usize,
    pub active_connections: usize,
    pub total_commands: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub errors: u64,
    pub errors_4xx: u64,
    pub errors_5xx: u64,
    pub article_bytes_total: u64,
    pub article_count: u64,
    pub latency_micros_total: u64,
    pub latency_count: u64,
    pub connection_failures: u64,
    pub health_status: HealthStatus,
}

/// Statistics for a single user
#[derive(Debug, Clone)]
pub struct UserStats {
    pub username: Arc<str>, // Shared reference to avoid allocations
    pub active_connections: usize,
    pub total_connections: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub total_commands: u64,
    pub errors: u64,
}

impl MetricsCollector {
    /// Create a new metrics collector
    ///
    /// # Arguments
    /// * `num_backends` - Number of backend servers to track
    pub fn new(num_backends: usize) -> Self {
        let backend_metrics = (0..num_backends)
            .map(|_| BackendMetrics {
                active_connections: AtomicUsize::new(0),
                total_commands: AtomicU64::new(0),
                bytes_sent: AtomicU64::new(0),
                bytes_received: AtomicU64::new(0),
                errors: AtomicU64::new(0),
                errors_4xx: AtomicU64::new(0),
                errors_5xx: AtomicU64::new(0),
                article_bytes_total: AtomicU64::new(0),
                article_count: AtomicU64::new(0),
                latency_micros_total: AtomicU64::new(0),
                latency_count: AtomicU64::new(0),
                connection_failures: AtomicU64::new(0),
                health_status: AtomicU8::new(0), // Healthy
            })
            .collect();

        Self {
            inner: Arc::new(MetricsInner {
                total_connections: AtomicU64::new(0),
                active_connections: AtomicUsize::new(0),
                stateful_sessions: AtomicUsize::new(0),
                backend_metrics,
                user_metrics: DashMap::with_capacity(64), // Pre-allocate for typical user count
                start_time: Instant::now(),
            }),
        }
    }

    /// Record a new client connection
    #[inline]
    pub fn connection_opened(&self) {
        self.inner.total_connections.fetch_add(1, Ordering::Relaxed);
        self.inner
            .active_connections
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Record a client disconnection
    #[inline]
    pub fn connection_closed(&self) {
        self.inner
            .active_connections
            .fetch_sub(1, Ordering::Relaxed);
    }

    /// Record bytes sent TO a specific backend (commands)
    #[inline]
    pub fn record_client_to_backend_bytes_for(&self, backend_id: usize, bytes: u64) {
        if let Some(backend) = self.inner.backend_metrics.get(backend_id) {
            backend.bytes_sent.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    /// Record bytes received FROM a specific backend (article data)
    #[inline]
    pub fn record_backend_to_client_bytes_for(&self, backend_id: usize, bytes: u64) {
        if let Some(backend) = self.inner.backend_metrics.get(backend_id) {
            backend.bytes_received.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    /// Type-safe recording: consumes unrecorded bytes and returns recorded marker
    ///
    /// This method uses the typestate pattern to prevent double-counting at compile time.
    /// The `Unrecorded` bytes are consumed and cannot be used again.
    ///
    /// Note: Global metrics are calculated by summing per-backend metrics.
    /// Use the per-backend recording methods to actually record bytes.
    #[inline]
    pub fn record_client_to_backend(
        &self,
        bytes: crate::types::MetricsBytes<crate::types::Unrecorded>,
    ) -> crate::types::MetricsBytes<crate::types::Recorded> {
        let count = bytes.into_u64();
        // No global recording - just consume and mark as recorded
        crate::types::MetricsBytes::new(count).mark_recorded()
    }

    /// Type-safe recording: consumes unrecorded bytes and returns recorded marker
    ///
    /// This method uses the typestate pattern to prevent double-counting at compile time.
    /// The `Unrecorded` bytes are consumed and cannot be used again.
    ///
    /// Note: Global metrics are calculated by summing per-backend metrics.
    /// Use the per-backend recording methods to actually record bytes.
    #[inline]
    pub fn record_backend_to_client(
        &self,
        bytes: crate::types::MetricsBytes<crate::types::Unrecorded>,
    ) -> crate::types::MetricsBytes<crate::types::Recorded> {
        let count = bytes.into_u64();
        // No global recording - just consume and mark as recorded
        crate::types::MetricsBytes::new(count).mark_recorded()
    }

    /// Type-safe directional recording
    ///
    /// Records bytes in the appropriate direction based on the transfer type.
    /// Prevents accidentally swapping directions at compile time.
    #[inline]
    pub fn record_directional(
        &self,
        bytes: crate::types::DirectionalBytes<crate::types::Unrecorded>,
    ) -> crate::types::MetricsBytes<crate::types::Recorded> {
        use crate::types::TransferDirection;

        match bytes.direction() {
            TransferDirection::ClientToBackend => self.record_client_to_backend(bytes.into_bytes()),
            TransferDirection::BackendToClient => self.record_backend_to_client(bytes.into_bytes()),
        }
    }

    /// Record a command processed
    #[inline]
    pub fn record_command(&self, backend_id: usize) {
        // Only track per-backend commands, no global total
        if let Some(backend) = self.inner.backend_metrics.get(backend_id) {
            backend.total_commands.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Record a backend error
    #[inline]
    pub fn record_error(&self, backend_id: usize) {
        if let Some(backend) = self.inner.backend_metrics.get(backend_id) {
            backend.errors.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Record a backend connection opened
    #[inline]
    pub fn backend_connection_opened(&self, backend_id: usize) {
        if let Some(backend) = self.inner.backend_metrics.get(backend_id) {
            backend.active_connections.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Record a backend connection closed
    #[inline]
    pub fn backend_connection_closed(&self, backend_id: usize) {
        if let Some(backend) = self.inner.backend_metrics.get(backend_id) {
            backend.active_connections.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Record user connection opened
    pub fn user_connection_opened(&self, username: Option<&str>) {
        let username_arc: Arc<str> = Arc::from(username.unwrap_or("<anonymous>"));
        self.inner
            .user_metrics
            .entry(Arc::clone(&username_arc))
            .and_modify(|metrics| {
                metrics.active_connections += 1;
                metrics.total_connections += 1;
            })
            .or_insert_with(|| UserMetrics {
                username: Arc::clone(&username_arc),
                active_connections: 1,
                total_connections: 1,
                bytes_sent: 0,
                bytes_received: 0,
                total_commands: 0,
                errors: 0,
            });
    }

    /// Record user connection closed
    pub fn user_connection_closed(&self, username: Option<&str>) {
        let username_arc: Arc<str> = Arc::from(username.unwrap_or("<anonymous>"));
        if let Some(mut metrics) = self.inner.user_metrics.get_mut(&username_arc) {
            metrics.active_connections = metrics.active_connections.saturating_sub(1);
        }
    }

    /// Record bytes sent for a specific user
    pub fn user_bytes_sent(&self, username: Option<&str>, bytes: u64) {
        let username_arc: Arc<str> = Arc::from(username.unwrap_or("<anonymous>"));
        self.inner
            .user_metrics
            .entry(Arc::clone(&username_arc))
            .and_modify(|metrics| {
                metrics.bytes_sent += bytes;
            })
            .or_insert_with(|| UserMetrics {
                username: Arc::clone(&username_arc),
                active_connections: 0,
                total_connections: 0,
                bytes_sent: bytes,
                bytes_received: 0,
                total_commands: 0,
                errors: 0,
            });
    }

    /// Record bytes received for a specific user
    pub fn user_bytes_received(&self, username: Option<&str>, bytes: u64) {
        let username_arc: Arc<str> = Arc::from(username.unwrap_or("<anonymous>"));
        self.inner
            .user_metrics
            .entry(Arc::clone(&username_arc))
            .and_modify(|metrics| {
                metrics.bytes_received += bytes;
            })
            .or_insert_with(|| UserMetrics {
                username: Arc::clone(&username_arc),
                active_connections: 0,
                total_connections: 0,
                bytes_sent: 0,
                bytes_received: bytes,
                total_commands: 0,
                errors: 0,
            });
    }

    /// Record a command processed for a specific user
    pub fn user_command(&self, username: Option<&str>) {
        let username_arc: Arc<str> = Arc::from(username.unwrap_or("<anonymous>"));
        self.inner
            .user_metrics
            .entry(Arc::clone(&username_arc))
            .and_modify(|metrics| {
                metrics.total_commands += 1;
            })
            .or_insert_with(|| UserMetrics {
                username: Arc::clone(&username_arc),
                active_connections: 0,
                total_connections: 0,
                bytes_sent: 0,
                bytes_received: 0,
                total_commands: 1,
                errors: 0,
            });
    }

    /// Record an error for a specific user
    pub fn user_error(&self, username: Option<&str>) {
        let username_arc: Arc<str> = Arc::from(username.unwrap_or("<anonymous>"));
        self.inner
            .user_metrics
            .entry(Arc::clone(&username_arc))
            .and_modify(|metrics| {
                metrics.errors += 1;
            })
            .or_insert_with(|| UserMetrics {
                username: Arc::clone(&username_arc),
                active_connections: 0,
                total_connections: 0,
                bytes_sent: 0,
                bytes_received: 0,
                total_commands: 0,
                errors: 1,
            });
    }

    /// Record a session entering stateful mode (hybrid mode tracking)
    #[inline]
    pub fn stateful_session_started(&self) {
        self.inner.stateful_sessions.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a session leaving stateful mode
    #[inline]
    pub fn stateful_session_ended(&self) {
        self.inner.stateful_sessions.fetch_sub(1, Ordering::Relaxed);
    }

    /// Record a 4xx error for a backend
    #[inline]
    pub fn record_error_4xx(&self, backend_id: usize) {
        if let Some(backend) = self.inner.backend_metrics.get(backend_id) {
            backend.errors.fetch_add(1, Ordering::Relaxed);
            backend.errors_4xx.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Record a 5xx error for a backend
    #[inline]
    pub fn record_error_5xx(&self, backend_id: usize) {
        if let Some(backend) = self.inner.backend_metrics.get(backend_id) {
            backend.errors.fetch_add(1, Ordering::Relaxed);
            backend.errors_5xx.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Record an article retrieval with its size
    #[inline]
    pub fn record_article(&self, backend_id: usize, bytes: u64) {
        if let Some(backend) = self.inner.backend_metrics.get(backend_id) {
            backend
                .article_bytes_total
                .fetch_add(bytes, Ordering::Relaxed);
            backend.article_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Record response latency in microseconds
    #[inline]
    pub fn record_latency_micros(&self, backend_id: usize, micros: u64) {
        if let Some(backend) = self.inner.backend_metrics.get(backend_id) {
            backend
                .latency_micros_total
                .fetch_add(micros, Ordering::Relaxed);
            backend.latency_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Record a connection failure
    #[inline]
    pub fn record_connection_failure(&self, backend_id: usize) {
        if let Some(backend) = self.inner.backend_metrics.get(backend_id) {
            backend.connection_failures.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Update backend health status
    #[inline]
    pub fn set_backend_health(&self, backend_id: usize, status: HealthStatus) {
        if let Some(backend) = self.inner.backend_metrics.get(backend_id) {
            backend
                .health_status
                .store(status.into(), Ordering::Relaxed);
        }
    }

    /// Get a snapshot of current metrics
    ///
    /// This creates a consistent point-in-time view of all metrics.
    /// Note: Individual values may be slightly inconsistent due to concurrent updates,
    /// but this is acceptable for display purposes.
    ///
    /// **Note:** `active_connections` per backend is NOT populated here.
    /// It must be populated separately by querying pool status from the router.
    /// Use `MetricsSnapshot::with_pool_status()` to add pool utilization data.
    #[must_use]
    pub fn snapshot(&self) -> MetricsSnapshot {
        let num_backends = self.inner.backend_metrics.len();
        let mut backend_stats = Vec::with_capacity(num_backends);

        // Pre-allocate and populate to avoid allocations
        for (id, metrics) in self.inner.backend_metrics.iter().enumerate() {
            backend_stats.push(BackendStats {
                backend_id: id,
                active_connections: 0, // Will be populated from pool status
                total_commands: metrics.total_commands.load(Ordering::Relaxed),
                bytes_sent: metrics.bytes_sent.load(Ordering::Relaxed),
                bytes_received: metrics.bytes_received.load(Ordering::Relaxed),
                errors: metrics.errors.load(Ordering::Relaxed),
                errors_4xx: metrics.errors_4xx.load(Ordering::Relaxed),
                errors_5xx: metrics.errors_5xx.load(Ordering::Relaxed),
                article_bytes_total: metrics.article_bytes_total.load(Ordering::Relaxed),
                article_count: metrics.article_count.load(Ordering::Relaxed),
                latency_micros_total: metrics.latency_micros_total.load(Ordering::Relaxed),
                latency_count: metrics.latency_count.load(Ordering::Relaxed),
                connection_failures: metrics.connection_failures.load(Ordering::Relaxed),
                health_status: metrics.health_status.load(Ordering::Relaxed).into(),
            });
        }

        // Calculate global totals by summing backend metrics
        let c2b_bytes: u64 = backend_stats.iter().map(|b| b.bytes_sent).sum();
        let b2c_bytes: u64 = backend_stats.iter().map(|b| b.bytes_received).sum();

        // Collect user stats (zero-copy username sharing via Arc<str>)
        // DashMap provides lock-free iteration - no mutex needed!
        let user_stats: Vec<UserStats> = self
            .inner
            .user_metrics
            .iter()
            .map(|entry| {
                let metrics = entry.value();
                UserStats {
                    username: Arc::clone(&metrics.username), // Arc clone is just pointer increment
                    active_connections: metrics.active_connections,
                    total_connections: metrics.total_connections,
                    bytes_sent: metrics.bytes_sent,
                    bytes_received: metrics.bytes_received,
                    total_commands: metrics.total_commands,
                    errors: metrics.errors,
                }
            })
            .collect();

        MetricsSnapshot {
            total_connections: self.inner.total_connections.load(Ordering::Relaxed),
            active_connections: self.inner.active_connections.load(Ordering::Relaxed),
            stateful_sessions: self.inner.stateful_sessions.load(Ordering::Relaxed),
            client_to_backend_bytes: BackendBytes::new(c2b_bytes),
            backend_to_client_bytes: BackendBytes::new(b2c_bytes),
            uptime: self.inner.start_time.elapsed(),
            backend_stats,
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

impl MetricsSnapshot {
    /// Update backend active connections from pool status
    ///
    /// Populates `active_connections` for each backend by querying the connection pool.
    /// Active connections = max_size - available connections.
    pub fn with_pool_status(mut self, router: &crate::router::BackendSelector) -> Self {
        use crate::pool::ConnectionProvider;

        for stats in &mut self.backend_stats {
            let backend_id = crate::types::BackendId::from_index(stats.backend_id);
            if let Some(provider) = router.get_backend_provider(backend_id) {
                let pool_status = provider.status();
                // Active = checked out connections = max - available
                let active = pool_status
                    .max_size
                    .get()
                    .saturating_sub(pool_status.available.get());
                stats.active_connections = active;
            }
        }
        self
    }

    /// Format uptime as a human-readable string
    #[must_use]
    pub fn format_uptime(&self) -> String {
        let secs = self.uptime.as_secs();
        let hours = secs / 3600;
        let minutes = (secs % 3600) / 60;
        let seconds = secs % 60;

        if hours > 0 {
            format!("{}h {}m {}s", hours, minutes, seconds)
        } else if minutes > 0 {
            format!("{}m {}s", minutes, seconds)
        } else {
            format!("{}s", seconds)
        }
    }

    /// Get total bytes transferred (sent + received) across backends
    #[must_use]
    #[inline]
    pub fn total_bytes(&self) -> u64 {
        self.client_to_backend_bytes.as_u64() + self.backend_to_client_bytes.as_u64()
    }

    /// Calculate throughput in bytes per second
    #[must_use]
    pub fn throughput_bps(&self) -> f64 {
        let secs = self.uptime.as_secs_f64();
        if secs > 0.0 {
            self.total_bytes() as f64 / secs
        } else {
            0.0
        }
    }
}

impl BackendStats {
    /// Calculate average article size in bytes
    #[must_use]
    #[inline]
    pub fn average_article_size(&self) -> Option<u64> {
        if self.article_count > 0 {
            Some(self.article_bytes_total / self.article_count)
        } else {
            None
        }
    }

    /// Calculate average latency in milliseconds
    #[must_use]
    #[inline]
    pub fn average_latency_ms(&self) -> Option<f64> {
        if self.latency_count > 0 {
            let avg_micros = self.latency_micros_total as f64 / self.latency_count as f64;
            Some(avg_micros / 1000.0) // Convert to milliseconds
        } else {
            None
        }
    }

    /// Calculate error rate as a percentage
    #[must_use]
    #[inline]
    pub fn error_rate_percent(&self) -> f64 {
        if self.total_commands > 0 {
            (self.errors as f64 / self.total_commands as f64) * 100.0
        } else {
            0.0
        }
    }

    /// Check if backend has high error rate (> 5%)
    #[must_use]
    #[inline]
    pub fn has_high_error_rate(&self) -> bool {
        self.error_rate_percent() > 5.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_collector_creation() {
        let metrics = MetricsCollector::new(3);
        assert_eq!(metrics.num_backends(), 3);

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.total_connections, 0);
        assert_eq!(snapshot.active_connections, 0);
        assert_eq!(snapshot.backend_stats.len(), 3);
    }

    #[test]
    fn test_connection_tracking() {
        let metrics = MetricsCollector::new(2);

        metrics.connection_opened();
        metrics.connection_opened();
        assert_eq!(metrics.snapshot().active_connections, 2);
        assert_eq!(metrics.snapshot().total_connections, 2);

        metrics.connection_closed();
        assert_eq!(metrics.snapshot().active_connections, 1);
        assert_eq!(metrics.snapshot().total_connections, 2);
    }

    #[test]
    fn test_bytes_tracking() {
        let metrics = MetricsCollector::new(2);

        // Record bytes for specific backends
        metrics.record_client_to_backend_bytes_for(0, 512);
        metrics.record_client_to_backend_bytes_for(1, 512);
        metrics.record_backend_to_client_bytes_for(0, 1024);
        metrics.record_backend_to_client_bytes_for(1, 1024);

        let snapshot = metrics.snapshot();
        // Global totals should be sum of all backends
        assert_eq!(snapshot.client_to_backend_bytes.as_u64(), 1024);
        assert_eq!(snapshot.backend_to_client_bytes.as_u64(), 2048);
        // Per-backend totals
        assert_eq!(snapshot.backend_stats[0].bytes_sent, 512);
        assert_eq!(snapshot.backend_stats[0].bytes_received, 1024);
        assert_eq!(snapshot.backend_stats[1].bytes_sent, 512);
        assert_eq!(snapshot.backend_stats[1].bytes_received, 1024);
    }

    #[test]
    fn test_command_tracking() {
        let metrics = MetricsCollector::new(2);

        metrics.record_command(0);
        metrics.record_command(0);
        metrics.record_command(1);

        let snapshot = metrics.snapshot();
        // No global total anymore, just per-backend
        assert_eq!(snapshot.backend_stats[0].total_commands, 2);
        assert_eq!(snapshot.backend_stats[1].total_commands, 1);
    }

    #[test]
    fn test_error_tracking() {
        let metrics = MetricsCollector::new(2);

        metrics.record_error(0);
        metrics.record_error(0);

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.backend_stats[0].errors, 2);
        assert_eq!(snapshot.backend_stats[1].errors, 0);
    }

    #[test]
    fn test_format_uptime() {
        let snapshot = MetricsSnapshot {
            total_connections: 0,
            active_connections: 0,
            stateful_sessions: 0,
            client_to_backend_bytes: BackendBytes::new(0),
            backend_to_client_bytes: BackendBytes::new(0),
            uptime: Duration::from_secs(3665), // 1h 1m 5s
            backend_stats: vec![],
            user_stats: vec![],
        };

        assert_eq!(snapshot.format_uptime(), "1h 1m 5s");
    }

    #[test]
    fn test_throughput_calculation() {
        let snapshot = MetricsSnapshot {
            total_connections: 0,
            active_connections: 0,
            stateful_sessions: 0,
            client_to_backend_bytes: BackendBytes::new(5000),
            backend_to_client_bytes: BackendBytes::new(5000),
            uptime: Duration::from_secs(10),
            backend_stats: vec![],
            user_stats: vec![],
        };

        assert_eq!(snapshot.throughput_bps(), 1000.0); // 10000 bytes / 10 seconds
    }

    #[test]
    fn test_snapshot_without_pool_status() {
        let metrics = MetricsCollector::new(2);
        metrics.record_client_to_backend_bytes_for(0, 100);
        metrics.record_command(1);

        let snapshot = metrics.snapshot();

        // Backend stats should have default (0) active_connections
        assert_eq!(snapshot.backend_stats[0].active_connections, 0);
        assert_eq!(snapshot.backend_stats[1].active_connections, 0);

        // Other metrics should still work
        assert_eq!(snapshot.backend_stats[0].bytes_sent, 100);
        assert_eq!(snapshot.backend_stats[1].total_commands, 1);
    }

    #[test]
    fn test_with_pool_status_integration() {
        use crate::pool::{ConnectionProvider, DeadpoolConnectionProvider};
        use crate::router::BackendSelector;
        use crate::types::{BackendId, ServerName};

        let metrics = MetricsCollector::new(2);

        // Create router with two backends
        let mut router = BackendSelector::new();

        let provider1 = DeadpoolConnectionProvider::new(
            "localhost".to_string(),
            119,
            "Backend 1".to_string(),
            10, // max_size = 10
            None,
            None,
        );

        let provider2 = DeadpoolConnectionProvider::new(
            "localhost".to_string(),
            120,
            "Backend 2".to_string(),
            5, // max_size = 5
            None,
            None,
        );

        router.add_backend(
            BackendId::from_index(0),
            ServerName::new("backend1".to_string()).unwrap(),
            provider1.clone(),
        );

        router.add_backend(
            BackendId::from_index(1),
            ServerName::new("backend2".to_string()).unwrap(),
            provider2.clone(),
        );

        // Get snapshot with pool status
        let snapshot = metrics.snapshot().with_pool_status(&router);

        // Check that active_connections is calculated from pool
        // Since pools are empty initially: active = max_size - available = max_size - max_size = 0
        let status1 = provider1.status();
        let status2 = provider2.status();

        let expected_active1 = status1
            .max_size
            .get()
            .saturating_sub(status1.available.get());
        let expected_active2 = status2
            .max_size
            .get()
            .saturating_sub(status2.available.get());

        assert_eq!(
            snapshot.backend_stats[0].active_connections,
            expected_active1
        );
        assert_eq!(
            snapshot.backend_stats[1].active_connections,
            expected_active2
        );
    }

    #[test]
    fn test_pool_status_calculates_active_correctly() {
        use crate::pool::{ConnectionProvider, DeadpoolConnectionProvider};
        use crate::router::BackendSelector;
        use crate::types::{BackendId, ServerName};

        let metrics = MetricsCollector::new(1);
        let mut router = BackendSelector::new();

        let provider = DeadpoolConnectionProvider::new(
            "localhost".to_string(),
            119,
            "Test Backend".to_string(),
            10,
            None,
            None,
        );

        router.add_backend(
            BackendId::from_index(0),
            ServerName::new("test".to_string()).unwrap(),
            provider.clone(),
        );

        let snapshot = metrics.snapshot().with_pool_status(&router);

        // Verify active = max_size - available
        let pool_status = provider.status();
        let expected = pool_status
            .max_size
            .get()
            .saturating_sub(pool_status.available.get());

        assert_eq!(snapshot.backend_stats[0].active_connections, expected);
    }

    #[test]
    fn test_user_connection_closed_decrements_active() {
        let metrics = MetricsCollector::new(1);

        // Open connection for user
        metrics.user_connection_opened(Some("testuser"));

        let snapshot1 = metrics.snapshot();
        assert_eq!(snapshot1.user_stats.len(), 1);
        assert_eq!(snapshot1.user_stats[0].username.as_ref(), "testuser");
        assert_eq!(snapshot1.user_stats[0].active_connections, 1);
        assert_eq!(snapshot1.user_stats[0].total_connections, 1);

        // Close connection - should decrement active_connections
        metrics.user_connection_closed(Some("testuser"));

        let snapshot2 = metrics.snapshot();
        assert_eq!(snapshot2.user_stats.len(), 1);
        assert_eq!(snapshot2.user_stats[0].active_connections, 0);
        assert_eq!(snapshot2.user_stats[0].total_connections, 1); // Total unchanged
    }

    #[test]
    fn test_user_connection_closed_with_arc_str_key() {
        let metrics = MetricsCollector::new(1);

        // Open multiple connections
        metrics.user_connection_opened(Some("alice"));
        metrics.user_connection_opened(Some("alice"));
        metrics.user_connection_opened(Some("bob"));

        let snapshot1 = metrics.snapshot();
        let alice = snapshot1
            .user_stats
            .iter()
            .find(|u| u.username.as_ref() == "alice")
            .unwrap();
        let bob = snapshot1
            .user_stats
            .iter()
            .find(|u| u.username.as_ref() == "bob")
            .unwrap();

        assert_eq!(alice.active_connections, 2);
        assert_eq!(bob.active_connections, 1);

        // Close one of alice's connections
        metrics.user_connection_closed(Some("alice"));

        let snapshot2 = metrics.snapshot();
        let alice2 = snapshot2
            .user_stats
            .iter()
            .find(|u| u.username.as_ref() == "alice")
            .unwrap();

        assert_eq!(alice2.active_connections, 1); // Decremented from 2 to 1
        assert_eq!(alice2.total_connections, 2); // Unchanged
    }

    #[test]
    fn test_article_size_tracking() {
        let metrics = MetricsCollector::new(1);

        // Record some articles
        metrics.record_article(0, 5000); // 5KB
        metrics.record_article(0, 15000); // 15KB
        metrics.record_article(0, 10000); // 10KB

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.backend_stats[0].article_count, 3);
        assert_eq!(snapshot.backend_stats[0].article_bytes_total, 30000);

        // Average should be 10KB
        assert_eq!(
            snapshot.backend_stats[0].average_article_size(),
            Some(10000)
        );
    }

    #[test]
    fn test_article_size_returns_none_when_no_articles() {
        let metrics = MetricsCollector::new(1);

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.backend_stats[0].article_count, 0);
        assert_eq!(snapshot.backend_stats[0].average_article_size(), None);
    }

    #[test]
    fn test_article_tracking_multiple_backends() {
        let metrics = MetricsCollector::new(3);

        // Backend 0: small articles
        metrics.record_article(0, 1000);
        metrics.record_article(0, 2000);

        // Backend 1: large articles
        metrics.record_article(1, 50000);
        metrics.record_article(1, 100000);

        // Backend 2: no articles

        let snapshot = metrics.snapshot();

        // Backend 0: avg 1.5KB
        assert_eq!(snapshot.backend_stats[0].article_count, 2);
        assert_eq!(snapshot.backend_stats[0].average_article_size(), Some(1500));

        // Backend 1: avg 75KB
        assert_eq!(snapshot.backend_stats[1].article_count, 2);
        assert_eq!(
            snapshot.backend_stats[1].average_article_size(),
            Some(75000)
        );

        // Backend 2: no data
        assert_eq!(snapshot.backend_stats[2].article_count, 0);
        assert_eq!(snapshot.backend_stats[2].average_article_size(), None);
    }
}
