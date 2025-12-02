//! TUI application state and logic

use crate::config::Server;
use crate::metrics::{MetricsCollector, MetricsSnapshot};
use crate::router::BackendSelector;
use crate::tui::log_capture::LogBuffer;
use crate::types::tui::{BytesPerSecond, CommandsPerSecond, HistorySize, Timestamp};
use std::collections::VecDeque;
use std::sync::Arc;

/// TUI view mode - controls what is displayed
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewMode {
    /// Normal view - shows all panels
    #[default]
    Normal,
    /// Log fullscreen - shows only title and logs (mostly fullscreen)
    LogFullscreen,
}

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

/// TUI application builder
///
/// Provides a fluent API for constructing TuiApp instances with optional configuration.
/// This replaces the multiple constructor pattern (new, with_log_buffer, with_history_size)
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
    servers: Arc<Vec<Server>>,
    cache: Option<Arc<crate::cache::ArticleCache>>,
    log_buffer: Option<LogBuffer>,
    history_size: HistorySize,
}

impl TuiAppBuilder {
    /// Create a new TUI app builder
    #[must_use]
    pub fn new(
        metrics: MetricsCollector,
        router: Arc<BackendSelector>,
        servers: Arc<Vec<Server>>,
    ) -> Self {
        Self {
            metrics,
            router,
            servers,
            cache: None,
            log_buffer: None,
            history_size: HistorySize::DEFAULT,
        }
    }

