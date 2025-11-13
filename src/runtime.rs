//! Tokio runtime configuration and builder
//!
//! This module provides testable runtime configuration and builder logic,
//! extracted from the binary for better separation of concerns.

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

    /// Create runtime config with specific thread count
    #[must_use]
    pub fn new(worker_threads: usize) -> Self {
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
    fn test_runtime_config_new() {
        let config = RuntimeConfig::new(8);

        assert_eq!(config.worker_threads(), 8);
        assert!(config.enable_cpu_pinning);
    }

    #[test]
    fn test_runtime_config_without_cpu_pinning() {
        let config = RuntimeConfig::new(4).without_cpu_pinning();

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
}
