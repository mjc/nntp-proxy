//! Caching-specific feature tests
//!
//! This module contains tests for unified article caching, cache strategies,
//! tiered TTL, adaptive prechecking, and cache integration with hybrid mode.

use futures::executor::block_on;
use nntp_proxy::cache::ArticleEntry;
use nntp_proxy::protocol::RequestKind;
use nntp_proxy::types::MessageId;

pub mod adaptive_precheck;
pub mod article_availability;
pub mod availability_only;
pub mod body_responses;
pub mod bug_430_articles;
pub mod bug_430_caching;
pub mod cache_before_precheck;
pub mod helpers;
pub mod racing_precheck;
pub mod tiered_ttl;
pub mod unified_cache;

pub fn article_response_bytes(
    entry: &ArticleEntry,
    request_kind: RequestKind,
    message_id: &MessageId<'_>,
) -> Option<Vec<u8>> {
    let response = entry.response_for(request_kind, message_id.as_str())?;
    let mut out = Vec::with_capacity(response.wire_len().get());
    block_on(response.write_to(&mut out)).ok()?;
    Some(out)
}

pub fn test_msg_id() -> MessageId<'static> {
    MessageId::from_borrowed("<test@example.com>").unwrap()
}

pub fn article_entry() -> ArticleEntry {
    ArticleEntry::from_response_bytes(
        b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n",
    )
}

pub fn body_entry() -> ArticleEntry {
    ArticleEntry::from_response_bytes(b"222 0 <test@example.com>\r\nBody content\r\n.\r\n")
}

pub fn head_entry() -> ArticleEntry {
    ArticleEntry::from_response_bytes(b"221 0 <test@example.com>\r\nSubject: Test\r\n.\r\n")
}

pub fn response_bytes(entry: &ArticleEntry, request_kind: RequestKind) -> Option<Vec<u8>> {
    article_response_bytes(entry, request_kind, &test_msg_id())
}

pub fn assert_serves(entry: &ArticleEntry, cases: &[(RequestKind, bool)]) {
    for (request_kind, expected) in cases {
        assert_eq!(
            response_bytes(entry, *request_kind).is_some(),
            *expected,
            "serve decision for {request_kind:?}"
        );
    }
}

pub fn assert_article_response(entry: &ArticleEntry, request_kind: RequestKind, expected: &[u8]) {
    assert_eq!(
        response_bytes(entry, request_kind).as_deref(),
        Some(expected)
    );
}

pub fn assert_no_article_response(entry: &ArticleEntry, request_kind: RequestKind) {
    assert_eq!(response_bytes(entry, request_kind), None);
}
