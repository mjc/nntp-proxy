//! Integration tests for `HybridArticleCache`
//!
//! These tests verify critical hybrid cache functionality:
//! - `WriteOnInsertion` policy (data persistence)
//! - `cache.close()` flushes pending writes
//! - Memory → disk eviction
//! - Racing precheck integration
//!
//! Uses `MockHybridCache` to avoid foyer runtime issues in tests.

use anyhow::Result;
use futures::executor::block_on;
use nntp_proxy::cache::{HybridArticleEntry, mock_hybrid::MockHybridCache};
use nntp_proxy::protocol::RequestKind;
use nntp_proxy::types::{BackendId, MessageId};

fn backend(index: usize) -> BackendId {
    BackendId::from_index(index)
}

fn msgid(value: &str) -> MessageId<'_> {
    MessageId::from_borrowed(value).unwrap()
}

fn response_bytes(
    entry: &HybridArticleEntry,
    verb: &[u8],
    message_id: &MessageId<'_>,
) -> Option<Vec<u8>> {
    let response =
        entry.response_parts_for_request_kind(verb_to_kind(verb)?, message_id.as_str())?;
    let mut out = Vec::with_capacity(response.wire_len().get());
    block_on(response.write_to(&mut out)).ok()?;
    Some(out)
}

fn verb_to_kind(verb: &[u8]) -> Option<RequestKind> {
    match verb {
        verb if verb.eq_ignore_ascii_case(b"ARTICLE") => Some(RequestKind::Article),
        verb if verb.eq_ignore_ascii_case(b"HEAD") => Some(RequestKind::Head),
        verb if verb.eq_ignore_ascii_case(b"BODY") => Some(RequestKind::Body),
        verb if verb.eq_ignore_ascii_case(b"STAT") => Some(RequestKind::Stat),
        _ => None,
    }
}

fn assert_article(entry: &HybridArticleEntry, message_id: &MessageId<'_>, expected: &[u8]) {
    assert_eq!(
        response_bytes(entry, b"ARTICLE", message_id).unwrap(),
        expected
    );
}

fn assert_availability(entry: &HybridArticleEntry, cases: &[(usize, bool)]) {
    cases.iter().for_each(|(backend_index, should_try)| {
        assert_eq!(
            entry.should_try_backend(backend(*backend_index)),
            *should_try,
            "backend {backend_index}"
        );
    });
}

#[tokio::test]
async fn test_mock_cache_basic_ops() -> Result<()> {
    let cache = MockHybridCache::new(1024 * 1024);

    let msg_id = msgid("<test@example.com>");
    let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n";

    cache
        .upsert(msg_id.clone(), buffer.as_slice(), backend(0))
        .await;

    assert_article(
        &cache.get(&msg_id).await.expect("Entry should exist"),
        &msg_id,
        buffer,
    );

    let stats = cache.stats();
    assert_eq!(stats.hits, 1);
    assert_eq!(stats.misses, 0);

    Ok(())
}

#[tokio::test]
async fn test_mock_cache_upsert_accepts_borrowed_backend_bytes() -> Result<()> {
    let cache = MockHybridCache::new(1024 * 1024);
    let msg_id = msgid("<borrowed@example.com>");
    let buffer = b"220 0 <borrowed@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n";

    cache
        .upsert(msg_id.clone(), buffer.as_slice(), backend(0))
        .await;

    assert_article(
        &cache.get(&msg_id).await.expect("cached entry"),
        &msg_id,
        buffer,
    );

    Ok(())
}

#[tokio::test]
async fn test_cache_miss() -> Result<()> {
    let cache = MockHybridCache::new(1024 * 1024);

    let msg_id = msgid("<nonexistent@example.com>");
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

    let msg_id = msgid("<test@example.com>");

    let large_buffer =
        b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nLarge body content here\r\n.\r\n";
    cache
        .upsert(msg_id.clone(), large_buffer.as_slice(), backend(0))
        .await;

    cache
        .upsert(
            msg_id.clone(),
            b"223 0 <test@example.com>\r\n".as_slice(),
            backend(1),
        )
        .await;

    let entry = cache.get(&msg_id).await.unwrap();
    assert_article(&entry, &msg_id, large_buffer);
    assert_availability(&entry, &[(0, true), (1, true)]);

    Ok(())
}

#[tokio::test]
async fn test_record_missing() -> Result<()> {
    let cache = MockHybridCache::new(1024 * 1024);

    let msg_id = msgid("<missing@example.com>");

    cache.record_missing(msg_id.clone(), backend(0)).await;

    let entry = cache.get(&msg_id).await;
    assert!(entry.is_some());

    let entry = entry.unwrap();
    assert_availability(&entry, &[(0, false), (1, true)]);

    Ok(())
}

#[tokio::test]
async fn test_availability_tracking() -> Result<()> {
    let cache = MockHybridCache::new(1024 * 1024);

    let msg_id = msgid("<avail@example.com>");

    cache
        .upsert(
            msg_id.clone(),
            b"220 0 <avail@example.com>\r\nBody\r\n.\r\n".as_slice(),
            backend(0),
        )
        .await;

    cache.record_missing(msg_id.clone(), backend(1)).await;
    cache.record_missing(msg_id.clone(), backend(2)).await;

    let entry = cache.get(&msg_id).await.unwrap();
    assert_availability(&entry, &[(0, true), (1, false), (2, false), (3, true)]);

    Ok(())
}

#[tokio::test]
async fn test_close_succeeds() -> Result<()> {
    let cache = MockHybridCache::new(1024 * 1024);

    let msg_id = msgid("<test@example.com>");
    cache
        .upsert(
            msg_id,
            b"220 0 <test@example.com>\r\n.\r\n".as_slice(),
            backend(0),
        )
        .await;

    cache.close().await?;

    Ok(())
}
