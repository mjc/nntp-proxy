//! Serializable dashboard state shared between local and attached TUI modes.

use crate::metrics::{
    BackendHealthStatus, BackendStats, CommandCount, DiskCacheStats, ErrorCount, MetricsSnapshot,
    UserStats,
};
use crate::tui::app::{ThroughputPoint, ViewMode};
use crate::tui::system_stats::SystemStats;
use crate::types::{
    BackendToClientBytes, BytesPerSecondRate, BytesReceived, BytesSent, ClientToBackendBytes,
    HostName, MaxConnections, Port, ServerName, TotalConnections,
};
use std::time::Duration;

/// Snapshot of the I/O buffer pool used by the summary panel.
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct BufferPoolStats {
    pub available: usize,
    pub in_use: usize,
    pub total: usize,
}

/// A backend entry rendered in the backend list and chart panels.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BackendDisplay {
    pub host: HostName,
    pub port: Port,
    pub name: ServerName,
    pub max_connections: MaxConnections,
}

/// A backend entry rendered in the backend list and chart panels.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BackendView {
    pub server: BackendDisplay,
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

/// Serialized user stats used by the dashboard payload.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DashboardUserStats {
    pub username: String,
    pub active_connections: usize,
    pub total_connections: TotalConnections,
    pub bytes_sent: BytesSent,
    pub bytes_received: BytesReceived,
    pub bytes_sent_per_sec: BytesPerSecondRate,
    pub bytes_received_per_sec: BytesPerSecondRate,
    pub total_commands: CommandCount,
    pub errors: ErrorCount,
}

impl DashboardUserStats {
    #[must_use]
    pub fn from_user_stats(user: &UserStats) -> Self {
        Self {
            username: user.username.clone(),
            active_connections: user.active_connections,
            total_connections: user.total_connections,
            bytes_sent: user.bytes_sent,
            bytes_received: user.bytes_received,
            bytes_sent_per_sec: user.bytes_sent_per_sec,
            bytes_received_per_sec: user.bytes_received_per_sec,
            total_commands: user.total_commands,
            errors: user.errors,
        }
    }

    #[must_use]
    pub const fn total_bytes(&self) -> u64 {
        self.bytes_sent
            .as_u64()
            .saturating_add(self.bytes_received.as_u64())
    }
}

/// Serialized metrics used by the dashboard payload.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DashboardMetrics {
    pub total_connections: u64,
    pub active_connections: usize,
    pub stateful_sessions: usize,
    pub client_to_backend_bytes: ClientToBackendBytes,
    pub backend_to_client_bytes: BackendToClientBytes,
    pub uptime: Duration,
    pub cache_entries: u64,
    pub cache_size_bytes: u64,
    pub cache_hit_rate: f64,
    pub disk_cache: Option<DiskCacheStats>,
    pub pipeline_batches: u64,
    pub pipeline_commands: u64,
    pub pipeline_requests_queued: u64,
    pub pipeline_requests_completed: u64,
}

impl DashboardMetrics {
    #[must_use]
    pub fn from_snapshot(snapshot: &MetricsSnapshot) -> Self {
        Self {
            total_connections: snapshot.total_connections,
            active_connections: snapshot.active_connections,
            stateful_sessions: snapshot.stateful_sessions,
            client_to_backend_bytes: snapshot.client_to_backend_bytes,
            backend_to_client_bytes: snapshot.backend_to_client_bytes,
            uptime: snapshot.uptime,
            cache_entries: snapshot.cache_entries,
            cache_size_bytes: snapshot.cache_size_bytes,
            cache_hit_rate: snapshot.cache_hit_rate,
            disk_cache: snapshot.disk_cache,
            pipeline_batches: snapshot.pipeline_batches,
            pipeline_commands: snapshot.pipeline_commands,
            pipeline_requests_queued: snapshot.pipeline_requests_queued,
            pipeline_requests_completed: snapshot.pipeline_requests_completed,
        }
    }

    #[must_use]
    pub fn format_uptime(&self) -> String {
        let secs = self.uptime.as_secs();
        let hours = secs / 3600;
        let minutes = (secs % 3600) / 60;
        let seconds = secs % 60;

        if hours > 0 {
            format!("{hours}h {minutes}m {seconds}s")
        } else if minutes > 0 {
            format!("{minutes}m {seconds}s")
        } else {
            format!("{seconds}s")
        }
    }

