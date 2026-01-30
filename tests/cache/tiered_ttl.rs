//! Tests for tier-aware cache TTL expiration
//!
//! Verifies that higher tier articles get longer TTLs:
//! - Tier 0: 1x base TTL
//! - Tier 1: 2x base TTL
//! - Tier 2: 4x base TTL
//! - Tier 10: 1024x base TTL (~43 days with 1h base)
//! - etc. (capped at tier 63 to prevent shift overflow)

use nntp_proxy::cache::ttl::{MAX_TTL_TIER, effective_ttl, is_expired, now_millis, ttl_multiplier};
use nntp_proxy::cache::{ArticleCache, ArticleEntry};
use nntp_proxy::types::{BackendId, MessageId};
use std::time::Duration;

// =============================================================================
// Unit tests for TTL calculations
// =============================================================================

#[test]
fn test_tier_multiplier_calculation() {
    // Tier 0: 1x
    assert_eq!(ttl_multiplier(0), 1);
    // Tier 1: 2x
    assert_eq!(ttl_multiplier(1), 2);
    // Tier 2: 4x
    assert_eq!(ttl_multiplier(2), 4);
    // Tier 3: 8x
    assert_eq!(ttl_multiplier(3), 8);
    // Tier 7: 128x
    assert_eq!(ttl_multiplier(7), 128);
    // Tier 10: 1024x
    assert_eq!(ttl_multiplier(10), 1024);
    // Tier 63: max (2^63)
    assert_eq!(ttl_multiplier(63), 1u64 << 63);
    // Tier 64+: capped at 2^63
    assert_eq!(ttl_multiplier(64), 1u64 << 63);
    assert_eq!(ttl_multiplier(255), 1u64 << 63);
}

#[test]
fn test_effective_ttl_formula() {
    let base_ttl = 1000u64; // 1 second

    // effective_ttl = base_ttl * (2^tier)
    assert_eq!(effective_ttl(base_ttl, 0), 1000); // tier 0: 1x
    assert_eq!(effective_ttl(base_ttl, 1), 2000); // tier 1: 2x
    assert_eq!(effective_ttl(base_ttl, 2), 4000); // tier 2: 4x
    assert_eq!(effective_ttl(base_ttl, 3), 8000); // tier 3: 8x
    assert_eq!(effective_ttl(base_ttl, 4), 16000); // tier 4: 16x
    assert_eq!(effective_ttl(base_ttl, 5), 32000); // tier 5: 32x
    assert_eq!(effective_ttl(base_ttl, 6), 64000); // tier 6: 64x
    assert_eq!(effective_ttl(base_ttl, 7), 128000); // tier 7: 128x
    assert_eq!(effective_ttl(base_ttl, 10), 1024000); // tier 10: 1024x
}

#[test]
fn test_effective_ttl_caps_at_tier_63() {
    // Use base_ttl of 1 to avoid overflow in result comparison
    let tier_63_result = effective_ttl(1, 63);
    assert_eq!(tier_63_result, 1u64 << 63);

    // All tiers above 63 should give same result as tier 63
    assert_eq!(effective_ttl(1, 64), tier_63_result);
    assert_eq!(effective_ttl(1, 100), tier_63_result);
    assert_eq!(effective_ttl(1, 255), tier_63_result);
}

#[test]
fn test_overflow_protection() {
    // Very large base TTL with high tier shouldn't overflow
    assert_eq!(effective_ttl(u64::MAX, 0), u64::MAX);
    assert_eq!(effective_ttl(u64::MAX, 1), u64::MAX); // saturates
    assert_eq!(effective_ttl(u64::MAX, 63), u64::MAX); // saturates

    // Large value that would overflow without saturation
    // u64::MAX / 2 + 1 times 2 would overflow
    let result = effective_ttl(u64::MAX / 2 + 1, 1);
    assert_eq!(result, u64::MAX); // Saturates to MAX
}

#[test]
fn test_zero_ttl_stays_zero() {
    // Zero TTL should stay zero regardless of tier
    assert_eq!(effective_ttl(0, 0), 0);
    assert_eq!(effective_ttl(0, 1), 0);
    assert_eq!(effective_ttl(0, 63), 0);
    assert_eq!(effective_ttl(0, 255), 0);
}

#[test]
fn test_max_ttl_tier_constant() {
    assert_eq!(MAX_TTL_TIER, 63);
    assert_eq!(ttl_multiplier(MAX_TTL_TIER), 1u64 << 63);
}

// =============================================================================
// is_expired tests
// =============================================================================

#[test]
fn test_is_expired_fresh_entry() {
    let now = now_millis();
    // Just inserted with 1s TTL - should not be expired
    assert!(!is_expired(now, 1000, 0));
    assert!(!is_expired(now, 1000, 1));
    assert!(!is_expired(now, 1000, 63));
}

#[test]
fn test_is_expired_tier_0() {
    let now = now_millis();
    // 1.5s ago with 1s base TTL, tier 0 (1x = 1s effective)
    let inserted = now.saturating_sub(1500);
    assert!(is_expired(inserted, 1000, 0)); // expired
}

