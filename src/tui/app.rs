//! TUI application state and logic

use crate::config::ServerConfig;
use crate::metrics::{MetricsCollector, MetricsSnapshot};
use crate::router::BackendSelector;
use crate::types::tui::{BytesPerSecond, CommandsPerSecond, HistorySize, Timestamp};
use std::collections::VecDeque;
use std::sync::Arc;

/// Historical throughput data point (generic over traffic direction)
#[derive(Debug, Clone)]
pub struct ThroughputPoint {
    /// Timestamp of this measurement
    timestamp: Timestamp,
    /// Bytes sent per second
    sent_per_sec: BytesPerSecond,
    /// Bytes received per second
    received_per_sec: BytesPerSecond,
    /// Commands processed per second (backend only)
    commands_per_sec: Option<CommandsPerSecond>,
}

impl ThroughputPoint {
    /// Create a new backend throughput point
    #[must_use]
    pub const fn new_backend(
        timestamp: Timestamp,
        sent_per_sec: BytesPerSecond,
        received_per_sec: BytesPerSecond,
        commands_per_sec: CommandsPerSecond,
    ) -> Self {
        Self {
            timestamp,
            sent_per_sec,
            received_per_sec,
            commands_per_sec: Some(commands_per_sec),
        }
    }

    /// Create a new client throughput point
    #[must_use]
    pub const fn new_client(
        timestamp: Timestamp,
        sent_per_sec: BytesPerSecond,
        received_per_sec: BytesPerSecond,
    ) -> Self {
        Self {
            timestamp,
            sent_per_sec,
            received_per_sec,
            commands_per_sec: None,
        }
    }

    /// Get timestamp
    #[must_use]
    #[inline]
    pub const fn timestamp(&self) -> Timestamp {
        self.timestamp
    }

    /// Get sent bytes per second
    #[must_use]
    #[inline]
    pub const fn sent_per_sec(&self) -> BytesPerSecond {
        self.sent_per_sec
    }

    /// Get received bytes per second
    #[must_use]
    #[inline]
    pub const fn received_per_sec(&self) -> BytesPerSecond {
        self.received_per_sec
    }

    /// Get commands per second (if backend point)
    #[must_use]
    #[inline]
    pub const fn commands_per_sec(&self) -> Option<CommandsPerSecond> {
        self.commands_per_sec
    }
}

/// Circular buffer for throughput history
#[derive(Debug, Clone)]
struct ThroughputHistory {
    points: VecDeque<ThroughputPoint>,
    capacity: HistorySize,
}

impl ThroughputHistory {
    /// Create a new history with the given capacity
    #[must_use]
    fn new(capacity: HistorySize) -> Self {
        Self {
            points: VecDeque::with_capacity(capacity.get()),
            capacity,
        }
    }

    /// Add a point, removing oldest if at capacity
    fn push(&mut self, point: ThroughputPoint) {
        if self.points.len() >= self.capacity.get() {
            self.points.pop_front();
        }
        self.points.push_back(point);
    }

    /// Get all points
    #[must_use]
    fn points(&self) -> &VecDeque<ThroughputPoint> {
        &self.points
    }

    /// Get the latest point
    #[must_use]
    fn latest(&self) -> Option<&ThroughputPoint> {
        self.points.back()
    }
}

/// TUI application state
pub struct TuiApp {
    /// Metrics collector (shared with proxy)
    metrics: MetricsCollector,
    /// Router for getting pending command counts
    router: Arc<BackendSelector>,
    /// Server configurations for display names
    servers: Arc<Vec<ServerConfig>>,
    /// Current metrics snapshot
    snapshot: MetricsSnapshot,
    /// Historical throughput data per backend
    backend_history: Vec<ThroughputHistory>,
    /// Historical client throughput (global)
    client_history: ThroughputHistory,
    /// Previous snapshot for calculating deltas
    previous_snapshot: Option<MetricsSnapshot>,
    /// Last update time
    last_update: Timestamp,
    /// History capacity
    #[allow(dead_code)]
    history_size: HistorySize,
}

