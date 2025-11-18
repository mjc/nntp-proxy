//! Tests for metrics/collector.rs module
//!
//! Tests lock-free metrics collection, recording, and snapshot generation.

use nntp_proxy::constants::user::ANONYMOUS;
use nntp_proxy::metrics::{HealthStatus, MetricsCollector};
use nntp_proxy::types::{BackendId, MetricsBytes, Unrecorded};

/// Test MetricsCollector creation
#[test]
fn test_metrics_collector_new() {
    let collector = MetricsCollector::new(3);
    assert_eq!(collector.num_backends(), 3);

    let snapshot = collector.snapshot();
    assert_eq!(snapshot.backend_stats.len(), 3);
}

/// Test connection lifecycle tracking
#[test]
fn test_connection_lifecycle() {
    let collector = MetricsCollector::new(1);

    // Initially zero
    let snapshot = collector.snapshot();
    assert_eq!(snapshot.total_connections, 0);
    assert_eq!(snapshot.active_connections, 0);

    // Open connection
    collector.connection_opened();
    let snapshot = collector.snapshot();
    assert_eq!(snapshot.total_connections, 1);
    assert_eq!(snapshot.active_connections, 1);

    // Open another
    collector.connection_opened();
    let snapshot = collector.snapshot();
    assert_eq!(snapshot.total_connections, 2);
    assert_eq!(snapshot.active_connections, 2);

    // Close one
    collector.connection_closed();
    let snapshot = collector.snapshot();
    assert_eq!(snapshot.total_connections, 2); // Total stays
    assert_eq!(snapshot.active_connections, 1); // Active decrements

    // Close another
    collector.connection_closed();
    let snapshot = collector.snapshot();
    assert_eq!(snapshot.total_connections, 2);
    assert_eq!(snapshot.active_connections, 0);
}

/// Test stateful session tracking
#[test]
fn test_stateful_session_tracking() {
    let collector = MetricsCollector::new(1);

    let snapshot = collector.snapshot();
    assert_eq!(snapshot.stateful_sessions, 0);

    // Start session
    collector.stateful_session_started();
    let snapshot = collector.snapshot();
    assert_eq!(snapshot.stateful_sessions, 1);

    // Start another
    collector.stateful_session_started();
    let snapshot = collector.snapshot();
    assert_eq!(snapshot.stateful_sessions, 2);

    // End one
    collector.stateful_session_ended();
    let snapshot = collector.snapshot();
    assert_eq!(snapshot.stateful_sessions, 1);

    // End another
    collector.stateful_session_ended();
    let snapshot = collector.snapshot();
    assert_eq!(snapshot.stateful_sessions, 0);
}

/// Test command recording for backends
#[test]
fn test_record_command() {
    let collector = MetricsCollector::new(2);
    let backend0 = BackendId::from_index(0);
    let backend1 = BackendId::from_index(1);

    // Record commands for backend 0
    collector.record_command(backend0);
    collector.record_command(backend0);
    collector.record_command(backend0);

    // Record commands for backend 1
    collector.record_command(backend1);

    let snapshot = collector.snapshot();
    assert_eq!(snapshot.backend_stats[0].total_commands.get(), 3);
    assert_eq!(snapshot.backend_stats[1].total_commands.get(), 1);
}

/// Test byte transfer recording
#[test]
fn test_byte_transfer_recording() {
    let collector = MetricsCollector::new(1);
    let backend = BackendId::from_index(0);

    collector.record_client_to_backend_bytes_for(backend, 100);
    collector.record_client_to_backend_bytes_for(backend, 200);

    collector.record_backend_to_client_bytes_for(backend, 500);
    collector.record_backend_to_client_bytes_for(backend, 700);

    let snapshot = collector.snapshot();
    assert_eq!(snapshot.backend_stats[0].bytes_sent.as_u64(), 300);
    assert_eq!(snapshot.backend_stats[0].bytes_received.as_u64(), 1200);
}

/// Test type-safe byte recording
#[test]
fn test_type_safe_byte_recording() {
    let collector = MetricsCollector::new(1);

    // Create unrecorded bytes
    let sent = MetricsBytes::<Unrecorded>::new(100);
    let recv = MetricsBytes::<Unrecorded>::new(200);

    // Record them (consumes and returns Recorded)
    let _sent_recorded = collector.record_client_to_backend(sent);
    let _recv_recorded = collector.record_backend_to_client(recv);

    // The returned values are marked as recorded
    // This would fail to compile: collector.record_client_to_backend(_sent_recorded);
}

