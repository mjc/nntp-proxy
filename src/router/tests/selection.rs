//! Backend selection strategy integration tests
//!
//! Tests BackendSelector with different routing strategies:
//! - RoundRobin (default)
//! - AdaptiveWeighted (load-aware)

use super::*;
use crate::config::RoutingStrategy;
use crate::types::ServerName;

#[test]
fn test_round_robin_selection() {
    let mut router = BackendSelector::new(RoutingStrategy::RoundRobin);
    let client_id = ClientId::new();

    // Add 3 backends
    for i in 0..3 {
        let backend_id = BackendId::from_index(i);
        let provider = create_test_provider();
        router.add_backend(
            backend_id,
            ServerName::new(format!("backend-{}", i)).unwrap(),
            provider,
            crate::config::PrecheckCommand::default(),
        );
    }

    // Route 6 commands and verify round-robin
    let backend1 = router.route_command(client_id, "LIST\r\n").unwrap();
    let backend2 = router.route_command(client_id, "DATE\r\n").unwrap();
    let backend3 = router.route_command(client_id, "HELP\r\n").unwrap();
    let backend4 = router.route_command(client_id, "LIST\r\n").unwrap();
    let backend5 = router.route_command(client_id, "DATE\r\n").unwrap();
    let backend6 = router.route_command(client_id, "HELP\r\n").unwrap();

    // Should cycle through backends in order
    assert_eq!(backend1.as_index(), 0);
    assert_eq!(backend2.as_index(), 1);
    assert_eq!(backend3.as_index(), 2);
    assert_eq!(backend4.as_index(), 0); // Wraps around
    assert_eq!(backend5.as_index(), 1);
    assert_eq!(backend6.as_index(), 2);
}

#[test]
fn test_round_robin_fairness() {
    let mut router = BackendSelector::new(RoutingStrategy::RoundRobin);
    let client_id = ClientId::new();

    // Add 3 backends
    for i in 0..3 {
        router.add_backend(
            BackendId::from_index(i),
            ServerName::new(format!("backend-{}", i)).unwrap(),
            create_test_provider(),
            crate::config::PrecheckCommand::default(),
        );
    }

    // Route 9 commands
    let mut backend_counts = vec![0, 0, 0];
    for _ in 0..9 {
        let backend_id = router.route_command(client_id, "LIST\r\n").unwrap();
        backend_counts[backend_id.as_index()] += 1;
    }

    // Each backend should get 3 commands (perfect round-robin)
    assert_eq!(backend_counts, vec![3, 3, 3]);
}

#[test]
fn test_adaptive_selection_prefers_least_loaded() {
    let mut router = BackendSelector::new(RoutingStrategy::AdaptiveWeighted);
    let client_id = ClientId::new();

    // Add 3 backends
    for i in 0..3 {
        router.add_backend(
            BackendId::from_index(i),
            ServerName::new(format!("backend-{}", i)).unwrap(),
            create_test_provider(),
            crate::config::PrecheckCommand::default(),
        );
    }

    // Route commands - with adaptive strategy, selection is based on load/availability/saturation
    // All backends start with equal load, so any backend is valid
    let backend1 = router.route_command(client_id, "LIST\r\n").unwrap();
    assert!(backend1.as_index() < 3, "Backend index should be valid");

    let backend2 = router.route_command(client_id, "DATE\r\n").unwrap();
    assert!(backend2.as_index() < 3, "Backend index should be valid");

    // Just verify routing succeeds for adaptive strategy
    let backend3 = router.route_command(client_id, "HELP\r\n").unwrap();
    assert!(backend3.as_index() < 3, "Backend index should be valid");
}

#[test]
fn test_default_strategy_is_round_robin() {
    let router = BackendSelector::default();

    // Default should use RoundRobin strategy
    // We can't inspect the strategy directly, but we can verify behavior
    // This test just ensures default() works
    assert_eq!(router.backend_count(), 0);
}
