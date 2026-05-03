//! Hybrid memory+disk article cache using foyer
//!
//! This module provides a two-tier cache that stores hot articles in memory
//! and spills to disk when memory capacity is exceeded. The entry type and
//! its foyer codec are in [`super::hybrid_codec`].
//!
//! # Architecture
//!
//! ```text
//! Client Request → Memory Cache (fast, limited)
//!                         ↓ miss
//!                  Disk Cache (slower, larger)
//!                         ↓ miss
//!                  Backend Server
//! ```
//!
//! # Usage
//!
//! Configure disk cache in your config.toml:
//! ```toml
//! [cache]
//! max_capacity = "256mb"
//!
//! [cache.disk]
//! path = "/var/cache/nntp-proxy"
//! capacity = "10gb"
//! ```
//!
//! # Compression Options
//!
//! The disk cache supports three compression codecs:
//! - **LZ4** (default): Fast compression (~60% reduction), minimal CPU overhead, SIMD-accelerated
//! - **Zstd**: Better compression ratio, moderate CPU overhead, SIMD-accelerated
//! - **None**: No compression, fastest but largest disk usage
//!
//! The LZ4 and Zstd codecs use auto-detected SIMD (SSE2/AVX2/AVX512) for maximum performance;
//! the `None` codec disables compression entirely and performs no codec-level SIMD work.
//!
//! # Performance Characteristics
//!
//! - Memory tier: ~1μs access latency, bounded by configured memory capacity
//! - Disk tier: ~100μs-1ms access latency, bounded by disk capacity
//! - Automatic promotion: Frequently accessed disk entries promoted to memory
//! - Compression: Reduces disk usage (LZ4: ~60%, Zstd: ~65%+ for typical NNTP articles)

use crate::config::CompressionCodec;
use crate::protocol::StatusCode;
use crate::types::{BackendId, MessageId};
use foyer::{
    BlockEngineConfig, DeviceBuilder, FsDeviceBuilder, HybridCache, HybridCacheBuilder,
    HybridCachePolicy, LruConfig, PsyncIoEngineConfig, RecoverMode, Source, Spawner,
};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tracing::{debug, info, warn};

use super::availability::ArticleAvailability;
use super::hybrid_codec::{CacheableStatusCode, DiskCachedArticle};
use super::ttl;

const HYBRID_CACHE_NAME: &str = "nntp-article-cache-v3";

/// Check available disk space at the given path using df command
fn check_available_space(path: &Path) -> Option<u64> {
    // Try to use statfs on Linux/Unix
    #[cfg(unix)]
    {
        // Get filesystem stats using a known working approach
        // We'll create a temp file to trigger actual space check
        if let Ok(temp_file) = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path.join(".space_check_tmp"))
        {
            drop(temp_file);
            let _ = std::fs::remove_file(path.join(".space_check_tmp"));
        }
    }
    None // For now, skip the check - let foyer handle it
}

/// Configuration for hybrid cache
#[derive(Debug, Clone)]
pub struct HybridCacheConfig {
    /// Memory cache capacity in bytes
    pub memory_capacity: u64,
    /// Disk cache capacity in bytes
    pub disk_capacity: u64,
    /// Path to disk cache directory
    pub disk_path: std::path::PathBuf,
    /// Time-to-live for cached articles (not directly used by foyer, but kept for API consistency)
    pub ttl: Duration,
    /// Whether to cache full article bodies
    pub cache_articles: bool,
    /// Compression codec for disk storage (lz4, zstd, or none)
    pub compression: CompressionCodec,
    /// Number of shards for concurrent access
    pub shards: usize,
}

impl Default for HybridCacheConfig {
    fn default() -> Self {
        Self {
            memory_capacity: 256 * 1024 * 1024,     // 256 MB memory
            disk_capacity: 10 * 1024 * 1024 * 1024, // 10 GB disk
            disk_path: std::path::PathBuf::from("/var/cache/nntp-proxy"),
            ttl: Duration::from_hours(1), // 1 hour
            cache_articles: true,
            compression: CompressionCodec::Lz4,
            shards: 16, // Match indexer shards for consistent lock contention
        }
    }
}

