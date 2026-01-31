//! NNTP Proxy implementation
//!
//! This module contains the main `NntpProxy` struct which orchestrates
//! connection handling, routing, and resource management.
//!
//! ## Module structure
//!
//! - [`builder`]: Builder pattern for constructing proxy instances
//! - [`lifecycle`]: Session lifecycle, idle tracking, and connection helpers

mod builder;
mod lifecycle;

pub use builder::NntpProxyBuilder;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::time::Instant;

use anyhow::Result;
use tracing::info;

use crate::auth::AuthHandler;
use crate::cache::UnifiedCache;
use crate::config::{RoutingMode, Server};
use crate::metrics::{ConnectionStatsAggregator, MetricsCollector};
use crate::pool::{BufferPool, DeadpoolConnectionProvider, prewarm_pools};
use crate::router;

#[derive(Debug, Clone)]
pub struct NntpProxy {
    pub(super) servers: Arc<Vec<Server>>,
    /// Backend selector for round-robin load balancing
    pub(super) router: Arc<router::BackendSelector>,
    /// Connection providers per server - easily swappable implementation
    pub(super) connection_providers: Vec<DeadpoolConnectionProvider>,
    /// Buffer pool for I/O operations
    pub(super) buffer_pool: BufferPool,
    /// Routing mode (Standard, PerCommand, or Hybrid)
    pub(super) routing_mode: RoutingMode,
    /// Authentication handler for client auth interception
    pub(super) auth_handler: Arc<AuthHandler>,
    /// Metrics collector for TUI and monitoring (always enabled)
    pub(super) metrics: MetricsCollector,
    /// Connection statistics aggregator (reduces log spam)
    pub(super) connection_stats: ConnectionStatsAggregator,
    /// Article cache (always present - tracks backend availability even with capacity=0)
    pub(super) cache: Arc<UnifiedCache>,
    /// Whether to cache article bodies (config-driven)
    pub(super) cache_articles: bool,
    /// Whether to use adaptive availability prechecking for STAT/HEAD
    pub(super) adaptive_precheck: bool,
    /// Timestamp (as epoch nanos) when last client disconnected (for idle detection)
    /// Uses epoch nanos since Instant isn't shareable across threads easily
    pub(super) last_activity_nanos: Arc<AtomicU64>,
    /// Number of currently active client connections
    pub(super) active_clients: Arc<AtomicUsize>,
    /// Reference instant for converting nanos to duration
    pub(super) start_instant: Instant,
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
    crate::session::error_classification::ErrorClassifier::is_client_disconnect(e)
}

