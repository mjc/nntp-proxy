//! NNTP Proxy implementation
//!
//! This module contains the main `NntpProxy` struct which orchestrates
//! connection handling, routing, and resource management.

use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tracing::{debug, error, info, warn};

use crate::auth::AuthHandler;
use crate::config::{Config, RoutingMode, Server};
use crate::constants::buffer::{POOL, POOL_COUNT};
use crate::metrics::{ConnectionStatsAggregator, MetricsCollector};
use crate::network::{ConnectionOptimizer, NetworkOptimizer, TcpOptimizer};
use crate::pool::{BufferPool, ConnectionProvider, DeadpoolConnectionProvider, prewarm_pools};
use crate::protocol::BACKEND_UNAVAILABLE;
use crate::router;
use crate::session::ClientSession;
use crate::types::{self, BufferSize};

/// Builder for constructing an `NntpProxy` with optional configuration overrides
///
/// # Examples
///
/// Basic usage with defaults:
/// ```no_run
/// # use nntp_proxy::{NntpProxyBuilder, Config, RoutingMode};
/// # use nntp_proxy::config::load_config;
/// # fn main() -> anyhow::Result<()> {
/// let config = load_config("config.toml")?;
/// let proxy = NntpProxyBuilder::new(config)
///     .with_routing_mode(RoutingMode::Hybrid)
///     .build()?;
/// # Ok(())
/// # }
/// ```
///
/// With custom buffer pool size:
/// ```no_run
/// # use nntp_proxy::{NntpProxyBuilder, Config, RoutingMode};
/// # use nntp_proxy::config::load_config;
/// # fn main() -> anyhow::Result<()> {
/// let config = load_config("config.toml")?;
/// let proxy = NntpProxyBuilder::new(config)
///     .with_routing_mode(RoutingMode::PerCommand)
///     .with_buffer_pool_size(512 * 1024)  // 512KB buffers
///     .with_buffer_pool_count(64)         // 64 buffers
///     .build()?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct NntpProxyBuilder {
    config: Config,
    routing_mode: RoutingMode,
    buffer_size: Option<usize>,
    buffer_count: Option<usize>,
    enable_metrics: bool,
}

