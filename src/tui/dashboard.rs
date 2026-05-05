//! Serializable dashboard state shared between local and attached TUI modes.

use crate::config::Server;
use crate::metrics::{BackendHealthStatus, BackendStats, MetricsSnapshot};
use crate::tui::app::{ThroughputPoint, ViewMode};
use crate::tui::system_stats::SystemStats;

/// Snapshot of the I/O buffer pool used by the summary panel.
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct BufferPoolStats {
    pub available: usize,
    pub in_use: usize,
    pub total: usize,
}

/// A backend entry rendered in the backend list and chart panels.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BackendView {
    pub server: Server,
    pub stats: BackendStats,
    pub active_connections: usize,
    pub health_status: BackendHealthStatus,
    pub pending_count: usize,
    pub load_ratio: Option<f64>,
    pub stateful_count: usize,
    pub traffic_share: Option<f64>,
    pub history: Vec<ThroughputPoint>,
}

impl BackendView {
    #[must_use]
    pub fn latest_throughput(&self) -> Option<&ThroughputPoint> {
        self.history.last()
    }
}

/// Full dashboard state, suitable for rendering locally or over websocket.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DashboardState {
    pub snapshot: MetricsSnapshot,
    pub backend_views: Vec<BackendView>,
    pub client_history: Vec<ThroughputPoint>,
    pub system_stats: SystemStats,
    pub view_mode: ViewMode,
    pub show_details: bool,
    pub log_lines: Vec<String>,
    pub buffer_pool: Option<BufferPoolStats>,
}

impl DashboardState {
    #[must_use]
    pub fn latest_client_throughput(&self) -> Option<&ThroughputPoint> {
        self.client_history.last()
    }

    #[must_use]
    pub fn latest_backend_throughput(&self, backend_idx: usize) -> Option<&ThroughputPoint> {
        self.backend_views
            .get(backend_idx)
            .and_then(BackendView::latest_throughput)
    }

    #[must_use]
    pub fn throughput_history(&self, backend_idx: usize) -> Option<&[ThroughputPoint]> {
        self.backend_views
            .get(backend_idx)
            .map(|view| view.history.as_slice())
    }

    #[must_use]
    pub fn backend_pending_count(&self, backend_idx: usize) -> usize {
        self.backend_views
            .get(backend_idx)
            .map_or(0, |view| view.pending_count)
    }

    #[must_use]
    pub fn backend_load_ratio(&self, backend_idx: usize) -> Option<f64> {
        self.backend_views
            .get(backend_idx)
            .and_then(|view| view.load_ratio)
    }

    #[must_use]
    pub fn backend_stateful_count(&self, backend_idx: usize) -> usize {
        self.backend_views
            .get(backend_idx)
            .map_or(0, |view| view.stateful_count)
    }

    #[must_use]
    pub fn backend_traffic_share(&self, backend_idx: usize) -> Option<f64> {
        self.backend_views
            .get(backend_idx)
            .and_then(|view| view.traffic_share)
    }

    #[must_use]
    pub fn buffer_pool(&self) -> Option<&BufferPoolStats> {
        self.buffer_pool.as_ref()
    }
}
