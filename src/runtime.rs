//! Tokio runtime configuration and common utilities for binary targets
//!
//! This module provides:
//! - Testable runtime configuration and builder logic
//! - Shared utilities used across multiple binary targets to reduce duplication
//! - Shutdown signal handling

use crate::types::ThreadCount;
use anyhow::{Context, Result};

const TOKIO_WORKER_THREAD_STACK_SIZE: usize = 8 * 1024 * 1024;

/// Runtime configuration
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Number of worker threads
    worker_threads: usize,
    /// Whether to enable CPU pinning (Linux only)
    enable_cpu_pinning: bool,
}

impl RuntimeConfig {
    /// Create runtime config from optional thread count
    ///
    /// If `threads` is None, defaults to 1 thread.
    /// If `threads` is Some(ThreadCount(0)), uses number of CPU cores.
    /// Single-threaded runtime is used if threads == 1.
    #[must_use]
    pub fn from_args(threads: Option<ThreadCount>) -> Self {
        let worker_threads = threads.map_or(1, |t| t.get());

        Self {
            worker_threads,
            enable_cpu_pinning: true,
        }
    }

    /// Disable CPU pinning
    #[must_use]
    pub const fn without_cpu_pinning(mut self) -> Self {
        self.enable_cpu_pinning = false;
        self
    }

    /// Get number of worker threads
    #[must_use]
    pub const fn worker_threads(&self) -> usize {
        self.worker_threads
    }

    /// Check if single-threaded
    #[must_use]
    pub const fn is_single_threaded(&self) -> bool {
        self.worker_threads == 1
    }

    /// Build the tokio runtime
    ///
    /// Creates either a current-thread or multi-threaded runtime based on
    /// the configured worker thread count. Applies CPU pinning if enabled.
    ///
    /// # Errors
    /// Returns error if runtime creation fails or CPU pinning fails
    pub fn build_runtime(self) -> Result<tokio::runtime::Runtime> {
        let rt = if self.is_single_threaded() {
            tracing::info!("Starting NNTP proxy with single-threaded runtime");
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
        } else {
            let num_cpus = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
            tracing::info!(
                "Starting NNTP proxy with {} worker threads (detected {} CPUs)",
                self.worker_threads,
                num_cpus
            );
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(self.worker_threads)
                .thread_stack_size(TOKIO_WORKER_THREAD_STACK_SIZE)
                .enable_all()
                .build()?
        };

        if self.enable_cpu_pinning {
            pin_to_cpu_cores(self.worker_threads);
        }

        Ok(rt)
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self::from_args(None)
    }
}

/// Pin current process to specific CPU cores for optimal performance
///
/// This is a best-effort operation - failures are logged but not fatal.
///
/// # Arguments
/// * `num_cores` - Number of CPU cores to pin to (`0..num_cores`)
#[cfg(target_os = "linux")]
fn pin_to_cpu_cores(num_cores: usize) {
    use nix::sched::{CpuSet, sched_setaffinity};
    use nix::unistd::Pid;

    let mut cpu_set = CpuSet::new();
    for core in 0..num_cores {
        let _ = cpu_set.set(core);
    }

    match sched_setaffinity(Pid::from_raw(0), &cpu_set) {
        Ok(()) => {
            tracing::info!(
                "Successfully pinned process to {} CPU cores for optimal performance",
                num_cores
            );
        }
        Err(e) => {
            tracing::warn!(
                "Failed to set CPU affinity: {}, continuing without pinning",
                e
            );
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn pin_to_cpu_cores(_num_cores: usize) {
    tracing::info!("CPU pinning not available on this platform");
}

/// Wait for shutdown signal (Ctrl+C or SIGTERM on Unix)
///
/// This is a common utility for all binary targets. Handler installation
/// failures are logged and treated as an immediate shutdown signal.
pub async fn shutdown_signal() {
    use tracing::warn;

    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            warn!("Failed to install Ctrl+C handler: {error}");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => warn!("Failed to install SIGTERM handler: {error}"),
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

// ============================================================================
// Binary Utilities - Shared code for the unified nntp-proxy entrypoint.
// ============================================================================

/// Load configuration and log server information
///
/// Common pattern across all binary targets - load config and display backends.
///
/// # Errors
/// Returns error if configuration loading fails
pub fn load_and_log_config(
    config_path: &str,
) -> Result<(crate::config::Config, crate::config::ConfigSource)> {
    use crate::load_config_with_fallback;
    use tracing::info;

    let (config, source) = load_config_with_fallback(config_path)?;

    info!("Loaded configuration from {}", source.description());
    info!("Loaded {} backend servers:", config.servers.len());
    for server in &config.servers {
        info!("  - {} ({}:{})", server.name, server.host, server.port);
    }

    Ok((config, source))
}

/// Extract listen address from CLI args or config
///
/// Prefers CLI args over config values.
#[must_use]
pub fn resolve_listen_address(
    host_arg: Option<&str>,
    port_arg: Option<crate::types::Port>,
    config: &crate::config::Config,
) -> (String, crate::types::Port) {
    let host = host_arg.map_or_else(|| config.proxy.host.clone(), String::from);
    let port = port_arg.unwrap_or(config.proxy.port);
    (host, port)
}

/// Bind TCP listener and log startup information
///
/// # Errors
/// Returns error if binding fails
pub async fn bind_listener(
    host: &str,
    port: crate::types::Port,
    routing_mode: crate::RoutingMode,
) -> Result<tokio::net::TcpListener> {
    use tracing::info;

    let listen_addr = format!("{}:{}", host, port.get());
    let listener = tokio::net::TcpListener::bind(&listen_addr)
        .await
        .with_context(|| format!("Failed to bind NNTP proxy listener at {listen_addr}"))?;

    info!("NNTP proxy listening on {} ({})", listen_addr, routing_mode);

    Ok(listener)
}

/// Spawn background task to prewarm connection pools
///
/// Returns immediately without blocking. Logs errors but doesn't fail.
pub fn spawn_connection_prewarming(proxy: &std::sync::Arc<crate::NntpProxy>) {
    use std::sync::Arc;
    use tracing::{info, warn};

    let proxy = Arc::clone(proxy);
    tokio::spawn(async move {
        info!("Prewarming connection pools...");
        if let Err(e) = proxy.prewarm_connections().await {
            warn!("Failed to prewarm connection pools: {}", e);
            return;
        }
        info!("Connection pools ready");
    });
}

/// Spawn background task to periodically flush connection stats
///
/// Flushes every 30 seconds to ensure metrics are up-to-date.
pub fn spawn_stats_flusher(stats: &crate::metrics::ConnectionStatsAggregator) {
    let stats = stats.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            stats.flush();
        }
    });
}

/// Spawn background task to periodically log cache statistics
///
/// Only spawns if cache is enabled AND debug logging is enabled.
/// Logs every 60 seconds.
pub fn spawn_cache_stats_logger(proxy: &std::sync::Arc<crate::NntpProxy>) {
    use std::sync::Arc;
    use tracing::debug;

    // Only spawn if debug logging is enabled
    if !tracing::enabled!(tracing::Level::DEBUG) {
        return;
    }

    // Cache is always present now
    let cache = Arc::clone(proxy.cache());
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(crate::constants::duration_polyfill::from_minutes(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            let entries = cache.entry_count();
            let size_bytes = cache.weighted_size();
            let hit_rate = cache.hit_rate();
            debug!(
                "Cache stats: entries={}, size={} ({:.1}% hit rate)",
                entries,
                crate::formatting::format_bytes(size_bytes),
                hit_rate
            );
        }
    });
}

