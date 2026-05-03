//! Tests for tier-aware cache TTL expiration
//!
//! Verifies that higher tier articles get longer TTLs:
//! - Tier 0: 1x base TTL
//! - Tier 1: 2x base TTL
//! - Tier 2: 4x base TTL
//! - Tier 10: 1024x base TTL (~43 days with 1h base)
//! - etc. (capped at tier 63 to prevent shift overflow)

use nntp_proxy::cache::ArticleCache;
use nntp_proxy::cache::ttl::{
    CacheTier, CacheTimestampMillis, CacheTtlMillis, MAX_TTL_TIER, effective_ttl, is_expired,
    now_millis, ttl_multiplier,
};
use nntp_proxy::types::{BackendId, MessageId};
use std::time::Duration;

fn assert_multiplier_cases(cases: &[(u8, u64)]) {
    cases.iter().for_each(|(tier, expected)| {
        assert_eq!(
            ttl_multiplier(CacheTier::new(*tier)),
            *expected,
            "tier {tier}"
        );
    });
}

fn assert_effective_ttl_cases(base_ttl: u64, cases: &[(u8, u64)]) {
    cases.iter().for_each(|(tier, expected)| {
        assert_eq!(
            effective_ttl_ms(base_ttl, CacheTier::new(*tier)),
            *expected,
            "tier {tier}"
        );
    });
}

const fn effective_ttl_ms(base_ttl: u64, tier: CacheTier) -> u64 {
    effective_ttl(CacheTtlMillis::new(base_ttl), tier).get()
}

fn is_expired_ms(inserted_at_millis: u64, base_ttl_millis: u64, tier: CacheTier) -> bool {
    is_expired(
        CacheTimestampMillis::new(inserted_at_millis),
        CacheTtlMillis::new(base_ttl_millis),
        tier,
    )
}

fn article_bytes(message_id: &str, body: &str) -> Vec<u8> {
    format!("220 0 {message_id}\r\n{body}\r\n.\r\n").into_bytes()
}

async fn upsert_tier(cache: &ArticleCache, message_id: MessageId<'_>, tier: u8, body: &str) {
    cache
        .upsert_ingest(
            message_id.clone(),
            article_bytes(message_id.as_str(), body),
            BackendId::from_index(0),
            tier.into(),
        )
        .await;
}

async fn cached_tier(cache: &ArticleCache, message_id: &MessageId<'_>) -> u8 {
    cache
        .get(message_id)
        .await
        .expect("Should be cached")
        .tier()
        .get()
}

// =============================================================================
// Unit tests for TTL calculations
// =============================================================================

#[test]
fn test_tier_multiplier_calculation() {
    assert_multiplier_cases(&[
        (0, 1),
        (1, 2),
        (2, 4),
        (3, 8),
        (7, 128),
        (10, 1024),
        (63, 1u64 << 63),
        (64, 1u64 << 63),
        (255, 1u64 << 63),
    ]);
}

#[test]
fn test_effective_ttl_formula() {
    assert_effective_ttl_cases(
        1000,
        &[
            (0, 1000),
            (1, 2000),
            (2, 4000),
            (3, 8000),
            (4, 16_000),
            (5, 32_000),
            (6, 64_000),
            (7, 128_000),
            (10, 1_024_000),
        ],
    );
}

#[test]
fn test_effective_ttl_caps_at_tier_63() {
    // Use base_ttl of 1 to avoid overflow in result comparison
    let tier_63_result = effective_ttl_ms(1, CacheTier::new(63));
    assert_eq!(tier_63_result, 1u64 << 63);

    assert_effective_ttl_cases(
        1,
        &[
            (64, tier_63_result),
            (100, tier_63_result),
            (255, tier_63_result),
        ],
    );
}

#[test]
fn test_overflow_protection() {
    // Very large base TTL with high tier shouldn't overflow
    assert_eq!(effective_ttl_ms(u64::MAX, CacheTier::new(0)), u64::MAX);
    assert_eq!(effective_ttl_ms(u64::MAX, CacheTier::new(1)), u64::MAX); // saturates
    assert_eq!(effective_ttl_ms(u64::MAX, CacheTier::new(63)), u64::MAX); // saturates

    // Large value that would overflow without saturation
    // u64::MAX / 2 + 1 times 2 would overflow
    let result = effective_ttl_ms(u64::MAX / 2 + 1, CacheTier::new(1));
    assert_eq!(result, u64::MAX); // Saturates to MAX
}

#[test]
fn test_zero_ttl_stays_zero() {
    assert_effective_ttl_cases(0, &[(0, 0), (1, 0), (63, 0), (255, 0)]);
}

#[test]
fn test_max_ttl_tier_constant() {
    assert_eq!(MAX_TTL_TIER, 63);
    assert_eq!(ttl_multiplier(CacheTier::new(MAX_TTL_TIER)), 1u64 << 63);
}

// =============================================================================
// is_expired tests
// =============================================================================

#[test]
fn test_is_expired_fresh_entry() {
    let now = now_millis();
    // Just inserted with 1s TTL - should not be expired
    [0, 1, 63].into_iter().for_each(|tier| {
        assert!(!is_expired_ms(now, 1000, CacheTier::new(tier)));
    });
}

#[test]
fn test_is_expired_tier_0() {
    let now = now_millis();
    // 1.5s ago with 1s base TTL, tier 0 (1x = 1s effective)
    let inserted = now.saturating_sub(1500);
    assert!(is_expired_ms(inserted, 1000, CacheTier::new(0))); // expired
}

