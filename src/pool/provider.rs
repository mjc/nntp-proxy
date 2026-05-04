//! Connection pool provider implementation
//!
//! This module contains the `DeadpoolConnectionProvider` which manages a pool of
//! NNTP connections using the deadpool library. It provides:
//! - Connection pooling with configurable size
//! - Automatic connection recycling
//! - Periodic health checks for idle connections
//! - Graceful shutdown with QUIT commands

use super::connection_trait::ConnectionProvider;
use super::deadpool_connection::{Pool, TcpManager, TcpManagerOptions};
use super::health_check::{HealthCheckMetrics, check_date_response};
use crate::pool::PoolStatus;
use crate::tls::TlsConfig;
use anyhow::Result;
use async_trait::async_trait;
use deadpool::managed;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

/// Connection provider using deadpool for connection pooling
#[derive(Debug, Clone)]
pub struct DeadpoolConnectionProvider {
    pool: Pool,
    name: String,
    /// Shutdown signal sender for background health check task.
    /// Stored to keep the channel alive - when dropped, the background task will terminate.
    /// Used by `shutdown()` method to gracefully stop health checks.
    shutdown_tx: Option<broadcast::Sender<()>>,
    /// Metrics for health check operations (lock-free)
    pub health_check_metrics: Arc<HealthCheckMetrics>,
    /// Original max pool size (before any cooldown reductions)
    original_max_size: usize,
    /// Number of active cooldown timers reducing pool size
    active_cooldowns: Arc<std::sync::atomic::AtomicUsize>,
    /// Connection replacement cooldown duration (None = disabled)
    replacement_cooldown: Option<std::time::Duration>,
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
    pub const fn max_connections(mut self, max_size: usize) -> Self {
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

        let manager = TcpManager::new(
            self.host,
            self.port,
            name.clone(),
            TcpManagerOptions {
                username: self.username,
                password: self.password,
                tls_config: self.tls_config,
                ..TcpManagerOptions::default()
            },
        )?;

        Ok(DeadpoolConnectionProvider::from_manager(
            manager,
            name,
            self.max_size,
        ))
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

    /// Create a simple connection provider with defaults
    ///
    /// Useful for testing and simple use cases. Uses 10 connections, no auth.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use nntp_proxy::pool::DeadpoolConnectionProvider;
    ///
    /// let provider = DeadpoolConnectionProvider::simple("news.example.com", 119)?;
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    ///
    /// # Errors
    /// Returns any validation or pool-construction error from the builder.
    pub fn simple(host: impl Into<String>, port: u16) -> Result<Self> {
        Self::builder(host, port).build()
    }

    /// Create a connection provider with authentication
    ///
    /// Convenience constructor for the common case of username/password auth.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use nntp_proxy::pool::DeadpoolConnectionProvider;
    ///
    /// let provider = DeadpoolConnectionProvider::with_auth(
    ///     "news.example.com",
    ///     119,
    ///     "myuser",
    ///     "mypass",
    /// )?;
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    ///
    /// # Errors
    /// Returns any validation or pool-construction error from the builder.
    pub fn with_auth(
        host: impl Into<String>,
        port: u16,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Result<Self> {
        Self::builder(host, port)
            .username(username)
            .password(password)
            .build()
    }

    /// Create a TLS-enabled connection provider
    ///
    /// Uses default TLS settings (verify certificates, system CA store).
    /// For NNTPS (port 563) or STARTTLS.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use nntp_proxy::pool::DeadpoolConnectionProvider;
    ///
    /// let provider = DeadpoolConnectionProvider::with_tls("news.example.com", 563)?;
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    ///
    /// # Errors
    /// Returns any TLS initialization, validation, or pool-construction error.
    pub fn with_tls(host: impl Into<String>, port: u16) -> Result<Self> {
        Self::builder(host, port)
            .tls_config(TlsConfig::default())
            .build()
    }

    /// Create a TLS-enabled connection provider with authentication
    ///
    /// Combines TLS with username/password auth - the most common setup
    /// for commercial Usenet providers.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use nntp_proxy::pool::DeadpoolConnectionProvider;
    ///
    /// let provider = DeadpoolConnectionProvider::with_tls_auth(
    ///     "news.example.com",
    ///     563,
    ///     "myuser",
    ///     "mypass",
    /// )?;
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    ///
    /// # Errors
    /// Returns any TLS initialization, validation, or pool-construction error.
    pub fn with_tls_auth(
        host: impl Into<String>,
        port: u16,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Result<Self> {
        Self::builder(host, port)
            .username(username)
            .password(password)
            .tls_config(TlsConfig::default())
            .build()
    }

    /// Create a new connection provider (plain TCP, no TLS)
    ///
    /// For TLS support, use `new_with_tls()` or the builder API.
    #[must_use]
    ///
    /// # Panics
    /// Panics only if plain `TcpManager` construction unexpectedly becomes
    /// fallible or deadpool rejects the requested pool size.
    pub fn new(
        host: String,
        port: u16,
        name: String,
        max_size: usize,
        username: Option<String>,
        password: Option<String>,
    ) -> Self {
        // Plain TCP (no TLS) cannot fail during TcpManager construction
        Self::from_manager(
            TcpManager::new(
                host,
                port,
                name.clone(),
                TcpManagerOptions {
                    username,
                    password,
                    ..TcpManagerOptions::default()
                },
            )
            .expect("Plain TCP TcpManager creation cannot fail"),
            name,
            max_size,
        )
    }

    /// Create a new connection provider with TLS support
    ///
    /// # Errors
    /// Returns any TLS initialization or provider-construction error.
    pub fn new_with_tls(
        host: String,
        port: u16,
        name: String,
        max_size: usize,
        username: Option<String>,
        password: Option<String>,
        tls_config: TlsConfig,
    ) -> Result<Self> {
        let manager = TcpManager::new(
            host,
            port,
            name.clone(),
            TcpManagerOptions {
                username,
                password,
                tls_config: Some(tls_config),
                ..TcpManagerOptions::default()
            },
        )?;
        Ok(Self::from_manager(manager, name, max_size))
    }

    /// Construct a provider from a pre-built `TcpManager`
    fn from_manager(manager: TcpManager, name: String, max_size: usize) -> Self {
        let pool = Pool::builder(manager)
            .max_size(max_size)
            .build()
            .expect("Failed to create connection pool");

        Self {
            pool,
            name,
            shutdown_tx: None,
            health_check_metrics: Arc::new(HealthCheckMetrics::new()),
            original_max_size: max_size,
            active_cooldowns: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            replacement_cooldown: None, // Default: no cooldown
        }
    }

    /// Create a connection provider from a server configuration
    ///
    /// This avoids unnecessary cloning of individual fields.
    ///
    /// # Errors
    /// Returns any TLS initialization or manager-construction error implied by
    /// the server configuration.
    ///
    /// # Panics
    /// Panics only if deadpool rejects the validated pool size.
    pub fn from_server_config(
        server: &crate::config::Server,
        recv_buffer_size: usize,
        send_buffer_size: usize,
    ) -> Result<Self> {
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

        let manager = TcpManager::new(
            server.host.to_string(),
            server.port.get(),
            server.name.to_string(),
            TcpManagerOptions {
                username: server.username.clone(),
                password: server.password.clone(),
                tls_config: Some(tls_config),
                recv_buffer_size,
                send_buffer_size,
                compress: server.compress,
                compress_level: server.compress_level,
                ..TcpManagerOptions::default()
            },
        )?;
        let max_size = server.max_connections.get();
        let pool = Pool::builder(manager)
            .max_size(max_size)
            .build()
            .expect("Failed to create connection pool");

        let keepalive_interval = server.connection_keepalive;

        // Create metrics and shutdown channel if keepalive is enabled
        let metrics = Arc::new(HealthCheckMetrics::new());
        let shutdown_tx = keepalive_interval.map_or_else(
            || None,
            |interval| {
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
            },
        );

        Ok(Self {
            pool,
            name: server.name.to_string(),
            shutdown_tx,
            health_check_metrics: metrics,
            original_max_size: max_size,
            active_cooldowns: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            replacement_cooldown: server.replacement_cooldown,
        })
    }

    /// Get a connection from the pool (automatically returned when dropped)
    ///
    /// # Errors
    /// Returns [`crate::connection_error::ConnectionError`] if deadpool cannot
    /// provide a healthy backend connection.
    pub async fn get_pooled_connection(
        &self,
    ) -> Result<managed::Object<TcpManager>, crate::connection_error::ConnectionError> {
        use crate::connection_error::ConnectionError;
        self.pool.get().await.map_err(|e| {
            let status = self.pool.status();
            let err = match e {
                deadpool::managed::PoolError::Backend(conn_err) => conn_err,
                _other => ConnectionError::PoolExhausted {
                    backend: self.name.clone(),
                    max_size: status.max_size,
                },
            };
            warn!(
                pool = %self.name,
                max_size = status.max_size,
                available = status.available,
                current_size = status.size,
                waiting = status.waiting,
                error = %err,
                "Connection acquisition failed"
            );
            err
        })
    }

    /// Clear all idle connections from the pool
    ///
    /// This drops all idle connections by resizing the pool to 0 and back.
    /// Active (checked-out) connections are not affected - they will be discarded
    /// when returned instead of being recycled.
    ///
    /// Use this to clear potentially stale connections after an idle period.
    pub fn clear_idle_connections(&self) {
        let available = self.pool.status().available;

        if available > 0 {
            debug!(
                pool = %self.name,
                available = available,
                "Clearing idle connections from pool"
            );

            // Calculate target max based on original size minus active cooldowns
            let cooldowns = self.active_cooldowns.load(Ordering::Acquire);
            let target_max = self.original_max_size.saturating_sub(cooldowns).max(1);

            // Resize to 0 drops all idle connections
            self.pool.resize(0);
            // Resize back to target allows new connections
            self.pool.resize(target_max);
        }
    }

    /// Remove a broken connection and temporarily reduce pool size.
    ///
    /// Gives the backend time to release the old connection's slot before
    /// deadpool creates a replacement. Prevents connection count from
    /// exceeding the backend's limit during high churn.
    ///
    /// If `replacement_cooldown` is None or `Duration::ZERO` (disabled), immediately drops
    /// the connection without cooldown (behaves like normal pool removal).
    ///
    /// CRITICAL: When cooldown is active, we resize the pool BEFORE dropping the
    /// connection. Otherwise, between `drop(conn)` and `pool.resize()`, any waiter
    /// calling `pool.get()` sees `size < max_size` and immediately creates a
    /// replacement — defeating the cooldown entirely.
    ///
    /// The ordering invariant is enforced at compile time: `conn` is moved into
    /// either [`shutdown_and_drop`] or [`resize_then_drop`], so the caller cannot
    /// accidentally `drop(conn)` before `pool.resize()`. Any attempt to reorder
    /// would be a use-after-move error.
    pub fn remove_with_cooldown(&self, conn: managed::Object<TcpManager>) {
        // If cooldown is disabled (None or zero duration), just shut down and drop
        let Some(cooldown) = self.replacement_cooldown.filter(|d| !d.is_zero()) else {
            let _ = socket2::SockRef::from(conn.underlying_tcp_stream())
                .shutdown(std::net::Shutdown::Both);
            drop(conn);
            return;
        };

        // Cap cooldowns to prevent pool collapse: never reduce below half original size.
        // If >50% of connections fail simultaneously, it's a systemic backend issue
        // (restart, network partition). Continuing to reduce would starve all clients.
        // Better to keep half the pool and let failed connections return errors.
        let max_reduction = self.original_max_size / 2;
        let current = self.active_cooldowns.load(Ordering::Acquire);
        if current >= max_reduction {
            // Already at max reduction — just shut down and drop without further cooldown
            let _ = socket2::SockRef::from(conn.underlying_tcp_stream())
                .shutdown(std::net::Shutdown::Both);
            drop(conn);
            return;
        }

        // Reduce pool max, THEN drop. `conn` is moved into `resize_then_drop`,
        // making it a compile error to drop conn before resize.
        let cooldowns = self.active_cooldowns.fetch_add(1, Ordering::AcqRel) + 1;
        let new_max = self.original_max_size.saturating_sub(cooldowns).max(1);
        resize_then_drop(&self.pool, conn, new_max);

        warn!(
            pool = %self.name,
            new_max_size = new_max,
            original_max_size = self.original_max_size,
            active_cooldowns = cooldowns,
            cooldown_secs = cooldown.as_secs(),
            "Connection removed, pool size temporarily reduced"
        );

        // Schedule restoration with Drop guard to ensure fetch_sub runs
        let active_cooldowns = self.active_cooldowns.clone();
        let pool = self.pool.clone();
        let original_max = self.original_max_size;
        let name = self.name.clone();
        tokio::spawn(async move {
            // Drop guard ensures fetch_sub runs even if task is cancelled
            struct CooldownGuard {
                active_cooldowns: Arc<std::sync::atomic::AtomicUsize>,
                pool: Pool,
                original_max: usize,
                name: String,
            }
            impl Drop for CooldownGuard {
                fn drop(&mut self) {
                    let remaining = self.active_cooldowns.fetch_sub(1, Ordering::AcqRel) - 1;
                    let restored_max = self.original_max.saturating_sub(remaining);
                    self.pool.resize(restored_max);
                    debug!(
                        pool = %self.name,
                        restored_max_size = restored_max,
                        remaining_cooldowns = remaining,
                        "Connection cooldown expired, pool size restored"
                    );
                }
            }
            let _guard = CooldownGuard {
                active_cooldowns,
                pool,
                original_max,
                name,
            };
            tokio::time::sleep(cooldown).await;
            // _guard drops here, running fetch_sub + resize
        });
    }

    /// Get the maximum pool size
    #[must_use]
    #[inline]
    pub fn max_size(&self) -> usize {
        self.pool.status().max_size
    }

    /// Get the name/identifier of this connection pool
    #[must_use]
    #[inline]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the backend host this pool connects to
    #[must_use]
    #[inline]
    pub fn host(&self) -> &str {
        &self.pool.manager().host
    }

    /// Get the backend port this pool connects to
    #[must_use]
    #[inline]
    pub fn port(&self) -> u16 {
        self.pool.manager().port
    }

    /// Get a reference to the health check metrics
    #[must_use]
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
    /// each cycle. It can be gracefully shut down via the `shutdown_rx` channel.
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
                () = sleep(interval) => {
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
                        // Shut down the raw TCP socket so recycle reliably detects it as dead
                        let _ = socket2::SockRef::from(conn_obj.underlying_tcp_stream())
                            .shutdown(std::net::Shutdown::Both);
                    }
                    // Return to pool normally — deadpool drops it before creating a replacement
                    drop(conn_obj);
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
        timeouts.wait = Some(crate::constants::timeout::SHUTDOWN_POOL_GET);

        for _ in 0..status.available {
            if let Ok(conn_obj) = self.pool.timeout_get(&timeouts).await {
                let mut conn = Object::take(conn_obj);
                // Timeout the write: a half-closed backend connection can block indefinitely
                let _ = tokio::time::timeout(
                    crate::constants::timeout::SHUTDOWN_QUIT_WRITE,
                    conn.write_all(b"QUIT\r\n"),
                )
                .await;
            } else {
                break;
            }
        }

        self.pool.close();
    }
}

/// Resize pool max THEN shut down and drop the connection.
///
/// **This function exists to make the wrong ordering a compile error.**
/// Because `conn` is moved in, the caller cannot `drop(conn)` before `pool.resize()`.
/// Any attempt to reorder operations would produce a use-after-move error.
///
/// The ordering matters: if we dropped first, waiters would see
/// `pool.size < pool.max_size` and immediately create a replacement,
/// defeating the cooldown.
fn resize_then_drop(pool: &Pool, conn: managed::Object<TcpManager>, new_max: usize) {
    pool.resize(new_max);
    let _ = socket2::SockRef::from(conn.underlying_tcp_stream()).shutdown(std::net::Shutdown::Both);
    drop(conn);
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

#[cfg(test)]
#[allow(clippy::float_cmp)] // These tests compare exact derived failure-rate fixtures.
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
        let builder = Builder::new("example.com", 563).tls_config(tls_config);
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
    fn test_provider_inherent_methods() {
        let provider = DeadpoolConnectionProvider::builder("localhost", 119)
            .build()
            .unwrap();

        // Test inherent methods
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

        let status = ConnectionProvider::status(&provider);
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

    /// Helper: spawn a mock NNTP server that greets each connection and keeps it alive.
    /// Returns the address to connect to.
    async fn spawn_mock_nntp_server() -> std::net::SocketAddr {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let (read_half, mut write_half) = stream.into_split();
                    let mut reader = BufReader::new(read_half);

                    let _ = write_half.write_all(b"200 mock\r\n").await;

                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                let cmd = line.trim().to_ascii_uppercase();
                                if cmd == "COMPRESS DEFLATE" {
                                    let _ = write_half.write_all(b"500 Not supported\r\n").await;
                                } else if cmd.starts_with("MODE") {
                                    let _ = write_half.write_all(b"200 Posting allowed\r\n").await;
                                } else if cmd.starts_with("QUIT") {
                                    let _ = write_half.write_all(b"205 Goodbye\r\n").await;
                                    break;
                                } else if cmd.starts_with("DATE") {
                                    let _ = write_half.write_all(b"111 20240101000000\r\n").await;
                                } else {
                                    let _ = write_half.write_all(b"200 OK\r\n").await;
                                }
                            }
                        }
                    }
                });
            }
        });

        addr
    }

    /// Helper: create a provider with cooldown for testing
    fn provider_with_cooldown(
        addr: std::net::SocketAddr,
        max_size: usize,
        cooldown: Option<std::time::Duration>,
    ) -> DeadpoolConnectionProvider {
        let manager = TcpManager::new(
            addr.ip().to_string(),
            addr.port(),
            format!("test-{}", addr.port()),
            TcpManagerOptions {
                compress: Some(false), // Disable compression — mock doesn't handle it
                ..TcpManagerOptions::default()
            },
        )
        .unwrap();

        let pool = Pool::builder(manager).max_size(max_size).build().unwrap();

        DeadpoolConnectionProvider {
            pool,
            name: format!("test-{}", addr.port()),
            shutdown_tx: None,
            health_check_metrics: Arc::new(crate::pool::health_check::HealthCheckMetrics::new()),
            original_max_size: max_size,
            active_cooldowns: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            replacement_cooldown: cooldown,
        }
    }

    /// Verify that `remove_with_cooldown` reduces pool `max_size` BEFORE releasing
    /// the connection. This is the fix for the race where waiters would see
    /// size < `max_size` and create a replacement connection.
    #[tokio::test]
    async fn test_remove_with_cooldown_resize_before_drop() {
        let addr = spawn_mock_nntp_server().await;
        let cooldown = std::time::Duration::from_secs(10);
        let max_size = 4;
        let provider = provider_with_cooldown(addr, max_size, Some(cooldown));

        let conn = provider.get_pooled_connection().await.unwrap();
        assert_eq!(provider.pool.status().max_size, max_size);

        // Remove with cooldown — this should reduce max_size BEFORE dropping conn
        provider.remove_with_cooldown(conn);

        // After remove_with_cooldown, max_size must be reduced immediately
        let status = provider.pool.status();
        assert_eq!(
            status.max_size,
            max_size - 1,
            "Pool max_size should be reduced to {} after remove_with_cooldown, got {}",
            max_size - 1,
            status.max_size
        );
        assert_eq!(provider.active_cooldowns.load(Ordering::Acquire), 1);
    }

    /// Verify the cooldown cap: never reduce below half original size
    #[tokio::test]
    async fn test_remove_with_cooldown_cap() {
        let addr = spawn_mock_nntp_server().await;
        let cooldown = std::time::Duration::from_secs(10);
        let max_size = 4;
        let provider = provider_with_cooldown(addr, max_size, Some(cooldown));

        // max_reduction = 4 / 2 = 2, so after 2 cooldowns it should stop reducing
        let conn1 = provider.get_pooled_connection().await.unwrap();
        let conn2 = provider.get_pooled_connection().await.unwrap();
        let conn3 = provider.get_pooled_connection().await.unwrap();

        provider.remove_with_cooldown(conn1);
        assert_eq!(provider.pool.status().max_size, 3);

        provider.remove_with_cooldown(conn2);
        assert_eq!(provider.pool.status().max_size, 2);

        // Third removal should NOT further reduce (at cap)
        provider.remove_with_cooldown(conn3);
        assert_eq!(
            provider.pool.status().max_size,
            2,
            "Pool max_size should not drop below half (2) of original (4)"
        );
    }

    /// Verify no cooldown when cooldown is disabled (None)
    #[tokio::test]
    async fn test_remove_with_cooldown_disabled() {
        let addr = spawn_mock_nntp_server().await;
        let max_size = 4;
        let provider = provider_with_cooldown(addr, max_size, None);

        let conn = provider.get_pooled_connection().await.unwrap();
        provider.remove_with_cooldown(conn);

        // With cooldown disabled, max_size should NOT be reduced
        assert_eq!(provider.pool.status().max_size, max_size);
        assert_eq!(provider.active_cooldowns.load(Ordering::Acquire), 0);
    }

    /// Verify cooldown restoration after timer expires.
    ///
    /// Note: Cannot use `start_paused = true` here because the mock NNTP server
    /// uses real TCP I/O which doesn't work with paused time (auto-advance
    /// would resolve the Notify/sleep before the TCP handshake completes).
    /// Instead we use a very short real cooldown.
    #[tokio::test]
    async fn test_remove_with_cooldown_restores_after_timer() {
        let addr = spawn_mock_nntp_server().await;
        let cooldown = std::time::Duration::from_millis(100);
        let max_size = 4;
        let provider = provider_with_cooldown(addr, max_size, Some(cooldown));

        let conn = provider.get_pooled_connection().await.unwrap();
        provider.remove_with_cooldown(conn);

        assert_eq!(provider.pool.status().max_size, 3);
        assert_eq!(provider.active_cooldowns.load(Ordering::Acquire), 1);

        // Wait for the cooldown timer to restore the pool
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        assert_eq!(
            provider.pool.status().max_size,
            max_size,
            "Pool max_size should be restored after cooldown expires"
        );
        assert_eq!(provider.active_cooldowns.load(Ordering::Acquire), 0);
    }
}
