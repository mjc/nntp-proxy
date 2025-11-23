//! Article location cache
//!
//! Tracks which backends have which articles to optimize routing decisions.
//! When adaptive_precheck is enabled, performs parallel STAT to all backends
//! on cache miss to discover article availability.

use std::sync::Arc;

use moka::sync::Cache;

use crate::types::{BackendId, MessageId};

/// Bitmap tracking which backends have an article (supports up to 64 backends)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendAvailability(u64);

impl BackendAvailability {
    /// Create empty availability map (no backends have the article)
    #[must_use]
    #[inline]
    pub const fn new() -> Self {
        Self(0)
    }

    /// Mark a backend as having the article
    #[inline]
    pub fn mark_available(&mut self, backend: BackendId) {
        self.0 |= 1 << backend.as_index();
    }

    /// Mark a backend as NOT having the article
    #[inline]
    pub fn mark_unavailable(&mut self, backend: BackendId) {
        self.0 &= !(1 << backend.as_index());
    }

    /// Check if a specific backend has the article
    #[must_use]
    #[inline]
    pub fn has_article(&self, backend: BackendId) -> bool {
        (self.0 & (1 << backend.as_index())) != 0
    }

    /// Get all backends that have the article
    #[must_use]
    pub fn available_backends(&self, max_backends: usize) -> Vec<BackendId> {
        (0..max_backends)
            .filter(|&i| (self.0 & (1 << i)) != 0)
            .map(BackendId::from_index)
            .collect()
    }

    /// Check if ANY backend has the article
    #[must_use]
    #[inline]
    pub const fn has_any(&self) -> bool {
        self.0 != 0
    }

    /// Get raw bitmap value
    #[must_use]
    #[inline]
    pub const fn as_u64(&self) -> u64 {
        self.0
    }
}

impl Default for BackendAvailability {
    fn default() -> Self {
        Self::new()
    }
}

/// Cache mapping message IDs to backend availability
///
/// Thread-safe: Uses Arc<Cache> internally, safe to clone and share across threads
#[derive(Debug, Clone)]
pub struct ArticleLocationCache {
    cache: Arc<Cache<Arc<str>, BackendAvailability>>,
    num_backends: usize,
}

impl ArticleLocationCache {
    /// Create new location cache with specified capacity
    ///
    /// Preallocates the cache to the specified size for optimal performance.
    /// Default: 640,000 entries (~64 MB)
    #[must_use]
    pub fn new(capacity: u64, num_backends: usize) -> Self {
        let cache = Cache::builder()
            .max_capacity(capacity)
            .initial_capacity(capacity as usize)
            .build();

        Self {
            cache: Arc::new(cache),
            num_backends,
        }
    }

    /// Look up article location in cache
    ///
    /// Returns None if not in cache, otherwise returns backend availability bitmap
    #[must_use]
    pub fn get(&self, message_id: &MessageId<'_>) -> Option<BackendAvailability> {
        let key: Arc<str> = Arc::from(message_id.as_str());
        self.cache.get(&key)
    }

    /// Update cache with article availability for a backend
    ///
    /// If the article already exists in cache, updates the bitmap.
    /// Otherwise, creates a new entry.
    pub fn update(&self, message_id: &MessageId<'_>, backend: BackendId, available: bool) {
        let key: Arc<str> = Arc::from(message_id.as_str());

        let mut availability = self.cache.get(&key).unwrap_or_default();

        if available {
            availability.mark_available(backend);
        } else {
            availability.mark_unavailable(backend);
        }

        self.cache.insert(key, availability);
    }

    /// Insert complete availability map for an article
    ///
    /// Used after parallel STAT to all backends
    pub fn insert(&self, message_id: &MessageId<'_>, availability: BackendAvailability) {
        let key: Arc<str> = Arc::from(message_id.as_str());
        self.cache.insert(key, availability);
    }

    /// Get number of backends tracked by this cache
    #[must_use]
    #[inline]
    pub const fn num_backends(&self) -> usize {
        self.num_backends
    }

    /// Get current cache entry count
    #[must_use]
    pub fn entry_count(&self) -> u64 {
        self.cache.entry_count()
    }