impl TuiApp {
    /// Create a new TUI application
    pub fn new(
        metrics: MetricsCollector,
        router: Arc<BackendSelector>,
        servers: Arc<Vec<ServerConfig>>,
    ) -> Self {
        Self::with_history_size(metrics, router, servers, HistorySize::DEFAULT)
    }

    /// Create with custom history size
    pub fn with_history_size(
        metrics: MetricsCollector,
        router: Arc<BackendSelector>,
        servers: Arc<Vec<ServerConfig>>,
        history_size: HistorySize,
    ) -> Self {
        let snapshot = metrics.snapshot();
        let backend_count = servers.len();

        // Initialize empty history for each backend
        let backend_history = (0..backend_count)
            .map(|_| ThroughputHistory::new(history_size))
            .collect();

        Self {
            metrics,
            router,
            servers,
            snapshot,
            backend_history,
            client_history: ThroughputHistory::new(history_size),
            previous_snapshot: None,
            last_update: Timestamp::now(),
            history_size,
        }
    }

    /// Calculate throughput rate from byte delta and time delta
    #[inline]
    fn calculate_rate(byte_delta: u64, time_delta_secs: f64) -> BytesPerSecond {
        if time_delta_secs > 0.0 {
            BytesPerSecond::new(byte_delta as f64 / time_delta_secs)
        } else {
            BytesPerSecond::zero()
        }
    }

    /// Calculate command rate from command delta and time delta
    #[inline]
    fn calculate_command_rate(cmd_delta: u64, time_delta_secs: f64) -> CommandsPerSecond {
        if time_delta_secs > 0.0 {
            CommandsPerSecond::new(cmd_delta as f64 / time_delta_secs)
        } else {
            CommandsPerSecond::zero()
        }
    }

    /// Update metrics snapshot and calculate throughput
    pub fn update(&mut self) {
        let new_snapshot = self.metrics.snapshot();
        let now = Timestamp::now();
        let time_delta = now.duration_since(self.last_update).as_secs_f64();

        if let Some(prev) = &self.previous_snapshot {
            // Calculate client traffic deltas (global totals)
            let client_to_backend_delta = new_snapshot
                .client_to_backend_bytes
                .saturating_sub(prev.client_to_backend_bytes);
            let backend_to_client_delta = new_snapshot
                .backend_to_client_bytes
                .saturating_sub(prev.backend_to_client_bytes);

            // Calculate rates
            let client_sent_rate = Self::calculate_rate(client_to_backend_delta, time_delta);
            let client_recv_rate = Self::calculate_rate(backend_to_client_delta, time_delta);

            // Store client throughput point
            let client_point = ThroughputPoint::new_client(now, client_sent_rate, client_recv_rate);
            self.client_history.push(client_point);

            // Calculate per-backend throughput
            for (i, (new_stats, prev_stats)) in new_snapshot
                .backend_stats
                .iter()
                .zip(prev.backend_stats.iter())
                .enumerate()
            {
                let sent_delta = new_stats.bytes_sent.saturating_sub(prev_stats.bytes_sent);
                let recv_delta = new_stats
                    .bytes_received
                    .saturating_sub(prev_stats.bytes_received);
                let cmd_delta = new_stats
                    .total_commands
                    .saturating_sub(prev_stats.total_commands);

                let sent_rate = Self::calculate_rate(sent_delta, time_delta);
                let recv_rate = Self::calculate_rate(recv_delta, time_delta);
                let cmd_rate = Self::calculate_command_rate(cmd_delta, time_delta);

                let point = ThroughputPoint::new_backend(now, sent_rate, recv_rate, cmd_rate);
                self.backend_history[i].push(point);
            }
        }

        // Update snapshots: new becomes previous for next iteration
        self.previous_snapshot = Some(new_snapshot.clone());
        self.snapshot = new_snapshot;
        self.last_update = now;
    }

    /// Get current metrics snapshot
    #[must_use]
    pub fn snapshot(&self) -> &MetricsSnapshot {
        &self.snapshot
    }