/// Hybrid article cache with memory and disk tiers
///
/// Uses foyer's `HybridCache` for automatic memory→disk spillover.
/// Hot articles stay in memory, cold articles spill to disk.
///
/// Supports tier-aware TTL: entries from higher tier backends get longer TTLs.
/// Formula: `effective_ttl = base_ttl * (2 ^ tier)`
///
/// Note: Keys are stored as `String` (message ID without brackets) because
/// foyer requires keys to implement the `Code` trait for disk serialization.
pub struct HybridArticleCache {
    cache: HybridCache<String, DiskCachedArticle>,
    hits: AtomicU64,
    misses: AtomicU64,
    disk_hits: AtomicU64,
    config: HybridCacheConfig,
    /// Base TTL in milliseconds (used for tier-aware expiration via `effective_ttl`)
    ttl_millis: ttl::CacheTtlMillis,
}

impl std::fmt::Debug for HybridArticleCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HybridArticleCache")
            .field("hits", &self.hits.load(Ordering::Relaxed))
            .field("misses", &self.misses.load(Ordering::Relaxed))
            .field("disk_hits", &self.disk_hits.load(Ordering::Relaxed))
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl HybridArticleCache {
    /// Create a new hybrid cache with the given configuration
    ///
    /// This will create the disk cache directory if it doesn't exist.
    pub async fn new(config: HybridCacheConfig) -> anyhow::Result<Self> {
        // Ensure disk cache directory exists
        std::fs::create_dir_all(&config.disk_path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::Other && e.to_string().contains("No space left") {
                anyhow::anyhow!(
                    "Failed to create cache directory '{}': DISK FULL - No space left on device. \
                     Free up disk space or choose a different disk_path in config.",
                    config.disk_path.display()
                )
            } else {
                anyhow::anyhow!(
                    "Failed to create cache directory '{}': {}",
                    config.disk_path.display(),
                    e
                )
            }
        })?;

        // Check available disk space before initializing
        if let Ok(_metadata) = std::fs::metadata(&config.disk_path)
            && let Some(available_bytes) = check_available_space(&config.disk_path)
        {
            let required_bytes = config.disk_capacity;
            if available_bytes < required_bytes {
                anyhow::bail!(
                    "Insufficient disk space for cache:\n\
                     Path: {}\n\
                     Required: {} GB\n\
                     Available: {} GB\n\
                     Solution: Free up {} GB or reduce 'disk_capacity' in config.",
                    config.disk_path.display(),
                    required_bytes / (1024 * 1024 * 1024),
                    available_bytes / (1024 * 1024 * 1024),
                    (required_bytes - available_bytes) / (1024 * 1024 * 1024)
                );
            }
        }

        // Use FsDevice - optimized for filesystem use (uses pread/pwrite properly)
        // Block engine controls partition sizes via block_size
        let disk_capacity_usize: usize = config.disk_capacity.try_into().map_err(|_| {
            anyhow::anyhow!(
                "Disk capacity {} bytes too large for platform (max {} bytes)",
                config.disk_capacity,
                usize::MAX
            )
        })?;

        let device = FsDeviceBuilder::new(&config.disk_path)
            .with_capacity(disk_capacity_usize)
            .build()
            .map_err(|e| {
                if e.to_string().contains("No space left") || e.to_string().contains("ENOSPC") {
                    anyhow::anyhow!(
                        "Failed to initialize disk cache at '{}': DISK FULL - No space left on device.\n\
                         Required: {} GB\n\
                         Solution: Free up disk space or reduce 'disk_capacity' in config.",
                        config.disk_path.display(),
                        config.disk_capacity / (1024 * 1024 * 1024)
                    )
                } else {
                    anyhow::anyhow!("Failed to initialize disk cache: {e}")
                }
            })?;

        let memory_capacity_usize: usize = config
            .memory_capacity
            .try_into()
            .map_err(|_| anyhow::anyhow!("Memory capacity too large for platform"))?;

        // Block size controls the disk partition file size used by foyer's block engine.
        // Smaller blocks = faster reclaim cycles but more FDs (~160 for 10GB at 64MB).

        // Create a dedicated tokio runtime for foyer disk I/O.
        // With WriteOnInsertion, every cache insert triggers a background disk write.
        // A separate runtime prevents disk I/O from starving the main proxy event loop.
        let foyer_runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(8)
            .max_blocking_threads(16)
            .thread_name("foyer-disk-io")
            .enable_all()
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to create foyer runtime: {e}"))?;

        let mut builder = HybridCacheBuilder::new()
            .with_name(HYBRID_CACHE_NAME)
            .with_policy(HybridCachePolicy::WriteOnInsertion)
            .memory(memory_capacity_usize)
            .with_shards(config.shards)
            .with_eviction_config(LruConfig {
                high_priority_pool_ratio: 0.1,
            })
            .with_weighter(|_key: &String, value: &DiskCachedArticle| value.payload_len().get())
            .storage()
            .with_io_engine_config(PsyncIoEngineConfig::new())
            .with_engine_config(
                BlockEngineConfig::new(device)
                    .with_block_size(64 * 1024 * 1024) // 64MB blocks - faster reclaim, ~160 FDs for 10GB
                    .with_indexer_shards(16)
                    .with_flushers(4)
                    .with_reclaimers(2),
            )
            .with_recover_mode(RecoverMode::Quiet)
            .with_spawner(Spawner::from(foyer_runtime));

        match config.compression {
            CompressionCodec::None => {
                // No compression - builder already has no compression by default
            }
            CompressionCodec::Lz4 => {
                builder = builder.with_compression(foyer::Compression::Lz4);
            }
            CompressionCodec::Zstd => {
                builder = builder.with_compression(foyer::Compression::Zstd);
            }
        }

        let cache = builder.build().await.map_err(|e| {
            if e.to_string().contains("No space left") || e.to_string().contains("ENOSPC") {
                anyhow::anyhow!(
                    "Failed to build disk cache: DISK FULL - No space left on device.\n\
                     Cache path: {}\n\
                     Memory size: {} MB\n\
                     Disk size: {} GB\n\
                     Solution: Free up disk space or reduce cache sizes in config.",
                    config.disk_path.display(),
                    config.memory_capacity / (1024 * 1024),
                    config.disk_capacity / (1024 * 1024 * 1024)
                )
            } else {
                anyhow::anyhow!("Failed to build hybrid cache: {e}")
            }
        })?;

        info!(
            memory_mb = config.memory_capacity / (1024 * 1024),
            disk_gb = config.disk_capacity / (1024 * 1024 * 1024),
            path = %config.disk_path.display(),
            compression = %config.compression,
            "Hybrid article cache initialized"
        );

        let ttl_millis = ttl::CacheTtlMillis::from_duration(config.ttl);
        Ok(Self {
            cache,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            disk_hits: AtomicU64::new(0),
            config,
            ttl_millis,
        })
    }

    /// Get an article from the cache
    ///
    /// Checks memory first, then disk. Returns None if not found in either tier.
    /// Applies tier-aware TTL expiration - higher tier entries get longer TTLs.
    pub(crate) async fn get(&self, message_id: &MessageId<'_>) -> Option<DiskCachedArticle> {
        let key = message_id.without_brackets().to_string();
        let result = self.cache.get(&key).await;

        match result {
            Ok(Some(entry)) => {
                let cloned = entry.value().clone();

                // Check tier-aware TTL expiration
                if cloned.is_expired(self.ttl_millis) {
                    // Expired by tier-aware TTL - treat as cache miss
                    // We intentionally do NOT remove from foyer. Eviction decisions are delegated
                    // to foyer's capacity-based and LRU policies. Expired entries may linger on
                    // disk until capacity pressure forces eviction, avoiding explicit removal overhead.
                    self.misses.fetch_add(1, Ordering::Relaxed);
                    return None;
                }

                self.hits.fetch_add(1, Ordering::Relaxed);
                let source = entry.source();
                // Track disk hits for monitoring
                if source == Source::Disk {
                    self.disk_hits.fetch_add(1, Ordering::Relaxed);
                }
                Some(cloned)
            }
            Ok(None) => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
            Err(e) => {
                warn!(error = %e, "Error reading from hybrid cache");
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Insert or update an article in the cache
    ///
    /// With `WriteOnInsertion` policy, articles are written to disk immediately
    /// in a background task (non-blocking).
    ///
    /// **UPSERT SEMANTICS**: Never overwrites a larger semantic payload with a smaller one.
    /// A cached full article (220/222 response) must not be replaced by STAT availability.
    ///
    /// The tier is stored with the entry for tier-aware TTL calculation.
    pub async fn upsert_ingest(
        &self,
        message_id: MessageId<'_>,
        buffer: impl Into<super::CacheIngestResponse>,
        backend_id: BackendId,
        tier: super::ttl::CacheTier,
    ) {
        let buffer = buffer.into();
        let key = message_id.without_brackets().to_string();
        let mut entry = if self.config.cache_articles {
            let buffer_len = buffer.len();
            let Some(entry) = DiskCachedArticle::from_ingest_response_with_tier(buffer, tier)
            else {
                warn!(msg_id = %key, buffer_len, "Cannot cache: invalid status code");
                return;
            };
            entry
        } else {
            let Some(status_code) = buffer
                .status_code()
                .and_then(|code| CacheableStatusCode::try_from(code.as_u16()).ok())
            else {
                warn!(
                    msg_id = %key,
                    "Cannot cache: invalid status code"
                );
                return;
            };
            DiskCachedArticle::availability_only(status_code, tier)
        };
        let entry_len = entry.payload_len();

        // Check for existing entry - don't overwrite larger semantic payloads with smaller ones.
        if let Ok(Some(existing)) = self.cache.get(&key).await {
            let existing_len = existing.value().payload_len();
            if existing_len > entry_len {
                // Existing entry is larger - just update availability info and refresh TTL
                let mut updated = existing.value().clone();
                updated.record_backend_has(backend_id);
                // Refresh timestamp on successful upsert to extend tier-aware TTL
                updated.timestamp = ttl::CacheTimestampMillis::now();
                self.cache.insert(key.clone(), updated);
                debug!(
                    msg_id = %key,
                    existing_bytes = existing_len.get(),
                    new_bytes = entry_len.get(),
                    "Hybrid cache upsert: preserved larger existing entry, updated availability"
                );
                return;
            }
        }

        entry.record_backend_has(backend_id);
        self.cache.insert(key.clone(), entry);
        if self.config.cache_articles {
            debug!(msg_id = %key, stored_bytes = entry_len.get(), tier = tier.get(), "Hybrid cache upsert");
        } else {
            debug!(msg_id = %key, stored_bytes = entry_len.get(), tier = tier.get(), "Hybrid cache upsert (availability only)");
        }
    }

    /// Record that a backend doesn't have an article (430 response)
    pub async fn record_missing(&self, message_id: MessageId<'_>, backend_id: BackendId) {
        let key = message_id.without_brackets().to_string();

        // Get existing entry or create a typed missing entry.
        let entry = if let Ok(Some(existing)) = self.cache.get(&key).await {
            let mut updated = existing.value().clone();
            updated.record_backend_missing(backend_id);
            updated
        } else {
            let mut entry = DiskCachedArticle::missing(super::ttl::CacheTier::new(0));
            entry.record_backend_missing(backend_id);
            entry
        };

        self.cache.insert(key, entry);
    }

    /// Record successful backend availability without storing response payload bytes.
    pub async fn record_has_status(
        &self,
        message_id: MessageId<'_>,
        status_code: StatusCode,
        backend_id: BackendId,
        tier: super::ttl::CacheTier,
    ) {
        let Ok(cacheable_status) = CacheableStatusCode::try_from(status_code.as_u16()) else {
            warn!(
                msg_id = %message_id,
                status_code = status_code.as_u16(),
                "Cannot record availability: invalid cache status code"
            );
            return;
        };

        let key = message_id.without_brackets().to_string();
        let entry = if let Ok(Some(existing)) = self.cache.get(&key).await {
            let mut updated = existing.value().clone();
            updated.record_backend_has_status(cacheable_status, backend_id, tier);
            updated
        } else {
            let mut entry = DiskCachedArticle::availability_only(cacheable_status, tier);
            entry.record_backend_has(backend_id);
            entry
        };

        self.cache.insert(key, entry);
    }

    /// Sync availability information at the end of a retry loop
    ///
    /// This is called ONCE at the end of a retry loop to persist all the
    /// backends that returned 430 during this request. Much more efficient
    /// than calling `record_missing` for each backend individually.
    ///
    /// IMPORTANT: Only creates a missing entry if ALL checked backends returned 430.
    /// If any backend successfully provided the article, we skip creating an entry
    /// (the actual article will be cached via upsert, which may race with this call).
    pub async fn sync_availability(
        &self,
        message_id: MessageId<'_>,
        availability: &ArticleAvailability,
    ) {
        // Only sync if we actually tried some backends
        if availability.checked_bits() == 0 {
            return;
        }

        let key = message_id.without_brackets().to_string();

        // Get existing entry or conditionally create a missing entry.
        let updated_entry = match self.cache.get(&key).await {
            Ok(Some(existing)) => {
                // Merge availability into existing entry
                let mut entry = existing.value().clone();
                entry.availability.merge_from(availability);
                Some(entry)
            }
            _ => {
                // No existing entry - only create a missing entry if all checked backends returned 430.
                if availability.any_backend_has_article() {
                    // A backend successfully provided the article.
                    // Don't create a missing entry - let upsert() handle it with the real article data.
                    None
                } else {
                    // All checked backends returned 430 - create typed missing entry.
                    let mut entry = DiskCachedArticle::missing(super::ttl::CacheTier::new(0));
                    entry.availability = *availability;
                    self.misses.fetch_add(1, Ordering::Relaxed);
                    Some(entry)
                }
            }
        };

        if let Some(entry) = updated_entry {
            self.cache.insert(key, entry);
        }
    }

    /// Get cache statistics
    pub fn stats(&self) -> HybridCacheStats {
        let foyer_stats = self.cache.statistics();
        HybridCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            disk_hits: self.disk_hits.load(Ordering::Relaxed),
            memory_capacity: self.config.memory_capacity,
            disk_capacity: self.config.disk_capacity,
            disk_write_bytes: foyer_stats.disk_write_bytes() as u64,
            disk_read_bytes: foyer_stats.disk_read_bytes() as u64,
            disk_write_ios: foyer_stats.disk_write_ios() as u64,
            disk_read_ios: foyer_stats.disk_read_ios() as u64,
        }
    }

    /// Close the cache gracefully
    ///
    /// Flushes pending writes to disk before returning.
    /// This waits for all enqueued disk writes to complete.
    pub async fn close(&self) -> anyhow::Result<()> {
        self.cache
            .close()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to close cache: {e}"))
    }
}

/// Statistics for hybrid cache
#[derive(Debug, Clone)]
pub struct HybridCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub disk_hits: u64,
    pub memory_capacity: u64,
    pub disk_capacity: u64,
    /// Bytes written to disk (from foyer Statistics)
    pub disk_write_bytes: u64,
    /// Bytes read from disk (from foyer Statistics)
    pub disk_read_bytes: u64,
    /// Number of write I/O operations
    pub disk_write_ios: u64,
    /// Number of read I/O operations
    pub disk_read_ios: u64,
}

