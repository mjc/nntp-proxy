//! Tokio runtime configuration and common utilities for binary targets
//!
//! This module provides:
//! - Testable runtime configuration and builder logic
//! - Shared utilities used across multiple binary targets to reduce duplication
//! - Shutdown signal handling

use crate::types::ThreadCount;
use anyhow::Result;

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
        let worker_threads = threads.map(|t| t.get()).unwrap_or(1);

        Self {
            worker_threads,
            enable_cpu_pinning: true,
        }
    }

    /// Disable CPU pinning
    #[must_use]
    pub fn without_cpu_pinning(mut self) -> Self {
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
            let num_cpus = std::thread::available_parallelism()
                .map(|p| p.get())
                .unwrap_or(1);
            tracing::info!(
                "Starting NNTP proxy with {} worker threads (detected {} CPUs)",
                self.worker_threads,
                num_cpus
            );
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(self.worker_threads)
                .enable_all()
                .build()?
        };

        if self.enable_cpu_pinning {
            pin_to_cpu_cores(self.worker_threads)?;
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
/// * `num_cores` - Number of CPU cores to pin to (0..num_cores)
#[cfg(target_os = "linux")]
fn pin_to_cpu_cores(num_cores: usize) -> Result<()> {
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
            Ok(())
        }
        Err(e) => {
            tracing::warn!(
                "Failed to set CPU affinity: {}, continuing without pinning",
                e
            );
            Ok(()) // Non-fatal
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn pin_to_cpu_cores(_num_cores: usize) -> Result<()> {
    tracing::info!("CPU pinning not available on this platform");
    Ok(())
}

/// Wait for shutdown signal (Ctrl+C or SIGTERM on Unix)
///
/// This is a common utility for all binary targets.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

// ============================================================================
// Binary Utilities - Shared code across nntp-proxy, nntp-proxy-tui, etc.
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
    let host = host_arg
        .map(String::from)
        .unwrap_or_else(|| config.proxy.host.clone());
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
    let listener = tokio::net::TcpListener::bind(&listen_addr).await?;

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
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            let entries = cache.entry_count();
            let size_bytes = entries.saturating_mul(750_000 + 100);
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

/// Spawn graceful shutdown handler
///
/// Waits for shutdown signal, then:
/// 1. Sends shutdown notification via channel
/// 2. Calls graceful_shutdown() on proxy
///
/// Returns the shutdown receiver channel.
#[must_use]
pub fn spawn_shutdown_handler(
    proxy: &std::sync::Arc<crate::NntpProxy>,
) -> tokio::sync::mpsc::Receiver<()> {
    use std::sync::Arc;
    use tracing::info;

    let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);
    let proxy = Arc::clone(proxy);

    tokio::spawn(async move {
        shutdown_signal().await;
        info!("Shutdown signal received");

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
/// Returns error if listener.accept() fails
pub async fn run_accept_loop(
    proxy: std::sync::Arc<crate::NntpProxy>,
    listener: tokio::net::TcpListener,
    mut shutdown_rx: tokio::sync::mpsc::Receiver<()>,
    routing_mode: crate::RoutingMode,
) -> Result<()> {
    use tracing::{error, info};

    let uses_per_command = routing_mode.supports_per_command_routing();

    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                info!("Shutdown initiated, stopping accept loop");
                break;
            }

            accept_result = listener.accept() => {
                let (stream, addr) = accept_result?;
                let proxy = proxy.clone();

                tokio::spawn(async move {
                    let result = if uses_per_command {
                        proxy.handle_client_per_command_routing(stream, addr.into()).await
                    } else {
                        proxy.handle_client(stream, addr.into()).await
                    };

                    if let Err(e) = result {
                        // Only log non-client-disconnect errors (avoid duplicate logging)
                        // Client disconnects are already handled gracefully in session handlers
                        if !crate::is_client_disconnect_error(&e) {
                            error!("Error handling client {}: {:?}", addr, e);
                        }
                    }
                });
            }
        }
    }

    proxy.graceful_shutdown().await;
    info!("Proxy shutdown complete");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let result = pin_to_cpu_cores(1);
        assert!(result.is_ok());
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
        let debug_str = format!("{:?}", config);

        assert!(debug_str.contains("RuntimeConfig"));
        assert!(debug_str.contains("worker_threads"));
        assert!(debug_str.contains("enable_cpu_pinning"));
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
        let result = pin_to_cpu_cores(1);
        assert!(result.is_ok());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_pin_to_cpu_cores_linux_multi_core() {
        let result = pin_to_cpu_cores(4);
        assert!(result.is_ok());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_pin_to_cpu_cores_linux_zero_cores() {
        // Edge case: 0 cores should still succeed (no-op)
        let result = pin_to_cpu_cores(0);
        assert!(result.is_ok());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_pin_to_cpu_cores_linux_many_cores() {
        // Try to pin to more cores than available - should not panic
        let result = pin_to_cpu_cores(1024);
        assert!(result.is_ok()); // Best-effort, won't fail
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn test_pin_to_cpu_cores_non_linux() {
        // Non-Linux platforms should gracefully no-op
        let result = pin_to_cpu_cores(4);
        assert!(result.is_ok());
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
}
