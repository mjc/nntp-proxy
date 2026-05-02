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
mod availability;
pub(super) mod entry_helpers;
mod hybrid;
mod hybrid_codec;
pub mod ttl;

pub mod mock_hybrid;

pub use article::{
    ArticleCache, ArticleEntry, CachedArticlePayloadSlices, CachedArticleResponse,
    CachedPayloadKind, CachedPayloadLen,
};
pub use availability::{ArticleAvailability, BackendStatus, MAX_BACKENDS};
pub use hybrid::{HybridArticleCache, HybridCacheConfig, HybridCacheStats};
pub use hybrid_codec::{CacheableStatusCode, HybridArticleEntry};

// Internal helper re-exported for session handlers
pub(crate) use entry_helpers::{extract_chunked_status_line, extract_status_line};

use crate::types::{BackendId, MessageId};
use smallvec::SmallVec;

/// Owned response storage passed across the async cache ingest boundary.
///
/// Hot-path code should hand off one of these owned forms directly instead of
/// flattening into a fresh `Vec<u8>` before spawning cache work.
#[derive(Debug)]
pub enum CacheBuffer {
    Vec(Vec<u8>),
    Pooled(crate::pool::PooledBuffer),
    Chunked(crate::pool::ChunkedResponse),
    Small(SmallVec<[u8; 128]>),
}

impl CacheBuffer {
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Vec(buf) => buf.len(),
            Self::Pooled(buf) => buf.len(),
            Self::Chunked(buf) => buf.len(),
            Self::Small(buf) => buf.len(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Cold-path flattening for cache backends that still require contiguous bytes.
    #[must_use]
    pub fn into_vec(self) -> Vec<u8> {
        match self {
            Self::Vec(buf) => buf,
            Self::Pooled(buf) => buf.as_ref().to_vec(),
            Self::Chunked(buf) => buf.to_vec(),
            Self::Small(buf) => buf.into_vec(),
        }
    }
}

impl PartialEq for CacheBuffer {
    fn eq(&self, other: &Self) -> bool {
        fn chunks<'a>(buf: &'a CacheBuffer) -> Box<dyn Iterator<Item = &'a [u8]> + 'a> {
            match buf {
                CacheBuffer::Vec(v) => Box::new(std::iter::once(v.as_slice())),
                CacheBuffer::Pooled(v) => Box::new(std::iter::once(v.as_ref())),
                CacheBuffer::Chunked(v) => Box::new(v.iter_chunks()),
                CacheBuffer::Small(v) => Box::new(std::iter::once(v.as_slice())),
            }
        }

        if self.len() != other.len() {
            return false;
        }

        let left = chunks(self).flat_map(|chunk| chunk.iter().copied());
        let mut right = chunks(other).flat_map(|chunk| chunk.iter().copied());
        left.eq(&mut right)
    }
}

impl Eq for CacheBuffer {}

impl From<Vec<u8>> for CacheBuffer {
    fn from(value: Vec<u8>) -> Self {
        Self::Vec(value)
    }
}

impl From<crate::pool::PooledBuffer> for CacheBuffer {
    fn from(value: crate::pool::PooledBuffer) -> Self {
        Self::Pooled(value)
    }
}

impl From<crate::pool::ChunkedResponse> for CacheBuffer {
    fn from(value: crate::pool::ChunkedResponse) -> Self {
        Self::Chunked(value)
    }
}

impl From<SmallVec<[u8; 128]>> for CacheBuffer {
    fn from(value: SmallVec<[u8; 128]>) -> Self {
        Self::Small(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cache_buffer_equality_spans_storage_forms() {
        let bytes = b"220 0 <test@example.com>\r\nBody\r\n.\r\n";
        let pool =
            crate::pool::BufferPool::new(crate::types::BufferSize::try_new(1024).unwrap(), 1)
                .with_capture_pool(8, 4);

        let mut pooled = pool.acquire().await;
        pooled.copy_from_slice(bytes);

        let mut chunked = crate::pool::ChunkedResponse::default();
        chunked.extend_from_slice(&pool, bytes);

        let small = SmallVec::<[u8; 128]>::from_slice(bytes);

        assert_eq!(
            CacheBuffer::Vec(bytes.to_vec()),
            CacheBuffer::Pooled(pooled)
        );
        assert_eq!(CacheBuffer::Chunked(chunked), CacheBuffer::Small(small));
    }
}

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
    #[must_use]
    pub fn memory(capacity: u64, ttl: std::time::Duration, cache_articles: bool) -> Self {
        Self::Memory(ArticleCache::new(capacity, ttl, cache_articles))
    }

    /// Create a hybrid cache (async because foyer needs async initialization)
    pub async fn hybrid(config: HybridCacheConfig) -> anyhow::Result<Self> {
        Ok(Self::Hybrid(HybridArticleCache::new(config).await?))
    }

    /// Get an article from the cache
    pub async fn get(&self, message_id: &MessageId<'_>) -> Option<ArticleEntry> {
        match self {
            Self::Memory(cache) => cache.get(message_id).await,
            Self::Hybrid(cache) => cache
                .get(message_id)
                .await
                .map(|entry| entry.to_article_entry()),
        }
    }

    /// Upsert (insert or update) an article in the cache
    ///
    /// The tier is used for tier-aware TTL calculation (higher tier = longer TTL).
    pub async fn upsert<B>(
        &self,
        message_id: MessageId<'_>,
        buffer: B,
        backend_id: BackendId,
        tier: ttl::CacheTier,
    ) where
        B: Into<CacheBuffer>,
    {
        let buffer = buffer.into();
        match self {
            Self::Memory(cache) => cache.upsert(message_id, buffer, backend_id, tier).await,
            Self::Hybrid(cache) => cache.upsert(message_id, buffer, backend_id, tier).await,
        }
    }

    /// Record that a backend returned 430 for this article
    pub async fn record_backend_missing(&self, message_id: MessageId<'_>, backend_id: BackendId) {
        match self {
            Self::Memory(cache) => cache.record_backend_missing(message_id, backend_id).await,
            Self::Hybrid(cache) => cache.record_missing(message_id, backend_id).await,
        }
    }

    /// Sync availability information for an article
    pub async fn sync_availability(
        &self,
        message_id: MessageId<'_>,
        availability: &ArticleAvailability,
    ) {
        match self {
            Self::Memory(cache) => cache.sync_availability(message_id, availability).await,
            Self::Hybrid(cache) => cache.sync_availability(message_id, availability).await,
        }
    }

    /// Get cache capacity
    #[must_use]
    pub fn capacity(&self) -> u64 {
        match self {
            Self::Memory(cache) => cache.capacity(),
            Self::Hybrid(cache) => cache.stats().memory_capacity,
        }
    }

    /// Get number of cached entries
    #[must_use]
    pub fn entry_count(&self) -> u64 {
        match self {
            Self::Memory(cache) => cache.entry_count(),
            Self::Hybrid(_cache) => 0, // foyer doesn't expose this easily
        }
    }

    /// Get weighted size in bytes
    #[must_use]
    pub fn weighted_size(&self) -> u64 {
        match self {
            Self::Memory(cache) => cache.weighted_size(),
            Self::Hybrid(cache) => cache.stats().memory_capacity,
        }
    }

    /// Get cache hit rate
    #[must_use]
    pub fn hit_rate(&self) -> f64 {
        match self {
            Self::Memory(cache) => cache.hit_rate(),
            Self::Hybrid(cache) => cache.stats().hit_rate(),
        }
    }

    /// Check if this is a hybrid cache (has disk tier)
    #[must_use]
    pub const fn is_hybrid(&self) -> bool {
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
