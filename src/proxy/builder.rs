//! Builder pattern for constructing `NntpProxy` instances
//!
//! Contains `NntpProxyBuilder` for configuring and building the proxy,
//! and `BuildContext` as an intermediate state for shared initialization.

use anyhow::{Context, Result};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::auth::AuthHandler;
use crate::cache::{HybridCacheConfig, UnifiedCache};
use crate::config::{Config, RoutingMode, Server};
use crate::constants::buffer::{POOL, POOL_COUNT};
use crate::metrics::{ConnectionStatsAggregator, MetricsCollector};
use crate::pool::{BufferPool, DeadpoolConnectionProvider};
use crate::router;
use crate::types::{self, BufferSize};

use super::NntpProxy;

/// Builder for constructing an `NntpProxy` with optional configuration overrides
///
/// # Examples
///
/// Basic usage with defaults:
/// ```no_run
/// # #[tokio::main]
/// # async fn main() -> anyhow::Result<()> {
/// use nntp_proxy::{NntpProxyBuilder, Config, RoutingMode};
/// use nntp_proxy::config::load_config;
///
/// let config = load_config("config.toml")?;
/// let proxy = NntpProxyBuilder::new(config)
///     .with_routing_mode(RoutingMode::Hybrid)
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// With custom buffer pool size:
/// ```no_run
/// # #[tokio::main]
/// # async fn main() -> anyhow::Result<()> {
/// use nntp_proxy::{NntpProxyBuilder, Config, RoutingMode};
/// use nntp_proxy::config::load_config;
///
/// let config = load_config("config.toml")?;
/// let proxy = NntpProxyBuilder::new(config)
///     .with_routing_mode(RoutingMode::PerCommand)
///     .with_buffer_pool_size(512 * 1024)  // 512KB buffers
///     .with_buffer_pool_count(64)         // 64 buffers
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct NntpProxyBuilder {
    config: Config,
    routing_mode: RoutingMode,
    buffer_size: Option<usize>,
    buffer_count: Option<usize>,
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

    /// Shared initialization logic for both `build()` and `build_sync()`.
    ///
    /// Creates connection providers, buffer pool, metrics, router, and auth handler.
    /// Returns the intermediate `BuildContext` and the cache config (for cache init).
    fn build_infrastructure(self) -> Result<(BuildContext, Option<crate::config::Cache>)> {
        if self.config.servers.is_empty() {
            anyhow::bail!("No servers configured in configuration");
        }

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
            BufferSize::try_new(buffer_size)
                .map_err(|_| anyhow::anyhow!("Buffer size must be non-zero"))?,
            buffer_count,
        );

        let metrics = MetricsCollector::new(self.config.servers.len());

        let adaptive_precheck = self
            .config
            .cache
            .as_ref()
            .map(|c| c.adaptive_precheck)
            .unwrap_or(false);

        let backend_strategy = self.config.proxy.backend_selection;
        let cache_config = self.config.cache;
        let servers = Arc::new(self.config.servers);

        let router = Arc::new({
            use types::BackendId;
            connection_providers.iter().enumerate().fold(
                router::BackendSelector::with_strategy(backend_strategy),
                |mut r, (idx, provider)| {
                    let backend_id = BackendId::from_index(idx);
                    r.add_backend(
                        backend_id,
                        servers[idx].name.clone(),
                        provider.clone(),
                        servers[idx].tier,
                    );
                    r
                },
            )
        });

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

        let ctx = BuildContext {
            connection_providers,
            buffer_pool,
            metrics,
            servers,
            router,
            auth_handler,
            adaptive_precheck,
            routing_mode: self.routing_mode,
        };

        Ok((ctx, cache_config))
    }

    /// Log cache configuration details
    fn log_cache_config(cache_config: &crate::config::Cache, cache_articles: bool) {
        let capacity = cache_config.max_capacity.as_u64();
        if capacity > 0 {
            if cache_articles {
                info!(
                    "Article cache enabled: max_capacity={}, ttl={}s (full caching)",
                    cache_config.max_capacity,
                    cache_config.ttl.as_secs()
                );
            } else {
                info!(
                    "Article cache enabled: max_capacity={}, ttl={}s (availability-only, bodies not cached)",
                    cache_config.max_capacity,
                    cache_config.ttl.as_secs()
                );
            }
        } else {
            info!("Backend availability tracking enabled (cache disabled, capacity=0)");
        }
    }

    /// Build the `NntpProxy` instance
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No servers are configured
    /// - Connection providers cannot be created
    /// - Buffer size is zero
    /// - Hybrid cache initialization fails (if disk cache configured)
    pub async fn build(self) -> Result<NntpProxy> {
        let (ctx, cache_config) = self.build_infrastructure()?;

        // Create article cache (always enabled for availability tracking)
        let (cache, cache_articles) = if let Some(cache_config) = &cache_config {
            let capacity = cache_config.max_capacity.as_u64();
            let cache_articles = cache_config.cache_articles;

            // Check if disk cache is configured
            let cache = if let Some(disk_config) = &cache_config.disk {
                let hybrid_config = HybridCacheConfig {
                    memory_capacity: capacity,
                    disk_path: disk_config.path.clone(),
                    disk_capacity: disk_config.capacity.as_u64(),
                    shards: disk_config.shards,
                    compression: disk_config.compression,
                    ttl: cache_config.ttl,
                    cache_articles,
                };

                info!(
                    "Initializing hybrid cache: memory={}MB, disk={}GB at {:?}",
                    capacity / (1024 * 1024),
                    disk_config.capacity.as_u64() / (1024 * 1024 * 1024),
                    disk_config.path
                );

                Arc::new(
                    UnifiedCache::hybrid(hybrid_config)
                        .await
                        .context("Failed to initialize hybrid disk cache")?,
                )
            } else {
                Arc::new(UnifiedCache::memory(
                    capacity,
                    cache_config.ttl,
                    cache_articles,
                ))
            };

            Self::log_cache_config(cache_config, cache_articles);
            (cache, cache_articles)
        } else {
            debug!("Cache not configured, using availability-only mode (capacity=0)");
            (
                Arc::new(UnifiedCache::memory(0, Duration::from_secs(3600), false)),
                true,
            )
        };

        Ok(ctx.into_proxy(cache, cache_articles))
    }

    /// Build the `NntpProxy` instance (synchronous version)
    ///
    /// This version only supports memory-only cache. If disk cache is configured
    /// in the config, it will be ignored and a warning will be logged.
    /// Use `build()` for full disk cache support.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No servers are configured
    /// - Connection providers cannot be created
    /// - Buffer size is zero
    pub fn build_sync(self) -> Result<NntpProxy> {
        let (ctx, cache_config) = self.build_infrastructure()?;

        // Create article cache (memory-only in sync version)
        let (cache, cache_articles) = if let Some(cache_config) = &cache_config {
            let capacity = cache_config.max_capacity.as_u64();
            let cache_articles = cache_config.cache_articles;

            if cache_config.disk.is_some() {
                warn!(
                    "Disk cache configured but build_sync() called - using memory-only cache. Use build() for disk cache support."
                );
            }

            let cache = Arc::new(UnifiedCache::memory(
                capacity,
                cache_config.ttl,
                cache_articles,
            ));

            Self::log_cache_config(cache_config, cache_articles);
            (cache, cache_articles)
        } else {
            debug!("Cache not configured, using availability-only mode (capacity=0)");
            (
                Arc::new(UnifiedCache::memory(0, Duration::from_secs(3600), false)),
                true,
            )
        };

        Ok(ctx.into_proxy(cache, cache_articles))
    }
}

