//! Tests for caching BODY responses (222) and request-kind matching
//!
//! This test suite validates:
//! 1. BODY (222) responses are cached when `cache_articles=true`
//! 2. Upsert only replaces buffers when new buffer is larger (prevents stub overwrite)
//! 3. Typed cache rendering correctly validates cached response vs request kind
//! 4. Cache serves BODY from cache but fetches full ARTICLE when needed

use anyhow::Result;
use nntp_proxy::cache::{ArticleCache, ArticleEntry};
use nntp_proxy::protocol::RequestKind;
use nntp_proxy::types::{BackendId, MessageId};
use std::time::Duration;

use super::{article_entry, article_response_bytes, assert_serves, body_entry, head_entry};

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
            0.into(),
        )
        .await;

    let cached = cache.get(&msg_id).await.expect("Article should be cached");
    assert_eq!(
        article_response_bytes(&cached, RequestKind::Body, &msg_id).unwrap(),
        full_article.as_bytes()
    );

    // Second upsert: Try to overwrite with stub (53 bytes)
    let stub = b"222 0 <test@example.com>\r\n".to_vec();
    cache
        .upsert(msg_id.clone(), stub.clone(), backend_id, 0.into())
        .await;

    let cached = cache
        .get(&msg_id)
        .await
        .expect("Article should still be cached");
    assert_eq!(
        article_response_bytes(&cached, RequestKind::Body, &msg_id).unwrap(),
        full_article.as_bytes(),
        "Full article should NOT be overwritten by stub"
    );
    assert_ne!(
        article_response_bytes(&cached, RequestKind::Body, &msg_id),
        Some(stub)
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
        .upsert(msg_id.clone(), stub.clone(), backend_id, 0.into())
        .await;

    let cached = cache.get(&msg_id).await.expect("Stub should be cached");
    assert!(
        article_response_bytes(&cached, RequestKind::Body, &msg_id).is_none(),
        "Stub should have no payload to serve"
    );

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
            0.into(),
        )
        .await;

    let cached = cache.get(&msg_id).await.expect("Article should be cached");
    assert_eq!(
        article_response_bytes(&cached, RequestKind::Body, &msg_id).unwrap(),
        full_article.as_bytes(),
        "Full article should replace stub"
    );

    Ok(())
}

#[test]
fn test_serves_request_kind_article_response() {
    assert_serves(
        &article_entry(),
        &[
            (RequestKind::Article, true),
            (RequestKind::Body, true),
            (RequestKind::Head, true),
            (RequestKind::Stat, true),
        ],
    );
}

#[test]
fn test_serves_request_kind_body_response() {
    assert_serves(
        &body_entry(),
        &[
            (RequestKind::Body, true),
            (RequestKind::Article, false),
            (RequestKind::Head, false),
            (RequestKind::Stat, true),
        ],
    );
}

#[test]
fn test_serves_request_kind_head_response() {
    assert_serves(
        &head_entry(),
        &[
            (RequestKind::Head, true),
            (RequestKind::Article, false),
            (RequestKind::Body, false),
            (RequestKind::Stat, true),
        ],
    );
}

#[test]
fn test_response_for_body_kind() {
    let body_response = b"222 0 <test@example.com>\r\nBody\r\n.\r\n".to_vec();
    let entry = ArticleEntry::from_response_bytes(body_response);

    assert_serves(&entry, &[(RequestKind::Body, true)]);
}

#[test]
fn test_is_complete_article_accepts_body_responses() {
    // BODY response (222) with full content should be considered complete
    let body_response = format!("222 0 <test@example.com>\r\n{}\r\n.\r\n", "X".repeat(100));
    let entry = ArticleEntry::from_response_bytes(body_response.as_bytes());

    assert!(
        entry.is_complete_article(),
        "BODY (222) with content should be considered complete"
    );

    // BODY stub should NOT be considered complete
    let stub = b"222 0 <test@example.com>\r\n".to_vec();
    let entry = ArticleEntry::from_response_bytes(stub);

    assert!(
        !entry.is_complete_article(),
        "BODY (222) stub should NOT be considered complete"
    );
}

#[test]
fn test_is_complete_article_rejects_stubs() {
    // Stubs are too small to be complete articles
    let stub_220 = b"220 0 <test@example.com>\r\n".to_vec();
    let entry = ArticleEntry::from_response_bytes(stub_220);
    assert!(
        !entry.is_complete_article(),
        "220 stub should not be complete"
    );

    let stub_222 = b"222 0 <test@example.com>\r\n".to_vec();
    let entry = ArticleEntry::from_response_bytes(stub_222);
    assert!(
        !entry.is_complete_article(),
        "222 stub should not be complete"
    );

    let stub_223 = b"223 0 <test@example.com>\r\n".to_vec();
    let entry = ArticleEntry::from_response_bytes(stub_223);
    assert!(
        !entry.is_complete_article(),
        "223 (STAT) should never be complete"
    );
}

#[test]
fn test_status_code_parsing() {
    let entry_220 = ArticleEntry::from_response_bytes(b"220 0 <test@example.com>\r\nTest\r\n.\r\n");
    assert_eq!(entry_220.status_code().as_u16(), 220);

    let entry_222 = ArticleEntry::from_response_bytes(b"222 0 <test@example.com>\r\nTest\r\n.\r\n");
    assert_eq!(entry_222.status_code().as_u16(), 222);

    let entry_221 = ArticleEntry::from_response_bytes(b"221 0 <test@example.com>\r\nTest\r\n.\r\n");
    assert_eq!(entry_221.status_code().as_u16(), 221);
}
#[test]
fn test_body_article_request_kind_mismatch() {
    let body_response = ArticleEntry::from_response_bytes(
        b"222 0 <test@example.com>\r\nBody content only\r\n.\r\n",
    );
    assert_serves(
        &body_response,
        &[
            (RequestKind::Article, false),
            (RequestKind::Body, true),
            (RequestKind::Head, false),
            (RequestKind::Stat, true),
        ],
    );

    let article_response = ArticleEntry::from_response_bytes(
        b"220 0 <test@example.com>\r\nHeaders\r\n\r\nBody\r\n.\r\n",
    );
    assert_serves(
        &article_response,
        &[
            (RequestKind::Article, true),
            (RequestKind::Body, true),
            (RequestKind::Head, true),
            (RequestKind::Stat, true),
        ],
    );
}