impl HybridCacheStats {
    /// Calculate hit rate as a percentage
    #[must_use]
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            (self.hits as f64 / total as f64) * 100.0
        }
    }

    /// Calculate disk hit rate (hits from disk vs total hits)
    #[must_use]
    pub fn disk_hit_rate(&self) -> f64 {
        if self.hits == 0 {
            0.0
        } else {
            (self.disk_hits as f64 / self.hits as f64) * 100.0
        }
    }
}

#[cfg(test)]
impl HybridArticleCache {
    /// Create a memory-only cache for testing.
    ///
    /// Uses `NoopIoEngineConfig` without a block engine config so foyer falls back
    /// to `NoopEngineConfig` — a trivially synchronous engine that spawns zero
    /// background tasks. Safe with any tokio runtime flavor.
    pub async fn new_memory_only(memory_capacity: u64) -> anyhow::Result<Self> {
        Self::new_memory_only_with_cache_articles(memory_capacity, true).await
    }

    async fn new_memory_only_with_cache_articles(
        memory_capacity: u64,
        cache_articles: bool,
    ) -> anyhow::Result<Self> {
        use foyer::NoopIoEngineConfig;

        let memory_capacity_usize: usize = memory_capacity
            .try_into()
            .map_err(|_| anyhow::anyhow!("Memory capacity too large for platform"))?;

        // NoopIoEngineConfig + no with_engine_config() call → foyer uses NoopEngineConfig,
        // which is completely synchronous and never spawns background flusher/reclaimer tasks.
        let builder = HybridCacheBuilder::new()
            .with_name("nntp-article-cache-test")
            .with_policy(HybridCachePolicy::WriteOnEviction)
            .memory(memory_capacity_usize)
            .with_shards(1)
            .with_eviction_config(LruConfig {
                high_priority_pool_ratio: 0.1,
            })
            .with_weighter(|_key: &String, value: &DiskCachedArticle| value.payload_len().get())
            .storage()
            .with_io_engine_config(Box::new(NoopIoEngineConfig) as Box<dyn foyer::IoEngineConfig>);

        let cache = builder.build().await?;

        let config = HybridCacheConfig {
            memory_capacity,
            disk_capacity: 0, // No disk in memory-only mode
            disk_path: std::path::PathBuf::new(),
            ttl: Duration::from_hours(1),
            cache_articles,
            compression: CompressionCodec::None,
            shards: 1,
        };

        Ok(Self {
            cache,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            disk_hits: AtomicU64::new(0),
            config,
            ttl_millis: ttl::CacheTtlMillis::new(3600 * 1000), // 1 hour in milliseconds
        })
    }
}

