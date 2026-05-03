//! Tests for `UnifiedCache` and related cache functionality
//!
//! This test suite covers:
//! - `UnifiedCache` enum dispatch (memory vs hybrid)
//! - `CacheStatsProvider` trait implementations
//! - `ArticleEntry::response_for()` including STAT synthesis
//! - `ArticleAvailability::from_bits()` and `ArticleEntry::set_availability()`
//! - `DiskCache` configuration defaults and validation

use futures::executor::block_on;
use nntp_proxy::cache::{
    ArticleAvailability, ArticleCache, ArticleEntry, BackendStatus, UnifiedCache,
};
use nntp_proxy::protocol::RequestKind;
use nntp_proxy::router::BackendCount;
use nntp_proxy::types::{BackendId, MessageId};
use std::time::Duration;

use super::{article_response_bytes, assert_article_response, assert_no_article_response};

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
        .upsert(msg_id.clone(), buffer.clone(), backend_id, 0.into())
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
        .upsert(msg_id.clone(), buffer.clone(), backend_id, 0.into())
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
        .upsert(msg_id.clone(), buffer, BackendId::from_index(0), 0.into())
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
// ArticleEntry::set_availability() Tests
// ============================================================================

#[test]
fn test_article_entry_set_availability() {
    let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
    let mut entry = ArticleEntry::from_response_bytes(buffer);

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
    let mut entry = ArticleEntry::from_response_bytes(buffer);

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
// Invalid Response Validation Tests
// ============================================================================

#[test]
fn test_response_validation_rejects_short_buffer() {
    // Buffer too short (< 9 bytes)
    let buffer = b"220 ok".to_vec();
    let entry = ArticleEntry::from_response_bytes(buffer);

    // Should fail validation and return None
    assert_no_article_response(&entry, RequestKind::Article);
}

#[test]
fn test_response_validation_rejects_missing_terminator() {
    // Buffer missing .\r\n terminator
    let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n".to_vec();
    let entry = ArticleEntry::from_response_bytes(buffer);

    assert_no_article_response(&entry, RequestKind::Article);
}

#[test]
fn test_response_validation_rejects_non_digit_start() {
    // Buffer not starting with digits - but this won't even parse as valid status code
    // So we test with a valid-looking but corrupted buffer
    let buffer = b"ABC 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
    let entry = ArticleEntry::from_response_bytes(buffer);

    assert_no_article_response(&entry, RequestKind::Article);
}

#[test]
fn test_response_validation_accepts_valid_buffer() {
    // Valid buffer passes validation
    let buffer =
        b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody content here\r\n.\r\n".to_vec();
    let entry = ArticleEntry::from_response_bytes(buffer.clone());

    assert_article_response(&entry, RequestKind::Article, &buffer);
}

// ============================================================================
// HybridArticleEntry Availability Timestamp Tests
// ============================================================================

use nntp_proxy::cache::HybridArticleEntry;

fn make_valid_article_buffer() -> Vec<u8> {
    b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec()
}

fn make_430_metadata_only_buffer() -> Vec<u8> {
    b"430 No such article\r\n".to_vec()
}

fn hybrid_article() -> HybridArticleEntry {
    HybridArticleEntry::from_response_bytes(make_valid_article_buffer()).unwrap()
}

fn hybrid_missing() -> HybridArticleEntry {
    HybridArticleEntry::from_response_bytes(make_430_metadata_only_buffer()).unwrap()
}

fn record_missing(entry: &mut HybridArticleEntry, backends: &[usize]) {
    backends
        .iter()
        .for_each(|backend| entry.record_backend_missing(BackendId::from_index(*backend)));
}

fn record_has(entry: &mut HybridArticleEntry, backends: &[usize]) {
    backends
        .iter()
        .for_each(|backend| entry.record_backend_has(BackendId::from_index(*backend)));
}

fn assert_hybrid_should_try(entry: &HybridArticleEntry, cases: &[(usize, bool)]) {
    cases.iter().for_each(|(backend, expected)| {
        assert_eq!(
            entry.should_try_backend(BackendId::from_index(*backend)),
            *expected,
            "backend {backend}"
        );
    });
}

fn hybrid_response_bytes(
    entry: &HybridArticleEntry,
    request_kind: RequestKind,
    message_id: &str,
) -> Option<Vec<u8>> {
    let response = entry.response_for(request_kind, message_id)?;
    let mut out = Vec::with_capacity(response.wire_len().get());
    block_on(response.write_to(&mut out)).ok()?;
    Some(out)
}

#[test]
fn test_hybrid_entry_has_availability_info_empty() {
    let entry = hybrid_article();

    // Fresh entry has no availability info
    assert!(!entry.has_availability_info());
}

#[test]
fn test_hybrid_entry_has_availability_info_after_missing() {
    let mut entry = hybrid_article();
    record_missing(&mut entry, &[0]);

    assert!(entry.has_availability_info());
}

#[test]
fn test_hybrid_entry_has_availability_info_after_has() {
    let mut entry = hybrid_article();
    record_has(&mut entry, &[0]);

    assert!(entry.has_availability_info());
}

#[test]
fn test_hybrid_entry_should_try_after_record_missing() {
    let mut entry = hybrid_article();

    assert_hybrid_should_try(&entry, &[(0, true), (1, true), (2, true)]);

    record_missing(&mut entry, &[1]);

    assert_hybrid_should_try(&entry, &[(0, true), (1, false), (2, true)]);
}

#[test]
fn test_hybrid_entry_should_try_after_record_has() {
    let mut entry = hybrid_article();

    record_missing(&mut entry, &[0]);
    assert_hybrid_should_try(&entry, &[(0, false)]);

    record_has(&mut entry, &[0]);

    assert_hybrid_should_try(&entry, &[(0, true)]);
}

#[test]
fn test_hybrid_entry_all_backends_exhausted_none() {
    let entry = hybrid_article();

    // With no missing backends marked, not exhausted
    assert!(!entry.all_backends_exhausted(BackendCount::new(3)));
}

#[test]
fn test_hybrid_entry_all_backends_exhausted_partial() {
    let mut entry = hybrid_article();

    record_missing(&mut entry, &[0, 1]);

    // Not exhausted yet
    assert!(!entry.all_backends_exhausted(BackendCount::new(3)));
}

#[test]
fn test_hybrid_entry_all_backends_exhausted_all() {
    let mut entry = hybrid_article();

    record_missing(&mut entry, &[0, 1, 2]);

    // Now exhausted
    assert!(entry.all_backends_exhausted(BackendCount::new(3)));
}

#[test]
fn test_hybrid_entry_430_metadata_only_creation() {
    let entry = hybrid_missing();

    // 430 responses should be cached
    assert_eq!(entry.status_code().as_u16(), 430);

    // But not considered complete articles
    assert!(!entry.is_complete_article());
}

#[test]
fn test_hybrid_entry_complete_article_detection() {
    let entry = hybrid_article();

    // 220 with proper terminator is a complete article
    assert!(entry.is_complete_article());
    assert_eq!(entry.status_code().as_u16(), 220);
}

#[test]
fn test_hybrid_entry_response_for_article() {
    let buffer = make_valid_article_buffer();
    let entry = hybrid_article();

    let response = hybrid_response_bytes(&entry, RequestKind::Article, "<test@example.com>");
    assert!(response.is_some());
    assert_eq!(response.unwrap(), buffer);
}

#[test]
fn test_hybrid_entry_response_for_stat_synthesized() {
    let entry = hybrid_article();

    let response = hybrid_response_bytes(&entry, RequestKind::Stat, "<test@example.com>");
    assert!(response.is_some());

    let response_str = String::from_utf8(response.unwrap()).unwrap();
    assert!(response_str.starts_with("223 0 <test@example.com>"));
}

#[test]
fn test_hybrid_entry_response_for_430_returns_none() {
    let entry = hybrid_missing();

    // missing entries should not serve ARTICLE requests
    assert!(hybrid_response_bytes(&entry, RequestKind::Article, "<test@example.com>").is_none());

    // missing entries should not serve STAT either (article doesn't exist)
    assert!(hybrid_response_bytes(&entry, RequestKind::Stat, "<test@example.com>").is_none());
}

#[test]
fn test_hybrid_entry_invalid_status_code_rejected() {
    // 200 is not a valid article response code
    let buffer = b"200 server ready\r\n".to_vec();
    let entry = HybridArticleEntry::from_response_bytes(buffer);

    // Should reject invalid status codes
    assert!(entry.is_none());
}

#[test]
fn test_hybrid_entry_valid_status_codes() {
    // Valid codes: 220 (ARTICLE), 221 (HEAD), 222 (BODY), 223 (STAT), 430 (not found)
    let valid_220 =
        HybridArticleEntry::from_response_bytes(b"220 0 <id>\r\nH: V\r\n\r\nBody\r\n.\r\n");
    let valid_221 = HybridArticleEntry::from_response_bytes(b"221 0 <id>\r\nH: V\r\n.\r\n");
    let valid_222 = HybridArticleEntry::from_response_bytes(b"222 0 <id>\r\nBody\r\n.\r\n");
    let valid_223 = HybridArticleEntry::from_response_bytes(b"223 0 <id>\r\n");
    let valid_430 = HybridArticleEntry::from_response_bytes(b"430 No article\r\n");

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

    let mut entry = hybrid_article();

    record_missing(&mut entry, &[0]);
    record_has(&mut entry, &[1]);

    // Serialize
    let mut encoded = Vec::new();
    entry.encode(&mut encoded).unwrap();

    // Deserialize
    let decoded = HybridArticleEntry::decode(&mut Cursor::new(&encoded)).unwrap();

    assert_eq!(decoded.status_code(), entry.status_code());
    assert_eq!(
        hybrid_response_bytes(&decoded, RequestKind::Article, "<test@example.com>"),
        hybrid_response_bytes(&entry, RequestKind::Article, "<test@example.com>")
    );
    assert_eq!(decoded.availability(), entry.availability());
    assert_eq!(decoded.tier().get(), entry.tier().get());
    assert_hybrid_should_try(&decoded, &[(0, false), (1, true)]);
}

#[test]
fn test_hybrid_entry_serialization_preserves_availability() {
    use foyer::Code;
    use std::io::Cursor;

    let mut entry = hybrid_article();

    record_missing(&mut entry, &[0, 2, 4]);
    record_has(&mut entry, &[1, 3]);

    // Serialize and deserialize
    let mut encoded = Vec::new();
    entry.encode(&mut encoded).unwrap();
    let decoded = HybridArticleEntry::decode(&mut Cursor::new(&encoded)).unwrap();

    assert_hybrid_should_try(
        &decoded,
        &[(0, false), (1, true), (2, false), (3, true), (4, false)],
    );
}

#[test]
fn test_hybrid_entry_serialization_estimated_size() {
    use foyer::Code;

    let entry = hybrid_article();

    let mut encoded = Vec::new();
    entry.encode(&mut encoded).unwrap();
    assert_eq!(entry.estimated_size(), encoded.len());
}

#[test]
fn test_hybrid_entry_serialization_430_metadata_only() {
    use foyer::Code;
    use std::io::Cursor;

    let mut entry = hybrid_missing();
    record_missing(&mut entry, &[0]);

    // Serialize and deserialize
    let mut encoded = Vec::new();
    entry.encode(&mut encoded).unwrap();
    let decoded = HybridArticleEntry::decode(&mut Cursor::new(&encoded)).unwrap();

    assert_eq!(decoded.status_code().as_u16(), 430);
    assert_hybrid_should_try(&decoded, &[(0, false)]);
}

#[test]
fn test_hybrid_entry_serialization_empty_availability() {
    use foyer::Code;
    use std::io::Cursor;

    let entry = hybrid_article();

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
    buf.extend_from_slice(&0u64.to_le_bytes()); // timestamp (8 bytes)
    buf.extend_from_slice(&1000u32.to_le_bytes()); // claims 1000 bytes
    buf.extend_from_slice(b"short"); // only 5 bytes (not 1000)

    let result = HybridArticleEntry::decode(&mut Cursor::new(&buf));
    assert!(
        result.is_err(),
        "Should fail when buffer length doesn't match claimed size"
    );
}