fn response_write_metrics_interval() -> Option<std::time::Duration> {
    let raw = std::env::var_os("NNTP_PROXY_RESPONSE_WRITE_METRICS_SECS")?;
    let raw = raw.to_string_lossy();
    let secs = raw.parse::<u64>().ok()?;
    (secs > 0).then(|| std::time::Duration::from_secs(secs))
}

fn client_writer_lock_metrics_interval() -> Option<std::time::Duration> {
    let raw = std::env::var_os("NNTP_PROXY_CLIENT_WRITER_LOCK_METRICS_SECS")?;
    let raw = raw.to_string_lossy();
    let secs = raw.parse::<u64>().ok()?;
    (secs > 0).then(|| std::time::Duration::from_secs(secs))
}

/// Spawn background task to periodically log `ChunkedResponse::write_all_to` activity.
///
/// Enable this only for targeted profiling sessions:
/// `NNTP_PROXY_RESPONSE_WRITE_METRICS_SECS=<seconds>`.
pub fn spawn_response_write_metrics_logger() {
    use tracing::info;

    let Some(period) = response_write_metrics_interval() else {
        return;
    };

    let mut previous = crate::pool::buffer::response_write_metrics_snapshot();

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            let current = crate::pool::buffer::response_write_metrics_snapshot();
            let responses_delta = current.responses.saturating_sub(previous.responses);
            let chunks_delta = current
                .chunks_written
                .saturating_sub(previous.chunks_written);
            let bytes_delta = current.bytes_written.saturating_sub(previous.bytes_written);
            let tiny_chunks_delta = current.tiny_chunks.saturating_sub(previous.tiny_chunks);
            let tiny_chunk_bytes_delta = current
                .tiny_chunk_bytes
                .saturating_sub(previous.tiny_chunk_bytes);
            let small_chunks_delta = current.small_chunks.saturating_sub(previous.small_chunks);
            let small_chunk_bytes_delta = current
                .small_chunk_bytes
                .saturating_sub(previous.small_chunk_bytes);
            let avg_chunks_milli = chunks_delta
                .saturating_mul(1000)
                .checked_div(responses_delta)
                .unwrap_or(0);

            info!(
                responses_delta = responses_delta,
                single_chunk_delta = current
                    .single_chunk_responses
                    .saturating_sub(previous.single_chunk_responses),
                multi_chunk_delta = current
                    .multi_chunk_responses
                    .saturating_sub(previous.multi_chunk_responses),
                chunks_delta = chunks_delta,
                bytes_delta = bytes_delta,
                tiny_chunks_delta = tiny_chunks_delta,
                tiny_chunk_bytes_delta = tiny_chunk_bytes_delta,
                small_chunks_delta = small_chunks_delta,
                small_chunk_bytes_delta = small_chunk_bytes_delta,
                avg_chunks_milli = avg_chunks_milli,
                max_chunks_seen = current.max_chunks_per_response,
                "Response write metrics"
            );

            previous = current;
        }
    });
}

/// Spawn background task to periodically log client-writer mutex contention.
///
/// Enable this only for targeted profiling sessions:
/// `NNTP_PROXY_CLIENT_WRITER_LOCK_METRICS_SECS=<seconds>`.
pub fn spawn_client_writer_lock_metrics_logger() {
    use tracing::info;

    let Some(period) = client_writer_lock_metrics_interval() else {
        return;
    };

    let mut previous = crate::session::shared_client_writer::client_writer_lock_metrics_snapshot();

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            let current =
                crate::session::shared_client_writer::client_writer_lock_metrics_snapshot();
            let requests_delta = current.lock_requests.saturating_sub(previous.lock_requests);
            let wait_nanos_delta = current.wait_nanos.saturating_sub(previous.wait_nanos);
            let avg_wait_nanos = if current
                .contended_locks
                .saturating_sub(previous.contended_locks)
                == 0
            {
                0
            } else {
                wait_nanos_delta
                    / (current
                        .contended_locks
                        .saturating_sub(previous.contended_locks) as u64)
            };

            info!(
                requests_delta = requests_delta,
                immediate_delta = current
                    .immediate_locks
                    .saturating_sub(previous.immediate_locks),
                contended_delta = current
                    .contended_locks
                    .saturating_sub(previous.contended_locks),
                wait_nanos_delta = wait_nanos_delta,
                avg_wait_nanos = avg_wait_nanos,
                max_wait_nanos_seen = current.max_wait_nanos,
                "Client writer lock metrics"
            );

            previous = current;
        }
    });
}

