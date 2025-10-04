mod types;

pub use types::{BackendHealth, HealthMetrics, HealthStatus};

use crate::types::BackendId;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::RwLock;
use tokio::time;

/// Configuration for health checking
#[derive(Debug, Clone)]
pub struct HealthCheckConfig {
    /// Interval between health checks
    pub check_interval: Duration,
    /// Timeout for each health check
    pub check_timeout: Duration,
    /// Number of consecutive failures before marking unhealthy
    pub unhealthy_threshold: u32,
}

impl Default for HealthCheckConfig {
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
    /// Health status for each backend
    backend_health: Arc<RwLock<HashMap<BackendId, BackendHealth>>>,
    /// Configuration
    config: HealthCheckConfig,
}

impl HealthChecker {
    /// Create a new health checker
    pub fn new(config: HealthCheckConfig) -> Self {
        Self {
            backend_health: Arc::new(RwLock::new(HashMap::new())),
            config,
        }
    }

    /// Initialize health tracking for a backend
    pub async fn register_backend(&self, backend_id: BackendId) {
        let mut health = self.backend_health.write().await;
        health.entry(backend_id).or_insert_with(BackendHealth::new);
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
                    self.check_backend(provider.clone(), backend_id).await;
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
        {
            let health = self.backend_health.read().await;
            if let Some(backend_health) = health.get(&backend_id) {
                if !backend_health.needs_check(self.config.check_interval) {
                    return;
                }
            }
        }

        // Perform the health check with timeout
        let check_result = time::timeout(
            self.config.check_timeout,
            self.perform_health_check(provider.clone(), backend_id),
        )
        .await;

        // Update health status
        let mut health = self.backend_health.write().await;
        let backend_health = health.entry(backend_id).or_insert_with(BackendHealth::new);

        match check_result {
            Ok(Ok(())) => {
                backend_health.record_success();
            }
            Ok(Err(_)) | Err(_) => {
                backend_health.record_failure(self.config.unhealthy_threshold);
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
        conn.write_all(b"DATE\r\n").await?;
        conn.flush().await?;

        // Read response
        let mut reader = BufReader::new(&mut *conn);
        let mut response = String::new();
        reader.read_line(&mut response).await?;

        // Check if response indicates success (111 response)
        if response.starts_with("111") {
            Ok(())
        } else {
            Err("Unexpected response from DATE command".into())
        }
    }

    /// Check if a backend is healthy
    pub async fn is_healthy(&self, backend_id: BackendId) -> bool {
        let health = self.backend_health.read().await;
        health
            .get(&backend_id)
            .map(|h| h.status == HealthStatus::Healthy)
            .unwrap_or(true) // Assume healthy if not tracked yet
    }

    /// Get health status for a specific backend
    pub async fn get_backend_health(&self, backend_id: BackendId) -> Option<BackendHealth> {
        let health = self.backend_health.read().await;
        health.get(&backend_id).cloned()
    }

    /// Get aggregated health metrics
    pub async fn get_metrics(&self) -> HealthMetrics {
        let health = self.backend_health.read().await;

        let mut metrics = HealthMetrics::default();
        metrics.total_checks = health.values().map(|h| h.total_successes + h.total_failures).sum();

        for backend_health in health.values() {
            match backend_health.status {
                HealthStatus::Healthy => metrics.healthy_count += 1,
                HealthStatus::Unhealthy => metrics.unhealthy_count += 1,
            }
        }

        metrics
    }

    /// Get all healthy backend IDs
    pub async fn get_healthy_backends(&self) -> Vec<BackendId> {
        let health = self.backend_health.read().await;
        health
            .iter()
            .filter(|(_, h)| h.status == HealthStatus::Healthy)
            .map(|(id, _)| *id)
            .collect()
    }
}
