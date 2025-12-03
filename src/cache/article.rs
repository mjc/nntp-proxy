//! Article caching implementation using LRU cache with TTL

use crate::protocol::StatusCode;
use crate::types::{BackendId, MessageId};
use moka::future::Cache;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Bitset representing which backends have an article
///
/// Bit N is set if BackendId::from_index(N) has the article.
/// Bit N is clear if BackendId::from_index(N) returned 430.
///
/// Examples (2 backends):
/// - 0b11 = Both backends have article
/// - 0b10 = Backend 1 has it, Backend 0 doesn't
/// - 0b01 = Backend 0 has it, Backend 1 doesn't
/// - 0b00 = No backend has it (or not yet checked)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendAvailability {
    /// Bitset of backends that have this article
    has_article: u8, // u8 supports up to 8 backends (plenty for NNTP)

    /// Bitset of backends we've checked (to distinguish "not checked" from "doesn't have")
    checked: u8,
}

impl BackendAvailability {
    /// Create empty availability (no backends checked)
    #[inline]
    pub const fn new() -> Self {
        Self {
            has_article: 0,
            checked: 0,
        }
    }

    /// Mark backend as having the article (success response: 220, 221, 222, 223)
    #[inline]
    pub fn mark_has(&mut self, backend_id: BackendId) {
        let bit = 1u8 << backend_id.as_index();
        self.has_article |= bit;
        self.checked |= bit;
    }

    /// Mark backend as NOT having the article (430 response)
    ///
    /// **CRITICAL**: Call this for ALL 430 responses - they are authoritative.
    ///
    /// 430 responses are reliable:
    /// - STAT/HEAD 430: Metadata doesn't exist → article almost certainly doesn't exist
    /// - ARTICLE/BODY 430: Article storage doesn't have it → definitely doesn't exist
    /// - Both are trustworthy for marking as missing
    ///
    /// **Contrast with success responses**:
    /// - STAT/HEAD success (223/221): Might be false positive (metadata exists, article doesn't)
    /// - ARTICLE/BODY success (220/222): Always accurate (article definitely exists)
    #[inline]
    pub fn mark_missing(&mut self, backend_id: BackendId) {
        let bit = 1u8 << backend_id.as_index();
        self.has_article &= !bit; // Clear bit
        self.checked |= bit; // Mark as checked
    }

    /// Check if backend has the article
    ///
    /// Returns:
    /// - Some(true) if backend is known to have the article
    /// - Some(false) if backend is known NOT to have the article
    /// - None if backend hasn't been checked yet
    #[inline]
    pub fn has(&self, backend_id: BackendId) -> Option<bool> {
        let bit = 1u8 << backend_id.as_index();
        if self.checked & bit == 0 {
            None // Not yet checked
        } else {
            Some(self.has_article & bit != 0)
        }
    }

    /// Get list of backends that might have the article
    /// (includes backends we haven't checked yet)
    pub fn potential_backends(&self, total_backends: usize) -> Vec<BackendId> {
        let mut result = Vec::new();
        for i in 0..total_backends {
            let backend_id = BackendId::from_index(i);
            match self.has(backend_id) {
                Some(true) => result.push(backend_id), // Known to have
                None => result.push(backend_id),       // Not yet checked
                Some(false) => {}                      // Known to NOT have
            }
        }
        result
    }

    /// Check if all backends have been checked and all returned 430
    pub fn all_backends_missing(&self, total_backends: usize) -> bool {
        let all_checked_mask = (1u8 << total_backends) - 1;
        self.checked == all_checked_mask && self.has_article == 0
    }
}

impl Default for BackendAvailability {
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
    backend_availability: BackendAvailability,

    /// Complete response buffer (Arc for cheap cloning)
    /// Format: "220 <msgid>\r\n<headers>\r\n\r\n<body>\r\n.\r\n"
    /// Status code is always at bytes [0..3]
    buffer: Arc<Vec<u8>>,
}