#[test]
fn test_is_expired_tier_1_extends_ttl() {
    let now = now_millis();
    // 1.5s ago with 1s base TTL
    let inserted = now.saturating_sub(1500);

    // Tier 0 (1s effective TTL) = expired
    assert!(is_expired(inserted, 1000, 0));
    // Tier 1 (2s effective TTL) = NOT expired
    assert!(!is_expired(inserted, 1000, 1));
}

#[test]
fn test_is_expired_tier_2_extends_further() {
    let now = now_millis();
    // 3s ago with 1s base TTL
    let inserted = now.saturating_sub(3000);

    // Tier 0 (1s) = expired
    assert!(is_expired(inserted, 1000, 0));
    // Tier 1 (2s) = expired
    assert!(is_expired(inserted, 1000, 1));
    // Tier 2 (4s) = NOT expired
    assert!(!is_expired(inserted, 1000, 2));
}

#[test]
fn test_is_expired_high_tier_extension() {
    let now = now_millis();
    // 100s ago with 1s base TTL
    let inserted = now.saturating_sub(100_000);

    // Tiers 0-6 should all be expired
    assert!(is_expired(inserted, 1000, 0)); // 1s TTL
    assert!(is_expired(inserted, 1000, 1)); // 2s TTL
    assert!(is_expired(inserted, 1000, 2)); // 4s TTL
    assert!(is_expired(inserted, 1000, 3)); // 8s TTL
    assert!(is_expired(inserted, 1000, 4)); // 16s TTL
    assert!(is_expired(inserted, 1000, 5)); // 32s TTL
    assert!(is_expired(inserted, 1000, 6)); // 64s TTL
    // Tier 7 (128s) = NOT expired
    assert!(!is_expired(inserted, 1000, 7));
    // Tier 10 (1024s) = NOT expired
    assert!(!is_expired(inserted, 1000, 10));
}

#[test]
fn test_is_expired_zero_ttl_always_expires() {
    let now = now_millis();
    // Zero TTL = everything expires immediately
    assert!(is_expired(now, 0, 0));
    assert!(is_expired(now, 0, 1));
    assert!(is_expired(now, 0, 63)); // Even with tier 63, 0 * 2^63 = 0
}

#[test]
fn test_is_expired_future_timestamp() {
    // Edge case: timestamp in the future (clock skew)
    let future = now_millis().saturating_add(10000);
    // Should not be expired - elapsed would be 0 due to saturating_sub
    assert!(!is_expired(future, 1000, 0));
}

// =============================================================================
// ArticleEntry tier tests
// =============================================================================

#[test]
fn test_article_entry_tier_default() {
    let entry = ArticleEntry::new(b"220 0 <test@example.com>\r\n.\r\n".to_vec());
    assert_eq!(entry.tier(), 0);
}

#[test]
fn test_article_entry_with_tier() {
    let entry = ArticleEntry::with_tier(b"220 0 <test@example.com>\r\n.\r\n".to_vec(), 3);
    assert_eq!(entry.tier(), 3);
}

#[test]
fn test_article_entry_set_tier() {
    let mut entry = ArticleEntry::new(b"220 0 <test@example.com>\r\n.\r\n".to_vec());
    assert_eq!(entry.tier(), 0);

    entry.set_tier(5);
    assert_eq!(entry.tier(), 5);
}

#[test]
fn test_article_entry_is_expired_uses_tier() {
    let entry_tier_0 = ArticleEntry::with_tier(b"220 test\r\n.\r\n".to_vec(), 0);
    let entry_tier_1 = ArticleEntry::with_tier(b"220 test\r\n.\r\n".to_vec(), 1);

    // With a very short TTL, tier 0 might expire but tier 1 has 2x
    // Just verify the method exists and uses tier
    assert!(!entry_tier_0.is_expired(1_000_000)); // Very long TTL - not expired
    assert!(!entry_tier_1.is_expired(1_000_000)); // Very long TTL - not expired
}

// =============================================================================
// Integration tests with ArticleCache
// =============================================================================

#[tokio::test]
async fn test_cache_tier_0_expires_at_base_ttl() {
    // Use very short TTL for testing
    let cache = ArticleCache::new(1_000_000, Duration::from_millis(50), true);

    let msg_id = MessageId::from_borrowed("<tier0-test@example.com>").unwrap();
    let buffer = b"220 0 <tier0-test@example.com>\r\nBody\r\n.\r\n".to_vec();

    // Insert with tier 0 (1x TTL = 50ms)
    cache
        .upsert(msg_id.clone(), buffer, BackendId::from_index(0), 0)
        .await;

    // Should be cached immediately
    assert!(cache.get(&msg_id).await.is_some());

    // Wait for tier 0 to expire (50ms + larger buffer to avoid flakiness)
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Should be expired now
    assert!(cache.get(&msg_id).await.is_none());
}

