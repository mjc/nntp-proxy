//! Connection pool provider implementation
//!
//! This module contains the `DeadpoolConnectionProvider` which manages a pool of
//! NNTP connections using the deadpool library. It provides:
//! - Connection pooling with configurable size
//! - Automatic connection recycling
//! - Periodic health checks for idle connections
//! - Graceful shutdown with QUIT commands

use super::connection_trait::ConnectionProvider;
use super::deadpool_connection::{Pool, PooledConnection, TcpManager, TcpManagerOptions};
use super::health_check::{HealthCheckMetrics, check_date_response};
use crate::pool::PoolStatus;
use crate::tls::TlsConfig;
use anyhow::Result;
use deadpool::managed;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatMissingPolicy {
    Disabled,
    Probe,
}

impl StatMissingPolicy {
    const fn from_enabled(enabled: bool) -> Self {
        if enabled { Self::Probe } else { Self::Disabled }
    }

    const fn is_enabled(self) -> bool {
        matches!(self, Self::Probe)
    }
}

impl From<bool> for StatMissingPolicy {
    fn from(enabled: bool) -> Self {
        Self::from_enabled(enabled)
    }
}

/// Connection provider using deadpool for connection pooling
#[derive(Debug, Clone)]
pub struct DeadpoolConnectionProvider {
    pool: Pool,
    name: Arc<str>,
    stat_missing: StatMissingPolicy,
    /// Shutdown signal sender for background health check task.
    /// Stored to keep the channel alive - when dropped, the background task will terminate.
    /// Used by `shutdown()` method to gracefully stop health checks.
    shutdown_tx: Option<broadcast::Sender<()>>,
    /// Metrics for health check operations (lock-free)
    pub health_check_metrics: Arc<HealthCheckMetrics>,
    /// Connections the pool may hold before cooldowns: the configured
    /// `max_connections`, or the adaptive cap that follows login refusals.
    cap: Arc<super::connection_cap::ConnectionCap>,
    /// Stops the adaptive cap's ticker; `None` for a fixed cap.
    cap_shutdown_tx: Option<broadcast::Sender<()>>,
    /// Number of active cooldown timers reducing pool size
    active_cooldowns: Arc<std::sync::atomic::AtomicUsize>,
    /// Connection replacement cooldown duration (None = disabled)
    replacement_cooldown: Option<std::time::Duration>,
    /// Set once process shutdown begins so teardown doesn't look like backend damage.
    is_shutting_down: Arc<std::sync::atomic::AtomicBool>,
    /// Service-time estimate and busy counters shared by every clone.
    load: Arc<super::admission::BackendLoad>,
}

/// One adaptive-cap evaluation: grow the cap when its climb interval has
/// passed, and resize the pool to match.
fn tick_connection_cap(
    pool: &Pool,
    cap: &super::connection_cap::ConnectionCap,
    active_cooldowns: &std::sync::atomic::AtomicUsize,
    name: &str,
    now: tokio::time::Instant,
) {
    use super::connection_cap::{CapChange, PoolDemand};
    // No growth while logins are paused: nothing could test it.
    if pool.manager().login_backoff_active() {
        return;
    }
    let status = pool.status();
    let demand = if status.size >= status.max_size && status.available == 0 {
        PoolDemand::Saturated
    } else {
        PoolDemand::Headroom
    };
    let Some(CapChange::Raised { from, to }) = cap.on_tick(demand, now) else {
        return;
    };
    sync_pool_size(pool, cap, active_cooldowns);
    if to > cap.configured() {
        info!(
            pool = %name,
            from,
            to,
            configured = cap.configured(),
            "Connection cap probing past max_connections while the pool is saturated"
        );
    } else {
        info!(
            pool = %name,
            from,
            to,
            configured = cap.configured(),
            "Connection cap climbing back after a refused login"
        );
    }
}

/// Resize the pool to what the cap and cooldowns allow right now. Every
/// resize goes through here: the size is read and applied under the cap's
/// resize lock, so a size computed from an older cap (a ticker raise racing a
/// refusal) cannot overwrite a newer one.
fn sync_pool_size(
    pool: &Pool,
    cap: &super::connection_cap::ConnectionCap,
    active_cooldowns: &std::sync::atomic::AtomicUsize,
) -> usize {
    let _resizes = cap.lock_resizes();
    let size = effective_max_size(cap, active_cooldowns);
    pool.resize(size);
    size
}

