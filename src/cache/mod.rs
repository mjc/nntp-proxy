//! Cache module for NNTP article caching
//!
//! This module provides caching functionality for NNTP articles,
//! allowing the proxy to cache article content and reduce backend load.
//!
//! The `ArticleAvailability` type serves dual purposes:
//! 1. Cache persistence - track which backends have which articles across requests
//! 2. Retry tracking - track which backends tried during 430 retry loops (transient)
//!
//! ## Cache Implementations
//!
//! - [`ArticleCache`] - In-memory only cache using moka (default when no disk config)
//! - [`HybridArticleCache`] - Memory + disk cache using foyer (when `[cache.disk]` configured)
//! - [`UnifiedCache`] - Enum that wraps either cache type with a common interface

mod article;
mod hybrid;

pub use article::{ArticleAvailability, ArticleCache, ArticleEntry, BackendStatus, MAX_BACKENDS};
pub use hybrid::{HybridArticleCache, HybridArticleEntry, HybridCacheConfig, HybridCacheStats};

use crate::types::{BackendId, MessageId};

/// Statistics for cache display in TUI
#[derive(Debug, Clone, Default)]
pub struct CacheDisplayStats {
    /// Number of cached entries
    pub entry_count: u64,
    /// Total size in bytes (memory tier for hybrid)
    pub size_bytes: u64,
    /// Cache hit rate as percentage (0.0 to 100.0)
    pub hit_rate: f64,
    /// Disk cache statistics (only for hybrid cache)
    pub disk: Option<DiskDisplayStats>,
}

/// Disk-tier statistics for hybrid cache
#[derive(Debug, Clone, Default)]
pub struct DiskDisplayStats {
    /// Hits served from disk tier
    pub disk_hits: u64,
    /// Percentage of total hits served from disk
    pub disk_hit_rate: f64,
    /// Configured disk capacity in bytes
    pub capacity: u64,
    /// Bytes actually written to disk
    pub bytes_written: u64,
    /// Bytes read from disk
    pub bytes_read: u64,
    /// Number of write I/O operations
    pub write_ios: u64,
    /// Number of read I/O operations
    pub read_ios: u64,
}

/// Trait for getting cache statistics for TUI display
pub trait CacheStatsProvider: Send + Sync {
    /// Get statistics for TUI display
    fn display_stats(&self) -> CacheDisplayStats;
}

impl CacheStatsProvider for ArticleCache {
    fn display_stats(&self) -> CacheDisplayStats {
        CacheDisplayStats {
            entry_count: self.entry_count(),
            size_bytes: self.weighted_size(),
            hit_rate: self.hit_rate(),
            disk: None,
        }
    }
}

impl CacheStatsProvider for HybridArticleCache {
    fn display_stats(&self) -> CacheDisplayStats {
        let stats = self.stats();
        CacheDisplayStats {
            entry_count: 0,                    // foyer doesn't expose entry count easily
            size_bytes: stats.memory_capacity, // Use configured capacity
            hit_rate: stats.hit_rate(),
            disk: Some(DiskDisplayStats {
                disk_hits: stats.disk_hits,
                disk_hit_rate: stats.disk_hit_rate(),
                capacity: stats.disk_capacity,
                bytes_written: stats.disk_write_bytes,
                bytes_read: stats.disk_read_bytes,
                write_ios: stats.disk_write_ios,
                read_ios: stats.disk_read_ios,
            }),
        }
    }
}

/// Unified cache that can be either memory-only (moka) or hybrid (foyer)
///
/// This enum provides a common interface for both cache implementations,
/// allowing the proxy to use either based on configuration.
#[derive(Debug)]
pub enum UnifiedCache {
    /// Memory-only cache using moka
    Memory(ArticleCache),
    /// Hybrid memory+disk cache using foyer
    Hybrid(HybridArticleCache),
}

impl UnifiedCache {
    /// Create a memory-only cache
    pub fn memory(capacity: u64, ttl: std::time::Duration, cache_articles: bool) -> Self {
        Self::Memory(ArticleCache::new(capacity, ttl, cache_articles))
    }

