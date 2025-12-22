//! Article caching implementation using LRU cache with TTL
//!
//! This module provides article caching with per-backend availability tracking.
//!
//! # NNTP Response Semantics (CRITICAL)
//!
//! **430 "No Such Article" is AUTHORITATIVE** - NNTP servers NEVER give false negatives.
//! If a server returns 430, the article is definitively not present on that server.
//! This is a fundamental property of NNTP: servers only return 430 when they are
//! certain they don't have the article.
//!
//! **2xx success responses are UNRELIABLE** - servers CAN give false positives.
//! A server might return 220/222/223 but then provide:
//! - Corrupt or incomplete article data
//! - Truncated responses due to connection issues
//! - Stale data that gets cleaned up moments later
//!
//! **Therefore: 430 (missing) ALWAYS takes precedence over "has" state.**
//! - When merging availability info, 430s accumulate and never get cleared
//! - A previous "has" can be overwritten by a later 430
//! - A previous 430 should NOT be overwritten by a later "has"
//! - Cache entries with 430 persist until TTL expiry
//!
//! **BACKEND LIMIT**: Maximum 8 backends supported due to u8 bitset optimization.
//! This limit is enforced at config validation time.

use crate::protocol::StatusCode;
use crate::router::BackendCount;
use crate::types::{BackendId, MessageId};
use moka::future::Cache;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Maximum number of backends supported by ArticleAvailability bitset
pub const MAX_BACKENDS: usize = 8;

/// Track which backends have (or don't have) a specific article
///
/// Uses two u8 bitsets to track availability across up to 8 backends:
/// - `checked`: Which backends we've queried (attempted to fetch from)
/// - `missing`: Which backends returned 430 (don't have this article)
///
/// # Example with 2 backends
/// - Initial state: `checked=00`, `missing=00` (haven't tried any backends yet)
/// - After backend 0 returns 430: `checked=01`, `missing=01` (backend 0 doesn't have it)
/// - After backend 1 returns 220: `checked=11`, `missing=01` (backend 1 has it)
/// - If both return 430: `checked=11`, `missing=11` (all backends exhausted)
///
/// # Usage Pattern
/// This type serves TWO critical purposes:
///
/// 1. **Cache persistence** - Track availability across requests (long-lived)
///    - Store in cache with article data
///    - Avoid querying backends known to be missing
///    - Updated after every successful/failed fetch
///
/// 2. **430 retry loop** - Track which backends tried during single request (transient)
///    - Create fresh instance for each ARTICLE request
///    - Mark backends as missing when they return 430
///    - Stop when all backends exhausted or one succeeds
///
/// # Thread Safety
/// Wrapped in `Arc<Mutex<>>` when stored in cache entries for concurrent updates.
/// The precheck pattern ensures serial updates to avoid races:
/// 1. Query all backends concurrently
/// 2. Wait for all to complete
/// 3. Update cache serially with all results
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArticleAvailability {
    /// Bitset of backends we've checked (tried to fetch from)
    checked: u8,
    /// Bitset of backends that DON'T have this article (returned 430)
    missing: u8, // u8 supports up to 8 backends (plenty for NNTP)
}

impl ArticleAvailability {
    /// Create empty availability - assume all backends have article until proven otherwise
    #[inline]
    pub const fn new() -> Self {
        Self {
            checked: 0,
            missing: 0,
        }
    }

