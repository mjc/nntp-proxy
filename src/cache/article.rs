//! Article caching implementation using LRU cache with TTL
//!
//! This module provides article caching with per-backend availability tracking.
//!
//! **BACKEND LIMIT**: Maximum 8 backends supported due to u8 bitset optimization.
//! This limit is enforced at config validation time.

use crate::protocol::StatusCode;
use crate::types::{BackendId, MessageId};
use moka::future::Cache;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Maximum number of backends supported by ArticleAvailability bitset
pub const MAX_BACKENDS: usize = 8;

/// Bitset tracking which backends DON'T have an article (returned 430)
///
/// **DEFAULT ASSUMPTION**: All backends have the article until proven otherwise.
///
/// Bit N is SET if BackendId::from_index(N) returned 430 (doesn't have article).
/// Bit N is CLEAR if BackendId::from_index(N) might have article (not yet tried or has it).
///
/// **EXIT CONDITION**: Article found OR all bits are set (all backends tried and returned 430).
///
/// Examples (2 backends):
/// - 0b00 = Assume both have article (initial state)
/// - 0b01 = Backend 0 returned 430, try Backend 1
/// - 0b10 = Backend 1 returned 430, try Backend 0
/// - 0b11 = Both returned 430, give up
///
/// **CRITICAL**: This is used for BOTH initial requests AND 430 retries.
/// The algorithm is identical - keep trying backends until success or all bits set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArticleAvailability {
    /// Bitset of backends that DON'T have this article (returned 430)
    missing: u8, // u8 supports up to 8 backends (plenty for NNTP)
}

impl ArticleAvailability {
    /// Create empty availability - assume all backends have article until proven otherwise
    #[inline]
    pub const fn new() -> Self {
        Self { missing: 0 }
    }

    /// Record that a backend returned 430 (doesn't have the article)
    ///
    /// # Panics (debug builds only)
    /// Panics if backend_id >= 8. Config validation enforces max 8 backends.
    #[inline]
    pub fn record_missing(&mut self, backend_id: BackendId) -> &mut Self {
        let idx = backend_id.as_index();
        debug_assert!(
            idx < MAX_BACKENDS,
            "Backend index {} exceeds MAX_BACKENDS ({})",
            idx,
            MAX_BACKENDS
        );
        self.missing |= 1u8 << idx;
        self
    }

    /// Check if a backend is known to be missing (returned 430)
    ///
    /// # Panics (debug builds only)
    /// Panics if backend_id >= 8. Config validation enforces max 8 backends.
    #[inline]
    pub fn is_missing(&self, backend_id: BackendId) -> bool {
        let idx = backend_id.as_index();
        debug_assert!(
            idx < MAX_BACKENDS,
            "Backend index {} exceeds MAX_BACKENDS ({})",
            idx,
            MAX_BACKENDS
        );
        self.missing & (1u8 << idx) != 0
    }

    /// Check if we should attempt to fetch from this backend
    ///
    /// Returns `true` if backend might have the article (not yet marked missing).
    ///
    /// # Panics (debug builds only)
    /// Panics if backend_id >= 8. Config validation enforces max 8 backends.
    #[inline]
    pub fn should_try(&self, backend_id: BackendId) -> bool {
        !self.is_missing(backend_id)
    }

    /// Check if all backends in the pool have been tried and returned 430
    ///
    /// This is the exit condition for 430 retry loops.
    ///
    /// # Panics (debug builds only)
    /// Panics if backend_count > 8. Config validation enforces max 8 backends.
    #[inline]
    pub fn all_exhausted(&self, backend_count: usize) -> bool {
        debug_assert!(
            backend_count <= MAX_BACKENDS,
            "Backend count {} exceeds MAX_BACKENDS ({})",
            backend_count,
            MAX_BACKENDS
        );
        match backend_count {
            0 => true,
            8 => self.missing == 0xFF,
            n => self.missing & ((1u8 << n) - 1) == (1u8 << n) - 1,
        }
    }

    /// Get an iterator over backends that should still be tried
    ///
    /// Returns backend IDs that haven't been marked missing yet.
    pub fn available_backends(&self, backend_count: usize) -> impl Iterator<Item = BackendId> + '_ {
        (0..backend_count.min(MAX_BACKENDS))
            .map(BackendId::from_index)
            .filter(move |&backend_id| self.should_try(backend_id))
    }

    /// Get the underlying bitset value (for debugging)
    #[inline]
    pub const fn as_u8(&self) -> u8 {
        self.missing
    }
}