/// Test record_command_execution convenience method
#[test]
fn test_record_command_execution() {
    let collector = MetricsCollector::new(1);
    let backend = BackendId::from_index(0);

    let sent = MetricsBytes::<Unrecorded>::new(150);
    let recv = MetricsBytes::<Unrecorded>::new(350);

    let (_sent_recorded, _recv_recorded) = collector.record_command_execution(backend, sent, recv);

    let snapshot = collector.snapshot();
    assert_eq!(snapshot.backend_stats[0].total_commands.get(), 1);
    assert_eq!(snapshot.backend_stats[0].bytes_sent.as_u64(), 150);
    assert_eq!(snapshot.backend_stats[0].bytes_received.as_u64(), 350);
}

/// Test error recording
#[test]
fn test_error_recording() {
    let collector = MetricsCollector::new(1);
    let backend = BackendId::from_index(0);

    collector.record_error(backend);
    collector.record_error(backend);

    let snapshot = collector.snapshot();
    assert_eq!(snapshot.backend_stats[0].errors.get(), 2);
}

/// Test 4xx error recording
#[test]
fn test_error_4xx_recording() {
    let collector = MetricsCollector::new(1);
    let backend = BackendId::from_index(0);

    collector.record_error_4xx(backend);
    collector.record_error_4xx(backend);
    collector.record_error_4xx(backend);

    let snapshot = collector.snapshot();
    assert_eq!(snapshot.backend_stats[0].errors_4xx.get(), 3);
    assert_eq!(snapshot.backend_stats[0].errors.get(), 3); // Also increments total
}

/// Test 5xx error recording
#[test]
fn test_error_5xx_recording() {
    let collector = MetricsCollector::new(1);
    let backend = BackendId::from_index(0);

    collector.record_error_5xx(backend);
    collector.record_error_5xx(backend);

    let snapshot = collector.snapshot();
    assert_eq!(snapshot.backend_stats[0].errors_5xx.get(), 2);
    assert_eq!(snapshot.backend_stats[0].errors.get(), 2); // Also increments total
}

/// Test mixed error recording
#[test]
fn test_mixed_error_recording() {
    let collector = MetricsCollector::new(1);
    let backend = BackendId::from_index(0);

    collector.record_error_4xx(backend);
    collector.record_error_4xx(backend);
    collector.record_error_5xx(backend);

    let snapshot = collector.snapshot();
    assert_eq!(snapshot.backend_stats[0].errors_4xx.get(), 2);
    assert_eq!(snapshot.backend_stats[0].errors_5xx.get(), 1);
    assert_eq!(snapshot.backend_stats[0].errors.get(), 3); // Total
}

/// Test article recording
#[test]
fn test_article_recording() {
    let collector = MetricsCollector::new(1);
    let backend = BackendId::from_index(0);

    collector.record_article(backend, 1024);
    collector.record_article(backend, 2048);
    collector.record_article(backend, 4096);

    let snapshot = collector.snapshot();
    assert_eq!(snapshot.backend_stats[0].article_count.get(), 3);
    assert_eq!(snapshot.backend_stats[0].article_bytes_total.get(), 7168);
}

/// Test TTFB (time to first byte) recording
#[test]
fn test_ttfb_recording() {
    let collector = MetricsCollector::new(1);
    let backend = BackendId::from_index(0);

    collector.record_ttfb_micros(backend, 1000);
    collector.record_ttfb_micros(backend, 2000);
    collector.record_ttfb_micros(backend, 3000);

    let snapshot = collector.snapshot();
    assert_eq!(snapshot.backend_stats[0].ttfb_micros_total.get(), 6000);
    assert_eq!(snapshot.backend_stats[0].ttfb_count.get(), 3);
}

/// Test send/recv timing recording
#[test]
fn test_send_recv_timing_recording() {
    let collector = MetricsCollector::new(1);
    let backend = BackendId::from_index(0);

    collector.record_send_recv_micros(backend, 100, 500);
    collector.record_send_recv_micros(backend, 200, 600);

    let snapshot = collector.snapshot();
    assert_eq!(snapshot.backend_stats[0].send_micros_total.get(), 300);
    assert_eq!(snapshot.backend_stats[0].recv_micros_total.get(), 1100);
}

/// Test connection failure recording
#[test]
fn test_connection_failure_recording() {
    let collector = MetricsCollector::new(1);
    let backend = BackendId::from_index(0);

    collector.record_connection_failure(backend);
    collector.record_connection_failure(backend);

    let snapshot = collector.snapshot();
    assert_eq!(snapshot.backend_stats[0].connection_failures.get(), 2);
}