#[cfg(tokio_unstable)]
#[derive(Clone, Copy, Debug)]
struct TokioRuntimeMetricsSnapshot {
    alive_tasks: usize,
    workers: usize,
    global_queue_depth: usize,
    blocking_queue_depth: usize,
    remote_schedule_count: u64,
    budget_forced_yield_count: u64,
    worker_local_schedule_count: u64,
    worker_overflow_count: u64,
    worker_steal_count: u64,
    worker_steal_operations: u64,
    worker_poll_count: u64,
    worker_park_count: u64,
    worker_noop_count: u64,
    worker_busy_total_ns: u128,
    worker_busy_min_ns: u128,
    worker_busy_max_ns: u128,
}

#[cfg(tokio_unstable)]
impl TokioRuntimeMetricsSnapshot {
    fn capture(metrics: &tokio::runtime::RuntimeMetrics) -> Self {
        let workers = metrics.num_workers();
        let mut worker_local_schedule_count = 0u64;
        let mut worker_overflow_count = 0u64;
        let mut worker_steal_count = 0u64;
        let mut worker_steal_operations = 0u64;
        let mut worker_poll_count = 0u64;
        let mut worker_park_count = 0u64;
        let mut worker_noop_count = 0u64;
        let mut worker_busy_total_ns = 0u128;
        let mut worker_busy_min_ns = u128::MAX;
        let mut worker_busy_max_ns = 0u128;

        for worker in 0..workers {
            worker_local_schedule_count += metrics.worker_local_schedule_count(worker);
            worker_overflow_count += metrics.worker_overflow_count(worker);
            worker_steal_count += metrics.worker_steal_count(worker);
            worker_steal_operations += metrics.worker_steal_operations(worker);
            worker_poll_count += metrics.worker_poll_count(worker);
            worker_park_count += metrics.worker_park_count(worker);
            worker_noop_count += metrics.worker_noop_count(worker);

            let busy_ns = metrics.worker_total_busy_duration(worker).as_nanos();
            worker_busy_total_ns += busy_ns;
            worker_busy_min_ns = worker_busy_min_ns.min(busy_ns);
            worker_busy_max_ns = worker_busy_max_ns.max(busy_ns);
        }

        Self {
            alive_tasks: metrics.num_alive_tasks(),
            workers,
            global_queue_depth: metrics.global_queue_depth(),
            blocking_queue_depth: metrics.blocking_queue_depth(),
            remote_schedule_count: metrics.remote_schedule_count(),
            budget_forced_yield_count: metrics.budget_forced_yield_count(),
            worker_local_schedule_count,
            worker_overflow_count,
            worker_steal_count,
            worker_steal_operations,
            worker_poll_count,
            worker_park_count,
            worker_noop_count,
            worker_busy_total_ns,
            worker_busy_min_ns: if workers == 0 { 0 } else { worker_busy_min_ns },
            worker_busy_max_ns,
        }
    }
}

#[cfg(tokio_unstable)]
fn tokio_runtime_metrics_interval() -> Option<std::time::Duration> {
    let raw = std::env::var_os("NNTP_PROXY_TOKIO_RUNTIME_METRICS_SECS")?;
    let raw = raw.to_string_lossy();
    let secs = raw.parse::<u64>().ok()?;
    (secs > 0).then(|| std::time::Duration::from_secs(secs))
}

/// Spawn background task to periodically log Tokio runtime scheduler metrics.
///
/// Enable this only for targeted profiling sessions:
/// `NNTP_PROXY_TOKIO_RUNTIME_METRICS_SECS=<seconds>`.
#[cfg(tokio_unstable)]
pub fn spawn_tokio_runtime_metrics_logger() {
    use tracing::info;

    let Some(period) = tokio_runtime_metrics_interval() else {
        return;
    };

    let handle = tokio::runtime::Handle::current();
    let mut previous = TokioRuntimeMetricsSnapshot::capture(&handle.metrics());

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            let current = TokioRuntimeMetricsSnapshot::capture(&handle.metrics());

            let busy_total_delta_ns = current
                .worker_busy_total_ns
                .saturating_sub(previous.worker_busy_total_ns);
            let busy_min_delta_ns = current
                .worker_busy_min_ns
                .saturating_sub(previous.worker_busy_min_ns);
            let busy_max_delta_ns = current
                .worker_busy_max_ns
                .saturating_sub(previous.worker_busy_max_ns);

            info!(
                workers = current.workers,
                alive_tasks = current.alive_tasks,
                global_queue_depth = current.global_queue_depth,
                blocking_queue_depth = current.blocking_queue_depth,
                remote_schedule_delta = current
                    .remote_schedule_count
                    .saturating_sub(previous.remote_schedule_count),
                budget_forced_yield_delta = current
                    .budget_forced_yield_count
                    .saturating_sub(previous.budget_forced_yield_count),
                worker_local_schedule_delta = current
                    .worker_local_schedule_count
                    .saturating_sub(previous.worker_local_schedule_count),
                worker_overflow_delta = current
                    .worker_overflow_count
                    .saturating_sub(previous.worker_overflow_count),
                worker_steal_delta = current
                    .worker_steal_count
                    .saturating_sub(previous.worker_steal_count),
                worker_steal_ops_delta = current
                    .worker_steal_operations
                    .saturating_sub(previous.worker_steal_operations),
                worker_poll_delta = current
                    .worker_poll_count
                    .saturating_sub(previous.worker_poll_count),
                worker_park_delta = current
                    .worker_park_count
                    .saturating_sub(previous.worker_park_count),
                worker_noop_delta = current
                    .worker_noop_count
                    .saturating_sub(previous.worker_noop_count),
                worker_busy_total_ms_delta = busy_total_delta_ns / 1_000_000,
                worker_busy_min_ms_delta = busy_min_delta_ns / 1_000_000,
                worker_busy_max_ms_delta = busy_max_delta_ns / 1_000_000,
                "Tokio runtime metrics"
            );

            previous = current;
        }
    });
}