impl Default for ArticleAvailability {
    fn default() -> Self {
        Self::new()
    }
}

/// Cache entry for an article
///
/// Stores complete NNTP response buffer plus backend availability tracking.
/// The buffer is validated once on insert, then can be served directly without re-parsing.
#[derive(Clone, Debug)]
pub struct ArticleEntry {
    /// Backend availability bitset (2 bytes)
    backend_availability: ArticleAvailability,

    /// Complete response buffer (Arc for cheap cloning)
    /// Format: "220 <msgid>\r\n<headers>\r\n\r\n<body>\r\n.\r\n"
    /// Status code is always at bytes [0..3]
    buffer: Arc<Vec<u8>>,

    /// Backend ID that originally provided this article (for availability tracking)
    backend_id: BackendId,
}

impl ArticleEntry {
    /// Create from response buffer and backend ID
    ///
    /// The buffer should be a complete NNTP response (status line + data + terminator).
    /// No validation is performed here - caller must ensure buffer is valid.
    ///
    /// Backend availability starts with assumption all backends have the article.
    /// The creating backend is implicitly assumed to have it (since we got it from there).
    pub fn new(buffer: Vec<u8>, backend_id: BackendId) -> Self {
        // Start with fresh availability (assumes all backends have it)
        let availability = ArticleAvailability::new();

        Self {
            backend_availability: availability,
            buffer: Arc::new(buffer),
            backend_id,
        }
    }

    /// Get raw buffer for serving to client
    #[inline]
    pub fn buffer(&self) -> &Arc<Vec<u8>> {
        &self.buffer
    }

    /// Get status code from the response buffer
    ///
    /// Parses the first 3 bytes as the status code.
    /// Returns None if buffer is too short or invalid.
    #[inline]
    pub fn status_code(&self) -> Option<StatusCode> {
        StatusCode::parse(&self.buffer)
    }

    /// Check if we should try fetching from this backend
    ///
    /// Returns false if backend is known to not have this article (returned 430 before).
    #[inline]
    pub fn should_try_backend(&self, backend_id: BackendId) -> bool {
        self.backend_availability.should_try(backend_id)
    }

    /// Record that a backend returned 430 (doesn't have this article)
    pub fn record_backend_missing(&mut self, backend_id: BackendId) {
        self.backend_availability.record_missing(backend_id);
    }

    /// Check if all backends have been tried and none have the article
    pub fn all_backends_exhausted(&self, total_backends: usize) -> bool {
        self.backend_availability.all_exhausted(total_backends)
    }

    /// Get backends that might have this article
    pub fn available_backends(
        &self,
        total_backends: usize,
    ) -> impl Iterator<Item = BackendId> + '_ {
        self.backend_availability.available_backends(total_backends)
    }

    /// Get response buffer (backward compatibility)
    ///
    /// This provides the same interface as the old CachedArticle.response field.
    #[inline]
    pub fn response(&self) -> &Arc<Vec<u8>> {
        &self.buffer
    }
}

/// Article cache using LRU eviction with TTL
///
/// Uses Arc<str> (message ID content without brackets) as key for zero-allocation lookups.
/// Arc<str> implements Borrow<str>, allowing cache.get(&str) without allocation.
#[derive(Clone, Debug)]
pub struct ArticleCache {
    cache: Arc<Cache<Arc<str>, ArticleEntry>>,
    hits: Arc<AtomicU64>,
    misses: Arc<AtomicU64>,
    capacity: u64,
    cache_articles: bool,
}

impl ArticleCache {
    /// Create a new article cache
    ///
    /// # Arguments
    /// * `max_capacity` - Maximum number of articles to cache
    /// * `ttl` - Time-to-live for cached articles
    /// * `cache_articles` - Whether to cache full article bodies (true) or just availability (false)
    pub fn new(max_capacity: u64, ttl: Duration, cache_articles: bool) -> Self {
        // Build cache with byte-based capacity using weigher
        // max_capacity is total bytes allowed
        let cache = Cache::builder()
            .max_capacity(max_capacity)
            .time_to_live(ttl)
            .weigher(|_key: &Arc<str>, entry: &ArticleEntry| -> u32 {
                // Weight is the actual buffer size in bytes
                // Use the buffer() method to get Arc<Vec<u8>>, then len() for the Vec length
                let buffer_size = entry.buffer().len();
                let overhead = 150;
                (buffer_size + overhead).try_into().unwrap_or(u32::MAX)
            })
            .build();

        Self {
            cache: Arc::new(cache),
            hits: Arc::new(AtomicU64::new(0)),
            misses: Arc::new(AtomicU64::new(0)),
            capacity: max_capacity,
            cache_articles,
        }
    }

