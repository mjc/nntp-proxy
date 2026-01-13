//! Tests for UnifiedCache and related cache functionality
//!
//! This test suite covers:
//! - UnifiedCache enum dispatch (memory vs hybrid)
//! - CacheStatsProvider trait implementations
//! - ArticleEntry::response_for_command() including STAT synthesis
//! - ArticleAvailability::from_bits() and ArticleEntry::set_availability()
//! - DiskCache configuration defaults and validation

use nntp_proxy::cache::{
    ArticleAvailability, ArticleCache, ArticleEntry, BackendStatus, UnifiedCache,
};
use nntp_proxy::router::BackendCount;
use nntp_proxy::types::{BackendId, MessageId};
use std::time::Duration;

// ============================================================================
// UnifiedCache Tests
// ============================================================================

#[tokio::test]
async fn test_unified_cache_memory_construction() {
    let cache = UnifiedCache::memory(1000, Duration::from_secs(60), true);
    assert!(!cache.is_hybrid());
    assert_eq!(cache.capacity(), 1000);
    assert_eq!(cache.entry_count(), 0);
}

#[tokio::test]
async fn test_unified_cache_memory_get_miss() {
    let cache = UnifiedCache::memory(1000, Duration::from_secs(60), true);
    let msg_id = MessageId::from_str_or_wrap("test@example.com").unwrap();

    let result = cache.get(&msg_id).await;
    assert!(result.is_none());
}

#[tokio::test]
async fn test_unified_cache_memory_upsert_and_get() {
    let cache = UnifiedCache::memory(100_000, Duration::from_secs(60), true);
    let msg_id = MessageId::from_str_or_wrap("test@example.com").unwrap();
    let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
    let backend_id = BackendId::from_index(0);

    cache
        .upsert(msg_id.clone(), buffer.clone(), backend_id)
        .await;

    let result = cache.get(&msg_id).await;
    assert!(result.is_some());
    let entry = result.unwrap();
    assert_eq!(&**entry.response(), buffer.as_slice());
}

