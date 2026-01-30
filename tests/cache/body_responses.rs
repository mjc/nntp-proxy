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
    let cache = ArticleCache::new(1_000_000, Duration::from_secs(300), true);
    let msg_id = MessageId::from_str_or_wrap("test@example.com")?;
    let backend_id = BackendId::from_index(0);

    // First upsert: Store full article (750KB)
    let full_article = format!(
        "222 0 <test@example.com>\r\n{}\r\n.\r\n",
        "X".repeat(750_000)
    );
    cache
        .upsert(
            msg_id.clone(),
            full_article.as_bytes().to_vec(),
            backend_id,
            0,
        )
        .await;

    let cached = cache.get(&msg_id).await.expect("Article should be cached");
    assert_eq!(
        cached.buffer().len(),
        full_article.len(),
        "Full article should be cached"
    );

    // Second upsert: Try to overwrite with stub (53 bytes)
    let stub = b"222 0 <test@example.com>\r\n".to_vec();
    cache
        .upsert(msg_id.clone(), stub.clone(), backend_id, 0)
        .await;

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
    let cache = ArticleCache::new(1_000_000, Duration::from_secs(300), true);
    let msg_id = MessageId::from_str_or_wrap("test@example.com")?;
    let backend_id = BackendId::from_index(0);

    // First upsert: Store stub
    let stub = b"222 0 <test@example.com>\r\n".to_vec();
    cache
        .upsert(msg_id.clone(), stub.clone(), backend_id, 0)
        .await;

    let cached = cache.get(&msg_id).await.expect("Stub should be cached");
    assert_eq!(cached.buffer().len(), stub.len(), "Stub should be cached");

    // Second upsert: Replace with full article (larger)
    let full_article = format!(
        "222 0 <test@example.com>\r\n{}\r\n.\r\n",
        "X".repeat(750_000)
    );
    cache
        .upsert(
            msg_id.clone(),
            full_article.as_bytes().to_vec(),
            backend_id,
            0,
        )
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
    let msg_id = "<test@example.com>";

    // ARTICLE response can serve ARTICLE, BODY, HEAD, or STAT requests
    assert!(
        entry.response_for_command("ARTICLE", msg_id).is_some(),
        "ARTICLE (220) should match ARTICLE command"
    );
    assert!(
        entry.response_for_command("BODY", msg_id).is_some(),
        "ARTICLE (220) should match BODY command"
    );
    assert!(
        entry.response_for_command("HEAD", msg_id).is_some(),
        "ARTICLE (220) should match HEAD command"
    );
    assert!(
        entry.response_for_command("STAT", msg_id).is_some(),
        "ARTICLE (220) should match STAT command"
    );
}

#[test]
fn test_matches_command_type_body_response() {
    // Create BODY response (222)
    let body_response = b"222 0 <test@example.com>\r\nBody content\r\n.\r\n".to_vec();
    let entry = ArticleEntry::new(body_response);
    let msg_id = "<test@example.com>";

    // BODY response can serve BODY and STAT requests
    assert!(
        entry.response_for_command("BODY", msg_id).is_some(),
        "BODY (222) should match BODY command"
    );
    assert!(
        entry.response_for_command("ARTICLE", msg_id).is_none(),
        "BODY (222) should NOT match ARTICLE command"
    );
    assert!(
        entry.response_for_command("HEAD", msg_id).is_none(),
        "BODY (222) should NOT match HEAD command"
    );
    assert!(
        entry.response_for_command("STAT", msg_id).is_some(),
        "BODY (222) should match STAT command (article exists)"
    );
}

#[test]
fn test_matches_command_type_head_response() {
    // Create HEAD response (221)
    let head_response = b"221 0 <test@example.com>\r\nSubject: Test\r\n.\r\n".to_vec();
    let entry = ArticleEntry::new(head_response);
    let msg_id = "<test@example.com>";

    // HEAD response can serve HEAD and STAT requests
    assert!(
        entry.response_for_command("HEAD", msg_id).is_some(),
        "HEAD (221) should match HEAD command"
    );
    assert!(
        entry.response_for_command("ARTICLE", msg_id).is_none(),
        "HEAD (221) should NOT match ARTICLE command"
    );
    assert!(
        entry.response_for_command("BODY", msg_id).is_none(),
        "HEAD (221) should NOT match BODY command"
    );
    assert!(
        entry.response_for_command("STAT", msg_id).is_some(),
        "HEAD (221) should match STAT command (article exists)"
    );
}

#[test]
fn test_response_for_command_verbs_uppercase() {
    let body_response = b"222 0 <test@example.com>\r\nBody\r\n.\r\n".to_vec();
    let entry = ArticleEntry::new(body_response);
    let msg_id = "<test@example.com>";

    // Only uppercase verbs are expected (caller is responsible for uppercasing)
    assert!(entry.response_for_command("BODY", msg_id).is_some());
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
#[test]
fn test_body_article_command_type_mismatch() {
    // When a BODY (222) response is cached and a client requests ARTICLE (220),
    // it should NOT match because BODY doesn't include headers
    let body_response =
        ArticleEntry::new(b"222 0 <test@example.com>\r\nBody content only\r\n.\r\n".to_vec());
    let msg_id = "<test@example.com>";

    // BODY response should NOT match ARTICLE request (no headers)
    assert!(
        body_response
            .response_for_command("ARTICLE", msg_id)
            .is_none(),
        "ARTICLE command should not match BODY (222) response"
    );

    // But BODY response should match BODY request
    assert!(
        body_response.response_for_command("BODY", msg_id).is_some(),
        "BODY command should match BODY (222) response"
    );

    // And should not match HEAD request (no body)
    assert!(
        body_response.response_for_command("HEAD", msg_id).is_none(),
        "HEAD command should not match BODY (222) response"
    );

    // STAT should work - we know the article exists
    assert!(
        body_response.response_for_command("STAT", msg_id).is_some(),
        "STAT should match BODY (222) response (article exists)"
    );

    // ARTICLE (220) response matches all three
    let article_response =
        ArticleEntry::new(b"220 0 <test@example.com>\r\nHeaders\r\n\r\nBody\r\n.\r\n".to_vec());
    assert!(
        article_response
            .response_for_command("ARTICLE", msg_id)
            .is_some(),
        "ARTICLE command should match ARTICLE (220) response"
    );
    assert!(
        article_response
            .response_for_command("BODY", msg_id)
            .is_some(),
        "BODY command should match ARTICLE (220) response"
    );
    assert!(
        article_response
            .response_for_command("HEAD", msg_id)
            .is_some(),
        "HEAD command should match ARTICLE (220) response"
    );
    assert!(
        article_response
            .response_for_command("STAT", msg_id)
            .is_some(),
        "STAT should match ARTICLE (220) response"
    );
}
