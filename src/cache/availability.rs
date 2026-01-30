//! Article availability tracking across backends
//!
//! Uses bitsets to track which backends have (or don't have) a specific article.
//! This is a self-contained type used by both the cache layer (persistence) and
//! the retry loop (transient tracking).
//!
//! # NNTP Response Semantics (CRITICAL)
//!
//! **430 "No Such Article" is AUTHORITATIVE** - NNTP servers NEVER give false negatives.
//! If a server returns 430, the article is definitively not present on that server.
//!
//! **2xx success responses are UNRELIABLE** - servers CAN give false positives.
//! A server might return 220/222/223 but then provide corrupt or incomplete data.
//!
//! **Therefore: 430 (missing) ALWAYS takes precedence over "has" state.**
//!
//! **BACKEND LIMIT**: Maximum 8 backends supported due to u8 bitset optimization.
//! This limit is enforced at config validation time.

use crate::router::BackendCount;
use crate::types::BackendId;

/// Maximum number of backends supported by ArticleAvailability bitset
pub const MAX_BACKENDS: usize = 8;

/// Status of a backend for a specific article
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendStatus {
    /// Backend hasn't been checked yet
    Unknown,
    /// Backend was checked and returned 430 (doesn't have article)
    Missing,
    /// Backend was checked and has the article
    HasArticle,
}

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

    /// Reconstruct from raw bitset values (used for deserialization)
    ///
    /// # Safety
    /// The caller must ensure the bits represent valid backend states.
    /// This is primarily used when deserializing from disk cache.
    #[inline]
    pub const fn from_bits(checked: u8, missing: u8) -> Self {
        Self { checked, missing }
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

    /// Query backend availability status
    ///
    /// # Panics (debug builds only)
    /// Panics if backend_id >= 8. Config validation enforces max 8 backends.
    #[inline]
    pub fn status(&self, backend_id: BackendId) -> BackendStatus {
        let idx = backend_id.as_index();
        debug_assert!(
            idx < MAX_BACKENDS,
            "Backend index {} exceeds MAX_BACKENDS ({})",
            idx,
            MAX_BACKENDS
        );
        let mask = 1u8 << idx;
        if self.checked & mask == 0 {
            BackendStatus::Unknown
        } else if self.missing & mask != 0 {
            BackendStatus::Missing
        } else {
            BackendStatus::HasArticle
        }
    }
}

impl Default for ArticleAvailability {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::BackendCount;
    use crate::types::BackendId;

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
}
