use std::time::{Duration, Instant};

/// Health status of a backend connection
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    /// Backend is responding to health checks
    Healthy,
    /// Backend is not responding or failing health checks
    Unhealthy,
}

/// Health information for a single backend
#[derive(Debug, Clone)]
pub struct BackendHealth {
    /// Current health status
    pub status: HealthStatus,
    /// When the status was last updated
    pub last_check: Instant,
    /// Number of consecutive failed checks
    pub consecutive_failures: u32,
    /// Total number of successful checks
    pub total_successes: u64,
    /// Total number of failed checks
    pub total_failures: u64,
    /// When the backend was last seen as healthy
    pub last_healthy: Option<Instant>,
}

impl BackendHealth {
    /// Create a new backend health tracker
    pub fn new() -> Self {
        Self {
            status: HealthStatus::Healthy,
            last_check: Instant::now(),
            consecutive_failures: 0,
            total_successes: 0,
            total_failures: 0,
            last_healthy: Some(Instant::now()),
        }
    }

    /// Record a successful health check
    pub fn record_success(&mut self) {
        self.status = HealthStatus::Healthy;
        self.last_check = Instant::now();
        self.consecutive_failures = 0;
        self.total_successes += 1;
        self.last_healthy = Some(Instant::now());
    }

    /// Record a failed health check
    pub fn record_failure(&mut self, unhealthy_threshold: u32) {
        self.last_check = Instant::now();
        self.consecutive_failures += 1;
        self.total_failures += 1;

        if self.consecutive_failures >= unhealthy_threshold {
            self.status = HealthStatus::Unhealthy;
        }
    }

    /// Check if the backend needs a health check
    pub fn needs_check(&self, interval: Duration) -> bool {
        self.last_check.elapsed() >= interval
    }

    /// Get time since last successful health check
    pub fn time_since_healthy(&self) -> Option<Duration> {
        self.last_healthy.map(|t| t.elapsed())
    }
}

impl Default for BackendHealth {
    fn default() -> Self {
        Self::new()
    }
}

/// Aggregated health metrics for all backends
#[derive(Debug, Clone, Default)]
pub struct HealthMetrics {
    /// Total number of health checks performed
    pub total_checks: u64,
    /// Number of currently healthy backends
    pub healthy_count: usize,
    /// Number of currently unhealthy backends
    pub unhealthy_count: usize,
    /// Number of backends that recovered from unhealthy state
    pub recovery_count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn test_backend_health_initial_state() {
        let health = BackendHealth::new();
        assert_eq!(health.status, HealthStatus::Healthy);
        assert_eq!(health.consecutive_failures, 0);
        assert_eq!(health.total_successes, 0);
        assert_eq!(health.total_failures, 0);
        assert!(health.last_healthy.is_some());
    }

    #[test]
    fn test_record_success() {
        let mut health = BackendHealth::new();
        health.consecutive_failures = 2;

        health.record_success();

        assert_eq!(health.status, HealthStatus::Healthy);
        assert_eq!(health.consecutive_failures, 0);
        assert_eq!(health.total_successes, 1);
        assert_eq!(health.total_failures, 0);
    }

    #[test]
    fn test_record_failure_below_threshold() {
        let mut health = BackendHealth::new();
        let threshold = 3;

        health.record_failure(threshold);
        assert_eq!(health.status, HealthStatus::Healthy);
        assert_eq!(health.consecutive_failures, 1);

        health.record_failure(threshold);
        assert_eq!(health.status, HealthStatus::Healthy);
        assert_eq!(health.consecutive_failures, 2);
    }

    #[test]
    fn test_record_failure_reaches_threshold() {
        let mut health = BackendHealth::new();
        let threshold = 3;

        for _ in 0..threshold {
            health.record_failure(threshold);
        }

        assert_eq!(health.status, HealthStatus::Unhealthy);
        assert_eq!(health.consecutive_failures, threshold);
        assert_eq!(health.total_failures, threshold as u64);
    }

    #[test]
    fn test_recovery_from_unhealthy() {
        let mut health = BackendHealth::new();
        let threshold = 3;

        // Make it unhealthy
        for _ in 0..threshold {
            health.record_failure(threshold);
        }
        assert_eq!(health.status, HealthStatus::Unhealthy);

        // Recovery
        health.record_success();
        assert_eq!(health.status, HealthStatus::Healthy);
        assert_eq!(health.consecutive_failures, 0);
    }

    #[test]
    fn test_needs_check() {
        let mut health = BackendHealth::new();
        let interval = Duration::from_millis(50);

        assert!(!health.needs_check(interval));

        sleep(Duration::from_millis(60));
        assert!(health.needs_check(interval));

        health.record_success();
        assert!(!health.needs_check(interval));
    }

    #[test]
    fn test_time_since_healthy() {
        let mut health = BackendHealth::new();

        sleep(Duration::from_millis(10));
        let elapsed = health
            .time_since_healthy()
            .expect("Should have last_healthy time");
        assert!(elapsed >= Duration::from_millis(10));

        // Make unhealthy
        health.record_failure(1);
        assert_eq!(health.status, HealthStatus::Unhealthy);

        // Should still have last_healthy time
        assert!(health.time_since_healthy().is_some());
    }
}
