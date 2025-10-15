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
/// Uses Arc<str> (message ID content without brackets) as key for zero-allocation lookups.
/// Arc<str> implements Borrow<str>, allowing cache.get(&str) without allocation.
#[derive(Clone)]
pub struct ArticleCache {
    cache: Arc<Cache<Arc<str>, CachedArticle>>,
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
    /// Accepts any lifetime MessageId and uses the string content (without brackets) as key.
    /// 
    /// **Zero-allocation**: Uses `without_brackets()` to get &str for direct lookup.
    /// This avoids allocating an owned MessageId for every cache access.
    pub async fn get<'a>(&self, message_id: &MessageId<'a>) -> Option<CachedArticle> {
        // Use without_brackets() to get the ID content as &str
        // moka::Cache supports &str lookups for String keys (via Borrow<str>)
        self.cache.get(message_id.without_brackets()).await
    }

    /// Store an article in the cache
    ///
    /// Accepts any lifetime MessageId and stores using the ID content (without brackets) as key.
    pub async fn insert<'a>(&self, message_id: MessageId<'a>, article: CachedArticle) {
        // Store using the message ID content without brackets as Arc<str>
        let key: Arc<str> = message_id.without_brackets().into();
        self.cache.insert(key, article).await;
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
