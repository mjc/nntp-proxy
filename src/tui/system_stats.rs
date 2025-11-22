//! System resource monitoring for TUI display
//!
//! Tracks CPU usage, memory consumption, and thread count for the proxy process.

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
/// Efficiently tracks process-specific metrics using sysinfo.
/// Call `update()` periodically (e.g., every TUI frame) to refresh stats.
pub struct SystemMonitor {
    system: System,
    pid: sysinfo::Pid,
    peak_cpu: f32,
    peak_memory: u64,
}

impl SystemMonitor {
    /// Create a new system monitor for the current process
    #[must_use]
    pub fn new() -> Self {
        let mut system = System::new_with_specifics(
            RefreshKind::nothing().with_processes(ProcessRefreshKind::everything()),
        );

        // Get current process PID
        let pid = sysinfo::get_current_pid().expect("Failed to get current PID");

        // Initial refresh
        system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);

        Self {
            system,
            pid,
            peak_cpu: 0.0,
            peak_memory: 0,
        }
    }

    /// Update and retrieve current system stats
    ///
    /// Should be called at regular intervals (e.g., TUI update rate of ~4Hz).
    /// First call may return zero for CPU usage - sysinfo needs 2+ samples.
    #[must_use]
    pub fn update(&mut self) -> SystemStats {
        // Refresh only our process (efficient)
        self.system
            .refresh_processes(ProcessesToUpdate::Some(&[self.pid]), true);

        if let Some(process) = self.system.process(self.pid) {
            let cpu = process.cpu_usage();
            let memory = process.memory();

            // Update peaks
            if cpu > self.peak_cpu {
                self.peak_cpu = cpu;
            }
            if memory > self.peak_memory {
                self.peak_memory = memory;
            }

            SystemStats {
                cpu_usage: cpu,
                peak_cpu_usage: self.peak_cpu,
                memory_bytes: memory,
                peak_memory_bytes: self.peak_memory,
                thread_count: process.tasks().map_or(1, |tasks| tasks.len()),
            }
        } else {
            // Process not found (shouldn't happen for our own PID)
            SystemStats::default()
        }
    }

    /// Get current stats without refreshing
    ///
    /// Returns the last refreshed stats. Useful if you just called `update()`.
    #[must_use]
    pub fn current(&self) -> SystemStats {
        if let Some(process) = self.system.process(self.pid) {
            SystemStats {
                cpu_usage: process.cpu_usage(),
                peak_cpu_usage: self.peak_cpu,
                memory_bytes: process.memory(),
                peak_memory_bytes: self.peak_memory,
                thread_count: process.tasks().map_or(1, |tasks| tasks.len()),
            }
        } else {
            SystemStats::default()
        }
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

        // Memory should be > 0 for a running process
        assert!(stats.memory_bytes > 0);
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
        assert!(stats1.memory_bytes > 0);

        // Second update - should succeed (memory can fluctuate)
        let stats2 = monitor.update();
        assert!(stats2.memory_bytes > 0);

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
