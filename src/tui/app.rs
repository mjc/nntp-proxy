//! TUI application state and logic

use crate::config::Server;
use crate::metrics::{MetricsCollector, MetricsSnapshot};
use crate::router::BackendSelector;
use crate::tui::dashboard::{
    BackendDisplay, BackendView, BufferPoolStats, DashboardMetrics, DashboardState,
    DashboardUserStats,
};
use crate::tui::log_capture::LogBuffer;
use crate::tui::rate_estimator::{CumulativeCount, RateEstimate, RateEstimator, RatePerSecond};
use crate::types::tui::{CommandsPerSecond, HistorySize, Throughput, Timestamp};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

/// TUI view mode - controls what is displayed
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum ViewMode {
    /// Normal view - shows all panels
    #[default]
    Normal,
    /// Log fullscreen - shows only title and logs (mostly fullscreen)
    LogFullscreen,
}

/// Historical throughput data point (generic over traffic direction)
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ThroughputPoint {
    /// Smoothed bytes sent per second
    sent_per_sec: Throughput,
    /// Smoothed bytes received per second
    received_per_sec: Throughput,
    /// Smoothed commands processed per second (backend only)
    commands_per_sec: Option<CommandsPerSecond>,
    /// Raw interval bytes sent per second
    #[serde(default)]
    raw_sent_per_sec: Option<Throughput>,
    /// Raw interval bytes received per second
    #[serde(default)]
    raw_received_per_sec: Option<Throughput>,
    /// Raw interval commands processed per second (backend only)
    #[serde(default)]
    raw_commands_per_sec: Option<CommandsPerSecond>,
}

impl ThroughputPoint {
    /// Create a new backend throughput point
    #[must_use]
    pub const fn new_backend(
        _timestamp: Timestamp,
        sent_per_sec: Throughput,
        received_per_sec: Throughput,
        commands_per_sec: CommandsPerSecond,
    ) -> Self {
        Self {
            sent_per_sec,
            received_per_sec,
            commands_per_sec: Some(commands_per_sec),
            raw_sent_per_sec: Some(sent_per_sec),
            raw_received_per_sec: Some(received_per_sec),
            raw_commands_per_sec: Some(commands_per_sec),
        }
    }

    /// Create a new backend throughput point from raw and smoothed estimates.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub const fn new_backend_estimate(
        _timestamp: Timestamp,
        raw_sent_per_sec: Throughput,
        sent_per_sec: Throughput,
        raw_received_per_sec: Throughput,
        received_per_sec: Throughput,
        raw_commands_per_sec: CommandsPerSecond,
        commands_per_sec: CommandsPerSecond,
    ) -> Self {
        Self {
            sent_per_sec,
            received_per_sec,
            commands_per_sec: Some(commands_per_sec),
            raw_sent_per_sec: Some(raw_sent_per_sec),
            raw_received_per_sec: Some(raw_received_per_sec),
            raw_commands_per_sec: Some(raw_commands_per_sec),
        }
    }

    /// Create a new client throughput point
    #[must_use]
    pub const fn new_client(
        _timestamp: Timestamp,
        sent_per_sec: Throughput,
        received_per_sec: Throughput,
    ) -> Self {
        Self {
            sent_per_sec,
            received_per_sec,
            commands_per_sec: None,
            raw_sent_per_sec: Some(sent_per_sec),
            raw_received_per_sec: Some(received_per_sec),
            raw_commands_per_sec: None,
        }
    }

    /// Create a new client throughput point from raw and smoothed estimates.
    #[must_use]
    pub const fn new_client_estimate(
        _timestamp: Timestamp,
        raw_sent_per_sec: Throughput,
        sent_per_sec: Throughput,
        raw_received_per_sec: Throughput,
        received_per_sec: Throughput,
    ) -> Self {
        Self {
            sent_per_sec,
            received_per_sec,
            commands_per_sec: None,
            raw_sent_per_sec: Some(raw_sent_per_sec),
            raw_received_per_sec: Some(raw_received_per_sec),
            raw_commands_per_sec: None,
        }
    }

    /// Get sent bytes per second
    #[must_use]
    #[inline]
    pub const fn sent_per_sec(&self) -> Throughput {
        self.sent_per_sec
    }

    /// Get received bytes per second
    #[must_use]
    #[inline]
    pub const fn received_per_sec(&self) -> Throughput {
        self.received_per_sec
    }

    /// Get commands per second (if backend point)
    #[must_use]
    #[inline]
    pub const fn commands_per_sec(&self) -> Option<CommandsPerSecond> {
        self.commands_per_sec
    }

    /// Get raw sent bytes per second.
    #[must_use]
    #[inline]
    pub fn raw_sent_per_sec(&self) -> Throughput {
        self.raw_sent_per_sec.unwrap_or(self.sent_per_sec)
    }

    /// Get raw received bytes per second.
    #[must_use]
    #[inline]
    pub fn raw_received_per_sec(&self) -> Throughput {
        self.raw_received_per_sec.unwrap_or(self.received_per_sec)
    }

    /// Get raw commands per second (if backend point).
    #[must_use]
    #[inline]
    pub fn raw_commands_per_sec(&self) -> Option<CommandsPerSecond> {
        self.raw_commands_per_sec.or(self.commands_per_sec)
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
    const fn points(&self) -> &VecDeque<ThroughputPoint> {
        &self.points
    }

    /// Get the latest point
    #[must_use]
    fn latest(&self) -> Option<&ThroughputPoint> {
        self.points.back()
    }
}

#[derive(Debug, Clone, Default)]
struct BackendRateEstimators {
    sent: RateEstimator,
    received: RateEstimator,
    commands: RateEstimator,
}

#[derive(Debug, Clone, Default)]
struct UserRateEstimators {
    sent: RateEstimator,
    received: RateEstimator,
}

/// TUI application builder
///
/// Provides a fluent API for constructing `TuiApp` instances with optional configuration.
/// This replaces the multiple constructor pattern (new, `with_log_buffer`, `with_history_size`)
/// with a single, flexible builder.
///
/// # Examples
///
/// ```ignore
/// use nntp_proxy::tui::TuiAppBuilder;
///
/// // Basic app
/// let app = TuiAppBuilder::new(metrics, router, servers).build();
///
/// // With log buffer
/// let app = TuiAppBuilder::new(metrics, router, servers)
///     .with_log_buffer(log_buffer)
///     .build();
///
/// // With custom history size
/// let app = TuiAppBuilder::new(metrics, router, servers)
///     .with_history_size(HistorySize::new(120))
///     .build();
/// ```
pub struct TuiAppBuilder {
    metrics: MetricsCollector,
    router: Arc<BackendSelector>,
    servers: Arc<[Server]>,
    cache: Option<Arc<crate::cache::UnifiedCache>>,
    buffer_pool: Option<crate::pool::BufferPool>,
    log_buffer: Option<LogBuffer>,
    history_size: HistorySize,
}

