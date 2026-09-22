//! Article missing-state tracking across provider slots.
//!
//! A bitset records which provider slots returned `430` for an article. The
//! cache stores this state across requests; retry loops keep a separate copy
//! for the current request.
//!
//! # NNTP response semantics
//!
//! A `430` ("No Such Article") response marks the provider slot as missing until
//! that negative fact expires or the cache entry is replaced. A hybrid cache
//! entry can retain its payload after an expired missing bit has been cleared.
//!
//! Servers can return false-positive `2xx` responses, so this type records only
//! misses.
//!
//! Availability uses `usize` bitmaps, so local retry checks remain compact while
//! allowing the backend count to grow with the target word size.

use super::{AvailabilityMask, AvailabilitySlot};
use crate::types::BackendId;

/// Maximum number of backends supported by `ArticleAvailability` bitset.
///
/// This is deliberately a fixed `usize` bitmap. It used to be u8; it is wider
/// now because I am tired of being harassed by shitty robots about 8 backends.
pub const MAX_BACKENDS: usize = BackendId::MAX_COUNT;

/// Status of a provider slot for a specific article.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendStatus {
    /// Backend hasn't been checked yet
    Unknown,
    /// Backend was checked and returned 430 (doesn't have article)
    Missing,
}

/// Track which provider slots are known not to have a specific article.
///
/// Each bit in the `usize` bitset represents a provider slot that returned `430`.
///
/// # Example with 2 provider slots
/// - Initial state: `missing=00` (no provider slot is known missing)
/// - After provider slot 0 returns 430: `missing=01` (slot 0 doesn't have it)
/// - If both return 430: `missing=11` (all configured provider slots exhausted)
///
/// # Usage
///
/// Cache entries retain these bits across requests to avoid querying providers
/// known to be missing an article. Only a `430` response sets a missing bit.
///
/// Each ARTICLE retry loop starts with a fresh instance and records provider
/// slots that return `430`. It stops when all configured slots are exhausted
/// or a request succeeds.
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
    /// Start with every provider slot eligible to be tried.
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self { missing: 0 }
    }

    /// Record that a backend returned 430 (doesn't have the article).
    ///
    /// The backend should not be retried while this negative fact is valid.
    /// Later positive observations do not clear the bit; cache expiry handling
    /// can clear it independently of the payload.
    ///
    #[inline]
    pub fn record_missing_slot(&mut self, slot: AvailabilitySlot) -> &mut Self {
        self.missing |= slot.bit();
        self
    }

    /// Check if a provider slot is known to be missing (returned 430).
    ///
    #[inline]
    #[must_use]
    pub fn is_missing_slot(&self, slot: AvailabilitySlot) -> bool {
        self.missing & slot.bit() != 0
    }

    #[inline]
    #[must_use]
    pub(crate) fn should_try_slot(&self, slot: AvailabilitySlot) -> bool {
        !self.is_missing_slot(slot)
    }

    /// Get the raw missing bitset for debugging
    #[inline]
    #[must_use]
    pub const fn missing_bits(&self) -> usize {
        self.missing
    }

    /// Check if every configured provider slot returned an authoritative 430.
    ///
    /// The mask is supplied by the router's identity layout. It must not be
    /// reconstructed from a transport backend count because several backend
    /// entries may share one provider slot.
    ///
    #[inline]
    #[must_use]
    pub fn all_exhausted(&self, configured: AvailabilityMask) -> bool {
        self.missing & configured.bits() == configured.bits()
    }

    /// Reconstruct from stored negative bits.
    #[inline]
    #[must_use]
    pub(crate) const fn from_missing_bits(missing: usize) -> Self {
        Self { missing }
    }

    /// Return whether at least one provider slot has a recorded `430`.
    #[inline]
    #[must_use]
    pub const fn has_availability_info(&self) -> bool {
        self.missing != 0
    }

    /// Query provider-slot availability status.
    ///
    #[inline]
    #[must_use]
    pub fn status(&self, slot: AvailabilitySlot) -> BackendStatus {
        let mask = slot.bit();
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
    use super::super::availability_identity::AvailabilityIdentity;
    use super::*;
    use crate::cache::AvailabilityLayout;
    use crate::config::Server;
    use crate::types::Port;

    fn provider_mask(count: usize) -> AvailabilityMask {
        let slots = (0..count)
            .map(|index| AvailabilitySlot::new(index).unwrap())
            .collect::<Vec<_>>();
        AvailabilityMask::from_slots(&slots)
    }
    use crate::types::BackendId;

    #[test]
    fn test_backend_availability_basic() {
        let mut avail = ArticleAvailability::new();
        let b0 = AvailabilitySlot::new(0).unwrap();
        let b1 = AvailabilitySlot::new(1).unwrap();

        // Default: assume all backends have it
        assert!(avail.should_try_slot(b0));
        assert!(avail.should_try_slot(b1));

        // Record b0 as missing (returned 430)
        avail.record_missing_slot(b0);
        assert!(!avail.should_try_slot(b0)); // Should not try again
        assert!(avail.should_try_slot(b1)); // Still should try

        // Record b1 as missing too
        avail.record_missing_slot(b1);
        assert!(!avail.should_try_slot(b1));
    }

    #[test]
    fn missing_backend_is_not_eligible() {
        let mut avail = ArticleAvailability::new();
        let b0 = AvailabilitySlot::new(0).unwrap();

        // First mark as missing
        avail.record_missing_slot(b0);
        assert!(avail.is_missing_slot(b0));

        assert!(avail.is_missing_slot(b0));
    }

    #[test]
    fn success_observation_does_not_change_availability() {
        let mut cache_state = ArticleAvailability::new();
        let b0 = AvailabilitySlot::new(0).unwrap();
        let b1 = AvailabilitySlot::new(1).unwrap();

        cache_state.record_missing_slot(b0);
        cache_state.record_missing_slot(b1);
        assert!(cache_state.is_missing_slot(b0));
        assert!(cache_state.is_missing_slot(b1));

        let fresh = ArticleAvailability::new();
        assert!(!fresh.is_missing_slot(b0));

        assert_eq!(cache_state.missing_bits(), 0b11);
    }

    #[test]
    fn test_backend_availability_all_exhausted() {
        let mut avail = ArticleAvailability::new();

        // None missing yet
        assert!(!avail.all_exhausted(provider_mask(2)));
        assert!(!avail.all_exhausted(provider_mask(3)));

        // Record backends 0 and 1 as missing
        avail.record_missing_slot(AvailabilitySlot::new(0).unwrap());
        avail.record_missing_slot(AvailabilitySlot::new(1).unwrap());

        // All 2 backends exhausted
        assert!(avail.all_exhausted(provider_mask(2)));

        // But not all 3 backends (backend 2 still untried)
        assert!(!avail.all_exhausted(provider_mask(3)));
    }

    #[test]
    fn same_host_shares_an_availability_slot_across_ports_and_accounts() {
        let servers = [
            Server::builder("news.example", Port::try_new(119).unwrap())
                .username("reader")
                .password("one")
                .build()
                .unwrap(),
            Server::builder("news.example", Port::try_new(563).unwrap())
                .username("reader")
                .password("two")
                .build()
                .unwrap(),
        ];
        let layout = AvailabilityLayout::from_servers(&servers).unwrap();

        assert_eq!(
            layout.slot_for_backend(BackendId::from_index(0)),
            layout.slot_for_backend(BackendId::from_index(1))
        );
        assert_eq!(layout.identity_count(), 1);
    }

    #[test]
    fn host_identity_canonicalizes_dns_case_and_trailing_dot() {
        let servers = [
            Server::builder("News.Example", Port::try_new(119).unwrap())
                .build()
                .unwrap(),
            Server::builder("news.example.", Port::try_new(563).unwrap())
                .build()
                .unwrap(),
        ];
        let layout = AvailabilityLayout::from_servers(&servers).unwrap();

        assert_eq!(
            layout.slot_for_backend(BackendId::from_index(0)),
            layout.slot_for_backend(BackendId::from_index(1))
        );
        assert_eq!(layout.identity_count(), 1);
    }

    #[test]
    fn availability_identity_ignores_account_and_port_but_not_host() {
        let servers = [
            Server::builder("news.example", Port::try_new(119).unwrap())
                .username("reader")
                .password("first-secret")
                .build()
                .unwrap(),
            Server::builder("news.example", Port::try_new(563).unwrap())
                .username("writer")
                .password("second-secret")
                .build()
                .unwrap(),
            Server::builder("news.example", Port::try_new(119).unwrap())
                .build()
                .unwrap(),
        ];
        let layout = AvailabilityLayout::from_servers(&servers).unwrap();

        assert_eq!(
            layout.slot_for_backend(BackendId::from_index(0)),
            layout.slot_for_backend(BackendId::from_index(1))
        );
        assert_eq!(
            layout.slot_for_backend(BackendId::from_index(0)),
            layout.slot_for_backend(BackendId::from_index(2))
        );
        let different_host = [
            Server::builder("other.example", Port::try_new(119).unwrap())
                .username("reader")
                .password("third-secret")
                .build()
                .unwrap(),
        ];
        let different_layout =
            AvailabilityLayout::from_servers(&[servers[0].clone(), different_host[0].clone()])
                .unwrap();
        assert_ne!(
            different_layout.slot_for_backend(BackendId::from_index(0)),
            different_layout.slot_for_backend(BackendId::from_index(1))
        );
        assert_eq!(different_layout.identity_count(), 2);
    }

    #[test]
    fn different_hosts_do_not_share_an_availability_slot() {
        let servers = [
            Server::builder("news-a.example", Port::try_new(119).unwrap())
                .username("reader")
                .password("first-secret")
                .build()
                .unwrap(),
            Server::builder("news-b.example", Port::try_new(563).unwrap())
                .username("reader")
                .password("second-secret")
                .build()
                .unwrap(),
        ];
        let layout = AvailabilityLayout::from_servers(&servers).unwrap();

        assert_ne!(
            layout.slot_for_backend(BackendId::from_index(0)),
            layout.slot_for_backend(BackendId::from_index(1))
        );
        assert_eq!(layout.identity_count(), 2);
    }

    #[test]
    fn hybrid_registry_appends_without_reusing_slots() {
        let directory = tempfile::tempdir().unwrap();
        let first = [Server::builder("news.example", Port::try_new(119).unwrap())
            .username("reader")
            .password("secret")
            .build()
            .unwrap()];
        let first_layout =
            AvailabilityLayout::from_hybrid_registry(&first, directory.path()).unwrap();
        let first_slot = first_layout.slot_for_backend(BackendId::from_index(0));

        let expanded = [
            first[0].clone(),
            Server::builder("other.example", Port::try_new(119).unwrap())
                .build()
                .unwrap(),
        ];
        let expanded_layout =
            AvailabilityLayout::from_hybrid_registry(&expanded, directory.path()).unwrap();
        assert_eq!(
            first_slot,
            expanded_layout
                .slot_for_identity(&AvailabilityIdentity::from(&first[0].host))
                .unwrap()
        );

        let reordered = [expanded[1].clone(), expanded[0].clone()];
        let reordered_layout =
            AvailabilityLayout::from_hybrid_registry(&reordered, directory.path()).unwrap();
        assert_eq!(
            first_slot,
            reordered_layout
                .slot_for_identity(&AvailabilityIdentity::from(&first[0].host))
                .unwrap()
        );
        let registry = std::fs::read(directory.path().join("availability.registry")).unwrap();
        assert!(
            !registry
                .windows(b"secret".len())
                .any(|window| window == b"secret")
        );
    }

    #[test]
    fn corrupt_hybrid_registry_starts_new_epoch_without_reusing_current_slots() {
        let directory = tempfile::tempdir().unwrap();
        let servers = [Server::builder("news.example", Port::try_new(119).unwrap())
            .username("reader")
            .password("secret")
            .build()
            .unwrap()];
        let initial = AvailabilityLayout::from_hybrid_registry(&servers, directory.path()).unwrap();
        let old_epoch = initial.availability_epoch();
        std::fs::write(directory.path().join("availability.registry"), b"corrupt").unwrap();

        let recovered =
            AvailabilityLayout::from_hybrid_registry(&servers, directory.path()).unwrap();
        assert_ne!(recovered.availability_epoch(), old_epoch);
        assert_eq!(
            recovered.slot_for_backend(BackendId::from_index(0)),
            AvailabilitySlot::new(0).unwrap()
        );
    }
}
