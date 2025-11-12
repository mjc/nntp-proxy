//! Real-time metrics collection for the NNTP proxy
//!
//! This module provides lock-free, thread-safe metrics tracking using atomic operations.
//! Metrics are designed to be updated frequently from hot paths with minimal overhead.

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

    // Data transfer metrics
    total_bytes_sent: AtomicU64,
    total_bytes_received: AtomicU64,

    // Command metrics
    total_commands: AtomicU64,

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
    pub total_bytes_sent: u64,
    pub total_bytes_received: u64,
    pub total_commands: u64,
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
                total_bytes_sent: AtomicU64::new(0),
                total_bytes_received: AtomicU64::new(0),
                total_commands: AtomicU64::new(0),
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

    /// Record bytes transferred to a backend
    #[inline]
    pub fn record_bytes_sent(&self, backend_id: usize, bytes: u64) {
        self.inner
            .total_bytes_sent
            .fetch_add(bytes, Ordering::Relaxed);
        if let Some(backend) = self.inner.backend_metrics.get(backend_id) {
            backend.bytes_sent.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    /// Record bytes received from a backend
    #[inline]
    pub fn record_bytes_received(&self, backend_id: usize, bytes: u64) {
        self.inner
            .total_bytes_received
            .fetch_add(bytes, Ordering::Relaxed);
        if let Some(backend) = self.inner.backend_metrics.get(backend_id) {
            backend.bytes_received.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    /// Record a command processed
    #[inline]
    pub fn record_command(&self, backend_id: usize) {
        self.inner.total_commands.fetch_add(1, Ordering::Relaxed);
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
    #[must_use]
    pub fn snapshot(&self) -> MetricsSnapshot {
        let backend_stats = self
            .inner
            .backend_metrics
            .iter()
            .enumerate()
            .map(|(id, metrics)| BackendStats {
                backend_id: id,
                active_connections: metrics.active_connections.load(Ordering::Relaxed),
                total_commands: metrics.total_commands.load(Ordering::Relaxed),
                bytes_sent: metrics.bytes_sent.load(Ordering::Relaxed),
                bytes_received: metrics.bytes_received.load(Ordering::Relaxed),
                errors: metrics.errors.load(Ordering::Relaxed),
            })
            .collect();

        MetricsSnapshot {
            total_connections: self.inner.total_connections.load(Ordering::Relaxed),
            active_connections: self.inner.active_connections.load(Ordering::Relaxed),
            total_bytes_sent: self.inner.total_bytes_sent.load(Ordering::Relaxed),
            total_bytes_received: self.inner.total_bytes_received.load(Ordering::Relaxed),
            total_commands: self.inner.total_commands.load(Ordering::Relaxed),
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

    /// Get total bytes transferred (sent + received)
    #[must_use]
    #[inline]
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes_sent + self.total_bytes_received
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

        metrics.record_bytes_sent(0, 1024);
        metrics.record_bytes_received(0, 2048);

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.total_bytes_sent, 1024);
        assert_eq!(snapshot.total_bytes_received, 2048);
        assert_eq!(snapshot.backend_stats[0].bytes_sent, 1024);
        assert_eq!(snapshot.backend_stats[0].bytes_received, 2048);
    }

    #[test]
    fn test_command_tracking() {
        let metrics = MetricsCollector::new(2);

        metrics.record_command(0);
        metrics.record_command(0);
        metrics.record_command(1);

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.total_commands, 3);
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
            total_bytes_sent: 0,
            total_bytes_received: 0,
            total_commands: 0,
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
            total_bytes_sent: 5000,
            total_bytes_received: 5000,
            total_commands: 0,
            uptime: Duration::from_secs(10),
            backend_stats: vec![],
        };

        assert_eq!(snapshot.throughput_bps(), 1000.0); // 10000 bytes / 10 seconds
    }
}
