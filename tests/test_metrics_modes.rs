//! Integration tests for metrics in different routing modes

use nntp_proxy::config::RoutingMode;
use nntp_proxy::metrics::MetricsCollector;
use nntp_proxy::pool::{ConnectionProvider, DeadpoolConnectionProvider};
use nntp_proxy::router::BackendSelector;
use nntp_proxy::types::{BackendId, BytesReceived, BytesSent, ServerName};

#[test]
fn test_metrics_with_pool_status_standard_mode() {
    // Standard mode: 1:1 client-to-backend mapping
    // Metrics should show:
    // - bytes_sent/received (recorded at session end)
    // - active_connections (from pool utilization)
    // - total_commands = 0 (not parsed in standard mode)

    let metrics = MetricsCollector::new(1);
    let mut router = BackendSelector::new();

    let provider = DeadpoolConnectionProvider::new(
        "localhost".to_string(),
        119,
        "Standard Backend".to_string(),
        10,
        None,
        None,
    );

    router.add_backend(
        BackendId::from_index(0),
        ServerName::try_new("standard-backend".to_string()).unwrap(),
        provider.clone(),
        0, // tier
    );

    // Simulate standard mode behavior: record bytes only
    metrics.record_client_to_backend_bytes_for(BackendId::from(0), 1000);
    metrics.record_backend_to_client_bytes_for(BackendId::from(0), 5000);

    let snapshot = metrics.snapshot(None).with_pool_status(&router);

    // Verify metrics
    assert_eq!(snapshot.backend_stats[0].bytes_sent, BytesSent::new(1000));
    assert_eq!(
        snapshot.backend_stats[0].bytes_received,
        BytesReceived::new(5000)
    );
    assert_eq!(
        snapshot.backend_stats[0].total_commands,
        nntp_proxy::metrics::CommandCount::new(0)
    ); // Not counted in standard mode

    // Pool utilization should work
    let pool_status = provider.status();
    let expected_active = pool_status
        .max_size
        .get()
        .saturating_sub(pool_status.available.get());
    assert_eq!(
        snapshot.backend_stats[0].active_connections,
        nntp_proxy::metrics::ActiveConnections::new(expected_active)
    );
}

#[test]
fn test_metrics_with_pool_status_per_command_mode() {
    // Per-command mode: Each command routed independently
    // Metrics should show:
    // - bytes_sent/received (recorded per command)
    // - total_commands (recorded per command)
    // - active_connections (from pool utilization)

    let metrics = MetricsCollector::new(2);
    let mut router = BackendSelector::new();

    let provider1 = DeadpoolConnectionProvider::new(
        "localhost".to_string(),
        119,
        "Backend 1".to_string(),
        10,
        None,
        None,
    );

    let provider2 = DeadpoolConnectionProvider::new(
        "localhost".to_string(),
        120,
        "Backend 2".to_string(),
        10,
        None,
        None,
    );

    router.add_backend(
        BackendId::from_index(0),
        ServerName::try_new("backend1".to_string()).unwrap(),
        provider1.clone(),
        0, // tier
    );

    router.add_backend(
        BackendId::from_index(1),
        ServerName::try_new("backend2".to_string()).unwrap(),
        provider2.clone(),
        0, // tier
    );

    // Simulate per-command mode: round-robin distribution
    metrics.record_command(BackendId::from(0));
    metrics.record_client_to_backend_bytes_for(BackendId::from(0), 50);
    metrics.record_backend_to_client_bytes_for(BackendId::from(0), 1000);

    metrics.record_command(BackendId::from(1));
    metrics.record_client_to_backend_bytes_for(BackendId::from(1), 50);
    metrics.record_backend_to_client_bytes_for(BackendId::from(1), 1000);

    metrics.record_command(BackendId::from(0));
    metrics.record_client_to_backend_bytes_for(BackendId::from(0), 50);
    metrics.record_backend_to_client_bytes_for(BackendId::from(0), 1000);

    let snapshot = metrics.snapshot(None).with_pool_status(&router);

    // Verify round-robin distribution
    assert_eq!(
        snapshot.backend_stats[0].total_commands,
        nntp_proxy::metrics::CommandCount::new(2)
    );
    assert_eq!(
        snapshot.backend_stats[1].total_commands,
        nntp_proxy::metrics::CommandCount::new(1)
    );

    assert_eq!(snapshot.backend_stats[0].bytes_sent, BytesSent::new(100));
    assert_eq!(snapshot.backend_stats[1].bytes_sent, BytesSent::new(50));

    // Pool utilization should work for both backends
    let status1 = provider1.status();
    let status2 = provider2.status();

    let expected_active1 = status1
        .max_size
        .get()
        .saturating_sub(status1.available.get());
    let expected_active2 = status2
        .max_size
        .get()
        .saturating_sub(status2.available.get());

    assert_eq!(
        snapshot.backend_stats[0].active_connections,
        nntp_proxy::metrics::ActiveConnections::new(expected_active1)
    );
    assert_eq!(
        snapshot.backend_stats[1].active_connections,
        nntp_proxy::metrics::ActiveConnections::new(expected_active2)
    );
}

