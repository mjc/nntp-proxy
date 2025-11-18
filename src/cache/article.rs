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
    /// **Zero-allocation**: `without_brackets()` returns `&str`, which moka accepts directly
    /// for `Arc<str>` keys via the `Borrow<str>` trait. This avoids allocating a new `Arc<str>`
    /// for every cache lookup. See `test_arc_str_borrow_lookup` test for verification.
    pub async fn get<'a>(&self, message_id: &MessageId<'a>) -> Option<CachedArticle> {
        // moka::Cache<Arc<str>, V> supports get(&str) via Borrow<str> trait
        // This is zero-allocation: no Arc<str> is created for the lookup
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

    /// Run pending background tasks (for testing)
    ///
    /// Moka performs maintenance tasks (eviction, expiration) asynchronously.
    /// This method ensures all pending tasks complete, useful for deterministic testing.
    pub async fn sync(&self) {
        self.cache.run_pending_tasks().await;
    }
}

/// Cache statistics
#[derive(Debug, Clone)]
pub struct CacheStats {
    pub entry_count: u64,
    pub weighted_size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MessageId;
    use std::time::Duration;

    #[tokio::test]
    async fn test_arc_str_borrow_lookup() {
        // Create cache with Arc<str> keys
        let cache = ArticleCache::new(100, Duration::from_secs(300));

        // Create a MessageId and insert an article
        let msgid = MessageId::from_borrowed("<test123@example.com>").unwrap();
        let article = CachedArticle {
            response: Arc::new(b"220 0 0 <test123@example.com>\r\ntest body\r\n.\r\n".to_vec()),
        };

        cache.insert(msgid.clone(), article.clone()).await;

        // Verify we can retrieve using a different MessageId instance (borrowed)
        // This demonstrates that Arc<str> supports Borrow<str> lookups via &str
        let msgid2 = MessageId::from_borrowed("<test123@example.com>").unwrap();
        let retrieved = cache.get(&msgid2).await;

        assert!(
            retrieved.is_some(),
            "Arc<str> cache should support Borrow<str> lookups"
        );
        assert_eq!(
            retrieved.unwrap().response.as_ref(),
            article.response.as_ref(),
            "Retrieved article should match inserted article"
        );
    }

    #[tokio::test]
    async fn test_cache_miss() {
        let cache = ArticleCache::new(100, Duration::from_secs(300));

        let msgid = MessageId::from_borrowed("<nonexistent@example.com>").unwrap();
        let result = cache.get(&msgid).await;

        assert!(
            result.is_none(),
            "Cache lookup for non-existent key should return None"
        );
    }
}
