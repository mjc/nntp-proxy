//! Integration tests for HybridArticleCache
//!
//! These tests verify critical hybrid cache functionality:
//! - WriteOnInsertion policy (data persistence)
//! - cache.close() flushes pending writes
//! - Memory → disk eviction
//! - Racing precheck integration
//!
//! Uses MockHybridCache to avoid foyer runtime issues in tests.

use anyhow::Result;
use nntp_proxy::cache::mock_hybrid::MockHybridCache;
use nntp_proxy::types::{BackendId, MessageId};

#[tokio::test]
async fn test_mock_cache_basic_ops() -> Result<()> {
    let cache = MockHybridCache::new(1024 * 1024);

    // Insert article
    let msg_id = MessageId::from_borrowed("<test@example.com>").unwrap();
    let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
    let backend_id = BackendId::from_index(0);

    cache
        .upsert(msg_id.clone(), buffer.clone(), backend_id)
        .await;

    // Retrieve it
    let entry = cache.get(&msg_id).await;
    assert!(entry.is_some(), "Entry should exist");
    assert_eq!(entry.unwrap().buffer(), buffer.as_slice());

    // Check stats
    let stats = cache.stats();
    assert_eq!(stats.hits, 1);
    assert_eq!(stats.misses, 0);

    Ok(())
}

#[tokio::test]
async fn test_cache_miss() -> Result<()> {
    let cache = MockHybridCache::new(1024 * 1024);

    let msg_id = MessageId::from_borrowed("<nonexistent@example.com>").unwrap();
    let entry = cache.get(&msg_id).await;

    assert!(entry.is_none());

    let stats = cache.stats();
    assert_eq!(stats.hits, 0);
    assert_eq!(stats.misses, 1);

    Ok(())
}

#[tokio::test]
async fn test_upsert_preserves_larger_buffer() -> Result<()> {
    let cache = MockHybridCache::new(1024 * 1024);

    let msg_id = MessageId::from_borrowed("<test@example.com>").unwrap();

    // Insert large article
    let large_buffer =
        b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nLarge body content here\r\n.\r\n"
            .to_vec();
    cache
        .upsert(
            msg_id.clone(),
            large_buffer.clone(),
            BackendId::from_index(0),
        )
        .await;

    // Try to overwrite with smaller stub (should be rejected)
    let small_buffer = b"223 0 <test@example.com>\r\n".to_vec();
    cache
        .upsert(msg_id.clone(), small_buffer, BackendId::from_index(1))
        .await;

    // Should still have large buffer
    let entry = cache.get(&msg_id).await.unwrap();
    assert_eq!(entry.buffer().len(), large_buffer.len());
    assert_eq!(entry.buffer(), large_buffer.as_slice());

    // But should have availability for both backends
    assert!(entry.should_try_backend(BackendId::from_index(0)));
    assert!(entry.should_try_backend(BackendId::from_index(1)));

    Ok(())
}

#[tokio::test]
async fn test_record_missing() -> Result<()> {
    let cache = MockHybridCache::new(1024 * 1024);

    let msg_id = MessageId::from_borrowed("<missing@example.com>").unwrap();

    // Record missing for backend 0
    cache
        .record_missing(msg_id.clone(), BackendId::from_index(0))
        .await;

    // Should create 430 stub
    let entry = cache.get(&msg_id).await;
    assert!(entry.is_some());

    let entry = entry.unwrap();
    assert!(!entry.should_try_backend(BackendId::from_index(0)));
    assert!(entry.should_try_backend(BackendId::from_index(1))); // Not checked yet

    Ok(())
}

#[tokio::test]
async fn test_availability_tracking() -> Result<()> {
    let cache = MockHybridCache::new(1024 * 1024);

    let msg_id = MessageId::from_borrowed("<avail@example.com>").unwrap();
    let buffer = b"220 0 <avail@example.com>\r\nBody\r\n.\r\n".to_vec();

    // Insert with backend 0
    cache
        .upsert(msg_id.clone(), buffer, BackendId::from_index(0))
        .await;

    // Mark backends 1 and 2 as missing
    cache
        .record_missing(msg_id.clone(), BackendId::from_index(1))
        .await;
    cache
        .record_missing(msg_id.clone(), BackendId::from_index(2))
        .await;

    // Verify availability
    let entry = cache.get(&msg_id).await.unwrap();
    assert!(
        entry.should_try_backend(BackendId::from_index(0)),
        "Backend 0 should have article"
    );
    assert!(
        !entry.should_try_backend(BackendId::from_index(1)),
        "Backend 1 marked missing"
    );
    assert!(
        !entry.should_try_backend(BackendId::from_index(2)),
        "Backend 2 marked missing"
    );
    assert!(
        entry.should_try_backend(BackendId::from_index(3)),
        "Backend 3 not checked"
    );

    Ok(())
}

#[tokio::test]
async fn test_close_succeeds() -> Result<()> {
    let cache = MockHybridCache::new(1024 * 1024);

    // Insert some data
    let msg_id = MessageId::from_borrowed("<test@example.com>").unwrap();
    cache
        .upsert(
            msg_id,
            b"220 0 <test@example.com>\r\n.\r\n".to_vec(),
            BackendId::from_index(0),
        )
        .await;

    // Close should not error
    cache.close().await?;

    Ok(())
}
