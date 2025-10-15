//! Connection pool provider implementation
//!
//! This module contains the `DeadpoolConnectionProvider` which manages a pool of
//! NNTP connections using the deadpool library. It provides:
//! - Connection pooling with configurable size
//! - Automatic connection recycling
//! - Periodic health checks for idle connections
//! - Graceful shutdown with QUIT commands

use super::connection_trait::ConnectionProvider;
use super::deadpool_connection::{Pool, TcpManager};
use super::health_check::{HealthCheckMetrics, check_date_response};
use crate::pool::PoolStatus;
use crate::tls::TlsConfig;
use anyhow::Result;
use async_trait::async_trait;
use deadpool::managed;
use std::sync::Arc;
use tokio::sync::{Mutex, broadcast};
use tracing::{debug, info, warn};

/// Connection provider using deadpool for connection pooling
#[derive(Debug, Clone)]
pub struct DeadpoolConnectionProvider {
    pool: Pool,
    name: String,
    /// Keepalive interval for periodic health checks (stored for debugging/info)
    #[allow(dead_code)]
    keepalive_interval: Option<std::time::Duration>,
    /// Shutdown signal sender for background health check task
    /// Kept alive to enable graceful shutdown when the provider is dropped
    #[allow(dead_code)]
    shutdown_tx: Option<broadcast::Sender<()>>,
    /// Metrics for health check operations
    pub health_check_metrics: Arc<Mutex<HealthCheckMetrics>>,
}

impl DeadpoolConnectionProvider {
    /// Create a new connection provider
    pub fn new(
        host: String,
        port: u16,
        name: String,
        max_size: usize,
        username: Option<String>,
        password: Option<String>,
    ) -> Self {
        let manager = TcpManager::new(host, port, name.clone(), username, password);
        let pool = Pool::builder(manager)
            .max_size(max_size)
            .build()
            .expect("Failed to create connection pool");

        Self {
            pool,
            name,
            keepalive_interval: None,
            shutdown_tx: None,
            health_check_metrics: Arc::new(Mutex::new(HealthCheckMetrics::new())),
        }
    }

    /// Create a new connection provider with TLS support
    pub fn new_with_tls(
        host: String,
        port: u16,
        name: String,
        max_size: usize,
        username: Option<String>,
        password: Option<String>,
        tls_config: TlsConfig,
    ) -> Result<Self> {
        let manager =
            TcpManager::new_with_tls(host, port, name.clone(), username, password, tls_config)?;
        let pool = Pool::builder(manager)
            .max_size(max_size)
            .build()
            .expect("Failed to create connection pool");

        Ok(Self {
            pool,
            name,
            keepalive_interval: None,
            shutdown_tx: None,
            health_check_metrics: Arc::new(Mutex::new(HealthCheckMetrics::new())),
        })
    }

    /// Create a connection provider from a server configuration
    ///
    /// This avoids unnecessary cloning of individual fields.
    pub fn from_server_config(server: &crate::config::ServerConfig) -> Result<Self> {
        let tls_config = TlsConfig {
            use_tls: server.use_tls,
            tls_verify_cert: server.tls_verify_cert,
            tls_cert_path: server.tls_cert_path.clone(),
        };

        let manager = TcpManager::new_with_tls(
            server.host.as_str().to_string(),
            server.port.get(),
            server.name.as_str().to_string(),
            server.username.clone(),
            server.password.clone(),
            tls_config,
        )?;
        let pool = Pool::builder(manager)
            .max_size(server.max_connections.get())
            .build()
            .expect("Failed to create connection pool");

        let keepalive_interval = server.connection_keepalive;

        // Create metrics and shutdown channel if keepalive is enabled
        let metrics = Arc::new(Mutex::new(HealthCheckMetrics::new()));
        let shutdown_tx = if let Some(interval) = keepalive_interval {
            let (tx, rx) = broadcast::channel(1);

            // Spawn background health check task
            let pool_clone = pool.clone();
            let name_clone = server.name.as_str().to_string();
            let metrics_clone = metrics.clone();
            tokio::spawn(async move {
                Self::run_periodic_health_checks(
                    pool_clone,
                    name_clone,
                    interval,
                    rx,
                    metrics_clone,
                )
                .await;
            });

            Some(tx)
        } else {
            None
        };

        Ok(Self {
            pool,
            name: server.name.as_str().to_string(),
            keepalive_interval,
            shutdown_tx,
            health_check_metrics: metrics,
        })
    }

    /// Get a connection from the pool (automatically returned when dropped)
    pub async fn get_pooled_connection(&self) -> Result<managed::Object<TcpManager>> {
        self.pool
            .get()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get connection from {}: {}", self.name, e))
    }

