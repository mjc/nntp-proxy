mod types;

pub use types::{BackendHealth, HealthMetrics, HealthStatus};

use crate::protocol::{DATE, ResponseParser};
use crate::types::BackendId;
use dashmap::DashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time;

/// Configuration for health checking
#[derive(Debug, Clone)]
pub struct HealthCheck {
    /// Interval between health checks
    pub check_interval: Duration,
    /// Timeout for each health check
    pub check_timeout: Duration,
    /// Number of consecutive failures before marking unhealthy
    pub unhealthy_threshold: u32,
}

impl Default for HealthCheck {
    fn default() -> Self {
        Self {
            check_interval: Duration::from_secs(30),
            check_timeout: Duration::from_secs(5),
            unhealthy_threshold: 3,
        }
    }
}

/// Health checker for backend connections
pub struct HealthChecker {
    /// Health status for each backend (lock-free)
    backend_health: Arc<DashMap<BackendId, BackendHealth>>,
    /// Configuration
    config: HealthCheck,
}

impl HealthChecker {
    /// Create a new health checker
    pub fn new(config: HealthCheck) -> Self {
        Self {
            backend_health: Arc::new(DashMap::new()),
            config,
        }
    }

    /// Initialize health tracking for a backend
    pub async fn register_backend(&self, backend_id: BackendId) {
        self.backend_health.entry(backend_id).or_default();
    }

    /// Start the background health checking task
    pub fn start_health_checks(
        self: Arc<Self>,
        providers: Vec<crate::pool::DeadpoolConnectionProvider>,
    ) {
        tokio::spawn(async move {
            let mut interval = time::interval(self.config.check_interval);
            loop {
                interval.tick().await;

                // Check each backend
                for (i, provider) in providers.iter().enumerate() {
                    let backend_id = BackendId::from_index(i);
                    self.clone()
                        .check_backend(provider.clone(), backend_id)
                        .await;
                }
            }
        });
    }

    /// Perform a health check on a single backend
    async fn check_backend(
        &self,
        provider: crate::pool::DeadpoolConnectionProvider,
        backend_id: BackendId,
    ) {
        // Check if this backend needs a check
        if let Some(backend_health) = self.backend_health.get(&backend_id)
            && !backend_health.needs_check(self.config.check_interval)
        {
            return;
        }

        // Perform the health check with timeout
        let check_result = time::timeout(
            self.config.check_timeout,
            self.perform_health_check(provider.clone(), backend_id),
        )
        .await;

        // Update health status
        let mut backend_health = self.backend_health.entry(backend_id).or_default();

        match check_result {
            Ok(Ok(())) => backend_health.record_success(),
            Ok(Err(_)) | Err(_) => backend_health.record_failure(self.config.unhealthy_threshold),
        }
    }

    /// Perform the actual health check by sending DATE command
    async fn perform_health_check(
        &self,
        provider: crate::pool::DeadpoolConnectionProvider,
        _backend_id: BackendId,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Get a pooled connection
        let mut conn = provider.get_pooled_connection().await?;

        // Send DATE command
        conn.write_all(DATE).await?;

        // Read response
        let mut reader = BufReader::new(&mut *conn);
        // Pre-allocate for typical NNTP DATE response ("111 YYYYMMDDhhmmss\r\n" ~20-30 bytes)
        let mut response = Vec::with_capacity(64);
        reader.read_until(b'\n', &mut response).await?;

        // Check if response indicates success (111 response)
        if ResponseParser::is_response_code(&response, 111) {
            Ok(())
        } else {
            Err("Unexpected response from DATE command".into())
        }
    }

    /// Check if a backend is healthy
    pub async fn is_healthy(&self, backend_id: BackendId) -> bool {
        self.backend_health
            .get(&backend_id)
            .map(|h| h.status == HealthStatus::Healthy)
            .unwrap_or(true) // Assume healthy if not tracked yet
    }

    /// Get health status for a specific backend
    pub async fn get_backend_health(&self, backend_id: BackendId) -> Option<BackendHealth> {
        self.backend_health.get(&backend_id).map(|h| h.clone())
    }

    /// Get aggregated health metrics
    pub async fn get_metrics(&self) -> HealthMetrics {
        let mut metrics = HealthMetrics {
            total_checks: self
                .backend_health
                .iter()
                .map(|entry| entry.total_successes + entry.total_failures)
                .sum(),
            ..Default::default()
        };

        self.backend_health
            .iter()
            .for_each(|entry| match entry.status {
                HealthStatus::Healthy => metrics.healthy_count += 1,
                HealthStatus::Unhealthy => metrics.unhealthy_count += 1,
            });

        metrics
    }