#[tokio::test]
async fn test_unified_cache_memory_record_missing() {
    let cache = UnifiedCache::memory(100_000, Duration::from_secs(60), true);
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
async fn test_unified_cache_sync_availability() {
    let cache = UnifiedCache::memory(100_000, Duration::from_secs(60), true);
    let msg_id = MessageId::from_str_or_wrap("test@example.com").unwrap();
    let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
    let backend_id = BackendId::from_index(0);

    // First insert an article
    cache
        .upsert(msg_id.clone(), buffer.clone(), backend_id)
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
async fn test_unified_cache_is_hybrid_false_for_memory() {
    let cache = UnifiedCache::memory(1000, Duration::from_secs(60), true);
    assert!(!cache.is_hybrid());
}

#[tokio::test]
async fn test_unified_cache_hit_rate_initially_zero() {
    let cache = UnifiedCache::memory(1000, Duration::from_secs(60), true);
    // Hit rate should be 0 (or NaN which displays as 0) when no operations
    let hit_rate = cache.hit_rate();
    assert!(hit_rate == 0.0 || hit_rate.is_nan());
}

#[tokio::test]
async fn test_unified_cache_weighted_size() {
    let cache = UnifiedCache::memory(100_000, Duration::from_secs(60), true);
    let msg_id = MessageId::from_str_or_wrap("test@example.com").unwrap();
    let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();

    cache
        .upsert(msg_id.clone(), buffer, BackendId::from_index(0))
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

    let cache = ArticleCache::new(10000, Duration::from_secs(60), true);
    let stats = cache.display_stats();

    assert_eq!(stats.entry_count, 0);
    assert_eq!(stats.size_bytes, 0);
    assert!(stats.disk.is_none()); // Memory-only cache has no disk stats
}

#[tokio::test]
async fn test_cache_stats_provider_unified_cache_memory() {
    use nntp_proxy::cache::CacheStatsProvider;

    let cache = UnifiedCache::memory(10000, Duration::from_secs(60), true);
    let stats = cache.display_stats();

    assert_eq!(stats.entry_count, 0);
    assert!(stats.disk.is_none()); // Memory-only cache has no disk stats
}

// ============================================================================
// ArticleEntry::response_for_command() Tests
// ============================================================================

#[test]
fn test_response_for_command_stat_synthesizes_223() {
    // Create ARTICLE response (220)
    let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
    let entry = ArticleEntry::new(buffer);

    let response = entry.response_for_command("STAT", "<test@example.com>");
    assert!(response.is_some());

    let response_bytes = response.unwrap();
    let response_str = String::from_utf8_lossy(&response_bytes);
    assert!(response_str.starts_with("223 0 <test@example.com>\r\n"));
}

#[test]
fn test_response_for_command_stat_from_body() {
    // BODY response (222) should also synthesize STAT response
    let buffer = b"222 0 <test@example.com>\r\nBody content\r\n.\r\n".to_vec();
    let entry = ArticleEntry::new(buffer);

    let response = entry.response_for_command("STAT", "<test@example.com>");
    assert!(response.is_some());

    let response_bytes = response.unwrap();
    let response_str = String::from_utf8_lossy(&response_bytes);
    assert!(response_str.starts_with("223 0 <test@example.com>\r\n"));
}

#[test]
fn test_response_for_command_stat_from_head() {
    // HEAD response (221) should also synthesize STAT response
    let buffer = b"221 0 <test@example.com>\r\nSubject: Test\r\n.\r\n".to_vec();
    let entry = ArticleEntry::new(buffer);

    let response = entry.response_for_command("STAT", "<test@example.com>");
    assert!(response.is_some());

    let response_bytes = response.unwrap();
    let response_str = String::from_utf8_lossy(&response_bytes);
    assert!(response_str.starts_with("223 0 <test@example.com>\r\n"));
}

#[test]
fn test_response_for_command_430_returns_none_for_stat() {
    // 430 should NOT synthesize STAT - article doesn't exist
    let buffer = b"430 No Such Article\r\n".to_vec();
    let entry = ArticleEntry::new(buffer);

    let response = entry.response_for_command("STAT", "<test@example.com>");
    assert!(response.is_none());
}

#[test]
fn test_response_for_command_article_direct_match() {
    let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
    let entry = ArticleEntry::new(buffer.clone());

    let response = entry.response_for_command("ARTICLE", "<test@example.com>");
    assert!(response.is_some());
    assert_eq!(response.unwrap(), buffer);
}

#[test]
fn test_response_for_command_body_from_article() {
    // ARTICLE (220) can serve BODY request
    let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
    let entry = ArticleEntry::new(buffer.clone());

    let response = entry.response_for_command("BODY", "<test@example.com>");
    assert!(response.is_some());
    assert_eq!(response.unwrap(), buffer);
}

#[test]
fn test_response_for_command_head_from_article() {
    // ARTICLE (220) can serve HEAD request
    let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
    let entry = ArticleEntry::new(buffer.clone());

    let response = entry.response_for_command("HEAD", "<test@example.com>");
    assert!(response.is_some());
    assert_eq!(response.unwrap(), buffer);
}

#[test]
fn test_response_for_command_body_cannot_serve_article() {
    // BODY (222) cannot serve ARTICLE request
    let buffer = b"222 0 <test@example.com>\r\nBody content\r\n.\r\n".to_vec();
    let entry = ArticleEntry::new(buffer);

    let response = entry.response_for_command("ARTICLE", "<test@example.com>");
    assert!(response.is_none());
}

#[test]
fn test_response_for_command_head_cannot_serve_body() {
    // HEAD (221) cannot serve BODY request
    let buffer = b"221 0 <test@example.com>\r\nSubject: Test\r\n.\r\n".to_vec();
    let entry = ArticleEntry::new(buffer);

    let response = entry.response_for_command("BODY", "<test@example.com>");
    assert!(response.is_none());
}

#[test]
fn test_response_for_command_validates_buffer() {
    // Create an invalid buffer (missing .\r\n terminator)
    let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody".to_vec();
    let entry = ArticleEntry::new(buffer);

    // Should return None because buffer fails validation
    let response = entry.response_for_command("ARTICLE", "<test@example.com>");
    assert!(response.is_none());
}

#[test]
fn test_response_for_command_empty_message_id() {
    let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
    let entry = ArticleEntry::new(buffer);

    // STAT with empty message ID should still work (just returns empty ID in response)
    let response = entry.response_for_command("STAT", "");
    assert!(response.is_some());
    let response_bytes = response.unwrap();
    let response_str = String::from_utf8_lossy(&response_bytes);
    assert!(response_str.starts_with("223 0 \r\n"));
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
    let avail = ArticleAvailability::from_bits(0b00000011, 0b00000001);

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
// ArticleEntry::set_availability() Tests
// ============================================================================

#[test]
fn test_article_entry_set_availability() {
    let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
    let mut entry = ArticleEntry::new(buffer);

    // Initially no availability info
    assert!(!entry.has_availability_info());

    // Create availability with some backends marked
    let mut avail = ArticleAvailability::new();
    avail.record_missing(BackendId::from_index(0));
    avail.record_has(BackendId::from_index(1));

    entry.set_availability(avail);

    // Now entry should have availability info
    assert!(entry.has_availability_info());
    assert!(!entry.should_try_backend(BackendId::from_index(0))); // missing
    assert!(entry.should_try_backend(BackendId::from_index(1))); // has
    assert!(entry.should_try_backend(BackendId::from_index(2))); // not checked = try
}

#[test]
fn test_article_entry_set_availability_overwrites() {
    let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
    let mut entry = ArticleEntry::new(buffer);

    // Set initial availability
    let mut avail1 = ArticleAvailability::new();
    avail1.record_missing(BackendId::from_index(0));
    entry.set_availability(avail1);

    assert!(!entry.should_try_backend(BackendId::from_index(0)));

    // Set new availability that overwrites
    let mut avail2 = ArticleAvailability::new();
    avail2.record_has(BackendId::from_index(0)); // Now backend 0 has it
    entry.set_availability(avail2);

    // Should reflect new availability
    assert!(entry.should_try_backend(BackendId::from_index(0)));
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
    assert!(config.compression); // Default is true
    assert_eq!(config.shards, 4);
}

#[test]
fn test_disk_cache_config_serde_roundtrip() {
    use nntp_proxy::config::DiskCache;

    let config = DiskCache {
        path: std::path::PathBuf::from("/tmp/test-cache"),
        capacity: nntp_proxy::types::CacheCapacity::try_new(1024 * 1024 * 1024).unwrap(),
        compression: false,
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
        compression = true
        shards = 4
    "#;

    let config: Cache = toml::from_str(toml_str).unwrap();

    assert!(config.disk.is_some());
    let disk = config.disk.unwrap();
    assert_eq!(disk.path, std::path::PathBuf::from("/tmp/nntp-cache"));
    assert!(disk.compression);
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
// Invalid Response Validation Tests
// ============================================================================

#[test]
fn test_response_validation_rejects_short_buffer() {
    // Buffer too short (< 9 bytes)
    let buffer = b"220 ok".to_vec();
    let entry = ArticleEntry::new(buffer);

    // Should fail validation and return None
    let response = entry.response_for_command("ARTICLE", "<test@example.com>");
    assert!(response.is_none());
}

#[test]
fn test_response_validation_rejects_missing_terminator() {
    // Buffer missing .\r\n terminator
    let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n".to_vec();
    let entry = ArticleEntry::new(buffer);

    let response = entry.response_for_command("ARTICLE", "<test@example.com>");
    assert!(response.is_none());
}

#[test]
fn test_response_validation_rejects_non_digit_start() {
    // Buffer not starting with digits - but this won't even parse as valid status code
    // So we test with a valid-looking but corrupted buffer
    let buffer = b"ABC 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
    let entry = ArticleEntry::new(buffer);

    // status_code() will return None, so response_for_command returns None
    let response = entry.response_for_command("ARTICLE", "<test@example.com>");
    assert!(response.is_none());
}

#[test]
fn test_response_validation_accepts_valid_buffer() {
    // Valid buffer passes validation
    let buffer =
        b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody content here\r\n.\r\n".to_vec();
    let entry = ArticleEntry::new(buffer.clone());

    let response = entry.response_for_command("ARTICLE", "<test@example.com>");
    assert!(response.is_some());
    assert_eq!(response.unwrap(), buffer);
}

// ============================================================================
// HybridArticleEntry Availability Timestamp Tests
// ============================================================================

use nntp_proxy::cache::HybridArticleEntry;

fn make_valid_article_buffer() -> Vec<u8> {
    b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec()
}

fn make_430_stub_buffer() -> Vec<u8> {
    b"430 No such article\r\n".to_vec()
}

#[test]
fn test_hybrid_entry_has_availability_info_empty() {
    let entry = HybridArticleEntry::new(make_valid_article_buffer()).unwrap();

    // Fresh entry has no availability info
    assert!(!entry.has_availability_info());
}

#[test]
fn test_hybrid_entry_has_availability_info_after_missing() {
    let mut entry = HybridArticleEntry::new(make_valid_article_buffer()).unwrap();

    entry.record_backend_missing(BackendId::from_index(0));

    assert!(entry.has_availability_info());
}

#[test]
fn test_hybrid_entry_has_availability_info_after_has() {
    let mut entry = HybridArticleEntry::new(make_valid_article_buffer()).unwrap();

    entry.record_backend_has(BackendId::from_index(0));

    assert!(entry.has_availability_info());
}

#[test]
fn test_hybrid_entry_is_availability_stale_fresh() {
    let entry = HybridArticleEntry::new(make_valid_article_buffer()).unwrap();

    // Entry just created should not be stale with reasonable TTL
    assert!(!entry.is_availability_stale(3600)); // 1 hour TTL
    assert!(!entry.is_availability_stale(60)); // 1 minute TTL
}

#[test]
fn test_hybrid_entry_is_availability_stale_zero_ttl() {
    let entry = HybridArticleEntry::new(make_valid_article_buffer()).unwrap();

    // With zero TTL, everything is immediately stale (edge case)
    // Note: implementation uses `> ttl` so zero TTL means 0 seconds old is not stale
    assert!(!entry.is_availability_stale(0));
}

#[test]
fn test_hybrid_entry_clear_stale_availability_not_stale() {
    let mut entry = HybridArticleEntry::new(make_valid_article_buffer()).unwrap();

    // Record some availability info
    entry.record_backend_missing(BackendId::from_index(0));
    entry.record_backend_has(BackendId::from_index(1));

    assert!(entry.has_availability_info());
    assert!(!entry.should_try_backend(BackendId::from_index(0)));
    assert!(entry.should_try_backend(BackendId::from_index(1)));

    // With large TTL, should NOT clear
    entry.clear_stale_availability(3600);

    // Availability should still be there
    assert!(entry.has_availability_info());
    assert!(!entry.should_try_backend(BackendId::from_index(0)));
}

#[test]
fn test_hybrid_entry_should_try_after_record_missing() {
    let mut entry = HybridArticleEntry::new(make_valid_article_buffer()).unwrap();

    // Initially should try all backends
    assert!(entry.should_try_backend(BackendId::from_index(0)));
    assert!(entry.should_try_backend(BackendId::from_index(1)));
    assert!(entry.should_try_backend(BackendId::from_index(2)));

    // Mark backend 1 as missing
    entry.record_backend_missing(BackendId::from_index(1));

    // Should NOT try backend 1, but others are fine
    assert!(entry.should_try_backend(BackendId::from_index(0)));
    assert!(!entry.should_try_backend(BackendId::from_index(1)));
    assert!(entry.should_try_backend(BackendId::from_index(2)));
}

#[test]
fn test_hybrid_entry_should_try_after_record_has() {
    let mut entry = HybridArticleEntry::new(make_valid_article_buffer()).unwrap();

    // Mark backend 0 as missing first
    entry.record_backend_missing(BackendId::from_index(0));
    assert!(!entry.should_try_backend(BackendId::from_index(0)));

    // Now mark it as having the article (e.g., it acquired it later)
    entry.record_backend_has(BackendId::from_index(0));

    // Should be able to try it again
    assert!(entry.should_try_backend(BackendId::from_index(0)));
}

#[test]
fn test_hybrid_entry_all_backends_exhausted_none() {
    let entry = HybridArticleEntry::new(make_valid_article_buffer()).unwrap();

    // With no missing backends marked, not exhausted
    assert!(!entry.all_backends_exhausted(BackendCount::new(3)));
}

#[test]
fn test_hybrid_entry_all_backends_exhausted_partial() {
    let mut entry = HybridArticleEntry::new(make_valid_article_buffer()).unwrap();

    // Mark 2 of 3 backends as missing
    entry.record_backend_missing(BackendId::from_index(0));
    entry.record_backend_missing(BackendId::from_index(1));

    // Not exhausted yet
    assert!(!entry.all_backends_exhausted(BackendCount::new(3)));
}

#[test]
fn test_hybrid_entry_all_backends_exhausted_all() {
    let mut entry = HybridArticleEntry::new(make_valid_article_buffer()).unwrap();

    // Mark all 3 backends as missing
    entry.record_backend_missing(BackendId::from_index(0));
    entry.record_backend_missing(BackendId::from_index(1));
    entry.record_backend_missing(BackendId::from_index(2));

    // Now exhausted
    assert!(entry.all_backends_exhausted(BackendCount::new(3)));
}

#[test]
fn test_hybrid_entry_430_stub_creation() {
    let entry = HybridArticleEntry::new(make_430_stub_buffer()).unwrap();

    // 430 responses should be cached
    assert_eq!(entry.status_code().unwrap().as_u16(), 430);

    // But not considered complete articles
    assert!(!entry.is_complete_article());
}

#[test]
fn test_hybrid_entry_complete_article_detection() {
    let article_buf = make_valid_article_buffer();
    let entry = HybridArticleEntry::new(article_buf).unwrap();

    // 220 with proper terminator is a complete article
    assert!(entry.is_complete_article());
    assert_eq!(entry.status_code().unwrap().as_u16(), 220);
}

#[test]
fn test_hybrid_entry_response_for_command_article() {
    let buffer = make_valid_article_buffer();
    let entry = HybridArticleEntry::new(buffer.clone()).unwrap();

    let response = entry.response_for_command("ARTICLE", "<test@example.com>");
    assert!(response.is_some());
    assert_eq!(response.unwrap(), buffer);
}

#[test]
fn test_hybrid_entry_response_for_command_stat_synthesized() {
    let entry = HybridArticleEntry::new(make_valid_article_buffer()).unwrap();

    let response = entry.response_for_command("STAT", "<test@example.com>");
    assert!(response.is_some());

    let response_str = String::from_utf8(response.unwrap()).unwrap();
    assert!(response_str.starts_with("223 0 <test@example.com>"));
}

#[test]
fn test_hybrid_entry_response_for_command_430_returns_none() {
    let entry = HybridArticleEntry::new(make_430_stub_buffer()).unwrap();

    // 430 stubs should not serve ARTICLE requests
    assert!(
        entry
            .response_for_command("ARTICLE", "<test@example.com>")
            .is_none()
    );

    // 430 stubs should not serve STAT either (article doesn't exist)
    assert!(
        entry
            .response_for_command("STAT", "<test@example.com>")
            .is_none()
    );
}

#[test]
fn test_hybrid_entry_invalid_status_code_rejected() {
    // 200 is not a valid article response code
    let buffer = b"200 server ready\r\n".to_vec();
    let entry = HybridArticleEntry::new(buffer);

    // Should reject invalid status codes
    assert!(entry.is_none());
}

#[test]
fn test_hybrid_entry_valid_status_codes() {
    // Valid codes: 220 (ARTICLE), 221 (HEAD), 222 (BODY), 223 (STAT), 430 (not found)
    let valid_220 = HybridArticleEntry::new(b"220 0 <id>\r\nH: V\r\n\r\nBody\r\n.\r\n".to_vec());
    let valid_221 = HybridArticleEntry::new(b"221 0 <id>\r\nH: V\r\n.\r\n".to_vec());
    let valid_222 = HybridArticleEntry::new(b"222 0 <id>\r\nBody\r\n.\r\n".to_vec());
    let valid_223 = HybridArticleEntry::new(b"223 0 <id>\r\n".to_vec());
    let valid_430 = HybridArticleEntry::new(b"430 No article\r\n".to_vec());

    assert!(valid_220.is_some());
    assert!(valid_221.is_some());
    assert!(valid_222.is_some());
    assert!(valid_223.is_some());
    assert!(valid_430.is_some());
}

// ============================================================================
// HybridArticleEntry Serialization Tests (Code trait)
// ============================================================================

#[test]
fn test_hybrid_entry_serialization_roundtrip() {
    use foyer::Code;
    use std::io::Cursor;

    let buffer = make_valid_article_buffer();
    let mut entry = HybridArticleEntry::new(buffer.clone()).unwrap();

    // Add some availability info
    entry.record_backend_missing(BackendId::from_index(0));
    entry.record_backend_has(BackendId::from_index(1));

    // Serialize
    let mut encoded = Vec::new();
    entry.encode(&mut encoded).unwrap();

    // Deserialize
    let decoded = HybridArticleEntry::decode(&mut Cursor::new(&encoded)).unwrap();

    // Verify all fields match
    assert_eq!(decoded.status_code(), entry.status_code());
    assert_eq!(decoded.buffer(), entry.buffer());
    assert!(!decoded.should_try_backend(BackendId::from_index(0)));
    assert!(decoded.should_try_backend(BackendId::from_index(1)));
}

#[test]
fn test_hybrid_entry_serialization_preserves_availability() {
    use foyer::Code;
    use std::io::Cursor;

    let mut entry = HybridArticleEntry::new(make_valid_article_buffer()).unwrap();

    // Mark several backends
    entry.record_backend_missing(BackendId::from_index(0));
    entry.record_backend_missing(BackendId::from_index(2));
    entry.record_backend_missing(BackendId::from_index(4));
    entry.record_backend_has(BackendId::from_index(1));
    entry.record_backend_has(BackendId::from_index(3));

    // Serialize and deserialize
    let mut encoded = Vec::new();
    entry.encode(&mut encoded).unwrap();
    let decoded = HybridArticleEntry::decode(&mut Cursor::new(&encoded)).unwrap();

    // Verify availability preserved
    assert!(!decoded.should_try_backend(BackendId::from_index(0)));
    assert!(decoded.should_try_backend(BackendId::from_index(1)));
    assert!(!decoded.should_try_backend(BackendId::from_index(2)));
    assert!(decoded.should_try_backend(BackendId::from_index(3)));
    assert!(!decoded.should_try_backend(BackendId::from_index(4)));
}

#[test]
fn test_hybrid_entry_serialization_estimated_size() {
    use foyer::Code;

    let buffer = make_valid_article_buffer();
    let entry = HybridArticleEntry::new(buffer.clone()).unwrap();

    // estimated_size = 2 (status) + 2 (checked+missing) + 8 (timestamp) + 4 (len) + buffer.len()
    // Format: [status:u16][checked:u8][missing:u8][timestamp:u64][len:u32][buffer:bytes]
    let expected_size = 2 + 2 + 8 + 4 + buffer.len();
    assert_eq!(entry.estimated_size(), expected_size);
}

#[test]
fn test_hybrid_entry_serialization_430_stub() {
    use foyer::Code;
    use std::io::Cursor;

    let mut entry = HybridArticleEntry::new(make_430_stub_buffer()).unwrap();
    entry.record_backend_missing(BackendId::from_index(0));

    // Serialize and deserialize
    let mut encoded = Vec::new();
    entry.encode(&mut encoded).unwrap();
    let decoded = HybridArticleEntry::decode(&mut Cursor::new(&encoded)).unwrap();

    assert_eq!(decoded.status_code().unwrap().as_u16(), 430);
    assert!(!decoded.should_try_backend(BackendId::from_index(0)));
}

#[test]
fn test_hybrid_entry_serialization_empty_availability() {
    use foyer::Code;
    use std::io::Cursor;

    let entry = HybridArticleEntry::new(make_valid_article_buffer()).unwrap();

    // No availability recorded
    assert!(!entry.has_availability_info());

    // Serialize and deserialize
    let mut encoded = Vec::new();
    entry.encode(&mut encoded).unwrap();
    let decoded = HybridArticleEntry::decode(&mut Cursor::new(&encoded)).unwrap();

    // Should still have no availability info
    assert!(!decoded.has_availability_info());

    // All backends should be tryable
    for i in 0..8 {
        assert!(decoded.should_try_backend(BackendId::from_index(i)));
    }
}

#[test]
fn test_hybrid_entry_decode_invalid_short_buffer() {
    use foyer::Code;
    use std::io::Cursor;

    // Buffer too short (less than header)
    let short_buf = vec![0u8; 5];
    let result = HybridArticleEntry::decode(&mut Cursor::new(&short_buf));

    assert!(result.is_err());
}

#[test]
fn test_hybrid_entry_decode_invalid_status_code() {
    use foyer::Code;
    use std::io::Cursor;

    // Encode a fake entry with invalid status code (200)
    let mut buf = Vec::new();
    buf.extend_from_slice(&200u16.to_le_bytes()); // Invalid status
    buf.extend_from_slice(&0u8.to_le_bytes()); // checked
    buf.extend_from_slice(&0u8.to_le_bytes()); // missing
    buf.extend_from_slice(&0u64.to_le_bytes()); // timestamp
    buf.extend_from_slice(&5u32.to_le_bytes()); // len
    buf.extend_from_slice(b"hello"); // buffer

    let result = HybridArticleEntry::decode(&mut Cursor::new(&buf));
    assert!(result.is_err());
}

#[test]
fn test_hybrid_entry_decode_truncated_buffer() {
    use foyer::Code;
    use std::io::Cursor;

    // Encode header that claims more data than available
    let mut buf = Vec::new();
    buf.extend_from_slice(&220u16.to_le_bytes()); // status (2 bytes)
    buf.extend_from_slice(&0u8.to_le_bytes()); // checked (1 byte)
    buf.extend_from_slice(&0u8.to_le_bytes()); // missing (1 byte)
    buf.extend_from_slice(&1000u32.to_le_bytes()); // claims 1000 bytes
    buf.extend_from_slice(b"short"); // only 5 bytes (not 1000)

    let result = HybridArticleEntry::decode(&mut Cursor::new(&buf));
    assert!(
        result.is_err(),
        "Should fail when buffer length doesn't match claimed size"
    );
}
