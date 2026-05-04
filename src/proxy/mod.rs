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
    pub(super) servers: Arc<[Server]>,
    /// Backend selector for round-robin load balancing
    pub(super) router: Arc<router::BackendSelector>,
    /// Connection providers per server - easily swappable implementation
    pub(super) connection_providers: Vec<DeadpoolConnectionProvider>,
    /// Buffer pool for I/O operations
    pub(super) buffer_pool: BufferPool,
    /// Routing mode (Standard, `PerCommand`, or Hybrid)
    pub(super) routing_mode: RoutingMode,
    /// Authentication handler for client auth interception
    pub(super) auth_handler: Arc<AuthHandler>,
    /// Metrics collector for TUI and monitoring (always enabled)
    pub(super) metrics: MetricsCollector,
    /// Connection statistics aggregator (reduces log spam)
    pub(super) connection_stats: ConnectionStatsAggregator,
    /// Article cache (always present - at minimum a fixed-size availability index)
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
    ///
    /// # Errors
    /// Returns any configuration, pool-construction, cache-initialization, or
    /// backend preflight error encountered while building the proxy.
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
    ///
    /// # Errors
    /// Returns any configuration or pool-construction error encountered while
    /// building the memory-only proxy.
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
    pub const fn builder(config: crate::config::Config) -> NntpProxyBuilder {
        NntpProxyBuilder::new(config)
    }

    /// Prewarm all connection pools before accepting clients
    /// Creates all connections concurrently and returns when ready
    ///
    /// # Errors
    /// Returns any connection-establishment or backend-authentication error
    /// encountered while prewarming the backend pools.
    pub async fn prewarm_connections(&self) -> Result<()> {
        prewarm_pools(&self.connection_providers, &self.servers).await
    }

    /// Gracefully shutdown all connection pools
    pub async fn graceful_shutdown(&self) {
        info!("Initiating graceful shutdown...");

        // Flush cache writes to disk before shutting down
        // With WriteOnInsertion, writes are enqueued async - close() waits for completion
        // Use a timeout: foyer's close() can hang if the runtime is winding down
        info!("Flushing disk cache writes...");
        match tokio::time::timeout(crate::constants::timeout::CACHE_CLOSE, self.cache.close()).await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!("Error closing cache: {}", e),
            Err(_) => tracing::warn!(
                "Cache close timed out after {}s, forcing shutdown",
                crate::constants::timeout::CACHE_CLOSE.as_secs()
            ),
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
    pub const fn router(&self) -> &Arc<router::BackendSelector> {
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
    pub const fn buffer_pool(&self) -> &BufferPool {
        &self.buffer_pool
    }

    /// Get the article cache (always present - capacity 0 if not configured)
    #[must_use]
    #[inline]
    pub const fn cache(&self) -> &Arc<UnifiedCache> {
        &self.cache
    }

    /// Get the metrics collector
    #[must_use]
    #[inline]
    pub const fn metrics(&self) -> &MetricsCollector {
        &self.metrics
    }

    /// Get connection stats aggregator
    #[must_use]
    #[inline]
    pub const fn connection_stats(&self) -> &ConnectionStatsAggregator {
        &self.connection_stats
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::config::{Config, Server};
    use crate::types::{MaxConnections, Port};

    pub(in crate::proxy) fn create_test_config() -> Config {
        Config {
            servers: vec![
                Server::builder("server1.example.com", Port::try_new(119).unwrap())
                    .name("Test Server 1")
                    .max_connections(MaxConnections::try_new(5).unwrap())
                    .build()
                    .unwrap(),
                Server::builder("server2.example.com", Port::try_new(119).unwrap())
                    .name("Test Server 2")
                    .max_connections(MaxConnections::try_new(8).unwrap())
                    .build()
                    .unwrap(),
                Server::builder("server3.example.com", Port::try_new(119).unwrap())
                    .name("Test Server 3")
                    .max_connections(MaxConnections::try_new(12).unwrap())
                    .build()
                    .unwrap(),
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
    fn test_new_sync_constructs_proxy() {
        // Ensure NntpProxy::new_sync() constructs through the builder path.
        let config = create_test_config();
        let proxy = NntpProxy::new_sync(config, RoutingMode::Stateful)
            .expect("Failed to create proxy with new_sync()");

        assert_eq!(proxy.servers().len(), 3);
        assert_eq!(proxy.router.backend_count(), 3);
    }
}
