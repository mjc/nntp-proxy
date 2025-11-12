//! Real-time metrics collection for the NNTP proxy
//!
//! This module provides lock-free, thread-safe metrics tracking using atomic operations.
//! Metrics are designed to be updated frequently from hot paths with minimal overhead.

use crate::types::BackendBytes;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

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

    // Per-backend metrics (indexed by backend ID)
    backend_metrics: Vec<BackendMetrics>,

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
}

/// Snapshot of current metrics (for display/reporting)
#[derive(Debug, Clone)]
pub struct MetricsSnapshot {
    pub total_connections: u64,
    pub active_connections: usize,
    // Traffic flows
    pub client_to_backend_bytes: BackendBytes, // Commands from client to backend
    pub backend_to_client_bytes: BackendBytes, // Article data from backend to client
    pub uptime: Duration,
    pub backend_stats: Vec<BackendStats>,
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
            })
            .collect();

        Self {
            inner: Arc::new(MetricsInner {
                total_connections: AtomicU64::new(0),
                active_connections: AtomicUsize::new(0),
                backend_metrics,
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
            });
        }

        // Calculate global totals by summing backend metrics
        let c2b_bytes: u64 = backend_stats.iter().map(|b| b.bytes_sent).sum();
        let b2c_bytes: u64 = backend_stats.iter().map(|b| b.bytes_received).sum();

        MetricsSnapshot {
            total_connections: self.inner.total_connections.load(Ordering::Relaxed),
            active_connections: self.inner.active_connections.load(Ordering::Relaxed),
            client_to_backend_bytes: BackendBytes::new(c2b_bytes),
            backend_to_client_bytes: BackendBytes::new(b2c_bytes),
            uptime: self.inner.start_time.elapsed(),
            backend_stats,
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
            client_to_backend_bytes: BackendBytes::new(0),
            backend_to_client_bytes: BackendBytes::new(0),
            uptime: Duration::from_secs(3665), // 1h 1m 5s
            backend_stats: vec![],
        };

        assert_eq!(snapshot.format_uptime(), "1h 1m 5s");
    }

    #[test]
    fn test_throughput_calculation() {
        let snapshot = MetricsSnapshot {
            total_connections: 0,
            active_connections: 0,
            client_to_backend_bytes: BackendBytes::new(5000),
            backend_to_client_bytes: BackendBytes::new(5000),
            uptime: Duration::from_secs(10),
            backend_stats: vec![],
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
}
