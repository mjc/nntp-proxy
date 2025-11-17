//! Comprehensive tests for UserStats type and methods

use nntp_proxy::metrics::UserStats;
use nntp_proxy::metrics::types::{CommandCount, ErrorCount};
use nntp_proxy::types::{BytesPerSecondRate, BytesReceived, BytesSent, TotalConnections};

#[test]
fn test_user_stats_new() {
    let stats = UserStats::new("testuser");

    assert_eq!(stats.username, "testuser");
    assert_eq!(stats.active_connections, 0);
    assert_eq!(stats.total_connections.get(), 0);
    assert_eq!(stats.bytes_sent.as_u64(), 0);
    assert_eq!(stats.bytes_received.as_u64(), 0);
    assert_eq!(stats.total_commands.get(), 0);
    assert_eq!(stats.errors.get(), 0);
    assert_eq!(stats.bytes_sent_per_sec.get(), 0);
    assert_eq!(stats.bytes_received_per_sec.get(), 0);
}

#[test]
fn test_user_stats_new_with_string() {
    let stats = UserStats::new("alice".to_string());
    assert_eq!(stats.username, "alice");
}

#[test]
fn test_user_stats_default() {
    let stats = UserStats::default();

    assert_eq!(stats.username, "");
    assert_eq!(stats.active_connections, 0);
    assert_eq!(stats.total_connections.get(), 0);
    assert_eq!(stats.bytes_sent.as_u64(), 0);
    assert_eq!(stats.bytes_received.as_u64(), 0);
    assert_eq!(stats.total_commands.get(), 0);
    assert_eq!(stats.errors.get(), 0);
}

#[test]
fn test_total_bytes() {
    let stats = UserStats {
        username: "bob".to_string(),
        bytes_sent: BytesSent::new(1000),
        bytes_received: BytesReceived::new(5000),
        ..Default::default()
    };

    assert_eq!(stats.total_bytes(), 6000);
}

#[test]
fn test_total_bytes_zero() {
    let stats = UserStats::new("charlie");
    assert_eq!(stats.total_bytes(), 0);
}

#[test]
fn test_total_bytes_overflow_protection() {
    let stats = UserStats {
        username: "overflow".to_string(),
        bytes_sent: BytesSent::new(u64::MAX),
        bytes_received: BytesReceived::new(1000),
        ..Default::default()
    };

    // Should saturate instead of wrapping
    assert_eq!(stats.total_bytes(), u64::MAX);
}

#[test]
fn test_total_bytes_per_sec() {
    let stats = UserStats {
        username: "dave".to_string(),
        bytes_sent_per_sec: BytesPerSecondRate::new(500),
        bytes_received_per_sec: BytesPerSecondRate::new(2000),
        ..Default::default()
    };

    assert_eq!(stats.total_bytes_per_sec(), 2500);
}

#[test]
fn test_total_bytes_per_sec_zero() {
    let stats = UserStats::new("eve");
    assert_eq!(stats.total_bytes_per_sec(), 0);
}

#[test]
fn test_total_bytes_per_sec_overflow_protection() {
    let stats = UserStats {
        username: "overflow".to_string(),
        bytes_sent_per_sec: BytesPerSecondRate::new(u64::MAX),
        bytes_received_per_sec: BytesPerSecondRate::new(1000),
        ..Default::default()
    };

    // Should saturate instead of wrapping
    assert_eq!(stats.total_bytes_per_sec(), u64::MAX);
}

#[test]
fn test_error_rate_percent_no_commands() {
    let stats = UserStats {
        username: "frank".to_string(),
        total_commands: CommandCount::new(0),
        errors: ErrorCount::new(5),
        ..Default::default()
    };

    // Zero commands should return 0% error rate (avoid division by zero)
    assert_eq!(stats.error_rate_percent(), 0.0);
}

#[test]
fn test_error_rate_percent_no_errors() {
    let stats = UserStats {
        username: "grace".to_string(),
        total_commands: CommandCount::new(100),
        errors: ErrorCount::new(0),
        ..Default::default()
    };

    assert_eq!(stats.error_rate_percent(), 0.0);
}

#[test]
fn test_error_rate_percent_some_errors() {
    let stats = UserStats {
        username: "henry".to_string(),
        total_commands: CommandCount::new(100),
        errors: ErrorCount::new(5),
        ..Default::default()
    };

    assert_eq!(stats.error_rate_percent(), 5.0);
}