    /// Get the maximum pool size
    #[must_use]
    #[inline]
    pub fn max_size(&self) -> usize {
        self.pool.status().max_size
    }

    /// Get a snapshot of the current health check metrics
    pub async fn get_health_check_metrics(&self) -> HealthCheckMetrics {
        self.health_check_metrics.lock().await.clone()
    }

    /// Gracefully shutdown the periodic health check task
    ///
    /// This sends a shutdown signal to the background health check task.
    /// The task will complete its current cycle and then terminate.
    pub fn shutdown(&self) {
        if let Some(tx) = &self.shutdown_tx {
            let _ = tx.send(());
        }
    }

    /// Run periodic health checks on idle connections
    ///
    /// This task runs in the background checking a limited number of idle connections
    /// each cycle. It can be gracefully shut down via the shutdown_rx channel.
    /// Health check metrics are recorded in the provided metrics object.
    async fn run_periodic_health_checks(
        pool: Pool,
        name: String,
        interval: std::time::Duration,
        mut shutdown_rx: broadcast::Receiver<()>,
        metrics: Arc<Mutex<HealthCheckMetrics>>,
    ) {
        use crate::constants::pool::{
            HEALTH_CHECK_POOL_TIMEOUT_MS, MAX_CONNECTIONS_PER_HEALTH_CHECK_CYCLE,
        };
        use tokio::time::{Duration, sleep};

        info!(
            pool = %name,
            interval_secs = interval.as_secs(),
            "Starting periodic health checks"
        );

        loop {
            tokio::select! {
                _ = sleep(interval) => {
                    // Time to run health check
                }
                _ = shutdown_rx.recv() => {
                    info!(pool = %name, "Shutting down periodic health check task");
                    break;
                }
            }

            let status = pool.status();
            if status.available == 0 {
                continue;
            }

            debug!(
                pool = %name,
                available = status.available,
                max_check = MAX_CONNECTIONS_PER_HEALTH_CHECK_CYCLE,
                "Running health check cycle"
            );

            // Check up to MAX_CONNECTIONS_PER_HEALTH_CHECK_CYCLE idle connections per cycle
            let check_count =
                std::cmp::min(status.available, MAX_CONNECTIONS_PER_HEALTH_CHECK_CYCLE);
            let mut checked = 0;
            let mut failed = 0;

            let mut timeouts = managed::Timeouts::new();
            timeouts.wait = Some(Duration::from_millis(HEALTH_CHECK_POOL_TIMEOUT_MS));

            for _ in 0..check_count {
                if let Ok(mut conn_obj) = pool.timeout_get(&timeouts).await {
                    checked += 1;

                    // Perform DATE health check
                    if let Err(e) = check_date_response(&mut conn_obj).await {
                        failed += 1;
                        warn!(
                            pool = %name,
                            error = %e,
                            "Health check failed, discarding connection"
                        );
                        // Drop the connection without returning it to pool
                        drop(managed::Object::take(conn_obj));
                    } else {
                        // Connection is healthy, return to pool automatically via Drop
                        drop(conn_obj);
                    }
                } else {
                    break;
                }
            }

            if checked > 0 {
                // Record metrics
                {
                    let mut m = metrics.lock().await;
                    m.record_cycle(checked, failed);
                }

                debug!(
                    pool = %name,
                    checked = checked,
                    failed = failed,
                    "Health check cycle complete"
                );
            }
        }

        info!(pool = %name, "Periodic health check task terminated");
    }

    /// Gracefully shutdown the pool
    pub async fn graceful_shutdown(&self) {
        use deadpool::managed::Object;
        use tokio::io::AsyncWriteExt;

        let status = self.pool.status();
        info!(
            "Shutting down pool '{}' ({} idle connections)",
            self.name, status.available
        );

        // Send QUIT to idle connections with minimal timeout
        let mut timeouts = managed::Timeouts::new();
        timeouts.wait = Some(std::time::Duration::from_millis(1));

        for _ in 0..status.available {
            if let Ok(conn_obj) = self.pool.timeout_get(&timeouts).await {
                let mut conn = Object::take(conn_obj);
                let _ = conn.write_all(b"QUIT\r\n").await;
            } else {
                break;
            }
        }

        self.pool.close();
    }
}

#[async_trait]
impl ConnectionProvider for DeadpoolConnectionProvider {
    fn status(&self) -> PoolStatus {
        let status = self.pool.status();
        PoolStatus {
            available: status.available,
            max_size: status.max_size,
            created: status.size,
        }
    }
}