#[cfg(test)]
mod tests {
    //! Cache-level integration tests for `HybridArticleCache`
    //!
    use super::*;

    #[test]
    fn hybrid_cache_name_cold_invalidates_old_disk_formats() {
        assert_eq!(HYBRID_CACHE_NAME, "nntp-article-cache-v3");
    }

    #[tokio::test]
    async fn test_hybrid_cache_basic() {
        let cache = HybridArticleCache::new_memory_only(1024 * 1024)
            .await
            .unwrap();

        // Insert an article
        let msg_id = MessageId::from_borrowed("<test123@example.com>").unwrap();
        let buffer = b"220 0 <test123@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
        cache
            .upsert_ingest(msg_id, buffer.clone(), BackendId::from_index(0), 0.into())
            .await;

        // Retrieve it
        let msg_id = MessageId::from_borrowed("<test123@example.com>").unwrap();
        let entry = cache.get(&msg_id).await.unwrap();
        let response = entry
            .cached_response_for(crate::protocol::RequestKind::Article, msg_id.as_str())
            .unwrap();
        let mut rendered = Vec::with_capacity(response.wire_len().get());
        response.write_to(&mut rendered).await.unwrap();
        assert_eq!(rendered, buffer);
        assert!(entry.should_try_backend(BackendId::from_index(0)));

        // Check stats
        let stats = cache.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 0);

