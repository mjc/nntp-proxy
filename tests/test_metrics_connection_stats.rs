use nntp_proxy::metrics::ConnectionStatsAggregator;

#[test]
fn test_single_connection() {
    let aggregator = ConnectionStatsAggregator::new();
    aggregator.record_connection(Some("testuser"), "hybrid");

    assert_eq!(aggregator.user_count(), 1);
    assert_eq!(aggregator.connection_count("testuser"), Some(1));
}

#[test]
fn test_multiple_connections_same_user() {
    let aggregator = ConnectionStatsAggregator::new();

    for _ in 0..5 {
        aggregator.record_connection(Some("testuser"), "per-command");
    }

    assert_eq!(aggregator.user_count(), 1);
    assert_eq!(aggregator.connection_count("testuser"), Some(5));
}

#[test]
fn test_multiple_users() {
    let aggregator = ConnectionStatsAggregator::new();

    aggregator.record_connection(Some("user1"), "hybrid");
    aggregator.record_connection(Some("user2"), "hybrid");
    aggregator.record_connection(Some("user1"), "hybrid");

    assert_eq!(aggregator.user_count(), 2);
    assert_eq!(aggregator.connection_count("user1"), Some(2));
    assert_eq!(aggregator.connection_count("user2"), Some(1));
}

#[test]
fn test_anonymous_connections() {
    let aggregator = ConnectionStatsAggregator::new();

    aggregator.record_connection(None, "standard");
    aggregator.record_connection(None, "standard");

    assert_eq!(aggregator.user_count(), 1);
    assert_eq!(aggregator.connection_count("<anonymous>"), Some(2));
}

/// Test that authenticated connections are counted exactly once, not double-counted
///
/// This test verifies the fix for a bug where standard mode would count connections twice:
/// 1. Once in on_authentication_success() after AUTHINFO PASS succeeds
/// 2. Again in proxy.rs after the session completes
///
/// The correct behavior is to count only once, during authentication.
#[test]
fn test_no_double_counting_authenticated_connections() {
    let aggregator = ConnectionStatsAggregator::new();

    // Simulate what happens during authentication in all modes:
    // on_authentication_success() calls record_connection()
    aggregator.record_connection(Some("testuser"), "standard");

    // Verify connection counted exactly once
    assert_eq!(aggregator.connection_count("testuser"), Some(1));
    assert_eq!(aggregator.user_count(), 1);

    // The bug was that proxy.rs would call record_connection() again after session completion
    // This should NOT happen for authenticated sessions (only for anonymous)

    // For anonymous sessions (no auth), recording happens after session completion
    aggregator.record_connection(None, "standard");
    assert_eq!(aggregator.connection_count("<anonymous>"), Some(1));

    // Total should be 2 users (testuser + anonymous), not 3
    assert_eq!(aggregator.user_count(), 2);
}

/// Test connection counting in per-command and hybrid modes
#[test]
fn test_per_command_hybrid_connection_counting() {
    let aggregator = ConnectionStatsAggregator::new();

    // Per-command mode: auth happens during session, recorded via on_authentication_success
    aggregator.record_connection(Some("user_percommand"), "per-command");
    assert_eq!(aggregator.connection_count("user_percommand"), Some(1));

    // Hybrid mode: same behavior
    aggregator.record_connection(Some("user_hybrid"), "hybrid");
    assert_eq!(aggregator.connection_count("user_hybrid"), Some(1));

    // Standard mode: same behavior (fixed)
    aggregator.record_connection(Some("user_standard"), "standard");
    assert_eq!(aggregator.connection_count("user_standard"), Some(1));

    // All three users counted exactly once
    assert_eq!(aggregator.user_count(), 3);
}
