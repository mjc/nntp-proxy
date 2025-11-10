//! Connection pool provider implementation
//!
//! This module contains the `DeadpoolConnectionProvider` which manages a pool of
//! NNTP connections using the deadpool library. It provides:
//! - Connection pooling with configurable size
//! - Automatic connection recycling
//! - Periodic health checks for idle connections
//! - Graceful shutdown with QUIT commands

use super::connection_pool::ConnectionPool;
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

/// Builder for constructing `DeadpoolConnectionProvider` instances
///
/// Provides a fluent API for creating connection providers with optional TLS configuration.
///
/// # Examples
///
/// ```no_run
/// use nntp_proxy::pool::DeadpoolConnectionProvider;
/// use nntp_proxy::tls::TlsConfig;
///
/// // Basic provider without TLS
/// let provider = DeadpoolConnectionProvider::builder("news.example.com", 119)
///     .name("Example Server")
///     .max_connections(10)
///     .build()
///     .unwrap();
///
/// // Provider with TLS and authentication
/// let tls_config = TlsConfig::builder()
///     .enabled(true)
///     .verify_cert(true)
///     .build();
///
/// let provider = DeadpoolConnectionProvider::builder("secure.example.com", 563)
///     .name("Secure Server")
///     .max_connections(20)
///     .username("user")
///     .password("pass")
///     .tls_config(tls_config)
///     .build()
///     .unwrap();
/// ```
pub struct DeadpoolConnectionProviderBuilder {
    host: String,
    port: u16,
    name: Option<String>,
    max_size: usize,
    username: Option<String>,
    password: Option<String>,
    tls_config: Option<TlsConfig>,
}

impl DeadpoolConnectionProviderBuilder {
    /// Create a new builder with required connection parameters
    ///
    /// # Arguments
    /// * `host` - Backend server hostname or IP address
    /// * `port` - Backend server port number
    #[must_use]
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            name: None,
            max_size: 10, // Default max connections
            username: None,
            password: None,
            tls_config: None,
        }
    }

    /// Set a friendly name for logging (defaults to "host:port")
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Set the maximum number of concurrent connections in the pool
    #[must_use]
    pub fn max_connections(mut self, max_size: usize) -> Self {
        self.max_size = max_size;
        self
    }

    /// Set authentication username
    #[must_use]
    pub fn username(mut self, username: impl Into<String>) -> Self {
        self.username = Some(username.into());
        self
    }

    /// Set authentication password
    #[must_use]
    pub fn password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }

    /// Configure TLS settings for secure connections
    #[must_use]
    pub fn tls_config(mut self, config: TlsConfig) -> Self {
        self.tls_config = Some(config);
        self
    }

    /// Build the connection provider
    ///
    /// # Errors
    ///
    /// Returns an error if TLS initialization fails when TLS is enabled
    pub fn build(self) -> Result<DeadpoolConnectionProvider> {
        let name = self
            .name
            .unwrap_or_else(|| format!("{}:{}", self.host, self.port));

        if let Some(tls_config) = self.tls_config {
            // Build with TLS
            let manager = TcpManager::new_with_tls(
                self.host,
                self.port,
                name.clone(),
                self.username,
                self.password,
                tls_config,
            )?;
            let pool = Pool::builder(manager)
                .max_size(self.max_size)
                .build()
                .expect("Failed to create connection pool");

            Ok(DeadpoolConnectionProvider {
                pool,
                name,
                keepalive_interval: None,
                shutdown_tx: None,
                health_check_metrics: Arc::new(Mutex::new(HealthCheckMetrics::new())),
            })
        } else {
            // Build without TLS
            let manager = TcpManager::new(
                self.host,
                self.port,
                name.clone(),
                self.username,
                self.password,
            );
            let pool = Pool::builder(manager)
                .max_size(self.max_size)
                .build()
                .expect("Failed to create connection pool");

            Ok(DeadpoolConnectionProvider {
                pool,
                name,
                keepalive_interval: None,
                shutdown_tx: None,
                health_check_metrics: Arc::new(Mutex::new(HealthCheckMetrics::new())),
            })
        }
    }
}

impl DeadpoolConnectionProvider {
    /// Create a builder for constructing a connection provider
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use nntp_proxy::pool::DeadpoolConnectionProvider;
    ///
    /// let provider = DeadpoolConnectionProvider::builder("news.example.com", 119)
    ///     .name("Example")
    ///     .max_connections(15)
    ///     .build()
    ///     .unwrap();
    /// ```
    #[must_use]
    pub fn builder(host: impl Into<String>, port: u16) -> DeadpoolConnectionProviderBuilder {
        DeadpoolConnectionProviderBuilder::new(host, port)
    }
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
        let tls_builder = TlsConfig::builder()
            .enabled(server.use_tls)
            .verify_cert(server.tls_verify_cert);

        // Use functional approach to conditionally add cert_path
        let tls_builder = server
            .tls_cert_path
            .as_ref()
            .map(|cert_path| tls_builder.clone().cert_path(cert_path.as_str()))
            .unwrap_or(tls_builder);

        let tls_config = tls_builder.build();

        let manager = TcpManager::new_with_tls(
            server.host.to_string(),
            server.port.get(),
            server.name.to_string(),
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
            let name_clone = server.name.to_string();
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
            name: server.name.to_string(),
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

#[async_trait]
impl ConnectionPool for DeadpoolConnectionProvider {
    async fn get(&self) -> Result<crate::stream::ConnectionStream> {
        let conn = self.get_pooled_connection().await?;

        // Extract the ConnectionStream from the deadpool Object wrapper.
        //
        // IMPORTANT: Object::take() consumes the wrapper and returns the inner stream.
        // This removes the connection from the pool permanently - it will NOT be
        // automatically returned when dropped. This is intentional for the ConnectionPool
        // trait which provides raw streams that the caller is responsible for managing.
        //
        // For automatic pool return, use get_pooled_connection() instead, which returns
        // a managed::Object that auto-returns to the pool on drop.
        let stream = deadpool::managed::Object::take(conn);
        Ok(stream)
    }
    fn name(&self) -> &str {
        &self.name
    }

    fn status(&self) -> PoolStatus {
        let status = self.pool.status();
        PoolStatus {
            available: status.available,
            max_size: status.max_size,
            created: status.size,
        }
    }

    fn host(&self) -> &str {
        &self.pool.manager().host
    }

    fn port(&self) -> u16 {
        self.pool.manager().port
    }
}
