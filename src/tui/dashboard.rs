//! Serializable dashboard state shared between local and attached TUI modes.

use crate::metrics::{
    ActiveConnections, BackendHealthStatus, BackendStats, CacheEntries, CommandCount,
    DiskCacheStats, ErrorCount, MetricsSnapshot, PendingRequests, PipelineBatches,
    PipelineCommands, PipelineRequestsCompleted, PipelineRequestsQueued, StatefulSessions,
    UserActiveConnections, UserStats,
};
use crate::tui::app::{ThroughputPoint, ViewMode};
use crate::tui::system_stats::SystemStats;
use crate::types::{
    BackendToClientBytes, BytesPerSecondRate, BytesReceived, BytesSent, ClientToBackendBytes,
    HostName, MaxConnections, Port, ServerName, TotalConnections,
};
use arrayvec::ArrayString;
use std::fmt;
use std::fmt::Write as _;
use std::time::Duration;

macro_rules! buffer_count_type {
    ($(#[$meta:meta])* $name:ident) => {
        crate::count_newtype!($(#[$meta])* $name, usize);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

buffer_count_type!(
    /// Number of buffers available in the pool
    AvailableBuffers
);

buffer_count_type!(
    /// Number of buffers currently in use
    InUseBuffers
);

buffer_count_type!(
    /// Total number of buffers in the pool
    TotalBuffers
);

/// Snapshot of the I/O buffer pool used by the summary panel.
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct BufferPoolStats {
    pub available: AvailableBuffers,
    pub in_use: InUseBuffers,
    pub total: TotalBuffers,
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
    pub active_connections: ActiveConnections,
    pub health_status: BackendHealthStatus,
    pub pending_count: PendingRequests,
    pub load_ratio: Option<f64>,
    pub stateful_count: StatefulSessions,
    pub traffic_share: Option<f64>,
    pub history: Vec<ThroughputPoint>,
}

impl BackendView {
    #[must_use]
    pub fn latest_throughput(&self) -> Option<&ThroughputPoint> {
        self.history.last()
    }
}

/// A backend entry sent to attached dashboard clients.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RemoteBackendView {
    pub server: BackendDisplay,
    pub stats: BackendStats,
    pub active_connections: ActiveConnections,
    pub health_status: BackendHealthStatus,
    pub pending_count: PendingRequests,
    pub stateful_count: StatefulSessions,
    pub traffic_share: Option<f64>,
    pub history: Vec<ThroughputPoint>,
}

impl RemoteBackendView {
    #[must_use]
    pub fn latest_throughput(&self) -> Option<&ThroughputPoint> {
        self.history.last()
    }
}

/// Serialized user stats used by the dashboard payload.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DashboardUserStats {
    pub username: String,
    pub active_connections: UserActiveConnections,
    pub total_connections: TotalConnections,
    pub bytes_sent: BytesSent,
    pub bytes_received: BytesReceived,
    pub bytes_sent_per_sec: BytesPerSecondRate,
    pub bytes_received_per_sec: BytesPerSecondRate,
    pub total_commands: CommandCount,
    pub errors: ErrorCount,
}

impl DashboardUserStats {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        username: String,
        active_connections: UserActiveConnections,
        total_connections: TotalConnections,
        bytes_sent: BytesSent,
        bytes_received: BytesReceived,
        bytes_sent_per_sec: BytesPerSecondRate,
        bytes_received_per_sec: BytesPerSecondRate,
        total_commands: CommandCount,
        errors: ErrorCount,
    ) -> Self {
        Self {
            username,
            active_connections: UserActiveConnections::new(
                active_connections
                    .get()
                    .min(usize::try_from(total_connections.get()).unwrap_or(usize::MAX)),
            ),
            total_connections,
            bytes_sent,
            bytes_received,
            bytes_sent_per_sec,
            bytes_received_per_sec,
            total_commands,
            errors,
        }
    }

    #[must_use]
    pub fn from_user_stats(user: &UserStats) -> Self {
        Self::from(user)
    }

    #[must_use]
    pub fn from_user_stats_with_active_connections(
        user: &UserStats,
        active_connections: UserActiveConnections,
    ) -> Self {
        Self::new(
            user.username.clone(),
            active_connections,
            user.total_connections,
            user.bytes_sent,
            user.bytes_received,
            user.bytes_sent_per_sec,
            user.bytes_received_per_sec,
            user.total_commands,
            user.errors,
        )
    }

    #[must_use]
    pub const fn total_bytes(&self) -> u64 {
        self.bytes_sent
            .as_u64()
            .saturating_add(self.bytes_received.as_u64())
    }
}

impl From<&UserStats> for DashboardUserStats {
    fn from(user: &UserStats) -> Self {
        Self::from_user_stats_with_active_connections(user, user.active_connections)
    }
}

#[derive(Debug, Clone, Copy)]
struct ActiveConnectionBudget {
    remaining: usize,
}

impl ActiveConnectionBudget {
    #[must_use]
    const fn new(global_active: ActiveConnections) -> Self {
        Self {
            remaining: global_active.get(),
        }
    }

    fn take_user(&mut self, requested: UserActiveConnections) -> UserActiveConnections {
        let granted = requested.get().min(self.remaining);
        self.remaining -= granted;
        UserActiveConnections::new(granted)
    }

    fn take_backend(&mut self, requested: ActiveConnections) -> ActiveConnections {
        let granted = requested.get().min(self.remaining);
        self.remaining -= granted;
        ActiveConnections::new(granted)
    }
}

/// Serialized metrics used by the dashboard payload.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DashboardMetrics {
    pub total_connections: TotalConnections,
    pub active_connections: ActiveConnections,
    pub stateful_sessions: StatefulSessions,
    pub client_to_backend_bytes: ClientToBackendBytes,
    pub backend_to_client_bytes: BackendToClientBytes,
    pub uptime: Duration,
    pub cache_entries: CacheEntries,
    pub cache_size_bytes: u64,
    pub cache_hit_rate: f64,
    pub disk_cache: Option<DiskCacheStats>,
    pub pipeline_batches: PipelineBatches,
    pub pipeline_commands: PipelineCommands,
    pub pipeline_requests_queued: PipelineRequestsQueued,
    pub pipeline_requests_completed: PipelineRequestsCompleted,
    #[serde(default)]
    pub in_flight_requests: PendingRequests,
}

impl DashboardMetrics {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        total_connections: TotalConnections,
        active_connections: ActiveConnections,
        stateful_sessions: StatefulSessions,
        client_to_backend_bytes: ClientToBackendBytes,
        backend_to_client_bytes: BackendToClientBytes,
        uptime: Duration,
        cache_entries: CacheEntries,
        cache_size_bytes: u64,
        cache_hit_rate: f64,
        disk_cache: Option<DiskCacheStats>,
        pipeline_batches: PipelineBatches,
        pipeline_commands: PipelineCommands,
        pipeline_requests_queued: PipelineRequestsQueued,
        pipeline_requests_completed: PipelineRequestsCompleted,
    ) -> Self {
        let active_connections = ActiveConnections::new(
            active_connections
                .get()
                .min(usize::try_from(total_connections.get()).unwrap_or(usize::MAX)),
        );
        let stateful_sessions =
            StatefulSessions::new(stateful_sessions.get().min(active_connections.get()));
        let pipeline_requests_completed = PipelineRequestsCompleted::new(
            pipeline_requests_completed
                .get()
                .min(pipeline_requests_queued.get()),
        );
        let pipeline_batches =
            PipelineBatches::new(pipeline_batches.get().min(pipeline_commands.get()));
        let cache_hit_rate = cache_hit_rate.clamp(0.0, 100.0);

        Self {
            total_connections,
            active_connections,
            stateful_sessions,
            client_to_backend_bytes,
            backend_to_client_bytes,
            uptime,
            cache_entries,
            cache_size_bytes,
            cache_hit_rate,
            disk_cache,
            pipeline_batches,
            pipeline_commands,
            pipeline_requests_queued,
            pipeline_requests_completed,
            in_flight_requests: PendingRequests::ZERO,
        }
    }

    #[must_use]
    pub fn from_snapshot(snapshot: &MetricsSnapshot) -> Self {
        Self::from(snapshot)
    }

    #[must_use]
    pub fn format_uptime(&self) -> ArrayString<24> {
        let secs = self.uptime.as_secs();
        let hours = secs / 3600;
        let minutes = (secs % 3600) / 60;
        let seconds = secs % 60;

        let mut text = ArrayString::<24>::new();
        if hours > 0 {
            let _ = write!(&mut text, "{hours}h {minutes}m {seconds}s");
        } else if minutes > 0 {
            let _ = write!(&mut text, "{minutes}m {seconds}s");
        } else {
            let _ = write!(&mut text, "{seconds}s");
        }

        text
    }

    #[must_use]
    pub const fn total_bytes(&self) -> u64 {
        self.client_to_backend_bytes.as_u64() + self.backend_to_client_bytes.as_u64()
    }
}

