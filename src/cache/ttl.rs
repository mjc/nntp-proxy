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

/// Calculate effective TTL based on tier
///
/// Returns `base_ttl * (2 ^ min(tier, MAX_TTL_TIER))`
///
/// # Examples
/// ```
/// use nntp_proxy::cache::ttl::effective_ttl;
///
/// assert_eq!(effective_ttl(1000, 0), 1000);  // 1x
/// assert_eq!(effective_ttl(1000, 1), 2000);  // 2x
/// assert_eq!(effective_ttl(1000, 2), 4000);  // 4x
/// assert_eq!(effective_ttl(1000, 10), 1_024_000); // 1024x
/// ```
#[inline]
#[must_use]
pub const fn effective_ttl(base_ttl_millis: u64, tier: u8) -> u64 {
    let capped_tier = if tier > MAX_TTL_TIER {
        MAX_TTL_TIER
    } else {
        tier
    };
    base_ttl_millis.saturating_mul(1u64 << capped_tier)
}

/// Get current timestamp in milliseconds since Unix epoch
#[inline]
#[must_use]
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
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
/// use nntp_proxy::cache::ttl::{is_expired, now_millis};
///
/// let now = now_millis();
/// assert!(!is_expired(now, 1000, 0)); // Just inserted, not expired
///
/// let old = now.saturating_sub(1500);
/// assert!(is_expired(old, 1000, 0)); // 1.5s ago with 1s TTL = expired
/// assert!(!is_expired(old, 1000, 1)); // 1.5s ago with 2s TTL = not expired
/// ```
#[inline]
#[must_use]
pub fn is_expired(inserted_at_millis: u64, base_ttl_millis: u64, tier: u8) -> bool {
    let elapsed = now_millis().saturating_sub(inserted_at_millis);
    elapsed >= effective_ttl(base_ttl_millis, tier)
}