    /// Get all healthy backend IDs
    pub async fn get_healthy_backends(&self) -> Vec<BackendId> {
        self.backend_health
            .iter()
            .filter(|entry| entry.value().status == HealthStatus::Healthy)
            .map(|entry| *entry.key())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn test_health_checker_creation() {
        let config = HealthCheck::default();
        let checker = HealthChecker::new(config);

        let metrics = checker.get_metrics().await;
        assert_eq!(metrics.healthy_count, 0);
        assert_eq!(metrics.unhealthy_count, 0);
    }

    #[tokio::test]
    async fn test_register_backend() {
        let config = HealthCheck::default();
        let checker = HealthChecker::new(config);

        let backend_id = BackendId::from_index(0);
        checker.register_backend(backend_id).await;

        let metrics = checker.get_metrics().await;
        assert_eq!(metrics.healthy_count, 1);
        assert_eq!(metrics.unhealthy_count, 0);
    }

    #[tokio::test]
    async fn test_multiple_backend_registration() {
        let config = HealthCheck::default();
        let checker = HealthChecker::new(config);

        for i in 0..3 {
            checker.register_backend(BackendId::from_index(i)).await;
        }

        let metrics = checker.get_metrics().await;
        assert_eq!(metrics.healthy_count, 3);
        assert_eq!(metrics.unhealthy_count, 0);
    }

    #[tokio::test]
    async fn test_get_healthy_backends() {
        let config = HealthCheck::default();
        let checker = HealthChecker::new(config);

        let backend_ids = vec![
            BackendId::from_index(0),
            BackendId::from_index(1),
            BackendId::from_index(2),
        ];

        for id in &backend_ids {
            checker.register_backend(*id).await;
        }

        let healthy = checker.get_healthy_backends().await;
        assert_eq!(healthy.len(), 3);
    }

    #[tokio::test]
    async fn test_health_check_config_default() {
        let config = HealthCheck::default();
        assert_eq!(config.check_interval, Duration::from_secs(30));
        assert_eq!(config.check_timeout, Duration::from_secs(5));
        assert_eq!(config.unhealthy_threshold, 3);
    }

    #[tokio::test]
    async fn test_health_check_config_custom() {
        let config = HealthCheck {
            check_interval: Duration::from_secs(10),
            check_timeout: Duration::from_secs(2),
            unhealthy_threshold: 5,
        };

        let checker = HealthChecker::new(config.clone());
        assert_eq!(checker.config.check_interval, Duration::from_secs(10));
        assert_eq!(checker.config.check_timeout, Duration::from_secs(2));
        assert_eq!(checker.config.unhealthy_threshold, 5);
    }

    #[tokio::test]
    async fn test_simulated_connection_failure() {
        let config = HealthCheck {
            check_interval: Duration::from_millis(100),
            check_timeout: Duration::from_millis(50),
            unhealthy_threshold: 2,
        };
        let checker = HealthChecker::new(config);
        let backend_id = BackendId::from_index(0);

        checker.register_backend(backend_id).await;

        // Simulate failures by manually updating health
        if let Some(mut backend_health) = checker.backend_health.get_mut(&backend_id) {
            // Record failures to reach threshold
            backend_health.record_failure(2);
            backend_health.record_failure(2);
        }

        let metrics = checker.get_metrics().await;
        assert_eq!(metrics.unhealthy_count, 1);
        assert_eq!(metrics.healthy_count, 0);
    }

    #[tokio::test]
    async fn test_recovery_after_failures() {
        let config = HealthCheck {
            check_interval: Duration::from_millis(100),
            check_timeout: Duration::from_millis(50),
            unhealthy_threshold: 2,
        };
        let checker = HealthChecker::new(config);
        let backend_id = BackendId::from_index(0);

        checker.register_backend(backend_id).await;

        // Simulate failures then success
        if let Some(mut backend_health) = checker.backend_health.get_mut(&backend_id) {
            backend_health.record_failure(2);
            backend_health.record_failure(2);
            // Now recover
            backend_health.record_success();
        }

        let metrics = checker.get_metrics().await;
        assert_eq!(metrics.healthy_count, 1);
        assert_eq!(metrics.unhealthy_count, 0);
    }

    #[tokio::test]
    async fn test_health_metrics_mixed_states() {
        let config = HealthCheck::default();
        let checker = HealthChecker::new(config);

        // Register multiple backends
        for i in 0..5 {
            checker.register_backend(BackendId::from_index(i)).await;
        }

        // Make some unhealthy
        // Make backends 1 and 3 unhealthy
        if let Some(mut backend_health) = checker.backend_health.get_mut(&BackendId::from_index(1))
        {
            backend_health.record_failure(3);
            backend_health.record_failure(3);
            backend_health.record_failure(3);
        }
        if let Some(mut backend_health) = checker.backend_health.get_mut(&BackendId::from_index(3))
        {
            backend_health.record_failure(3);
            backend_health.record_failure(3);
            backend_health.record_failure(3);
        }

        let metrics = checker.get_metrics().await;
        assert_eq!(metrics.healthy_count, 3);
        assert_eq!(metrics.unhealthy_count, 2);

        let healthy = checker.get_healthy_backends().await;
        assert_eq!(healthy.len(), 3);
        assert!(healthy.contains(&BackendId::from_index(0)));
        assert!(healthy.contains(&BackendId::from_index(2)));
        assert!(healthy.contains(&BackendId::from_index(4)));
    }

    #[tokio::test]
    async fn test_backend_health_isolation() {
        let config = HealthCheck::default();
        let checker = HealthChecker::new(config);

        let backend1 = BackendId::from_index(0);
        let backend2 = BackendId::from_index(1);

        checker.register_backend(backend1).await;
        checker.register_backend(backend2).await;

        // Fail only backend1
        if let Some(mut backend_health) = checker.backend_health.get_mut(&backend1) {
            backend_health.record_failure(3);
            backend_health.record_failure(3);
            backend_health.record_failure(3);
        }

        let healthy = checker.get_healthy_backends().await;
        assert_eq!(healthy.len(), 1);
        assert_eq!(healthy[0], backend2);
    }
}