impl ArticleEntry {
    /// Create from response buffer and backend ID
    ///
    /// The buffer should be a complete NNTP response (status line + data + terminator).
    /// No validation is performed here - caller must ensure buffer is valid.
    pub fn new(buffer: Vec<u8>, backend_id: BackendId) -> Self {
        let mut availability = BackendAvailability::new();
        availability.mark_has(backend_id);

        Self {
            backend_availability: availability,
            buffer: Arc::new(buffer),
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

    /// Get backends that might have this article
    #[inline]
    pub fn potential_backends(&self, total_backends: usize) -> Vec<BackendId> {
        self.backend_availability.potential_backends(total_backends)
    }

    /// Mark backend as not having this article (430 response)
    pub fn mark_backend_missing(&mut self, backend_id: BackendId) {
        self.backend_availability.mark_missing(backend_id);
    }

    /// Mark backend as having this article
    pub fn mark_backend_has(&mut self, backend_id: BackendId) {
        self.backend_availability.mark_has(backend_id);
    }

    /// Check if backend has the article
    #[inline]
    pub fn backend_has(&self, backend_id: BackendId) -> Option<bool> {
        self.backend_availability.has(backend_id)
    }

    /// Check if all backends are known to not have this article
    pub fn all_backends_missing(&self, total_backends: usize) -> bool {
        self.backend_availability
            .all_backends_missing(total_backends)
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

    /// Store an article in the cache
    ///
    /// Accepts any lifetime MessageId and stores using the ID content (without brackets) as key.
    pub async fn insert<'a>(&self, message_id: MessageId<'a>, article: ArticleEntry) {
        // Store using the message ID content without brackets as Arc<str>
        let key: Arc<str> = message_id.without_brackets().into();
        self.cache.insert(key, article).await;
    }

    /// Upsert cache entry (insert or update)
    ///
    /// If entry exists: updates backend availability
    /// If entry doesn't exist: inserts new entry
    pub async fn upsert<'a>(
        &self,
        message_id: MessageId<'a>,
        buffer: Vec<u8>,
        backend_id: BackendId,
    ) {
        let key: Arc<str> = message_id.without_brackets().into();

        if let Some(mut existing) = self.cache.get(key.as_ref()).await {
            // Update existing entry - mark this backend as having the article
            existing.mark_backend_has(backend_id);
            self.cache.insert(key, existing).await;
        } else {
            // Create new entry
            let entry = ArticleEntry::new(buffer, backend_id);
            self.cache.insert(key, entry).await;
        }
    }

    /// Mark backend as not having this article (430 response)
    pub async fn mark_backend_missing<'a>(
        &self,
        message_id: &MessageId<'a>,
        backend_id: BackendId,
    ) {
        let key = message_id.without_brackets();

        if let Some(mut entry) = self.cache.get(key).await {
            entry.mark_backend_missing(backend_id);
            self.cache.insert(key.into(), entry).await;
        } else {
            // No existing entry - we could create availability-only entry here
            // but for now, we'll skip it (backend doesn't have it, we don't cache negatives yet)
        }
    }

    /// Get cache statistics
    pub async fn stats(&self) -> CacheStats {
        CacheStats {
            entry_count: self.cache.entry_count(),
            weighted_size: self.cache.weighted_size(),
        }
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
        let mut avail = BackendAvailability::new();
        let b0 = BackendId::from_index(0);
        let b1 = BackendId::from_index(1);

        assert_eq!(avail.has(b0), None); // Not checked

        avail.mark_has(b0);
        assert_eq!(avail.has(b0), Some(true));
        assert_eq!(avail.has(b1), None); // Still not checked

        avail.mark_missing(b1);
        assert_eq!(avail.has(b1), Some(false));
    }

    #[test]
    fn test_backend_availability_potential_backends() {
        let mut avail = BackendAvailability::new();
        avail.mark_has(BackendId::from_index(0));
        avail.mark_missing(BackendId::from_index(1));

        let candidates = avail.potential_backends(3);
        assert_eq!(
            candidates,
            vec![
                BackendId::from_index(0), // Known to have
                BackendId::from_index(2), // Not yet checked
            ]
        );
    }

    #[test]
    fn test_backend_availability_all_missing() {
        let mut avail = BackendAvailability::new();
        avail.mark_missing(BackendId::from_index(0));
        avail.mark_missing(BackendId::from_index(1));

        assert!(avail.all_backends_missing(2));
        assert!(!avail.all_backends_missing(3)); // Backend 2 not checked
    }

    #[test]
    fn test_article_entry_basic() {
        let buffer = b"220 0 <test@example.com>\r\nBody\r\n.\r\n".to_vec();
        let backend_id = BackendId::from_index(0);
        let entry = ArticleEntry::new(buffer.clone(), backend_id);

        assert_eq!(entry.status_code(), Some(StatusCode::new(220)));
        assert_eq!(entry.buffer().as_ref(), &buffer);
        assert_eq!(entry.backend_has(backend_id), Some(true));
        assert_eq!(entry.backend_has(BackendId::from_index(1)), None);
    }