    /// Set the article cache for monitoring cache statistics
    #[must_use]
    pub fn with_cache(mut self, cache: Arc<crate::cache::ArticleCache>) -> Self {
        self.cache = Some(cache);
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
    pub fn with_history_size(mut self, history_size: HistorySize) -> Self {
        self.history_size = history_size;
        self
    }

    /// Build the TuiApp
    #[must_use]
    pub fn build(self) -> TuiApp {
        use crate::tui::SystemMonitor;

        let snapshot = Arc::new(self.metrics.snapshot(self.cache.as_deref()));
        let backend_count = self.servers.len();

        // Initialize empty history for each backend
        let backend_history = (0..backend_count)
            .map(|_| ThroughputHistory::new(self.history_size))
            .collect();

        TuiApp {
            metrics: self.metrics,
            router: self.router,
            servers: self.servers,
            cache: self.cache,
            snapshot,
            backend_history,
            client_history: ThroughputHistory::new(self.history_size),
            previous_snapshot: None,
            last_update: Timestamp::now(),
            history_size: self.history_size,
            log_buffer: Arc::new(self.log_buffer.unwrap_or_default()),
            view_mode: ViewMode::default(),
            show_details: false,
            system_monitor: SystemMonitor::new(),
            system_stats: Default::default(),
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
    servers: Arc<Vec<Server>>,
    /// Current metrics snapshot (Arc for zero-cost sharing)
    snapshot: Arc<MetricsSnapshot>,
    /// Historical throughput data per backend
    backend_history: Vec<ThroughputHistory>,
    /// Historical client throughput (global)
    client_history: ThroughputHistory,
    /// Previous snapshot for calculating deltas (Arc for zero-cost sharing)
    previous_snapshot: Option<Arc<MetricsSnapshot>>,
    /// Last update time
    last_update: Timestamp,
    /// History capacity
    #[allow(dead_code)]
    history_size: HistorySize,
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
    cache: Option<Arc<crate::cache::ArticleCache>>,
}

impl TuiApp {
    /// Create a new TUI application
    ///
    /// **Note:** Prefer using `TuiAppBuilder` for more flexibility.
    /// This method is a convenience wrapper.
    #[must_use]
    pub fn new(
        metrics: MetricsCollector,
        router: Arc<BackendSelector>,
        servers: Arc<Vec<Server>>,
    ) -> Self {
        TuiAppBuilder::new(metrics, router, servers).build()
    }

    /// Calculate throughput rate from byte delta and time delta
    /// Calculate byte transfer rate
    #[inline]
    fn calculate_rate(byte_delta: u64, time_delta_secs: f64) -> BytesPerSecond {
        if time_delta_secs > 0.0 {
            BytesPerSecond::new((byte_delta as f64) / time_delta_secs)
        } else {
            BytesPerSecond::zero()
        }
    }

    /// Calculate command rate from command delta and time delta
    #[inline]
    fn calculate_command_rate(cmd_delta: u64, time_delta_secs: f64) -> CommandsPerSecond {
        if time_delta_secs > 0.0 {
            CommandsPerSecond::new((cmd_delta as f64) / time_delta_secs)
        } else {
            CommandsPerSecond::zero()
        }
    }

    /// Calculate per-user rates from deltas
    #[inline]
    fn calculate_user_rates(
        &self,
        current: &crate::metrics::UserStats,
        prev_snapshot: &crate::metrics::MetricsSnapshot,
        time_delta: f64,
    ) -> crate::metrics::UserStats {
        use crate::types::BytesPerSecondRate;

        let (bytes_sent_per_sec, bytes_received_per_sec) = prev_snapshot
            .user_stats
            .iter()
            .find(|u| u.username == current.username)
            .map(|prev| {
                let sent_delta = current.bytes_sent.saturating_sub(prev.bytes_sent);
                let recv_delta = current.bytes_received.saturating_sub(prev.bytes_received);
                (
                    BytesPerSecondRate::new(
                        Self::calculate_rate(sent_delta.into(), time_delta).get() as u64,
                    ),
                    BytesPerSecondRate::new(
                        Self::calculate_rate(recv_delta.into(), time_delta).get() as u64,
                    ),
                )
            })
            .unwrap_or((BytesPerSecondRate::ZERO, BytesPerSecondRate::ZERO));

        crate::metrics::UserStats {
            username: current.username.clone(),
            active_connections: current.active_connections,
            total_connections: current.total_connections,
            bytes_sent: current.bytes_sent,
            bytes_received: current.bytes_received,
            total_commands: current.total_commands,
            errors: current.errors,
            bytes_sent_per_sec,
            bytes_received_per_sec,
        }
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
            let client_sent_rate = Self::calculate_rate(client_to_backend_delta.into(), time_delta);
            let client_recv_rate = Self::calculate_rate(backend_to_client_delta.into(), time_delta);

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

                let sent_rate = Self::calculate_rate(sent_delta.into(), time_delta);
                let recv_rate = Self::calculate_rate(recv_delta.into(), time_delta);
                let cmd_rate = Self::calculate_command_rate(cmd_delta.get(), time_delta);

                let point = ThroughputPoint::new_backend(now, sent_rate, recv_rate, cmd_rate);
                self.backend_history[i].push(point);
            }

            // Enrich user stats with calculated rates
            let user_stats = new_snapshot
                .user_stats
                .iter()
                .map(|stats| self.calculate_user_rates(stats, prev, time_delta))
                .collect();

            // Build enriched snapshot (shares backend_stats via Arc)
            self.snapshot = Arc::new(crate::metrics::MetricsSnapshot {
                user_stats,
                backend_stats: Arc::clone(&new_snapshot.backend_stats),
                ..*new_snapshot
            });
            self.previous_snapshot = Some(new_snapshot);
        } else {
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

    /// Get latest backend throughput for a backend
    #[must_use]
    pub fn latest_backend_throughput(&self, backend_idx: usize) -> Option<&ThroughputPoint> {
        self.backend_history
            .get(backend_idx)
            .and_then(|h| h.latest())
    }

    /// Get log buffer for displaying recent log messages
    #[must_use]
    pub fn log_buffer(&self) -> &Arc<LogBuffer> {
        &self.log_buffer
    }

    /// Get current view mode
    #[must_use]
    pub const fn view_mode(&self) -> ViewMode {
        self.view_mode
    }

    /// Toggle between normal and log fullscreen view
    pub fn toggle_log_fullscreen(&mut self) {
        self.view_mode = match self.view_mode {
            ViewMode::Normal => ViewMode::LogFullscreen,
            ViewMode::LogFullscreen => ViewMode::Normal,
        };
    }

    /// Toggle detailed metrics display
    pub fn toggle_details(&mut self) {
        self.show_details = !self.show_details;
    }

    /// Get current details display state
    #[must_use]
    pub fn show_details(&self) -> bool {
        self.show_details
    }

    /// Get current system stats (CPU, memory, threads)
    #[must_use]
    pub const fn system_stats(&self) -> &crate::tui::SystemStats {
        &self.system_stats
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
    fn create_test_servers(count: usize) -> Arc<Vec<Server>> {
        Arc::new(
            (0..count)
                .map(|i| {
                    Server::builder(
                        format!("backend{}.example.com", i),
                        Port::try_new(119).unwrap(),
                    )
                    .name(format!("Backend {}", i))
                    .build()
                    .unwrap()
                })
                .collect(),
        )
    }

    /// Helper to create test TuiApp
    fn create_test_app(backend_count: usize) -> TuiApp {
        let metrics = MetricsCollector::new(backend_count);
        let router = Arc::new(BackendSelector::new());
        let servers = create_test_servers(backend_count);
        TuiApp::new(metrics, router, servers)
    }

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
            Server::builder("test.example.com", Port::try_new(119).unwrap())
                .name("Test Server".to_string())
                .build()
                .unwrap(),
        ]);

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
                "History at iteration {} should be <= 5, got {}",
                i,
                len
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
        assert_eq!(rate.get(), 0.0);

        // 1000 bytes in 1 second = 1000 B/s
        let rate = TuiApp::calculate_rate(1000, 1.0);
        assert_eq!(rate.get(), 1000.0);

        // 1000 bytes in 0.5 seconds = 2000 B/s
        let rate = TuiApp::calculate_rate(1000, 0.5);
        assert_eq!(rate.get(), 2000.0);
    }

    #[test]
    fn test_calculate_command_rate() {
        // Zero time should give zero rate
        let rate = TuiApp::calculate_command_rate(100, 0.0);
        assert_eq!(rate.get(), 0.0);

        // 100 commands in 1 second = 100 cmd/s
        let rate = TuiApp::calculate_command_rate(100, 1.0);
        assert_eq!(rate.get(), 100.0);

        // 50 commands in 0.5 seconds = 100 cmd/s
        let rate = TuiApp::calculate_command_rate(50, 0.5);
        assert_eq!(rate.get(), 100.0);
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
            .with_log_buffer(log_buffer.clone())
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
            BytesPerSecond::new(1000.0),
            BytesPerSecond::new(2000.0),
            CommandsPerSecond::new(50.0),
        );

        assert_eq!(point.sent_per_sec().get(), 1000.0);
        assert_eq!(point.received_per_sec().get(), 2000.0);
        assert_eq!(point.commands_per_sec().unwrap().get(), 50.0);

        let client_point = ThroughputPoint::new_client(
            Timestamp::now(),
            BytesPerSecond::new(500.0),
            BytesPerSecond::new(1500.0),
        );

        assert_eq!(client_point.sent_per_sec().get(), 500.0);
        assert_eq!(client_point.received_per_sec().get(), 1500.0);
        assert!(client_point.commands_per_sec().is_none());
    }