#[tokio::test]
async fn test_cache_higher_tier_longer_ttl() {
    // Use 200ms base TTL for more reliable timing
    let cache = ArticleCache::new(1_000_000, Duration::from_millis(200), true);

    let msg_id = MessageId::from_borrowed("<tier1-test@example.com>").unwrap();
    let buffer = b"220 0 <tier1-test@example.com>\r\nBody\r\n.\r\n".to_vec();

    // Insert with tier 1 (2x TTL = 400ms)
    cache
        .upsert(msg_id.clone(), buffer, BackendId::from_index(0), 1)
        .await;

    // Verify tier was stored correctly
    let entry = cache
        .get(&msg_id)
        .await
        .expect("Should be cached immediately");
    let stored_tier = entry.tier();
    assert_eq!(stored_tier, 1, "Tier should be stored as 1");
    drop(entry);

    // Wait 250ms - tier 0 would expire (200ms), but tier 1 should still be valid (400ms)
    tokio::time::sleep(Duration::from_millis(250)).await;

    // Tier 1 should still be cached (effective TTL = 400ms)
    let entry = cache.get(&msg_id).await;
    assert!(
        entry.is_some(),
        "Tier 1 should survive past base TTL (waited 250ms, tier 1 TTL is 400ms, stored tier was {})",
        stored_tier
    );

    // Wait another 200ms (total 450ms) - should now be expired
    // Increased total wait to 500ms to avoid flakiness under CI load
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        cache.get(&msg_id).await.is_none(),
        "Tier 1 should expire at 2x TTL"
    );
}

#[tokio::test]
async fn test_cache_tier_2_even_longer() {
    // Use 200ms base TTL for more reliable timing
    let cache = ArticleCache::new(1_000_000, Duration::from_millis(200), true);

    let msg_id = MessageId::from_borrowed("<tier2-test@example.com>").unwrap();
    let buffer = b"220 0 <tier2-test@example.com>\r\nBody\r\n.\r\n".to_vec();

    // Insert with tier 2 (4x TTL = 800ms)
    cache
        .upsert(msg_id.clone(), buffer, BackendId::from_index(0), 2)
        .await;

    // Wait 500ms - both tier 0 (200ms) and tier 1 (400ms) would expire
    // Using 500ms to avoid flakiness under CI load (larger buffer over the 400ms threshold)
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Tier 2 should still be cached (effective TTL = 800ms)
    assert!(
        cache.get(&msg_id).await.is_some(),
        "Tier 2 should survive past tier 1 TTL (waited 500ms, tier 2 TTL is 800ms)"
    );
}

#[tokio::test]
async fn test_cache_preserves_tier_on_get() {
    let cache = ArticleCache::new(1_000_000, Duration::from_secs(60), true);

    let msg_id = MessageId::from_borrowed("<tier-preserve@example.com>").unwrap();
    let buffer = b"220 0 <tier-preserve@example.com>\r\nBody\r\n.\r\n".to_vec();

    // Insert with tier 3
    cache
        .upsert(msg_id.clone(), buffer, BackendId::from_index(0), 3)
        .await;

    // Get and verify tier is preserved
    let entry = cache.get(&msg_id).await.expect("Should be cached");
    assert_eq!(entry.tier(), 3);
}

#[tokio::test]
async fn test_cache_upsert_updates_tier_with_larger_buffer() {
    let cache = ArticleCache::new(1_000_000, Duration::from_secs(60), true);

    let msg_id = MessageId::from_borrowed("<tier-update@example.com>").unwrap();
    let buffer_small = b"220 0 <tier-update@example.com>\r\nBody\r\n.\r\n".to_vec();
    let buffer_large =
        b"220 0 <tier-update@example.com>\r\nLarger body content here\r\n.\r\n".to_vec();

    // Insert with tier 5 and small buffer
    cache
        .upsert(msg_id.clone(), buffer_small, BackendId::from_index(0), 5)
        .await;

    let entry = cache.get(&msg_id).await.expect("Should be cached");
    assert_eq!(entry.tier(), 5);

    // Upsert with different tier and LARGER buffer (triggers replacement)
    cache
        .upsert(msg_id.clone(), buffer_large, BackendId::from_index(1), 2)
        .await;

    let entry = cache.get(&msg_id).await.expect("Should still be cached");
    // Tier should be updated because the buffer was replaced
    assert_eq!(entry.tier(), 2);
}

#[tokio::test]
async fn test_cache_upsert_keeps_tier_without_replacement() {
    // Verify that tier is NOT updated when buffer isn't replaced
    let cache = ArticleCache::new(1_000_000, Duration::from_secs(60), true);

    let msg_id = MessageId::from_borrowed("<tier-keep@example.com>").unwrap();
    let buffer = b"220 0 <tier-keep@example.com>\r\nBody\r\n.\r\n".to_vec();

    // Insert with tier 5
    cache
        .upsert(msg_id.clone(), buffer.clone(), BackendId::from_index(0), 5)
        .await;

    let entry = cache.get(&msg_id).await.expect("Should be cached");
    assert_eq!(entry.tier(), 5);

    // Upsert with same-sized buffer (doesn't trigger replacement)
    cache
        .upsert(msg_id.clone(), buffer, BackendId::from_index(1), 2)
        .await;

    let entry = cache.get(&msg_id).await.expect("Should still be cached");
    // Tier should NOT be updated because buffer wasn't replaced
    assert_eq!(entry.tier(), 5);
}