/// Test backend connection lifecycle
#[test]
fn test_backend_connection_lifecycle() {
    let collector = MetricsCollector::new(1);
    let backend = BackendId::from_index(0);

    let snapshot = collector.snapshot();
    assert_eq!(snapshot.backend_stats[0].active_connections.get(), 0);

    collector.backend_connection_opened(backend);
    let snapshot = collector.snapshot();
    assert_eq!(snapshot.backend_stats[0].active_connections.get(), 1);

    collector.backend_connection_opened(backend);
    let snapshot = collector.snapshot();
    assert_eq!(snapshot.backend_stats[0].active_connections.get(), 2);

    collector.backend_connection_closed(backend);
    let snapshot = collector.snapshot();
    assert_eq!(snapshot.backend_stats[0].active_connections.get(), 1);
}

/// Test health status recording
#[test]
fn test_health_status_recording() {
    let collector = MetricsCollector::new(1);
    let backend = BackendId::from_index(0);

    collector.set_backend_health(backend, HealthStatus::Healthy);
    let snapshot = collector.snapshot();
    assert_eq!(
        snapshot.backend_stats[0].health_status,
        HealthStatus::Healthy
    );

    collector.set_backend_health(backend, HealthStatus::Degraded);
    let snapshot = collector.snapshot();
    assert_eq!(
        snapshot.backend_stats[0].health_status,
        HealthStatus::Degraded
    );

    collector.set_backend_health(backend, HealthStatus::Down);
    let snapshot = collector.snapshot();
    assert_eq!(snapshot.backend_stats[0].health_status, HealthStatus::Down);
}

/// Test user metrics: connection opened
#[test]
fn test_user_connection_opened() {
    let collector = MetricsCollector::new(1);

    collector.user_connection_opened(Some("user1"));
    collector.user_connection_opened(Some("user1"));
    collector.user_connection_opened(Some("user2"));

    let snapshot = collector.snapshot();

    let user1 = snapshot
        .user_stats
        .iter()
        .find(|u| u.username == "user1")
        .unwrap();
    assert_eq!(user1.active_connections, 2);
    assert_eq!(user1.total_connections.get(), 2);

    let user2 = snapshot
        .user_stats
        .iter()
        .find(|u| u.username == "user2")
        .unwrap();
    assert_eq!(user2.active_connections, 1);
    assert_eq!(user2.total_connections.get(), 1);
}

/// Test user metrics: connection closed
#[test]
fn test_user_connection_closed() {
    let collector = MetricsCollector::new(1);

    collector.user_connection_opened(Some("user1"));
    collector.user_connection_opened(Some("user1"));

    let snapshot = collector.snapshot();
    let user1 = snapshot
        .user_stats
        .iter()
        .find(|u| u.username == "user1")
        .unwrap();
    assert_eq!(user1.active_connections, 2);

    collector.user_connection_closed(Some("user1"));

    let snapshot = collector.snapshot();
    let user1 = snapshot
        .user_stats
        .iter()
        .find(|u| u.username == "user1")
        .unwrap();
    assert_eq!(user1.active_connections, 1);
    assert_eq!(user1.total_connections.get(), 2); // Total unchanged
}

/// Test anonymous user tracking
#[test]
fn test_anonymous_user_tracking() {
    let collector = MetricsCollector::new(1);

    collector.user_connection_opened(None);
    collector.user_connection_opened(None);

    let snapshot = collector.snapshot();
    let anon = snapshot
        .user_stats
        .iter()
        .find(|u| u.username == ANONYMOUS)
        .unwrap();
    assert_eq!(anon.active_connections, 2);
    assert_eq!(anon.total_connections.get(), 2);
}

/// Test user bytes sent
#[test]
fn test_user_bytes_sent() {
    let collector = MetricsCollector::new(1);

    collector.user_connection_opened(Some("user1"));
    collector.user_bytes_sent(Some("user1"), 1000);
    collector.user_bytes_sent(Some("user1"), 2000);

    let snapshot = collector.snapshot();
    let user1 = snapshot
        .user_stats
        .iter()
        .find(|u| u.username == "user1")
        .unwrap();
    assert_eq!(user1.bytes_sent.as_u64(), 3000);
}

/// Test user bytes received
#[test]
fn test_user_bytes_received() {
    let collector = MetricsCollector::new(1);

    collector.user_connection_opened(Some("user1"));
    collector.user_bytes_received(Some("user1"), 500);
    collector.user_bytes_received(Some("user1"), 700);

    let snapshot = collector.snapshot();
    let user1 = snapshot
        .user_stats
        .iter()
        .find(|u| u.username == "user1")
        .unwrap();
    assert_eq!(user1.bytes_received.as_u64(), 1200);
}

/// Test user command tracking
#[test]
fn test_user_command_tracking() {
    let collector = MetricsCollector::new(1);

    collector.user_connection_opened(Some("user1"));
    collector.user_command(Some("user1"));
    collector.user_command(Some("user1"));
    collector.user_command(Some("user1"));

    let snapshot = collector.snapshot();
    let user1 = snapshot
        .user_stats
        .iter()
        .find(|u| u.username == "user1")
        .unwrap();
    assert_eq!(user1.total_commands.get(), 3);
}

