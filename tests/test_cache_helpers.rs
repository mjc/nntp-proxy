//! Tests for cache helper functions and functional style refactoring
//!
//! Tests for:
//! - Cache path optimizations
//! - Cache hit/miss behavior
//! - Command filtering for cache
//! - TTL and capacity limits

use anyhow::Result;
use nntp_proxy::cache::ArticleCache;
use nntp_proxy::types::{BackendId, MessageId};
use std::sync::Arc;
use std::time::Duration;

/// Test cache hit serves cached content
#[tokio::test]
async fn test_cache_hit() -> Result<()> {
    let cache = Arc::new(ArticleCache::new(20000, Duration::from_secs(300), true));

    let msgid = MessageId::from_borrowed("<test@example.com>").unwrap();
    let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();

    // Insert
    cache
        .upsert(msgid.clone(), buffer.clone(), BackendId::from_index(0))
        .await;

    // Retrieve - should hit
    let retrieved = cache.get(&msgid).await;
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().buffer().as_ref(), &buffer);

    Ok(())
}

/// Test cache miss returns None
#[tokio::test]
async fn test_cache_miss() -> Result<()> {
    let cache = Arc::new(ArticleCache::new(20000, Duration::from_secs(300), true));

    let msgid = MessageId::from_borrowed("<nonexistent@example.com>").unwrap();
    let result = cache.get(&msgid).await;

    assert!(result.is_none());
    Ok(())
}

/// Test multiple message IDs can be cached
#[tokio::test]
async fn test_multiple_cache_entries() -> Result<()> {
    let cache = Arc::new(ArticleCache::new(20000, Duration::from_secs(300), true));

    for i in 1..=5 {
        let msg_str = format!("<test{}@example.com>", i);
        let msgid = MessageId::from_borrowed(&msg_str).unwrap();
        let buffer = format!("220 Body {}\r\n.\r\n", i).into_bytes();
        cache.upsert(msgid, buffer, BackendId::from_index(0)).await;
    }

    // Sync cache to ensure all inserts are processed
    cache.sync().await;

    let stats = cache.stats().await;
    assert_eq!(stats.entry_count, 5);

    // Verify each is retrievable
    for i in 1..=5 {
        let msg_str = format!("<test{}@example.com>", i);
        let msgid = MessageId::from_borrowed(&msg_str).unwrap();
        assert!(cache.get(&msgid).await.is_some());
    }

    Ok(())
}

/// Test cache respects TTL
#[tokio::test]
async fn test_cache_ttl_expiration() -> Result<()> {
    let cache = Arc::new(ArticleCache::new(20000, Duration::from_millis(100), true));

    let msgid = MessageId::from_borrowed("<test@example.com>").unwrap();
    let buffer = b"220 0 <test@example.com>\r\nBody\r\n.\r\n".to_vec();

    // Insert article
    cache
        .upsert(msgid.clone(), buffer, BackendId::from_index(0))
        .await;

    // Should be in cache immediately
    assert!(cache.get(&msgid).await.is_some());

    // Wait for TTL to expire
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Should be gone
    assert!(cache.get(&msgid).await.is_none());

    Ok(())
}

/// Test cache capacity limits (LRU eviction)
#[tokio::test]
async fn test_cache_capacity_limit() -> Result<()> {
    let cache = Arc::new(ArticleCache::new(2, Duration::from_secs(300), true)); // Only 2 entries

    // Insert 3 articles with delays for deterministic eviction
    for i in 1..=3 {
        let msg_str = format!("<test{}@example.com>", i);
        let msgid = MessageId::from_borrowed(&msg_str).unwrap();
        let buffer = format!("220 Body {}\r\n.\r\n", i).into_bytes();
        cache.upsert(msgid, buffer, BackendId::from_index(0)).await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let stats = cache.stats().await;
    assert!(
        stats.entry_count <= 2,
        "Cache should respect capacity limit, got {}",
        stats.entry_count
    );

    Ok(())
}

/// Test cache with different MessageId formats
#[tokio::test]
async fn test_cache_different_message_id_formats() -> Result<()> {
    let cache = Arc::new(ArticleCache::new(20000, Duration::from_secs(300), true));

    // Different valid message-ID formats
    let test_cases = vec![
        "<simple@example.com>",
        "<complex+tag@sub.domain.com>",
        "<123456@news.server>",
        "<$uuid@host>",
    ];

    for msg_str in test_cases {
        let msgid = MessageId::from_borrowed(msg_str).unwrap();
        let buffer = format!("220 {}\r\n.\r\n", msg_str).into_bytes();
        cache
            .upsert(msgid.clone(), buffer, BackendId::from_index(0))
            .await;

        // Verify retrievable
        assert!(cache.get(&msgid).await.is_some());
    }

    Ok(())
}

/// Test cache stats reporting
#[tokio::test]
async fn test_cache_stats() -> Result<()> {
    // Increased capacity to account for corrected weigher (2.5x correction factor)
    // Each 1KB entry now weighs ~3.2KB after correction, so 10 entries need ~32KB
    let cache = Arc::new(ArticleCache::new(40000, Duration::from_secs(300), true));

    // Initially empty
    let stats = cache.stats().await;
    assert_eq!(stats.entry_count, 0);

    // Add some entries
    for i in 1..=10 {
        let msg_str = format!("<test{}@example.com>", i);
        let msgid = MessageId::from_borrowed(&msg_str).unwrap();
        let mut buffer = vec![0u8; 1024];
        // Put valid status code at start
        buffer[0..3].copy_from_slice(b"220");
        cache.upsert(msgid, buffer, BackendId::from_index(0)).await;
    }

    // Run pending background tasks to ensure cache is fully updated
    cache.sync().await;

    let stats = cache.stats().await;
    assert_eq!(stats.entry_count, 10);
    assert!(stats.weighted_size > 0);

    Ok(())
}

/// Test Arc<str> borrow lookup (zero-allocation cache get)
#[tokio::test]
async fn test_zero_allocation_lookup() -> Result<()> {
    let cache = Arc::new(ArticleCache::new(20000, Duration::from_secs(300), true));

    let msgid1 = MessageId::from_borrowed("<test@example.com>").unwrap();
    let buffer = b"220 Body\r\n.\r\n".to_vec();

    cache.upsert(msgid1, buffer, BackendId::from_index(0)).await;

    // Create different MessageId instance with same content
    let msgid2 = MessageId::from_borrowed("<test@example.com>").unwrap();

    // Should find via Borrow<str> trait (zero allocation)
    assert!(cache.get(&msgid2).await.is_some());

    Ok(())
}
