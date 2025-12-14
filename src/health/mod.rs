mod types;

pub use types::{BackendHealth, HealthMetrics, HealthStatus};

use crate::config::HealthCheck;
use crate::protocol::{DATE, StatusCode};
use crate::types::BackendId;
use dashmap::DashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time;

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
            let mut interval = time::interval(self.config.interval);
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
            && !backend_health.needs_check(self.config.interval)
        {
            return;
        }

        // Perform the health check with timeout
        let check_result = time::timeout(
            self.config.timeout,
            self.perform_health_check(provider.clone(), backend_id),
        )
        .await;

        // Update health status
        let mut backend_health = self.backend_health.entry(backend_id).or_default();

        match check_result {
            Ok(Ok(())) => backend_health.record_success(),
            Ok(Err(_)) | Err(_) => {
                backend_health.record_failure(self.config.unhealthy_threshold.get())
            }
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
        if StatusCode::parse(&response).is_some_and(|code| code.as_u16() == 111) {
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
    use crate::types::MaxErrors;
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
        assert_eq!(config.interval, Duration::from_secs(30));
        assert_eq!(config.timeout, Duration::from_secs(5));
        assert_eq!(config.unhealthy_threshold.get(), 3);
    }

    #[tokio::test]
    async fn test_health_check_config_custom() {
        let config = HealthCheck {
            interval: Duration::from_secs(10),
            timeout: Duration::from_secs(2),
            unhealthy_threshold: MaxErrors::try_new(5).unwrap(),
        };

        let checker = HealthChecker::new(config.clone());
        assert_eq!(checker.config.interval, Duration::from_secs(10));
        assert_eq!(checker.config.timeout, Duration::from_secs(2));
        assert_eq!(checker.config.unhealthy_threshold.get(), 5);
    }

    #[tokio::test]
    async fn test_simulated_connection_failure() {
        let config = HealthCheck {
            interval: Duration::from_millis(100),
            timeout: Duration::from_millis(50),
            unhealthy_threshold: MaxErrors::try_new(2).unwrap(),
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
            interval: Duration::from_millis(100),
            timeout: Duration::from_millis(50),
            unhealthy_threshold: MaxErrors::try_new(2).unwrap(),
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

    #[tokio::test]
    async fn test_is_healthy_for_registered_backend() {
        let config = HealthCheck::default();
        let checker = HealthChecker::new(config);
        let backend_id = BackendId::from_index(0);

        checker.register_backend(backend_id).await;

        // Should be healthy initially
        assert!(checker.is_healthy(backend_id).await);
    }

    #[tokio::test]
    async fn test_is_healthy_for_unregistered_backend() {
        let config = HealthCheck::default();
        let checker = HealthChecker::new(config);
        let backend_id = BackendId::from_index(99);

        // Unregistered backends assumed healthy
        assert!(checker.is_healthy(backend_id).await);
    }

    #[tokio::test]
    async fn test_is_healthy_after_failure() {
        let config = HealthCheck::default();
        let checker = HealthChecker::new(config);
        let backend_id = BackendId::from_index(0);

        checker.register_backend(backend_id).await;

        // Make unhealthy
        if let Some(mut backend_health) = checker.backend_health.get_mut(&backend_id) {
            backend_health.record_failure(3);
            backend_health.record_failure(3);
            backend_health.record_failure(3);
        }

        assert!(!checker.is_healthy(backend_id).await);
    }

    #[tokio::test]
    async fn test_get_backend_health_registered() {
        let config = HealthCheck::default();
        let checker = HealthChecker::new(config);
        let backend_id = BackendId::from_index(0);

        checker.register_backend(backend_id).await;

        let health = checker.get_backend_health(backend_id).await;
        assert!(health.is_some());
        assert_eq!(health.unwrap().status, HealthStatus::Healthy);
    }

    #[tokio::test]
    async fn test_get_backend_health_unregistered() {
        let config = HealthCheck::default();
        let checker = HealthChecker::new(config);
        let backend_id = BackendId::from_index(99);

        let health = checker.get_backend_health(backend_id).await;
        assert!(health.is_none());
    }

    #[tokio::test]
    async fn test_health_metrics_total_checks() {
        let config = HealthCheck::default();
        let checker = HealthChecker::new(config);
        let backend_id = BackendId::from_index(0);

        checker.register_backend(backend_id).await;

        // Perform some successes and failures
        if let Some(mut backend_health) = checker.backend_health.get_mut(&backend_id) {
            backend_health.record_success();
            backend_health.record_success();
            backend_health.record_failure(3);
        }

        let metrics = checker.get_metrics().await;
        assert_eq!(metrics.total_checks, 3); // 2 successes + 1 failure
    }

    #[tokio::test]
    async fn test_health_check_clone() {
        let config1 = HealthCheck {
            interval: Duration::from_secs(15),
            timeout: Duration::from_secs(3),
            unhealthy_threshold: MaxErrors::try_new(5).unwrap(),
        };

        let config2 = config1.clone();

        assert_eq!(config1.interval, config2.interval);
        assert_eq!(config1.timeout, config2.timeout);
        assert_eq!(config1.unhealthy_threshold, config2.unhealthy_threshold);
    }

    #[tokio::test]
    async fn test_health_check_debug() {
        let config = HealthCheck::default();
        let debug_str = format!("{:?}", config);

        assert!(debug_str.contains("HealthCheck"));
        assert!(debug_str.contains("interval"));
        assert!(debug_str.contains("timeout"));
        assert!(debug_str.contains("unhealthy_threshold"));
    }

    #[tokio::test]
    async fn test_multiple_backends_different_states() {
        let config = HealthCheck::default();
        let checker = HealthChecker::new(config);

        // Register 4 backends
        for i in 0..4 {
            checker.register_backend(BackendId::from_index(i)).await;
        }

        // Make backend 0 and 2 unhealthy
        for backend_idx in [0, 2] {
            if let Some(mut backend_health) = checker
                .backend_health
                .get_mut(&BackendId::from_index(backend_idx))
            {
                for _ in 0..3 {
                    backend_health.record_failure(3);
                }
            }
        }

        // Verify is_healthy for each
        assert!(!checker.is_healthy(BackendId::from_index(0)).await);
        assert!(checker.is_healthy(BackendId::from_index(1)).await);
        assert!(!checker.is_healthy(BackendId::from_index(2)).await);
        assert!(checker.is_healthy(BackendId::from_index(3)).await);

        let metrics = checker.get_metrics().await;
        assert_eq!(metrics.healthy_count, 2);
        assert_eq!(metrics.unhealthy_count, 2);
    }

    #[tokio::test]
    async fn test_health_check_custom_thresholds() {
        let config1 = HealthCheck {
            interval: Duration::from_secs(1),
            timeout: Duration::from_secs(1),
            unhealthy_threshold: MaxErrors::try_new(1).unwrap(),
        };

        let config2 = HealthCheck {
            interval: Duration::from_secs(60),
            timeout: Duration::from_secs(10),
            unhealthy_threshold: MaxErrors::try_new(10).unwrap(),
        };

        let checker1 = HealthChecker::new(config1);
        let checker2 = HealthChecker::new(config2);

        assert_eq!(checker1.config.unhealthy_threshold.get(), 1);
        assert_eq!(checker2.config.unhealthy_threshold.get(), 10);
    }

    #[tokio::test]
    async fn test_get_healthy_backends_empty() {
        let config = HealthCheck::default();
        let checker = HealthChecker::new(config);

        let healthy = checker.get_healthy_backends().await;
        assert!(healthy.is_empty());
    }

    #[tokio::test]
    async fn test_get_healthy_backends_all_unhealthy() {
        let config = HealthCheck::default();
        let checker = HealthChecker::new(config);

        // Register 3 backends and make all unhealthy
        for i in 0..3 {
            let backend_id = BackendId::from_index(i);
            checker.register_backend(backend_id).await;

            if let Some(mut backend_health) = checker.backend_health.get_mut(&backend_id) {
                for _ in 0..3 {
                    backend_health.record_failure(3);
                }
            }
        }

        let healthy = checker.get_healthy_backends().await;
        assert!(healthy.is_empty());
    }

    #[tokio::test]
    async fn test_backend_health_after_multiple_operations() {
        let config = HealthCheck::default();
        let checker = HealthChecker::new(config);
        let backend_id = BackendId::from_index(0);

        checker.register_backend(backend_id).await;

        // Record consecutive failures to reach threshold
        if let Some(mut backend_health) = checker.backend_health.get_mut(&backend_id) {
            backend_health.record_failure(3);
            backend_health.record_failure(3);
            backend_health.record_failure(3); // 3 consecutive - now unhealthy
        }

        assert!(!checker.is_healthy(backend_id).await);

        // Recover with a success
        if let Some(mut backend_health) = checker.backend_health.get_mut(&backend_id) {
            backend_health.record_success(); // Resets consecutive failures
        }

        assert!(checker.is_healthy(backend_id).await);
    }
}