#[cfg(not(tokio_unstable))]
pub fn spawn_tokio_runtime_metrics_logger() {
    if std::env::var_os("NNTP_PROXY_TOKIO_RUNTIME_METRICS_SECS").is_some() {
        tracing::warn!("NNTP_PROXY_TOKIO_RUNTIME_METRICS_SECS requires a tokio_unstable build");
    }
}

/// Persist runtime state that should survive process restarts.
///
/// Saves metrics unconditionally and availability state when the proxy uses
/// the dedicated availability index.
///
/// # Errors
/// Returns an error if metrics or availability persistence fails, but still
/// attempts both writes so one failure does not suppress the other.
pub async fn persist_runtime_state(
    proxy: &std::sync::Arc<crate::NntpProxy>,
    stats_path: std::path::PathBuf,
    availability_path: Option<std::path::PathBuf>,
    server_names: Vec<String>,
) -> Result<()> {
    let mut first_error = None;

    let metrics = proxy.metrics().clone();
    let metrics_path = stats_path.clone();
    handle_shutdown_metrics_save_result(
        &mut first_error,
        &stats_path,
        tokio::task::spawn_blocking(move || metrics.save_to_disk(&metrics_path, &server_names))
            .await,
    );

    if let Some(path) = availability_path {
        let cache = proxy.cache().clone();
        let save_path = path.clone();
        handle_shutdown_availability_save_result(
            &mut first_error,
            &path,
            tokio::task::spawn_blocking(move || cache.save_to_disk(&save_path)).await,
        );
    }

    if let Some(error) = first_error {
        Err(error)
    } else {
        Ok(())
    }
}

fn remember_first_persistence_error(slot: &mut Option<anyhow::Error>, error: anyhow::Error) {
    if slot.is_none() {
        *slot = Some(error);
    }
}

fn handle_shutdown_metrics_save_result(
    first_error: &mut Option<anyhow::Error>,
    stats_path: &std::path::Path,
    result: Result<Result<()>, tokio::task::JoinError>,
) {
    use tracing::{info, warn};

    match result {
        Ok(Ok(())) => info!("Metrics saved to {}", stats_path.display()),
        Ok(Err(e)) => {
            warn!("Failed to save metrics on shutdown: {}", e);
            remember_first_persistence_error(first_error, e);
        }
        Err(e) => {
            warn!("Failed to join metrics save task on shutdown: {}", e);
            remember_first_persistence_error(first_error, e.into());
        }
    }
}

fn handle_shutdown_availability_save_result(
    first_error: &mut Option<anyhow::Error>,
    availability_path: &std::path::Path,
    result: Result<Result<bool>, tokio::task::JoinError>,
) {
    use tracing::{info, warn};

    match result {
        Ok(Ok(true)) => info!(
            "Availability index saved to {}",
            availability_path.display()
        ),
        Ok(Ok(false)) => {}
        Ok(Err(e)) => {
            warn!("Failed to save availability index on shutdown: {}", e);
            remember_first_persistence_error(first_error, e);
        }
        Err(e) => {
            warn!("Failed to join availability save task on shutdown: {}", e);
            remember_first_persistence_error(first_error, e.into());
        }
    }
}

fn handle_periodic_metrics_save_result(
    stats_path: &std::path::Path,
    result: Result<Result<()>, tokio::task::JoinError>,
) {
    use tracing::warn;

    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!("Failed to save metrics to {}: {}", stats_path.display(), e),
        Err(e) => warn!("Failed to save metrics to {}: {}", stats_path.display(), e),
    }
}

fn handle_periodic_availability_save_result(
    availability_path: &std::path::Path,
    result: Result<Result<bool>, tokio::task::JoinError>,
) {
    use tracing::warn;

    match result {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => warn!(
            "Failed to save availability index to {}: {}",
            availability_path.display(),
            e
        ),
        Err(e) => warn!(
            "Failed to save availability index to {}: {}",
            availability_path.display(),
            e
        ),
    }
}

/// Spawn graceful shutdown handler
///
/// Waits for shutdown signal, then:
/// 1. Sends shutdown notification via channel
/// 2. Calls `graceful_shutdown()` on proxy
///
/// Returns the shutdown receiver channel.
#[must_use]
pub fn spawn_shutdown_handler(
    proxy: &std::sync::Arc<crate::NntpProxy>,
    stats_path: std::path::PathBuf,
    availability_path: Option<std::path::PathBuf>,
    server_names: Vec<String>,
) -> tokio::sync::mpsc::Receiver<()> {
    use std::sync::Arc;
    use tracing::{info, warn};

    let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);
    let proxy = Arc::clone(proxy);

    tokio::spawn(async move {
        shutdown_signal().await;
        info!("Shutdown signal received");

        if let Err(e) =
            persist_runtime_state(&proxy, stats_path, availability_path, server_names).await
        {
            warn!("Failed to persist runtime state on shutdown: {}", e);
        }

        // Notify listeners
        let _ = shutdown_tx.send(()).await;

        // Close idle connections
        proxy.graceful_shutdown().await;
        info!("Graceful shutdown complete");
    });

    shutdown_rx
}

