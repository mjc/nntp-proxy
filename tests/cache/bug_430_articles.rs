//! Test for the `cache_articles=true` 430 bug
//!
//! BUG: When `cache_articles=true`, the proxy returns 430 for articles that backends
//! actually have. This is caused by a race condition:
//!
//! 1. `spawn_cache_upsert()` is fire-and-forget (async)
//! 2. `sync_availability()` runs immediately after success
//! 3. If upsert hasn't completed, `sync_availability` creates a missing entry
//! 4. The missing entry overwrites the real article when upsert finally runs
//!    OR the missing entry is served on the next request before upsert completes
//!
//! This test verifies the fix: `sync_availability` should not create missing entries
//! when the availability shows a backend HAS the article.

use anyhow::Result;
use nntp_proxy::cache::{ArticleAvailability, ArticleCache};
use nntp_proxy::protocol::RequestKind;
use nntp_proxy::router::BackendCount;
use nntp_proxy::types::{BackendId, MessageId};
use std::time::Duration;

use super::article_response_bytes;

/// Test that `sync_availability` does NOT create a missing entry when a backend has the article
#[tokio::test]
async fn test_sync_availability_does_not_create_430_metadata_only_when_backend_has_article()
-> Result<()> {
    let cache = ArticleCache::new(1_000_000, Duration::from_secs(300), true);
    let msg_id = MessageId::new("<test@example.com>".to_string())?;

    // Simulate the scenario: backend 0 has the article
    let mut availability = ArticleAvailability::new();
    availability.record_has(BackendId::from_index(0));

    // Call sync_availability when NO cache entry exists yet
    // (simulating the race where upsert hasn't completed)
    cache.sync_availability(msg_id.clone(), &availability).await;

    // Check what got cached
    let cached = cache.get(&msg_id).await;

    // BUG: sync_availability creates a "430\r\n" metadata-only response even when availability shows
    // backend 0 HAS the article. This is wrong - if a backend has it, we should
    // either not create an entry at all, or create a proper placeholder.

    // The cached entry should NOT be a 430 response
    if let Some(entry) = cached {
        let status = entry.status_code();
        assert!(
            status.as_u16() != 430,
            "sync_availability should NOT create missing entry when backend has article! \
             Got status code: {:?}",
            status
        );
    }
    // If no entry was created, that's also acceptable

    Ok(())
}

/// Test the full race condition scenario
#[tokio::test]
async fn test_race_condition_upsert_vs_sync_availability() -> Result<()> {
    let cache = ArticleCache::new(1_000_000, Duration::from_secs(300), true);
    let msg_id = MessageId::new("<race@example.com>".to_string())?;

    // Simulate the race: sync_availability runs BEFORE upsert
    let mut availability = ArticleAvailability::new();
    availability.record_has(BackendId::from_index(0));

    // sync_availability runs first (no entry yet)
    cache.sync_availability(msg_id.clone(), &availability).await;

    // Now upsert runs with the actual article
    let article_data = b"220 0 <race@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
    cache
        .upsert_backend_response(
            msg_id.clone(),
            article_data.clone(),
            BackendId::from_index(0),
            0.into(),
        )
        .await;

    // The cached entry should be the article, not a missing entry
    let cached = cache.get(&msg_id).await.expect("Should have cache entry");
    let status = cached.status_code();

    assert_eq!(
        status.as_u16(),
        220,
        "After upsert, cache should have article (220), not missing entry. Got: {}",
        status.as_u16()
    );

    Ok(())
}

/// Test that repeated requests don't get 430 due to corrupted cache
#[tokio::test]
async fn test_second_request_gets_article_not_430() -> Result<()> {
    let cache = ArticleCache::new(1_000_000, Duration::from_secs(300), true);
    let msg_id = MessageId::new("<second@example.com>".to_string())?;

    // First request: upsert the article
    let article_data = b"220 0 <second@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
    cache
        .upsert_backend_response(
            msg_id.clone(),
            article_data.clone(),
            BackendId::from_index(0),
            0.into(),
        )
        .await;

    // Simulate sync_availability being called after (shouldn't corrupt)
    let mut availability = ArticleAvailability::new();
    availability.record_has(BackendId::from_index(0));
    cache.sync_availability(msg_id.clone(), &availability).await;

    // Second request: should get the article
    let cached = cache.get(&msg_id).await.expect("Should have cache entry");
    let status = cached.status_code();

    assert_eq!(
        status.as_u16(),
        220,
        "Cache should preserve article (220) after sync_availability. Got: {}",
        status.as_u16()
    );

    let rendered = article_response_bytes(&cached, RequestKind::Article, &msg_id).unwrap();
    assert!(rendered.starts_with(b"220"));

    Ok(())
}

// ============================================================================
// Additional thorough tests for ArticleAvailability
// ============================================================================