    /// Get server configurations
    #[must_use]
    pub fn servers(&self) -> &[ServerConfig] {
        &self.servers
    }

    /// Get client throughput history (global)
    #[must_use]
    pub fn client_throughput_history(&self) -> &VecDeque<ThroughputPoint> {
        self.client_history.points()
    }

    /// Get latest client throughput
    #[must_use]
    pub fn latest_client_throughput(&self) -> Option<&ThroughputPoint> {
        self.client_history.latest()
    }

    /// Get pending command count for a backend
    #[must_use]
    pub fn backend_pending_count(&self, backend_idx: usize) -> usize {
        use crate::types::BackendId;
        self.router
            .backend_load(BackendId::from_index(backend_idx))
            .unwrap_or(0)
    }

    /// Get throughput history for a backend
    #[must_use]
    pub fn throughput_history(&self, backend_idx: usize) -> &VecDeque<ThroughputPoint> {
        self.backend_history[backend_idx].points()
    }

    /// Get latest throughput for a backend
    #[must_use]
    pub fn latest_backend_throughput(&self, backend_idx: usize) -> Option<&ThroughputPoint> {
        self.backend_history.get(backend_idx).and_then(|h| h.latest())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerConfig;
    use crate::metrics::MetricsCollector;
    use crate::router::BackendSelector;
    use std::sync::Arc;

    /// Test for the bug where previous_snapshot was set to self.snapshot instead of new_snapshot.
    /// This caused the TUI to skip every other snapshot, calculating deltas over 2x the time period
    /// and showing 2x the actual throughput.
    ///
    /// The bug: previous_snapshot = Some(self.snapshot.clone())  // OLD snapshot
    /// The fix: previous_snapshot = Some(new_snapshot.clone())   // NEW snapshot (just used)
    #[test]
    fn test_previous_snapshot_uses_new_snapshot_not_old() {
        let metrics = MetricsCollector::new(1);
        let router = Arc::new(BackendSelector::new());
        let servers = Arc::new(vec![
            ServerConfig::builder("test.example.com", 119)
                .name("Test Server".to_string())
                .build()
                .unwrap(),
        ]);

        let mut app = TuiApp::new(metrics.clone(), router, servers);

        // Update 1: 1000 bytes total
        metrics.record_backend_to_client_bytes_for(0, 1000);
        app.update();
        assert_eq!(app.snapshot().backend_to_client_bytes.as_u64(), 1000);
        // After update 1, previous_snapshot should be snapshot from update 1 (1000)
        assert_eq!(
            app.previous_snapshot
                .as_ref()
                .unwrap()
                .backend_to_client_bytes
                .as_u64(),
            1000
        );

        // Update 2: 2000 bytes total
        metrics.record_backend_to_client_bytes_for(0, 1000);
        app.update();
        assert_eq!(app.snapshot().backend_to_client_bytes.as_u64(), 2000);
        // After update 2, previous_snapshot should be snapshot from update 2 (2000)
        // BUG would have left it at update 1 (1000)
        assert_eq!(
            app.previous_snapshot
                .as_ref()
                .unwrap()
                .backend_to_client_bytes
                .as_u64(),
            2000,
            "previous_snapshot should be updated to the new_snapshot (2000), not left as old self.snapshot (1000)"
        );

        // Update 3: 3000 bytes total
        metrics.record_backend_to_client_bytes_for(0, 1000);
        app.update();
        assert_eq!(app.snapshot().backend_to_client_bytes.as_u64(), 3000);
        // After update 3, previous_snapshot should be snapshot from update 3 (3000)
        // BUG would have it at update 2 (2000), causing next delta to be (4000-2000)=2000 instead of (4000-3000)=1000
        assert_eq!(
            app.previous_snapshot
                .as_ref()
                .unwrap()
                .backend_to_client_bytes
                .as_u64(),
            3000,
            "previous_snapshot should be 3000 (from update 3), not 2000 (from update 2) - bug would cause 2x deltas"
        );
    }
}
