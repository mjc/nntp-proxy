//! Metrics snapshot type and methods
//!
//! Contains the immutable MetricsSnapshot struct with functional methods
//! for querying and aggregating metrics across backends.

use super::types::*;
use crate::types::BackendBytes;
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
#[derive(Debug, Clone, Default)]
pub struct MetricsSnapshot {
    pub total_connections: u64,
    pub active_connections: usize,
    pub stateful_sessions: usize,
    pub client_to_backend_bytes: BackendBytes,
    pub backend_to_client_bytes: BackendBytes,
    pub uptime: Duration,
    pub backend_stats: Vec<BackendStats>,
    pub user_stats: Vec<UserStats>,
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

        for stats in &mut self.backend_stats {
            let backend_id = crate::types::BackendId::from_index(stats.backend_id);
            if let Some(provider) = router.get_backend_provider(backend_id) {
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
    pub fn high_error_backends(&self) -> impl Iterator<Item = usize> + '_ {
        self.backend_stats
            .iter()
            .filter(|stats| stats.has_high_error_rate())
            .map(|stats| stats.backend_id)
    }

    /// Get all healthy backends
    ///
    /// Returns an iterator of backend IDs with Healthy status.
    /// This is a pure calculation using iterator composition.
    pub fn healthy_backends(&self) -> impl Iterator<Item = usize> + '_ {
        self.backend_stats
            .iter()
            .filter(|stats| stats.health_status == HealthStatus::Healthy)
            .map(|stats| stats.backend_id)
    }

    /// Get backend statistics by ID
    ///
    /// Returns None if backend_id is out of range.
    #[must_use]
    pub fn backend(&self, backend_id: usize) -> Option<&BackendStats> {
        self.backend_stats.get(backend_id)
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
                HealthStatus::Healthy => (h + 1, d, dn),
                HealthStatus::Degraded => (h, d + 1, dn),
                HealthStatus::Down => (h, d, dn + 1),
            })
    }
}