    #[must_use]
    pub const fn total_bytes(&self) -> u64 {
        self.client_to_backend_bytes.as_u64() + self.backend_to_client_bytes.as_u64()
    }
}

/// Full dashboard state, suitable for rendering locally or over websocket.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DashboardState {
    pub metrics: DashboardMetrics,
    pub backend_views: Vec<BackendView>,
    pub top_users: Vec<DashboardUserStats>,
    pub client_history: Vec<ThroughputPoint>,
    pub system_stats: SystemStats,
    pub view_mode: ViewMode,
    pub show_details: bool,
    pub log_lines: Vec<String>,
    pub buffer_pool: Option<BufferPoolStats>,
}

impl DashboardState {
    #[must_use]
    fn backend_view(&self, backend_idx: usize) -> Option<&BackendView> {
        self.backend_views.get(backend_idx)
    }

    #[must_use]
    pub fn latest_client_throughput(&self) -> Option<&ThroughputPoint> {
        self.client_history.last()
    }

    #[must_use]
    pub fn latest_backend_throughput(&self, backend_idx: usize) -> Option<&ThroughputPoint> {
        self.backend_view(backend_idx)
            .and_then(BackendView::latest_throughput)
    }

    #[must_use]
    pub fn throughput_history(&self, backend_idx: usize) -> Option<&[ThroughputPoint]> {
        self.backend_view(backend_idx)
            .map(|view| view.history.as_slice())
    }

    #[must_use]
    pub fn backend_pending_count(&self, backend_idx: usize) -> usize {
        self.backend_view(backend_idx)
            .map_or(0, |view| view.pending_count)
    }

    #[must_use]
    pub fn backend_load_ratio(&self, backend_idx: usize) -> Option<f64> {
        self.backend_view(backend_idx)
            .and_then(|view| view.load_ratio)
    }

    #[must_use]
    pub fn backend_stateful_count(&self, backend_idx: usize) -> usize {
        self.backend_view(backend_idx)
            .map_or(0, |view| view.stateful_count)
    }

    #[must_use]
    pub fn backend_traffic_share(&self, backend_idx: usize) -> Option<f64> {
        self.backend_view(backend_idx)
            .and_then(|view| view.traffic_share)
    }

    #[must_use]
    pub fn buffer_pool(&self) -> Option<&BufferPoolStats> {
        self.buffer_pool.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{BackendHealthStatus, BackendStats};
    use crate::tui::app::{ThroughputPoint, ViewMode};
    use crate::types::Port;
    use crate::types::tui::{Throughput, Timestamp};

    fn sample_backend_view() -> BackendView {
        BackendView {
            server: BackendDisplay {
                host: HostName::try_new("backend.example.com".to_string()).unwrap(),
                port: Port::try_new(119).unwrap(),
                name: ServerName::try_new("Backend".to_string()).unwrap(),
                max_connections: MaxConnections::try_new(10).unwrap(),
            },
            stats: BackendStats::default(),
            active_connections: 1,
            health_status: BackendHealthStatus::Healthy,
            pending_count: 2,
            load_ratio: Some(0.5),
            stateful_count: 3,
            traffic_share: Some(42.0),
            history: vec![ThroughputPoint::new_backend(
                Timestamp::now(),
                Throughput::new(1.0),
                Throughput::new(2.0),
                crate::types::tui::CommandsPerSecond::new(3.0),
            )],
        }
    }

    #[test]
    fn backend_accessors_handle_out_of_range_indices() {
        let state = DashboardState {
            metrics: DashboardMetrics::default(),
            backend_views: vec![sample_backend_view()],
            top_users: Vec::new(),
            client_history: Vec::new(),
            system_stats: SystemStats::default(),
            view_mode: ViewMode::Normal,
            show_details: false,
            log_lines: Vec::new(),
            buffer_pool: None,
        };

        assert!(state.latest_backend_throughput(1).is_none());
        assert!(state.throughput_history(1).is_none());
        assert_eq!(state.backend_pending_count(1), 0);
        assert!(state.backend_load_ratio(1).is_none());
        assert_eq!(state.backend_stateful_count(1), 0);
        assert!(state.backend_traffic_share(1).is_none());
    }
}
