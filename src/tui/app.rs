//! TUI application state and logic

use crate::config::ServerConfig;
use crate::metrics::{MetricsCollector, MetricsSnapshot};
use std::sync::Arc;

/// TUI application state
pub struct TuiApp {
    /// Metrics collector (shared with proxy)
    metrics: MetricsCollector,
    /// Server configurations for display names
    servers: Arc<Vec<ServerConfig>>,
    /// Current metrics snapshot
    snapshot: MetricsSnapshot,
}

impl TuiApp {
    /// Create a new TUI application
    pub fn new(metrics: MetricsCollector, servers: Arc<Vec<ServerConfig>>) -> Self {
        let snapshot = metrics.snapshot();
        Self {
            metrics,
            servers,
            snapshot,
        }
    }

    /// Update metrics snapshot
    pub fn update(&mut self) {
        self.snapshot = self.metrics.snapshot();
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
}
