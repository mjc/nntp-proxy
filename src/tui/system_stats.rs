//! System resource monitoring for TUI display
//!
//! Tracks CPU usage, memory consumption, and thread count for the proxy process.
//! Runs sysinfo polling on a background thread to avoid blocking the TUI.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};

/// System resource statistics for the current process
#[derive(Debug, Clone, Default)]
pub struct SystemStats {
    /// CPU usage percentage (0.0 - 100.0 per core, can exceed 100.0 on multi-core)
    pub cpu_usage: f32,
    /// Peak CPU usage percentage
    pub peak_cpu_usage: f32,
    /// Memory usage in bytes
    pub memory_bytes: u64,
    /// Peak memory usage in bytes
    pub peak_memory_bytes: u64,
    /// Number of active threads
    pub thread_count: usize,
}

/// System resource monitor
///
/// Spawns a background thread that polls sysinfo every 2 seconds.
/// The main thread retrieves stats via non-blocking channel receive.
pub struct SystemMonitor {
    stats_rx: mpsc::Receiver<SystemStats>,
    peak_cpu: f32,
    peak_memory: u64,
    cached_stats: SystemStats,
}

impl SystemMonitor {
    /// Create a new system monitor for the current process
    ///
    /// Spawns a background thread that polls sysinfo every 2 seconds.
    /// Blocks until the first stats are available.
    #[must_use]
    pub fn new() -> Self {
        let (stats_tx, stats_rx) = mpsc::channel();

        // Spawn background polling thread
        thread::spawn(move || {
            let mut system = System::new_with_specifics(
                RefreshKind::new()
                    .with_processes(ProcessRefreshKind::new().with_cpu().with_memory()),
            );

            let pid = sysinfo::get_current_pid().expect("Failed to get current PID");

            loop {
                // Refresh our process stats
                system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);

                if let Some(process) = system.process(pid) {
                    let stats = SystemStats {
                        cpu_usage: process.cpu_usage(),
                        peak_cpu_usage: 0.0, // Will be calculated by receiver
                        memory_bytes: process.memory(),
                        peak_memory_bytes: 0, // Will be calculated by receiver
                        thread_count: process.tasks().map_or(1, |t| t.len()),
                    };

                    // Send stats (non-blocking, drops if receiver is slow)
                    let _ = stats_tx.send(stats);
                }

                // Sleep for 2 seconds before next poll
                thread::sleep(Duration::from_secs(2));
            }
        });

        // Wait for first stats to arrive (blocking receive)
        let initial_stats = stats_rx
            .recv()
            .expect("Failed to receive initial system stats");

        Self {
            stats_rx,
            peak_cpu: initial_stats.cpu_usage,
            peak_memory: initial_stats.memory_bytes,
            cached_stats: initial_stats,
        }
    }

    /// Update and retrieve current system stats
    ///
    /// Non-blocking receive from background thread. Returns cached stats if no new data.
    #[must_use]
    pub fn update(&mut self) -> SystemStats {
        // Try to receive latest stats (non-blocking)
        if let Ok(mut stats) = self.stats_rx.try_recv() {
            // Update peaks
            if stats.cpu_usage > self.peak_cpu {
                self.peak_cpu = stats.cpu_usage;
            }
            if stats.memory_bytes > self.peak_memory {
                self.peak_memory = stats.memory_bytes;
            }

            stats.peak_cpu_usage = self.peak_cpu;
            stats.peak_memory_bytes = self.peak_memory;

            self.cached_stats = stats;
        }

        self.cached_stats.clone()
    }

    /// Get current stats without refreshing
    ///
    /// Returns the last cached stats. Useful if you just called `update()`.
    #[must_use]
    pub fn current(&self) -> SystemStats {
        self.cached_stats.clone()
    }
}

impl Default for SystemMonitor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_monitor_creation() {
        let monitor = SystemMonitor::new();
        let stats = monitor.current();

        // Just verify we can create and get stats
        // Memory and thread count can vary by platform
        let _ = stats.memory_bytes;
        // Thread count should be >= 1
        assert!(stats.thread_count >= 1);
        // CPU might be 0 on first sample
        assert!(stats.cpu_usage >= 0.0);
    }

    #[test]
    fn test_system_monitor_update() {
        let mut monitor = SystemMonitor::new();

        // First update
        let stats1 = monitor.update();
        let _ = stats1.memory_bytes; // Just access it

        // Second update - should succeed (memory can fluctuate)
        let stats2 = monitor.update();
        let _ = stats2.memory_bytes; // Just access it

        // CPU and thread count should be reasonable
        assert!(stats2.cpu_usage >= 0.0);
        assert!(stats2.thread_count >= 1);
    }

    #[test]
    fn test_current_without_update() {
        let monitor = SystemMonitor::new();
        let stats1 = monitor.current();
        let stats2 = monitor.current();

        // Should return same values without update
        assert_eq!(stats1.memory_bytes, stats2.memory_bytes);
        assert_eq!(stats1.cpu_usage, stats2.cpu_usage);
    }

    #[test]
    fn test_system_stats_default() {
        let stats = SystemStats::default();
        assert_eq!(stats.cpu_usage, 0.0);
        assert_eq!(stats.peak_cpu_usage, 0.0);
        assert_eq!(stats.memory_bytes, 0);
        assert_eq!(stats.peak_memory_bytes, 0);
        assert_eq!(stats.thread_count, 0);
    }
}