    /// Get an article from the cache
    ///
    /// Accepts any lifetime MessageId and uses the string content (without brackets) as key.
    ///
    /// **Zero-allocation**: `without_brackets()` returns `&str`, which moka accepts directly
    /// for `Arc<str>` keys via the `Borrow<str>` trait. This avoids allocating a new `Arc<str>`
    /// for every cache lookup. See `test_arc_str_borrow_lookup` test for verification.
    pub async fn get<'a>(&self, message_id: &MessageId<'a>) -> Option<ArticleEntry> {
        // moka::Cache<Arc<str>, V> supports get(&str) via Borrow<str> trait
        // This is zero-allocation: no Arc<str> is created for the lookup
        let result = self.cache.get(message_id.without_brackets()).await;

        if result.is_some() {
            self.hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        } else {
            self.misses
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        result
    }

    /// Insert article into cache
    ///
    /// Accepts any lifetime MessageId and stores using the ID content (without brackets) as key.
    ///
    /// When `cache_articles=false`, stores minimal stub (just status code) for availability tracking.
    /// When `cache_articles=true`, stores the full article body.
    ///
    /// For production code, prefer `upsert` which handles buffer extraction and availability tracking.
    pub async fn insert<'a>(&self, message_id: MessageId<'a>, article: ArticleEntry) {
        // If not caching full articles, create minimal stub from the article's buffer
        let entry = if !self.cache_articles {
            let buffer = article.buffer();
            let stub_buffer = self.create_minimal_stub(&buffer);
            ArticleEntry::new(stub_buffer, article.backend_id)
        } else {
            article
        };

        // Store using the message ID content without brackets as Arc<str>
        let key: Arc<str> = message_id.without_brackets().into();
        self.cache.insert(key, entry).await;
    }

    /// Upsert cache entry (insert or update)
    ///
    /// If entry exists: updates backend availability
    /// If entry doesn't exist: inserts new entry
    ///
    /// When `cache_articles=false`, extracts status code from buffer and stores minimal stub.
    /// When `cache_articles=true`, stores full buffer.
    ///
    /// With new semantics: all backends are assumed to have article until proven otherwise (430).
    /// So we just insert the entry - no need to mark the backend explicitly.
    pub async fn upsert<'a>(
        &self,
        message_id: MessageId<'a>,
        buffer: Vec<u8>,
        backend_id: BackendId,
    ) {
        let key: Arc<str> = message_id.without_brackets().into();

        if let Some(_existing) = self.cache.get(key.as_ref()).await {
            // Entry already exists - nothing to do
            // Availability tracking starts fresh (assume all backends have it)
        } else {
            // Create new entry - strip to stub if not caching bodies
            let storage_buffer = if self.cache_articles {
                buffer
            } else {
                // Extract status code and create minimal stub
                self.create_minimal_stub(&buffer)
            };
            let entry = ArticleEntry::new(storage_buffer, backend_id);
            self.cache.insert(key, entry).await;
        }
    }

    /// Create minimal stub from response buffer
    ///
    /// Extracts the status code from the first line and creates a minimal stub.
    /// Falls back to "200\r\n" if parsing fails.
    fn create_minimal_stub(&self, buffer: &[u8]) -> Vec<u8> {
        // Find first line (status code line)
        if let Some(end) = buffer.iter().position(|&b| b == b'\n') {
            // Extract status code (first 3 digits)
            if end >= 3 {
                let code = &buffer[..3];
                // Verify it's actually digits
                if code.iter().all(|&b| b.is_ascii_digit()) {
                    return format!("{}\r\n", String::from_utf8_lossy(code)).into_bytes();
                }
            }
        }
        // Fallback if we can't parse
        b"200\r\n".to_vec()
    }

    /// Record that a backend returned 430 for this article
    pub async fn record_backend_missing<'a>(
        &self,
        message_id: MessageId<'a>,
        backend_id: BackendId,
    ) {
        let key = message_id.without_brackets();

        if let Some(mut entry) = self.cache.get(key).await {
            entry.record_backend_missing(backend_id);
            self.cache.insert(key.into(), entry).await;
        }
        // Note: We don't cache negative-only entries (no article body)
    }

    /// Get cache statistics
    pub async fn stats(&self) -> CacheStats {
        CacheStats {
            entry_count: self.cache.entry_count(),
            weighted_size: self.cache.weighted_size(),
        }
    }

    /// Get maximum cache capacity
    #[inline]
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Get current number of cached entries (synchronous)
    #[inline]
    pub fn entry_count(&self) -> u64 {
        self.cache.entry_count()
    }

    /// Get current weighted size in bytes (synchronous)
    #[inline]
    pub fn weighted_size(&self) -> u64 {
        self.cache.weighted_size()
    }

    /// Get cache hit rate as percentage (0.0 to 100.0)
    #[inline]
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

    fn create_test_article(msgid: &str, backend_id: BackendId) -> ArticleEntry {
        let buffer = format!("220 0 {}\r\nSubject: Test\r\n\r\nBody\r\n.\r\n", msgid).into_bytes();
        ArticleEntry::new(buffer, backend_id)
    }

    #[test]
    fn test_backend_availability_basic() {
        let mut avail = ArticleAvailability::new();
        let b0 = BackendId::from_index(0);
        let b1 = BackendId::from_index(1);

        // Default: assume all backends have it
        assert!(avail.should_try(b0));
        assert!(avail.should_try(b1));

        // Record b0 as missing (returned 430)
        avail.record_missing(b0);
        assert!(!avail.should_try(b0)); // Should not try again
        assert!(avail.should_try(b1)); // Still should try

        // Record b1 as missing too
        avail.record_missing(b1);
        assert!(!avail.should_try(b1));
    }

    #[test]
    fn test_backend_availability_all_exhausted() {
        let mut avail = ArticleAvailability::new();

        // None missing yet
        assert!(!avail.all_exhausted(2));
        assert!(!avail.all_exhausted(3));

        // Record backends 0 and 1 as missing
        avail.record_missing(BackendId::from_index(0));
        avail.record_missing(BackendId::from_index(1));

        // All 2 backends exhausted
        assert!(avail.all_exhausted(2));

        // But not all 3 backends (backend 2 still untried)
        assert!(!avail.all_exhausted(3));
    }

    #[test]
    fn test_article_entry_basic() {
        let buffer = b"220 0 <test@example.com>\r\nBody\r\n.\r\n".to_vec();
        let backend_id = BackendId::from_index(0);
        let entry = ArticleEntry::new(buffer.clone(), backend_id);

        assert_eq!(entry.status_code(), Some(StatusCode::new(220)));
        assert_eq!(entry.buffer().as_ref(), &buffer);

        // Default: should try all backends
        assert!(entry.should_try_backend(backend_id));
        assert!(entry.should_try_backend(BackendId::from_index(1)));
    }

    #[test]
    fn test_article_entry_record_backend_missing() {
        let backend0 = BackendId::from_index(0);
        let backend1 = BackendId::from_index(1);
        let mut entry = create_test_article("<test@example.com>", backend0);

        // Initially should try both
        assert!(entry.should_try_backend(backend0));
        assert!(entry.should_try_backend(backend1));

        // Record backend1 as missing (430 response)
        entry.record_backend_missing(backend1);

        // Should still try backend0, but not backend1
        assert!(entry.should_try_backend(backend0));
        assert!(!entry.should_try_backend(backend1));
    }

    #[test]
    fn test_article_entry_all_backends_exhausted() {
        let backend0 = BackendId::from_index(0);
        let backend1 = BackendId::from_index(1);
        let mut entry = create_test_article("<test@example.com>", backend0);

        // Not all exhausted yet
        assert!(!entry.all_backends_exhausted(2));

        // Record both as missing
        entry.record_backend_missing(backend0);
        entry.record_backend_missing(backend1);

        // Now all 2 backends are exhausted
        assert!(entry.all_backends_exhausted(2));
    }

    #[tokio::test]
    async fn test_arc_str_borrow_lookup() {
        // Create cache with Arc<str> keys
        let cache = ArticleCache::new(100, Duration::from_secs(300), true);

        // Create a MessageId and insert an article
        let msgid = MessageId::from_borrowed("<test123@example.com>").unwrap();
        let article = create_test_article("<test123@example.com>", BackendId::from_index(0));

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
            retrieved.unwrap().buffer().as_ref(),
            article.buffer().as_ref(),
            "Retrieved article should match inserted article"
        );
    }

    #[tokio::test]
    async fn test_cache_hit_miss() {
        let cache = ArticleCache::new(100, Duration::from_secs(300), true);

        let msgid = MessageId::from_borrowed("<nonexistent@example.com>").unwrap();
        let result = cache.get(&msgid).await;

        assert!(
            result.is_none(),
            "Cache lookup for non-existent key should return None"
        );
    }

    #[tokio::test]
    async fn test_cache_insert_and_retrieve() {
        let cache = ArticleCache::new(10, Duration::from_secs(300), true);

        let msgid = MessageId::from_borrowed("<article@example.com>").unwrap();
        let article = create_test_article("<article@example.com>", BackendId::from_index(0));

        cache.insert(msgid.clone(), article.clone()).await;

        let retrieved = cache.get(&msgid).await.unwrap();
        assert_eq!(retrieved.buffer().as_ref(), article.buffer().as_ref());
    }

    #[tokio::test]
    async fn test_cache_upsert_new_entry() {
        let cache = ArticleCache::new(100, Duration::from_secs(300), true);

        let msgid = MessageId::from_borrowed("<test@example.com>").unwrap();
        let buffer = b"220 0 <test@example.com>\r\nBody\r\n.\r\n".to_vec();

        cache
            .upsert(msgid.clone(), buffer.clone(), BackendId::from_index(0))
            .await;

        let retrieved = cache.get(&msgid).await.unwrap();
        assert_eq!(retrieved.buffer().as_ref(), &buffer);
        // Default: should try all backends
        assert!(retrieved.should_try_backend(BackendId::from_index(0)));
        assert!(retrieved.should_try_backend(BackendId::from_index(1)));
    }

    #[tokio::test]
    async fn test_cache_upsert_existing_entry() {
        let cache = ArticleCache::new(100, Duration::from_secs(300), true);

        let msgid = MessageId::from_borrowed("<test@example.com>").unwrap();
        let buffer = b"220 0 <test@example.com>\r\nBody\r\n.\r\n".to_vec();

        // Insert with backend 0
        cache
            .upsert(msgid.clone(), buffer.clone(), BackendId::from_index(0))
            .await;

        // Update with backend 1 - does nothing (entry already exists)
        cache
            .upsert(msgid.clone(), buffer.clone(), BackendId::from_index(1))
            .await;

        let retrieved = cache.get(&msgid).await.unwrap();
        // Default: should try all backends
        assert!(retrieved.should_try_backend(BackendId::from_index(0)));
        assert!(retrieved.should_try_backend(BackendId::from_index(1)));
    }

    #[tokio::test]
    async fn test_record_backend_missing() {
        let cache = ArticleCache::new(100, Duration::from_secs(300), true);

        let msgid = MessageId::from_borrowed("<test@example.com>").unwrap();
        let article = create_test_article("<test@example.com>", BackendId::from_index(0));

        cache.insert(msgid.clone(), article).await;

        // Record backend 1 as missing
        cache
            .record_backend_missing(msgid.clone(), BackendId::from_index(1))
            .await;

        let retrieved = cache.get(&msgid).await.unwrap();
        // Backend 0 should still be tried, backend 1 should not
        assert!(retrieved.should_try_backend(BackendId::from_index(0)));
        assert!(!retrieved.should_try_backend(BackendId::from_index(1)));
    }

    #[tokio::test]
    async fn test_cache_stats() {
        let cache = ArticleCache::new(1024 * 1024, Duration::from_secs(300), true); // 1MB

        // Initial stats
        let stats = cache.stats().await;
        assert_eq!(stats.entry_count, 0);

        // Insert one article
        let msgid = MessageId::from_borrowed("<test@example.com>").unwrap();
        let article = create_test_article("<test@example.com>", BackendId::from_index(0));
        cache.insert(msgid, article).await;

        // Wait for background tasks
        cache.sync().await;

        // Check stats again
        let stats = cache.stats().await;
        assert_eq!(stats.entry_count, 1);
    }

    #[tokio::test]
    async fn test_cache_ttl_expiration() {
        let cache = ArticleCache::new(1024 * 1024, Duration::from_millis(50), true); // 1MB

        let msgid = MessageId::from_borrowed("<expire@example.com>").unwrap();
        let article = create_test_article("<expire@example.com>", BackendId::from_index(0));

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
    async fn test_insert_respects_cache_articles_flag() {
        // Test with cache_articles=false - should store minimal stub
        let cache_stub = ArticleCache::new(1024 * 1024, Duration::from_secs(300), false);

        let msgid = MessageId::from_borrowed("<test@example.com>").unwrap();
        let full_article = create_test_article("<test@example.com>", BackendId::from_index(0));
        let full_size = full_article.buffer().len();

        cache_stub.insert(msgid.clone(), full_article).await;
        cache_stub.sync().await;

        let retrieved = cache_stub.get(&msgid).await.unwrap();
        let stub_size = retrieved.buffer().len();

        // Stub should be much smaller than full article (just "220\r\n")
        assert!(
            stub_size < 10,
            "Stub size {} should be < 10 bytes",
            stub_size
        );
        assert!(
            stub_size < full_size,
            "Stub {} should be smaller than full {}",
            stub_size,
            full_size
        );

        // Test with cache_articles=true - should store full article
        let cache_full = ArticleCache::new(1024 * 1024, Duration::from_secs(300), true);

        let msgid2 = MessageId::from_borrowed("<test2@example.com>").unwrap();
        let full_article2 = create_test_article("<test2@example.com>", BackendId::from_index(0));
        let original_size = full_article2.buffer().len();

        cache_full.insert(msgid2.clone(), full_article2).await;
        cache_full.sync().await;

        let retrieved2 = cache_full.get(&msgid2).await.unwrap();

        // Should store full article
        assert_eq!(retrieved2.buffer().len(), original_size);
    }

    #[tokio::test]
    async fn test_cache_capacity_limit() {
        let cache = ArticleCache::new(500, Duration::from_secs(300), true); // 500 bytes total

        // Insert 3 articles (exceeds capacity)
        for i in 1..=3 {
            let msgid_str = format!("<article{}@example.com>", i);
            let msgid = MessageId::new(msgid_str).unwrap();
            let article = create_test_article(&msgid.to_string(), BackendId::from_index(0));
            cache.insert(msgid, article).await;
            cache.sync().await; // Force eviction
        }

        // Wait for eviction to complete
        tokio::time::sleep(Duration::from_millis(10)).await;
        cache.sync().await;

        let stats = cache.stats().await;
        assert!(
            stats.entry_count <= 3,
            "Cache should have at most 3 entries with 500 byte capacity"
        );
    }

    #[tokio::test]
    async fn test_article_entry_clone() {
        let article = create_test_article("<test@example.com>", BackendId::from_index(0));

        let cloned = article.clone();
        assert_eq!(article.buffer().as_ref(), cloned.buffer().as_ref());
        assert!(Arc::ptr_eq(article.buffer(), cloned.buffer()));
    }

    #[tokio::test]
    async fn test_cache_clone() {
        let cache1 = ArticleCache::new(1024 * 1024, Duration::from_secs(300), true); // 1MB
        let cache2 = cache1.clone();

        let msgid = MessageId::from_borrowed("<test@example.com>").unwrap();
        let article = create_test_article("<test@example.com>", BackendId::from_index(0));

        cache1.insert(msgid.clone(), article).await;
        cache1.sync().await;

        // Should be accessible from cloned cache
        assert!(cache2.get(&msgid).await.is_some());
    }

    #[tokio::test]
    async fn test_cache_with_owned_message_id() {
        let cache = ArticleCache::new(1024 * 1024, Duration::from_secs(300), true); // 1MB

        // Use owned MessageId
        let msgid = MessageId::new("<owned@example.com>".to_string()).unwrap();
        let article = create_test_article("<owned@example.com>", BackendId::from_index(0));

        cache.insert(msgid.clone(), article).await;

        // Retrieve with borrowed MessageId
        let borrowed_msgid = MessageId::from_borrowed("<owned@example.com>").unwrap();
        assert!(cache.get(&borrowed_msgid).await.is_some());
    }
}