/// Intermediate state from shared builder initialization
///
/// Contains everything needed to construct an `NntpProxy` except the cache,
/// which differs between `build()` (supports disk) and `build_sync()` (memory-only).
pub(super) struct BuildContext {
    connection_providers: Vec<DeadpoolConnectionProvider>,
    buffer_pool: BufferPool,
    metrics: MetricsCollector,
    servers: Arc<Vec<Server>>,
    router: Arc<router::BackendSelector>,
    auth_handler: Arc<AuthHandler>,
    adaptive_precheck: bool,
    routing_mode: RoutingMode,
}

impl BuildContext {
    /// Construct the final `NntpProxy` from this context and a cache
    pub(super) fn into_proxy(self, cache: Arc<UnifiedCache>, cache_articles: bool) -> NntpProxy {
        NntpProxy {
            servers: self.servers,
            router: self.router,
            connection_providers: self.connection_providers,
            buffer_pool: self.buffer_pool,
            routing_mode: self.routing_mode,
            auth_handler: self.auth_handler,
            metrics: self.metrics,
            connection_stats: ConnectionStatsAggregator::new(),
            cache,
            cache_articles,
            adaptive_precheck: self.adaptive_precheck,
            last_activity_nanos: Arc::new(AtomicU64::new(0)),
            active_clients: Arc::new(AtomicUsize::new(0)),
            start_instant: Instant::now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_config() -> Config {
        super::super::tests::create_test_config()
    }

    #[test]
    fn test_builder_basic_usage() {
        let config = create_test_config();
        let proxy = NntpProxyBuilder::new(config)
            .build_sync()
            .expect("Failed to build proxy");

        assert_eq!(proxy.servers().len(), 3);
        assert_eq!(proxy.router.backend_count(), 3);
    }

    #[test]
    fn test_builder_with_routing_mode() {
        let config = create_test_config();
        let proxy = NntpProxyBuilder::new(config)
            .with_routing_mode(RoutingMode::PerCommand)
            .build_sync()
            .expect("Failed to build proxy");

        assert_eq!(proxy.servers().len(), 3);
    }

    #[test]
    fn test_builder_with_custom_buffer_pool() {
        let config = create_test_config();
        let proxy = NntpProxyBuilder::new(config)
            .with_buffer_pool_size(512 * 1024)
            .with_buffer_pool_count(64)
            .build_sync()
            .expect("Failed to build proxy");

        assert_eq!(proxy.servers().len(), 3);
        // Pool size and count are used internally but not exposed for verification
    }

    #[test]
    fn test_builder_with_all_options() {
        let config = create_test_config();
        let proxy = NntpProxyBuilder::new(config)
            .with_routing_mode(RoutingMode::Hybrid)
            .with_buffer_pool_size(1024 * 1024)
            .with_buffer_pool_count(16)
            .build_sync()
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
        let result = NntpProxyBuilder::new(config).build_sync();

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No servers configured")
        );
    }
}
