//! Tests for `UnifiedCache` and related cache functionality
//!
//! This test suite covers:
//! - `UnifiedCache` enum dispatch (memory vs hybrid)
//! - `CacheStatsProvider` trait implementations
//! - `CachedArticle::cached_response_for()` including STAT synthesis
//! - `ArticleAvailability::from_bits()`
//! - `DiskCache` configuration defaults and validation

use nntp_proxy::cache::{ArticleAvailability, ArticleCache, BackendStatus, UnifiedCache};
use nntp_proxy::protocol::RequestKind;
use nntp_proxy::types::{BackendId, MessageId};
use std::time::Duration;

use super::article_response_bytes;
// UnifiedCache Tests
// ============================================================================

#[tokio::test]
async fn test_unified_cache_memory_construction() {
    let cache = UnifiedCache::memory(1000, Duration::from_secs(60));
    assert!(!cache.is_hybrid());
    assert_eq!(cache.capacity(), 1000);
    assert_eq!(cache.entry_count(), 0);
}

#[tokio::test]
async fn test_unified_cache_availability_construction() {
    let cache = UnifiedCache::availability(4096, std::time::Duration::MAX);
    assert!(!cache.is_hybrid());
    assert_eq!(cache.capacity(), 4096);
    assert_eq!(cache.entry_count(), 0);
}

#[tokio::test]
async fn test_unified_cache_memory_get_miss() {
    let cache = UnifiedCache::memory(1000, Duration::from_secs(60));
    let msg_id = MessageId::from_str_or_wrap("test@example.com").unwrap();

    let result = cache.get(&msg_id).await;
    assert!(result.is_none());
}

#[tokio::test]
async fn test_unified_cache_availability_get_miss() {
    let cache = UnifiedCache::availability(4096, std::time::Duration::MAX);
    let msg_id = MessageId::from_str_or_wrap("test@example.com").unwrap();

    let result = cache.get(&msg_id).await;
    assert!(result.is_none());
}

#[tokio::test]
async fn test_unified_cache_memory_upsert_and_get() {
    let cache = UnifiedCache::memory(100_000, Duration::from_secs(60));
    let msg_id = MessageId::from_str_or_wrap("test@example.com").unwrap();
    let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
    let backend_id = BackendId::from_index(0);

    cache
        .upsert_ingest(msg_id.clone(), buffer.clone(), backend_id, 0.into())
        .await;

    let result = cache.get(&msg_id).await;
    assert!(result.is_some());
    let entry = result.unwrap();
    assert_eq!(
        article_response_bytes(&entry, RequestKind::Article, &msg_id).unwrap(),
        buffer
    );
}

#[tokio::test]
async fn test_unified_cache_memory_record_missing() {
    let cache = UnifiedCache::memory(100_000, Duration::from_secs(60));
    let msg_id = MessageId::from_str_or_wrap("missing@example.com").unwrap();
    let backend_id = BackendId::from_index(0);

    cache
        .record_backend_missing(msg_id.clone(), backend_id)
        .await;

    let result = cache.get(&msg_id).await;
    assert!(result.is_some());
    let entry = result.unwrap();
    assert!(!entry.should_try_backend(backend_id));
}

#[tokio::test]
async fn test_unified_cache_availability_record_missing() {
    let cache = UnifiedCache::availability(8192, std::time::Duration::MAX);
    let msg_id = MessageId::from_str_or_wrap("missing@example.com").unwrap();
    let backend_id = BackendId::from_index(0);

    cache
        .record_backend_missing(msg_id.clone(), backend_id)
        .await;

    let entry = cache.get(&msg_id).await.expect("availability entry");
    assert!(!entry.should_try_backend(backend_id));
    assert_eq!(entry.availability().missing_bits(), 0b0000_0001);
}

