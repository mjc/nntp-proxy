//! Metrics snapshot type and methods
//!
//! Contains the immutable MetricsSnapshot struct with functional methods
//! for querying and aggregating metrics across backends.

use super::types::*;
use crate::types::{BackendId, BackendToClientBytes, ClientToBackendBytes};
use std::sync::Arc;
use std::time::Duration;

use super::BackendStats;
use super::UserStats;

/// Snapshot of current metrics (for display/reporting)
///
/// This is an immutable snapshot designed for functional composition.
/// Created by `MetricsCollector::snapshot()` and enriched with fluent methods.
///
/// # Rates
/// This snapshot contains cumulative counters only. The TUI calculates rates
/// by taking deltas between snapshots over time.
///
/// # Arc Sharing
/// `backend_stats` is Arc-wrapped to avoid cloning the entire Vec when
/// calculating user rates every TUI frame (4 Hz). This reduces allocations
/// from O(backends) to O(1) per update.
#[derive(Debug, Clone, Default)]
pub struct MetricsSnapshot {
    pub total_connections: u64,
    pub active_connections: usize,
    pub stateful_sessions: usize,
    pub client_to_backend_bytes: ClientToBackendBytes,
    pub backend_to_client_bytes: BackendToClientBytes,
    pub uptime: Duration,
    pub backend_stats: Arc<Vec<BackendStats>>,
    pub user_stats: Vec<UserStats>,
    pub cache_entries: u64,
    pub cache_size_bytes: u64,
    pub cache_hit_rate: f64,
}

