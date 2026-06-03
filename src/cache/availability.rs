//! Article availability tracking across backends
//!
//! Uses bitsets to track which backends have (or don't have) a specific article.
//! This is a self-contained type used by both the cache layer (persistence) and
//! the retry loop (transient tracking).
//!
//! # NNTP Response Semantics (CRITICAL)
//!
//! **430 "No Such Article" is AUTHORITATIVE** - once a backend returns 430 for
//! a cache entry, that backend stays missing for the lifetime of that entry.
//!
//! **2xx success responses are UNRELIABLE** - servers CAN give false positives.
//! A server might return 220/222/223 but then provide corrupt or incomplete data.
//!
//! **Therefore: 430 (missing) ALWAYS takes precedence over "has" state.**
//!
//! Availability uses `usize` bitmaps, so local retry checks remain compact while
//! allowing the backend count to grow with the target word size.

use crate::router::BackendCount;
use crate::types::BackendId;

/// Maximum number of backends supported by `ArticleAvailability` bitset.
///
/// This is deliberately a fixed `usize` bitmap. It used to be u8; it is wider
/// now because I am tired of being harassed by shitty robots about 8 backends.
pub const MAX_BACKENDS: usize = BackendId::MAX_COUNT;

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

/// A backend that is known to be eligible for an article attempt.
///
/// This is intentionally not constructible outside this module. Code that sends
/// an article request to a backend or records a positive article observation must
/// first prove the backend was not already known missing by asking
/// [`ArticleAvailability`].
#[derive(Debug, PartialEq, Eq)]
pub struct EligibleArticleBackend {
    backend_id: BackendId,
}

impl EligibleArticleBackend {
    #[inline]
    #[must_use]
    pub const fn backend_id(&self) -> BackendId {
        self.backend_id
    }

    #[inline]
    #[must_use]
    pub fn as_index(&self) -> usize {
        self.backend_id.as_index()
    }

    #[inline]
    #[must_use]
    pub const fn positive_observation(&self) -> ArticleBackendHasArticle {
        ArticleBackendHasArticle {
            backend_id: self.backend_id,
        }
    }
}

/// Proof that a backend produced a positive article response.
///
/// This token is intentionally not accepted by request execution. It can be
/// copied into async cache updates without duplicating the executable
/// [`EligibleArticleBackend`] retry token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArticleBackendHasArticle {
    backend_id: BackendId,
}

impl ArticleBackendHasArticle {
    #[inline]
    #[must_use]
    pub const fn backend_id(self) -> BackendId {
        self.backend_id
    }
}

/// Track which backends have (or don't have) a specific article
///
/// Uses two `usize` bitsets to track availability:
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
    checked: usize,
    /// Bitset of backends that DON'T have this article (returned 430)
    missing: usize,
}

