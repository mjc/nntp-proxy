//! Round-robin selection strategy tests

use super::*;
use crate::types::ServerName;

#[test]
fn test_round_robin_selection() {
    let mut router = BackendSelector::new();
    let client_id = ClientId::new();

    // Add 3 backends with equal pool sizes for predictable distribution
    for i in 0..3 {
        let backend_id = BackendId::from_index(i);
        let provider = create_test_provider(); // Creates backend with 2 connections
        router.add_backend(
            backend_id,
            ServerName::new(format!("backend-{}", i)).unwrap(),
            provider,
        );
    }

    // Total weight = 3 * 2 = 6
    // Each backend should get 1/3 of requests

    // Route 6 commands (one full cycle through the weight)
    let mut backends = Vec::new();
    for _ in 0..6 {
        backends.push(router.route_command(client_id, "LIST\r\n").unwrap());
    }

    // Count distribution
    let mut counts = [0; 3];
    for backend in backends {
        counts[backend.as_index()] += 1;
    }

    // With equal weights, should get exactly 2 requests each in 6 total
    assert_eq!(
        counts,
        [2, 2, 2],
        "Equal weight backends should get equal distribution"
    );
}

#[test]
fn test_load_balancing_fairness() {
    let mut router = BackendSelector::new();
    let client_id = ClientId::new();

    // Add 3 backends with equal weights
    for i in 0..3 {
        router.add_backend(
            BackendId::from_index(i),
            ServerName::new(format!("backend-{}", i)).unwrap(),
            create_test_provider(), // 2 connections each
        );
    }

    // Total weight = 6
    // Route 12 commands (2 full cycles)
    let mut backend_counts = vec![0, 0, 0];
    for _ in 0..12 {
        let backend_id = router.route_command(client_id, "LIST\r\n").unwrap();
        backend_counts[backend_id.as_index()] += 1;
    }

    // Each backend should get exactly 4 commands (12 / 3 = 4)
    assert_eq!(
        backend_counts,
        vec![4, 4, 4],
        "Equal weights should yield equal distribution"
    );
}
