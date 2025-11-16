//! Comprehensive tests for metrics module

use nntp_proxy::metrics::*;
use nntp_proxy::types::BackendBytes;
use std::time::Duration;

#[test]
fn test_backend_stats_default() {
    let stats = BackendStats::default();
    assert_eq!(stats.backend_id, 0);
    assert_eq!(stats.total_commands.get(), 0);
    assert_eq!(stats.errors.get(), 0);
    assert_eq!(stats.active_connections.get(), 0);
    assert_eq!(stats.connection_failures.get(), 0);
}

#[test]
fn test_backend_stats_average_article_size() {
    let mut stats = BackendStats::default();
    stats.article_count = ArticleCount::new(10);
    stats.article_bytes_total = 1000;

    assert_eq!(stats.average_article_size(), Some(100));

    // Zero articles
    stats.article_count = ArticleCount::new(0);
    assert_eq!(stats.average_article_size(), None);
}

#[test]
fn test_backend_stats_timing_averages() {
    let mut stats = BackendStats::default();
    stats.ttfb_micros_total = TtfbMicros::new(10000);
    stats.send_micros_total = SendMicros::new(500);
    stats.recv_micros_total = RecvMicros::new(9000);
    stats.ttfb_count = 5;

    // 10000 / 5 / 1000 = 2.0ms
    assert_eq!(stats.average_ttfb_ms(), Some(2.0));

    // 500 / 5 / 1000 = 0.1ms
    assert!((stats.average_send_ms().unwrap() - 0.1).abs() < 1e-10);

    // 9000 / 5 / 1000 = 1.8ms
    assert!((stats.average_recv_ms().unwrap() - 1.8).abs() < 1e-10);

    // Overhead = 2.0 - 0.1 - 1.8 = 0.1ms
    assert!((stats.average_overhead_ms().unwrap() - 0.1).abs() < 1e-10);
}

#[test]
fn test_backend_stats_timing_averages_zero_count() {
    let stats = BackendStats::default();

    assert_eq!(stats.average_ttfb_ms(), None);
    assert_eq!(stats.average_send_ms(), None);
    assert_eq!(stats.average_recv_ms(), None);
    assert_eq!(stats.average_overhead_ms(), None);
}

#[test]
fn test_backend_stats_error_rate() {
    let mut stats = BackendStats::default();
    stats.total_commands = CommandCount::new(100);
    stats.errors = ErrorCount::new(6);

    assert_eq!(stats.error_rate_percent(), 6.0);
    assert!(stats.has_high_error_rate());

    // Low error rate
    stats.errors = ErrorCount::new(2);
    assert_eq!(stats.error_rate_percent(), 2.0);
    assert!(!stats.has_high_error_rate());

    // Zero commands
    stats.total_commands = CommandCount::new(0);
    assert_eq!(stats.error_rate_percent(), 0.0);
}

#[test]
fn test_metrics_snapshot_total_bytes() {
    let snapshot = MetricsSnapshot {
        total_connections: 10,
        active_connections: 5,
        stateful_sessions: 2,
        client_to_backend_bytes: BackendBytes::new(1000),
        backend_to_client_bytes: BackendBytes::new(5000),
        uptime: Duration::from_secs(60),
        backend_stats: vec![],
        user_stats: vec![],
    };

    assert_eq!(snapshot.total_bytes(), 6000);
}

#[test]
fn test_metrics_snapshot_throughput() {
    let snapshot = MetricsSnapshot {
        total_connections: 10,
        active_connections: 5,
        stateful_sessions: 2,
        client_to_backend_bytes: BackendBytes::new(2000),
        backend_to_client_bytes: BackendBytes::new(8000),
        uptime: Duration::from_secs(10),
        backend_stats: vec![],
        user_stats: vec![],
    };

    // 10000 bytes / 10 seconds = 1000 bytes/sec
    assert_eq!(snapshot.throughput_bps(), 1000.0);

    // Zero uptime
    let snapshot_zero = MetricsSnapshot {
        uptime: Duration::from_secs(0),
        ..snapshot
    };
    assert_eq!(snapshot_zero.throughput_bps(), 0.0);
}

