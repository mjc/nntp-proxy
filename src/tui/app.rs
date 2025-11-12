//! TUI application state and logic

use crate::config::ServerConfig;
use crate::metrics::{MetricsCollector, MetricsSnapshot};
use crate::router::BackendSelector;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

/// Number of historical data points to keep (60 points = 1 minute at 1 update/sec)
const HISTORY_SIZE: usize = 60;

/// Historical throughput data point
#[derive(Debug, Clone)]
pub struct ThroughputPoint {
    /// Timestamp of this measurement
    pub timestamp: Instant,
    /// Proxy ↔ Backend traffic
    pub backend_sent_per_sec: f64,     // Bytes sent TO backends (commands)
    pub backend_received_per_sec: f64, // Bytes received FROM backends (article data)
    pub commands_per_sec: f64,         // Commands processed per second
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
    throughput_history: Vec<VecDeque<ThroughputPoint>>,
    /// Historical client throughput (global, not per-backend)
    client_throughput_history: VecDeque<ClientThroughputPoint>,
    /// Previous snapshot for calculating deltas
    previous_snapshot: Option<MetricsSnapshot>,
    /// Last update time
    last_update: Instant,
}

/// Client throughput point (global, not per-backend)
#[derive(Debug, Clone)]
pub struct ClientThroughputPoint {
    pub timestamp: Instant,
    pub sent_per_sec: f64,     // TO clients (article data)
    pub received_per_sec: f64, // FROM clients (commands)
}

impl TuiApp {
    /// Create a new TUI application
    pub fn new(
        metrics: MetricsCollector,
        router: Arc<BackendSelector>,
        servers: Arc<Vec<ServerConfig>>,
    ) -> Self {
        let snapshot = metrics.snapshot();
        let backend_count = servers.len();

        // Initialize empty history for each backend
        let throughput_history = vec![VecDeque::with_capacity(HISTORY_SIZE); backend_count];

        Self {
            metrics,
            router,
            servers,
            snapshot,
            throughput_history,
            client_throughput_history: VecDeque::with_capacity(HISTORY_SIZE),
            previous_snapshot: None,
            last_update: Instant::now(),
        }
    }

    /// Update metrics snapshot and calculate throughput
    pub fn update(&mut self) {
        let new_snapshot = self.metrics.snapshot();
        let now = Instant::now();
        let time_delta = now.duration_since(self.last_update).as_secs_f64();

        if let Some(prev) = &self.previous_snapshot {
            // Calculate traffic deltas (global totals)
            let client_to_backend_delta = new_snapshot
                .client_to_backend_bytes
                .saturating_sub(prev.client_to_backend_bytes);
            let backend_to_client_delta = new_snapshot
                .backend_to_client_bytes
                .saturating_sub(prev.backend_to_client_bytes);

            let client_to_backend_per_sec = if time_delta > 0.0 {
                client_to_backend_delta as f64 / time_delta
            } else {
                0.0
            };

            let backend_to_client_per_sec = if time_delta > 0.0 {
                backend_to_client_delta as f64 / time_delta
            } else {
                0.0
            };

            // Store throughput (global, once)
            let client_point = ClientThroughputPoint {
                timestamp: now,
                sent_per_sec: client_to_backend_per_sec,
                received_per_sec: backend_to_client_per_sec,
            };

            if self.client_throughput_history.len() >= HISTORY_SIZE {
                self.client_throughput_history.pop_front();
            }
            self.client_throughput_history.push_back(client_point);

            // Calculate per-backend throughput (backend traffic only)
            for (i, (new_stats, prev_stats)) in new_snapshot
                .backend_stats
                .iter()
                .zip(prev.backend_stats.iter())
                .enumerate()
            {
                let backend_sent_delta = new_stats.bytes_sent.saturating_sub(prev_stats.bytes_sent);
                let backend_recv_delta = new_stats
                    .bytes_received
                    .saturating_sub(prev_stats.bytes_received);
                let commands_delta = new_stats
                    .total_commands
                    .saturating_sub(prev_stats.total_commands);

                let backend_sent_per_sec = if time_delta > 0.0 {
                    backend_sent_delta as f64 / time_delta
                } else {
                    0.0
                };

                let backend_recv_per_sec = if time_delta > 0.0 {
                    backend_recv_delta as f64 / time_delta
                } else {
                    0.0
                };

                let commands_per_sec = if time_delta > 0.0 {
                    commands_delta as f64 / time_delta
                } else {
                    0.0
                };

                let point = ThroughputPoint {
                    timestamp: now,
                    backend_sent_per_sec,
                    backend_received_per_sec: backend_recv_per_sec,
                    commands_per_sec,
                };

                // Add to history, removing oldest if at capacity
                if self.throughput_history[i].len() >= HISTORY_SIZE {
                    self.throughput_history[i].pop_front();
                }
                self.throughput_history[i].push_back(point);
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
    pub fn client_throughput_history(&self) -> &VecDeque<ClientThroughputPoint> {
        &self.client_throughput_history
    }

    /// Get pending command count for a backend
    #[must_use]
    pub fn backend_pending_count(&self, backend_id: usize) -> usize {
        use crate::types::BackendId;
        self.router
            .backend_load(BackendId::from_index(backend_id))
            .unwrap_or(0)
    }

    /// Get throughput history for a backend
    #[must_use]
    pub fn throughput_history(&self, backend_id: usize) -> &VecDeque<ThroughputPoint> {
        &self.throughput_history[backend_id]
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
