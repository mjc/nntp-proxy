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
use crate::config::{Config, Memory, RoutingMode, Server};
use crate::metrics::{ConnectionStatsAggregator, MetricsCollector};
use crate::pool::{BufferPool, DeadpoolConnectionProvider};
use crate::router;
use crate::types::BufferSize;

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
    metrics_store: Option<crate::metrics::MetricsStore>,
}

impl NntpProxyBuilder {
    /// Create a new builder with the given configuration
    ///
    /// The routing mode starts from `config.routing.routing_mode` (`Hybrid` by default).
    #[must_use]
    pub const fn new(config: Config) -> Self {
        let routing_mode = config.routing.routing_mode;
        Self {
            config,
            routing_mode,
            buffer_size: None,
            buffer_count: None,
            metrics_store: None,
        }
    }

    /// Set the routing mode
    ///
    /// Available modes:
    /// - `Stateful`: 1:1 client-to-backend mapping
    /// - `PerCommand`: Each command routes to a different backend
    /// - `Hybrid`: Starts in per-command mode, switches to stateful when needed
    #[must_use]
    pub const fn with_routing_mode(mut self, mode: RoutingMode) -> Self {
        self.routing_mode = mode;
        self
    }

    /// Override the configured main buffer pool size (1 MiB by default)
    ///
    /// This affects the size of each buffer in the pool. Larger buffers
    /// can improve throughput for large article transfers but use more memory.
    #[must_use]
    pub const fn with_buffer_pool_size(mut self, size: usize) -> Self {
        self.buffer_size = Some(size);
        self
    }

    /// Override the configured main buffer pool count (50 by default)
    ///
    /// This affects how many buffers are pre-allocated. Should roughly match
    /// the expected number of concurrent connections.
    #[must_use]
    pub const fn with_buffer_pool_count(mut self, count: usize) -> Self {
        self.buffer_count = Some(count);
        self
    }