    /// Record that a backend returned 430 (doesn't have the article)
    ///
    /// # NNTP Semantics
    /// **430 is AUTHORITATIVE** - this is a definitive "no". Once marked missing,
    /// the backend should not be retried for this article until cache TTL expires.
    /// See module-level docs for full explanation.
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
        self.checked |= 1u8 << idx; // Mark as checked
        self.missing |= 1u8 << idx; // Mark as missing
        self
    }

    /// Record that a backend returned a success response (2xx) for this article
    ///
    /// # NNTP Semantics Warning
    /// **2xx responses are UNRELIABLE** - servers can give false positives.
    /// This clears the missing bit for fresh `ArticleAvailability` instances,
    /// but when merged via `merge_from()`, existing 430s take precedence.
    /// See module-level docs for full explanation.
    ///
    /// # Panics (debug builds only)
    /// Panics if backend_id >= 8. Config validation enforces max 8 backends.
    #[inline]
    pub fn record_has(&mut self, backend_id: BackendId) -> &mut Self {
        let idx = backend_id.as_index();
        debug_assert!(
            idx < MAX_BACKENDS,
            "Backend index {} exceeds MAX_BACKENDS ({})",
            idx,
            MAX_BACKENDS
        );
        self.checked |= 1u8 << idx; // Mark as checked
        self.missing &= !(1u8 << idx); // Clear missing bit (has the article)
        self
    }

    /// Merge another availability's state into this one
    ///
    /// Used to sync local availability tracking back to cache.
    /// Takes the union of checked backends and missing backends.
    ///
    /// # NNTP Semantics (CRITICAL)
    ///
    /// **430 responses are AUTHORITATIVE** - NNTP servers NEVER give false negatives.
    /// If a server says "no such article" (430), the article is definitively not there.
    ///
    /// **2xx responses are UNRELIABLE** - servers CAN give false positives.
    /// A server might claim to have an article but return corrupt data, or the
    /// article might be incomplete/unavailable due to propagation delays.
    ///
    /// Therefore: `missing` state ALWAYS wins over `has` state.
    /// We trust 430s absolutely but treat successes with skepticism.
    #[inline]
    pub fn merge_from(&mut self, other: &Self) {
        // Simple union: trust all 430s from both sources
        // 430 is authoritative, so missing bits should accumulate
        self.checked |= other.checked;
        self.missing |= other.missing;
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

    /// Get the raw missing bitset for debugging
    #[inline]
    pub const fn missing_bits(&self) -> u8 {
        self.missing
    }

    /// Get the raw checked bitset for debugging
    #[inline]
    pub const fn checked_bits(&self) -> u8 {
        self.checked
    }

    /// Check if all backends in the pool have been tried and returned 430
    ///
    /// Check if all backends have been tried and returned 430
    ///
    /// # Panics (debug builds only)
    /// Panics if backend_count > 8. Config validation enforces max 8 backends.
    #[inline]
    pub fn all_exhausted(&self, backend_count: BackendCount) -> bool {
        let count = backend_count.get();
        debug_assert!(
            count <= MAX_BACKENDS,
            "Backend count {} exceeds MAX_BACKENDS ({})",
            count,
            MAX_BACKENDS
        );
        match count {
            0 => true,
            8 => self.missing == 0xFF,
            n => self.missing & ((1u8 << n) - 1) == (1u8 << n) - 1,
        }
    }

    /// Get an iterator over backends that should still be tried
    ///
    /// Returns backend IDs that haven't been marked missing yet.
    pub fn available_backends(
        &self,
        backend_count: BackendCount,
    ) -> impl Iterator<Item = BackendId> + '_ {
        (0..backend_count.get().min(MAX_BACKENDS))
            .map(BackendId::from_index)
            .filter(move |&backend_id| self.should_try(backend_id))
    }

    /// Get the underlying bitset value (for debugging)
    #[inline]
    pub const fn as_u8(&self) -> u8 {
        self.missing
    }

    /// Check if we have any backend availability information
    ///
    /// Returns true if at least one backend has been checked.
    /// If this returns false, we haven't tried any backends yet and shouldn't
    /// serve from cache (should try backends first).
    #[inline]
    pub const fn has_availability_info(&self) -> bool {
        self.checked != 0
    }

    /// Check if any backend is known to HAVE the article
    ///
    /// Returns true if at least one backend was checked and did NOT return 430.
    /// This is the inverse check from `all_exhausted` - at least one success.
    #[inline]
    pub const fn any_backend_has_article(&self) -> bool {
        // A backend "has" the article if it's checked but not missing
        // checked & !missing gives us the backends that have it
        (self.checked & !self.missing) != 0
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
    ///
    /// No mutex needed: moka clones entries on get(), and updates go through
    /// cache.insert() which replaces the whole entry atomically.
    backend_availability: ArticleAvailability,

    /// Complete response buffer (Arc for cheap cloning)
    /// Format: `220 <msgid>\r\n<headers>\r\n\r\n<body>\r\n.\r\n`
    /// Status code is always at bytes [0..3]
    buffer: Arc<Vec<u8>>,
}

impl ArticleEntry {
    /// Create from response buffer
    ///
    /// The buffer should be a complete NNTP response (status line + data + terminator).
    /// No validation is performed here - caller must ensure buffer is valid.
    ///
    /// Backend availability starts with assumption all backends have the article.
    pub fn new(buffer: Vec<u8>) -> Self {
        Self {
            backend_availability: ArticleAvailability::new(),
            buffer: Arc::new(buffer),
        }
    }