impl NntpProxy {
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
    /// # #[tokio::main]
    /// # async fn main() -> anyhow::Result<()> {
    /// let config = load_config("config.toml")?;
    /// let proxy = NntpProxy::new(config, RoutingMode::Hybrid).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn new(config: crate::config::Config, routing_mode: RoutingMode) -> Result<Self> {
        NntpProxyBuilder::new(config)
            .with_routing_mode(routing_mode)
            .build()
            .await
    }

    /// Create a new `NntpProxy` synchronously (memory-only cache)
    ///
    /// This version only supports memory-only cache. If disk cache is configured,
    /// it will be ignored. Use [`NntpProxy::new`] for full disk cache support.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use nntp_proxy::{NntpProxy, Config, RoutingMode};
    /// # use nntp_proxy::config::load_config;
    /// # fn main() -> anyhow::Result<()> {
    /// let config = load_config("config.toml")?;
    /// let proxy = NntpProxy::new_sync(config, RoutingMode::Hybrid)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn new_sync(config: crate::config::Config, routing_mode: RoutingMode) -> Result<Self> {
        NntpProxyBuilder::new(config)
            .with_routing_mode(routing_mode)
            .build_sync()
    }

    /// Create a builder for more fine-grained control over proxy configuration
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use nntp_proxy::{NntpProxy, Config, RoutingMode};
    /// # use nntp_proxy::config::load_config;
    /// # #[tokio::main]
    /// # async fn main() -> anyhow::Result<()> {
    /// let config = load_config("config.toml")?;
    /// let proxy = NntpProxy::builder(config)
    ///     .with_routing_mode(RoutingMode::Hybrid)
    ///     .with_buffer_pool_size(512 * 1024)
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn builder(config: crate::config::Config) -> NntpProxyBuilder {
        NntpProxyBuilder::new(config)
    }

    /// Prewarm all connection pools before accepting clients
    /// Creates all connections concurrently and returns when ready
    pub async fn prewarm_connections(&self) -> Result<()> {
        prewarm_pools(&self.connection_providers, &self.servers).await
    }

    /// Gracefully shutdown all connection pools
    pub async fn graceful_shutdown(&self) {
        info!("Initiating graceful shutdown...");

        // Flush cache writes to disk before shutting down
        // With WriteOnInsertion, writes are enqueued async - close() waits for completion
        info!("Flushing disk cache writes...");
        if let Err(e) = self.cache.close().await {
            tracing::warn!("Error closing cache: {}", e);
        }

        info!("Shutting down connection pools...");
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

    /// Get the article cache (always present - capacity 0 if not configured)
    #[must_use]
    #[inline]
    pub fn cache(&self) -> &Arc<UnifiedCache> {
        &self.cache
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
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::config::{Config, Server, health_check_max_per_cycle, health_check_pool_timeout};
    use crate::types::{HostName, MaxConnections, Port, ServerName};

    pub(in crate::proxy) fn create_test_config() -> Config {
        Config {
            servers: vec![
                Server {
                    host: HostName::try_new("server1.example.com".to_string()).unwrap(),
                    port: Port::try_new(119).unwrap(),
                    name: ServerName::try_new("Test Server 1".to_string()).unwrap(),
                    username: None,
                    password: None,
                    max_connections: MaxConnections::try_new(5).unwrap(),
                    use_tls: false,
                    tls_verify_cert: true,
                    tls_cert_path: None,
                    connection_keepalive: None,
                    health_check_max_per_cycle: health_check_max_per_cycle(),
                    health_check_pool_timeout: health_check_pool_timeout(),
                    tier: 0,
                    compress: None,
                    compress_level: None,
                },
                Server {
                    host: HostName::try_new("server2.example.com".to_string()).unwrap(),
                    port: Port::try_new(119).unwrap(),
                    name: ServerName::try_new("Test Server 2".to_string()).unwrap(),
                    username: None,
                    password: None,
                    max_connections: MaxConnections::try_new(8).unwrap(),
                    use_tls: false,
                    tls_verify_cert: true,
                    tls_cert_path: None,
                    connection_keepalive: None,
                    health_check_max_per_cycle: health_check_max_per_cycle(),
                    health_check_pool_timeout: health_check_pool_timeout(),
                    tier: 0,
                    compress: None,
                    compress_level: None,
                },
                Server {
                    host: HostName::try_new("server3.example.com".to_string()).unwrap(),
                    port: Port::try_new(119).unwrap(),
                    name: ServerName::try_new("Test Server 3".to_string()).unwrap(),
                    username: None,
                    password: None,
                    max_connections: MaxConnections::try_new(12).unwrap(),
                    use_tls: false,
                    tls_verify_cert: true,
                    tls_cert_path: None,
                    connection_keepalive: None,
                    health_check_max_per_cycle: health_check_max_per_cycle(),
                    health_check_pool_timeout: health_check_pool_timeout(),
                    tier: 0,
                    compress: None,
                    compress_level: None,
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn test_proxy_creation_with_servers() {
        let config = create_test_config();
        let proxy = Arc::new(
            NntpProxy::new_sync(config, RoutingMode::Stateful).expect("Failed to create proxy"),
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
        let result = NntpProxy::new_sync(config, RoutingMode::Stateful);

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
            NntpProxy::new_sync(config, RoutingMode::Stateful).expect("Failed to create proxy"),
        );

        // Proxy should have a router with backends
        assert_eq!(proxy.router.backend_count(), 3);
    }

    #[test]
    fn test_backward_compatibility_new() {
        // Ensure NntpProxy::new_sync() still works (it uses builder internally)
        let config = create_test_config();
        let proxy = NntpProxy::new_sync(config, RoutingMode::Stateful)
            .expect("Failed to create proxy with new_sync()");

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