/// Pool size an adaptive or fixed cap allows after replacement cooldowns,
/// never below one so the backend can still be probed.
fn effective_max_size(
    cap: &super::connection_cap::ConnectionCap,
    active_cooldowns: &std::sync::atomic::AtomicUsize,
) -> usize {
    cap.current()
        .saturating_sub(active_cooldowns.load(Ordering::Acquire))
        .max(1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeadpoolStatusCounts {
    pub available: usize,
    pub size: usize,
    pub max_size: usize,
    pub waiting: usize,
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
    stat_missing: StatMissingPolicy,
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
            stat_missing: StatMissingPolicy::Disabled,
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

    /// Enable/disable `STAT` pre-probing on article-missing retries.
    #[must_use]
    pub const fn stat_missing(mut self, stat_missing: bool) -> Self {
        self.stat_missing = StatMissingPolicy::from_enabled(stat_missing);
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
            self.stat_missing,
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
            StatMissingPolicy::Disabled,
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
        Ok(Self::from_manager(
            manager,
            name,
            max_size,
            StatMissingPolicy::Disabled,
        ))
    }

    /// Construct a provider from a pre-built `TcpManager`
    fn from_manager(
        manager: TcpManager,
        name: String,
        max_size: usize,
        stat_missing: StatMissingPolicy,
    ) -> Self {
        let pool = super::deadpool_connection::configured_pool(manager, max_size);

        Self {
            pool,
            name: Arc::from(name),
            stat_missing,
            shutdown_tx: None,
            health_check_metrics: Arc::new(HealthCheckMetrics::new()),
            cap: Arc::new(super::connection_cap::ConnectionCap::new(
                super::connection_cap::ConnectionCapPolicy::Fixed,
                max_size,
            )),
            cap_shutdown_tx: None,
            active_cooldowns: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            replacement_cooldown: None, // Default: no cooldown
            is_shutting_down: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            load: Arc::default(),
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
        let pool = super::deadpool_connection::configured_pool(manager, max_size);

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

        let cap = Arc::new(super::connection_cap::ConnectionCap::new(
            server.connection_cap,
            max_size,
        ));
        let active_cooldowns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let cap_shutdown_tx = (server.connection_cap
            == super::connection_cap::ConnectionCapPolicy::Adaptive)
            .then(|| {
                let (tx, rx) = broadcast::channel(1);
                tokio::spawn(Self::run_connection_cap_ticker(
                    pool.clone(),
                    Arc::clone(&cap),
                    Arc::clone(&active_cooldowns),
                    Arc::from(server.name.to_string()),
                    rx,
                ));
                tx
            });

        Ok(Self {
            pool,
            name: Arc::from(server.name.to_string()),
            stat_missing: server.stat_missing_enabled().into(),
            shutdown_tx,
            health_check_metrics: metrics,
            cap,
            cap_shutdown_tx,
            active_cooldowns,
            replacement_cooldown: server.replacement_cooldown,
            is_shutting_down: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            load: Arc::default(),
        })
    }

    /// Get a connection from the pool (automatically returned when dropped)
    ///
    /// # Errors
    /// Returns [`crate::connection_error::ConnectionError`] if deadpool cannot
    /// provide a healthy backend connection.
    pub(in crate::pool) async fn get_pooled_connection(
        &self,
    ) -> Result<PooledConnection, crate::connection_error::ConnectionError> {
        self.get_pooled_connection_with(&self.pool.timeouts())
            .await
            .inspect_err(|err| self.log_acquisition_failure(err))
    }

    fn log_acquisition_failure(&self, err: &crate::connection_error::ConnectionError) {
        // A backend in login backoff fails every checkout by design until its
        // window closes; the refusal that opened the window was already logged.
        if matches!(
            err,
            crate::connection_error::ConnectionError::LoginBackoff { .. }
        ) {
            debug!(pool = %self.name, error = %err, "Connection acquisition skipped");
            return;
        }
        let status = self.pool.status();
        warn!(
            pool = %self.name,
            max_size = status.max_size,
            available = status.available,
            current_size = status.size,
            waiting = status.waiting,
            error = %err,
            "Connection acquisition failed"
        );
    }

    async fn get_pooled_connection_with(
        &self,
        timeouts: &managed::Timeouts,
    ) -> Result<PooledConnection, crate::connection_error::ConnectionError> {
        use crate::connection_error::ConnectionError;

        // Deadpool's wait timer ends when a slot is taken, and its create
        // timer starts then. A checkout that never takes a slot is an empty
        // pool. A setup that starts after the wait, including one that takes
        // the slot as the wait timer elapses, keeps the setup timeout.
        let err = match self.pool.timeout_get(timeouts).await {
            Ok(connection) => return Ok(connection),
            Err(error) => match error {
                deadpool::managed::PoolError::Backend(conn_err) => {
                    if conn_err.is_login_refusal() {
                        self.note_login_refused();
                    }
                    conn_err
                }
                deadpool::managed::PoolError::Timeout(managed::TimeoutType::Wait) => {
                    ConnectionError::PoolExhausted {
                        backend: self.name.to_string(),
                        max_size: self.pool.status().max_size,
                    }
                }
                deadpool::managed::PoolError::Timeout(
                    managed::TimeoutType::Create | managed::TimeoutType::Recycle,
                ) => ConnectionError::IoError(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("Backend '{}' connection setup timed out", self.name),
                )),
                deadpool::managed::PoolError::Closed => ConnectionError::PoolClosed {
                    backend: self.name.to_string(),
                },
                deadpool::managed::PoolError::NoRuntimeSpecified => {
                    ConnectionError::IoError(std::io::Error::other(format!(
                        "Backend '{}' pool has no async runtime",
                        self.name
                    )))
                }
                deadpool::managed::PoolError::PostCreateHook(hook) => match hook {
                    deadpool::managed::HookError::Backend(conn_err) => conn_err,
                    deadpool::managed::HookError::Message(message) => {
                        ConnectionError::IoError(std::io::Error::other(message.to_string()))
                    }
                },
            },
        };
        Err(err)
    }

    /// Checkout like [`Self::checkout_connection_guard`], but give up after
    /// `limit` with [`crate::connection_error::ConnectionError::PoolExhausted`].
    ///
    /// # Errors
    /// Returns [`crate::connection_error::ConnectionError`] when no healthy backend
    /// connection can be acquired within `limit`.
    pub async fn checkout_connection_guard_within(
        &self,
        limit: std::time::Duration,
    ) -> Result<
        crate::pool::connection_guard::IdleConnection,
        crate::connection_error::ConnectionError,
    > {
        self.checkout_for_attempt(super::admission::CheckoutBudget::new(
            limit,
            super::admission::BacklogPolicy::Queue,
        ))
        .await
    }

    /// Checkout for a routed attempt that can move on to another backend.
    ///
    /// When the budget allows it and the pool's queue is too long to serve
    /// this checkout within its wait, fail at once with
    /// [`crate::connection_error::ConnectionError::Backlogged`] instead of
    /// joining the queue. Otherwise wait at most `budget.wait()` for a slot.
    ///
    /// Only deadpool's wait for a slot is shortened: once a slot is taken,
    /// connection setup and recycling keep their own deadlines. Cancelling the
    /// whole checkout instead would abort a TLS handshake or authentication
    /// part-way and waste the backend connection it was building.
    ///
    /// Both busy outcomes are expected under load and the caller routes
    /// around them, so they are counted and summarised per backend instead of
    /// logged per request.
    ///
    /// # Errors
    /// Returns [`crate::connection_error::ConnectionError`] when the backend is
    /// skipped as backlogged or no healthy connection is acquired in time.
    pub async fn checkout_for_attempt(
        &self,
        budget: super::admission::CheckoutBudget,
    ) -> Result<
        crate::pool::connection_guard::IdleConnection,
        crate::connection_error::ConnectionError,
    > {
        use crate::connection_error::ConnectionError;

        let last_resort = budget.backlog() == super::admission::BacklogPolicy::LastResort;
        if last_resort {
            self.load.note_last_resort_wait();
        }
        if let super::admission::Admission::Skip { estimated_wait } = self.admission(budget) {
            if let Some(summary) = self.load.note_backlog_skip() {
                self.log_skip_summary(summary);
            }
            return Err(ConnectionError::Backlogged {
                backend: Arc::clone(&self.name),
                estimated_wait,
            });
        }

        let timeouts = managed::Timeouts {
            wait: Some(budget.wait()),
            ..self.pool.timeouts()
        };
        match self.get_pooled_connection_with(&timeouts).await {
            Ok(conn) => {
                if last_resort {
                    self.load.note_last_resort_connected();
                }
                Ok(crate::pool::connection_guard::IdleConnection::new(
                    conn,
                    self.clone(),
                ))
            }
            Err(err @ ConnectionError::PoolExhausted { .. }) => {
                if last_resort {
                    self.load.note_last_resort_timeout();
                }
                if let Some(summary) = self.load.note_busy_timeout() {
                    self.log_skip_summary(summary);
                }
                Err(err)
            }
            Err(err) => {
                self.log_acquisition_failure(&err);
                Err(err)
            }
        }
    }

    fn admission(&self, budget: super::admission::CheckoutBudget) -> super::admission::Admission {
        if matches!(
            budget.backlog(),
            super::admission::BacklogPolicy::Queue | super::admission::BacklogPolicy::LastResort
        ) {
            return super::admission::Admission::Queue;
        }
        // Without a sample nothing can be skipped; avoid reading pool status.
        let Some(service_time) = self.load.service_time() else {
            return super::admission::Admission::Queue;
        };
        super::admission::admit(
            super::admission::estimate_wait(self.status_counts(), Some(service_time)),
            budget,
        )
    }

    /// Info, not warn: skipping a busy backend is load shedding working as
    /// intended, and the request was routed elsewhere or answered. Operators
    /// still see at the default level which backends are undersized.
    fn log_skip_summary(&self, summary: super::admission::SkipSummary) {
        let status = self.status_counts();
        let service_time = self.load.service_time();
        let estimated_wait = super::admission::estimate_wait(status, service_time);
        info!(
            pool = %self.name,
            backlog_skips = summary.backlog_skips,
            busy_timeouts = summary.busy_timeouts,
            service_time_ms = service_time.unwrap_or_default().as_millis(),
            estimated_wait_ms = estimated_wait.as_millis(),
            max_size = status.max_size,
            available = status.available,
            waiting = status.waiting,
            interval_secs = super::admission::SKIP_SUMMARY_INTERVAL.as_secs(),
            "Backend busy; requests skipped it or timed out waiting for a connection"
        );
    }

    /// How long a checkout joining this pool's queue now would wait, for
    /// choosing the least backlogged of several busy backends.
    #[must_use]
    pub fn estimated_wait(&self) -> std::time::Duration {
        super::admission::estimate_wait(self.status_counts(), self.load.service_time())
    }

    /// Record how long one command held a connection from this pool.
    pub(crate) fn record_service_time(&self, held: std::time::Duration) {
        self.load.record_service_time(held);
    }

    /// Count a request whose deadline ran out with this backend next in line.
    pub fn note_deadline_expiry(&self) {
        self.load.note_deadline_expiry();
    }

    /// Fold a lookup sent after another backend answered `430` into this
    /// backend's rescue rate.
    pub fn note_fallback_lookup(&self, outcome: super::admission::FallbackOutcome) {
        self.load.note_fallback_lookup(outcome);
    }

    /// Fold an article lookup's answer into this backend's standing.
    pub fn note_article_lookup(&self, outcome: super::admission::ArticleLookupOutcome) {
        self.load.note_article_lookup(outcome);
    }

    /// Recent article-lookup standing in parts per million.
    #[must_use]
    pub fn article_standing_ppm(&self) -> u32 {
        self.load.standing_ppm()
    }

    /// Delivery rate, first-byte time, client stalls and leased connections.
    #[must_use]
    pub fn delivery(&self) -> &super::delivery::DeliveryStats {
        self.load.delivery()
    }

    /// Count a client lease holding one of this backend's connections idle
    /// until the returned token drops.
    #[must_use]
    pub fn idle_lease(&self) -> super::delivery::IdleLease {
        super::delivery::IdleLease::new(Arc::clone(&self.load))
    }

    /// Whether `admission` says this backend is not worth waiting on when busy.
    #[must_use]
    pub fn is_unlikely_to_rescue(&self, admission: &super::admission::ArticleAdmission) -> bool {
        admission.is_unlikely_to_rescue(&self.load)
    }

    /// Whether a pass-over of this unlikely rescuer should be let through as a
    /// probe instead.
    pub fn take_rescue_probe(&self) -> bool {
        self.load.take_rescue_probe()
    }

    /// Shared admission counters for this backend, for the stats file.
    #[must_use]
    pub fn load(&self) -> Arc<super::admission::BackendLoad> {
        Arc::clone(&self.load)
    }

    /// Checkout a pooled connection in the idle state.
    ///
    /// Callers outside `crate::pool` should use this entrypoint instead of accessing
    /// raw pooled objects directly.
    ///
    /// # Errors
    /// Returns [`crate::connection_error::ConnectionError`] when no healthy backend
    /// connection can be acquired.
    pub async fn checkout_connection_guard(
        &self,
    ) -> Result<
        crate::pool::connection_guard::IdleConnection,
        crate::connection_error::ConnectionError,
    > {
        let conn = self.get_pooled_connection().await?;
        Ok(crate::pool::connection_guard::IdleConnection::new(
            conn,
            self.clone(),
        ))
    }

    #[must_use]
    pub const fn stat_missing_enabled(&self) -> bool {
        self.stat_missing.is_enabled()
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

            let _resizes = self.cap.lock_resizes();
            let target_max = self.effective_max_size();

            // Resize to 0 drops all idle connections
            self.pool.resize(0);
            // Resize back to target allows new connections
            self.pool.resize(target_max);
        }
    }

    /// Remove a connection without temporarily reducing pool size.
    ///
    /// Use this when the connection cannot be safely returned to the pool, but
    /// the backend did not fail. For example, a client disconnect can leave
    /// unread backend response bytes in flight, making the socket dirty without
    /// implying that replacement connections should be throttled.
    pub(super) fn remove_without_cooldown(&self, mut conn: PooledConnection) {
        conn.tag_close_reason(super::churn::CloseReason::ClientAbort);
        shutdown_and_drop(conn);
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
    pub(super) fn remove_with_cooldown(
        &self,
        mut conn: PooledConnection,
        reason: super::churn::CloseReason,
    ) {
        conn.tag_close_reason(reason);
        if self.is_shutting_down.load(Ordering::Acquire) {
            shutdown_and_drop(conn);
            return;
        }

        // If cooldown is disabled (None or zero duration), just shut down and drop
        let Some(cooldown) = self.replacement_cooldown.filter(|d| !d.is_zero()) else {
            shutdown_and_drop(conn);
            return;
        };

        // Cap cooldowns to prevent pool collapse: never reduce below half original size.
        // If >50% of connections fail simultaneously, it's a systemic backend issue
        // (restart, network partition). Continuing to reduce would starve all clients.
        // Better to keep half the pool and let failed connections return errors.
        let max_reduction = self.cap.current() / 2;
        let current = self.active_cooldowns.load(Ordering::Acquire);
        if current >= max_reduction {
            // Already at max reduction — just shut down and drop without further cooldown
            shutdown_and_drop(conn);
            return;
        }

        // Reduce pool max, THEN drop. `conn` is moved into `resize_then_drop`,
        // making it a compile error to drop conn before resize.
        let cooldowns = self.active_cooldowns.fetch_add(1, Ordering::AcqRel) + 1;
        let new_max = resize_then_drop(&self.pool, &self.cap, &self.active_cooldowns, conn);

        warn!(
            pool = %self.name,
            new_max_size = new_max,
            connection_cap = self.cap.current(),
            active_cooldowns = cooldowns,
            cooldown_secs = cooldown.as_secs(),
            "Connection removed, pool size temporarily reduced"
        );

        // Schedule restoration with Drop guard to ensure fetch_sub runs
        let active_cooldowns = self.active_cooldowns.clone();
        let pool = self.pool.clone();
        let cap = Arc::clone(&self.cap);
        let name = self.name.clone();
        tokio::spawn(async move {
            // Drop guard ensures fetch_sub runs even if task is cancelled
            struct CooldownGuard {
                active_cooldowns: Arc<std::sync::atomic::AtomicUsize>,
                pool: Pool,
                cap: Arc<super::connection_cap::ConnectionCap>,
                name: Arc<str>,
            }
            impl Drop for CooldownGuard {
                fn drop(&mut self) {
                    let remaining = self.active_cooldowns.fetch_sub(1, Ordering::AcqRel) - 1;
                    let restored_max =
                        sync_pool_size(&self.pool, &self.cap, &self.active_cooldowns);
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
                cap,
                name,
            };
            tokio::time::sleep(cooldown).await;
            // _guard drops here, running fetch_sub + resize
        });
    }

    /// Pool size the cap and current cooldowns allow.
    fn effective_max_size(&self) -> usize {
        effective_max_size(&self.cap, &self.active_cooldowns)
    }

    /// Connections this backend may hold now, before replacement cooldowns.
    /// Equals `max_connections` unless `connection_cap = "adaptive"` moved it.
    #[must_use]
    pub fn connection_cap(&self) -> usize {
        self.cap.current()
    }

    /// Times a refused login lowered the adaptive cap.
    #[must_use]
    pub fn connection_cap_reductions(&self) -> u64 {
        self.cap.reductions()
    }

    /// A login was refused: with an adaptive cap, shrink the pool to the
    /// connections the provider had accepted.
    fn note_login_refused(&self) {
        let open = self.pool.status().size;
        if let Some(super::connection_cap::CapChange::Lowered { from, to }) =
            self.cap.on_login_refused(open, tokio::time::Instant::now())
        {
            sync_pool_size(&self.pool, &self.cap, &self.active_cooldowns);
            warn!(
                pool = %self.name,
                from,
                to,
                configured = self.cap.configured(),
                "Backend refused a login; lowering its connection cap"
            );
        }
    }

    /// Grow an adaptive cap back as time passes without refusals. See
    /// [`super::connection_cap`] for the schedule.
    async fn run_connection_cap_ticker(
        pool: Pool,
        cap: Arc<super::connection_cap::ConnectionCap>,
        active_cooldowns: Arc<std::sync::atomic::AtomicUsize>,
        name: Arc<str>,
        mut shutdown_rx: broadcast::Receiver<()>,
    ) {
        loop {
            tokio::select! {
                () = tokio::time::sleep(super::connection_cap::TICK) => {}
                _ = shutdown_rx.recv() => break,
            }
            tick_connection_cap(
                &pool,
                &cap,
                &active_cooldowns,
                &name,
                tokio::time::Instant::now(),
            );
        }
    }

    /// Run one cap evaluation as of `now`, as the ticker does.
    #[cfg(test)]
    fn tick_connection_cap_at(&self, now: tokio::time::Instant) {
        tick_connection_cap(
            &self.pool,
            &self.cap,
            &self.active_cooldowns,
            &self.name,
            now,
        );
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
        self.name.as_ref()
    }

    #[must_use]
    pub fn status_counts(&self) -> DeadpoolStatusCounts {
        let status = self.pool.status();
        DeadpoolStatusCounts {
            available: status.available,
            size: status.size,
            max_size: status.max_size,
            waiting: status.waiting,
        }
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

    /// Connection churn counters for this backend.
    #[must_use]
    pub fn connection_churn(&self) -> super::churn::ConnectionChurnSnapshot {
        self.pool.manager().churn.snapshot()
    }

    /// Whether login backoff is currently keeping new connections away.
    #[must_use]
    pub fn login_backoff_active(&self) -> bool {
        self.pool.manager().login_backoff_active()
    }

    /// Pool slots temporarily withheld after broken connections.
    #[must_use]
    pub fn active_cooldowns(&self) -> usize {
        self.active_cooldowns.load(Ordering::Relaxed)
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
        self.is_shutting_down.store(true, Ordering::Release);
        if let Some(tx) = &self.shutdown_tx {
            let _ = tx.send(());
        }
        if let Some(tx) = &self.cap_shutdown_tx {
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
                    if let Err(e) = check_date_response(&mut *conn_obj).await {
                        failed += 1;
                        conn_obj.tag_close_reason(super::churn::CloseReason::HealthCheck);
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

        self.shutdown();

        let status = self.pool.status();
        info!(
            "Shutting down pool '{}' ({} idle connections)",
            self.name, status.available
        );

        // Send QUIT to idle connections with minimal timeout
        let mut timeouts = managed::Timeouts::new();
        timeouts.wait = Some(crate::constants::timeout::SHUTDOWN_POOL_GET);

        let mut idle_connections = Vec::with_capacity(status.available);
        for _ in 0..status.available {
            if let Ok(conn_obj) = self.pool.timeout_get(&timeouts).await {
                idle_connections.push(Object::take(conn_obj));
            } else {
                break;
            }
        }

        shutdown_connections_concurrently(idle_connections, |mut conn| async move {
            // Timeout the write: a half-closed backend connection can block indefinitely
            let request = crate::protocol::RequestContext::from_verb_args(b"QUIT", b"");
            let _ = tokio::time::timeout(
                crate::constants::timeout::SHUTDOWN_QUIT_WRITE,
                request.send_to(&mut conn),
            )
            .await;
        })
        .await;

        self.pool.close();
    }
}

async fn shutdown_connections_concurrently<T, F, Fut>(connections: Vec<T>, shutdown_one: F)
where
    F: FnMut(T) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut pending = connections
        .into_iter()
        .map(shutdown_one)
        .collect::<FuturesUnordered<_>>();

    while pending.next().await.is_some() {}
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
/// Shrink the pool for a new cooldown, then retire `conn`. Returns the new
/// pool size.
fn resize_then_drop(
    pool: &Pool,
    cap: &super::connection_cap::ConnectionCap,
    active_cooldowns: &std::sync::atomic::AtomicUsize,
    conn: PooledConnection,
) -> usize {
    let new_max = sync_pool_size(pool, cap, active_cooldowns);
    shutdown_and_drop(conn);
    new_max
}

fn shutdown_and_drop(conn: PooledConnection) {
    let _ = socket2::SockRef::from(conn.underlying_tcp_stream()).shutdown(std::net::Shutdown::Both);
    // `Object::drop` returns the object to deadpool. Take it first so a retired
    // socket cannot re-enter the idle pool after its file descriptor is closed.
    drop(deadpool::managed::Object::take(conn));
}

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
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

    /// Mock provider that accepts the first `accept` logins and answers every
    /// later `AUTHINFO PASS` with a connection-limit refusal.
    async fn spawn_limited_login_server(accept: usize) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let logins = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let logins = Arc::clone(&logins);
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut lines = tokio::io::BufReader::new(read).lines();
                    write.write_all(b"200 ready\r\n").await.unwrap();
                    let mut index = 0;
                    while let Ok(Some(_line)) = lines.next_line().await {
                        let reply: &[u8] = match index {
                            0 => b"381 password required\r\n",
                            1 if logins.fetch_add(1, Ordering::SeqCst) < accept => b"281 ok\r\n",
                            1 => b"502 Too many connections\r\n",
                            _ => b"200 ok\r\n",
                        };
                        index += 1;
                        if write.write_all(reply).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        addr
    }

    fn capped_provider(
        addr: std::net::SocketAddr,
        max: usize,
        policy: crate::pool::connection_cap::ConnectionCapPolicy,
    ) -> DeadpoolConnectionProvider {
        let server = crate::config::Server::builder(
            addr.ip().to_string(),
            crate::types::Port::try_new(addr.port()).unwrap(),
        )
        .name("capped")
        .username("user")
        .password("pass")
        .max_connections(crate::types::MaxConnections::try_new(max).unwrap())
        .compress(Some(false))
        .connection_cap(policy)
        .build()
        .unwrap();
        DeadpoolConnectionProvider::from_server_config(&server, 0, 0).unwrap()
    }

    #[tokio::test]
    async fn adaptive_cap_drops_the_pool_to_the_accepted_count_on_refusal() {
        use crate::pool::connection_cap::ConnectionCapPolicy;
        let addr = spawn_limited_login_server(2).await;
        let provider = capped_provider(addr, 4, ConnectionCapPolicy::Adaptive);

        let first = provider.checkout_connection_guard().await.unwrap();
        let second = provider.checkout_connection_guard().await.unwrap();
        let third = provider.checkout_connection_guard().await;
        assert!(third.is_err(), "the third login is refused");

        assert_eq!(provider.connection_cap(), 2);
        assert_eq!(provider.connection_cap_reductions(), 1);
        assert_eq!(
            provider.max_size(),
            2,
            "the pool itself shrinks, so routing and waits see the real limit"
        );

        // While the refusal's login backoff runs, the cap does not climb.
        let after_a_minute =
            tokio::time::Instant::now() + crate::pool::connection_cap::CLIMB_BELOW_CONFIGURED;
        provider.tick_connection_cap_at(after_a_minute);
        assert_eq!(provider.connection_cap(), 2);

        drop((first, second));
        provider.shutdown();
    }

    #[tokio::test]
    async fn adaptive_cap_climbs_back_and_resizes_the_pool() {
        use crate::pool::connection_cap::{CLIMB_BELOW_CONFIGURED, ConnectionCapPolicy};
        let addr = spawn_limited_login_server(usize::MAX).await;
        let provider = capped_provider(addr, 4, ConnectionCapPolicy::Adaptive);
        // Lower the cap as a refusal with one connection open would.
        let refused_at = tokio::time::Instant::now();
        provider.cap.on_login_refused(1, refused_at);
        sync_pool_size(&provider.pool, &provider.cap, &provider.active_cooldowns);
        assert_eq!(provider.max_size(), 1);

        provider.tick_connection_cap_at(refused_at + CLIMB_BELOW_CONFIGURED);
        assert_eq!(provider.connection_cap(), 2);
        assert_eq!(provider.max_size(), 2, "the pool grows with the cap");

        provider.tick_connection_cap_at(refused_at + CLIMB_BELOW_CONFIGURED * 2);
        provider.tick_connection_cap_at(refused_at + CLIMB_BELOW_CONFIGURED * 3);
        assert_eq!(provider.max_size(), 4);
        // At max_connections an idle pool does not creep past it.
        provider.tick_connection_cap_at(refused_at + CLIMB_BELOW_CONFIGURED * 30);
        assert_eq!(provider.max_size(), 4);
        provider.shutdown();
    }

    /// A climb and a refusal racing on different threads must leave the pool
    /// at the size the final cap allows, never at a size computed from a cap
    /// that has since moved.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_cap_changes_leave_the_pool_at_the_latest_cap() {
        use crate::pool::connection_cap::{CLIMB_ABOVE_CONFIGURED, ConnectionCapPolicy};
        let addr = spawn_limited_login_server(usize::MAX).await;
        let provider = capped_provider(addr, 64, ConnectionCapPolicy::Adaptive);
        let start = tokio::time::Instant::now();
        for round in 0..200u32 {
            let climber = provider.clone();
            let refuser = provider.clone();
            let at = start + CLIMB_ABOVE_CONFIGURED * (round + 1);
            let climb = std::thread::spawn(move || climber.tick_connection_cap_at(at));
            let refuse = std::thread::spawn(move || {
                let open = 1 + (round as usize % 63);
                if let Some(_change) = refuser.cap.on_login_refused(open, at) {
                    sync_pool_size(&refuser.pool, &refuser.cap, &refuser.active_cooldowns);
                }
            });
            climb.join().unwrap();
            refuse.join().unwrap();
            assert_eq!(
                provider.max_size(),
                provider.effective_max_size(),
                "round {round}: pool size drifted from the cap"
            );
        }
        provider.shutdown();
    }

    #[tokio::test]
    async fn fixed_cap_keeps_the_configured_pool_size_after_a_refusal() {
        use crate::pool::connection_cap::ConnectionCapPolicy;
        let addr = spawn_limited_login_server(2).await;
        let provider = capped_provider(addr, 4, ConnectionCapPolicy::Fixed);

        let first = provider.checkout_connection_guard().await.unwrap();
        let second = provider.checkout_connection_guard().await.unwrap();
        assert!(provider.checkout_connection_guard().await.is_err());

        assert_eq!(provider.connection_cap(), 4);
        assert_eq!(provider.max_size(), 4);
        drop((first, second));
    }

    fn assert_setup_deadline(
        result: &Result<
            crate::pool::connection_guard::IdleConnection,
            crate::connection_error::ConnectionError,
        >,
    ) {
        assert!(
            matches!(
                result,
                Err(crate::connection_error::ConnectionError::IoError(error))
                    if error.kind() == std::io::ErrorKind::TimedOut
                        && error.to_string().contains("connection setup timed out")
            ),
            "setup did not surface its deadline: {}",
            match result {
                Ok(_) => "checkout returned a connection".to_string(),
                Err(error) => error.to_string(),
            }
        );
    }

    async fn arm_setup_deadline<T>(task: &tokio::task::JoinHandle<T>) {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(!task.is_finished(), "setup finished before its deadline");
        tokio::time::advance(
            crate::constants::timeout::CONNECTION + std::time::Duration::from_millis(1),
        )
        .await;
        for _ in 0..16 {
            if task.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            task.is_finished(),
            "setup was not polled through its deadline"
        );
    }

    fn hold_socket(socket: tokio::net::TcpStream) {
        tokio::spawn(async move {
            let _held = socket;
            std::future::pending::<()>().await;
        });
    }

    #[tokio::test]
    async fn closed_pool_is_not_an_empty_pool() {
        let provider = DeadpoolConnectionProvider::builder("127.0.0.1", 1)
            .max_connections(1)
            .build()
            .unwrap();
        provider.pool.close();

        let error = provider.get_pooled_connection().await.unwrap_err();
        assert!(
            matches!(
                error,
                crate::connection_error::ConnectionError::PoolClosed { .. }
            ),
            "closed pool was reported as {error}"
        );
    }

    #[tokio::test]
    async fn saturated_pool_acquisition_has_a_deadline() {
        let addr = spawn_mock_nntp_server().await;
        let provider = provider_with_cooldown(addr, 1, None);
        let held = provider.get_pooled_connection().await.unwrap();
        tokio::time::pause();
        let result = tokio::time::timeout(
            crate::constants::timeout::CONNECTION * 2,
            provider.get_pooled_connection(),
        )
        .await
        .expect("pool acquisition did not enforce its deadline");
        let error = result.expect_err("a saturated pool must not return a connection");
        assert!(
            matches!(
                error,
                crate::connection_error::ConnectionError::PoolExhausted { max_size: 1, .. }
            ),
            "a wait for the only slot was reported as {error}"
        );
        drop(held);
        tokio::time::resume();
        assert!(provider.get_pooled_connection().await.is_ok());
    }

    #[tokio::test]
    async fn bounded_checkout_wait_does_not_cancel_connection_setup() {
        let wait = std::time::Duration::from_millis(50);
        let addr = spawn_mock_nntp_server_greeting_after(wait * 6).await;
        let provider = provider_with_cooldown(addr, 1, None);

        let conn = provider
            .checkout_connection_guard_within(wait)
            .await
            .expect("setup past the slot wait keeps its own deadline");
        conn.release();
        assert_eq!(
            provider.pool.status().size,
            1,
            "the new connection is pooled"
        );
    }

    #[tokio::test]
    async fn bounded_checkout_wait_reports_a_full_pool() {
        let addr = spawn_mock_nntp_server().await;
        let provider = provider_with_cooldown(addr, 1, None);
        let _held = provider.get_pooled_connection().await.unwrap();

        let started = std::time::Instant::now();
        let result = provider
            .checkout_connection_guard_within(std::time::Duration::from_millis(50))
            .await;
        assert!(
            matches!(
                result,
                Err(crate::connection_error::ConnectionError::PoolExhausted { max_size: 1, .. })
            ),
            "a full pool must surface as PoolExhausted"
        );
        assert!(started.elapsed() < crate::constants::timeout::CONNECTION);
    }

    #[tokio::test]
    async fn backlogged_checkout_fails_at_once_without_queueing() {
        use crate::pool::admission::{BacklogPolicy, CheckoutBudget};

        let addr = spawn_mock_nntp_server().await;
        let provider = provider_with_cooldown(addr, 1, None);
        let _held = provider.get_pooled_connection().await.unwrap();
        provider.record_service_time(std::time::Duration::from_secs(5));
        let budget = CheckoutBudget::new(
            std::time::Duration::from_secs(2),
            BacklogPolicy::SkipAbove { percent: 100 },
        );

        let started = std::time::Instant::now();
        let result = provider.checkout_for_attempt(budget).await;
        assert!(
            matches!(
                result,
                Err(crate::connection_error::ConnectionError::Backlogged { .. })
            ),
            "a pool that cannot serve within the budget is skipped"
        );
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
        assert_eq!(provider.load().snapshot().backlog_skips, 1);

        let queued = provider
            .checkout_for_attempt(CheckoutBudget::new(
                std::time::Duration::from_millis(50),
                BacklogPolicy::Queue,
            ))
            .await;
        assert!(
            matches!(
                queued,
                Err(crate::connection_error::ConnectionError::PoolExhausted { .. })
            ),
            "with skipping off the checkout waits its limit"
        );
        assert_eq!(provider.load().snapshot().busy_timeouts, 1);
    }

    #[tokio::test]
    async fn completed_exchange_feeds_the_service_time_estimate() {
        let addr = spawn_mock_nntp_server().await;
        let provider = provider_with_cooldown(addr, 1, None);
        assert_eq!(provider.load().service_time(), None);

        let mut conn = provider
            .checkout_connection_guard()
            .await
            .unwrap()
            .activate();
        let _ = conn.stream_mut();
        conn.mark_response_complete();
        let _ = conn.complete_success();
        assert!(
            provider.load().service_time().is_some(),
            "a finished command records how long it held the connection"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn setup_that_starts_after_waiting_is_a_setup_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let provider = DeadpoolConnectionProvider::builder(addr.ip().to_string(), addr.port())
            .max_connections(1)
            .build()
            .unwrap();

        let holder = provider.clone();
        let mut holder_task = tokio::spawn(async move { holder.checkout_connection_guard().await });
        let (socket, _) = tokio::select! {
            accepted = listener.accept() => accepted.unwrap(),
            _ = &mut holder_task => panic!("holder finished before the backend accepted"),
        };
        hold_socket(socket);

        let waiter = provider.clone();
        let mut waiter_task = tokio::spawn(async move { waiter.checkout_connection_guard().await });
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(
            !waiter_task.is_finished(),
            "the waiter finished while the slot was held"
        );

        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        holder_task.abort();
        let _ = holder_task.await;

        let (socket, _) = tokio::select! {
            accepted = listener.accept() => accepted.unwrap(),
            _ = &mut waiter_task => panic!("the waiter finished when the slot became free"),
        };
        hold_socket(socket);
        assert!(
            !waiter_task.is_finished(),
            "setup was reported before its own deadline"
        );

        tokio::time::advance(
            crate::constants::timeout::CONNECTION + std::time::Duration::from_millis(1),
        )
        .await;
        for _ in 0..16 {
            if waiter_task.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            waiter_task.is_finished(),
            "setup that started after the wait was not polled through its deadline"
        );
        let result = waiter_task.await.expect("waiter panicked");
        assert_setup_deadline(&result);
        assert_eq!(ConnectionProvider::status(&provider).created.get(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_creation_times_out_and_frees_the_pool_slot() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let provider = DeadpoolConnectionProvider::builder(addr.ip().to_string(), addr.port())
            .max_connections(1)
            .build()
            .unwrap();

        // The creator blocks in a socket read, so a paused clock does not
        // auto-advance. The creator's setup deadline and the waiter's wait
        // deadline start together. If the waiter takes the freed slot in that
        // same instant, its setup timer starts then and needs another deadline.
        let creator = provider.clone();
        let mut creator_task =
            tokio::spawn(async move { creator.checkout_connection_guard().await });
        let (socket, _) = tokio::select! {
            accepted = listener.accept() => accepted.unwrap(),
            _ = &mut creator_task => panic!("checkout finished before the backend accepted"),
        };
        tokio::spawn(async move {
            let _held = socket;
            std::future::pending::<()>().await;
        });

        let waiter = provider.clone();
        let waiter_task = tokio::spawn(async move { waiter.checkout_connection_guard().await });
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(
            !creator_task.is_finished(),
            "stalled creation finished before its deadline"
        );
        assert!(
            !waiter_task.is_finished(),
            "the waiter finished before the stalled creation reached its deadline"
        );

        let step = crate::constants::timeout::CONNECTION + std::time::Duration::from_millis(1);
        tokio::time::advance(step).await;
        for _ in 0..16 {
            if creator_task.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            creator_task.is_finished(),
            "stalled creation was not polled through its deadline"
        );
        if !waiter_task.is_finished() {
            tokio::time::advance(step).await;
            for _ in 0..16 {
                if waiter_task.is_finished() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        }
        assert!(
            waiter_task.is_finished(),
            "the waiter was not polled through its deadline"
        );

        let first = creator_task.await.expect("stalled creation panicked");
        assert_setup_deadline(&first);
        let second = waiter_task.await.expect("waiter panicked");
        let waited_for_the_slot = matches!(
            second,
            Err(crate::connection_error::ConnectionError::PoolExhausted { max_size: 1, .. })
        );
        let started_setup = matches!(
            &second,
            Err(crate::connection_error::ConnectionError::IoError(error))
                if error.kind() == std::io::ErrorKind::TimedOut
                    && error.to_string().contains("connection setup timed out")
        );
        assert!(
            waited_for_the_slot || started_setup,
            "waiter was not bounded by the single slot: {}",
            match &second {
                Ok(_) => "checkout returned a connection".to_string(),
                Err(error) => error.to_string(),
            }
        );
        assert_eq!(ConnectionProvider::status(&provider).created.get(), 0);
        assert_eq!(provider.status_counts().waiting, 0);

        tokio::time::resume();
        while let Ok(Ok(_)) =
            tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept()).await
        {
        }

        let third = provider.checkout_connection_guard();
        tokio::pin!(third);
        let replacement =
            tokio::time::timeout(std::time::Duration::from_secs(2), listener.accept());
        tokio::pin!(replacement);
        tokio::select! {
            accepted = &mut replacement => {
                accepted
                    .expect("the stalled creation still occupied the only pool slot")
                    .unwrap();
            }
            _ = &mut third => panic!("replacement checkout finished before the backend accepted"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn backend_setup_has_a_deadline_when_connect_stalls() {
        // TEST-NET-1 (RFC 5737) does not answer when the host has a route to it.
        // A closed local port resets immediately. Without a route, connect fails
        // at once with an unreachable error and never reaches the setup deadline.
        // UDP connect only checks that route; it does not send a packet.
        if std::net::UdpSocket::bind("0.0.0.0:0")
            .and_then(|socket| socket.connect("192.0.2.1:9"))
            .is_err()
        {
            eprintln!("skipping: no route to 192.0.2.1");
            return;
        }
        let provider = DeadpoolConnectionProvider::builder("192.0.2.1", 9)
            .max_connections(1)
            .build()
            .unwrap();
        let checkout = provider.clone();
        let task = tokio::spawn(async move { checkout.checkout_connection_guard().await });

        arm_setup_deadline(&task).await;

        let result = task.await.expect("connect stall panicked");
        assert_setup_deadline(&result);
        assert_eq!(ConnectionProvider::status(&provider).created.get(), 0);
        assert_eq!(provider.status_counts().waiting, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn backend_setup_has_a_deadline_when_tls_handshake_stalls() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let provider = DeadpoolConnectionProvider::builder(addr.ip().to_string(), addr.port())
            .max_connections(1)
            .tls_config(
                TlsConfig::builder()
                    .enabled(true)
                    .verify_cert(false)
                    .build(),
            )
            .build()
            .unwrap();
        let checkout = provider.clone();
        let mut task = tokio::spawn(async move { checkout.checkout_connection_guard().await });
        let (mut socket, _) = tokio::select! {
            accepted = listener.accept() => accepted.unwrap(),
            _ = &mut task => panic!("checkout finished before the backend accepted"),
        };
        let mut hello = [0u8; 1];
        tokio::select! {
            read = socket.read(&mut hello) => {
                assert_eq!(read.expect("client hello read"), 1);
                assert_eq!(hello[0], 0x16, "expected a TLS handshake record");
            }
            _ = &mut task => panic!("checkout finished before the TLS handshake started"),
        }
        hold_socket(socket);

        arm_setup_deadline(&task).await;

        let result = task.await.expect("tls stall panicked");
        assert_setup_deadline(&result);
        assert_eq!(ConnectionProvider::status(&provider).created.get(), 0);
        assert_eq!(provider.status_counts().waiting, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn backend_setup_has_a_deadline_when_auth_stalls() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let provider = DeadpoolConnectionProvider::builder(addr.ip().to_string(), addr.port())
            .username("stall-user")
            .max_connections(1)
            .build()
            .unwrap();
        let checkout = provider.clone();
        let mut task = tokio::spawn(async move { checkout.checkout_connection_guard().await });
        let (socket, _) = tokio::select! {
            accepted = listener.accept() => accepted.unwrap(),
            _ = &mut task => panic!("checkout finished before the backend accepted"),
        };
        let (read, mut write) = socket.into_split();
        let mut reader = tokio::io::BufReader::new(read);
        write.write_all(b"200 Ready\r\n").await.unwrap();
        let mut line = String::new();
        tokio::select! {
            read = reader.read_line(&mut line) => {
                assert!(read.expect("auth command read") > 0, "backend closed before AUTHINFO");
            }
            _ = &mut task => panic!("checkout finished before AUTHINFO USER"),
        }
        assert_eq!(
            line.trim(),
            "AUTHINFO USER stall-user",
            "stall was not at authentication"
        );
        tokio::spawn(async move {
            let _held_read = reader;
            let _held_write = write;
            std::future::pending::<()>().await;
        });

        arm_setup_deadline(&task).await;

        let result = task.await.expect("auth stall panicked");
        assert_setup_deadline(&result);
        assert_eq!(ConnectionProvider::status(&provider).created.get(), 0);
        assert_eq!(provider.status_counts().waiting, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn backend_setup_has_a_deadline_when_mode_reader_stalls() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let provider = DeadpoolConnectionProvider::builder(addr.ip().to_string(), addr.port())
            .max_connections(1)
            .build()
            .unwrap();
        let checkout = provider.clone();
        let mut task = tokio::spawn(async move { checkout.checkout_connection_guard().await });
        let (socket, _) = tokio::select! {
            accepted = listener.accept() => accepted.unwrap(),
            _ = &mut task => panic!("checkout finished before the backend accepted"),
        };
        let (read, mut write) = socket.into_split();
        let mut reader = tokio::io::BufReader::new(read);
        write.write_all(b"200 Ready\r\n").await.unwrap();
        let mut line = String::new();
        tokio::select! {
            read = reader.read_line(&mut line) => {
                assert!(read.expect("mode reader read") > 0, "backend closed before MODE READER");
            }
            _ = &mut task => panic!("checkout finished before MODE READER"),
        }
        assert_eq!(line.trim(), "MODE READER", "stall was not at MODE READER");
        tokio::spawn(async move {
            let _held_read = reader;
            let _held_write = write;
            std::future::pending::<()>().await;
        });

        arm_setup_deadline(&task).await;

        let result = task.await.expect("mode reader stall panicked");
        assert_setup_deadline(&result);
        assert_eq!(ConnectionProvider::status(&provider).created.get(), 0);
        assert_eq!(provider.status_counts().waiting, 0);
    }

    #[test]
    fn test_builder_new() {
        let builder = Builder::new("news.example.com", 119);
        assert_eq!(builder.host, "news.example.com");
        assert_eq!(builder.port, 119);
        assert_eq!(builder.max_size, 10); // Default
        assert_eq!(builder.stat_missing, StatMissingPolicy::Disabled);
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
    fn test_builder_with_stat_missing() {
        let builder = Builder::new("example.com", 119).stat_missing(true);
        assert_eq!(builder.stat_missing, StatMissingPolicy::Probe);
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
        spawn_mock_nntp_server_greeting_after(std::time::Duration::ZERO).await
    }

    /// Like [`spawn_mock_nntp_server`], but each connection waits `delay`
    /// before greeting, so connection setup outlasts short checkout waits.
    async fn spawn_mock_nntp_server_greeting_after(
        delay: std::time::Duration,
    ) -> std::net::SocketAddr {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
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

        let pool = crate::pool::deadpool_connection::configured_pool(manager, max_size);

        DeadpoolConnectionProvider {
            pool,
            name: Arc::from(format!("test-{}", addr.port())),
            stat_missing: StatMissingPolicy::Disabled,
            shutdown_tx: None,
            health_check_metrics: Arc::new(crate::pool::health_check::HealthCheckMetrics::new()),
            cap: Arc::new(crate::pool::connection_cap::ConnectionCap::new(
                crate::pool::connection_cap::ConnectionCapPolicy::Fixed,
                max_size,
            )),
            cap_shutdown_tx: None,
            active_cooldowns: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            replacement_cooldown: cooldown,
            is_shutting_down: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            load: Arc::default(),
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
        provider.remove_with_cooldown(conn, crate::pool::churn::CloseReason::BackendError);

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

        provider.remove_with_cooldown(conn1, crate::pool::churn::CloseReason::BackendError);
        assert_eq!(provider.pool.status().max_size, 3);

        provider.remove_with_cooldown(conn2, crate::pool::churn::CloseReason::BackendError);
        assert_eq!(provider.pool.status().max_size, 2);

        // Third removal should NOT further reduce (at cap)
        provider.remove_with_cooldown(conn3, crate::pool::churn::CloseReason::BackendError);
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
        provider.remove_with_cooldown(conn, crate::pool::churn::CloseReason::BackendError);

        // With cooldown disabled, max_size should NOT be reduced
        assert_eq!(provider.pool.status().max_size, max_size);
        assert_eq!(provider.active_cooldowns.load(Ordering::Acquire), 0);
    }

    /// Every close is counted once with the reason its retirer gave, and an
    /// idle connection dropped by the pool itself counts as `idle`, so
    /// `created` minus all closes equals the live pool size.
    #[tokio::test]
    async fn churn_counts_each_close_under_its_reason() {
        use crate::pool::churn::CloseReason;
        let addr = spawn_mock_nntp_server().await;
        let provider = provider_with_cooldown(addr, 4, None);

        let a = provider.get_pooled_connection().await.unwrap();
        let b = provider.get_pooled_connection().await.unwrap();
        let c = provider.get_pooled_connection().await.unwrap();
        let d = provider.get_pooled_connection().await.unwrap();
        provider.remove_with_cooldown(a, CloseReason::BackendError);
        provider.remove_with_cooldown(b, CloseReason::Cancelled);
        provider.remove_without_cooldown(c);
        drop(d); // back to the pool, idle
        provider.clear_idle_connections();

        let churn = provider.connection_churn();
        assert_eq!(churn.created, 4);
        assert_eq!(churn.closed_backend_error, 1);
        assert_eq!(churn.closed_cancelled, 1);
        assert_eq!(churn.closed_client_abort, 1);
        assert_eq!(churn.closed_idle, 1);
        let closed = churn.closed_backend_error
            + churn.closed_cancelled
            + churn.closed_client_abort
            + churn.closed_health_check
            + churn.closed_stale
            + churn.closed_idle;
        assert_eq!(churn.created - closed, provider.pool.status().size as u64);
    }

    /// Verify explicit no-cooldown removal closes a dirty connection without
    /// reducing capacity even when replacement cooldown is configured.
    #[tokio::test]
    async fn test_remove_without_cooldown_preserves_pool_size() {
        let addr = spawn_mock_nntp_server().await;
        let cooldown = std::time::Duration::from_secs(10);
        let max_size = 4;
        let provider = provider_with_cooldown(addr, max_size, Some(cooldown));

        let conn = provider.get_pooled_connection().await.unwrap();
        provider.remove_without_cooldown(conn);

        assert_eq!(provider.pool.status().max_size, max_size);
        assert_eq!(provider.pool.status().size, 0);
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
        provider.remove_with_cooldown(conn, crate::pool::churn::CloseReason::BackendError);

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

    #[tokio::test]
    async fn test_remove_with_cooldown_skips_reduction_during_shutdown() {
        let addr = spawn_mock_nntp_server().await;
        let cooldown = std::time::Duration::from_secs(10);
        let max_size = 4;
        let provider = provider_with_cooldown(addr, max_size, Some(cooldown));

        provider.shutdown();

        let conn = provider.get_pooled_connection().await.unwrap();
        provider.remove_with_cooldown(conn, crate::pool::churn::CloseReason::BackendError);

        assert_eq!(provider.pool.status().max_size, max_size);
        assert_eq!(provider.active_cooldowns.load(Ordering::Acquire), 0);
        assert!(provider.is_shutting_down.load(Ordering::Acquire));
    }

    #[tokio::test(start_paused = true)]
    async fn test_shutdown_connections_concurrently_runs_all_writes_together() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        let completed = Arc::new(AtomicUsize::new(0));
        let completed_clone = completed.clone();
        let task = tokio::spawn(async move {
            shutdown_connections_concurrently(vec![1_u8, 2, 3], move |_| {
                let completed = completed_clone.clone();
                async move {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    completed.fetch_add(1, AtomicOrdering::Relaxed);
                }
            })
            .await;
        });

        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_millis(499)).await;
        assert_eq!(completed.load(AtomicOrdering::Relaxed), 0);
        assert!(!task.is_finished());

        tokio::time::advance(std::time::Duration::from_millis(1)).await;
        task.await.unwrap();
        assert_eq!(completed.load(AtomicOrdering::Relaxed), 3);
    }
}