    /// Get configured cache capacity
    #[must_use]
    pub fn capacity(&self) -> u64 {
        self.cache.policy().max_capacity().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backend_availability_empty() {
        let avail = BackendAvailability::new();
        assert_eq!(avail.as_u64(), 0);
        assert!(!avail.has_any());
        assert!(!avail.has_article(BackendId::from_index(0)));
    }

    #[test]
    fn test_backend_availability_mark_available() {
        let mut avail = BackendAvailability::new();
        avail.mark_available(BackendId::from_index(0));
        avail.mark_available(BackendId::from_index(3));

        assert!(avail.has_any());
        assert!(avail.has_article(BackendId::from_index(0)));
        assert!(!avail.has_article(BackendId::from_index(1)));
        assert!(!avail.has_article(BackendId::from_index(2)));
        assert!(avail.has_article(BackendId::from_index(3)));

        assert_eq!(avail.as_u64(), 0b1001); // bits 0 and 3 set
    }

    #[test]
    fn test_backend_availability_mark_unavailable() {
        let mut avail = BackendAvailability::new();
        avail.mark_available(BackendId::from_index(0));
        avail.mark_available(BackendId::from_index(1));
        avail.mark_unavailable(BackendId::from_index(0));

        assert!(avail.has_any());
        assert!(!avail.has_article(BackendId::from_index(0)));
        assert!(avail.has_article(BackendId::from_index(1)));
    }

    #[test]
    fn test_backend_availability_available_backends() {
        let mut avail = BackendAvailability::new();
        avail.mark_available(BackendId::from_index(1));
        avail.mark_available(BackendId::from_index(3));
        avail.mark_available(BackendId::from_index(5));

        let backends = avail.available_backends(10);
        assert_eq!(backends.len(), 3);
        assert!(backends.contains(&BackendId::from_index(1)));
        assert!(backends.contains(&BackendId::from_index(3)));
        assert!(backends.contains(&BackendId::from_index(5)));
    }

    #[test]
    fn test_cache_get_miss() {
        let cache = ArticleLocationCache::new(100, 3);
        let msg_id = MessageId::new("<test@example.com>".to_string()).unwrap();

        assert!(cache.get(&msg_id).is_none());
    }

    #[test]
    fn test_cache_update() {
        let cache = ArticleLocationCache::new(100, 3);
        let msg_id = MessageId::new("<test@example.com>".to_string()).unwrap();

        // Update with backend 0 has it
        cache.update(&msg_id, BackendId::from_index(0), true);

        let avail = cache.get(&msg_id).unwrap();
        assert!(avail.has_article(BackendId::from_index(0)));
        assert!(!avail.has_article(BackendId::from_index(1)));

        // Update with backend 1 also has it
        cache.update(&msg_id, BackendId::from_index(1), true);

        let avail = cache.get(&msg_id).unwrap();
        assert!(avail.has_article(BackendId::from_index(0)));
        assert!(avail.has_article(BackendId::from_index(1)));
    }

    #[test]
    fn test_cache_insert() {
        let cache = ArticleLocationCache::new(100, 3);
        let msg_id = MessageId::new("<test@example.com>".to_string()).unwrap();

        let mut avail = BackendAvailability::new();
        avail.mark_available(BackendId::from_index(0));
        avail.mark_available(BackendId::from_index(2));

        cache.insert(&msg_id, avail);

        let cached = cache.get(&msg_id).unwrap();
        assert!(cached.has_article(BackendId::from_index(0)));
        assert!(!cached.has_article(BackendId::from_index(1)));
        assert!(cached.has_article(BackendId::from_index(2)));
    }

    #[test]
    fn test_cache_capacity() {
        let cache = ArticleLocationCache::new(1000, 3);
        assert_eq!(cache.capacity(), 1000);
        assert_eq!(cache.entry_count(), 0);
    }

    #[test]
    fn test_cache_lru_eviction() {
        let cache = ArticleLocationCache::new(2, 3); // Only 2 entries

        let msg1 = MessageId::new("<msg1@example.com>".to_string()).unwrap();
        let msg2 = MessageId::new("<msg2@example.com>".to_string()).unwrap();
        let msg3 = MessageId::new("<msg3@example.com>".to_string()).unwrap();

        cache.update(&msg1, BackendId::from_index(0), true);
        cache.update(&msg2, BackendId::from_index(1), true);
        cache.update(&msg3, BackendId::from_index(2), true);

        // msg1 should be evicted (LRU)
        // Note: moka eviction may be async, so we can't strictly test this
        // but we can verify capacity is respected
        assert!(cache.entry_count() <= 2);
    }
}