impl TuiAppBuilder {
    /// Create a new TUI app builder
    #[must_use]
    pub const fn new(
        metrics: MetricsCollector,
        router: Arc<BackendSelector>,
        servers: Arc<[Server]>,
    ) -> Self {
        Self {
            metrics,
            router,
            servers,
            cache: None,
            buffer_pool: None,
            log_buffer: None,
            history_size: HistorySize::DEFAULT,
        }
    }

    /// Set the article cache for monitoring cache statistics
    #[must_use]
    pub fn with_cache(mut self, cache: Arc<crate::cache::UnifiedCache>) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Set the buffer pool for monitoring I/O buffer statistics
    #[must_use]
    pub fn with_buffer_pool(mut self, buffer_pool: crate::pool::BufferPool) -> Self {
        self.buffer_pool = Some(buffer_pool);
        self
    }

    /// Set the log buffer for displaying recent log messages
    #[must_use]
    pub fn with_log_buffer(mut self, log_buffer: LogBuffer) -> Self {
        self.log_buffer = Some(log_buffer);
        self
    }

    /// Set custom history size (default is 60 points)
    #[must_use]
    pub const fn with_history_size(mut self, history_size: HistorySize) -> Self {
        self.history_size = history_size;
        self
    }

    /// Build the `TuiApp`
    #[must_use]
    pub fn build(self) -> TuiApp {
        use crate::tui::SystemMonitor;

        let snapshot = Arc::new(self.metrics.snapshot(self.cache.as_deref()));
        let backend_count = self.servers.len();

        // Initialize empty history for each backend
        let backend_history = (0..backend_count)
            .map(|_| ThroughputHistory::new(self.history_size))
            .collect();
        let backend_rate_estimators = (0..backend_count)
            .map(|_| BackendRateEstimators::default())
            .collect();

        TuiApp {
            metrics: self.metrics,
            router: self.router,
            servers: self.servers,
            cache: self.cache,
            buffer_pool: self.buffer_pool,
            snapshot,
            backend_history,
            backend_rate_estimators,
            client_history: ThroughputHistory::new(self.history_size),
            client_sent_estimator: RateEstimator::default(),
            client_received_estimator: RateEstimator::default(),
            user_rate_estimators: HashMap::new(),
            previous_snapshot: None,
            last_update: Timestamp::now(),
            log_buffer: Arc::new(self.log_buffer.unwrap_or_default()),
            view_mode: ViewMode::default(),
            show_details: false,
            system_monitor: SystemMonitor::new(),
            system_stats: crate::tui::SystemStats::default(),
        }
    }
}

/// TUI application state
pub struct TuiApp {
    /// Metrics collector (shared with proxy)
    metrics: MetricsCollector,
    /// Router for getting pending command counts
    router: Arc<BackendSelector>,
    /// Server configurations for display names
    servers: Arc<[Server]>,
    /// Current metrics snapshot (Arc for zero-cost sharing)
    snapshot: Arc<MetricsSnapshot>,
    /// Historical throughput data per backend
    backend_history: Vec<ThroughputHistory>,
    /// Per-backend rate estimators
    backend_rate_estimators: Vec<BackendRateEstimators>,
    /// Historical client throughput (global)
    client_history: ThroughputHistory,
    /// Global client-to-backend rate estimator
    client_sent_estimator: RateEstimator,
    /// Global backend-to-client rate estimator
    client_received_estimator: RateEstimator,
    /// Per-user byte rate estimators keyed by username
    user_rate_estimators: HashMap<String, UserRateEstimators>,
    /// Previous snapshot for calculating deltas (Arc for zero-cost sharing)
    previous_snapshot: Option<Arc<MetricsSnapshot>>,
    /// Last update time
    last_update: Timestamp,
    /// Log buffer for displaying recent log messages
    log_buffer: Arc<LogBuffer>,
    /// Current view mode (normal or log fullscreen)
    view_mode: ViewMode,
    /// Show detailed metrics (timing breakdown, etc.)
    show_details: bool,
    /// System resource monitor
    system_monitor: crate::tui::SystemMonitor,
    /// Current system stats (CPU, memory, threads)
    system_stats: crate::tui::SystemStats,
    /// Article cache (optional - only present in caching mode)
    cache: Option<Arc<crate::cache::UnifiedCache>>,
    /// Buffer pool for I/O operations (optional - for monitoring buffer stats)
    buffer_pool: Option<crate::pool::BufferPool>,
}