#[test]
fn test_metrics_with_pool_status_hybrid_mode() {
    // Hybrid mode: Starts per-command, switches to stateful
    // Metrics should show:
    // - bytes_sent/received (both modes)
    // - total_commands (counted)
    // - active_connections (from pool utilization)

    let metrics = MetricsCollector::new(1);
    let mut router = BackendSelector::new();

    let provider = DeadpoolConnectionProvider::new(
        "localhost".to_string(),
        119,
        "Hybrid Backend".to_string(),
        10,
        None,
        None,
    );

    router.add_backend(
        BackendId::from_index(0),
        ServerName::try_new("hybrid-backend".to_string()).unwrap(),
        provider.clone(),
        0, // tier
    );

    // Simulate hybrid mode: start with per-command
    metrics.record_command(BackendId::from(0));
    metrics.record_client_to_backend_bytes_for(BackendId::from(0), 50);
    metrics.record_backend_to_client_bytes_for(BackendId::from(0), 200);

    metrics.record_command(BackendId::from(0));
    metrics.record_client_to_backend_bytes_for(BackendId::from(0), 50);
    metrics.record_backend_to_client_bytes_for(BackendId::from(0), 200);

    // Switch to stateful (GROUP command) - bytes continue accumulating
    metrics.record_command(BackendId::from(0));
    metrics.record_client_to_backend_bytes_for(BackendId::from(0), 100);
    metrics.record_backend_to_client_bytes_for(BackendId::from(0), 5000);

    let snapshot = metrics.snapshot(None).with_pool_status(&router);

    // Verify metrics accumulated across both modes
    assert_eq!(
        snapshot.backend_stats[0].total_commands,
        nntp_proxy::metrics::CommandCount::new(3)
    );
    assert_eq!(snapshot.backend_stats[0].bytes_sent, BytesSent::new(200));
    assert_eq!(
        snapshot.backend_stats[0].bytes_received,
        BytesReceived::new(5400)
    );

    // Pool utilization should work
    let pool_status = provider.status();
    let expected_active = pool_status
        .max_size
        .get()
        .saturating_sub(pool_status.available.get());
    assert_eq!(
        snapshot.backend_stats[0].active_connections,
        nntp_proxy::metrics::ActiveConnections::new(expected_active)
    );
}

#[test]
fn test_all_modes_show_meaningful_metrics() {
    // This test verifies that each routing mode produces useful metrics
    // for monitoring and debugging purposes

    let test_cases = vec![
        (RoutingMode::Stateful, "Standard mode"),
        (RoutingMode::PerCommand, "Per-command mode"),
        (RoutingMode::Hybrid, "Hybrid mode"),
    ];

    for (mode, mode_name) in test_cases {
        let metrics = MetricsCollector::new(1);
        let mut router = BackendSelector::new();

        let provider = DeadpoolConnectionProvider::new(
            "localhost".to_string(),
            119,
            format!("{} Backend", mode_name),
            10,
            None,
            None,
        );

        router.add_backend(
            BackendId::from_index(0),
            ServerName::try_new("test-backend".to_string()).unwrap(),
            provider.clone(),
            0, // tier
        );

        // Record some activity
        metrics.record_client_to_backend_bytes_for(BackendId::from(0), 1000);
        metrics.record_backend_to_client_bytes_for(BackendId::from(0), 5000);

        if matches!(mode, RoutingMode::PerCommand | RoutingMode::Hybrid) {
            // These modes track commands
            metrics.record_command(BackendId::from(0));
        }

        let snapshot = metrics.snapshot(None).with_pool_status(&router);

        // All modes should show:
        // 1. Bytes transferred
        assert_eq!(
            snapshot.backend_stats[0].bytes_sent,
            BytesSent::new(1000),
            "{} should track bytes sent",
            mode_name
        );
        assert_eq!(
            snapshot.backend_stats[0].bytes_received,
            BytesReceived::new(5000),
            "{} should track bytes received",
            mode_name
        );

        // 2. Pool utilization (active connections)
        let pool_status = provider.status();
        let expected_active = pool_status
            .max_size
            .get()
            .saturating_sub(pool_status.available.get());
        assert_eq!(
            snapshot.backend_stats[0].active_connections,
            nntp_proxy::metrics::ActiveConnections::new(expected_active),
            "{} should show pool utilization",
            mode_name
        );

        // 3. Commands (per-command and hybrid only)
        match mode {
            RoutingMode::Stateful => {
                // Standard mode doesn't parse commands
                assert_eq!(
                    snapshot.backend_stats[0].total_commands,
                    nntp_proxy::metrics::CommandCount::new(0)
                );
            }
            RoutingMode::PerCommand | RoutingMode::Hybrid => {
                // These modes count commands
                assert_eq!(
                    snapshot.backend_stats[0].total_commands,
                    nntp_proxy::metrics::CommandCount::new(1)
                );
            }
        }
    }
}