    /// Create a hybrid cache (async because foyer needs async initialization)
    pub async fn hybrid(config: HybridCacheConfig) -> anyhow::Result<Self> {
        Ok(Self::Hybrid(HybridArticleCache::new(config).await?))
    }

    /// Get an article from the cache
    pub async fn get<'a>(&self, message_id: &MessageId<'a>) -> Option<ArticleEntry> {
        match self {
            Self::Memory(cache) => cache.get(message_id).await,
            Self::Hybrid(cache) => {
                // Convert HybridArticleEntry to ArticleEntry
                cache.get(message_id).await.map(|entry| {
                    // Get availability before consuming buffer
                    let availability = entry.availability();
                    let mut article_entry = ArticleEntry::new(entry.into_buffer());
                    // Copy availability from hybrid entry
                    article_entry.set_availability(availability);
                    article_entry
                })
            }
        }
    }

    /// Upsert (insert or update) an article in the cache
    pub async fn upsert<'a>(
        &self,
        message_id: MessageId<'a>,
        buffer: Vec<u8>,
        backend_id: BackendId,
    ) {
        match self {
            Self::Memory(cache) => cache.upsert(message_id, buffer, backend_id).await,
            Self::Hybrid(cache) => cache.upsert(message_id, buffer, backend_id).await,
        }
    }

    /// Record that a backend returned 430 for this article
    pub async fn record_backend_missing<'a>(
        &self,
        message_id: MessageId<'a>,
        backend_id: BackendId,
    ) {
        match self {
            Self::Memory(cache) => cache.record_backend_missing(message_id, backend_id).await,
            Self::Hybrid(cache) => cache.record_missing(message_id, backend_id).await,
        }
    }

    /// Sync availability information for an article
    pub async fn sync_availability<'a>(
        &self,
        message_id: MessageId<'a>,
        availability: &ArticleAvailability,
    ) {
        match self {
            Self::Memory(cache) => cache.sync_availability(message_id, availability).await,
            Self::Hybrid(cache) => cache.sync_availability(message_id, availability).await,
        }
    }

    /// Get cache capacity
    pub fn capacity(&self) -> u64 {
        match self {
            Self::Memory(cache) => cache.capacity(),
            Self::Hybrid(cache) => cache.stats().memory_capacity,
        }
    }

    /// Get number of cached entries
    pub fn entry_count(&self) -> u64 {
        match self {
            Self::Memory(cache) => cache.entry_count(),
            Self::Hybrid(_cache) => 0, // foyer doesn't expose this easily
        }
    }

    /// Get weighted size in bytes
    pub fn weighted_size(&self) -> u64 {
        match self {
            Self::Memory(cache) => cache.weighted_size(),
            Self::Hybrid(cache) => cache.stats().memory_capacity,
        }
    }

    /// Get cache hit rate
    pub fn hit_rate(&self) -> f64 {
        match self {
            Self::Memory(cache) => cache.hit_rate(),
            Self::Hybrid(cache) => cache.stats().hit_rate(),
        }
    }

    /// Check if this is a hybrid cache (has disk tier)
    pub fn is_hybrid(&self) -> bool {
        matches!(self, Self::Hybrid(_))
    }

    /// Run pending background tasks (for testing)
    ///
    /// Ensures all async maintenance tasks complete for deterministic testing.
    pub async fn sync(&self) {
        match self {
            Self::Memory(cache) => cache.sync().await,
            Self::Hybrid(_) => {} // foyer handles this differently
        }
    }

    /// Close the cache and flush all pending writes
    ///
    /// For hybrid cache, this ensures all enqueued disk writes complete before returning.
    /// For memory cache, this is a no-op (no persistent state).
    pub async fn close(&self) -> anyhow::Result<()> {
        match self {
            Self::Memory(_) => Ok(()), // No persistent state
            Self::Hybrid(cache) => cache.close().await,
        }
    }
}

impl CacheStatsProvider for UnifiedCache {
    fn display_stats(&self) -> CacheDisplayStats {
        match self {
            Self::Memory(cache) => cache.display_stats(),
            Self::Hybrid(cache) => cache.display_stats(),
        }
    }
}