/// Run the main accept loop for client connections
///
/// Accepts connections and spawns a task for each based on routing mode.
/// Exits when shutdown signal is received.
///
/// # Errors
/// Returns error if `listener.accept()` fails
pub async fn run_accept_loop(
    proxy: std::sync::Arc<crate::NntpProxy>,
    listener: tokio::net::TcpListener,
    mut shutdown_rx: tokio::sync::mpsc::Receiver<()>,
    routing_mode: crate::RoutingMode,
) -> Result<()> {
    use tracing::{error, info};

    let uses_per_command = routing_mode.supports_per_command_routing();
    let mut client_tasks = tokio::task::JoinSet::new();

    loop {
        tokio::select! {
            biased;

            _ = shutdown_rx.recv() => {
                info!("Shutdown initiated, stopping accept loop");
                break;
            }

            Some(join_result) = client_tasks.join_next(), if !client_tasks.is_empty() => {
                if let Err(error) = join_result
                    && !error.is_cancelled()
                {
                    error!("Client session task failed: {:?}", error);
                }
            }

            accept_result = listener.accept() => {
                let (stream, addr) = accept_result?;
                let proxy = proxy.clone();

                let client_task = async move {
                    let result = if uses_per_command {
                        proxy.handle_client_per_command_routing(stream, addr.into()).await
                    } else {
                        proxy.handle_client(stream, addr.into()).await
                    };

                    match result {
                        // Normal completion or client disconnect — no action needed
                        Ok(()) | Err(crate::session::SessionError::ClientDisconnect(_)) => {}
                        Err(crate::session::SessionError::Backend(e)) => {
                            error!("Error handling client {}: {:?}", addr, e);
                        }
                    }
                };

                client_tasks.spawn(client_task);
            }
        }
    }

    client_tasks.abort_all();
    while let Some(join_result) = client_tasks.join_next().await {
        if let Err(error) = join_result
            && !error.is_cancelled()
        {
            error!("Client session task failed during shutdown: {:?}", error);
        }
    }

    proxy.graceful_shutdown().await;
    info!("Proxy shutdown complete");

    Ok(())
}

// ============================================================================
// Metrics Persistence Utilities
// ============================================================================

/// Resolve the metrics stats file path
///
/// If `stats_file` is configured, use it; otherwise default to "stats.json" alongside config file.
///
/// # Arguments
/// * `config_path` - Path to the configuration file
/// * `configured_path` - Optional configured path from config file
#[must_use]
pub fn resolve_stats_file_path(
    config_path: &str,
    configured_path: Option<&std::path::PathBuf>,
) -> std::path::PathBuf {
    use std::path::Path;

    configured_path.map_or_else(
        || {
            // Default to stats.json alongside config file
            let config_dir = Path::new(config_path)
                .parent()
                .unwrap_or_else(|| Path::new("."));
            config_dir.join("stats.json")
        },
        std::clone::Clone::clone,
    )
}

/// Resolve the persistence path for the availability index when body storage is disabled.
///
/// Returns None unless `store_article_bodies = false`. If `availability_index_path` is
/// configured, use it; otherwise default to "availability.idx" alongside config.
#[must_use]
pub fn resolve_availability_file_path(
    config_path: &str,
    cache_config: Option<&crate::config::Cache>,
) -> Option<std::path::PathBuf> {
    use std::path::Path;

    let cache_config = cache_config?;
    if cache_config.store_article_bodies {
        return None;
    }

    Some(
        cache_config
            .availability_index_path
            .clone()
            .unwrap_or_else(|| {
                let config_dir = Path::new(config_path)
                    .parent()
                    .unwrap_or_else(|| Path::new("."));
                config_dir.join("availability.idx")
            }),
    )
}

/// Load metrics from disk if the stats file exists
///
/// Returns None if file doesn't exist, is empty, or can't be parsed.
/// Errors are logged but not fatal.
///
/// # Arguments
/// * `stats_path` - Path to the stats JSON file
/// * `server_names` - Current backend server names (for matching restored data)
pub fn load_metrics_from_disk(
    stats_path: &std::path::Path,
    server_names: &[String],
) -> Option<crate::metrics::MetricsStore> {
    use tracing::{info, warn};

    match crate::metrics::MetricsStore::load(stats_path, server_names) {
        Ok(Some(store)) => {
            info!("Restored metrics from {}", stats_path.display());
            Some(store)
        }
        Ok(None) => {
            info!(
                "Stats file {} is empty or doesn't exist, starting fresh",
                stats_path.display()
            );
            None
        }
        Err(e) => {
            warn!(
                "Failed to load stats from {}: {}, starting fresh",
                stats_path.display(),
                e
            );
            None
        }
    }
}

/// Load availability state from disk if the file exists.
///
/// Returns true when an availability-only cache was restored successfully.
pub fn load_availability_from_disk(
    cache: &std::sync::Arc<crate::cache::UnifiedCache>,
    availability_path: &std::path::Path,
) -> bool {
    use tracing::{info, warn};

    match cache.load_from_disk(availability_path) {
        Ok(true) => {
            info!(
                "Restored availability index from {}",
                availability_path.display()
            );
            true
        }
        Ok(false) => {
            info!(
                "Availability file {} is missing or not needed, starting fresh",
                availability_path.display()
            );
            false
        }
        Err(e) => {
            warn!(
                "Failed to load availability index from {}: {}, starting fresh",
                availability_path.display(),
                e
            );
            false
        }
    }
}