impl NntpProxyBuilder {
    /// Create a new builder with the given configuration
    ///
    /// The routing mode defaults to `Standard` (1:1) mode.
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            config,
            routing_mode: RoutingMode::Stateful,
            buffer_size: None,
            buffer_count: None,
            enable_metrics: false, // Default to disabled for performance
        }
    }

    /// Set the routing mode
    ///
    /// Available modes:
    /// - `Standard`: 1:1 client-to-backend mapping (default)
    /// - `PerCommand`: Each command routes to a different backend
    /// - `Hybrid`: Starts in per-command mode, switches to stateful when needed
    #[must_use]
    pub fn with_routing_mode(mut self, mode: RoutingMode) -> Self {
        self.routing_mode = mode;
        self
    }

    /// Override the default buffer pool size (256KB)
    ///
    /// This affects the size of each buffer in the pool. Larger buffers
    /// can improve throughput for large article transfers but use more memory.
    #[must_use]
    pub fn with_buffer_pool_size(mut self, size: usize) -> Self {
        self.buffer_size = Some(size);
        self
    }

    /// Override the default buffer pool count (32)
    ///
    /// This affects how many buffers are pre-allocated. Should roughly match
    /// the expected number of concurrent connections.
    #[must_use]
    pub fn with_buffer_pool_count(mut self, count: usize) -> Self {
        self.buffer_count = Some(count);
        self
    }

    /// Enable metrics collection for TUI and monitoring
    ///
    /// **Warning:** Metrics collection causes ~45% performance regression
    /// (110MB/s -> 60MB/s) due to atomic operations in the hot path.
    /// Only enable this when you need the TUI or monitoring.
    #[must_use]
    pub fn with_metrics(mut self) -> Self {
        self.enable_metrics = true;
        self
    }

    /// Build the `NntpProxy` instance
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No servers are configured
    /// - Connection providers cannot be created
    /// - Buffer size is zero
    pub fn build(self) -> Result<NntpProxy> {
        if self.config.servers.is_empty() {
            anyhow::bail!("No servers configured in configuration");
        }

        // Use provided values or defaults
        let buffer_size = self.buffer_size.unwrap_or(POOL);
        let buffer_count = self.buffer_count.unwrap_or(POOL_COUNT);

        // Create deadpool connection providers for each server
        let connection_providers: Result<Vec<DeadpoolConnectionProvider>> = self
            .config
            .servers
            .iter()
            .map(|server| {
                info!(
                    "Configuring deadpool connection provider for '{}'",
                    server.name
                );
                DeadpoolConnectionProvider::from_server_config(server)
            })
            .collect();

        let connection_providers = connection_providers?;

        let buffer_pool = BufferPool::new(
            BufferSize::new(buffer_size)
                .ok_or_else(|| anyhow::anyhow!("Buffer size must be non-zero"))?,
            buffer_count,
        );

        // Create metrics collector (before moving servers)
        let metrics = MetricsCollector::new(self.config.servers.len());

        let servers = Arc::new(self.config.servers);

        // Create backend selector and add all backends
        let router = Arc::new({
            use types::BackendId;
            let backend_strategy = self.config.proxy.backend_selection;
            connection_providers.iter().enumerate().fold(
                router::BackendSelector::with_strategy(backend_strategy),
                |mut r, (idx, provider)| {
                    let backend_id = BackendId::from_index(idx);
                    r.add_backend(backend_id, servers[idx].name.clone(), provider.clone());
                    r
                },
            )
        });

        // Create auth handler from config
        let auth_handler = {
            let all_users: Vec<(String, String)> = self
                .config
                .client_auth
                .all_users()
                .into_iter()
                .map(|(u, p)| (u.to_string(), p.to_string()))
                .collect();

            if all_users.is_empty() {
                Arc::new(AuthHandler::default())
            } else {
                Arc::new(AuthHandler::with_users(all_users).with_context(|| {
                    "Invalid authentication configuration. \
                             If you set username/password in config, they cannot be empty. \
                             Remove them entirely to disable authentication."
                })?)
            }
        };

        Ok(NntpProxy {
            servers,
            router,
            connection_providers,
            buffer_pool,
            routing_mode: self.routing_mode,
            auth_handler,
            metrics,
            enable_metrics: self.enable_metrics,
            connection_stats: ConnectionStatsAggregator::new(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct NntpProxy {
    servers: Arc<Vec<Server>>,
    /// Backend selector for round-robin load balancing
    router: Arc<router::BackendSelector>,
    /// Connection providers per server - easily swappable implementation
    connection_providers: Vec<DeadpoolConnectionProvider>,
    /// Buffer pool for I/O operations
    buffer_pool: BufferPool,
    /// Routing mode (Standard, PerCommand, or Hybrid)
    routing_mode: RoutingMode,
    /// Authentication handler for client auth interception
    auth_handler: Arc<AuthHandler>,
    /// Metrics collector for TUI and monitoring
    metrics: MetricsCollector,
    /// Whether metrics collection is enabled (causes ~45% perf penalty)
    enable_metrics: bool,
    /// Connection statistics aggregator (reduces log spam)
    connection_stats: ConnectionStatsAggregator,
}

/// Classify an error as a client disconnect (broken pipe/connection reset)
///
/// Returns true for errors that indicate the client disconnected normally,
/// which should be logged at DEBUG level rather than WARN.
///
/// # Examples
///
/// ```
/// use std::io::{Error, ErrorKind};
/// use nntp_proxy::is_client_disconnect_error;
///
/// let broken_pipe = Error::from(ErrorKind::BrokenPipe);
/// let wrapped = anyhow::Error::from(broken_pipe);
/// assert!(is_client_disconnect_error(&wrapped));
///
/// let other_error = anyhow::anyhow!("some other error");
/// assert!(!is_client_disconnect_error(&other_error));
/// ```
#[inline]
pub fn is_client_disconnect_error(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>().is_some_and(|io_err| {
        matches!(
            io_err.kind(),
            std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
        )
    })
}

impl NntpProxy {
    // Helper methods for session management

    #[inline]
    fn record_connection_opened(&self) {
        if self.enable_metrics {
            self.metrics.connection_opened();
        }
    }

    #[inline]
    fn record_connection_closed(&self) {
        if self.enable_metrics {
            self.metrics.connection_closed();
        }
    }

    /// Build a session with standard configuration (conditionally enables metrics)
    fn build_session(
        &self,
        client_addr: SocketAddr,
        router: Option<Arc<router::BackendSelector>>,
        routing_mode: RoutingMode,
        cache: Option<Arc<crate::cache::ArticleCache>>,
    ) -> ClientSession {
        let mut builder = ClientSession::builder(
            client_addr,
            self.buffer_pool.clone(),
            self.auth_handler.clone(),
        )
        .with_routing_mode(routing_mode)
        .with_connection_stats(self.connection_stats.clone());

        if let Some(r) = router {
            builder = builder.with_router(r);
        }

        if let Some(c) = cache {
            builder = builder.with_cache(c);
        }

        if self.enable_metrics {
            builder = builder.with_metrics(self.metrics.clone());
        }

        builder.build()
    }

    /// Log session completion and record stats
    fn log_session_completion(
        &self,
        client_addr: SocketAddr,
        session_id: &str,
        session: &ClientSession,
        routing_mode_str: &str,
        metrics: &types::TransferMetrics,
    ) {
        self.connection_stats
            .record_disconnection(session.username().as_deref(), routing_mode_str);

        debug!(
            "Session {} [{}] ↑{} ↓{}",
            client_addr,
            session_id,
            crate::formatting::format_bytes(metrics.client_to_backend.as_u64()),
            crate::formatting::format_bytes(metrics.backend_to_client.as_u64())
        );
    }

    /// Handle session errors with appropriate logging
    fn handle_session_error(&self, e: anyhow::Error, client_addr: SocketAddr, session_id: &str) {
        if is_client_disconnect_error(&e) {
            debug!(
                "Client {} [{}] disconnected: {} (normal for test connections)",
                client_addr, session_id, e
            );
        } else {
            warn!("Session error {} [{}]: {}", client_addr, session_id, e);
        }
    }

    /// Create a new `NntpProxy` with the given configuration and routing mode
    ///
    /// This is a convenience method that uses the builder internally.
    /// For more control over configuration, use [`NntpProxy::builder`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use nntp_proxy::{NntpProxy, Config, RoutingMode};
    /// # use nntp_proxy::config::load_config;
    /// # fn main() -> anyhow::Result<()> {
    /// let config = load_config("config.toml")?;
    /// let proxy = NntpProxy::new(config, RoutingMode::Hybrid)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn new(config: Config, routing_mode: RoutingMode) -> Result<Self> {
        NntpProxyBuilder::new(config)
            .with_routing_mode(routing_mode)
            .build()
    }

    /// Create a builder for more fine-grained control over proxy configuration
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use nntp_proxy::{NntpProxy, Config, RoutingMode};
    /// # use nntp_proxy::config::load_config;
    /// # fn main() -> anyhow::Result<()> {
    /// let config = load_config("config.toml")?;
    /// let proxy = NntpProxy::builder(config)
    ///     .with_routing_mode(RoutingMode::Hybrid)
    ///     .with_buffer_pool_size(512 * 1024)
    ///     .build()?;
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn builder(config: Config) -> NntpProxyBuilder {
        NntpProxyBuilder::new(config)
    }

    /// Prewarm all connection pools before accepting clients
    /// Creates all connections concurrently and returns when ready
    pub async fn prewarm_connections(&self) -> Result<()> {
        prewarm_pools(&self.connection_providers, &self.servers).await
    }

    /// Gracefully shutdown all connection pools
    pub async fn graceful_shutdown(&self) {
        info!("Initiating graceful shutdown of all connection pools...");

        for provider in &self.connection_providers {
            provider.graceful_shutdown().await;
        }

        info!("All connection pools have been shut down gracefully");
    }

    /// Get the list of servers
    #[must_use]
    #[inline]
    pub fn servers(&self) -> &[Server] {
        &self.servers
    }

    /// Get the router
    #[must_use]
    #[inline]
    pub fn router(&self) -> &Arc<router::BackendSelector> {
        &self.router
    }

    /// Get the connection providers
    #[must_use]
    #[inline]
    pub fn connection_providers(&self) -> &[DeadpoolConnectionProvider] {
        &self.connection_providers
    }

    /// Get the buffer pool
    #[must_use]
    #[inline]
    pub fn buffer_pool(&self) -> &BufferPool {
        &self.buffer_pool
    }

    /// Get the metrics collector
    #[must_use]
    #[inline]
    pub fn metrics(&self) -> &MetricsCollector {
        &self.metrics
    }

    /// Get connection stats aggregator
    #[must_use]
    #[inline]
    pub fn connection_stats(&self) -> &ConnectionStatsAggregator {
        &self.connection_stats
    }

    /// Common setup for client connections
    ///
    /// Sends the proxy's greeting immediately to the client.
    /// Backend greetings are consumed when connections are created in the pool.
    async fn setup_client_connection(
        &self,
        client_stream: &mut TcpStream,
        client_addr: SocketAddr,
    ) -> Result<()> {
        // Send proxy greeting immediately
        // (Backend greetings already consumed during connection creation)
        crate::protocol::send_proxy_greeting(client_stream, client_addr).await
    }

    pub async fn handle_client(
        &self,
        mut client_stream: TcpStream,
        client_addr: SocketAddr,
    ) -> Result<()> {
        debug!("New client connection from {}", client_addr);

        // Record connection metrics
        self.record_connection_opened();

        // Use a dummy ClientId and command for routing (synchronous 1:1 mapping)
        use types::ClientId;
        let client_id = ClientId::new();

        // Select backend using router's round-robin
        let backend_id = self.router.route_command(client_id, "")?;
        let server_idx = backend_id.as_index();
        let server = &self.servers[server_idx];

        info!(
            "Routing client {} to backend {:?} ({}:{})",
            client_addr, backend_id, server.host, server.port
        );

        // Setup connection (prewarm and greeting)
        self.setup_client_connection(&mut client_stream, client_addr)
            .await?;

        // Get pooled backend connection
        let pool_status = self.connection_providers[server_idx].status();
        debug!(
            "Pool status for {}: {}/{} available, {} created",
            server.name, pool_status.available, pool_status.max_size, pool_status.created
        );

        let mut backend_conn = match self.connection_providers[server_idx]
            .get_pooled_connection()
            .await
        {
            Ok(conn) => {
                debug!("Got pooled connection for {}", server.name);
                conn
            }
            Err(e) => {
                error!(
                    "Failed to get pooled connection for {} (client {}): {}",
                    server.name, client_addr, e
                );
                let _ = client_stream.write_all(BACKEND_UNAVAILABLE).await;
                return Err(anyhow::anyhow!(
                    "Failed to get pooled connection for backend '{}' (client {}): {}",
                    server.name,
                    client_addr,
                    e
                ));
            }
        };

        // Apply socket optimizations for high-throughput
        let client_optimizer = TcpOptimizer::new(&client_stream);
        if let Err(e) = client_optimizer.optimize() {
            debug!("Failed to optimize client socket: {}", e);
        }

        let backend_optimizer = ConnectionOptimizer::new(&backend_conn);
        if let Err(e) = backend_optimizer.optimize() {
            debug!("Failed to optimize backend socket: {}", e);
        }

        // Create session and handle connection
        let session = self.build_session(client_addr, None, self.routing_mode, None);
        let session_id = crate::formatting::short_id(session.client_id().as_uuid());

        debug!("Starting session for client {}", client_addr);
        let copy_result = if self.enable_metrics {
            // Use metrics-enabled version for periodic reporting
            session
                .handle_with_pooled_backend_and_metrics(
                    client_stream,
                    &mut *backend_conn,
                    backend_id,
                )
                .await
        } else {
            // Use basic version without metrics overhead
            session
                .handle_with_pooled_backend(client_stream, &mut *backend_conn)
                .await
        };

        // Record connection statistics if not already recorded during auth
        // (on_authentication_success already records for authenticated sessions)
        if !self.auth_handler.is_enabled() || session.username().is_none() {
            let routing_mode_str = match session.mode() {
                crate::session::SessionMode::PerCommand => "per-command",
                crate::session::SessionMode::Stateful => match self.routing_mode {
                    RoutingMode::Stateful => "standard",
                    RoutingMode::Hybrid => "hybrid",
                    _ => "stateful",
                },
            };
            self.connection_stats
                .record_connection(session.username().as_deref(), routing_mode_str);
        }

        // Complete the routing (decrement pending count)
        self.router.complete_command(backend_id);

        // Log session results and handle backend connection errors
        match copy_result {
            Ok(metrics) => {
                self.log_session_completion(
                    client_addr,
                    &session_id,
                    &session,
                    "standard",
                    &metrics,
                );

                // Record transfer metrics
                if self.enable_metrics {
                    self.metrics.record_client_to_backend_bytes_for(
                        backend_id,
                        metrics.client_to_backend.as_u64(),
                    );
                    self.metrics.record_backend_to_client_bytes_for(
                        backend_id,
                        metrics.backend_to_client.as_u64(),
                    );
                }
            }
            Err(e) => {
                // Record error
                if self.enable_metrics {
                    self.metrics.record_error(backend_id);
                }

                // Check if this is a backend I/O error - if so, remove connection from pool
                if crate::pool::is_connection_error(&e) {
                    warn!(
                        "Backend connection error for client {}: {} - removing connection from pool",
                        client_addr, e
                    );
                    crate::pool::remove_from_pool(backend_conn);
                    self.record_connection_closed();
                    return Err(e);
                }
                warn!("Session error for client {}: {}", client_addr, e);
            }
        }

        self.record_connection_closed();
        Ok(())
    }

    /// Handle client connection using per-command routing with article caching
    pub async fn handle_client_with_cache(
        &self,
        client_stream: TcpStream,
        client_addr: SocketAddr,
        cache: Arc<crate::cache::ArticleCache>,
    ) -> Result<()> {
        self.handle_per_command_routing_internal(client_stream, client_addr, Some(cache), "caching")
            .await
    }

    /// Handle client connection using per-command routing mode
    ///
    /// This creates a session with the router, allowing commands from this client
    /// to be routed to different backends based on load balancing.
    pub async fn handle_client_per_command_routing(
        &self,
        client_stream: TcpStream,
        client_addr: SocketAddr,
    ) -> Result<()> {
        self.handle_per_command_routing_internal(client_stream, client_addr, None, "per-command")
            .await
    }

    /// Internal implementation for per-command routing (with optional caching)
    async fn handle_per_command_routing_internal(
        &self,
        client_stream: TcpStream,
        client_addr: SocketAddr,
        cache: Option<Arc<crate::cache::ArticleCache>>,
        mode_label: &str,
    ) -> Result<()> {
        debug!(
            "New {} routing client connection from {}",
            mode_label, client_addr
        );

        // Record connection metrics
        self.record_connection_opened();

        // Enable TCP_NODELAY for low latency
        if let Err(e) = client_stream.set_nodelay(true) {
            debug!("Failed to set TCP_NODELAY for {}: {}", client_addr, e);
        }

        // Create session with optional cache
        let session = self.build_session(
            client_addr,
            Some(self.router.clone()),
            self.routing_mode,
            cache,
        );

        let session_id = crate::formatting::short_id(session.client_id().as_uuid());

        // Handle the session with per-command routing
        let result = session
            .handle_per_command_routing(client_stream)
            .await
            .with_context(|| {
                format!(
                    "{} routing session failed for {} [{}]",
                    mode_label, client_addr, session_id
                )
            });

        // Log session results
        match result {
            Ok(metrics) => {
                self.log_session_completion(
                    client_addr,
                    &session_id,
                    &session,
                    &self.routing_mode.to_string().to_lowercase(),
                    &metrics,
                );
            }
            Err(e) => {
                self.handle_session_error(e, client_addr, &session_id);
            }
        }

        self.record_connection_closed();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn create_test_config() -> Config {
        use crate::config::{health_check_max_per_cycle, health_check_pool_timeout};
        use crate::types::{HostName, MaxConnections, Port, ServerName};
        Config {
            servers: vec![
                Server {
                    host: HostName::new("server1.example.com".to_string()).unwrap(),
                    port: Port::new(119).unwrap(),
                    name: ServerName::new("Test Server 1".to_string()).unwrap(),
                    username: None,
                    password: None,
                    max_connections: MaxConnections::new(5).unwrap(),
                    use_tls: false,
                    tls_verify_cert: true,
                    tls_cert_path: None,
                    connection_keepalive: None,
                    health_check_max_per_cycle: health_check_max_per_cycle(),
                    health_check_pool_timeout: health_check_pool_timeout(),
                },
                Server {
                    host: HostName::new("server2.example.com".to_string()).unwrap(),
                    port: Port::new(119).unwrap(),
                    name: ServerName::new("Test Server 2".to_string()).unwrap(),
                    username: None,
                    password: None,
                    max_connections: MaxConnections::new(8).unwrap(),
                    use_tls: false,
                    tls_verify_cert: true,
                    tls_cert_path: None,
                    connection_keepalive: None,
                    health_check_max_per_cycle: health_check_max_per_cycle(),
                    health_check_pool_timeout: health_check_pool_timeout(),
                },
                Server {
                    host: HostName::new("server3.example.com".to_string()).unwrap(),
                    port: Port::new(119).unwrap(),
                    name: ServerName::new("Test Server 3".to_string()).unwrap(),
                    username: None,
                    password: None,
                    max_connections: MaxConnections::new(12).unwrap(),
                    use_tls: false,
                    tls_verify_cert: true,
                    tls_cert_path: None,
                    connection_keepalive: None,
                    health_check_max_per_cycle: health_check_max_per_cycle(),
                    health_check_pool_timeout: health_check_pool_timeout(),
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn test_proxy_creation_with_servers() {
        let config = create_test_config();
        let proxy = Arc::new(
            NntpProxy::new(config, RoutingMode::Stateful).expect("Failed to create proxy"),
        );

        assert_eq!(proxy.servers().len(), 3);
        assert_eq!(proxy.servers()[0].name.as_str(), "Test Server 1");
    }

    #[test]
    fn test_proxy_creation_with_empty_servers() {
        let config = Config {
            servers: vec![],
            ..Default::default()
        };
        let result = NntpProxy::new(config, RoutingMode::Stateful);

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No servers configured")
        );
    }

    #[test]
    fn test_proxy_has_router() {
        let config = create_test_config();
        let proxy = Arc::new(
            NntpProxy::new(config, RoutingMode::Stateful).expect("Failed to create proxy"),
        );

        // Proxy should have a router with backends
        assert_eq!(proxy.router.backend_count(), 3);
    }

    #[test]
    fn test_builder_basic_usage() {
        let config = create_test_config();
        let proxy = NntpProxy::builder(config)
            .build()
            .expect("Failed to build proxy");

        assert_eq!(proxy.servers().len(), 3);
        assert_eq!(proxy.router.backend_count(), 3);
    }

    #[test]
    fn test_builder_with_routing_mode() {
        let config = create_test_config();
        let proxy = NntpProxy::builder(config)
            .with_routing_mode(RoutingMode::PerCommand)
            .build()
            .expect("Failed to build proxy");

        assert_eq!(proxy.servers().len(), 3);
    }

    #[test]
    fn test_builder_with_custom_buffer_pool() {
        let config = create_test_config();
        let proxy = NntpProxy::builder(config)
            .with_buffer_pool_size(512 * 1024)
            .with_buffer_pool_count(64)
            .build()
            .expect("Failed to build proxy");

        assert_eq!(proxy.servers().len(), 3);
        // Pool size and count are used internally but not exposed for verification
    }

    #[test]
    fn test_builder_with_all_options() {
        let config = create_test_config();
        let proxy = NntpProxy::builder(config)
            .with_routing_mode(RoutingMode::Hybrid)
            .with_buffer_pool_size(1024 * 1024)
            .with_buffer_pool_count(16)
            .build()
            .expect("Failed to build proxy");

        assert_eq!(proxy.servers().len(), 3);
        assert_eq!(proxy.router.backend_count(), 3);
    }

    #[test]
    fn test_builder_empty_servers_error() {
        let config = Config {
            servers: vec![],
            ..Default::default()
        };
        let result = NntpProxy::builder(config).build();

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No servers configured")
        );
    }

    #[test]
    fn test_backward_compatibility_new() {
        // Ensure NntpProxy::new() still works (it uses builder internally)
        let config = create_test_config();
        let proxy = NntpProxy::new(config, RoutingMode::Stateful)
            .expect("Failed to create proxy with new()");

        assert_eq!(proxy.servers().len(), 3);
        assert_eq!(proxy.router.backend_count(), 3);
    }

    // Tests for is_client_disconnect_error function
    mod error_classification {
        use super::*;
        use std::io::{Error, ErrorKind};

        #[test]
        fn test_broken_pipe_is_client_disconnect() {
            let io_err = Error::from(ErrorKind::BrokenPipe);
            let err = anyhow::Error::from(io_err);
            assert!(is_client_disconnect_error(&err));
        }

        #[test]
        fn test_connection_reset_is_client_disconnect() {
            let io_err = Error::from(ErrorKind::ConnectionReset);
            let err = anyhow::Error::from(io_err);
            assert!(is_client_disconnect_error(&err));
        }

        #[test]
        fn test_other_io_errors_not_client_disconnect() {
            let error_kinds = vec![
                ErrorKind::NotFound,
                ErrorKind::PermissionDenied,
                ErrorKind::ConnectionRefused,
                ErrorKind::ConnectionAborted,
                ErrorKind::AddrInUse,
                ErrorKind::AddrNotAvailable,
                ErrorKind::TimedOut,
                ErrorKind::Interrupted,
                ErrorKind::UnexpectedEof,
                ErrorKind::WouldBlock,
            ];

            for kind in error_kinds {
                let io_err = Error::from(kind);
                let err = anyhow::Error::from(io_err);
                assert!(
                    !is_client_disconnect_error(&err),
                    "{:?} should not be classified as client disconnect",
                    kind
                );
            }
        }

        #[test]
        fn test_non_io_error_not_client_disconnect() {
            let err = anyhow::anyhow!("generic error message");
            assert!(!is_client_disconnect_error(&err));
        }

        #[test]
        fn test_wrapped_broken_pipe_error() {
            let io_err = Error::from(ErrorKind::BrokenPipe);
            let err = anyhow::Error::from(io_err).context("failed to write to client");
            assert!(is_client_disconnect_error(&err));
        }

        #[test]
        fn test_wrapped_connection_reset_error() {
            let io_err = Error::from(ErrorKind::ConnectionReset);
            let err = anyhow::Error::from(io_err).context("failed to read from client");
            assert!(is_client_disconnect_error(&err));
        }

        #[test]
        fn test_deeply_wrapped_error() {
            let io_err = Error::from(ErrorKind::BrokenPipe);
            let err = anyhow::Error::from(io_err)
                .context("inner context")
                .context("outer context");
            assert!(is_client_disconnect_error(&err));
        }

        #[test]
        fn test_custom_io_error_message() {
            let io_err = Error::new(ErrorKind::BrokenPipe, "custom broken pipe");
            let err = anyhow::Error::from(io_err);
            assert!(is_client_disconnect_error(&err));
        }
    }
}
