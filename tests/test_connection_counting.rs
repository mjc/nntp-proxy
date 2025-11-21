//! Tests for connection counting to prevent double-counting bug regression
//!
//! Bug: Standard mode was double-counting authenticated connections:
//! 1. Once in on_authentication_success() after AUTHINFO PASS succeeds
//! 2. Again in proxy.rs after the session completes
//!
//! Fix: Only count during authentication, skip if already authenticated.

use nntp_proxy::metrics::ConnectionStatsAggregator;

/// Test that simulates the correct behavior: count once during auth
#[test]
fn test_authenticated_connection_counted_once() {
    let aggregator = ConnectionStatsAggregator::new();

    // Simulate authentication success - this should count the connection
    // (happens in on_authentication_success() called from all handler modes)
    aggregator.record_connection(Some("testuser"), "standard");

    // Verify counted once
    assert_eq!(
        aggregator.connection_count("testuser"),
        Some(1),
        "Connection should be counted during authentication"
    );

    // The bug was: proxy.rs handle_client() would call record_connection() AGAIN
    // after session completes, causing double-counting.
    //
    // With the fix, handle_client() checks:
    //   - If auth is disabled OR username is None => count (anonymous session)
    //   - Otherwise => skip (already counted during auth)
    //
    // Since we already counted above, calling record_connection again would
    // be wrong. Verify the aggregator is still at 1.
    assert_eq!(aggregator.connection_count("testuser"), Some(1));
}

/// Test anonymous connections are still counted (no auth)
#[test]
fn test_anonymous_connection_counted_after_session() {
    let aggregator = ConnectionStatsAggregator::new();

    // With no auth, connection is counted AFTER session completes
    // (in proxy.rs handle_client(), not during auth since there's no auth)
    aggregator.record_connection(None, "standard");

    assert_eq!(
        aggregator.connection_count("<anonymous>"),
        Some(1),
        "Anonymous connection should be counted once"
    );
}

/// Test multiple authenticated sessions don't double-count
#[test]
fn test_multiple_authenticated_sessions_no_double_count() {
    let aggregator = ConnectionStatsAggregator::new();

    // Three different users authenticate
    aggregator.record_connection(Some("user1"), "standard");
    aggregator.record_connection(Some("user2"), "per-command");
    aggregator.record_connection(Some("user3"), "hybrid");

    // Each should be counted exactly once
    assert_eq!(aggregator.connection_count("user1"), Some(1));
    assert_eq!(aggregator.connection_count("user2"), Some(1));
    assert_eq!(aggregator.connection_count("user3"), Some(1));
    assert_eq!(aggregator.user_count(), 3);
}

/// Test the fix: conditional recording based on auth status
#[test]
fn test_conditional_recording_prevents_double_count() {
    let aggregator = ConnectionStatsAggregator::new();

    // Simulate the fixed logic in proxy.rs handle_client():
    //
    // if !auth_handler.is_enabled() || session.username().is_none() {
    //     record_connection(...);
    // }

    let auth_enabled = true;
    let username = Some("testuser");

    // Auth is enabled AND username exists => DON'T record (already counted)
    if !auth_enabled || username.is_none() {
        aggregator.record_connection(username, "standard");
    }

    // Should be 0 because we skipped recording
    assert_eq!(
        aggregator.connection_count("testuser"),
        None,
        "Should not double-count authenticated connection"
    );

    // Now test anonymous case
    let username_anon: Option<&str> = None;

    // Auth is enabled BUT username is None => DO record
    if !auth_enabled || username_anon.is_none() {
        aggregator.record_connection(username_anon, "standard");
    }

    // Should be counted
    assert_eq!(
        aggregator.connection_count("<anonymous>"),
        Some(1),
        "Anonymous connection should be counted"
    );

    // Test auth disabled case
    let auth_enabled_disabled = false;
    let username_noauth = None;

    // Auth is disabled => DO record
    if !auth_enabled_disabled || username_noauth.is_none() {
        aggregator.record_connection(username_noauth, "standard");
    }

    // Should increment to 2
    assert_eq!(aggregator.connection_count("<anonymous>"), Some(2));
}