impl TuiApp {
    #[cfg(test)]
    #[allow(clippy::cast_precision_loss)] // TUI rates are derived display values, not exact persisted counters.
    const fn counter_as_f64(value: u64) -> f64 {
        // TUI throughput and rate calculations are display-only aggregates.
        // The exact counters remain stored as integers in the metrics snapshot.
        value as f64
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // TUI smoothed rates are non-negative display values.
    const fn rate_as_u64(rate: RatePerSecond) -> u64 {
        rate.get() as u64
    }

    /// Create a new TUI application
    ///
    /// **Note:** Prefer using `TuiAppBuilder` for more flexibility.
    /// This method is a convenience wrapper.
    #[must_use]
    pub fn new(
        metrics: MetricsCollector,
        router: Arc<BackendSelector>,
        servers: Arc<[Server]>,
    ) -> Self {
        TuiAppBuilder::new(metrics, router, servers).build()
    }

    /// Calculate byte transfer rate from byte delta and time delta
    #[cfg(test)]
    #[inline]
    fn calculate_rate(byte_delta: u64, time_delta_secs: f64) -> Throughput {
        if time_delta_secs > 0.0 {
            Throughput::new(Self::counter_as_f64(byte_delta) / time_delta_secs)
        } else {
            Throughput::zero()
        }
    }

    /// Calculate command rate from command delta and time delta
    #[cfg(test)]
    #[inline]
    fn calculate_command_rate(cmd_delta: u64, time_delta_secs: f64) -> CommandsPerSecond {
        if time_delta_secs > 0.0 {
            CommandsPerSecond::new(Self::counter_as_f64(cmd_delta) / time_delta_secs)
        } else {
            CommandsPerSecond::zero()
        }
    }

    fn estimate_or_decay(
        estimator: &mut RateEstimator,
        total: u64,
        now: Timestamp,
    ) -> RateEstimate {
        estimator
            .record(CumulativeCount::new(total), now)
            .unwrap_or_else(|| RateEstimate::new(RatePerSecond::zero(), estimator.rate_at(now)))
    }

    fn throughput_estimate(
        estimator: &mut RateEstimator,
        total: u64,
        now: Timestamp,
    ) -> (Throughput, Throughput) {
        let estimate = Self::estimate_or_decay(estimator, total, now);
        (
            Throughput::new(estimate.raw().get()),
            Throughput::new(estimate.smoothed().get()),
        )
    }

    fn command_estimate(
        estimator: &mut RateEstimator,
        total: u64,
        now: Timestamp,
    ) -> (CommandsPerSecond, CommandsPerSecond) {
        let estimate = Self::estimate_or_decay(estimator, total, now);
        (
            CommandsPerSecond::new(estimate.raw().get()),
            CommandsPerSecond::new(estimate.smoothed().get()),
        )
    }

    fn build_backend_estimated_throughput_point(
        now: Timestamp,
        estimators: &mut BackendRateEstimators,
        stats: &crate::metrics::BackendStats,
    ) -> ThroughputPoint {
        let (raw_sent, sent) =
            Self::throughput_estimate(&mut estimators.sent, stats.bytes_sent.as_u64(), now);
        let (raw_received, received) =
            Self::throughput_estimate(&mut estimators.received, stats.bytes_received.as_u64(), now);
        let (raw_commands, commands) =
            Self::command_estimate(&mut estimators.commands, stats.total_commands.get(), now);

        ThroughputPoint::new_backend_estimate(
            now,
            raw_sent,
            sent,
            raw_received,
            received,
            raw_commands,
            commands,
        )
    }

    #[must_use]
    #[cfg(test)]
    fn build_backend_throughput_point(
        now: Timestamp,
        time_delta: f64,
        new_stats: &crate::metrics::BackendStats,
        prev_stats: &crate::metrics::BackendStats,
    ) -> ThroughputPoint {
        let sent_delta = new_stats.bytes_sent.saturating_sub(prev_stats.bytes_sent);
        let recv_delta = new_stats
            .bytes_received
            .saturating_sub(prev_stats.bytes_received);
        let cmd_delta = new_stats
            .total_commands
            .saturating_sub(prev_stats.total_commands);

        ThroughputPoint::new_backend(
            now,
            Self::calculate_rate(sent_delta.into(), time_delta),
            Self::calculate_rate(recv_delta.into(), time_delta),
            Self::calculate_command_rate(cmd_delta.get(), time_delta),
        )
    }

    fn estimate_user_rates(
        now: Timestamp,
        estimators: &mut UserRateEstimators,
        current: &crate::metrics::UserStats,
    ) -> crate::metrics::UserStats {
        use crate::types::BytesPerSecondRate;

        let sent = Self::estimate_or_decay(&mut estimators.sent, current.bytes_sent.as_u64(), now);
        let received = Self::estimate_or_decay(
            &mut estimators.received,
            current.bytes_received.as_u64(),
            now,
        );

        crate::metrics::UserStats {
            username: current.username.clone(),
            active_connections: current.active_connections,
            total_connections: current.total_connections,
            bytes_sent: current.bytes_sent,
            bytes_received: current.bytes_received,
            total_commands: current.total_commands,
            errors: current.errors,
            bytes_sent_per_sec: BytesPerSecondRate::new(Self::rate_as_u64(sent.smoothed())),
            bytes_received_per_sec: BytesPerSecondRate::new(Self::rate_as_u64(received.smoothed())),
        }
    }

    fn empty_user_estimators_at(now: Timestamp) -> UserRateEstimators {
        let mut estimators = UserRateEstimators::default();
        estimators.sent.record(CumulativeCount::new(0), now);
        estimators.received.record(CumulativeCount::new(0), now);
        estimators
    }

    fn prune_user_estimators(&mut self, user_stats: &[crate::metrics::UserStats]) {
        let active_users = user_stats
            .iter()
            .map(|stats| stats.username.as_str())
            .collect::<HashSet<_>>();
        self.user_rate_estimators
            .retain(|username, _| active_users.contains(username.as_str()));
    }

    fn seed_rate_estimators(&mut self, snapshot: &crate::metrics::MetricsSnapshot, now: Timestamp) {
        self.client_sent_estimator.record(
            CumulativeCount::new(snapshot.client_to_backend_bytes.as_u64()),
            now,
        );
        self.client_received_estimator.record(
            CumulativeCount::new(snapshot.backend_to_client_bytes.as_u64()),
            now,
        );

        self.backend_rate_estimators
            .iter_mut()
            .zip(snapshot.backend_stats.iter())
            .for_each(|(estimators, stats)| {
                estimators
                    .sent
                    .record(CumulativeCount::new(stats.bytes_sent.as_u64()), now);
                estimators
                    .received
                    .record(CumulativeCount::new(stats.bytes_received.as_u64()), now);
                estimators
                    .commands
                    .record(CumulativeCount::new(stats.total_commands.get()), now);
            });

        self.user_rate_estimators.clear();
        snapshot.user_stats.iter().for_each(|stats| {
            let mut estimators = UserRateEstimators::default();
            estimators
                .sent
                .record(CumulativeCount::new(stats.bytes_sent.as_u64()), now);
            estimators
                .received
                .record(CumulativeCount::new(stats.bytes_received.as_u64()), now);
            self.user_rate_estimators
                .insert(stats.username.clone(), estimators);
        });
    }

    /// Update metrics snapshot and calculate throughput
    pub fn update(&mut self) {
        // Update system stats (CPU, memory)
        self.system_stats = self.system_monitor.update();

        let new_snapshot = Arc::new(
            self.metrics
                .snapshot(self.cache.as_deref())
                .with_pool_status(&self.router),
        );
        let now = Timestamp::now();

        if self.previous_snapshot.is_some() {
            let (raw_client_sent_rate, client_sent_rate) = Self::throughput_estimate(
                &mut self.client_sent_estimator,
                new_snapshot.client_to_backend_bytes.as_u64(),
                now,
            );
            let (raw_client_recv_rate, client_recv_rate) = Self::throughput_estimate(
                &mut self.client_received_estimator,
                new_snapshot.backend_to_client_bytes.as_u64(),
                now,
            );

            let client_point = ThroughputPoint::new_client_estimate(
                now,
                raw_client_sent_rate,
                client_sent_rate,
                raw_client_recv_rate,
                client_recv_rate,
            );
            self.client_history.push(client_point);

            self.backend_history
                .iter_mut()
                .zip(self.backend_rate_estimators.iter_mut())
                .zip(new_snapshot.backend_stats.iter())
                .for_each(|((history, estimators), stats)| {
                    history.push(Self::build_backend_estimated_throughput_point(
                        now, estimators, stats,
                    ));
                });

            self.prune_user_estimators(&new_snapshot.user_stats);
            let user_stats = new_snapshot
                .user_stats
                .iter()
                .map(|stats| {
                    let last_update = self.last_update;
                    let estimators = self
                        .user_rate_estimators
                        .entry(stats.username.clone())
                        .or_insert_with(|| Self::empty_user_estimators_at(last_update));
                    Self::estimate_user_rates(now, estimators, stats)
                })
                .collect();

            // Build enriched snapshot (shares backend_stats via Arc)
            self.snapshot = Arc::new(crate::metrics::MetricsSnapshot {
                user_stats,
                backend_stats: Arc::clone(&new_snapshot.backend_stats),
                ..*new_snapshot
            });
            self.previous_snapshot = Some(new_snapshot);
        } else {
            self.seed_rate_estimators(&new_snapshot, now);
            // First update - no previous snapshot
            self.previous_snapshot = Some(Arc::clone(&new_snapshot));
            self.snapshot = new_snapshot;
        }

        self.last_update = now;
    }

    /// Get current metrics snapshot
    #[must_use]
    pub fn snapshot(&self) -> &MetricsSnapshot {
        &self.snapshot
    }

    /// Get server configurations
    #[must_use]
    pub fn servers(&self) -> &[Server] {
        &self.servers
    }

    /// Get client throughput history (global)
    #[must_use]
    pub const fn client_throughput_history(&self) -> &VecDeque<ThroughputPoint> {
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
            .map_or(0, |pending| pending.get())
    }

    /// Get load ratio for a backend (pending / `max_connections`)
    #[must_use]
    pub fn backend_load_ratio(&self, backend_idx: usize) -> Option<f64> {
        use crate::types::BackendId;
        self.router
            .backend_load_ratio(BackendId::from_index(backend_idx))
            .map(|ratio| ratio.get())
    }

    /// Get stateful connection count for a backend
    #[must_use]
    pub fn backend_stateful_count(&self, backend_idx: usize) -> usize {
        use crate::types::BackendId;
        self.router
            .stateful_count(BackendId::from_index(backend_idx))
            .map_or(0, |count| count.get())
    }

    /// Get traffic share percentage for a backend
    #[must_use]
    pub fn backend_traffic_share(&self, backend_idx: usize) -> Option<f64> {
        use crate::types::BackendId;
        self.router
            .backend_traffic_share(BackendId::from_index(backend_idx))
            .map(|share| share.get())
    }

    /// Get throughput history for a backend
    #[must_use]
    ///
    /// # Panics
    /// Panics if `backend_idx` is out of range for the configured backend history.
    pub fn throughput_history(&self, backend_idx: usize) -> &VecDeque<ThroughputPoint> {
        self.backend_history(backend_idx)
            .expect("backend history index should be valid")
            .points()
    }

    /// Get latest backend throughput for a backend
    #[must_use]
    pub fn latest_backend_throughput(&self, backend_idx: usize) -> Option<&ThroughputPoint> {
        self.backend_history(backend_idx)
            .and_then(ThroughputHistory::latest)
    }

    /// Get log buffer for displaying recent log messages
    #[must_use]
    pub const fn log_buffer(&self) -> &Arc<LogBuffer> {
        &self.log_buffer
    }

    /// Get current view mode
    #[must_use]
    pub const fn view_mode(&self) -> ViewMode {
        self.view_mode
    }

    #[must_use]
    fn backend_history(&self, backend_idx: usize) -> Option<&ThroughputHistory> {
        self.backend_history.get(backend_idx)
    }

    /// Toggle between normal and log fullscreen view
    pub const fn toggle_log_fullscreen(&mut self) {
        self.view_mode = match self.view_mode {
            ViewMode::Normal => ViewMode::LogFullscreen,
            ViewMode::LogFullscreen => ViewMode::Normal,
        };
    }

    /// Toggle detailed metrics display
    pub const fn toggle_details(&mut self) {
        self.show_details = !self.show_details;
    }

    /// Get current details display state
    #[must_use]
    pub const fn show_details(&self) -> bool {
        self.show_details
    }

    /// Get current system stats (CPU, memory, threads)
    #[must_use]
    pub const fn system_stats(&self) -> &crate::tui::SystemStats {
        &self.system_stats
    }

    /// Get buffer pool (optional - for monitoring buffer stats)
    #[must_use]
    pub const fn buffer_pool(&self) -> Option<&crate::pool::BufferPool> {
        self.buffer_pool.as_ref()
    }

    /// Build a serializable snapshot of the current dashboard state.
    #[must_use]
    pub fn snapshot_state(&self) -> DashboardState {
        self.snapshot_state_with_log_limit(None)
    }

    /// Build a serializable snapshot of the current dashboard state with a bounded log tail.
    #[must_use]
    pub fn snapshot_state_with_log_limit(&self, log_limit: Option<usize>) -> DashboardState {
        let buffer_pool = self.buffer_pool.as_ref().map(Self::snapshot_buffer_pool);

        DashboardState {
            metrics: DashboardMetrics::from_snapshot(self.snapshot.as_ref()),
            backend_views: self.snapshot_backend_views(),
            top_users: self.snapshot_top_users(),
            client_history: self.client_history.points().iter().cloned().collect(),
            system_stats: self.system_stats.clone(),
            view_mode: self.view_mode,
            show_details: self.show_details,
            log_lines: self.snapshot_log_lines(log_limit),
            buffer_pool,
        }
    }

    fn snapshot_log_lines(&self, log_limit: Option<usize>) -> Vec<String> {
        match log_limit {
            Some(limit) => self.log_buffer.recent_lines(limit),
            None => self.log_buffer.all_lines(),
        }
    }

    fn snapshot_backend_views(&self) -> Vec<BackendView> {
        self.snapshot
            .backend_stats
            .iter()
            .zip(self.servers.iter())
            .enumerate()
            .map(|(i, (stats, server))| self.snapshot_backend_view(i, server, stats))
            .collect()
    }

    fn snapshot_backend_view(
        &self,
        backend_idx: usize,
        server: &Server,
        stats: &crate::metrics::BackendStats,
    ) -> BackendView {
        BackendView {
            server: BackendDisplay {
                host: server.host.clone(),
                port: server.port,
                name: server.name.clone(),
                max_connections: server.max_connections,
            },
            stats: stats.clone(),
            active_connections: stats.active_connections.get(),
            health_status: stats.health_status,
            pending_count: self.backend_pending_count(backend_idx),
            load_ratio: self.backend_load_ratio(backend_idx),
            stateful_count: self.backend_stateful_count(backend_idx),
            traffic_share: self.backend_traffic_share(backend_idx),
            history: Self::snapshot_throughput_history(self.backend_history(backend_idx)),
        }
    }

    fn snapshot_throughput_history(history: Option<&ThroughputHistory>) -> Vec<ThroughputPoint> {
        history
            .map(|history| history.points().iter().cloned().collect())
            .unwrap_or_default()
    }

    fn snapshot_buffer_pool(pool: &crate::pool::BufferPool) -> BufferPoolStats {
        let (available, in_use, total) = pool.stats();
        BufferPoolStats {
            available,
            in_use,
            total,
        }
    }

    fn snapshot_top_users(&self) -> Vec<DashboardUserStats> {
        let mut top_users = self
            .snapshot
            .user_stats
            .iter()
            .map(DashboardUserStats::from_user_stats)
            .collect::<Vec<_>>();
        top_users.sort_by_key(|user| std::cmp::Reverse(user.total_bytes()));
        top_users.truncate(10);
        top_users
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Server;
    use crate::metrics::MetricsCollector;
    use crate::router::BackendSelector;
    use crate::types::Port;
    use std::sync::Arc;
    use std::time::Duration;

    /// Helper to create test servers
    fn create_test_servers(count: usize) -> Arc<[Server]> {
        (0..count)
            .map(|i| {
                Server::builder(
                    format!("backend{i}.example.com"),
                    Port::try_new(119).unwrap(),
                )
                .name(format!("Backend {i}"))
                .build()
                .unwrap()
            })
            .collect::<Vec<_>>()
            .into()
    }

    /// Helper to create test `TuiApp`
    fn create_test_app(backend_count: usize) -> TuiApp {
        let metrics = MetricsCollector::new(backend_count);
        let router = Arc::new(BackendSelector::new());
        let servers = create_test_servers(backend_count);
        TuiApp::new(metrics, router, servers)
    }

    fn assert_f64_eq(actual: f64, expected: f64) {
        assert_eq!(actual.to_bits(), expected.to_bits());
    }

    /// Test for the bug where `previous_snapshot` was set to self.snapshot instead of `new_snapshot`.
    /// This caused the TUI to skip every other snapshot, calculating deltas over 2x the time period
    /// and showing 2x the actual throughput.
    ///
    /// The bug: `previous_snapshot` = Some(self.snapshot.clone())  // OLD snapshot
    /// The fix: `previous_snapshot` = `Some(new_snapshot.clone())`   // NEW snapshot (just used)
    #[test]
    fn test_previous_snapshot_uses_new_snapshot_not_old() {
        let metrics = MetricsCollector::new(1);
        let router = Arc::new(BackendSelector::new());
        let servers: Arc<[Server]> = vec![
            Server::builder("test.example.com", Port::try_new(119).unwrap())
                .name("Test Server".to_string())
                .build()
                .unwrap(),
        ]
        .into();

        let mut app = TuiApp::new(metrics.clone(), router, servers);

        // Update 1: 1000 bytes total
        metrics.record_backend_to_client_bytes_for(crate::types::BackendId::from_index(0), 1000);
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
        metrics.record_backend_to_client_bytes_for(crate::types::BackendId::from_index(0), 1000);
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
        metrics.record_backend_to_client_bytes_for(crate::types::BackendId::from_index(0), 1000);
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

    #[test]
    fn test_initial_state() {
        let app = create_test_app(2);

        // Initial state checks
        assert_eq!(app.snapshot().active_connections, 0);
        assert_eq!(app.snapshot().total_connections, 0);
        assert_eq!(app.snapshot().backend_stats.len(), 2);
        assert!(app.previous_snapshot.is_none());
        assert!(app.latest_client_throughput().is_none());
    }

    #[test]
    fn test_throughput_history_initialization() {
        let app = create_test_app(3);

        // Should have empty histories for all backends
        assert_eq!(app.backend_history.len(), 3);
        for i in 0..3 {
            assert_eq!(app.throughput_history(i).len(), 0);
            assert!(app.latest_backend_throughput(i).is_none());
        }

        // Client history should also be empty
        assert_eq!(app.client_throughput_history().len(), 0);
    }

    #[test]
    fn test_first_update_establishes_baseline() {
        let metrics = MetricsCollector::new(1);
        let router = Arc::new(BackendSelector::new());
        let servers = create_test_servers(1);
        let mut app = TuiApp::new(metrics.clone(), router, servers);

        // Simulate some traffic
        metrics.record_backend_to_client_bytes_for(crate::types::BackendId::from_index(0), 1000);

        // First update
        app.update();

        // Should have a previous snapshot now
        assert!(app.previous_snapshot.is_some());

        // But no throughput points yet (need delta)
        assert!(app.latest_client_throughput().is_none());
        assert!(app.latest_backend_throughput(0).is_none());
    }

    #[test]
    fn test_throughput_calculation_with_time_delta() {
        let metrics = MetricsCollector::new(1);
        let router = Arc::new(BackendSelector::new());
        let servers = create_test_servers(1);
        let mut app = TuiApp::new(metrics.clone(), router, servers);

        // First update (baseline)
        app.update();

        // Wait a bit and simulate traffic
        std::thread::sleep(Duration::from_millis(100));
        metrics.record_backend_to_client_bytes_for(crate::types::BackendId::from_index(0), 100_000);

        // Second update
        app.update();

        // Now should have throughput data
        assert!(app.latest_client_throughput().is_some());
        assert!(app.latest_backend_throughput(0).is_some());

        let client_throughput = app.latest_client_throughput().unwrap();
        assert!(client_throughput.received_per_sec().get() > 0.0);
    }

    #[test]
    fn test_history_buffer_circular() {
        let metrics = MetricsCollector::new(1);
        let router = Arc::new(BackendSelector::new());
        let servers = create_test_servers(1);
        let mut app = TuiAppBuilder::new(metrics.clone(), router, servers)
            .with_history_size(HistorySize::new(5)) // Small history for testing
            .build();

        // First update (baseline)
        app.update();

        // Add more updates than history capacity
        for i in 0..10 {
            std::thread::sleep(Duration::from_millis(10));
            metrics
                .record_backend_to_client_bytes_for(crate::types::BackendId::from_index(0), 1000);
            app.update();

            // History should cap at 5
            let len = app.client_throughput_history().len();
            assert!(
                len <= 5,
                "History at iteration {i} should be <= 5, got {len}"
            );
        }

        // Final check: history should be exactly at capacity
        assert_eq!(app.client_throughput_history().len(), 5);
    }

    #[test]
    fn test_per_backend_throughput_independence() {
        let metrics = MetricsCollector::new(3);
        let router = Arc::new(BackendSelector::new());
        let servers = create_test_servers(3);
        let mut app = TuiApp::new(metrics.clone(), router, servers);

        // Baseline
        app.update();

        std::thread::sleep(Duration::from_millis(100));

        // Different traffic per backend
        metrics
            .record_backend_to_client_bytes_for(crate::types::BackendId::from_index(0), 1_000_000);
        metrics
            .record_backend_to_client_bytes_for(crate::types::BackendId::from_index(1), 2_000_000);
        metrics
            .record_backend_to_client_bytes_for(crate::types::BackendId::from_index(2), 3_000_000);

        app.update();

        // Each backend should have different throughput
        let t0 = app
            .latest_backend_throughput(0)
            .unwrap()
            .received_per_sec()
            .get();
        let t1 = app
            .latest_backend_throughput(1)
            .unwrap()
            .received_per_sec()
            .get();
        let t2 = app
            .latest_backend_throughput(2)
            .unwrap()
            .received_per_sec()
            .get();

        assert!(t0 < t1, "Backend 0 should have less than backend 1");
        assert!(t1 < t2, "Backend 1 should have less than backend 2");
    }

    #[test]
    fn test_calculate_rate() {
        // Zero time should give zero rate
        let rate = TuiApp::calculate_rate(1000, 0.0);
        assert_f64_eq(rate.get(), 0.0);

        // 1000 bytes in 1 second = 1000 B/s
        let rate = TuiApp::calculate_rate(1000, 1.0);
        assert_f64_eq(rate.get(), 1000.0);

        // 1000 bytes in 0.5 seconds = 2000 B/s
        let rate = TuiApp::calculate_rate(1000, 0.5);
        assert_f64_eq(rate.get(), 2000.0);
    }

    #[test]
    fn test_calculate_command_rate() {
        // Zero time should give zero rate
        let rate = TuiApp::calculate_command_rate(100, 0.0);
        assert_f64_eq(rate.get(), 0.0);

        // 100 commands in 1 second = 100 cmd/s
        let rate = TuiApp::calculate_command_rate(100, 1.0);
        assert_f64_eq(rate.get(), 100.0);

        // 50 commands in 0.5 seconds = 100 cmd/s
        let rate = TuiApp::calculate_command_rate(50, 0.5);
        assert_f64_eq(rate.get(), 100.0);
    }

    #[test]
    fn test_build_backend_throughput_point() {
        use crate::metrics::{BackendStats, CommandCount};
        use crate::types::{BytesReceived, BytesSent};

        let prev = BackendStats {
            bytes_sent: BytesSent::new(100),
            bytes_received: BytesReceived::new(200),
            total_commands: CommandCount::new(10),
            ..Default::default()
        };

        let next = BackendStats {
            bytes_sent: BytesSent::new(250),
            bytes_received: BytesReceived::new(500),
            total_commands: CommandCount::new(40),
            ..prev.clone()
        };

        let point = TuiApp::build_backend_throughput_point(Timestamp::now(), 0.5, &next, &prev);

        assert_f64_eq(point.sent_per_sec().get(), 300.0);
        assert_f64_eq(point.received_per_sec().get(), 600.0);
        assert_f64_eq(point.commands_per_sec().unwrap().get(), 60.0);
    }

    #[test]
    fn test_with_log_buffer() {
        use crate::tui::log_capture::LogBuffer;

        let log_buffer = LogBuffer::new();
        log_buffer.push("Test log 1".to_string());
        log_buffer.push("Test log 2".to_string());

        let metrics = MetricsCollector::new(1);
        let router = Arc::new(BackendSelector::new());
        let servers = create_test_servers(1);

        let app = TuiAppBuilder::new(metrics, router, servers)
            .with_log_buffer(log_buffer)
            .build();

        // Should have access to log buffer
        let logs = app.log_buffer().recent_lines(2);
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0], "Test log 1");
        assert_eq!(logs[1], "Test log 2");
    }

    #[test]
    fn test_throughput_point_accessors() {
        let point = ThroughputPoint::new_backend(
            Timestamp::now(),
            Throughput::new(1000.0),
            Throughput::new(2000.0),
            CommandsPerSecond::new(50.0),
        );

        assert_f64_eq(point.sent_per_sec().get(), 1000.0);
        assert_f64_eq(point.received_per_sec().get(), 2000.0);
        assert_f64_eq(point.commands_per_sec().unwrap().get(), 50.0);

        let client_point = ThroughputPoint::new_client(
            Timestamp::now(),
            Throughput::new(500.0),
            Throughput::new(1500.0),
        );

        assert_f64_eq(client_point.sent_per_sec().get(), 500.0);
        assert_f64_eq(client_point.received_per_sec().get(), 1500.0);
        assert!(client_point.commands_per_sec().is_none());
    }

    #[test]
    fn test_throughput_history_latest() {
        let mut history = ThroughputHistory::new(HistorySize::new(10));

        assert!(history.latest().is_none());

        let point1 = ThroughputPoint::new_client(
            Timestamp::now(),
            Throughput::new(100.0),
            Throughput::new(200.0),
        );
        history.push(point1);

        assert!(history.latest().is_some());
        assert_f64_eq(history.latest().unwrap().sent_per_sec().get(), 100.0);

        let point2 = ThroughputPoint::new_client(
            Timestamp::now(),
            Throughput::new(300.0),
            Throughput::new(400.0),
        );
        history.push(point2);

        // Latest should be point2
        assert_f64_eq(history.latest().unwrap().sent_per_sec().get(), 300.0);
    }

    // Tests for TuiAppBuilder
    #[test]
    fn test_builder_basic() {
        let metrics = MetricsCollector::new(2);
        let router = Arc::new(BackendSelector::new());
        let servers = create_test_servers(2);

        let app = TuiAppBuilder::new(metrics, router, servers).build();

        assert_eq!(app.snapshot().backend_stats.len(), 2);
        assert_eq!(app.backend_history.len(), 2);
    }

    #[test]
    fn test_builder_with_log_buffer() {
        use crate::tui::log_capture::LogBuffer;

        let log_buffer = LogBuffer::new();
        log_buffer.push("Test log".to_string());

        let metrics = MetricsCollector::new(1);
        let router = Arc::new(BackendSelector::new());
        let servers = create_test_servers(1);

        let app = TuiAppBuilder::new(metrics, router, servers)
            .with_log_buffer(log_buffer)
            .build();

        let logs = app.log_buffer().recent_lines(1);
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0], "Test log");
    }

    #[test]
    fn test_builder_chaining() {
        use crate::tui::log_capture::LogBuffer;

        let log_buffer = LogBuffer::new();
        let metrics = MetricsCollector::new(3);
        let router = Arc::new(BackendSelector::new());
        let servers = create_test_servers(3);

        let app = TuiAppBuilder::new(metrics, router, servers)
            .with_log_buffer(log_buffer)
            .with_history_size(HistorySize::new(90))
            .build();

        assert_eq!(app.backend_history.len(), 3);
    }

    #[test]
    fn test_snapshot_state_serialization_round_trip() {
        use crate::tui::log_capture::LogBuffer;

        let log_buffer = LogBuffer::new();
        log_buffer.push("Dashboard log line".to_string());

        let metrics = MetricsCollector::new(1);
        let router = Arc::new(BackendSelector::new());
        let servers = Arc::from(vec![
            Server::builder("backend.example.com", Port::try_new(119).unwrap())
                .name("Backend")
                .username("backend-user")
                .password("backend-pass")
                .build()
                .unwrap(),
        ]);

        let app = TuiAppBuilder::new(metrics, router, servers)
            .with_log_buffer(log_buffer)
            .build();

        let snapshot = app.snapshot_state();
        assert_eq!(snapshot.backend_views.len(), 1);
        assert_eq!(snapshot.top_users.len(), 0);
        assert_eq!(snapshot.client_history.len(), 0);
        assert_eq!(snapshot.log_lines, vec!["Dashboard log line".to_string()]);
        assert_eq!(snapshot.view_mode, ViewMode::Normal);
        assert!(!snapshot.show_details);

        let json = serde_json::to_string(&snapshot).expect("snapshot should serialize");
        assert!(!json.contains("backend-user"));
        assert!(!json.contains("backend-pass"));
        let decoded: DashboardState =
            serde_json::from_str(&json).expect("snapshot should deserialize");

        assert_eq!(decoded.backend_views.len(), 1);
        assert_eq!(decoded.top_users.len(), 0);
        assert_eq!(decoded.log_lines, vec!["Dashboard log line".to_string()]);
        assert_eq!(decoded.view_mode, ViewMode::Normal);
        assert!(!decoded.show_details);
        assert!(decoded.buffer_pool.is_none());
        assert_eq!(decoded.backend_views[0].server.name.as_str(), "Backend");
    }

    #[test]
    fn test_snapshot_state_with_log_limit_keeps_recent_tail() {
        use crate::tui::log_capture::LogBuffer;

        let log_buffer = LogBuffer::new();
        for i in 0..5 {
            log_buffer.push(format!("Dashboard log line {i}"));
        }

        let metrics = MetricsCollector::new(1);
        let router = Arc::new(BackendSelector::new());
        let servers = create_test_servers(1);

        let app = TuiAppBuilder::new(metrics, router, servers)
            .with_log_buffer(log_buffer)
            .build();

        let snapshot = app.snapshot_state_with_log_limit(Some(2));
        assert_eq!(
            snapshot.log_lines,
            vec![
                "Dashboard log line 3".to_string(),
                "Dashboard log line 4".to_string()
            ]
        );
    }

    #[test]
    fn test_snapshot_state_collects_history_and_buffer_pool() {
        use crate::pool::BufferPool;
        use crate::tui::log_capture::LogBuffer;
        use crate::types::BufferSize;

        let log_buffer = LogBuffer::new();
        let metrics = MetricsCollector::new(1);
        let router = Arc::new(BackendSelector::new());
        let servers = create_test_servers(1);
        let buffer_pool = BufferPool::new(BufferSize::try_new(8192).unwrap(), 4);

        let mut app = TuiAppBuilder::new(metrics, router, servers)
            .with_log_buffer(log_buffer)
            .with_buffer_pool(buffer_pool)
            .build();

        let client_point = ThroughputPoint::new_client(
            Timestamp::now(),
            Throughput::new(10.0),
            Throughput::new(20.0),
        );
        let backend_point = ThroughputPoint::new_backend(
            Timestamp::now(),
            Throughput::new(30.0),
            Throughput::new(40.0),
            CommandsPerSecond::new(5.0),
        );

        app.client_history.push(client_point.clone());
        app.backend_history[0].push(backend_point.clone());

        let snapshot = app.snapshot_state();

        assert_eq!(snapshot.client_history.len(), 1);
        assert_f64_eq(
            snapshot.client_history[0].sent_per_sec().get(),
            client_point.sent_per_sec().get(),
        );
        assert_f64_eq(
            snapshot.client_history[0].received_per_sec().get(),
            client_point.received_per_sec().get(),
        );
        assert_eq!(snapshot.backend_views.len(), 1);
        assert_eq!(snapshot.backend_views[0].history.len(), 1);
        assert_eq!(snapshot.top_users.len(), 0);
        assert_f64_eq(
            snapshot.backend_views[0].history[0].sent_per_sec().get(),
            backend_point.sent_per_sec().get(),
        );
        assert_f64_eq(
            snapshot.backend_views[0].history[0]
                .received_per_sec()
                .get(),
            backend_point.received_per_sec().get(),
        );
        assert_f64_eq(
            snapshot.backend_views[0].history[0]
                .commands_per_sec()
                .unwrap()
                .get(),
            backend_point.commands_per_sec().unwrap().get(),
        );
        assert_eq!(
            snapshot.buffer_pool.as_ref().map(|pool| pool.total),
            Some(4)
        );
    }

    #[test]
    fn test_snapshot_state_serializes_dashboard_metrics_and_top_users() {
        use crate::metrics::{CommandCount, DiskCacheStats, ErrorCount};

        let metrics = MetricsCollector::new(1);
        let router = Arc::new(BackendSelector::new());
        let servers = create_test_servers(1);
        let mut app = TuiAppBuilder::new(metrics, router, servers).build();

        app.snapshot = Arc::new(MetricsSnapshot {
            total_connections: 42,
            active_connections: 3,
            stateful_sessions: 2,
            client_to_backend_bytes: crate::types::ClientToBackendBytes::new(100),
            backend_to_client_bytes: crate::types::BackendToClientBytes::new(250),
            uptime: Duration::from_secs(61),
            user_stats: vec![crate::metrics::UserStats {
                username: "alice".to_string(),
                active_connections: 2,
                total_connections: crate::types::TotalConnections::new(9),
                bytes_sent: crate::types::BytesSent::new(500),
                bytes_received: crate::types::BytesReceived::new(700),
                bytes_sent_per_sec: crate::types::BytesPerSecondRate::new(11),
                bytes_received_per_sec: crate::types::BytesPerSecondRate::new(13),
                total_commands: CommandCount::new(17),
                errors: ErrorCount::new(1),
            }],
            cache_entries: 7,
            cache_size_bytes: 8192,
            cache_hit_rate: 12.5,
            disk_cache: Some(DiskCacheStats {
                disk_hits: 3,
                disk_hit_rate: 50.0,
                disk_capacity: 1024,
                bytes_written: 2048,
                bytes_read: 1024,
                write_ios: 4,
                read_ios: 5,
            }),
            pipeline_batches: 6,
            pipeline_commands: 12,
            pipeline_requests_queued: 8,
            pipeline_requests_completed: 7,
            ..MetricsSnapshot::default()
        });

        let snapshot = app.snapshot_state();
        let json = serde_json::to_string(&snapshot).expect("snapshot should serialize");
        let decoded: DashboardState =
            serde_json::from_str(&json).expect("snapshot should deserialize");

        assert_eq!(decoded.metrics.total_connections, 42);
        assert_eq!(decoded.metrics.active_connections, 3);
        assert_eq!(decoded.metrics.stateful_sessions, 2);
        assert_eq!(decoded.metrics.cache_entries, 7);
        assert_eq!(decoded.metrics.cache_size_bytes, 8192);
        assert_eq!(decoded.metrics.cache_hit_rate, 12.5);
        assert_eq!(decoded.metrics.pipeline_batches, 6);
        assert_eq!(decoded.metrics.pipeline_commands, 12);
        assert_eq!(decoded.top_users.len(), 1);
        assert_eq!(decoded.top_users[0].username, "alice");
        assert_eq!(decoded.top_users[0].active_connections, 2);
        assert_eq!(decoded.top_users[0].bytes_sent_per_sec.get(), 11);
        assert_eq!(decoded.top_users[0].bytes_received_per_sec.get(), 13);
    }

    #[test]
    fn test_throughput_point_preserves_raw_and_smoothed_rates_for_dashboard_serde() {
        let point = ThroughputPoint::new_backend_estimate(
            Timestamp::now(),
            Throughput::new(10_000.0),
            Throughput::new(2_500.0),
            Throughput::new(20_000.0),
            Throughput::new(5_000.0),
            CommandsPerSecond::new(100.0),
            CommandsPerSecond::new(25.0),
        );

        assert_f64_eq(point.raw_sent_per_sec().get(), 10_000.0);
        assert_f64_eq(point.sent_per_sec().get(), 2_500.0);
        assert_f64_eq(point.raw_received_per_sec().get(), 20_000.0);
        assert_f64_eq(point.received_per_sec().get(), 5_000.0);
        assert_f64_eq(point.raw_commands_per_sec().unwrap().get(), 100.0);
        assert_f64_eq(point.commands_per_sec().unwrap().get(), 25.0);

        let json = serde_json::to_string(&point).expect("point should serialize");
        let decoded: ThroughputPoint =
            serde_json::from_str(&json).expect("point should deserialize");
        assert_f64_eq(decoded.raw_sent_per_sec().get(), 10_000.0);
        assert_f64_eq(decoded.sent_per_sec().get(), 2_500.0);
    }

    #[test]
    fn test_throughput_point_deserializes_legacy_dashboard_payload() {
        let json = r#"{
            "sent_per_sec": 1234.0,
            "received_per_sec": 5678.0,
            "commands_per_sec": 9.0
        }"#;

        let decoded: ThroughputPoint =
            serde_json::from_str(json).expect("legacy point should deserialize");

        assert_f64_eq(decoded.raw_sent_per_sec().get(), 1234.0);
        assert_f64_eq(decoded.sent_per_sec().get(), 1234.0);
        assert_f64_eq(decoded.raw_received_per_sec().get(), 5678.0);
        assert_f64_eq(decoded.received_per_sec().get(), 5678.0);
        assert_f64_eq(decoded.raw_commands_per_sec().unwrap().get(), 9.0);
        assert_f64_eq(decoded.commands_per_sec().unwrap().get(), 9.0);
    }

    #[test]
    fn test_tui_update_uses_smoothed_backend_and_user_rates() {
        let metrics = MetricsCollector::new(1);
        let router = Arc::new(BackendSelector::new());
        let servers = create_test_servers(1);
        let mut app = TuiApp::new(metrics.clone(), router, servers);

        app.update();

        std::thread::sleep(Duration::from_millis(20));
        metrics.record_backend_to_client_bytes_for(crate::types::BackendId::from_index(0), 1_000);
        metrics.user_bytes_received(Some("alice"), 1_000);
        app.update();

        std::thread::sleep(Duration::from_millis(20));
        metrics.record_backend_to_client_bytes_for(crate::types::BackendId::from_index(0), 100_000);
        metrics.user_bytes_received(Some("alice"), 100_000);
        app.update();

        let backend = app.latest_backend_throughput(0).unwrap();
        assert!(
            backend.received_per_sec().get() < backend.raw_received_per_sec().get(),
            "backend display rate should use smoothed value"
        );

        let user = app
            .snapshot()
            .user_stats
            .iter()
            .find(|user| user.username == "alice")
            .expect("alice stats should exist");
        let raw_received = backend.raw_received_per_sec().get() as u64;
        assert!(
            user.bytes_received_per_sec.get() < raw_received,
            "user display rate should use smoothed value"
        );
    }
}