#[tokio::test]
async fn test_unified_cache_sync_availability() {
    let cache = UnifiedCache::memory(100_000, Duration::from_secs(60));
    let msg_id = MessageId::from_str_or_wrap("test@example.com").unwrap();
    let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
    let backend_id = BackendId::from_index(0);

    // First insert an article
    cache
        .upsert_ingest(msg_id.clone(), buffer.clone(), backend_id, 0.into())
        .await;

    // Create availability with backend 1 marked as missing
    let mut availability = ArticleAvailability::new();
    availability.record_missing(BackendId::from_index(1));

    // Sync availability
    cache.sync_availability(msg_id.clone(), &availability).await;

    // Verify backend 1 is now marked as missing
    let result = cache.get(&msg_id).await.unwrap();
    assert!(!result.should_try_backend(BackendId::from_index(1)));
    // Backend 0 should still be OK (it has the article)
    assert!(result.should_try_backend(backend_id));
}

#[tokio::test]
async fn test_unified_cache_availability_sync_persists_only_missing_bits() {
    let cache = UnifiedCache::availability(8192, std::time::Duration::MAX);
    let msg_id = MessageId::from_str_or_wrap("test@example.com").unwrap();
    let mut availability = ArticleAvailability::new();
    availability.record_missing(BackendId::from_index(1));
    availability.record_has(BackendId::from_index(2));

    cache.sync_availability(msg_id.clone(), &availability).await;

    let result = cache.get(&msg_id).await.expect("availability entry");
    assert!(!result.should_try_backend(BackendId::from_index(1)));
    assert!(result.should_try_backend(BackendId::from_index(2)));
    assert_eq!(result.availability().checked_bits(), 0b0000_0010);
    assert_eq!(result.availability().missing_bits(), 0b0000_0010);
}

#[tokio::test]
async fn test_unified_cache_is_hybrid_false_for_memory() {
    let cache = UnifiedCache::memory(1000, Duration::from_secs(60));
    assert!(!cache.is_hybrid());
}

#[tokio::test]
async fn test_unified_cache_hit_rate_initially_zero() {
    let cache = UnifiedCache::memory(1000, Duration::from_secs(60));
    // Hit rate should be 0 (or NaN which displays as 0) when no operations
    let hit_rate = cache.hit_rate();
    assert!(hit_rate == 0.0 || hit_rate.is_nan());
}

#[tokio::test]
async fn test_unified_cache_weighted_size() {
    let cache = UnifiedCache::memory(100_000, Duration::from_secs(60));
    let msg_id = MessageId::from_str_or_wrap("test@example.com").unwrap();
    let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();

    cache
        .upsert_ingest(msg_id.clone(), buffer, BackendId::from_index(0), 0.into())
        .await;

    // Run pending tasks to ensure moka updates its internal state
    cache.sync().await;

    // Verify the entry exists
    assert_eq!(cache.entry_count(), 1);

    // Weighted size should now be > 0
    assert!(cache.weighted_size() > 0);
}

// ============================================================================
// CacheStatsProvider Tests
// ============================================================================

#[tokio::test]
async fn test_cache_stats_provider_article_cache() {
    use nntp_proxy::cache::CacheStatsProvider;

    let cache = ArticleCache::new(10000, Duration::from_secs(60));
    let stats = cache.display_stats();

    assert_eq!(stats.entry_count, 0);
    assert_eq!(stats.size_bytes, 0);
    assert!(stats.disk.is_none()); // Memory-only cache has no disk stats
}

#[tokio::test]
async fn test_cache_stats_provider_unified_cache_memory() {
    use nntp_proxy::cache::CacheStatsProvider;

    let cache = UnifiedCache::memory(10000, Duration::from_secs(60));
    let stats = cache.display_stats();

    assert_eq!(stats.entry_count, 0);
    assert!(stats.disk.is_none()); // Memory-only cache has no disk stats
}

// ============================================================================
// ArticleAvailability::from_bits() Tests
// ============================================================================

#[test]
fn test_availability_from_bits_empty() {
    let avail = ArticleAvailability::from_bits(0, 0);
    assert!(!avail.has_availability_info());

    // All backends should be "unknown" (not yet checked)
    for i in 0..8 {
        assert_eq!(
            avail.status(BackendId::from_index(i)),
            BackendStatus::Unknown
        );
    }
}