    #[test]
    fn test_article_entry_mark_backend_missing() {
        let backend0 = BackendId::from_index(0);
        let backend1 = BackendId::from_index(1);
        let mut entry = create_test_article("<test@example.com>", backend0);

        entry.mark_backend_missing(backend1);

        assert_eq!(entry.backend_has(backend0), Some(true));
        assert_eq!(entry.backend_has(backend1), Some(false));
    }

    #[test]
    fn test_article_entry_potential_backends() {
        let backend0 = BackendId::from_index(0);
        let backend1 = BackendId::from_index(1);
        let mut entry = create_test_article("<test@example.com>", backend0);

        entry.mark_backend_missing(backend1);

        let potential = entry.potential_backends(3);
        assert_eq!(
            potential,
            vec![
                BackendId::from_index(0), // Has it
                BackendId::from_index(2), // Not checked
            ]
        );
    }

    #[tokio::test]
    async fn test_arc_str_borrow_lookup() {
        // Create cache with Arc<str> keys
        let cache = ArticleCache::new(100, Duration::from_secs(300));

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
        let article = create_test_article("<article@example.com>", BackendId::from_index(0));

        cache.insert(msgid.clone(), article.clone()).await;

        let retrieved = cache.get(&msgid).await.unwrap();
        assert_eq!(retrieved.buffer().as_ref(), article.buffer().as_ref());
    }

    #[tokio::test]
    async fn test_cache_upsert_new_entry() {
        let cache = ArticleCache::new(100, Duration::from_secs(300));

        let msgid = MessageId::from_borrowed("<test@example.com>").unwrap();
        let buffer = b"220 0 <test@example.com>\r\nBody\r\n.\r\n".to_vec();

        cache
            .upsert(msgid.clone(), buffer.clone(), BackendId::from_index(0))
            .await;

        let retrieved = cache.get(&msgid).await.unwrap();
        assert_eq!(retrieved.buffer().as_ref(), &buffer);
        assert_eq!(retrieved.backend_has(BackendId::from_index(0)), Some(true));
    }

    #[tokio::test]
    async fn test_cache_upsert_existing_entry() {
        let cache = ArticleCache::new(100, Duration::from_secs(300));

        let msgid = MessageId::from_borrowed("<test@example.com>").unwrap();
        let buffer = b"220 0 <test@example.com>\r\nBody\r\n.\r\n".to_vec();

        // Insert with backend 0
        cache
            .upsert(msgid.clone(), buffer.clone(), BackendId::from_index(0))
            .await;

        // Update with backend 1
        cache
            .upsert(msgid.clone(), buffer.clone(), BackendId::from_index(1))
            .await;

        let retrieved = cache.get(&msgid).await.unwrap();
        assert_eq!(retrieved.backend_has(BackendId::from_index(0)), Some(true));
        assert_eq!(retrieved.backend_has(BackendId::from_index(1)), Some(true));
    }

    #[tokio::test]
    async fn test_cache_mark_backend_missing() {
        let cache = ArticleCache::new(100, Duration::from_secs(300));

        let msgid = MessageId::from_borrowed("<test@example.com>").unwrap();
        let article = create_test_article("<test@example.com>", BackendId::from_index(0));

        cache.insert(msgid.clone(), article).await;

        // Mark backend 1 as missing
        cache
            .mark_backend_missing(&msgid, BackendId::from_index(1))
            .await;

        let retrieved = cache.get(&msgid).await.unwrap();
        assert_eq!(retrieved.backend_has(BackendId::from_index(0)), Some(true));
        assert_eq!(retrieved.backend_has(BackendId::from_index(1)), Some(false));
    }

    #[tokio::test]
    async fn test_cache_stats() {
        let cache = ArticleCache::new(1024 * 1024, Duration::from_secs(300)); // 1MB

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
        let cache = ArticleCache::new(1024 * 1024, Duration::from_millis(50)); // 1MB

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
    async fn test_cache_capacity_limit() {
        let cache = ArticleCache::new(500, Duration::from_secs(300)); // 500 bytes total

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
        let cache1 = ArticleCache::new(1024 * 1024, Duration::from_secs(300)); // 1MB
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
        let cache = ArticleCache::new(1024 * 1024, Duration::from_secs(300)); // 1MB

        // Use owned MessageId
        let msgid = MessageId::new("<owned@example.com>".to_string()).unwrap();
        let article = create_test_article("<owned@example.com>", BackendId::from_index(0));

        cache.insert(msgid.clone(), article).await;

        // Retrieve with borrowed MessageId
        let borrowed_msgid = MessageId::from_borrowed("<owned@example.com>").unwrap();
        assert!(cache.get(&borrowed_msgid).await.is_some());
    }
}