        cache.close().await.unwrap();
    }

    #[tokio::test]
    async fn test_hybrid_cache_availability_only_stores_no_payload() {
        let cache = HybridArticleCache::new_memory_only_with_cache_articles(1024 * 1024, false)
            .await
            .unwrap();

        let msg_id = MessageId::from_borrowed("<test123@example.com>").unwrap();
        let buffer = b"220 0 <test123@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
        cache
            .upsert_ingest(msg_id.clone(), buffer, BackendId::from_index(0), 0.into())
            .await;

        let entry = cache.get(&msg_id).await.unwrap();
        assert_eq!(entry.payload_len(), 0);
        assert!(entry.should_try_backend(BackendId::from_index(0)));

        cache.close().await.unwrap();
    }

    #[tokio::test]
    async fn test_hybrid_cache_availability_tracking() {
        let cache = HybridArticleCache::new_memory_only(1024 * 1024)
            .await
            .unwrap();

        // Record a 430 response
        let msg_id = MessageId::from_borrowed("<missing@example.com>").unwrap();
        cache.record_missing(msg_id, BackendId::from_index(0)).await;

        // Check availability
        let msg_id = MessageId::from_borrowed("<missing@example.com>").unwrap();
        let entry = cache.get(&msg_id).await.unwrap();
        assert_eq!(
            entry.payload_len().get(),
            0,
            "missing hybrid cache entries must not retain response payload bytes"
        );
        assert!(!entry.should_try_backend(BackendId::from_index(0)));
        assert!(entry.should_try_backend(BackendId::from_index(1)));

        cache.close().await.unwrap();
    }
}