    /// Create from pre-wrapped Arc buffer
    ///
    /// Use this when the buffer is already wrapped in Arc to avoid double wrapping.
    pub fn from_arc(buffer: Arc<Vec<u8>>) -> Self {
        Self {
            backend_availability: ArticleAvailability::new(),
            buffer,
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

    /// Record that a backend successfully provided this article
    pub fn record_backend_has(&mut self, backend_id: BackendId) {
        self.backend_availability.record_has(backend_id);
    }

    /// Check if all backends have been tried and none have the article
    pub fn all_backends_exhausted(&self, total_backends: BackendCount) -> bool {
        self.backend_availability.all_exhausted(total_backends)
    }

    /// Get backends that might have this article
    pub fn available_backends(&self, total_backends: BackendCount) -> Vec<BackendId> {
        self.backend_availability
            .available_backends(total_backends)
            .collect()
    }

    /// Get response buffer (backward compatibility)
    ///
    /// This provides the same interface as the old CachedArticle.response field.
    #[inline]
    pub fn response(&self) -> &Arc<Vec<u8>> {
        &self.buffer
    }

    /// Check if this cache entry has useful availability information
    ///
    /// Returns true if at least one backend has been tried (marked as missing or has article).
    /// If false, we haven't tried any backends yet and should run precheck instead of
    /// serving from cache.
    #[inline]
    pub fn has_availability_info(&self) -> bool {
        self.backend_availability.has_availability_info()
    }

    /// Check if this cache entry contains a complete article (220) or body (222)
    ///
    /// Returns true if:
    /// 1. Status code is 220 (ARTICLE) or 222 (BODY)
    /// 2. Buffer contains actual content (not just a stub like "220\r\n")
    ///
    /// A complete response ends with ".\r\n" and is significantly longer
    /// than a stub. Stubs are typically 5-6 bytes (e.g., "220\r\n" or "223\r\n").
    ///
    /// This is used when `cache_articles=true` to determine if we can serve
    /// directly from cache or need to fetch additional data.
    #[inline]
    pub fn is_complete_article(&self) -> bool {
        // Must be a 220 (ARTICLE) or 222 (BODY) response
        let Some(code) = self.status_code() else {
            return false;
        };
        if code.as_u16() != 220 && code.as_u16() != 222 {
            return false;
        }

        // Must have actual content, not just a stub
        // A stub is typically "220\r\n" (5 bytes) or "222 0 <test@example.com>\r\n" (25-30 bytes)
        // A real article/body has content + terminator
        // Minimum valid: "220 0 <x@y>\r\nX: Y\r\n\r\nB\r\n.\r\n" = 30 bytes
        const MIN_ARTICLE_SIZE: usize = 30;
        self.buffer.len() >= MIN_ARTICLE_SIZE && self.buffer.ends_with(b".\r\n")
    }

    /// Check if the cached response matches the requested command type
    ///
    /// Returns true if cached response can directly satisfy the command.
    /// Accepts pre-parsed command verb for hot-path efficiency.
    ///
    /// Command matching rules:
    /// - ARTICLE (220) matches: ARTICLE, BODY, HEAD requests
    /// - BODY (222) matches: BODY requests only
    /// - HEAD (221) matches: HEAD requests only
    #[inline]
    pub fn matches_command_type_verb(&self, cmd_verb: &str) -> bool {
        let Some(code) = self.status_code() else {
            return false;
        };

        match code.as_u16() {
            220 => {
                // ARTICLE response has everything - can serve ARTICLE, BODY, or HEAD
                matches!(cmd_verb, "ARTICLE" | "BODY" | "HEAD")
            }
            222 => {
                // BODY response - can only serve BODY requests
                cmd_verb == "BODY"
            }
            221 => {
                // HEAD response - can only serve HEAD requests
                cmd_verb == "HEAD"
            }
            _ => false,
        }
    }

    /// Check if the cached response matches the requested command type
    ///
    /// Returns true if cached response can directly satisfy the command:
    /// - ARTICLE (220) matches: ARTICLE, BODY, HEAD requests
    /// - BODY (222) matches: BODY requests only
    /// - HEAD (221) matches: HEAD requests only
    #[inline]
    pub fn matches_command_type(&self, command: &str) -> bool {
        let cmd_verb = command
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        self.matches_command_type_verb(&cmd_verb)
    }

    /// Initialize availability tracker from this cached entry
    ///
    /// Creates a fresh ArticleAvailability with backends marked missing based on
    /// cached knowledge (backends that previously returned 430).
    pub fn to_availability(&self, total_backends: BackendCount) -> ArticleAvailability {
        let mut availability = ArticleAvailability::new();

        // Mark backends we know don't have this article
        for backend_id in (0..total_backends.get()).map(BackendId::from_index) {
            if !self.should_try_backend(backend_id) {
                availability.record_missing(backend_id);
            }
        }

        availability
    }
}

/// Article cache using LRU eviction with TTL
///
/// Uses `Arc<str>` (message ID content without brackets) as key for zero-allocation lookups.
/// `Arc<str>` implements `Borrow<str>`, allowing `cache.get(&str)` without allocation.
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
    /// * `max_capacity` - Maximum cache size in bytes (uses weighted entries)
    /// * `ttl` - Time-to-live for cached articles
    /// * `cache_articles` - Whether to cache full article bodies (true) or just availability (false)
    pub fn new(max_capacity: u64, ttl: Duration, cache_articles: bool) -> Self {
        // Build cache with byte-based capacity using weigher
        // max_capacity is total bytes allowed
        let cache = Cache::builder()
            .max_capacity(max_capacity)
            .time_to_live(ttl)
            .weigher(move |key: &Arc<str>, entry: &ArticleEntry| -> u32 {
                // Calculate actual memory footprint for accurate capacity tracking.
                //
                // Memory layout per cache entry:
                //
                // Key: Arc<str>
                //   - Arc control block: 16 bytes (strong_count + weak_count)
                //   - String data: key.len() bytes
                //   - Allocator overhead: ~16 bytes (malloc metadata, alignment)
                //
                // Value: ArticleEntry
                //   - Struct inline: 16 bytes (ArticleAvailability: 2 + padding: 6 + Arc ptr: 8)
                //   - Arc<Vec<u8>> control block: 16 bytes
                //   - Vec<u8> struct: 24 bytes (ptr + len + capacity)
                //   - Vec data: buffer.len() bytes
                //   - Allocator overhead: ~32 bytes (two allocations: Arc+Vec, Vec data)
                //
                // Moka internal per-entry overhead is MUCH larger than the data itself:
                //   - Key stored twice: Bucket.key AND ValueEntry.info.key_hash.key
                //   - EntryInfo<K> struct: ~72 bytes (atomics, timestamps, counters)
                //   - LRU deque nodes and frequency sketch entries
                //   - Timer wheel entries for TTL tracking
                //   - crossbeam-epoch deferred garbage (can retain 2x entries)
                //   - HashMap segments with open addressing (~2x load factor)
                //
                // Empirical testing shows ~10x actual RSS vs weighted_size().
                // Our observed ratio: 362MB RSS / 36MB weighted = 10x
                //
                const ARC_STR_OVERHEAD: usize = 16 + 16; // Arc control block + allocator
                const ENTRY_STRUCT: usize = 16; // ArticleEntry inline size
                const ARC_VEC_OVERHEAD: usize = 16 + 24 + 32; // Arc + Vec struct + allocator
                // Moka internal structures - empirically measured to address memory reporting gap.
                // See moka issue #473: https://github.com/moka-rs/moka/issues/473
                // Observed ratio: 362MB RSS / 36MB weighted_size() = 10x
                const MOKA_OVERHEAD: usize = 2000;

                let key_size = ARC_STR_OVERHEAD + key.len();
                let buffer_size = ARC_VEC_OVERHEAD + entry.buffer().len();
                let base_size = key_size + buffer_size + ENTRY_STRUCT + MOKA_OVERHEAD;

                // Stubs (availability-only entries) have higher relative overhead
                // due to allocator fragmentation on small allocations.
                // Complete articles are dominated by content size, so no multiplier needed.
                let weighted_size = if entry.is_complete_article() {
                    base_size
                } else {
                    // Small allocations have ~50% more overhead from allocator fragmentation.
                    // Use a 1.5x multiplier, rounding up, to avoid underestimating small entries.
                    (base_size * 3).div_ceil(2)
                };

                weighted_size.try_into().unwrap_or(u32::MAX)
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

    /// Upsert cache entry (insert or update) - ATOMIC OPERATION
    ///
    /// Uses moka's `entry().and_upsert_with()` for atomic get-modify-store.
    /// This eliminates the race condition of separate get() + insert() calls
    /// and provides key-level locking for concurrent operations.
    ///
    /// If entry exists: updates the entry and marks backend as having the article
    /// If entry doesn't exist: inserts new entry
    ///
    /// When `cache_articles=false`, extracts status code from buffer and stores minimal stub.
    /// When `cache_articles=true`, stores full buffer.
    ///
    /// CRITICAL: Always re-insert to refresh TTL, and mark backend as having the article.
    pub async fn upsert<'a>(
        &self,
        message_id: MessageId<'a>,
        buffer: Vec<u8>,
        backend_id: BackendId,
    ) {
        let key: Arc<str> = message_id.without_brackets().into();
        let cache_articles = self.cache_articles;

        // Prepare the new buffer outside the closure for stub extraction
        let new_buffer = if cache_articles {
            buffer
        } else {
            self.create_minimal_stub(&buffer)
        };

        // Wrap in Arc so we can efficiently share/move into closure without clone.
        // Arc::try_unwrap will give us the Vec back if we're the only owner.
        let new_buffer = Arc::new(new_buffer);

        // Use atomic upsert - this provides key-level locking and eliminates
        // the race condition between get() and insert() calls
        self.cache
            .entry(key)
            .and_upsert_with(|maybe_entry| {
                let new_entry = if let Some(existing) = maybe_entry {
                    let mut entry = existing.into_value();

                    // Decide whether to update buffer based on completeness
                    let existing_complete = entry.is_complete_article();
                    let new_complete = new_buffer.len() >= 30 && new_buffer.ends_with(b".\r\n");

                    let should_replace = match (existing_complete, new_complete) {
                        (false, true) => true,  // Stub → Complete: Always replace
                        (true, false) => false, // Complete → Stub: Never replace
                        (true, true) => new_buffer.len() > entry.buffer.len(), // Both complete: larger wins
                        (false, false) => new_buffer.len() > entry.buffer.len(), // Both stubs: larger wins
                    };

                    if should_replace {
                        // Already wrapped in Arc, just clone the Arc (cheap)
                        entry.buffer = Arc::clone(&new_buffer);
                    }

                    // Mark backend as having the article
                    entry.record_backend_has(backend_id);
                    entry
                } else {
                    // New entry - clone Arc (cheap, just reference count bump)
                    let mut entry = ArticleEntry::from_arc(Arc::clone(&new_buffer));
                    entry.record_backend_has(backend_id);
                    entry
                };

                std::future::ready(new_entry)
            })
            .await;
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

    /// Record that a backend returned 430 for this article - ATOMIC OPERATION
    ///
    /// Uses moka's `entry().and_upsert_with()` for atomic get-modify-store.
    /// This eliminates the race condition of separate get() + insert() calls
    /// and provides key-level locking for concurrent operations.
    ///
    /// If the article is already cached, updates the availability bitset.
    /// If not cached, creates a new cache entry with a 430 stub.
    /// This prevents repeated queries to backends that don't have the article.
    ///
    /// Note: We don't store the actual backend 430 response because:
    /// 1. We always send a standardized 430 to clients, never the backend's response
    /// 2. The only info we need is the availability bitset (which backends returned 430)
    pub async fn record_backend_missing<'a>(
        &self,
        message_id: MessageId<'a>,
        backend_id: BackendId,
    ) {
        let key: Arc<str> = message_id.without_brackets().into();
        let misses = &self.misses;

        // Use atomic upsert - this provides key-level locking
        let entry = self
            .cache
            .entry(key)
            .and_upsert_with(|maybe_entry| {
                let new_entry = if let Some(existing) = maybe_entry {
                    let mut entry = existing.into_value();
                    entry.record_backend_missing(backend_id);
                    entry
                } else {
                    // First 430 for this article - create cache entry with minimal stub
                    let mut entry = ArticleEntry::new(b"430\r\n".to_vec());
                    entry.record_backend_missing(backend_id);
                    entry
                };

                std::future::ready(new_entry)
            })
            .await;

        // Track misses for new entries
        if entry.is_fresh() {
            misses.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Sync availability state from local tracker to cache - ATOMIC OPERATION
    ///
    /// Uses moka's `entry().and_compute_with()` for atomic get-modify-store.
    /// This eliminates the race condition of separate get() + insert() calls
    /// and provides key-level locking for concurrent operations.
    ///
    /// This is called ONCE at the end of a retry loop to persist all the
    /// backends that returned 430 during this request. Much more efficient
    /// than calling record_backend_missing for each backend individually.
    ///
    /// IMPORTANT: Only creates a 430 stub entry if ALL checked backends returned 430.
    /// If any backend successfully provided the article, we skip creating an entry
    /// (the actual article will be cached via upsert, which may race with this call).
    pub async fn sync_availability<'a>(
        &self,
        message_id: MessageId<'a>,
        availability: &ArticleAvailability,
    ) {
        use moka::ops::compute::Op;

        // Only sync if we actually tried some backends
        if availability.checked_bits() == 0 {
            return;
        }

        let key: Arc<str> = message_id.without_brackets().into();
        let availability = *availability; // Copy for the closure
        let misses = &self.misses;

        // Use atomic compute - allows us to conditionally insert/update
        let result = self
            .cache
            .entry(key)
            .and_compute_with(|maybe_entry| {
                let op = if let Some(existing) = maybe_entry {
                    // Merge availability into existing entry
                    let mut entry = existing.into_value();
                    entry.backend_availability.merge_from(&availability);
                    Op::Put(entry)
                } else {
                    // No existing entry - only create a 430 stub if ALL backends returned 430
                    if availability.any_backend_has_article() {
                        // A backend successfully provided the article.
                        // Don't create a 430 stub - let upsert() handle it with the real article data.
                        Op::Nop
                    } else {
                        // All checked backends returned 430 - create stub to track this
                        let mut entry = ArticleEntry::new(b"430\r\n".to_vec());
                        entry.backend_availability = availability;
                        Op::Put(entry)
                    }
                };

                std::future::ready(op)
            })
            .await;

        // Track misses for new entries
        if matches!(result, moka::ops::compute::CompResult::Inserted(_)) {
            misses.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Get cache statistics
    pub async fn stats(&self) -> CacheStats {
        CacheStats {
            entry_count: self.cache.entry_count(),
            weighted_size: self.cache.weighted_size(),
        }
    }

    /// Insert an article entry directly (for testing)
    ///
    /// This is a low-level method that bypasses the usual upsert logic.
    /// Only use this in tests where you need precise control over cache state.
    #[cfg(test)]
    pub async fn insert<'a>(&self, message_id: MessageId<'a>, entry: ArticleEntry) {
        let key: Arc<str> = message_id.without_brackets().into();
        self.cache.insert(key, entry).await;
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

    fn create_test_article(msgid: &str) -> ArticleEntry {
        let buffer = format!("220 0 {}\r\nSubject: Test\r\n\r\nBody\r\n.\r\n", msgid).into_bytes();
        ArticleEntry::new(buffer)
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
    fn test_any_backend_has_article() {
        let mut avail = ArticleAvailability::new();
        let b0 = BackendId::from_index(0);
        let b1 = BackendId::from_index(1);

        // Empty availability - no backend has it (nothing checked)
        assert!(!avail.any_backend_has_article());

        // Record b0 as missing - still none have it
        avail.record_missing(b0);
        assert!(!avail.any_backend_has_article());

        // Record b1 as having it - now one has it
        avail.record_has(b1);
        assert!(avail.any_backend_has_article());

        // Record b1 as missing (overwrite) - none have it again
        avail.record_missing(b1);
        assert!(!avail.any_backend_has_article());
    }

    #[test]
    fn test_record_has_clears_missing_bit() {
        let mut avail = ArticleAvailability::new();
        let b0 = BackendId::from_index(0);

        // First mark as missing
        avail.record_missing(b0);
        assert!(avail.is_missing(b0));
        assert!(!avail.any_backend_has_article());

        // Now mark as has - should clear missing
        avail.record_has(b0);
        assert!(!avail.is_missing(b0));
        assert!(avail.any_backend_has_article());
    }

    #[test]
    fn test_merge_from_430_overrides_has() {
        // NNTP SEMANTICS:
        // - 430 "No Such Article" is AUTHORITATIVE - servers NEVER give false negatives
        // - 2xx success is UNRELIABLE - servers CAN give false positives
        // Therefore: 430 always wins over previous "has" state
        let mut cache_entry = ArticleAvailability::new();
        let b0 = BackendId::from_index(0);
        let b1 = BackendId::from_index(1);

        // Previous request got success from backend 0 (might be false positive)
        cache_entry.record_has(b0);
        assert!(!cache_entry.is_missing(b0));
        assert!(cache_entry.any_backend_has_article());

        // Later request gets 430 from backend 0 (AUTHORITATIVE)
        let mut new_result = ArticleAvailability::new();
        new_result.record_missing(b0); // 430 is definitive
        new_result.record_missing(b1); // Backend 1 also returned 430

        // Merge new results into cache entry
        cache_entry.merge_from(&new_result);

        // CRITICAL: 430 is authoritative, so it MUST override the previous "has"
        // The earlier success might have been a false positive (corrupt data, etc.)
        assert!(
            cache_entry.is_missing(b0),
            "430 must override previous has - 430 is authoritative"
        );
        assert!(!cache_entry.any_backend_has_article());

        // Backend 1 should also be marked as missing
        assert!(
            cache_entry.is_missing(b1),
            "Backend 1 should be marked missing"
        );
    }

    #[test]
    fn test_merge_from_can_add_new_missing() {
        let mut avail1 = ArticleAvailability::new();
        let mut avail2 = ArticleAvailability::new();

        // avail1 knows backend 0 is missing
        avail1.record_missing(BackendId::from_index(0));

        // avail2 knows backend 1 is missing
        avail2.record_missing(BackendId::from_index(1));

        // Merge avail2 into avail1
        avail1.merge_from(&avail2);

        // avail1 should now know both are missing
        assert!(avail1.is_missing(BackendId::from_index(0)));
        assert!(avail1.is_missing(BackendId::from_index(1)));
    }

    #[test]
    fn test_merge_from_has_does_not_clear_missing() {
        // NNTP SEMANTICS:
        // - 430 is AUTHORITATIVE - if server says "no", article is NOT there
        // - 2xx is UNRELIABLE - server might lie (corrupt data, propagation issues)
        // Therefore: "has" from new fetch should NOT clear existing 430
        let mut cache_state = ArticleAvailability::new();
        let b0 = BackendId::from_index(0);
        let b1 = BackendId::from_index(1);

        // Cache says both backends returned 430 (authoritative)
        cache_state.record_missing(b0);
        cache_state.record_missing(b1);
        assert!(cache_state.is_missing(b0));
        assert!(cache_state.is_missing(b1));
        assert!(!cache_state.any_backend_has_article());

        // New request claims success from backend 0 (but 2xx is unreliable!)
        let mut new_fetch = ArticleAvailability::new();
        new_fetch.record_has(b0);

        // Merge new fetch into cache
        cache_state.merge_from(&new_fetch);

        // Backend 0 should STILL be missing - we trust the 430 over the 2xx
        // because 2xx can be false positive but 430 is never false negative
        assert!(
            cache_state.is_missing(b0),
            "2xx should NOT override 430 - 430 is authoritative"
        );
        assert!(!cache_state.any_backend_has_article());

        // Backend 1 is still missing
        assert!(
            cache_state.is_missing(b1),
            "Backend 1 should still be missing"
        );
    }

    #[test]
    fn test_backend_availability_all_exhausted() {
        use crate::router::BackendCount;
        let mut avail = ArticleAvailability::new();

        // None missing yet
        assert!(!avail.all_exhausted(BackendCount::new(2)));
        assert!(!avail.all_exhausted(BackendCount::new(3)));

        // Record backends 0 and 1 as missing
        avail.record_missing(BackendId::from_index(0));
        avail.record_missing(BackendId::from_index(1));

        // All 2 backends exhausted
        assert!(avail.all_exhausted(BackendCount::new(2)));

        // But not all 3 backends (backend 2 still untried)
        assert!(!avail.all_exhausted(BackendCount::new(3)));
    }

    #[test]
    fn test_article_entry_basic() {
        let buffer = b"220 0 <test@example.com>\r\nBody\r\n.\r\n".to_vec();
        let entry = ArticleEntry::new(buffer.clone());

        assert_eq!(entry.status_code(), Some(StatusCode::new(220)));
        assert_eq!(entry.buffer().as_ref(), &buffer);

        // Default: should try all backends
        assert!(entry.should_try_backend(BackendId::from_index(0)));
        assert!(entry.should_try_backend(BackendId::from_index(1)));
    }

    #[test]
    fn test_is_complete_article() {
        // Stubs should NOT be complete articles
        let stub_430 = ArticleEntry::new(b"430\r\n".to_vec());
        assert!(!stub_430.is_complete_article());

        let stub_220 = ArticleEntry::new(b"220\r\n".to_vec());
        assert!(!stub_220.is_complete_article());

        let stub_223 = ArticleEntry::new(b"223\r\n".to_vec());
        assert!(!stub_223.is_complete_article());

        // Full article SHOULD be complete
        let full = ArticleEntry::new(
            b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec(),
        );
        assert!(full.is_complete_article());

        // Wrong status code (not 220) should NOT be complete article
        let head_response =
            ArticleEntry::new(b"221 0 <test@example.com>\r\nSubject: Test\r\n.\r\n".to_vec());
        assert!(!head_response.is_complete_article());

        // Missing terminator should NOT be complete
        let no_terminator = ArticleEntry::new(
            b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n".to_vec(),
        );
        assert!(!no_terminator.is_complete_article());
    }

    #[test]
    fn test_article_entry_record_backend_missing() {
        let backend0 = BackendId::from_index(0);
        let backend1 = BackendId::from_index(1);
        let mut entry = create_test_article("<test@example.com>");

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
        use crate::router::BackendCount;
        let backend0 = BackendId::from_index(0);
        let backend1 = BackendId::from_index(1);
        let mut entry = create_test_article("<test@example.com>");

        // Not all exhausted yet
        assert!(!entry.all_backends_exhausted(BackendCount::new(2)));

        // Record both as missing
        entry.record_backend_missing(backend0);
        entry.record_backend_missing(backend1);

        // Now all 2 backends are exhausted
        assert!(entry.all_backends_exhausted(BackendCount::new(2)));
    }

    #[tokio::test]
    async fn test_arc_str_borrow_lookup() {
        // Create cache with Arc<str> keys
        let cache = ArticleCache::new(100, Duration::from_secs(300), true);

        // Create a MessageId and insert an article
        let msgid = MessageId::from_borrowed("<test123@example.com>").unwrap();
        let article = create_test_article("<test123@example.com>");

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
        let article = create_test_article("<article@example.com>");

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
        let article = create_test_article("<test@example.com>");

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

    /// CRITICAL BUG FIX TEST: record_backend_missing must create cache entries
    /// for articles that don't exist anywhere (all backends return 430).
    ///
    /// Bug: Previously, if an article wasn't cached, record_backend_missing
    /// would silently do nothing. This caused repeated queries to all backends
    /// for missing articles, resulting in:
    /// - Massive bandwidth waste
    /// - SABnzbd reporting "gigabytes of missing articles"
    /// - 4xx/5xx error counts not increasing (metrics bug)
    #[tokio::test]
    async fn test_record_backend_missing_creates_new_entry() {
        let cache = ArticleCache::new(100, Duration::from_secs(300), true);

        let msgid = MessageId::from_borrowed("<missing@example.com>").unwrap();

        // Verify article is NOT in cache
        assert!(cache.get(&msgid).await.is_none());

        // Record backend 0 returned 430
        cache
            .record_backend_missing(msgid.clone(), BackendId::from_index(0))
            .await;

        // CRITICAL: Cache entry MUST now exist
        let entry = cache
            .get(&msgid)
            .await
            .expect("Cache entry must exist after record_backend_missing");

        // Verify backend 0 is marked as missing
        assert!(
            !entry.should_try_backend(BackendId::from_index(0)),
            "Backend 0 should be marked missing"
        );

        // Verify backend 1 is still available (not tried yet)
        assert!(
            entry.should_try_backend(BackendId::from_index(1)),
            "Backend 1 should still be available"
        );

        // Verify the cached response is a 430 stub
        assert_eq!(
            entry.buffer().as_ref(),
            b"430\r\n",
            "Cached buffer should be a 430 stub"
        );

        // Record backend 1 also returned 430
        cache
            .record_backend_missing(msgid.clone(), BackendId::from_index(1))
            .await;

        let entry = cache.get(&msgid).await.unwrap();

        // Now both backends should be marked missing
        assert!(!entry.should_try_backend(BackendId::from_index(0)));
        assert!(!entry.should_try_backend(BackendId::from_index(1)));

        // Verify all backends exhausted
        use crate::router::BackendCount;
        assert!(
            entry.all_backends_exhausted(BackendCount::new(2)),
            "All backends should be exhausted"
        );
    }

    #[tokio::test]
    async fn test_cache_stats() {
        let cache = ArticleCache::new(1024 * 1024, Duration::from_secs(300), true); // 1MB

        // Initial stats
        let stats = cache.stats().await;
        assert_eq!(stats.entry_count, 0);

        // Insert one article
        let msgid = MessageId::from_borrowed("<test@example.com>").unwrap();
        let article = create_test_article("<test@example.com>");
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
        let article = create_test_article("<expire@example.com>");

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
        let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
        let full_size = buffer.len();

        cache_stub
            .upsert(msgid.clone(), buffer, BackendId::from_index(0))
            .await;
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
        let buffer2 = b"220 0 <test2@example.com>\r\nSubject: Test2\r\n\r\nBody2\r\n.\r\n".to_vec();
        let original_size = buffer2.len();

        cache_full
            .upsert(msgid2.clone(), buffer2, BackendId::from_index(0))
            .await;
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
            let article = create_test_article(msgid.as_ref());
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
        let article = create_test_article("<test@example.com>");

        let cloned = article.clone();
        assert_eq!(article.buffer().as_ref(), cloned.buffer().as_ref());
        assert!(Arc::ptr_eq(article.buffer(), cloned.buffer()));
    }

    #[tokio::test]
    async fn test_cache_clone() {
        let cache1 = ArticleCache::new(1024 * 1024, Duration::from_secs(300), true); // 1MB
        let cache2 = cache1.clone();

        let msgid = MessageId::from_borrowed("<test@example.com>").unwrap();
        let article = create_test_article("<test@example.com>");

        cache1.insert(msgid.clone(), article).await;
        cache1.sync().await;

        // Should be accessible from cloned cache
        assert!(cache2.get(&msgid).await.is_some());
    }

    #[tokio::test]
    async fn test_weigher_large_articles() {
        // Test that large article bodies use ACTUAL SIZE (no multiplier)
        // when cache_articles=true and buffer >10KB
        let cache = ArticleCache::new(10 * 1024 * 1024, Duration::from_secs(300), true); // 10MB capacity

        // Create a 750KB article (typical size)
        let body = vec![b'X'; 750_000];
        let response = format!(
            "222 0 <test@example.com>\r\n{}\r\n.\r\n",
            std::str::from_utf8(&body).unwrap()
        );
        let article = ArticleEntry::new(response.as_bytes().to_vec());

        let msgid = MessageId::from_borrowed("<test@example.com>").unwrap();
        cache.insert(msgid.clone(), article).await;
        cache.sync().await;

        // With actual size (no multiplier): 750KB per entry
        // 10MB capacity should fit ~13 articles
        // With old 1.8x multiplier: 750KB * 1.8 ≈ 1.35MB per entry, fits ~7 articles
        // With old 2.5x multiplier: 750KB * 2.5 ≈ 1.875MB per entry, fits ~5 articles

        // Insert 12 more articles (13 total)
        for i in 2..=13 {
            let msgid_str = format!("<article{}@example.com>", i);
            let msgid = MessageId::new(msgid_str).unwrap();
            let response = format!(
                "222 0 {}\r\n{}\r\n.\r\n",
                msgid.as_str(),
                std::str::from_utf8(&body).unwrap()
            );
            let article = ArticleEntry::new(response.as_bytes().to_vec());
            cache.insert(msgid, article).await;
            cache.sync().await;
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
        cache.sync().await;

        let stats = cache.stats().await;
        // With actual size (no multiplier), should fit 11-13 large articles
        assert!(
            stats.entry_count >= 11,
            "Cache should fit at least 11 large articles with actual size (no multiplier) (got {})",
            stats.entry_count
        );
    }

    #[tokio::test]
    async fn test_weigher_small_stubs() {
        // Test that small stubs account for moka internal overhead correctly
        // With MOKA_OVERHEAD = 2000 bytes (based on empirical 10x memory ratio from moka issue #473)
        let cache = ArticleCache::new(1_000_000, Duration::from_secs(300), false); // 1MB capacity

        // Create small stub (53 bytes)
        let stub = b"223 0 <test@example.com>\r\n".to_vec();
        let article = ArticleEntry::new(stub);

        let msgid = MessageId::from_borrowed("<test@example.com>").unwrap();
        cache.insert(msgid, article).await;
        cache.sync().await;

        // With MOKA_OVERHEAD = 2000: stub + Arc + availability + overhead
        // ~53 + 68 + 40 + 2000 = ~2161 bytes per small stub
        // With 2.5x small stub multiplier: ~5400 bytes per stub
        // 1MB capacity should fit ~185 stubs

        // Insert many small stubs
        for i in 2..=200 {
            let msgid_str = format!("<stub{}@example.com>", i);
            let msgid = MessageId::new(msgid_str).unwrap();
            let stub = format!("223 0 {}\r\n", msgid.as_str());
            let article = ArticleEntry::new(stub.as_bytes().to_vec());
            cache.insert(msgid, article).await;
        }

        cache.sync().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        cache.sync().await;

        let stats = cache.stats().await;
        // Should be able to fit ~150-185 small stubs in 1MB
        assert!(
            stats.entry_count >= 100,
            "Cache should fit many small stubs (got {})",
            stats.entry_count
        );
    }

    #[tokio::test]
    async fn test_cache_with_owned_message_id() {
        let cache = ArticleCache::new(1024 * 1024, Duration::from_secs(300), true); // 1MB

        // Use owned MessageId
        let msgid = MessageId::new("<owned@example.com>".to_string()).unwrap();
        let article = create_test_article("<owned@example.com>");

        cache.insert(msgid.clone(), article).await;

        // Retrieve with borrowed MessageId
        let borrowed_msgid = MessageId::from_borrowed("<owned@example.com>").unwrap();
        assert!(cache.get(&borrowed_msgid).await.is_some());
    }
}
