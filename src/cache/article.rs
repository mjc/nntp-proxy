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
#[derive(Clone)]
pub struct ArticleCache {
    cache: Arc<Cache<MessageId, CachedArticle>>,
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
    pub async fn get(&self, message_id: &MessageId) -> Option<CachedArticle> {
        self.cache.get(message_id).await
    }

    /// Store an article in the cache
    pub async fn insert(&self, message_id: MessageId, article: CachedArticle) {
        self.cache.insert(message_id, article).await;
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