#[test]
fn test_is_expired_tier_1_extends_ttl() {
    let now = now_millis();
    // 1.5s ago with 1s base TTL
    let inserted = now.saturating_sub(1500);

    // Tier 0 (1s effective TTL) = expired
    assert!(is_expired_ms(inserted, 1000, CacheTier::new(0)));
    // Tier 1 (2s effective TTL) = NOT expired
    assert!(!is_expired_ms(inserted, 1000, CacheTier::new(1)));
}

#[test]
fn test_is_expired_tier_2_extends_further() {
    let now = now_millis();
    // 3s ago with 1s base TTL
    let inserted = now.saturating_sub(3000);

    // Tier 0 (1s) = expired
    assert!(is_expired_ms(inserted, 1000, CacheTier::new(0)));
    // Tier 1 (2s) = expired
    assert!(is_expired_ms(inserted, 1000, CacheTier::new(1)));
    // Tier 2 (4s) = NOT expired
    assert!(!is_expired_ms(inserted, 1000, CacheTier::new(2)));
}

#[test]
fn test_is_expired_high_tier_extension() {
    let now = now_millis();
    // 100s ago with 1s base TTL
    let inserted = now.saturating_sub(100_000);

    (0..=6).for_each(|tier| assert!(is_expired_ms(inserted, 1000, CacheTier::new(tier))));
    [7, 10].into_iter().for_each(|tier| {
        assert!(!is_expired_ms(inserted, 1000, CacheTier::new(tier)));
    });
}

#[test]
fn test_is_expired_zero_ttl_always_expires() {
    let now = now_millis();
    [0, 1, 63].into_iter().for_each(|tier| {
        assert!(is_expired_ms(now, 0, CacheTier::new(tier)));
    });
}

#[test]
fn test_is_expired_future_timestamp() {
    // Edge case: timestamp in the future (clock skew)
    let future = now_millis().saturating_add(10000);
    // Should not be expired - elapsed would be 0 due to saturating_sub
    assert!(!is_expired_ms(future, 1000, CacheTier::new(0)));
}

// =============================================================================
// Integration tests with ArticleCache
// =============================================================================

#[tokio::test]
async fn test_cache_tier_0_expires_at_base_ttl() {
    // Use very short TTL for testing
    let cache = ArticleCache::new(1_000_000, Duration::from_millis(50), true);

    let msg_id = MessageId::from_borrowed("<tier0-test@example.com>").unwrap();

    // Insert with tier 0 (1x TTL = 50ms)
    upsert_tier(&cache, msg_id.clone(), 0, "Body").await;

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

    // Insert with tier 1 (2x TTL = 400ms)
    upsert_tier(&cache, msg_id.clone(), 1, "Body").await;

    // Verify tier was stored correctly
    let stored_tier = cached_tier(&cache, &msg_id).await;
    assert_eq!(stored_tier, 1, "Tier should be stored as 1");

    // Wait 250ms - tier 0 would expire (200ms), but tier 1 should still be valid (400ms)
    tokio::time::sleep(Duration::from_millis(250)).await;

    // Tier 1 should still be cached (effective TTL = 400ms)
    let entry = cache.get(&msg_id).await;
    assert!(
        entry.is_some(),
        "Tier 1 should survive past base TTL (waited 250ms, tier 1 TTL is 400ms, stored tier was {stored_tier})"
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

    // Insert with tier 2 (4x TTL = 800ms)
    upsert_tier(&cache, msg_id.clone(), 2, "Body").await;

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
    let cache = ArticleCache::new(1_000_000, Duration::from_mins(1), true);

    let msg_id = MessageId::from_borrowed("<tier-preserve@example.com>").unwrap();

    // Insert with tier 3
    upsert_tier(&cache, msg_id.clone(), 3, "Body").await;

    // Get and verify tier is preserved
    assert_eq!(cached_tier(&cache, &msg_id).await, 3);
}

#[tokio::test]
async fn test_cache_upsert_updates_tier_with_larger_buffer() {
    let cache = ArticleCache::new(1_000_000, Duration::from_mins(1), true);

    let msg_id = MessageId::from_borrowed("<tier-update@example.com>").unwrap();
    let buffer_large = article_bytes(msg_id.as_str(), "Larger body content here");

    // Insert with tier 5 and small buffer
    upsert_tier(&cache, msg_id.clone(), 5, "Body").await;

    assert_eq!(cached_tier(&cache, &msg_id).await, 5);

    // Upsert with different tier and LARGER buffer (triggers replacement)
    cache
        .upsert_ingest(
            msg_id.clone(),
            buffer_large,
            BackendId::from_index(1),
            2.into(),
        )
        .await;

    // Tier should be updated because the buffer was replaced
    assert_eq!(cached_tier(&cache, &msg_id).await, 2);
}

#[tokio::test]
async fn test_cache_upsert_keeps_tier_without_replacement() {
    // Verify that tier is NOT updated when buffer isn't replaced
    let cache = ArticleCache::new(1_000_000, Duration::from_mins(1), true);

    let msg_id = MessageId::from_borrowed("<tier-keep@example.com>").unwrap();
    let buffer = article_bytes(msg_id.as_str(), "Body");

    // Insert with tier 5
    cache
        .upsert_ingest(
            msg_id.clone(),
            buffer.clone(),
            BackendId::from_index(0),
            5.into(),
        )
        .await;

    assert_eq!(cached_tier(&cache, &msg_id).await, 5);

    // Upsert with same-sized buffer (doesn't trigger replacement)
    cache
        .upsert_ingest(msg_id.clone(), buffer, BackendId::from_index(1), 2.into())
        .await;

    // Tier should NOT be updated because buffer wasn't replaced
    assert_eq!(cached_tier(&cache, &msg_id).await, 5);
}
