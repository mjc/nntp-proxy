use nntp_proxy::metrics::ConnectionStatsAggregator;

#[test]
fn test_single_connection() {
    let aggregator = ConnectionStatsAggregator::new();
    aggregator.record_connection(Some("testuser"), "hybrid");

    assert_eq!(aggregator.get_user_count(), 1);
    assert_eq!(aggregator.get_connection_count("testuser"), Some(1));
}

#[test]
fn test_multiple_connections_same_user() {
    let aggregator = ConnectionStatsAggregator::new();

    for _ in 0..5 {
        aggregator.record_connection(Some("testuser"), "per-command");
    }

    assert_eq!(aggregator.get_user_count(), 1);
    assert_eq!(aggregator.get_connection_count("testuser"), Some(5));
}

#[test]
fn test_multiple_users() {
    let aggregator = ConnectionStatsAggregator::new();

    aggregator.record_connection(Some("user1"), "hybrid");
    aggregator.record_connection(Some("user2"), "hybrid");
    aggregator.record_connection(Some("user1"), "hybrid");

    assert_eq!(aggregator.get_user_count(), 2);
    assert_eq!(aggregator.get_connection_count("user1"), Some(2));
    assert_eq!(aggregator.get_connection_count("user2"), Some(1));
}

#[test]
fn test_anonymous_connections() {
    let aggregator = ConnectionStatsAggregator::new();

    aggregator.record_connection(None, "standard");
    aggregator.record_connection(None, "standard");

    assert_eq!(aggregator.get_user_count(), 1);
    assert_eq!(aggregator.get_connection_count("<anonymous>"), Some(2));
}