/// Spawn background task to periodically save metrics to disk
///
/// Saves every 30 seconds. Logs errors but doesn't fail.
pub fn spawn_metrics_saver(
    proxy: &std::sync::Arc<crate::NntpProxy>,
    stats_path: std::path::PathBuf,
    server_names: Vec<String>,
) {
    use std::sync::Arc;

    let proxy = Arc::clone(proxy);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let path = stats_path.clone();
            let names = server_names.clone();
            let metrics = proxy.metrics().clone();
            let save_path = path.clone();
            handle_periodic_metrics_save_result(
                &stats_path,
                tokio::task::spawn_blocking(move || metrics.save_to_disk(&save_path, &names)).await,
            );
        }
    });
}

/// Spawn the read-only Prometheus `/metrics` exporter.
///
/// Binds `listen` and answers `GET /metrics` with the current metrics snapshot
/// in Prometheus text format. Intended for a local scraper (e.g. vmagent) on a
/// loopback address; gated by the `[metrics]` config section (the caller only
/// invokes this when enabled). Bind failures are logged and the task exits — the
/// proxy keeps running without the exporter.
pub fn spawn_metrics_exporter(
    proxy: &std::sync::Arc<crate::NntpProxy>,
    listen: std::net::SocketAddr,
    server_names: Vec<String>,
) {
    use std::sync::Arc;

    let proxy = Arc::clone(proxy);
    tokio::spawn(async move {
        let listener = match tokio::net::TcpListener::bind(listen).await {
            Ok(listener) => listener,
            Err(e) => {
                tracing::warn!("Metrics exporter failed to bind {listen}: {e}");
                return;
            }
        };
        tracing::info!("Metrics exporter listening on http://{listen}/metrics");
        loop {
            let socket = match listener.accept().await {
                Ok((socket, _peer)) => socket,
                Err(e) => {
                    tracing::warn!("Metrics exporter accept error: {e}");
                    continue;
                }
            };
            let proxy = Arc::clone(&proxy);
            let names = server_names.clone();
            tokio::spawn(async move {
                let mut socket = socket;
                let result = crate::metrics::exporter::serve_connection(&mut socket, || {
                    let snapshot = proxy
                        .metrics()
                        .snapshot(Some(proxy.cache()))
                        .with_pool_status(proxy.router());
                    crate::metrics::exporter::render_prometheus(&snapshot, &names)
                })
                .await;
                if let Err(e) = result {
                    tracing::debug!("Metrics exporter connection error: {e}");
                }
            });
        }
    });
}

/// Spawn background task to periodically save availability state to disk.
///
/// Saves every 30 seconds when the proxy uses the dedicated availability index.
pub fn spawn_availability_saver(
    proxy: &std::sync::Arc<crate::NntpProxy>,
    availability_path: Option<std::path::PathBuf>,
) {
    use std::sync::Arc;

    let Some(availability_path) = availability_path else {
        return;
    };

    let proxy = Arc::clone(proxy);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let cache = proxy.cache().clone();
            let path = availability_path.clone();
            let save_path = path.clone();
            handle_periodic_availability_save_result(
                &path,
                tokio::task::spawn_blocking(move || cache.save_to_disk(&save_path)).await,
            );
        }
    });
}

