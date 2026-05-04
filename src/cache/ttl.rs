//! Tier-aware TTL calculations
//!
//! Higher tier backends get longer cache TTLs to reduce expensive backup server queries.
//!
//! ## Formula
//! `effective_ttl = base_ttl * (2 ^ tier)`
//!
//! | Tier | Multiplier | Example (1h base) |
//! |------|------------|-------------------|
//! | 0    | 1x         | 1 hour            |
//! | 1    | 2x         | 2 hours           |
//! | 2    | 4x         | 4 hours           |
//! | 7    | 128x       | 5.3 days          |
//! | 10   | 1024x      | 42.7 days         |
//!
//! Tiers above 63 are capped (max safe bit shift for u64).

/// Maximum tier for TTL calculation (prevents shift overflow)
///
/// Tiers above this are capped because `1u64 << 64` would panic.
/// In practice, tier 63 gives a 2^63 multiplier, which for a 1 hour base TTL
/// corresponds to an astronomically large duration (on the order of 10^15 years),
/// i.e. effectively infinite for cache purposes.
pub const MAX_TTL_TIER: u8 = 63;

/// Backend cache tier used for tier-aware TTL calculations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct CacheTier(u8);

impl CacheTier {
    #[must_use]
    pub const fn new(value: u8) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl From<u8> for CacheTier {
    fn from(value: u8) -> Self {
        Self::new(value)
    }
}

/// Millisecond Unix timestamp used by cache entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct CacheTimestampMillis(u64);

impl CacheTimestampMillis {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    #[must_use]
    pub fn now() -> Self {
        Self(now_millis())
    }
}

impl From<u64> for CacheTimestampMillis {
    fn from(value: u64) -> Self {
        Self::new(value)
    }
}

/// Base cache TTL in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct CacheTtlMillis(u64);

impl CacheTtlMillis {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    #[must_use]
    pub fn from_duration(duration: std::time::Duration) -> Self {
        Self(duration_millis_u64(duration))
    }
}

impl From<u64> for CacheTtlMillis {
    fn from(value: u64) -> Self {
        Self::new(value)
    }
}

/// Calculate effective TTL based on tier
///
/// Returns `base_ttl * (2 ^ min(tier, MAX_TTL_TIER))`
///
/// # Examples
/// ```
/// use nntp_proxy::cache::ttl::{CacheTier, CacheTtlMillis, effective_ttl};
///
/// assert_eq!(effective_ttl(CacheTtlMillis::new(1000), CacheTier::new(0)).get(), 1000);  // 1x
/// assert_eq!(effective_ttl(CacheTtlMillis::new(1000), CacheTier::new(1)).get(), 2000);  // 2x
/// assert_eq!(effective_ttl(CacheTtlMillis::new(1000), CacheTier::new(2)).get(), 4000);  // 4x
/// assert_eq!(effective_ttl(CacheTtlMillis::new(1000), CacheTier::new(10)).get(), 1_024_000); // 1024x
/// ```
#[inline]
#[must_use]
pub const fn effective_ttl(base_ttl: CacheTtlMillis, tier: CacheTier) -> CacheTtlMillis {
    let tier = tier.get();
    let capped_tier = if tier > MAX_TTL_TIER {
        MAX_TTL_TIER
    } else {
        tier
    };
    CacheTtlMillis::new(base_ttl.get().saturating_mul(1u64 << capped_tier))
}

/// Get current timestamp in milliseconds since Unix epoch
#[inline]
#[must_use]
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, duration_millis_u64)
}