    /// Restore metrics from a previously-saved store
    ///
    /// If provided, the builder will initialize the `MetricsCollector` with this store,
    /// allowing metrics to persist across proxy restarts.
    #[must_use]
    pub fn with_metrics_store(mut self, store: crate::metrics::MetricsStore) -> Self {
        self.metrics_store = Some(store);
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
        self.config.validate()?;

        let memory = self.config.memory.clone();
        let buffer_count = self.buffer_count.unwrap_or(memory.buffer_pool_count);
        let backend_connection_capacity = self
            .config
            .servers
            .iter()
            .map(|server| server.max_connections.get())
            .sum::<usize>();
        if buffer_count < backend_connection_capacity {
            warn!(
                buffer_pool_count = buffer_count,
                backend_connection_capacity,
                "Buffer pool count is below backend connection capacity; exhausted pools will use visible fallback allocations"
            );
        }

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
                DeadpoolConnectionProvider::from_server_config(
                    server,
                    memory.socket_recv_buffer_size,
                    memory.socket_send_buffer_size,
                )
            })
            .collect();

        let connection_providers = connection_providers?;

        let buffer_pool = BufferPool::new(
            BufferSize::try_new(self.buffer_size.unwrap_or(memory.buffer_pool_size))
                .map_err(|_| anyhow::anyhow!("Buffer size must be non-zero"))?,
            buffer_count,
        )
        .with_capture_pool(memory.capture_pool_size, memory.capture_pool_count);

        let metrics = match self.metrics_store {
            Some(store) => MetricsCollector::with_store(store),
            None => MetricsCollector::new(self.config.servers.len()),
        };

        let adaptive_precheck = self.config.routing.adaptive_precheck;
        let queue_backpressure = self.config.routing.queue.backpressure.clone();

        let backend_strategy = self.config.routing.backend_selection;
        let cache_config = self.config.cache;
        let backend_count = router::BackendCount::try_new(self.config.servers.len())
            .expect("config validation bounds backend count");
        let servers: Arc<[Server]> = self.config.servers.into();

        let router = Arc::new({
            let mut r = router::BackendSelector::with_strategy(backend_strategy)
                .with_queue_backpressure(
                    queue_backpressure.enabled,
                    queue_backpressure.soft_waiters_per_connection_percent,
                    queue_backpressure.hard_waiters_per_connection_percent,
                    queue_backpressure.all_busy_sleep_ms,
                );
            for (idx, provider) in connection_providers.iter().enumerate() {
                let backend_id = r.add_backend(
                    servers[idx].name.clone(),
                    provider.clone(),
                    servers[idx].tier,
                );
                debug_assert_eq!(backend_id.as_index(), idx);
            }
            debug_assert_eq!(r.backend_count(), backend_count);
            r
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
            memory,
        };

        Ok((ctx, cache_config))
    }

    /// Log cache configuration details
    fn log_cache_config(cache_config: &crate::config::Cache, store_article_bodies: bool) {
        if store_article_bodies {
            info!(
                "Article cache enabled: max_capacity={}, ttl={}s (full caching)",
                cache_config.article_cache_capacity,
                cache_config.article_cache_ttl_secs.as_secs()
            );
        } else {
            info!(
                "Backend availability tracking enabled: max_capacity={}, ttl={}s (fixed-size availability index, bodies not cached)",
                cache_config.article_cache_capacity,
                cache_config.article_cache_ttl_secs.as_secs()
            );
        }
    }

    fn warn_if_disk_cache_inactive(
        cache_config: &crate::config::Cache,
        store_article_bodies: bool,
    ) {
        if cache_config.disk.is_some() && !store_article_bodies {
            warn!(
                "Disk cache configured but store_article_bodies=false - disk cache is inactive in availability-only mode."
            );
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
        let (cache, store_article_bodies) = if let Some(cache_config) = &cache_config {
            let capacity = cache_config.article_cache_capacity.as_u64();
            let store_article_bodies = cache_config.store_article_bodies;

            Self::warn_if_disk_cache_inactive(cache_config, store_article_bodies);

            let cache = if !store_article_bodies {
                Arc::new(UnifiedCache::availability(
                    cache_config.article_cache_ttl_secs,
                ))
            } else if let Some(disk_config) = &cache_config.disk {
                let hybrid_config = HybridCacheConfig {
                    memory_capacity: capacity,
                    disk_path: disk_config.path.clone(),
                    disk_capacity: disk_config.capacity.as_u64(),
                    shards: disk_config.shards,
                    compression: disk_config.compression,
                    ttl: cache_config.article_cache_ttl_secs,
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
                    cache_config.article_cache_ttl_secs,
                ))
            };

            Self::log_cache_config(cache_config, store_article_bodies);
            (cache, store_article_bodies)
        } else {
            debug!("Cache not configured, using in-memory availability tracking only");
            (Arc::new(UnifiedCache::availability(Duration::MAX)), false)
        };

        Ok(ctx.into_proxy(cache, store_article_bodies))
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
        let (cache, store_article_bodies) = if let Some(cache_config) = &cache_config {
            let store_article_bodies = cache_config.store_article_bodies;

            Self::warn_if_disk_cache_inactive(cache_config, store_article_bodies);

            if cache_config.disk.is_some() && store_article_bodies {
                warn!(
                    "Disk cache configured but build_sync() called - using memory-only cache. Use build() for disk cache support."
                );
            }

            let cache = if store_article_bodies {
                let capacity = cache_config.article_cache_capacity.as_u64();
                Arc::new(UnifiedCache::memory(
                    capacity,
                    cache_config.article_cache_ttl_secs,
                ))
            } else {
                Arc::new(UnifiedCache::availability(
                    cache_config.article_cache_ttl_secs,
                ))
            };

            Self::log_cache_config(cache_config, store_article_bodies);
            (cache, store_article_bodies)
        } else {
            debug!("Cache not configured, using in-memory availability tracking only");
            (Arc::new(UnifiedCache::availability(Duration::MAX)), false)
        };

        Ok(ctx.into_proxy(cache, store_article_bodies))
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
    servers: Arc<[Server]>,
    router: Arc<router::BackendSelector>,
    auth_handler: Arc<AuthHandler>,
    adaptive_precheck: bool,
    routing_mode: RoutingMode,
    memory: Memory,
}

