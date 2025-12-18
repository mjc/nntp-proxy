//! Tests for caching BODY responses (222) and command type matching
//!
//! This test suite validates:
//! 1. BODY (222) responses are cached when cache_articles=true
//! 2. Upsert only replaces buffers when new buffer is larger (prevents stub overwrite)
//! 3. matches_command_type() correctly validates cached response vs command
//! 4. Cache serves BODY from cache but fetches full ARTICLE when needed

use anyhow::Result;
use nntp_proxy::cache::{ArticleCache, ArticleEntry};
use nntp_proxy::types::{BackendId, MessageId};
use std::time::Duration;

// NOTE: Cache policy tests are in src/session/routing/cache_policy.rs unit tests
// as the routing module is private

#[tokio::test]
async fn test_upsert_prevents_stub_overwrite() -> Result<()> {
    let cache = ArticleCache::new(1000, Duration::from_secs(300), true);
    let msg_id = MessageId::from_str_or_wrap("test@example.com")?;
    let backend_id = BackendId::from_index(0);

    // First upsert: Store full article (750KB)
    let full_article = format!(
        "222 0 <test@example.com>\r\n{}\r\n.\r\n",
        "X".repeat(750_000)
    );
    cache
        .upsert(msg_id.clone(), full_article.as_bytes().to_vec(), backend_id)
        .await;

    let cached = cache.get(&msg_id).await.expect("Article should be cached");
    assert_eq!(
        cached.buffer().len(),
        full_article.len(),
        "Full article should be cached"
    );

    // Second upsert: Try to overwrite with stub (53 bytes)
    let stub = b"222 0 <test@example.com>\r\n".to_vec();
    cache.upsert(msg_id.clone(), stub.clone(), backend_id).await;

    let cached = cache
        .get(&msg_id)
        .await
        .expect("Article should still be cached");
    assert_eq!(
        cached.buffer().len(),
        full_article.len(),
        "Full article should NOT be overwritten by stub"
    );
    assert_ne!(
        cached.buffer().len(),
        stub.len(),
        "Stub should not overwrite full article"
    );

    Ok(())
}

#[tokio::test]
async fn test_upsert_allows_larger_buffer_update() -> Result<()> {
    let cache = ArticleCache::new(1000, Duration::from_secs(300), true);
    let msg_id = MessageId::from_str_or_wrap("test@example.com")?;
    let backend_id = BackendId::from_index(0);

    // First upsert: Store stub
    let stub = b"222 0 <test@example.com>\r\n".to_vec();
    cache.upsert(msg_id.clone(), stub.clone(), backend_id).await;

    let cached = cache.get(&msg_id).await.expect("Stub should be cached");
    assert_eq!(cached.buffer().len(), stub.len(), "Stub should be cached");

    // Second upsert: Replace with full article (larger)
    let full_article = format!(
        "222 0 <test@example.com>\r\n{}\r\n.\r\n",
        "X".repeat(750_000)
    );
    cache
        .upsert(msg_id.clone(), full_article.as_bytes().to_vec(), backend_id)
        .await;

    let cached = cache.get(&msg_id).await.expect("Article should be cached");
    assert_eq!(
        cached.buffer().len(),
        full_article.len(),
        "Full article should replace stub"
    );

    Ok(())
}

#[test]
fn test_matches_command_type_article_response() {
    // Create ARTICLE response (220)
    let article_response =
        b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
    let entry = ArticleEntry::new(article_response);

    // ARTICLE response can serve ARTICLE, BODY, or HEAD requests
    assert!(
        entry.matches_command_type("ARTICLE <test@example.com>"),
        "ARTICLE (220) should match ARTICLE command"
    );
    assert!(
        entry.matches_command_type("BODY <test@example.com>"),
        "ARTICLE (220) should match BODY command"
    );
    assert!(
        entry.matches_command_type("HEAD <test@example.com>"),
        "ARTICLE (220) should match HEAD command"
    );
}

