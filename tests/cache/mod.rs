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