impl BuildContext {
    /// Construct the final `NntpProxy` from this context and a cache
    pub(super) fn into_proxy(
        self,
        cache: Arc<UnifiedCache>,
        store_article_bodies: bool,
    ) -> NntpProxy {
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
            memory: self.memory,
            store_article_bodies,
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
    use crate::cache::AvailabilityIndex;
    use crate::config::Cache;
    use crate::types::CacheCapacity;

    fn create_test_config() -> Config {
        super::super::tests::create_test_config()
    }

    fn availability_only_config(capacity: u64) -> Config {
        let mut config = create_test_config();
        config.cache = Some(Cache {
            article_cache_capacity: CacheCapacity::try_new(capacity).unwrap(),
            article_cache_ttl_secs: crate::constants::duration_polyfill::from_minutes(1),
            store_article_bodies: false,
            adaptive_precheck: false,
            disk: None,
            availability_index_path: None,
        });
        config
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
    fn test_builder_keeps_configured_buffer_pool_count_as_hard_cap() {
        let mut config = create_test_config();
        config.memory.buffer_pool_count = 4;
        config.servers[0].max_connections = crate::types::MaxConnections::try_new(40).unwrap();
        config.servers[1].max_connections = crate::types::MaxConnections::try_new(50).unwrap();
        config.servers[2].max_connections = crate::types::MaxConnections::try_new(50).unwrap();

        let proxy = NntpProxyBuilder::new(config)
            .build_sync()
            .expect("Failed to build proxy");

        assert_eq!(proxy.buffer_pool().stats().2, 4);
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
    fn test_build_sync_keeps_availability_index_enabled_when_budget_is_too_small() {
        let proxy = NntpProxyBuilder::new(availability_only_config(1))
            .build_sync()
            .expect("Failed to build proxy");

        assert!(!proxy.store_article_bodies);
        assert_eq!(
            proxy.cache.capacity(),
            AvailabilityIndex::fixed_capacity_bytes()
        );
        assert_eq!(
            proxy.cache.weighted_size(),
            AvailabilityIndex::fixed_capacity_bytes()
        );
    }

    #[tokio::test]
    async fn test_build_keeps_availability_index_enabled_when_budget_is_too_small() {
        let proxy = NntpProxyBuilder::new(availability_only_config(1))
            .build()
            .await
            .expect("Failed to build proxy");

        assert!(!proxy.store_article_bodies);
        assert_eq!(
            proxy.cache.capacity(),
            AvailabilityIndex::fixed_capacity_bytes()
        );
        assert_eq!(
            proxy.cache.weighted_size(),
            AvailabilityIndex::fixed_capacity_bytes()
        );
    }

    #[tokio::test]
    async fn test_build_enables_fixed_availability_index_when_budget_can_hold_it() {
        let proxy = NntpProxyBuilder::new(availability_only_config(
            AvailabilityIndex::fixed_capacity_bytes(),
        ))
        .build()
        .await
        .expect("Failed to build proxy");

        assert!(!proxy.store_article_bodies);
        assert_eq!(
            proxy.cache.capacity(),
            AvailabilityIndex::fixed_capacity_bytes()
        );
        assert_eq!(
            proxy.cache.weighted_size(),
            AvailabilityIndex::fixed_capacity_bytes()
        );
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

    #[test]
    fn test_builder_rejects_servers_beyond_availability_bitmap() {
        let base = create_test_config();
        let mut config = Config {
            servers: (0..=crate::cache::MAX_BACKENDS)
                .map(|index| {
                    let mut server = base.servers[0].clone();
                    server.name =
                        crate::types::ServerName::try_new(format!("server-{index}")).unwrap();
                    server
                })
                .collect(),
            ..base
        };
        config.memory.buffer_pool_count = config
            .servers
            .iter()
            .map(|server| server.max_connections.get())
            .sum();

        let result = NntpProxyBuilder::new(config).build_sync();

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("article availability uses a usize bitmap")
        );
    }
}