/// Test the new `any_backend_has_article()` method
#[test]
fn test_any_backend_has_article_basic() {
    let mut availability = ArticleAvailability::new();

    // Initially no backend has it (nothing checked)
    assert!(
        !availability.any_backend_has_article(),
        "Empty availability should not have any backend with article"
    );

    // Record backend 0 as missing (430)
    availability.record_missing(BackendId::from_index(0));
    assert!(
        !availability.any_backend_has_article(),
        "After recording missing, should still not have any backend with article"
    );

    // Record backend 1 as having it
    availability.record_has(BackendId::from_index(1));
    assert!(
        availability.any_backend_has_article(),
        "After recording has, should have a backend with article"
    );
}

/// Test `any_backend_has_article` with multiple backends
#[test]
fn test_any_backend_has_article_multiple_backends() {
    let mut availability = ArticleAvailability::new();

    // Mark backends 0, 1, 2 as missing
    availability.record_missing(BackendId::from_index(0));
    availability.record_missing(BackendId::from_index(1));
    availability.record_missing(BackendId::from_index(2));

    assert!(
        !availability.any_backend_has_article(),
        "All backends missing should return false"
    );

    // Mark backend 3 as having it
    availability.record_has(BackendId::from_index(3));
    assert!(
        availability.any_backend_has_article(),
        "One backend has it should return true"
    );
}

/// Test that recording has clears missing bit
#[test]
fn test_record_has_clears_missing() {
    let mut availability = ArticleAvailability::new();

    // First record as missing
    availability.record_missing(BackendId::from_index(0));
    assert!(availability.is_missing(BackendId::from_index(0)));
    assert!(!availability.any_backend_has_article());

    // Now record as has - should clear the missing bit
    availability.record_has(BackendId::from_index(0));
    assert!(!availability.is_missing(BackendId::from_index(0)));
    assert!(availability.any_backend_has_article());
}

/// Test `sync_availability` with mixed availability (some have, some missing)
#[tokio::test]
async fn test_sync_availability_mixed_results() -> Result<()> {
    let cache = ArticleCache::new(1_000_000, Duration::from_secs(300), true);
    let msg_id = MessageId::new("<mixed@example.com>".to_string())?;

    // Simulate: backend 0 returned 430, backend 1 has it
    let mut availability = ArticleAvailability::new();
    availability.record_missing(BackendId::from_index(0));
    availability.record_has(BackendId::from_index(1));

    // sync_availability should NOT create missing entry because backend 1 has it
    cache.sync_availability(msg_id.clone(), &availability).await;

    let cached = cache.get(&msg_id).await;
    if let Some(entry) = cached {
        assert!(
            entry.status_code().as_u16() != 430,
            "Should not create missing entry when one backend has article"
        );
    }
    // No entry is also acceptable

    Ok(())
}

/// Test `sync_availability` creates missing entry only when ALL backends are missing
#[tokio::test]
async fn test_sync_availability_creates_430_when_all_missing() -> Result<()> {
    let cache = ArticleCache::new(1_000_000, Duration::from_secs(300), true);
    let msg_id = MessageId::new("<allmissing@example.com>".to_string())?;

    // All backends returned 430
    let mut availability = ArticleAvailability::new();
    availability.record_missing(BackendId::from_index(0));
    availability.record_missing(BackendId::from_index(1));

    // Should create missing entry
    cache.sync_availability(msg_id.clone(), &availability).await;

    let cached = cache.get(&msg_id).await.expect("Should have cache entry");
    assert_eq!(
        cached.status_code().as_u16(),
        430,
        "Should create missing entry when all backends are missing"
    );

    // Verify availability is preserved
    assert!(
        !cached.should_try_backend(BackendId::from_index(0)),
        "Backend 0 should be marked as missing"
    );
    assert!(
        !cached.should_try_backend(BackendId::from_index(1)),
        "Backend 1 should be marked as missing"
    );

    Ok(())
}

/// Test that `all_exhausted` works correctly
#[test]
fn test_all_exhausted_with_backend_count() {
    let mut availability = ArticleAvailability::new();

    // With 2 backends, not exhausted yet
    assert!(!availability.all_exhausted(BackendCount::new(2)));

    // Mark backend 0 as missing
    availability.record_missing(BackendId::from_index(0));
    assert!(!availability.all_exhausted(BackendCount::new(2)));

    // Mark backend 1 as missing - now exhausted
    availability.record_missing(BackendId::from_index(1));
    assert!(availability.all_exhausted(BackendCount::new(2)));

    // But not exhausted if we have 3 backends
    assert!(!availability.all_exhausted(BackendCount::new(3)));
}

/// Test that `try_serve_from_cache` doesn't serve missing entries when `cache_articles=true`
///
/// This is a critical edge case: if a missing entry is in the cache, we should NOT
/// serve it directly - we should fall through to backend routing.
#[tokio::test]
async fn test_430_metadata_only_not_served_directly() -> Result<()> {
    let cache = ArticleCache::new(1_000_000, Duration::from_secs(300), true);
    let msg_id = MessageId::new("<metadata_only@example.com>".to_string())?;

    // Create a missing entry in the cache (simulating all backends returned 430 previously)
    let mut availability = ArticleAvailability::new();
    availability.record_missing(BackendId::from_index(0));
    availability.record_missing(BackendId::from_index(1));
    cache.sync_availability(msg_id.clone(), &availability).await;

    // Verify a missing entry is in cache
    let cached = cache.get(&msg_id).await.expect("Should have missing entry");
    assert_eq!(cached.status_code().as_u16(), 430);

    // When we have a missing entry, the availability info shows all backends exhausted
    // The session handler should check all_exhausted and send 430 only if true
    // (This test documents the expected behavior - actual implementation is in session handler)
    assert!(cached.all_backends_exhausted(BackendCount::new(2)));

    Ok(())
}