/// TTL multiplier for a given tier (for display/logging)
///
/// # Examples
/// ```
/// use nntp_proxy::cache::ttl::ttl_multiplier;
///
/// assert_eq!(ttl_multiplier(0), 1);
/// assert_eq!(ttl_multiplier(1), 2);
/// assert_eq!(ttl_multiplier(10), 1024);
/// assert_eq!(ttl_multiplier(63), 1u64 << 63); // max
/// ```
#[inline]
#[must_use]
pub const fn ttl_multiplier(tier: u8) -> u64 {
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

    // =========================================================================
    // effective_ttl tests
    // =========================================================================

    #[test]
    fn effective_ttl_tier_0_is_base() {
        assert_eq!(effective_ttl(1000, 0), 1000);
        assert_eq!(effective_ttl(0, 0), 0);
        assert_eq!(effective_ttl(1, 0), 1);
    }

    #[test]
    fn effective_ttl_tier_1_is_2x() {
        assert_eq!(effective_ttl(1000, 1), 2000);
        assert_eq!(effective_ttl(500, 1), 1000);
    }

    #[test]
    fn effective_ttl_tier_2_is_4x() {
        assert_eq!(effective_ttl(1000, 2), 4000);
    }

    #[test]
    fn effective_ttl_tier_3_is_8x() {
        assert_eq!(effective_ttl(1000, 3), 8000);
    }

    #[test]
    fn effective_ttl_tier_7_is_128x() {
        assert_eq!(effective_ttl(1000, 7), 128_000);
    }

    #[test]
    fn effective_ttl_tier_10_is_1024x() {
        assert_eq!(effective_ttl(1000, 10), 1_024_000);
    }

    #[test]
    fn effective_ttl_caps_at_tier_63() {
        let tier_63_result = effective_ttl(1, 63);
        assert_eq!(tier_63_result, 1u64 << 63);
        // Tiers above 63 should give same result
        assert_eq!(effective_ttl(1, 64), tier_63_result);
        assert_eq!(effective_ttl(1, 100), tier_63_result);
        assert_eq!(effective_ttl(1, 255), tier_63_result);
    }

    #[test]
    fn effective_ttl_saturates_on_overflow() {
        // Very large base TTL with high tier shouldn't overflow
        assert_eq!(effective_ttl(u64::MAX, 0), u64::MAX);
        assert_eq!(effective_ttl(u64::MAX, 1), u64::MAX);
        assert_eq!(effective_ttl(u64::MAX, 63), u64::MAX);

        // Large value that would overflow without saturation
        // u64::MAX / 2 + 1 times 2 would overflow
        assert_eq!(effective_ttl(u64::MAX / 2 + 1, 1), u64::MAX);
    }

    #[test]
    fn effective_ttl_zero_base() {
        // Zero TTL should stay zero regardless of tier
        assert_eq!(effective_ttl(0, 0), 0);
        assert_eq!(effective_ttl(0, 1), 0);
        assert_eq!(effective_ttl(0, 63), 0);
        assert_eq!(effective_ttl(0, 255), 0);
    }

    // =========================================================================
    // ttl_multiplier tests
    // =========================================================================

    #[test]
    fn ttl_multiplier_values() {
        assert_eq!(ttl_multiplier(0), 1);
        assert_eq!(ttl_multiplier(1), 2);
        assert_eq!(ttl_multiplier(2), 4);
        assert_eq!(ttl_multiplier(3), 8);
        assert_eq!(ttl_multiplier(4), 16);
        assert_eq!(ttl_multiplier(5), 32);
        assert_eq!(ttl_multiplier(6), 64);
        assert_eq!(ttl_multiplier(7), 128);
        assert_eq!(ttl_multiplier(10), 1024);
        assert_eq!(ttl_multiplier(63), 1u64 << 63);
    }

    #[test]
    fn ttl_multiplier_caps_at_tier_63() {
        let max = 1u64 << 63;
        assert_eq!(ttl_multiplier(64), max);
        assert_eq!(ttl_multiplier(100), max);
        assert_eq!(ttl_multiplier(255), max);
    }

    // =========================================================================
    // is_expired tests
    // =========================================================================

    #[test]
    fn is_expired_fresh_entry() {
        let now = now_millis();
        assert!(!is_expired(now, 1000, 0)); // Just inserted, 1s TTL
        assert!(!is_expired(now, 100, 0)); // Just inserted, 100ms TTL (1ms too tight for test timing)
    }

    #[test]
    fn is_expired_old_entry() {
        let old = now_millis().saturating_sub(2000);
        assert!(is_expired(old, 1000, 0)); // 2s ago, 1s TTL = expired
        assert!(is_expired(old, 1999, 0)); // 2s ago, 1.999s TTL = expired
    }

    #[test]
    fn is_expired_boundary() {
        let inserted = now_millis().saturating_sub(1000);
        // Exactly at TTL boundary - should be expired (>= comparison)
        assert!(is_expired(inserted, 1000, 0));
        // Well under TTL - not expired (use 2000ms to avoid test timing issues)
        assert!(!is_expired(inserted, 2000, 0));
    }

    #[test]
    fn is_expired_respects_tier() {
        let inserted = now_millis().saturating_sub(1500);
        // 1.5s ago with 1s base TTL
        assert!(is_expired(inserted, 1000, 0)); // 1s TTL - expired
        assert!(!is_expired(inserted, 1000, 1)); // 2s TTL - not expired
        assert!(!is_expired(inserted, 1000, 2)); // 4s TTL - not expired
    }

    #[test]
    fn is_expired_high_tier_extends_ttl() {
        let inserted = now_millis().saturating_sub(100_000); // 100s ago
        // With 1s base TTL
        assert!(is_expired(inserted, 1000, 0)); // 1s TTL - expired
        assert!(is_expired(inserted, 1000, 1)); // 2s TTL - expired
        assert!(is_expired(inserted, 1000, 5)); // 32s TTL - expired
        assert!(is_expired(inserted, 1000, 6)); // 64s TTL - expired
        assert!(!is_expired(inserted, 1000, 7)); // 128s TTL - not expired
        assert!(!is_expired(inserted, 1000, 10)); // 1024s TTL - not expired
    }

    #[test]
    fn is_expired_zero_ttl() {
        let now = now_millis();
        // Zero TTL = everything expires immediately
        assert!(is_expired(now, 0, 0));
        assert!(is_expired(now, 0, 63)); // Even with tier 63, 0 * 2^63 = 0
    }

    #[test]
    fn is_expired_future_timestamp() {
        // Edge case: timestamp in the future (clock skew)
        let future = now_millis().saturating_add(10000);
        // Should not be expired - elapsed would be 0 due to saturating_sub
        assert!(!is_expired(future, 1000, 0));
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
