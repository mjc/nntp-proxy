//! Least-loaded selection strategy tests
//!
//! Tests for the least-loaded algorithm that routes to the backend
//! with the fewest pending requests relative to capacity.

use super::*;
use nntp_proxy::config::BackendSelectionStrategy;

#[test]
fn test_least_loaded_basic() {
    let mut selector = BackendSelector::with_strategy(BackendSelectionStrategy::LeastLoaded);

    // Add two backends with equal capacity
    selector.add_backend(
        BackendId::from_index(0),
        ServerName::new("backend0".to_string()).unwrap(),
        create_backend("backend0", 10),
    );
    selector.add_backend(
        BackendId::from_index(1),
        ServerName::new("backend1".to_string()).unwrap(),
        create_backend("backend1", 10),
    );

    // First request should go to backend 0 (both empty, picks first)
    let backend1 = selector.route_command(ClientId::new(), "LIST").unwrap();
    assert_eq!(backend1.as_index(), 0);

    // Second request should go to backend 1 (backend 0 has 1 pending)
    let backend2 = selector.route_command(ClientId::new(), "LIST").unwrap();
    assert_eq!(backend2.as_index(), 1);

    // Third request should go to backend 0 again (both have 1 pending, picks first)
    let backend3 = selector.route_command(ClientId::new(), "LIST").unwrap();
    assert_eq!(backend3.as_index(), 0);

    // Complete a command on backend 0
    selector.complete_command(backend1);

    // Next request should go to backend 0 (now has 1 pending vs backend 1's 1 pending)
    let backend4 = selector.route_command(ClientId::new(), "LIST").unwrap();
    assert_eq!(backend4.as_index(), 0);
}

#[test]
fn test_least_loaded_unequal_capacity() {
    let mut selector = BackendSelector::with_strategy(BackendSelectionStrategy::LeastLoaded);

    // Backend 0: 10 connections (small)
    // Backend 1: 50 connections (large)
    selector.add_backend(
        BackendId::from_index(0),
        ServerName::new("small".to_string()).unwrap(),
        create_backend("small", 10),
    );
    selector.add_backend(
        BackendId::from_index(1),
        ServerName::new("large".to_string()).unwrap(),
        create_backend("large", 50),
    );

    // Route 15 requests
    let mut counts = [0; 2];
    for _ in 0..15 {
        let backend = selector.route_command(ClientId::new(), "LIST").unwrap();
        counts[backend.as_index()] += 1;
    }

    // Backend 1 (larger) should get more requests
    // Expected rough distribution: backend 0 gets ~3, backend 1 gets ~12
    // (ratio should favor the larger backend significantly)
    assert!(
        counts[1] > counts[0],
        "Large backend should get more requests: {:?}",
        counts
    );
    assert!(
        counts[1] >= 10,
        "Large backend should get most requests: {:?}",
        counts
    );
}

#[test]
fn test_least_loaded_respects_pending_counts() {
    let mut selector = BackendSelector::with_strategy(BackendSelectionStrategy::LeastLoaded);

    selector.add_backend(
        BackendId::from_index(0),
        ServerName::new("backend0".to_string()).unwrap(),
        create_backend("backend0", 10),
    );
    selector.add_backend(
        BackendId::from_index(1),
        ServerName::new("backend1".to_string()).unwrap(),
        create_backend("backend1", 10),
    );

    // Route 10 requests - they should distribute evenly (both start at 0)
    for _ in 0..10 {
        selector.route_command(ClientId::new(), "LIST").unwrap();
    }

    // Both backends now have 5 pending each (even distribution)
    assert_eq!(selector.backend_load(BackendId::from_index(0)), Some(5));
    assert_eq!(selector.backend_load(BackendId::from_index(1)), Some(5));

    // Complete 3 commands from backend 0
    for _ in 0..3 {
        selector.complete_command(BackendId::from_index(0));
    }

    // Now backend 0 has 2 pending, backend 1 has 5 pending
    // Next request should go to backend 0 (ratio 2/10 = 0.2 vs 5/10 = 0.5)
    let backend = selector.route_command(ClientId::new(), "LIST").unwrap();
    assert_eq!(
        backend.as_index(),
        0,
        "Should route to less loaded backend (backend 0 with 2 pending vs backend 1 with 5 pending)"
    );
}

#[test]
fn test_least_loaded_single_backend() {
    let mut selector = BackendSelector::with_strategy(BackendSelectionStrategy::LeastLoaded);

    selector.add_backend(
        BackendId::from_index(0),
        ServerName::new("only".to_string()).unwrap(),
        create_backend("only", 10),
    );

    // All requests should go to the only backend
    for _ in 0..20 {
        let backend = selector.route_command(ClientId::new(), "LIST").unwrap();
        assert_eq!(backend.as_index(), 0);
    }
}

#[test]
fn test_least_loaded_load_balancing_fairness() {
    let mut selector = BackendSelector::with_strategy(BackendSelectionStrategy::LeastLoaded);

    // Three backends with equal capacity
    for i in 0..3 {
        selector.add_backend(
            BackendId::from_index(i),
            ServerName::new(format!("backend-{}", i)).unwrap(),
            create_backend(&format!("backend-{}", i), 10),
        );
    }

    // Route 30 requests
    let mut counts = [0; 3];
    for _ in 0..30 {
        let backend = selector.route_command(ClientId::new(), "LIST").unwrap();
        counts[backend.as_index()] += 1;
    }

    // With equal capacity and no completions, should distribute evenly
    // Each backend should get exactly 10 requests (30 / 3 = 10)
    assert_eq!(
        counts,
        [10, 10, 10],
        "Should distribute evenly: {:?}",
        counts
    );
}