/// Test concurrent upsert and `sync_availability` (simulating the race)
#[tokio::test]
async fn test_concurrent_upsert_and_sync() -> Result<()> {
    use std::sync::Arc;
    use tokio::sync::Barrier;

    let cache = Arc::new(ArticleCache::new(1_000_000, Duration::from_secs(300), true));
    let msg_id = MessageId::new("<concurrent@example.com>".to_string())?;

    // Use a barrier to synchronize the two operations
    let barrier = Arc::new(Barrier::new(2));

    let cache1 = cache.clone();
    let msg_id1 = msg_id.clone();
    let barrier1 = barrier.clone();

    // Task 1: upsert
    let upsert_task = tokio::spawn(async move {
        barrier1.wait().await;
        let article_data =
            b"220 0 <concurrent@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
        cache1
            .upsert_backend_response(msg_id1, article_data, BackendId::from_index(0), 0.into())
            .await;
    });

    let cache2 = cache.clone();
    let msg_id2 = msg_id.clone();
    let barrier2 = barrier.clone();

    // Task 2: sync_availability
    let sync_task = tokio::spawn(async move {
        barrier2.wait().await;
        let mut availability = ArticleAvailability::new();
        availability.record_has(BackendId::from_index(0));
        cache2.sync_availability(msg_id2, &availability).await;
    });

    // Wait for both to complete
    upsert_task.await?;
    sync_task.await?;

    // Regardless of order, we should NOT have a missing entry
    let article = cache.get(&msg_id).await;
    if let Some(entry) = article {
        assert!(
            entry.status_code().as_u16() != 430,
            "Concurrent operations should not result in missing entry when backend has article"
        );
    }

    Ok(())
}

/// Test that metadata-only responses don't get served when `cache_articles=true`
#[tokio::test]
async fn test_metadata_only_not_served_as_article() -> Result<()> {
    let cache = ArticleCache::new(1_000_000, Duration::from_secs(300), true);
    let msg_id = MessageId::new("<metadata_only-test@example.com>".to_string())?;

    // Simulate STAT precheck creating a metadata-only response
    let mut availability = ArticleAvailability::new();
    availability.record_has(BackendId::from_index(0));

    // First, put a 223 metadata-only response in the cache (as if from STAT precheck)
    // We'll use upsert with a metadata-only response-like buffer to simulate this
    cache
        .upsert_backend_response(
            msg_id.clone(),
            b"223\r\n".to_vec(),
            BackendId::from_index(0),
            0.into(),
        )
        .await;

    // Get the cached entry
    let cached = cache.get(&msg_id).await.expect("Should have entry");

    // Verify it's NOT a complete article (what is_complete_article should return)
    assert!(
        !cached.is_complete_article(),
        "STAT metadata-only response should not be identified as complete article"
    );

    // The session handler would check is_complete_article() and NOT serve this metadata-only response
    // Instead, it would fall through to fetch the real article from backend

    Ok(())
}

/// Test the full scenario: precheck creates a metadata-only response, then ARTICLE command needs real body
#[tokio::test]
async fn test_precheck_metadata_only_then_article_request() -> Result<()> {
    let cache = ArticleCache::new(1_000_000, Duration::from_secs(300), true);
    let msg_id = MessageId::new("<precheck-then-article@example.com>".to_string())?;

    // Step 1: STAT precheck creates a metadata-only response
    cache
        .upsert_backend_response(
            msg_id.clone(),
            b"223\r\n".to_vec(),
            BackendId::from_index(0),
            0.into(),
        )
        .await;

    // Verify a metadata-only response exists but is not a complete article
    let cached = cache
        .get(&msg_id)
        .await
        .expect("Should have metadata-only response");
    assert!(!cached.is_complete_article());
    assert!(cached.has_availability_info()); // Availability IS tracked

    // Step 2: ARTICLE command comes in - should NOT serve the metadata-only response
    // (In real code, is_complete_article() check causes fallthrough to backend fetch)

    // Step 3: Simulate backend fetch and upsert of real article
    let real_article =
        b"220 0 <precheck-then-article@example.com>\r\nSubject: Test\r\n\r\nReal body.\r\n.\r\n"
            .to_vec();
    cache
        .upsert_backend_response(
            msg_id.clone(),
            real_article.clone(),
            BackendId::from_index(0),
            0.into(),
        )
        .await;

    // Verify real article replaced metadata-only response
    let cached = cache.get(&msg_id).await.expect("Should have article");
    assert!(cached.is_complete_article());
    assert_eq!(cached.status_code().as_u16(), 220);
    assert!(
        article_response_bytes(&cached, RequestKind::Article, &msg_id)
            .unwrap()
            .ends_with(b".\r\n")
    );

    Ok(())
}