#[test]
fn test_metrics_snapshot_format_uptime() {
    let snapshot_secs = MetricsSnapshot {
        total_connections: 0,
        active_connections: 0,
        stateful_sessions: 0,
        client_to_backend_bytes: BackendBytes::new(0),
        backend_to_client_bytes: BackendBytes::new(0),
        uptime: Duration::from_secs(45),
        backend_stats: vec![],
        user_stats: vec![],
    };
    assert_eq!(snapshot_secs.format_uptime(), "45s");

    let snapshot_mins = MetricsSnapshot {
        total_connections: 0,
        active_connections: 0,
        stateful_sessions: 0,
        client_to_backend_bytes: BackendBytes::new(0),
        backend_to_client_bytes: BackendBytes::new(0),
        uptime: Duration::from_secs(185),
        backend_stats: vec![],
        user_stats: vec![],
    };
    assert_eq!(snapshot_mins.format_uptime(), "3m 5s");

    let snapshot_hours = MetricsSnapshot {
        total_connections: 0,
        active_connections: 0,
        stateful_sessions: 0,
        client_to_backend_bytes: BackendBytes::new(0),
        backend_to_client_bytes: BackendBytes::new(0),
        uptime: Duration::from_secs(7265),
        backend_stats: vec![],
        user_stats: vec![],
    };
    assert_eq!(snapshot_hours.format_uptime(), "2h 1m 5s");
}

#[test]
fn test_health_status_conversion() {
    assert_eq!(u8::from(HealthStatus::Healthy), 0);
    assert_eq!(u8::from(HealthStatus::Degraded), 1);
    assert_eq!(u8::from(HealthStatus::Down), 2);

    assert_eq!(HealthStatus::from(0), HealthStatus::Healthy);
    assert_eq!(HealthStatus::from(1), HealthStatus::Degraded);
    assert_eq!(HealthStatus::from(2), HealthStatus::Down);
    assert_eq!(HealthStatus::from(99), HealthStatus::Healthy); // Invalid treated as 0
}

#[test]
fn test_backend_stats_with_realistic_values() {
    let mut stats = BackendStats::default();
    stats.backend_id = 0;
    stats.total_commands = CommandCount::new(1000);
    stats.errors = ErrorCount::new(10);
    stats.errors_4xx = ErrorCount::new(7);
    stats.errors_5xx = ErrorCount::new(3);
    stats.bytes_sent = 50000;
    stats.bytes_received = 1000000;
    stats.article_count = ArticleCount::new(50);
    stats.article_bytes_total = 1000000;
    stats.ttfb_micros_total = TtfbMicros::new(500000);
    stats.ttfb_count = 1000;
    stats.send_micros_total = SendMicros::new(50000);
    stats.recv_micros_total = RecvMicros::new(400000);
    stats.connection_failures = FailureCount::new(2);
    stats.health_status = HealthStatus::Healthy;

    // Verify calculations
    assert_eq!(stats.error_rate_percent(), 1.0);
    assert!(!stats.has_high_error_rate());
    assert_eq!(stats.average_article_size(), Some(20000)); // 1MB / 50
    assert!((stats.average_ttfb_ms().unwrap() - 0.5).abs() < 1e-10); // 500000 / 1000 / 1000
    assert!((stats.average_send_ms().unwrap() - 0.05).abs() < 1e-10);
    assert!((stats.average_recv_ms().unwrap() - 0.4).abs() < 1e-10);
    assert!((stats.average_overhead_ms().unwrap() - 0.05).abs() < 1e-10);
}

#[test]
fn test_user_stats_structure() {
    let user_stats = UserStats {
        username: "testuser".to_string(),
        active_connections: 2,
        total_connections: 10,
        bytes_sent: 5000,
        bytes_received: 50000,
        total_commands: 100,
        errors: 2,
        bytes_sent_per_sec: 100,
        bytes_received_per_sec: 1000,
    };

    assert_eq!(user_stats.username, "testuser");
    assert_eq!(user_stats.active_connections, 2);
    assert_eq!(user_stats.total_commands, 100);
}

#[test]
fn test_metrics_snapshot_with_multiple_backends() {
    let stats1 = BackendStats {
        backend_id: 0,
        total_commands: CommandCount::new(100),
        bytes_sent: 1000,
        bytes_received: 10000,
        ..Default::default()
    };

    let stats2 = BackendStats {
        backend_id: 1,
        total_commands: CommandCount::new(50),
        bytes_sent: 500,
        bytes_received: 5000,
        ..Default::default()
    };

    let snapshot = MetricsSnapshot {
        total_connections: 20,
        active_connections: 10,
        stateful_sessions: 5,
        client_to_backend_bytes: BackendBytes::new(1500),
        backend_to_client_bytes: BackendBytes::new(15000),
        uptime: Duration::from_secs(100),
        backend_stats: vec![stats1, stats2],
        user_stats: vec![],
    };

    assert_eq!(snapshot.backend_stats.len(), 2);
    assert_eq!(snapshot.backend_stats[0].backend_id, 0);
    assert_eq!(snapshot.backend_stats[1].backend_id, 1);
    assert_eq!(snapshot.total_bytes(), 16500);
    assert_eq!(snapshot.throughput_bps(), 165.0);
}
