//! Article caching implementation using LRU cache with TTL

use crate::types::MessageId;
use moka::future::Cache;
use std::sync::atomic::{AtomicU64, Ordering};
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
    hits: Arc<AtomicU64>,
    misses: Arc<AtomicU64>,
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
            hits: Arc::new(AtomicU64::new(0)),
            misses: Arc::new(AtomicU64::new(0)),
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
        let result = self.cache.get(message_id.without_brackets()).await;
        if result.is_some() {
            self.hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
        }
        result
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

    /// Get cache hit rate as percentage (0.0 - 100.0)
    #[must_use]
    pub fn hit_rate(&self) -> f64 {
        let hits = self.hits.load(Ordering::Relaxed);
        let misses = self.misses.load(Ordering::Relaxed);
        let total = hits + misses;
        if total == 0 {
            0.0
        } else {
            (hits as f64 / total as f64) * 100.0
        }
    }

    /// Get total number of cache lookups
    #[must_use]
    pub fn total_lookups(&self) -> u64 {
        self.hits.load(Ordering::Relaxed) + self.misses.load(Ordering::Relaxed)
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

    #[tokio::test]
    async fn test_cache_insert_and_retrieve() {
        let cache = ArticleCache::new(10, Duration::from_secs(300));

        let msgid = MessageId::from_borrowed("<article@example.com>").unwrap();
        let response_data =
            b"220 1 <article@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
        let article = CachedArticle {
            response: Arc::new(response_data.clone()),
        };

        cache.insert(msgid.clone(), article).await;

        let retrieved = cache.get(&msgid).await.unwrap();
        assert_eq!(retrieved.response.as_ref(), &response_data);
    }

    #[tokio::test]
    async fn test_cache_stats() {
        let cache = ArticleCache::new(10, Duration::from_secs(300));

        // Initial stats
        let stats = cache.stats().await;
        assert_eq!(stats.entry_count, 0);

        // Insert one article
        let msgid = MessageId::from_borrowed("<test@example.com>").unwrap();
        let article = CachedArticle {
            response: Arc::new(b"220 test\r\n.\r\n".to_vec()),
        };
        cache.insert(msgid, article).await;

        // Wait for background tasks
        cache.sync().await;

        // Check stats again
        let stats = cache.stats().await;
        assert_eq!(stats.entry_count, 1);
    }

    #[tokio::test]
    async fn test_cache_ttl_expiration() {
        let cache = ArticleCache::new(10, Duration::from_millis(50));

        let msgid = MessageId::from_borrowed("<expire@example.com>").unwrap();
        let article = CachedArticle {
            response: Arc::new(b"220 test\r\n.\r\n".to_vec()),
        };

        cache.insert(msgid.clone(), article).await;

        // Should be cached immediately
        assert!(cache.get(&msgid).await.is_some());

        // Wait for TTL expiration + sync
        tokio::time::sleep(Duration::from_millis(100)).await;
        cache.sync().await;

        // Should be expired
        assert!(cache.get(&msgid).await.is_none());
    }

    #[tokio::test]
    async fn test_cache_capacity_limit() {
        let cache = ArticleCache::new(2, Duration::from_secs(300));

        // Insert 3 articles (exceeds capacity of 2)
        for i in 1..=3 {
            let msgid_str = format!("<article{}@example.com>", i);
            let msgid = MessageId::new(msgid_str).unwrap();
            let article = CachedArticle {
                response: Arc::new(format!("220 {}\r\n.\r\n", i).into_bytes()),
            };
            cache.insert(msgid, article).await;
            cache.sync().await; // Force eviction
        }

        // Wait for eviction to complete
        tokio::time::sleep(Duration::from_millis(10)).await;
        cache.sync().await;

        let stats = cache.stats().await;
        assert!(
            stats.entry_count <= 2,
            "Cache should evict to maintain capacity"
        );
    }

    #[tokio::test]
    async fn test_cached_article_clone() {
        let response = Arc::new(b"220 test\r\n.\r\n".to_vec());
        let article = CachedArticle {
            response: response.clone(),
        };

        let cloned = article.clone();
        assert_eq!(article.response.as_ref(), cloned.response.as_ref());
        assert!(Arc::ptr_eq(&article.response, &cloned.response));
    }

    #[tokio::test]
    async fn test_cache_clone() {
        let cache1 = ArticleCache::new(10, Duration::from_secs(300));
        let cache2 = cache1.clone();

        let msgid = MessageId::from_borrowed("<test@example.com>").unwrap();
        let article = CachedArticle {
            response: Arc::new(b"220 test\r\n.\r\n".to_vec()),
        };

        cache1.insert(msgid.clone(), article).await;
        cache1.sync().await;

        // Should be accessible from cloned cache
        assert!(cache2.get(&msgid).await.is_some());
    }

    #[tokio::test]
    async fn test_cache_with_owned_message_id() {
        let cache = ArticleCache::new(10, Duration::from_secs(300));

        // Use owned MessageId
        let msgid = MessageId::new("<owned@example.com>".to_string()).unwrap();
        let article = CachedArticle {
            response: Arc::new(b"220 test\r\n.\r\n".to_vec()),
        };

        cache.insert(msgid.clone(), article).await;

        // Retrieve with borrowed MessageId
        let borrowed_msgid = MessageId::from_borrowed("<owned@example.com>").unwrap();
        assert!(cache.get(&borrowed_msgid).await.is_some());
    }
}