/// Spawn background task that periodically checks for and clears idle backend connections.
///
/// Runs every 60 seconds and delegates to [`NntpProxy::check_and_clear_stale_pools`],
/// which checks each backend independently against its configured `backend_idle_timeout`.
///
/// This prevents zombie connections from accumulating during extended idle periods,
/// which was causing connection limit cascades in the observed 6-day failure.
pub fn spawn_idle_connection_clearer(proxy: &std::sync::Arc<crate::NntpProxy>) {
    use std::sync::Arc;
    use std::time::Duration;

    /// How often to check for idle backends
    const CHECK_INTERVAL: Duration = crate::constants::duration_polyfill::from_minutes(1);

    let proxy = Arc::clone(proxy);

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(CHECK_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            proxy.check_and_clear_stale_pools();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Port;

    #[test]
    fn test_runtime_config_from_args_default() {
        let config = RuntimeConfig::from_args(None);

        // Should default to single-threaded (1 thread)
        assert_eq!(config.worker_threads(), 1);
        assert!(config.is_single_threaded());
        assert!(config.enable_cpu_pinning); // CPU pinning helps even with 1 thread
    }

    #[test]
    fn test_runtime_config_from_args_explicit() {
        let thread_count = ThreadCount::new(4).unwrap();
        let config = RuntimeConfig::from_args(Some(thread_count));

        assert_eq!(config.worker_threads(), 4);
        assert!(!config.is_single_threaded());
    }

    #[test]
    fn test_runtime_config_single_threaded() {
        let thread_count = ThreadCount::new(1).unwrap();
        let config = RuntimeConfig::from_args(Some(thread_count));

        assert_eq!(config.worker_threads(), 1);
        assert!(config.is_single_threaded());
    }

    #[test]
    fn test_runtime_config_multi_threaded() {
        let thread_count = ThreadCount::new(8).unwrap();
        let config = RuntimeConfig::from_args(Some(thread_count));

        assert_eq!(config.worker_threads(), 8);
        assert!(config.enable_cpu_pinning);
    }

    #[test]
    fn test_runtime_config_without_cpu_pinning() {
        let thread_count = ThreadCount::new(4).unwrap();
        let config = RuntimeConfig::from_args(Some(thread_count)).without_cpu_pinning();

        assert_eq!(config.worker_threads(), 4);
        assert!(!config.enable_cpu_pinning);
    }

    #[test]
    fn test_runtime_config_default() {
        let config = RuntimeConfig::default();

        // Should match from_args(None)
        let expected = RuntimeConfig::from_args(None);
        assert_eq!(config.worker_threads(), expected.worker_threads());
    }

    #[test]
    fn test_pin_to_cpu_cores_non_fatal() {
        // Should not panic even if pinning fails
        pin_to_cpu_cores(1);
    }

    // Edge case tests

    #[test]
    fn test_runtime_config_zero_threads_auto() {
        // ThreadCount(0) means auto-detect CPUs
        // We can't test ThreadCount::new(0) directly as it returns None,
        // but we can test the from_args behavior
        let config = RuntimeConfig::from_args(None);
        assert_eq!(config.worker_threads(), 1); // Defaults to 1
    }

    #[test]
    fn test_runtime_config_large_thread_count() {
        let thread_count = ThreadCount::new(128).unwrap();
        let config = RuntimeConfig::from_args(Some(thread_count));

        assert_eq!(config.worker_threads(), 128);
        assert!(!config.is_single_threaded());
    }

    #[test]
    fn test_runtime_config_clone() {
        let config = RuntimeConfig::from_args(Some(ThreadCount::new(4).unwrap()));
        let cloned = config.clone();

        assert_eq!(cloned.worker_threads(), config.worker_threads());
        assert_eq!(cloned.enable_cpu_pinning, config.enable_cpu_pinning);
    }

    #[test]
    fn test_runtime_config_debug() {
        let config = RuntimeConfig::from_args(Some(ThreadCount::new(2).unwrap()));
        let debug_str = format!("{config:?}");

        assert!(debug_str.contains("RuntimeConfig"));
        assert!(debug_str.contains("worker_threads"));
        assert!(debug_str.contains("enable_cpu_pinning"));
    }

    #[tokio::test]
    async fn test_bind_listener_context_mentions_proxy_listener() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let err = bind_listener(
            "127.0.0.1",
            Port::try_new(addr.port()).unwrap(),
            crate::RoutingMode::PerCommand,
        )
        .await
        .unwrap_err();

        let err = format!("{err:#}");
        assert!(err.contains("Failed to bind NNTP proxy listener"));
    }

    #[tokio::test]
    async fn test_accept_loop_aborts_active_clients_on_shutdown() {
        use std::sync::Arc;
        use tokio::io::AsyncReadExt;

        let proxy = Arc::new(
            crate::NntpProxy::new_sync(
                crate::create_default_config(),
                crate::RoutingMode::PerCommand,
            )
            .unwrap(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel(1);

        let accept_loop = tokio::spawn(run_accept_loop(
            proxy,
            listener,
            shutdown_rx,
            crate::RoutingMode::PerCommand,
        ));

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut greeting = Vec::new();
        let mut byte = [0; 1];
        while !greeting.ends_with(b"\n") {
            client.read_exact(&mut byte).await.unwrap();
            greeting.push(byte[0]);
        }
        assert!(greeting.starts_with(b"200") || greeting.starts_with(b"201"));

        shutdown_tx.send(()).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), accept_loop)
            .await
            .expect("accept loop should stop promptly after shutdown")
            .unwrap()
            .unwrap();

        let read_result =
            tokio::time::timeout(std::time::Duration::from_secs(3), client.read(&mut byte))
                .await
                .expect("client socket should close when active session is aborted");
        assert!(matches!(read_result, Ok(0) | Err(_)));
    }

    #[tokio::test]
    async fn test_accept_loop_aborts_all_active_clients_and_closes_listener() {
        use std::sync::Arc;
        use tokio::io::AsyncReadExt;

        let proxy = Arc::new(
            crate::NntpProxy::new_sync(
                crate::create_default_config(),
                crate::RoutingMode::PerCommand,
            )
            .unwrap(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel(1);

        let accept_loop = tokio::spawn(run_accept_loop(
            proxy,
            listener,
            shutdown_rx,
            crate::RoutingMode::PerCommand,
        ));

        let mut clients = Vec::new();
        for _ in 0..3 {
            let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
            let mut greeting = Vec::new();
            let mut byte = [0; 1];
            while !greeting.ends_with(b"\n") {
                client.read_exact(&mut byte).await.unwrap();
                greeting.push(byte[0]);
            }
            assert!(greeting.starts_with(b"200") || greeting.starts_with(b"201"));
            clients.push(client);
        }

        shutdown_tx.send(()).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), accept_loop)
            .await
            .expect("accept loop should stop promptly after shutdown")
            .unwrap()
            .unwrap();

        assert!(
            tokio::net::TcpStream::connect(addr).await.is_err(),
            "listener should be closed after accept-loop shutdown"
        );

        for mut client in clients {
            let mut byte = [0; 1];
            let read_result =
                tokio::time::timeout(std::time::Duration::from_secs(3), client.read(&mut byte))
                    .await
                    .expect("client socket should close when session is aborted");
            assert!(matches!(read_result, Ok(0) | Err(_)));
        }
    }

    // Builder pattern tests

    #[test]
    fn test_runtime_config_builder_chaining() {
        let config =
            RuntimeConfig::from_args(Some(ThreadCount::new(4).unwrap())).without_cpu_pinning();

        assert_eq!(config.worker_threads(), 4);
        assert!(!config.enable_cpu_pinning);
    }

    #[test]
    fn test_runtime_config_builder_multiple_calls() {
        let config = RuntimeConfig::from_args(Some(ThreadCount::new(8).unwrap()))
            .without_cpu_pinning()
            .without_cpu_pinning(); // Idempotent

        assert!(!config.enable_cpu_pinning);
    }

    // Getter tests

    #[test]
    fn test_worker_threads_getter() {
        let config = RuntimeConfig::from_args(Some(ThreadCount::new(6).unwrap()));
        assert_eq!(config.worker_threads(), 6);
    }

    #[test]
    fn test_is_single_threaded_true() {
        let config = RuntimeConfig::from_args(Some(ThreadCount::new(1).unwrap()));
        assert!(config.is_single_threaded());
    }

    #[test]
    fn test_is_single_threaded_false() {
        let config = RuntimeConfig::from_args(Some(ThreadCount::new(2).unwrap()));
        assert!(!config.is_single_threaded());
    }

    #[test]
    fn test_is_single_threaded_false_for_large() {
        let config = RuntimeConfig::from_args(Some(ThreadCount::new(100).unwrap()));
        assert!(!config.is_single_threaded());
    }

    // CPU pinning platform-specific tests

    #[test]
    #[cfg(target_os = "linux")]
    fn test_pin_to_cpu_cores_linux_single_core() {
        pin_to_cpu_cores(1);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_pin_to_cpu_cores_linux_multi_core() {
        pin_to_cpu_cores(4);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_pin_to_cpu_cores_linux_zero_cores() {
        // Edge case: 0 cores should still succeed (no-op)
        pin_to_cpu_cores(0);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_pin_to_cpu_cores_linux_many_cores() {
        // Try to pin to more cores than available - should not panic
        pin_to_cpu_cores(1024);
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn test_pin_to_cpu_cores_non_linux() {
        // Non-Linux platforms should gracefully no-op
        pin_to_cpu_cores(4);
    }

    // Default implementation tests

    #[test]
    fn test_default_matches_from_args_none() {
        let default_config = RuntimeConfig::default();
        let explicit_config = RuntimeConfig::from_args(None);

        assert_eq!(
            default_config.worker_threads(),
            explicit_config.worker_threads()
        );
        assert_eq!(
            default_config.enable_cpu_pinning,
            explicit_config.enable_cpu_pinning
        );
    }

    #[test]
    fn test_default_is_single_threaded() {
        let config = RuntimeConfig::default();
        assert!(config.is_single_threaded());
    }

    #[test]
    fn test_default_has_cpu_pinning_enabled() {
        let config = RuntimeConfig::default();
        assert!(config.enable_cpu_pinning);
    }

    // Configuration combination tests

    #[test]
    fn test_config_single_threaded_with_pinning() {
        let config = RuntimeConfig::from_args(Some(ThreadCount::new(1).unwrap()));
        assert!(config.is_single_threaded());
        assert!(config.enable_cpu_pinning);
    }

    #[test]
    fn test_config_single_threaded_without_pinning() {
        let config =
            RuntimeConfig::from_args(Some(ThreadCount::new(1).unwrap())).without_cpu_pinning();
        assert!(config.is_single_threaded());
        assert!(!config.enable_cpu_pinning);
    }

    #[test]
    fn test_config_multi_threaded_with_pinning() {
        let config = RuntimeConfig::from_args(Some(ThreadCount::new(4).unwrap()));
        assert!(!config.is_single_threaded());
        assert!(config.enable_cpu_pinning);
    }

    #[test]
    fn test_config_multi_threaded_without_pinning() {
        let config =
            RuntimeConfig::from_args(Some(ThreadCount::new(4).unwrap())).without_cpu_pinning();
        assert!(!config.is_single_threaded());
        assert!(!config.enable_cpu_pinning);
    }

    // ========================================================================
    // Metrics Persistence Tests
    // ========================================================================

    #[test]
    fn test_resolve_stats_file_path_with_configured() {
        use std::path::PathBuf;

        let configured = PathBuf::from("/custom/path/stats.json");
        let result = resolve_stats_file_path("config.toml", Some(&configured));

        assert_eq!(result, configured);
    }

    #[test]
    fn test_resolve_stats_file_path_default() {
        let result = resolve_stats_file_path("/etc/nntp-proxy/config.toml", None);

        // Should be /etc/nntp-proxy/stats.json
        assert_eq!(result.file_name().unwrap(), "stats.json");
        assert!(result.to_string_lossy().contains("nntp-proxy"));
    }

    #[test]
    fn test_resolve_stats_file_path_bare_filename() {
        let result = resolve_stats_file_path("config.toml", None);

        // Bare filename should resolve to ./stats.json
        assert_eq!(result.file_name().unwrap(), "stats.json");
    }

    #[test]
    fn test_load_metrics_from_disk_missing_file() {
        use std::path::Path;

        let nonexistent_path = Path::new("/tmp/this-does-not-exist-12345.json");
        let result = load_metrics_from_disk(nonexistent_path, &[]);

        assert!(result.is_none(), "Missing file should return None");
    }

    #[test]
    fn test_resolve_availability_file_path_none_for_full_cache() {
        let cache = crate::config::Cache {
            store_article_bodies: true,
            ..Default::default()
        };
        assert!(resolve_availability_file_path("config.toml", Some(&cache)).is_none());
    }

    #[test]
    fn test_resolve_availability_file_path_none_without_cache_config() {
        assert!(resolve_availability_file_path("config.toml", None).is_none());
    }

    #[test]
    fn test_resolve_availability_file_path_default_for_availability_only() {
        let cache = crate::config::Cache {
            store_article_bodies: false,
            ..Default::default()
        };

        let result = resolve_availability_file_path("/etc/nntp-proxy/config.toml", Some(&cache))
            .expect("availability-only mode should resolve a path");

        assert_eq!(result.file_name().unwrap(), "availability.idx");
        assert!(result.to_string_lossy().contains("nntp-proxy"));
    }

    #[test]
    fn test_resolve_availability_file_path_with_configured() {
        let cache = crate::config::Cache {
            store_article_bodies: false,
            availability_index_path: Some(std::path::PathBuf::from(
                "/custom/path/availability.idx",
            )),
            ..Default::default()
        };

        let result = resolve_availability_file_path("config.toml", Some(&cache))
            .expect("configured path should be used");

        assert_eq!(result, cache.availability_index_path.unwrap());
    }
}