#[test]
fn test_availability_from_bits_some_missing() {
    // checked=0b00000011, missing=0b00000001 means:
    // - backend 0: checked and missing (430)
    // - backend 1: checked and has article
    let avail = ArticleAvailability::from_bits(0b0000_0011, 0b0000_0001);

    assert!(avail.has_availability_info());
    assert_eq!(
        avail.status(BackendId::from_index(0)),
        BackendStatus::Missing
    );
    assert_eq!(
        avail.status(BackendId::from_index(1)),
        BackendStatus::HasArticle
    );
    assert_eq!(
        avail.status(BackendId::from_index(2)),
        BackendStatus::Unknown
    );
}

#[test]
fn test_availability_from_bits_all_missing() {
    // All 8 backends checked and missing
    let avail = ArticleAvailability::from_bits(0xFF, 0xFF);

    for i in 0..8 {
        assert_eq!(
            avail.status(BackendId::from_index(i)),
            BackendStatus::Missing
        );
        assert!(avail.is_missing(BackendId::from_index(i)));
    }
}

#[test]
fn test_availability_from_bits_roundtrip() {
    let mut original = ArticleAvailability::new();
    original.record_missing(BackendId::from_index(0));
    original.record_missing(BackendId::from_index(2));
    original.record_has(BackendId::from_index(1));
    original.record_has(BackendId::from_index(3));

    let checked = original.checked_bits();
    let missing = original.missing_bits();

    let reconstructed = ArticleAvailability::from_bits(checked, missing);

    // Should be equivalent
    assert_eq!(
        reconstructed.status(BackendId::from_index(0)),
        original.status(BackendId::from_index(0))
    );
    assert_eq!(
        reconstructed.status(BackendId::from_index(1)),
        original.status(BackendId::from_index(1))
    );
    assert_eq!(
        reconstructed.status(BackendId::from_index(2)),
        original.status(BackendId::from_index(2))
    );
    assert_eq!(
        reconstructed.status(BackendId::from_index(3)),
        original.status(BackendId::from_index(3))
    );
}

// ============================================================================
// DiskCache Configuration Tests
// ============================================================================

#[test]
fn test_disk_cache_config_defaults() {
    use nntp_proxy::config::DiskCache;

    let config = DiskCache::default();

    assert_eq!(
        config.path,
        std::path::PathBuf::from("/var/cache/nntp-proxy")
    );
    assert!(config.capacity.as_u64() > 0); // 10 GB default
    assert_eq!(
        config.compression,
        nntp_proxy::config::CompressionCodec::Lz4
    ); // Default is lz4
    assert_eq!(config.shards, 4);
}

#[test]
fn test_disk_cache_config_serde_roundtrip() {
    use nntp_proxy::config::DiskCache;

    let config = DiskCache {
        path: std::path::PathBuf::from("/tmp/test-cache"),
        capacity: nntp_proxy::types::CacheCapacity::try_new(1024 * 1024 * 1024).unwrap(),
        compression: nntp_proxy::config::CompressionCodec::None,
        shards: 8,
    };

    let toml_str = toml::to_string(&config).unwrap();
    let parsed: DiskCache = toml::from_str(&toml_str).unwrap();

    assert_eq!(parsed.path, config.path);
    assert_eq!(parsed.capacity.as_u64(), config.capacity.as_u64());
    assert_eq!(parsed.compression, config.compression);
    assert_eq!(parsed.shards, config.shards);
}

#[test]
fn test_cache_config_with_disk() {
    use nntp_proxy::config::Cache;

    let toml_str = r#"
        max_capacity = "256mb"
        ttl = 3600
        cache_articles = true
        adaptive_precheck = false

        [disk]
        path = "/tmp/nntp-cache"
        capacity = "5gb"
        compression = "lz4"
        shards = 4
    "#;

    let config: Cache = toml::from_str(toml_str).unwrap();

    assert!(config.disk.is_some());
    let disk = config.disk.unwrap();
    assert_eq!(disk.path, std::path::PathBuf::from("/tmp/nntp-cache"));
    assert_eq!(disk.compression, nntp_proxy::config::CompressionCodec::Lz4);
}

#[test]
fn test_cache_config_without_disk() {
    use nntp_proxy::config::Cache;

    let toml_str = r#"
        max_capacity = "256mb"
        ttl = 3600
        cache_articles = true
        adaptive_precheck = false
    "#;

    let config: Cache = toml::from_str(toml_str).unwrap();

    assert!(config.disk.is_none());
}

// ============================================================================