#[test]
fn test_error_rate_percent_all_errors() {
    let stats = UserStats {
        username: "iris".to_string(),
        total_commands: CommandCount::new(50),
        errors: ErrorCount::new(50),
        ..Default::default()
    };

    assert_eq!(stats.error_rate_percent(), 100.0);
}

#[test]
fn test_error_rate_percent_fractional() {
    let stats = UserStats {
        username: "jack".to_string(),
        total_commands: CommandCount::new(300),
        errors: ErrorCount::new(1),
        ..Default::default()
    };

    // 1/300 = 0.333...%
    let rate = stats.error_rate_percent();
    assert!(rate > 0.33 && rate < 0.34);
}

#[test]
fn test_has_activity_no_activity() {
    let stats = UserStats::new("karen");
    assert!(!stats.has_activity());
}

#[test]
fn test_has_activity_with_commands() {
    let stats = UserStats {
        username: "larry".to_string(),
        total_commands: CommandCount::new(1),
        ..Default::default()
    };

    assert!(stats.has_activity());
}

#[test]
fn test_has_activity_with_connections() {
    let stats = UserStats {
        username: "mary".to_string(),
        total_connections: TotalConnections::new(1),
        ..Default::default()
    };

    assert!(stats.has_activity());
}

#[test]
fn test_has_activity_with_both() {
    let stats = UserStats {
        username: "nancy".to_string(),
        total_commands: CommandCount::new(10),
        total_connections: TotalConnections::new(2),
        ..Default::default()
    };

    assert!(stats.has_activity());
}

#[test]
fn test_is_connected_no_connections() {
    let stats = UserStats::new("oscar");
    assert!(!stats.is_connected());
}

#[test]
fn test_is_connected_with_active_connection() {
    let stats = UserStats {
        username: "paul".to_string(),
        active_connections: 1,
        ..Default::default()
    };

    assert!(stats.is_connected());
}

#[test]
fn test_is_connected_with_multiple_connections() {
    let stats = UserStats {
        username: "quinn".to_string(),
        active_connections: 5,
        ..Default::default()
    };

    assert!(stats.is_connected());
}

#[test]
fn test_is_connected_past_connections_only() {
    let stats = UserStats {
        username: "rachel".to_string(),
        active_connections: 0,
        total_connections: TotalConnections::new(10), // Past connections
        ..Default::default()
    };

    assert!(!stats.is_connected());
}

#[test]
fn test_user_stats_clone() {
    let stats1 = UserStats {
        username: "sam".to_string(),
        active_connections: 3,
        total_connections: TotalConnections::new(10),
        bytes_sent: BytesSent::new(5000),
        bytes_received: BytesReceived::new(50000),
        total_commands: CommandCount::new(100),
        errors: ErrorCount::new(2),
        bytes_sent_per_sec: BytesPerSecondRate::new(100),
        bytes_received_per_sec: BytesPerSecondRate::new(1000),
    };

    let stats2 = stats1.clone();

    assert_eq!(stats2.username, stats1.username);
    assert_eq!(stats2.active_connections, stats1.active_connections);
    assert_eq!(stats2.total_connections, stats1.total_connections);
    assert_eq!(stats2.bytes_sent, stats1.bytes_sent);
    assert_eq!(stats2.bytes_received, stats1.bytes_received);
    assert_eq!(stats2.total_commands, stats1.total_commands);
    assert_eq!(stats2.errors, stats1.errors);
    assert_eq!(stats2.bytes_sent_per_sec, stats1.bytes_sent_per_sec);
    assert_eq!(stats2.bytes_received_per_sec, stats1.bytes_received_per_sec);
}

#[test]
fn test_realistic_user_stats() {
    let stats = UserStats {
        username: "prod_user_123".to_string(),
        active_connections: 2,
        total_connections: TotalConnections::new(50),
        bytes_sent: BytesSent::new(1_024_000), // ~1 MB sent
        bytes_received: BytesReceived::new(50_240_000), // ~50 MB received
        total_commands: CommandCount::new(1000),
        errors: ErrorCount::new(3),
        bytes_sent_per_sec: BytesPerSecondRate::new(10_240), // ~10 KB/s
        bytes_received_per_sec: BytesPerSecondRate::new(512_000), // ~500 KB/s
    };

    // Validate calculations with realistic values
    assert_eq!(stats.total_bytes(), 51_264_000); // ~51 MB total
    assert_eq!(stats.total_bytes_per_sec(), 522_240); // ~522 KB/s
    assert!(stats.error_rate_percent() < 1.0); // < 1% error rate
    assert!(stats.has_activity());
    assert!(stats.is_connected());
}