impl From<&MetricsSnapshot> for DashboardMetrics {
    fn from(snapshot: &MetricsSnapshot) -> Self {
        Self::new(
            snapshot.total_connections,
            snapshot.active_connections,
            snapshot.stateful_sessions,
            snapshot.client_to_backend_bytes,
            snapshot.backend_to_client_bytes,
            snapshot.uptime,
            snapshot.cache_entries,
            snapshot.cache_size_bytes,
            snapshot.cache_hit_rate,
            snapshot.disk_cache,
            snapshot.pipeline_batches,
            snapshot.pipeline_commands,
            snapshot.pipeline_requests_queued,
            snapshot.pipeline_requests_completed,
        )
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

/// Slim dashboard state sent to attached dashboard clients.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RemoteDashboardState {
    pub metrics: DashboardMetrics,
    pub backend_views: Vec<RemoteBackendView>,
    pub top_users: Vec<DashboardUserStats>,
    pub latest_client_throughput: Option<ThroughputPoint>,
    pub system_stats: SystemStats,
    pub log_lines: Vec<String>,
}

impl DashboardState {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        metrics: DashboardMetrics,
        mut backend_views: Vec<BackendView>,
        mut top_users: Vec<DashboardUserStats>,
        client_history: Vec<ThroughputPoint>,
        system_stats: SystemStats,
        view_mode: ViewMode,
        show_details: bool,
        log_lines: Vec<String>,
        buffer_pool: Option<BufferPoolStats>,
    ) -> Self {
        normalize_backend_active_counts(&mut backend_views, metrics.active_connections);
        normalize_user_active_counts(&mut top_users, metrics.active_connections);

        Self {
            metrics,
            backend_views,
            top_users,
            client_history,
            system_stats,
            view_mode,
            show_details,
            log_lines,
            buffer_pool,
        }
    }

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
    pub fn backend_pending_count(&self, backend_idx: usize) -> PendingRequests {
        self.backend_view(backend_idx)
            .map_or(PendingRequests::ZERO, |view| view.pending_count)
    }

