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
use tokio::sync::broadcast;
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
    /// Metrics for health check operations (lock-free)
    pub health_check_metrics: Arc<HealthCheckMetrics>,
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
pub struct Builder {
    host: String,
    port: u16,
    name: Option<String>,
    max_size: usize,
    username: Option<String>,
    password: Option<String>,
    tls_config: Option<TlsConfig>,
}

impl Builder {
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
                health_check_metrics: Arc::new(HealthCheckMetrics::new()),
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
                health_check_metrics: Arc::new(HealthCheckMetrics::new()),
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
    pub fn builder(host: impl Into<String>, port: u16) -> Builder {
        Builder::new(host, port)
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
            health_check_metrics: Arc::new(HealthCheckMetrics::new()),
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
            health_check_metrics: Arc::new(HealthCheckMetrics::new()),
        })
    }

    /// Create a connection provider from a server configuration
    ///
    /// This avoids unnecessary cloning of individual fields.
    pub fn from_server_config(server: &crate::config::Server) -> Result<Self> {
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
        let metrics = Arc::new(HealthCheckMetrics::new());
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

    /// Get a reference to the health check metrics
    pub fn health_check_metrics(&self) -> &HealthCheckMetrics {
        &self.health_check_metrics
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
        metrics: Arc<HealthCheckMetrics>,
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
                // Record metrics (lock-free)
                metrics.record_cycle(checked, failed);

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
        use crate::types::{AvailableConnections, CreatedConnections, MaxPoolSize};
        let status = self.pool.status();
        PoolStatus {
            available: AvailableConnections::new(status.available),
            max_size: MaxPoolSize::new(status.max_size),
            created: CreatedConnections::new(status.size),
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
        use crate::types::{AvailableConnections, CreatedConnections, MaxPoolSize};
        let status = self.pool.status();
        PoolStatus {
            available: AvailableConnections::new(status.available),
            max_size: MaxPoolSize::new(status.max_size),
            created: CreatedConnections::new(status.size),
        }
    }

    fn host(&self) -> &str {
        &self.pool.manager().host
    }

    fn port(&self) -> u16 {
        self.pool.manager().port
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder_new() {
        let builder = Builder::new("news.example.com", 119);
        assert_eq!(builder.host, "news.example.com");
        assert_eq!(builder.port, 119);
        assert_eq!(builder.max_size, 10); // Default
        assert!(builder.name.is_none());
        assert!(builder.username.is_none());
        assert!(builder.password.is_none());
        assert!(builder.tls_config.is_none());
    }

    #[test]
    fn test_builder_with_name() {
        let builder = Builder::new("example.com", 119).name("Test Server");
        assert_eq!(builder.name, Some("Test Server".to_string()));
    }

    #[test]
    fn test_builder_with_max_connections() {
        let builder = Builder::new("example.com", 119).max_connections(25);
        assert_eq!(builder.max_size, 25);
    }

    #[test]
    fn test_builder_with_username() {
        let builder = Builder::new("example.com", 119).username("testuser");
        assert_eq!(builder.username, Some("testuser".to_string()));
    }

    #[test]
    fn test_builder_with_password() {
        let builder = Builder::new("example.com", 119).password("testpass");
        assert_eq!(builder.password, Some("testpass".to_string()));
    }

    #[test]
    fn test_builder_with_tls_config() {
        let tls_config = TlsConfig::builder().enabled(true).build();
        let builder = Builder::new("example.com", 563).tls_config(tls_config.clone());
        assert!(builder.tls_config.is_some());
    }

    #[test]
    fn test_builder_chaining() {
        let builder = Builder::new("news.example.com", 119)
            .name("Chained Server")
            .max_connections(30)
            .username("user")
            .password("pass");

        assert_eq!(builder.name, Some("Chained Server".to_string()));
        assert_eq!(builder.max_size, 30);
        assert_eq!(builder.username, Some("user".to_string()));
        assert_eq!(builder.password, Some("pass".to_string()));
    }

    #[test]
    fn test_builder_default_name_from_host_port() {
        let provider = Builder::new("test.example.com", 8119)
            .max_connections(5)
            .build()
            .unwrap();

        // Default name should be "host:port"
        assert_eq!(provider.name(), "test.example.com:8119");
    }

    #[test]
    fn test_builder_custom_name_used() {
        let provider = Builder::new("test.example.com", 8119)
            .name("Custom Name")
            .build()
            .unwrap();

        assert_eq!(provider.name(), "Custom Name");
    }

    #[test]
    fn test_provider_builder_method() {
        let builder = DeadpoolConnectionProvider::builder("example.com", 119);
        assert_eq!(builder.host, "example.com");
        assert_eq!(builder.port, 119);
    }

    #[test]
    fn test_provider_status_conversion() {
        let provider = DeadpoolConnectionProvider::builder("localhost", 119)
            .max_connections(15)
            .build()
            .unwrap();

        let status = ConnectionProvider::status(&provider);
        assert_eq!(status.max_size.get(), 15);
        // Initially no connections created
        assert_eq!(status.created.get(), 0);
    }

    #[test]
    fn test_provider_implements_connection_pool_trait() {
        let provider = DeadpoolConnectionProvider::builder("localhost", 119)
            .build()
            .unwrap();

        // Should implement ConnectionPool trait methods
        assert_eq!(provider.name(), "localhost:119");
        assert_eq!(provider.host(), "localhost");
        assert_eq!(provider.port(), 119);
    }

    #[test]
    fn test_provider_with_all_builder_options() {
        let tls_config = TlsConfig::builder().enabled(false).build();

        let provider = DeadpoolConnectionProvider::builder("news.test.com", 563)
            .name("Full Test")
            .max_connections(42)
            .username("testuser")
            .password("testpass")
            .tls_config(tls_config)
            .build()
            .unwrap();

        assert_eq!(provider.name(), "Full Test");
        assert_eq!(provider.host(), "news.test.com");
        assert_eq!(provider.port(), 563);

        let status = ConnectionPool::status(&provider);
        assert_eq!(status.max_size.get(), 42);
    }

    #[test]
    fn test_health_check_metrics_initialization() {
        let provider = DeadpoolConnectionProvider::builder("localhost", 119)
            .build()
            .unwrap();

        let metrics = &provider.health_check_metrics;
        assert_eq!(metrics.cycles_run(), 0);
        assert_eq!(metrics.connections_checked(), 0);
        assert_eq!(metrics.connections_failed(), 0);
        assert_eq!(metrics.failure_rate(), 0.0);
    }

    #[test]
    fn test_builder_accepts_string_types() {
        // Test that builder accepts &str
        let _ = Builder::new("example.com", 119);

        // Test that builder accepts String
        let _ = Builder::new(String::from("example.com"), 119);

        // Test that name accepts &str
        let _ = Builder::new("example.com", 119).name("test");

        // Test that name accepts String
        let _ = Builder::new("example.com", 119).name(String::from("test"));
    }

    #[test]
    fn test_builder_zero_max_connections() {
        // Should allow zero (deadpool will handle it)
        let provider = Builder::new("localhost", 119)
            .max_connections(0)
            .build()
            .unwrap();

        let status = ConnectionProvider::status(&provider);
        assert_eq!(status.max_size.get(), 0);
    }

    #[test]
    fn test_builder_large_max_connections() {
        let provider = Builder::new("localhost", 119)
            .max_connections(1000)
            .build()
            .unwrap();

        let status = ConnectionProvider::status(&provider);
        assert_eq!(status.max_size.get(), 1000);
    }

    #[test]
    fn test_connection_pool_trait_status_consistency() {
        let provider = DeadpoolConnectionProvider::builder("localhost", 119)
            .max_connections(20)
            .build()
            .unwrap();

        // Both trait implementations should return same status
        let status1 = <DeadpoolConnectionProvider as ConnectionProvider>::status(&provider);
        let status2 = <DeadpoolConnectionProvider as ConnectionPool>::status(&provider);

        assert_eq!(status1.max_size, status2.max_size);
        assert_eq!(status1.available, status2.available);
        assert_eq!(status1.created, status2.created);
    }

    #[test]
    fn test_provider_name_special_characters() {
        let provider = Builder::new("example.com", 119)
            .name("Server-123_Test.Name")
            .build()
            .unwrap();

        assert_eq!(provider.name(), "Server-123_Test.Name");
    }

    #[test]
    fn test_provider_name_unicode() {
        let provider = Builder::new("example.com", 119)
            .name("测试服务器")
            .build()
            .unwrap();

        assert_eq!(provider.name(), "测试服务器");
    }

    #[test]
    fn test_provider_empty_name() {
        let provider = Builder::new("example.com", 119).name("").build().unwrap();

        assert_eq!(provider.name(), "");
    }

    #[test]
    fn test_builder_idempotent_chaining() {
        // Setting the same value multiple times should use the last value
        let builder = Builder::new("example.com", 119)
            .name("First")
            .name("Second")
            .max_connections(10)
            .max_connections(20);

        assert_eq!(builder.name, Some("Second".to_string()));
        assert_eq!(builder.max_size, 20);
    }
}
