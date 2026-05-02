//! Caching-specific feature tests
//!
//! This module contains tests for unified article caching, cache strategies,
//! tiered TTL, adaptive prechecking, and cache integration with hybrid mode.

use futures::executor::block_on;
use nntp_proxy::cache::ArticleEntry;
use nntp_proxy::types::MessageId;

pub mod adaptive_precheck;
pub mod article_availability;
pub mod availability_only;
pub mod body_responses;
pub mod bug_430_articles;
pub mod bug_430_caching;
pub mod cache_before_precheck;
pub mod helpers;
pub mod hybrid_integration;
pub mod racing_precheck;
pub mod tiered_ttl;
pub mod unified_cache;

pub fn article_response_bytes(
    entry: &ArticleEntry,
    verb: &[u8],
    message_id: &MessageId<'_>,
) -> Option<Vec<u8>> {
    let response = entry.response_parts_for_command_bytes(verb, message_id.as_str())?;
    let mut out = Vec::with_capacity(response.wire_len().get());
    block_on(response.write_to(&mut out)).ok()?;
    Some(out)
}

pub fn test_msg_id() -> MessageId<'static> {
    MessageId::from_borrowed("<test@example.com>").unwrap()
}

pub fn article_entry() -> ArticleEntry {
    ArticleEntry::from_wire_response(
        b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n",
    )
}

pub fn body_entry() -> ArticleEntry {
    ArticleEntry::from_wire_response(b"222 0 <test@example.com>\r\nBody content\r\n.\r\n")
}

pub fn head_entry() -> ArticleEntry {
    ArticleEntry::from_wire_response(b"221 0 <test@example.com>\r\nSubject: Test\r\n.\r\n")
}

pub fn response_bytes(entry: &ArticleEntry, verb: &[u8]) -> Option<Vec<u8>> {
    article_response_bytes(entry, verb, &test_msg_id())
}

pub fn assert_serves(entry: &ArticleEntry, cases: &[(&[u8], bool)]) {
    for (verb, expected) in cases {
        assert_eq!(
            response_bytes(entry, verb).is_some(),
            *expected,
            "serve decision for {}",
            String::from_utf8_lossy(verb)
        );
    }
}

pub fn assert_article_response(entry: &ArticleEntry, verb: &[u8], expected: &[u8]) {
    assert_eq!(response_bytes(entry, verb).as_deref(), Some(expected));
}

pub fn assert_no_article_response(entry: &ArticleEntry, verb: &[u8]) {
    assert_eq!(response_bytes(entry, verb), None);
}
