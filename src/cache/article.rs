//! Article caching implementation using LRU cache with TTL

use crate::types::MessageId;
use moka::future::Cache;
use std::sync::Arc;
use std::time::Duration;

/// Cached article data
#[derive(Clone, Debug)]
pub struct CachedArticle {
    /// The complete response including status line and article data
    /// Wrapped in Arc for cheap cloning when retrieving from cache
    pub response: Arc<Vec<u8>>,
}

/// Article cache using LRU eviction with TTL
///
/// Uses MessageId<'static> (owned) for storage since cache must outlive input strings
#[derive(Clone)]
pub struct ArticleCache {
    cache: Arc<Cache<MessageId<'static>, CachedArticle>>,
}

impl ArticleCache {
    /// Create a new article cache
    ///
    /// # Arguments
    /// * `max_capacity` - Maximum number of articles to cache
    /// * `ttl` - Time-to-live for cached articles
    pub fn new(max_capacity: u64, ttl: Duration) -> Self {
        let cache = Cache::builder()
            .max_capacity(max_capacity)
            .time_to_live(ttl)
            .build();

        Self {
            cache: Arc::new(cache),
        }
    }

    /// Get an article from the cache
    ///
    /// Accepts any lifetime MessageId since we only need to borrow it for lookup.
    /// 
    /// Note: moka::Cache doesn't support borrowed key lookups, so we must convert
    /// the MessageId to owned form for the lookup. This is a necessary allocation
    /// but only happens on cache access, not during parsing.
    pub async fn get<'a>(&self, message_id: &MessageId<'a>) -> Option<CachedArticle> {
        // Convert to owned MessageId for cache lookup
        // This is necessary because moka::Cache requires owned keys for get()
        let owned_id = message_id.to_owned();
        self.cache.get(&owned_id).await
    }

    /// Store an article in the cache
    ///
    /// Accepts any lifetime MessageId and converts it to owned for storage
    pub async fn insert<'a>(&self, message_id: MessageId<'a>, article: CachedArticle) {
        // Convert to owned MessageId<'static> for storage
        let owned_id = message_id.to_owned();
        self.cache.insert(owned_id, article).await;
    }

    /// Get cache statistics
    pub async fn stats(&self) -> CacheStats {
        CacheStats {
            entry_count: self.cache.entry_count(),
            weighted_size: self.cache.weighted_size(),
        }
    }
}

/// Cache statistics
#[derive(Debug, Clone)]
pub struct CacheStats {
    pub entry_count: u64,
    pub weighted_size: u64,
}