    #[test]
    fn test_throughput_history_latest() {
        let mut history = ThroughputHistory::new(HistorySize::new(10));

        assert!(history.latest().is_none());

        let point1 = ThroughputPoint::new_client(
            Timestamp::now(),
            BytesPerSecond::new(100.0),
            BytesPerSecond::new(200.0),
        );
        history.push(point1.clone());

        assert!(history.latest().is_some());
        assert_eq!(history.latest().unwrap().sent_per_sec().get(), 100.0);

        let point2 = ThroughputPoint::new_client(
            Timestamp::now(),
            BytesPerSecond::new(300.0),
            BytesPerSecond::new(400.0),
        );
        history.push(point2.clone());

        // Latest should be point2
        assert_eq!(history.latest().unwrap().sent_per_sec().get(), 300.0);
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
            .with_log_buffer(log_buffer.clone())
            .build();

        let logs = app.log_buffer().recent_lines(1);
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0], "Test log");
    }

    #[test]
    fn test_builder_with_custom_history_size() {
        let metrics = MetricsCollector::new(1);
        let router = Arc::new(BackendSelector::new());
        let servers = create_test_servers(1);

        let custom_size = HistorySize::new(120);
        let app = TuiAppBuilder::new(metrics, router, servers)
            .with_history_size(custom_size)
            .build();

        // Verify history size is set
        assert_eq!(app.history_size, custom_size);
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
        assert_eq!(app.history_size.get(), 90);
    }
}
