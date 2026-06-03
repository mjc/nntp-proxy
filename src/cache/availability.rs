//! Article missing-state tracking across backends
//!
//! Uses bitsets to track which backends definitively do not have a specific article.
//! This is a self-contained type used by both the cache layer (persistence) and
//! the retry loop (transient tracking).
//!
//! # NNTP Response Semantics (CRITICAL)
//!
//! **430 "No Such Article" is AUTHORITATIVE** - once a backend returns 430 for
//! a cache entry, that backend stays missing for the lifetime of that entry.
//!
//! **2xx success responses are UNRELIABLE** - servers CAN give false positives,
//! so they are not represented in this structure.
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

/// Track which backends are known not to have a specific article.
///
/// Uses a `usize` bitset to track which backends returned authoritative 430.
///
/// # Example with 2 backends
/// - Initial state: `missing=00` (no backend is known missing)
/// - After backend 0 returns 430: `missing=01` (backend 0 doesn't have it)
/// - If both return 430: `missing=11` (all backends exhausted)
///
/// # Usage Pattern
/// This type serves two critical purposes:
///
/// 1. **Cache persistence** - Track availability across requests (long-lived)
///    - Store authoritative negative facts in cache entries
///    - Avoid querying backends known to be missing
///    - Updated only after 430 responses
///
/// 2. **430 retry loop** - Track which backends tried during single request (transient)
///    - Create fresh instance for each ARTICLE request
///    - Mark backends as missing when they return 430
///    - Stop when all backends exhausted or one succeeds
///
/// # Concurrency
/// Cache entries store this value directly. Memory-cache updates use atomic
/// per-key merge operations, and hybrid-cache updates use per-key locks before
/// replacing an entry. Request-local retry state owns a separate copy, then
/// records each 430 fact directly to the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArticleAvailability {
    /// Bitset of backends that DON'T have this article (returned 430)
    missing: usize,
}

impl ArticleAvailability {
    /// Create empty availability - assume all backends have article until proven otherwise
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self { missing: 0 }
    }

    /// Record that a backend returned 430 (doesn't have the article).
    ///
    /// Once marked missing, the backend should not be retried for this cache entry.
    /// Later positive observations do not clear the missing bit.
    ///
    #[inline]
    pub fn record_missing(&mut self, backend_id: BackendId) -> &mut Self {
        let mask = backend_id.availability_bit();
        self.missing |= mask; // Mark as missing
        self
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

    /// Get the checked bitset for compatibility with existing cache metadata.
    ///
    /// Missing is the only checked state represented here.
    #[inline]
    #[must_use]
    pub const fn checked_bits(&self) -> usize {
        self.missing
    }

    /// Check if all backends in the pool have been tried and returned 430
    ///
    /// Check if all backends have been tried and returned 430
    ///
    #[inline]
    #[must_use]
    pub fn all_exhausted(&self, backend_count: BackendCount) -> bool {
        let expected_missing = match backend_count.get() {
            0 => 0,
            MAX_BACKENDS => usize::MAX,
            n => (1usize << n) - 1,
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
        let _ = checked;
        Self { missing }
    }

    /// Check if we have any authoritative backend-missing information.
    ///
    /// Returns true if at least one backend is known missing.
    #[inline]
    #[must_use]
    pub const fn has_availability_info(&self) -> bool {
        self.missing != 0
    }

    /// Query backend availability status
    ///
    #[inline]
    #[must_use]
    pub fn status(&self, backend_id: BackendId) -> BackendStatus {
        let mask = backend_id.availability_bit();
        if self.missing & mask != 0 {
            BackendStatus::Missing
        } else {
            BackendStatus::Unknown
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

    fn backend_count(count: usize) -> BackendCount {
        BackendCount::try_new(count).expect("test backend count fits availability bitmap")
    }
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
    fn missing_backend_is_not_eligible() {
        let mut avail = ArticleAvailability::new();
        let b0 = BackendId::from_index(0);

        // First mark as missing
        avail.record_missing(b0);
        assert!(avail.is_missing(b0));

        assert!(avail.eligible_backend(b0).is_none());
    }

    #[test]
    fn positive_proof_does_not_change_availability() {
        let mut cache_state = ArticleAvailability::new();
        let b0 = BackendId::from_index(0);
        let b1 = BackendId::from_index(1);

        cache_state.record_missing(b0);
        cache_state.record_missing(b1);
        assert!(cache_state.is_missing(b0));
        assert!(cache_state.is_missing(b1));

        let fresh = ArticleAvailability::new();
        let _positive_proof = fresh
            .eligible_backend(b0)
            .expect("backend is eligible before a 430");

        assert_eq!(cache_state.missing_bits(), 0b11);
    }

    #[test]
    fn test_backend_availability_all_exhausted() {
        let mut avail = ArticleAvailability::new();

        // None missing yet
        assert!(!avail.all_exhausted(backend_count(2)));
        assert!(!avail.all_exhausted(backend_count(3)));

        // Record backends 0 and 1 as missing
        avail.record_missing(BackendId::from_index(0));
        avail.record_missing(BackendId::from_index(1));

        // All 2 backends exhausted
        assert!(avail.all_exhausted(backend_count(2)));

        // But not all 3 backends (backend 2 still untried)
        assert!(!avail.all_exhausted(backend_count(3)));
    }
}