#[allow(clippy::cast_possible_truncation)] // `Duration::as_millis` is saturated back into our u64 TTL counter.
fn duration_millis_u64(duration: std::time::Duration) -> u64 {
    // TTLs and timestamps are stored as u64 millisecond counters. Saturating on
    // overflow preserves ordering while avoiding a noisy error path here.
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Check if an entry has expired based on tier-aware TTL
///
/// # Arguments
/// * `inserted_at_millis` - When the entry was cached (milliseconds since epoch)
/// * `base_ttl_millis` - Base TTL in milliseconds
/// * `tier` - Backend tier (0 = primary, higher = backup)
///
/// # Examples
/// ```
/// use nntp_proxy::cache::ttl::{CacheTier, CacheTimestampMillis, CacheTtlMillis, is_expired, now_millis};
///
/// let now = now_millis();
/// assert!(!is_expired(CacheTimestampMillis::new(now), CacheTtlMillis::new(1000), CacheTier::new(0))); // Just inserted, not expired
///
/// let old = now.saturating_sub(1500);
/// assert!(is_expired(CacheTimestampMillis::new(old), CacheTtlMillis::new(1000), CacheTier::new(0))); // 1.5s ago with 1s TTL = expired
/// assert!(!is_expired(CacheTimestampMillis::new(old), CacheTtlMillis::new(1000), CacheTier::new(1))); // 1.5s ago with 2s TTL = not expired
/// ```
#[inline]
#[must_use]
pub fn is_expired(
    inserted_at: CacheTimestampMillis,
    base_ttl: CacheTtlMillis,
    tier: CacheTier,
) -> bool {
    let elapsed = now_millis().saturating_sub(inserted_at.get());
    elapsed >= effective_ttl(base_ttl, tier).get()
}

/// TTL multiplier for a given tier (for display/logging)
///
/// # Examples
/// ```
/// use nntp_proxy::cache::ttl::{CacheTier, ttl_multiplier};
///
/// assert_eq!(ttl_multiplier(CacheTier::new(0)), 1);
/// assert_eq!(ttl_multiplier(CacheTier::new(1)), 2);
/// assert_eq!(ttl_multiplier(CacheTier::new(10)), 1024);
/// assert_eq!(ttl_multiplier(CacheTier::new(63)), 1u64 << 63); // max
/// ```
#[inline]
#[must_use]
pub const fn ttl_multiplier(tier: CacheTier) -> u64 {
    let tier = tier.get();
    let capped = if tier > MAX_TTL_TIER {
        MAX_TTL_TIER
    } else {
        tier
    };
    1u64 << capped
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn ttl(value: u64) -> CacheTtlMillis {
        CacheTtlMillis::new(value)
    }

    const fn timestamp(value: u64) -> CacheTimestampMillis {
        CacheTimestampMillis::new(value)
    }

    const fn effective_ttl_ms(base_ttl_millis: u64, tier: CacheTier) -> u64 {
        effective_ttl(ttl(base_ttl_millis), tier).get()
    }

    fn is_expired_ms(inserted_at_millis: u64, base_ttl_millis: u64, tier: CacheTier) -> bool {
        is_expired(timestamp(inserted_at_millis), ttl(base_ttl_millis), tier)
    }

    // =========================================================================
    // effective_ttl tests
    // =========================================================================

    #[test]
    fn effective_ttl_tier_0_is_base() {
        assert_eq!(effective_ttl_ms(1000, CacheTier::new(0)), 1000);
        assert_eq!(effective_ttl_ms(0, CacheTier::new(0)), 0);
        assert_eq!(effective_ttl_ms(1, CacheTier::new(0)), 1);
    }

    #[test]
    fn effective_ttl_tier_1_is_2x() {
        assert_eq!(effective_ttl_ms(1000, CacheTier::new(1)), 2000);
        assert_eq!(effective_ttl_ms(500, CacheTier::new(1)), 1000);
    }

    #[test]
    fn effective_ttl_tier_2_is_4x() {
        assert_eq!(effective_ttl_ms(1000, CacheTier::new(2)), 4000);
    }

    #[test]
    fn effective_ttl_tier_3_is_8x() {
        assert_eq!(effective_ttl_ms(1000, CacheTier::new(3)), 8000);
    }

    #[test]
    fn effective_ttl_tier_7_is_128x() {
        assert_eq!(effective_ttl_ms(1000, CacheTier::new(7)), 128_000);
    }

    #[test]
    fn effective_ttl_tier_10_is_1024x() {
        assert_eq!(effective_ttl_ms(1000, CacheTier::new(10)), 1_024_000);
    }

    #[test]
    fn effective_ttl_caps_at_tier_63() {
        let tier_63_result = effective_ttl_ms(1, CacheTier::new(63));
        assert_eq!(tier_63_result, 1u64 << 63);
        // Tiers above 63 should give same result
        assert_eq!(effective_ttl_ms(1, CacheTier::new(64)), tier_63_result);
        assert_eq!(effective_ttl_ms(1, CacheTier::new(100)), tier_63_result);
        assert_eq!(effective_ttl_ms(1, CacheTier::new(255)), tier_63_result);
    }

    #[test]
    fn effective_ttl_saturates_on_overflow() {
        // Very large base TTL with high tier shouldn't overflow
        assert_eq!(effective_ttl_ms(u64::MAX, CacheTier::new(0)), u64::MAX);
        assert_eq!(effective_ttl_ms(u64::MAX, CacheTier::new(1)), u64::MAX);
        assert_eq!(effective_ttl_ms(u64::MAX, CacheTier::new(63)), u64::MAX);

        // Large value that would overflow without saturation
        // u64::MAX / 2 + 1 times 2 would overflow
        assert_eq!(
            effective_ttl_ms(u64::MAX / 2 + 1, CacheTier::new(1)),
            u64::MAX
        );
    }

    #[test]
    fn effective_ttl_zero_base() {
        // Zero TTL should stay zero regardless of tier
        assert_eq!(effective_ttl_ms(0, CacheTier::new(0)), 0);
        assert_eq!(effective_ttl_ms(0, CacheTier::new(1)), 0);
        assert_eq!(effective_ttl_ms(0, CacheTier::new(63)), 0);
        assert_eq!(effective_ttl_ms(0, CacheTier::new(255)), 0);
    }

    // =========================================================================
    // ttl_multiplier tests
    // =========================================================================

    #[test]
    fn ttl_multiplier_values() {
        assert_eq!(ttl_multiplier(CacheTier::new(0)), 1);
        assert_eq!(ttl_multiplier(CacheTier::new(1)), 2);
        assert_eq!(ttl_multiplier(CacheTier::new(2)), 4);
        assert_eq!(ttl_multiplier(CacheTier::new(3)), 8);
        assert_eq!(ttl_multiplier(CacheTier::new(4)), 16);
        assert_eq!(ttl_multiplier(CacheTier::new(5)), 32);
        assert_eq!(ttl_multiplier(CacheTier::new(6)), 64);
        assert_eq!(ttl_multiplier(CacheTier::new(7)), 128);
        assert_eq!(ttl_multiplier(CacheTier::new(10)), 1024);
        assert_eq!(ttl_multiplier(CacheTier::new(63)), 1u64 << 63);
    }

    #[test]
    fn ttl_multiplier_caps_at_tier_63() {
        let max = 1u64 << 63;
        assert_eq!(ttl_multiplier(CacheTier::new(64)), max);
        assert_eq!(ttl_multiplier(CacheTier::new(100)), max);
        assert_eq!(ttl_multiplier(CacheTier::new(255)), max);
    }

    // =========================================================================
    // is_expired tests
    // =========================================================================

    #[test]
    fn is_expired_fresh_entry() {
        let now = now_millis();
        assert!(!is_expired_ms(now, 1000, CacheTier::new(0))); // Just inserted, 1s TTL
        assert!(!is_expired_ms(now, 100, CacheTier::new(0))); // Just inserted, 100ms TTL (1ms too tight for test timing)
    }

    #[test]
    fn is_expired_old_entry() {
        let old = now_millis().saturating_sub(2000);
        assert!(is_expired_ms(old, 1000, CacheTier::new(0))); // 2s ago, 1s TTL = expired
        assert!(is_expired_ms(old, 1999, CacheTier::new(0))); // 2s ago, 1.999s TTL = expired
    }

    #[test]
    fn is_expired_boundary() {
        let inserted = now_millis().saturating_sub(1000);
        // Exactly at TTL boundary - should be expired (>= comparison)
        assert!(is_expired_ms(inserted, 1000, CacheTier::new(0)));
        // Well under TTL - not expired (use 2000ms to avoid test timing issues)
        assert!(!is_expired_ms(inserted, 2000, CacheTier::new(0)));
    }

    #[test]
    fn is_expired_respects_tier() {
        let inserted = now_millis().saturating_sub(1500);
        // 1.5s ago with 1s base TTL
        assert!(is_expired_ms(inserted, 1000, CacheTier::new(0))); // 1s TTL - expired
        assert!(!is_expired_ms(inserted, 1000, CacheTier::new(1))); // 2s TTL - not expired
        assert!(!is_expired_ms(inserted, 1000, CacheTier::new(2))); // 4s TTL - not expired
    }

    #[test]
    fn is_expired_high_tier_extends_ttl() {
        let inserted = now_millis().saturating_sub(100_000); // 100s ago
        // With 1s base TTL
        assert!(is_expired_ms(inserted, 1000, CacheTier::new(0))); // 1s TTL - expired
        assert!(is_expired_ms(inserted, 1000, CacheTier::new(1))); // 2s TTL - expired
        assert!(is_expired_ms(inserted, 1000, CacheTier::new(5))); // 32s TTL - expired
        assert!(is_expired_ms(inserted, 1000, CacheTier::new(6))); // 64s TTL - expired
        assert!(!is_expired_ms(inserted, 1000, CacheTier::new(7))); // 128s TTL - not expired
        assert!(!is_expired_ms(inserted, 1000, CacheTier::new(10))); // 1024s TTL - not expired
    }

    #[test]
    fn is_expired_zero_ttl() {
        let now = now_millis();
        // Zero TTL = everything expires immediately
        assert!(is_expired_ms(now, 0, CacheTier::new(0)));
        assert!(is_expired_ms(now, 0, CacheTier::new(63))); // Even with tier 63, 0 * 2^63 = 0
    }

    #[test]
    fn is_expired_future_timestamp() {
        // Edge case: timestamp in the future (clock skew)
        let future = now_millis().saturating_add(10000);
        // Should not be expired - elapsed would be 0 due to saturating_sub
        assert!(!is_expired_ms(future, 1000, CacheTier::new(0)));
    }

    // =========================================================================
    // now_millis tests
    // =========================================================================

    #[test]
    fn now_millis_is_reasonable() {
        let now = now_millis();
        // Sanity check: should be after 2024-01-01 (roughly 1704067200000 ms)
        // No upper bound to avoid time-bomb failures as years pass
        assert!(now > 1_700_000_000_000);
    }

    #[test]
    fn now_millis_is_monotonic() {
        let t1 = now_millis();
        let t2 = now_millis();
        assert!(t2 >= t1);
    }
}