#[test]
fn test_matches_command_type_body_response() {
    // Create BODY response (222)
    let body_response = b"222 0 <test@example.com>\r\nBody content\r\n.\r\n".to_vec();
    let entry = ArticleEntry::new(body_response);

    // BODY response can only serve BODY requests
    assert!(
        entry.matches_command_type("BODY <test@example.com>"),
        "BODY (222) should match BODY command"
    );
    assert!(
        !entry.matches_command_type("ARTICLE <test@example.com>"),
        "BODY (222) should NOT match ARTICLE command"
    );
    assert!(
        !entry.matches_command_type("HEAD <test@example.com>"),
        "BODY (222) should NOT match HEAD command"
    );
}

#[test]
fn test_matches_command_type_head_response() {
    // Create HEAD response (221)
    let head_response = b"221 0 <test@example.com>\r\nSubject: Test\r\n.\r\n".to_vec();
    let entry = ArticleEntry::new(head_response);

    // HEAD response can only serve HEAD requests
    assert!(
        entry.matches_command_type("HEAD <test@example.com>"),
        "HEAD (221) should match HEAD command"
    );
    assert!(
        !entry.matches_command_type("ARTICLE <test@example.com>"),
        "HEAD (221) should NOT match ARTICLE command"
    );
    assert!(
        !entry.matches_command_type("BODY <test@example.com>"),
        "HEAD (221) should NOT match BODY command"
    );
}

#[test]
fn test_matches_command_type_case_insensitive() {
    let body_response = b"222 0 <test@example.com>\r\nBody\r\n.\r\n".to_vec();
    let entry = ArticleEntry::new(body_response);

    // Case variations should all work
    assert!(entry.matches_command_type("BODY <test@example.com>"));
    assert!(entry.matches_command_type("body <test@example.com>"));
    assert!(entry.matches_command_type("Body <test@example.com>"));
    assert!(entry.matches_command_type("bOdY <test@example.com>"));
}

#[test]
fn test_is_complete_article_accepts_body_responses() {
    // BODY response (222) with full content should be considered complete
    let body_response = format!("222 0 <test@example.com>\r\n{}\r\n.\r\n", "X".repeat(100));
    let entry = ArticleEntry::new(body_response.as_bytes().to_vec());

    assert!(
        entry.is_complete_article(),
        "BODY (222) with content should be considered complete"
    );

    // BODY stub should NOT be considered complete
    let stub = b"222 0 <test@example.com>\r\n".to_vec();
    let entry = ArticleEntry::new(stub);

    assert!(
        !entry.is_complete_article(),
        "BODY (222) stub should NOT be considered complete"
    );
}

#[test]
fn test_is_complete_article_rejects_stubs() {
    // Stubs are too small to be complete articles
    let stub_220 = b"220 0 <test@example.com>\r\n".to_vec();
    let entry = ArticleEntry::new(stub_220);
    assert!(
        !entry.is_complete_article(),
        "220 stub should not be complete"
    );

    let stub_222 = b"222 0 <test@example.com>\r\n".to_vec();
    let entry = ArticleEntry::new(stub_222);
    assert!(
        !entry.is_complete_article(),
        "222 stub should not be complete"
    );

    let stub_223 = b"223 0 <test@example.com>\r\n".to_vec();
    let entry = ArticleEntry::new(stub_223);
    assert!(
        !entry.is_complete_article(),
        "223 (STAT) should never be complete"
    );
}

#[test]
fn test_status_code_parsing() {
    let entry_220 = ArticleEntry::new(b"220 0 <test@example.com>\r\nTest\r\n.\r\n".to_vec());
    assert_eq!(entry_220.status_code().unwrap().as_u16(), 220);

    let entry_222 = ArticleEntry::new(b"222 0 <test@example.com>\r\nTest\r\n.\r\n".to_vec());
    assert_eq!(entry_222.status_code().unwrap().as_u16(), 222);

    let entry_221 = ArticleEntry::new(b"221 0 <test@example.com>\r\nTest\r\n.\r\n".to_vec());
    assert_eq!(entry_221.status_code().unwrap().as_u16(), 221);
}
