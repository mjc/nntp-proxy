//! Integration tests for the critical 430 caching bug fix
//!
//! BUG: ArticleCache::record_backend_missing was silently doing nothing when
//! an article wasn't already cached. This caused:
//! 1. Missing articles to be queried from ALL backends EVERY request
//! 2. SABnzbd reporting "gigabytes of missing articles"
//! 3. 4xx error metrics not incrementing properly
//! 4. Only 5 cache entries instead of hundreds
//!
//! FIX: record_backend_missing now creates cache entries for 430 responses,
//! preventing repeated queries to backends that don't have the article.

use nntp_proxy::cache::ArticleCache;
use nntp_proxy::metrics::MetricsCollector;
use nntp_proxy::router::BackendCount;
use nntp_proxy::types::{BackendId, MessageId};
use std::time::Duration;

/// Test that 430 responses create cache entries (unit-level)
#[tokio::test]
async fn test_430_response_creates_cache_entry() {
    let cache = ArticleCache::new(1000, Duration::from_secs(300), true);

    let msgid = MessageId::from_borrowed("<missing@example.com>").unwrap();

    // Initially not cached
    assert!(cache.get(&msgid).await.is_none());

    // Backend 0 returns 430
    cache
        .record_backend_missing(msgid.clone(), BackendId::from_index(0))
        .await;

    // MUST create cache entry
    let entry = cache
        .get(&msgid)
        .await
        .expect("Cache entry must exist after 430");

    // Backend 0 should be marked missing
    assert!(!entry.should_try_backend(BackendId::from_index(0)));

    // Backend 1 should still be available
    assert!(entry.should_try_backend(BackendId::from_index(1)));
}

/// Test that subsequent 430s update the same cache entry
#[tokio::test]
async fn test_multiple_430s_update_same_entry() {
    let cache = ArticleCache::new(1000, Duration::from_secs(300), true);

    let msgid = MessageId::from_borrowed("<missing@example.com>").unwrap();

    // Backend 0 returns 430
    cache
        .record_backend_missing(msgid.clone(), BackendId::from_index(0))
        .await;

    // Backend 1 returns 430
    cache
        .record_backend_missing(msgid.clone(), BackendId::from_index(1))
        .await;

    let entry = cache.get(&msgid).await.unwrap();

    // Both backends should be marked missing
    assert!(!entry.should_try_backend(BackendId::from_index(0)));
    assert!(!entry.should_try_backend(BackendId::from_index(1)));

    // All backends exhausted
    assert!(
        entry.all_backends_exhausted(BackendCount::new(2)),
        "All backends should be exhausted"
    );
}

/// Test that 430 responses increment 4xx error metrics
#[tokio::test]
async fn test_430_increments_4xx_metrics() {
    let metrics = MetricsCollector::new(2); // 2 backends

    let backend_id = BackendId::from_index(0);

    // Record 430 error
    metrics.record_error_4xx(backend_id);
    metrics.record_error_4xx(backend_id);
    metrics.record_error_4xx(backend_id);

    let snapshot = metrics.snapshot(None);
    let backend_stats = &snapshot.backend_stats[0];

    // Verify 4xx count
    assert_eq!(
        backend_stats.errors_4xx.get(),
        3,
        "4xx error count should be 3"
    );

    // Verify total error count
    assert_eq!(
        backend_stats.errors.get(),
        3,
        "Total error count should be 3"
    );
}

/// Test cache growth with missing articles
#[tokio::test]
async fn test_cache_grows_with_430_responses() {
    // With MOKA_OVERHEAD=2000 and 2.5x multiplier for small entries:
    // Each 430 stub (~5 bytes) weighs approx (5 + 68 + 40 + 2000) * 2.5 ≈ 5280 bytes
    // So 100 entries need ~528KB capacity
    let cache = ArticleCache::new(1_000_000, Duration::from_secs(300), true);

    // Initial stats
    let stats = cache.stats().await;
    assert_eq!(stats.entry_count, 0);

    // Record 100 different missing articles
    // Keep the strings alive to prevent early eviction
    let mut msg_ids = Vec::new();
    for i in 0..100 {
        let msg_id_str = format!("<missing{}@example.com>", i);
        let msgid = MessageId::from_borrowed(&msg_id_str).unwrap();

        cache
            .record_backend_missing(msgid, BackendId::from_index(0))
            .await;
        msg_ids.push(msg_id_str);
    }

    // Wait for async inserts
    cache.sync().await;

    // Cache should have 100 entries now
    let stats = cache.stats().await;
    assert!(
        stats.entry_count >= 100,
        "Cache should have at least 100 entries, got {}",
        stats.entry_count
    );
}

/// Regression test: Verify bug symptoms are fixed
#[tokio::test]
async fn test_regression_bug_symptoms_fixed() {
    // With MOKA_OVERHEAD=2000 and 2.5x multiplier for small entries:
    // Each 430 stub (~5 bytes) weighs approx (5 + 68 + 40 + 2000) * 2.5 ≈ 5280 bytes
    // So 500 entries need ~2.64MB capacity
    let cache = ArticleCache::new(5_000_000, Duration::from_secs(300), true);
    let metrics = MetricsCollector::new(2);

    // Simulate SABnzbd requesting hundreds of missing articles
    // Keep strings alive
    let mut msg_ids = Vec::new();
    for i in 0..500 {
        let msg_id_str = format!("<missing{}@example.com>", i);
        let msgid = MessageId::from_borrowed(&msg_id_str).unwrap();

        // Both backends return 430
        for backend_idx in 0..2 {
            let backend_id = BackendId::from_index(backend_idx);
            cache
                .record_backend_missing(msgid.clone(), backend_id)
                .await;
            metrics.record_error_4xx(backend_id);
        }
        msg_ids.push(msg_id_str);
    }

    cache.sync().await;

    // BUG SYMPTOM 1: Cache should have hundreds of entries, not just 5
    let stats = cache.stats().await;
    assert!(
        stats.entry_count >= 500,
        "Cache should have 500+ entries (was 5 before fix), got {}",
        stats.entry_count
    );

    // BUG SYMPTOM 2: 4xx metrics should be non-zero
    let snapshot = metrics.snapshot(None);
    for (idx, backend_stats) in snapshot.backend_stats.iter().enumerate() {
        assert!(
            backend_stats.errors_4xx.get() > 0,
            "Backend {} 4xx errors should be counted (was 0 before fix)",
            idx
        );
        assert_eq!(
            backend_stats.errors_4xx.get(),
            500,
            "Each backend should have 500 4xx errors"
        );
    }

    // BUG SYMPTOM 3: Total errors should match 4xx errors
    for (idx, backend_stats) in snapshot.backend_stats.iter().enumerate() {
        assert_eq!(
            backend_stats.errors.get(),
            backend_stats.errors_4xx.get(),
            "Backend {} total errors should equal 4xx errors for this test",
            idx
        );
    }
}