    #[must_use]
    pub fn backend_load_ratio(&self, backend_idx: usize) -> Option<f64> {
        self.backend_view(backend_idx)
            .and_then(|view| view.load_ratio)
    }

    #[must_use]
    pub fn backend_stateful_count(&self, backend_idx: usize) -> StatefulSessions {
        self.backend_view(backend_idx)
            .map_or(StatefulSessions::ZERO, |view| view.stateful_count)
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

impl RemoteDashboardState {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        metrics: DashboardMetrics,
        mut backend_views: Vec<RemoteBackendView>,
        mut top_users: Vec<DashboardUserStats>,
        latest_client_throughput: Option<ThroughputPoint>,
        system_stats: SystemStats,
        log_lines: Vec<String>,
    ) -> Self {
        normalize_remote_backend_active_counts(&mut backend_views, metrics.active_connections);
        normalize_user_active_counts(&mut top_users, metrics.active_connections);

        Self {
            metrics,
            backend_views,
            top_users,
            latest_client_throughput,
            system_stats,
            log_lines,
        }
    }

    #[must_use]
    fn backend_view(&self, backend_idx: usize) -> Option<&RemoteBackendView> {
        self.backend_views.get(backend_idx)
    }

    #[must_use]
    pub fn latest_client_throughput(&self) -> Option<&ThroughputPoint> {
        self.latest_client_throughput.as_ref()
    }

    #[must_use]
    pub fn backend_pending_count(&self, backend_idx: usize) -> PendingRequests {
        self.backend_view(backend_idx)
            .map_or(PendingRequests::ZERO, |view| view.pending_count)
    }

    #[must_use]
    pub fn backend_stateful_count(&self, backend_idx: usize) -> StatefulSessions {
        self.backend_view(backend_idx)
            .map_or(StatefulSessions::ZERO, |view| view.stateful_count)
    }

    #[must_use]
    pub fn backend_traffic_share(&self, backend_idx: usize) -> Option<f64> {
        self.backend_view(backend_idx)
            .and_then(|view| view.traffic_share)
    }
}

impl From<BackendView> for RemoteBackendView {
    fn from(view: BackendView) -> Self {
        Self {
            server: view.server,
            stats: view.stats,
            active_connections: view.active_connections,
            health_status: view.health_status,
            pending_count: view.pending_count,
            stateful_count: view.stateful_count,
            traffic_share: view.traffic_share,
            history: view.history,
        }
    }
}

impl From<DashboardState> for RemoteDashboardState {
    fn from(state: DashboardState) -> Self {
        Self::new(
            state.metrics,
            state
                .backend_views
                .into_iter()
                .map(RemoteBackendView::from)
                .collect(),
            state.top_users,
            state.client_history.last().cloned(),
            state.system_stats,
            state.log_lines,
        )
    }
}

fn normalize_user_active_counts(
    users: &mut [DashboardUserStats],
    global_active: ActiveConnections,
) {
    let mut budget = ActiveConnectionBudget::new(global_active);
    users.iter_mut().for_each(|user| {
        user.active_connections = budget.take_user(user.active_connections);
    });
}

fn normalize_backend_active_counts(backends: &mut [BackendView], global_active: ActiveConnections) {
    let mut budget = ActiveConnectionBudget::new(global_active);
    backends.iter_mut().for_each(|backend| {
        backend.active_connections = budget.take_backend(backend.active_connections);
    });
}

fn normalize_remote_backend_active_counts(
    backends: &mut [RemoteBackendView],
    global_active: ActiveConnections,
) {
    let mut budget = ActiveConnectionBudget::new(global_active);
    backends.iter_mut().for_each(|backend| {
        backend.active_connections = budget.take_backend(backend.active_connections);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{BackendHealthStatus, BackendStats, UserActiveConnections};
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
            active_connections: ActiveConnections::new(1),
            health_status: BackendHealthStatus::Healthy,
            pending_count: PendingRequests::new(2),
            load_ratio: Some(0.5),
            stateful_count: StatefulSessions::new(3),
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
        assert_eq!(state.backend_pending_count(1), PendingRequests::ZERO);
        assert!(state.backend_load_ratio(1).is_none());
        assert_eq!(state.backend_stateful_count(1), StatefulSessions::ZERO);
        assert!(state.backend_traffic_share(1).is_none());
    }

    #[test]
    fn remote_dashboard_state_keeps_latest_client_point_and_drops_local_only_fields() {
        let latest_client = ThroughputPoint::new_client(
            Timestamp::now(),
            Throughput::new(10.0),
            Throughput::new(20.0),
        );
        let state = DashboardState {
            metrics: DashboardMetrics::default(),
            backend_views: vec![sample_backend_view()],
            top_users: Vec::new(),
            client_history: vec![
                ThroughputPoint::new_client(
                    Timestamp::now(),
                    Throughput::new(1.0),
                    Throughput::new(2.0),
                ),
                latest_client.clone(),
            ],
            system_stats: SystemStats::default(),
            view_mode: ViewMode::LogFullscreen,
            show_details: true,
            log_lines: vec!["hello".to_string()],
            buffer_pool: Some(BufferPoolStats {
                available: AvailableBuffers::new(1),
                in_use: InUseBuffers::new(2),
                total: TotalBuffers::new(3),
            }),
        };

        let remote = RemoteDashboardState::from(state);

        assert_eq!(remote.backend_views.len(), 1);
        assert_eq!(remote.log_lines, vec!["hello".to_string()]);
        assert_eq!(
            remote
                .latest_client_throughput()
                .map(|point| point.sent_per_sec().get()),
            Some(latest_client.sent_per_sec().get())
        );
    }

    #[test]
    fn dashboard_user_stats_keeps_live_and_lifetime_connections_separate() {
        let user = crate::metrics::UserStats {
            username: "alice".to_string(),
            active_connections: UserActiveConnections::new(2),
            total_connections: crate::types::TotalConnections::new(9),
            ..Default::default()
        };

        let dashboard = DashboardUserStats::from_user_stats(&user);

        assert_eq!(dashboard.active_connections.get(), 2);
        assert_eq!(dashboard.total_connections.get(), 9);
    }
}