impl MetricsSnapshot {
    /// Update backend active connections from pool status
    ///
    /// Populates `active_connections` for each backend by querying the connection pool.
    /// Active connections = max_size - available connections.
    ///
    /// This is a fluent method for functional composition: `snapshot.with_pool_status(router)`
    #[must_use]
    pub fn with_pool_status(mut self, router: &crate::router::BackendSelector) -> Self {
        use crate::pool::ConnectionProvider;

        // Get mutable access - clones only if Arc has other refs
        let backend_stats = Arc::make_mut(&mut self.backend_stats);

        for stats in backend_stats {
            if let Some(provider) = router.backend_provider(stats.backend_id) {
                let pool_status = provider.status();
                // Active = checked out connections = max - available
                let active = pool_status
                    .max_size
                    .get()
                    .saturating_sub(pool_status.available.get());
                stats.active_connections = ActiveConnections::new(active);
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
    ///
    /// This is a pure calculation method - no side effects.
    #[must_use]
    #[inline]
    pub fn total_bytes(&self) -> u64 {
        self.client_to_backend_bytes.as_u64() + self.backend_to_client_bytes.as_u64()
    }

    /// Calculate throughput in bytes per second
    ///
    /// Returns 0.0 if uptime is zero (avoid division by zero).
    /// This is a pure calculation method - no side effects.
    #[must_use]
    pub fn throughput_bps(&self) -> f64 {
        let secs = self.uptime.as_secs_f64();
        if secs > 0.0 {
            self.total_bytes() as f64 / secs
        } else {
            0.0
        }
    }

    /// Get total number of commands processed across all backends
    ///
    /// This is a pure calculation using iterator composition.
    #[must_use]
    #[inline]
    pub fn total_commands(&self) -> u64 {
        self.backend_stats
            .iter()
            .map(|stats| stats.total_commands.get())
            .sum()
    }

    /// Get total number of errors across all backends
    ///
    /// This is a pure calculation using iterator composition.
    #[must_use]
    #[inline]
    pub fn total_errors(&self) -> u64 {
        self.backend_stats
            .iter()
            .map(|stats| stats.errors.get())
            .sum()
    }

    /// Get overall error rate percentage
    ///
    /// Returns error rate across all backends combined.
    /// This is a pure calculation method - no side effects.
    #[must_use]
    pub fn error_rate_percent(&self) -> f64 {
        let total_cmds = self.total_commands();
        let total_errs = self.total_errors();
        ErrorRatePercent::from_raw_counts(total_errs, total_cmds).get()
    }

    /// Get all backends with high error rates
    ///
    /// Returns an iterator of backend IDs with error rates > 5%.
    /// This is a pure calculation using iterator composition.
    pub fn high_error_backends(&self) -> impl Iterator<Item = BackendId> + '_ {
        self.backend_stats
            .iter()
            .filter(|stats| stats.has_high_error_rate())
            .map(|stats| stats.backend_id)
    }

    /// Get all healthy backends
    ///
    /// Returns an iterator of backend IDs with Healthy status.
    /// This is a pure calculation using iterator composition.
    pub fn healthy_backends(&self) -> impl Iterator<Item = BackendId> + '_ {
        self.backend_stats
            .iter()
            .filter(|stats| stats.health_status == BackendHealthStatus::Healthy)
            .map(|stats| stats.backend_id)
    }

    /// Get backend statistics by ID
    ///
    /// Returns None if backend_id is out of range.
    #[must_use]
    pub fn backend(&self, backend_id: BackendId) -> Option<&BackendStats> {
        self.backend_stats.get(backend_id.as_index())
    }

    /// Count backends by health status
    ///
    /// Returns (healthy, degraded, down) counts.
    /// This is a pure calculation using iterator composition.
    #[must_use]
    pub fn backend_health_counts(&self) -> (usize, usize, usize) {
        self.backend_stats
            .iter()
            .fold((0, 0, 0), |(h, d, dn), stats| match stats.health_status {
                BackendHealthStatus::Healthy => (h + 1, d, dn),
                BackendHealthStatus::Degraded => (h, d + 1, dn),
                BackendHealthStatus::Down => (h, d, dn + 1),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::BackendId;

    fn create_test_snapshot() -> MetricsSnapshot {
        use crate::types::metrics::*;
        use crate::types::{ArticleBytesTotal, BytesReceived, BytesSent, TimingMeasurementCount};

        let backend1 = BackendStats {
            backend_id: BackendId::from_index(0),
            total_commands: CommandCount::new(100),
            errors: ErrorCount::new(5),
            bytes_sent: BytesSent::new(1000),
            bytes_received: BytesReceived::new(2000),
            health_status: BackendHealthStatus::Healthy,
            active_connections: ActiveConnections::new(3),
            errors_4xx: ErrorCount::new(2),
            errors_5xx: ErrorCount::new(3),
            article_bytes_total: ArticleBytesTotal::new(5000),
            article_count: ArticleCount::new(10),
            ttfb_micros_total: TtfbMicros::new(1000),
            ttfb_count: TimingMeasurementCount::new(10),
            send_micros_total: SendMicros::new(500),
            recv_micros_total: RecvMicros::new(1500),
            connection_failures: FailureCount::new(0),
        };

        let backend2 = BackendStats {
            backend_id: BackendId::from_index(1),
            total_commands: CommandCount::new(50),
            errors: ErrorCount::new(10),
            bytes_sent: BytesSent::new(500),
            bytes_received: BytesReceived::new(1500),
            health_status: BackendHealthStatus::Degraded,
            active_connections: ActiveConnections::new(2),
            errors_4xx: ErrorCount::new(5),
            errors_5xx: ErrorCount::new(5),
            article_bytes_total: ArticleBytesTotal::new(2500),
            article_count: ArticleCount::new(5),
            ttfb_micros_total: TtfbMicros::new(500),
            ttfb_count: TimingMeasurementCount::new(5),
            send_micros_total: SendMicros::new(250),
            recv_micros_total: RecvMicros::new(750),
            connection_failures: FailureCount::new(1),
        };

        MetricsSnapshot {
            total_connections: 5,
            active_connections: 5,
            stateful_sessions: 2,
            client_to_backend_bytes: ClientToBackendBytes::new(1500),
            backend_to_client_bytes: BackendToClientBytes::new(3500),
            uptime: Duration::from_secs(3600),
            backend_stats: Arc::new(vec![backend1, backend2]),
            user_stats: vec![],
            cache_entries: 0,
            cache_size_bytes: 0,
            cache_hit_rate: 0.0,
        }
    }

    #[test]
    fn test_format_uptime_hours() {
        let snapshot = MetricsSnapshot {
            uptime: Duration::from_secs(3661), // 1h 1m 1s
            ..Default::default()
        };
        assert_eq!(snapshot.format_uptime(), "1h 1m 1s");
    }

    #[test]
    fn test_format_uptime_minutes() {
        let snapshot = MetricsSnapshot {
            uptime: Duration::from_secs(125), // 2m 5s
            ..Default::default()
        };
        assert_eq!(snapshot.format_uptime(), "2m 5s");
    }

    #[test]
    fn test_format_uptime_seconds() {
        let snapshot = MetricsSnapshot {
            uptime: Duration::from_secs(42),
            ..Default::default()
        };
        assert_eq!(snapshot.format_uptime(), "42s");
    }

    #[test]
    fn test_format_uptime_zero() {
        let snapshot = MetricsSnapshot {
            uptime: Duration::from_secs(0),
            ..Default::default()
        };
        assert_eq!(snapshot.format_uptime(), "0s");
    }

    #[test]
    fn test_total_bytes() {
        let snapshot = create_test_snapshot();
        assert_eq!(snapshot.total_bytes(), 5000); // 1500 + 3500
    }

    #[test]
    fn test_total_bytes_zero() {
        let snapshot = MetricsSnapshot::default();
        assert_eq!(snapshot.total_bytes(), 0);
    }

    #[test]
    fn test_throughput_bps() {
        let snapshot = create_test_snapshot();
        let expected = 5000.0 / 3600.0; // total_bytes / uptime_secs
        assert!((snapshot.throughput_bps() - expected).abs() < 0.01);
    }

    #[test]
    fn test_throughput_bps_zero_uptime() {
        let snapshot = MetricsSnapshot {
            client_to_backend_bytes: ClientToBackendBytes::new(1000),
            backend_to_client_bytes: BackendToClientBytes::new(2000),
            uptime: Duration::from_secs(0),
            ..Default::default()
        };
        assert_eq!(snapshot.throughput_bps(), 0.0);
    }

    #[test]
    fn test_total_commands() {
        let snapshot = create_test_snapshot();
        assert_eq!(snapshot.total_commands(), 150); // 100 + 50
    }

    #[test]
    fn test_total_commands_empty() {
        let snapshot = MetricsSnapshot::default();
        assert_eq!(snapshot.total_commands(), 0);
    }

    #[test]
    fn test_total_errors() {
        let snapshot = create_test_snapshot();
        assert_eq!(snapshot.total_errors(), 15); // 5 + 10
    }

    #[test]
    fn test_total_errors_empty() {
        let snapshot = MetricsSnapshot::default();
        assert_eq!(snapshot.total_errors(), 0);
    }

    #[test]
    fn test_error_rate_percent() {
        let snapshot = create_test_snapshot();
        let expected = 15.0 / 150.0 * 100.0; // (total_errors / total_commands) * 100
        assert!((snapshot.error_rate_percent() - expected).abs() < 0.01);
    }

    #[test]
    fn test_error_rate_percent_zero_commands() {
        let snapshot = MetricsSnapshot::default();
        assert_eq!(snapshot.error_rate_percent(), 0.0);
    }

    #[test]
    fn test_high_error_backends() {
        let snapshot = create_test_snapshot();
        let high_error: Vec<_> = snapshot.high_error_backends().collect();
        // Backend2 has 10/50 = 20% error rate (> 5%)
        // Backend1 has 5/100 = 5% error rate (not > 5%)
        assert_eq!(high_error.len(), 1);
        assert_eq!(high_error[0], BackendId::from_index(1));
    }

    #[test]
    fn test_high_error_backends_empty() {
        let snapshot = MetricsSnapshot::default();
        let high_error: Vec<_> = snapshot.high_error_backends().collect();
        assert_eq!(high_error.len(), 0);
    }

    #[test]
    fn test_healthy_backends() {
        let snapshot = create_test_snapshot();
        let healthy: Vec<_> = snapshot.healthy_backends().collect();
        assert_eq!(healthy.len(), 1);
        assert_eq!(healthy[0], BackendId::from_index(0));
    }

    #[test]
    fn test_healthy_backends_all_down() {
        let backend = BackendStats {
            health_status: BackendHealthStatus::Down,
            ..Default::default()
        };

        let snapshot = MetricsSnapshot {
            backend_stats: Arc::new(vec![backend]),
            ..Default::default()
        };

        let healthy: Vec<_> = snapshot.healthy_backends().collect();
        assert_eq!(healthy.len(), 0);
    }

    #[test]
    fn test_backend_by_id() {
        let snapshot = create_test_snapshot();

        let backend0 = snapshot.backend(BackendId::from_index(0));
        assert!(backend0.is_some());
        assert_eq!(backend0.unwrap().backend_id, BackendId::from_index(0));
        assert_eq!(backend0.unwrap().total_commands.get(), 100);

        let backend1 = snapshot.backend(BackendId::from_index(1));
        assert!(backend1.is_some());
        assert_eq!(backend1.unwrap().backend_id, BackendId::from_index(1));
        assert_eq!(backend1.unwrap().total_commands.get(), 50);
    }

    #[test]
    fn test_backend_by_id_out_of_range() {
        let snapshot = create_test_snapshot();
        let backend = snapshot.backend(BackendId::from_index(99));
        assert!(backend.is_none());
    }

    #[test]
    fn test_backend_health_counts() {
        let snapshot = create_test_snapshot();
        let (healthy, degraded, down) = snapshot.backend_health_counts();
        assert_eq!(healthy, 1);
        assert_eq!(degraded, 1);
        assert_eq!(down, 0);
    }

    #[test]
    fn test_backend_health_counts_mixed() {
        let backends = vec![
            BackendStats {
                backend_id: BackendId::from_index(0),
                health_status: BackendHealthStatus::Healthy,
                ..Default::default()
            },
            BackendStats {
                backend_id: BackendId::from_index(1),
                health_status: BackendHealthStatus::Healthy,
                ..Default::default()
            },
            BackendStats {
                backend_id: BackendId::from_index(2),
                health_status: BackendHealthStatus::Down,
                ..Default::default()
            },
        ];

        let snapshot = MetricsSnapshot {
            backend_stats: Arc::new(backends),
            ..Default::default()
        };

        let (healthy, degraded, down) = snapshot.backend_health_counts();
        assert_eq!(healthy, 2);
        assert_eq!(degraded, 0);
        assert_eq!(down, 1);
    }

    #[test]
    fn test_backend_health_counts_empty() {
        let snapshot = MetricsSnapshot::default();
        let (healthy, degraded, down) = snapshot.backend_health_counts();
        assert_eq!(healthy, 0);
        assert_eq!(degraded, 0);
        assert_eq!(down, 0);
    }

    #[test]
    fn test_snapshot_default() {
        let snapshot = MetricsSnapshot::default();
        assert_eq!(snapshot.total_connections, 0);
        assert_eq!(snapshot.active_connections, 0);
        assert_eq!(snapshot.stateful_sessions, 0);
        assert_eq!(snapshot.total_bytes(), 0);
        assert_eq!(snapshot.uptime, Duration::from_secs(0));
        assert_eq!(snapshot.backend_stats.len(), 0);
        assert_eq!(snapshot.user_stats.len(), 0);
    }

    #[test]
    fn test_snapshot_clone() {
        let snapshot = create_test_snapshot();
        let cloned = snapshot.clone();

        assert_eq!(snapshot.total_connections, cloned.total_connections);
        assert_eq!(snapshot.total_bytes(), cloned.total_bytes());
        assert_eq!(snapshot.uptime, cloned.uptime);

        // Arc should share the same allocation
        assert!(Arc::ptr_eq(&snapshot.backend_stats, &cloned.backend_stats));
    }
}