/// Test user error tracking
#[test]
fn test_user_error_tracking() {
    let collector = MetricsCollector::new(1);

    collector.user_connection_opened(Some("user1"));
    collector.user_error(Some("user1"));
    collector.user_error(Some("user1"));

    let snapshot = collector.snapshot();
    let user1 = snapshot
        .user_stats
        .iter()
        .find(|u| u.username == "user1")
        .unwrap();
    assert_eq!(user1.errors.get(), 2);
}

/// Test multiple users simultaneously
#[test]
fn test_multiple_users_simultaneously() {
    let collector = MetricsCollector::new(1);

    // User 1
    collector.user_connection_opened(Some("user1"));
    collector.user_command(Some("user1"));
    collector.user_bytes_sent(Some("user1"), 100);

    // User 2
    collector.user_connection_opened(Some("user2"));
    collector.user_command(Some("user2"));
    collector.user_command(Some("user2"));
    collector.user_bytes_sent(Some("user2"), 200);

    // Anonymous
    collector.user_connection_opened(None);
    collector.user_command(None);

    let snapshot = collector.snapshot();
    assert_eq!(snapshot.user_stats.len(), 3);

    let user1 = snapshot
        .user_stats
        .iter()
        .find(|u| u.username == "user1")
        .unwrap();
    assert_eq!(user1.total_commands.get(), 1);
    assert_eq!(user1.bytes_sent.as_u64(), 100);

    let user2 = snapshot
        .user_stats
        .iter()
        .find(|u| u.username == "user2")
        .unwrap();
    assert_eq!(user2.total_commands.get(), 2);
    assert_eq!(user2.bytes_sent.as_u64(), 200);
}

/// Test snapshot total bytes calculation
#[test]
fn test_snapshot_total_bytes() {
    let collector = MetricsCollector::new(2);
    let backend0 = BackendId::from_index(0);
    let backend1 = BackendId::from_index(1);

    collector.record_client_to_backend_bytes_for(backend0, 100);
    collector.record_backend_to_client_bytes_for(backend0, 200);

    collector.record_client_to_backend_bytes_for(backend1, 300);
    collector.record_backend_to_client_bytes_for(backend1, 400);

    let snapshot = collector.snapshot();
    assert_eq!(snapshot.client_to_backend_bytes.as_u64(), 400);
    assert_eq!(snapshot.backend_to_client_bytes.as_u64(), 600);
}

/// Test uptime tracking
#[test]
fn test_uptime_tracking() {
    let collector = MetricsCollector::new(1);

    std::thread::sleep(std::time::Duration::from_millis(10));

    let snapshot = collector.snapshot();
    assert!(snapshot.uptime.as_millis() >= 10);
}

/// Test concurrent metric updates
#[test]
fn test_concurrent_metric_updates() {
    use std::sync::Arc;
    use std::thread;

    let collector = Arc::new(MetricsCollector::new(1));
    let backend = BackendId::from_index(0);

    let mut handles = vec![];

    for _ in 0..10 {
        let c = collector.clone();
        let handle = thread::spawn(move || {
            for _ in 0..100 {
                c.record_command(backend);
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }

    let snapshot = collector.snapshot();
    assert_eq!(snapshot.backend_stats[0].total_commands.get(), 1000);
}

/// Test MetricsCollector is Send + Sync
#[test]
fn test_metrics_collector_is_send_sync() {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}

    assert_send::<MetricsCollector>();
    assert_sync::<MetricsCollector>();
}

/// Test zero backends
#[test]
fn test_zero_backends() {
    let collector = MetricsCollector::new(0);
    assert_eq!(collector.num_backends(), 0);

    let snapshot = collector.snapshot();
    assert_eq!(snapshot.backend_stats.len(), 0);
}

/// Test large number of backends
#[test]
fn test_large_number_of_backends() {
    let collector = MetricsCollector::new(100);
    assert_eq!(collector.num_backends(), 100);

    let snapshot = collector.snapshot();
    assert_eq!(snapshot.backend_stats.len(), 100);
}

/// Test recording to invalid backend is graceful
#[test]
fn test_invalid_backend_graceful() {
    let collector = MetricsCollector::new(1);
    let invalid_backend = BackendId::from_index(99);

    // These should not panic
    collector.record_command(invalid_backend);
    collector.record_error(invalid_backend);
    collector.record_client_to_backend_bytes_for(invalid_backend, 100);

    // Should have no effect
    let snapshot = collector.snapshot();
    assert_eq!(snapshot.backend_stats[0].total_commands.get(), 0);
}