impl ArticleAvailability {
    /// Create empty availability - assume all backends have article until proven otherwise
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self {
            checked: 0,
            missing: 0,
        }
    }

    /// Record that a backend returned 430 (doesn't have the article).
    ///
    /// Once marked missing, the backend should not be retried for this cache entry.
    /// Later positive observations do not clear the missing bit.
    ///
    #[inline]
    pub fn record_missing(&mut self, backend_id: BackendId) -> &mut Self {
        let mask = backend_id.availability_bit();
        self.checked |= mask; // Mark as checked
        self.missing |= mask; // Mark as missing
        self
    }

    /// Record that a backend returned a success response (2xx) for this article.
    ///
    /// This marks the backend as checked. It deliberately does not clear an
    /// existing missing bit: within a live cache entry, 430 state is permanent.
    ///
    #[inline]
    pub fn record_has(&mut self, backend: &EligibleArticleBackend) -> &mut Self {
        let mask = backend.backend_id.availability_bit();
        self.checked |= mask; // Mark as checked
        self
    }

    /// Record a positive article observation that has already passed through
    /// the retry eligibility path.
    #[inline]
    pub fn record_observed_has(&mut self, backend: ArticleBackendHasArticle) -> &mut Self {
        let mask = backend.backend_id.availability_bit();
        self.checked |= mask;
        self
    }

    /// Merge another availability's state into this one
    ///
    /// Used to sync local availability tracking back to cache.
    /// Takes the union of checked backends and missing backends.
    ///
    /// # NNTP Semantics (CRITICAL)
    ///
    /// **430 responses are AUTHORITATIVE** - once a live cache entry records a
    /// 430 for a backend, later success observations do not clear it.
    ///
    /// **2xx responses are UNRELIABLE** - servers CAN give false positives.
    /// A server might claim to have an article but return corrupt data, or the
    /// article might be incomplete/unavailable due to propagation delays.
    ///
    /// Therefore: `missing` state ALWAYS wins over `has` state.
    /// We trust 430s absolutely but treat successes with skepticism.
    #[inline]
    pub const fn merge_from(&mut self, other: &Self) {
        // Simple union: trust all 430s from both sources
        // 430 is authoritative, so missing bits should accumulate
        self.checked |= other.checked;
        self.missing |= other.missing;
    }

    /// Check if a backend is known to be missing (returned 430)
    ///
    #[inline]
    #[must_use]
    pub fn is_missing(&self, backend_id: BackendId) -> bool {
        self.missing & backend_id.availability_bit() != 0
    }

    /// Check if we should attempt to fetch from this backend
    ///
    /// Returns `true` if backend might have the article (not yet marked missing).
    ///
    #[inline]
    #[must_use]
    pub(crate) fn should_try(&self, backend_id: BackendId) -> bool {
        !self.is_missing(backend_id)
    }

    /// Return an article-attempt token only if this backend is not known missing.
    #[inline]
    #[must_use]
    pub fn eligible_backend(&self, backend_id: BackendId) -> Option<EligibleArticleBackend> {
        self.should_try(backend_id)
            .then_some(EligibleArticleBackend { backend_id })
    }

    /// Get the raw missing bitset for debugging
    #[inline]
    #[must_use]
    pub const fn missing_bits(&self) -> usize {
        self.missing
    }

    /// Get the raw checked bitset for debugging
    #[inline]
    #[must_use]
    pub const fn checked_bits(&self) -> usize {
        self.checked
    }

    /// Check if all backends in the pool have been tried and returned 430
    ///
    /// Check if all backends have been tried and returned 430
    ///
    #[inline]
    #[must_use]
    pub fn all_exhausted(&self, backend_count: BackendCount) -> bool {
        let count = backend_count.get();
        debug_assert!(
            count <= MAX_BACKENDS,
            "Backend count {count} exceeds MAX_BACKENDS ({MAX_BACKENDS})"
        );
        let expected_missing = match count {
            0 => 0,
            MAX_BACKENDS => usize::MAX,
            n => 1usize
                .checked_shl(n as u32)
                .map(|mask| mask - 1)
                .expect("backend count exceeds usize availability bitset"),
        };
        self.missing & expected_missing == expected_missing
    }

    /// Get the underlying bitset value (for debugging)
    #[inline]
    #[must_use]
    pub const fn as_usize(&self) -> usize {
        self.missing
    }

    /// Reconstruct from raw bitset values (used for deserialization)
    ///
    /// # Safety
    /// The caller must ensure the bits represent valid backend states.
    /// This is primarily used when deserializing from disk cache.
    #[inline]
    #[must_use]
    pub(crate) const fn from_bits(checked: usize, missing: usize) -> Self {
        Self { checked, missing }
    }

    /// Check if we have any backend availability information
    ///
    /// Returns true if at least one backend has been checked.
    /// If this returns false, we haven't tried any backends yet and shouldn't
    /// serve from cache (should try backends first).
    #[inline]
    #[must_use]
    pub const fn has_availability_info(&self) -> bool {
        self.checked != 0
    }

    /// Check if any backend is known to HAVE the article
    ///
    /// Returns true if at least one backend was checked and did NOT return 430.
    /// This is the inverse check from `all_exhausted` - at least one success.
    #[inline]
    #[must_use]
    pub const fn any_backend_has_article(&self) -> bool {
        // A backend "has" the article if it's checked but not missing
        // checked & !missing gives us the backends that have it
        (self.checked & !self.missing) != 0
    }

    /// Query backend availability status
    ///
    #[inline]
    #[must_use]
    pub fn status(&self, backend_id: BackendId) -> BackendStatus {
        let mask = backend_id.availability_bit();
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
        let b1_eligible = avail.eligible_backend(b1).unwrap();
        avail.record_has(&b1_eligible);
        assert!(avail.any_backend_has_article());

        // Record b1 as missing (overwrite) - none have it again
        avail.record_missing(b1);
        assert!(!avail.any_backend_has_article());
    }

    #[test]
    fn missing_backend_is_not_eligible_for_record_has() {
        let mut avail = ArticleAvailability::new();
        let b0 = BackendId::from_index(0);

        // First mark as missing
        avail.record_missing(b0);
        assert!(avail.is_missing(b0));
        assert!(!avail.any_backend_has_article());

        assert!(avail.eligible_backend(b0).is_none());
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
        let b0_eligible = cache_entry.eligible_backend(b0).unwrap();
        cache_entry.record_has(&b0_eligible);
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
        let b0_eligible = new_fetch.eligible_backend(b0).unwrap();
        new_fetch.record_has(&b0_eligible);

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
