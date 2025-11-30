//! Weighted round-robin tests
//!
//! Tests for the weighted round-robin algorithm that distributes requests
//! proportionally based on backend pool sizes (max_connections).

use super::*;
use crate::pool::DeadpoolConnectionProvider;
use crate::types::ServerName;

/// Helper to create a test backend with specified max_connections
fn create_backend(name: &str, max_connections: usize) -> DeadpoolConnectionProvider {
    DeadpoolConnectionProvider::builder("localhost", 119)
        .name(name)
        .max_connections(max_connections)
        .build()
        .unwrap()
}

#[test]
fn test_total_weight_accumulation() {
    let mut selector = BackendSelector::new();
    assert_eq!(selector.total_weight(), 0);

    selector.add_backend(
        BackendId::from_index(0),
        ServerName::new("backend1".to_string()).unwrap(),
        create_backend("backend1", 20),
    );
    assert_eq!(selector.total_weight(), 20);

    selector.add_backend(
        BackendId::from_index(1),
        ServerName::new("backend2".to_string()).unwrap(),
        create_backend("backend2", 30),
    );
    assert_eq!(selector.total_weight(), 50);

    selector.add_backend(
        BackendId::from_index(2),
        ServerName::new("backend3".to_string()).unwrap(),
        create_backend("backend3", 50),
    );
    assert_eq!(selector.total_weight(), 100);
}

#[test]
fn test_weighted_distribution_equal_weights() {
    let mut selector = BackendSelector::new();

    // Two backends with equal pool sizes (10 each)
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

    // Total weight = 20
    assert_eq!(selector.total_weight(), 20);

    // Issue 100 requests and count distribution
    let mut counts = [0; 2];
    for _ in 0..100 {
        let backend = selector.route_command(ClientId::new(), "LIST").unwrap();
        counts[backend.as_index()] += 1;
    }

    // Should be roughly 50/50 distribution (within 10% tolerance)
    assert!(
        counts[0] >= 45 && counts[0] <= 55,
        "Backend 0: {}",
        counts[0]
    );
    assert!(
        counts[1] >= 45 && counts[1] <= 55,
        "Backend 1: {}",
        counts[1]
    );
    assert_eq!(counts[0] + counts[1], 100);
}

#[test]
fn test_weighted_distribution_unequal_weights() {
    let mut selector = BackendSelector::new();

    // Backend 0: 40 connections
    // Backend 1: 50 connections
    // Total: 90, so backend 0 gets 40/90 = 44.4%, backend 1 gets 50/90 = 55.6%
    selector.add_backend(
        BackendId::from_index(0),
        ServerName::new("small".to_string()).unwrap(),
        create_backend("small", 40),
    );
    selector.add_backend(
        BackendId::from_index(1),
        ServerName::new("large".to_string()).unwrap(),
        create_backend("large", 50),
    );

    assert_eq!(selector.total_weight(), 90);

    // Issue 900 requests (10x the total weight for good statistical sample)
    let mut counts = [0; 2];
    for _ in 0..900 {
        let backend = selector.route_command(ClientId::new(), "LIST").unwrap();
        counts[backend.as_index()] += 1;
    }

    // Backend 0 should get ~44.4% (400 out of 900)
    // Backend 1 should get ~55.6% (500 out of 900)
    // Allow 5% tolerance
    assert!(
        counts[0] >= 380 && counts[0] <= 420,
        "Backend 0 expected ~400, got {}",
        counts[0]
    );
    assert!(
        counts[1] >= 475 && counts[1] <= 525,
        "Backend 1 expected ~500, got {}",
        counts[1]
    );
    assert_eq!(counts[0] + counts[1], 900);
}

#[test]
fn test_weighted_distribution_three_backends() {
    let mut selector = BackendSelector::new();

    // Backend 0: 10 connections (10/60 = 16.7%)
    // Backend 1: 20 connections (20/60 = 33.3%)
    // Backend 2: 30 connections (30/60 = 50.0%)
    selector.add_backend(
        BackendId::from_index(0),
        ServerName::new("small".to_string()).unwrap(),
        create_backend("small", 10),
    );
    selector.add_backend(
        BackendId::from_index(1),
        ServerName::new("medium".to_string()).unwrap(),
        create_backend("medium", 20),
    );
    selector.add_backend(
        BackendId::from_index(2),
        ServerName::new("large".to_string()).unwrap(),
        create_backend("large", 30),
    );

    assert_eq!(selector.total_weight(), 60);

    // Issue 600 requests (10x total weight)
    let mut counts = [0; 3];
    for _ in 0..600 {
        let backend = selector.route_command(ClientId::new(), "LIST").unwrap();
        counts[backend.as_index()] += 1;
    }

    // Expected: 100, 200, 300 (allow 10% tolerance for smaller samples)
    assert!(
        counts[0] >= 90 && counts[0] <= 110,
        "Backend 0 expected ~100, got {}",
        counts[0]
    );
    assert!(
        counts[1] >= 180 && counts[1] <= 220,
        "Backend 1 expected ~200, got {}",
        counts[1]
    );
    assert!(
        counts[2] >= 270 && counts[2] <= 330,
        "Backend 2 expected ~300, got {}",
        counts[2]
    );
    assert_eq!(counts[0] + counts[1] + counts[2], 600);
}

#[test]
fn test_weighted_real_world_scenario() {
    let mut selector = BackendSelector::new();

    // Simulate the reported issue:
    // usenet.farm: 40 connections
    // NewsDemon: 50 connections
    selector.add_backend(
        BackendId::from_index(0),
        ServerName::new("usenet.farm".to_string()).unwrap(),
        create_backend("usenet.farm", 40),
    );
    selector.add_backend(
        BackendId::from_index(1),
        ServerName::new("NewsDemon".to_string()).unwrap(),
        create_backend("NewsDemon", 50),
    );

    assert_eq!(selector.total_weight(), 90);

    // Simulate 90 concurrent clients each making 10 requests
    let total_requests = 900;
    let mut counts = [0; 2];

    for _ in 0..total_requests {
        let backend = selector
            .route_command(ClientId::new(), "ARTICLE <test@example.com>")
            .unwrap();
        counts[backend.as_index()] += 1;
    }

    // usenet.farm should get 40/90 = 44.4% → ~400 requests
    // NewsDemon should get 50/90 = 55.6% → ~500 requests
    let usenet_farm_expected = 400;
    let newsdemon_expected = 500;

    // Allow 5% tolerance
    assert!(
        counts[0] >= usenet_farm_expected - 20 && counts[0] <= usenet_farm_expected + 20,
        "usenet.farm expected ~{}, got {}",
        usenet_farm_expected,
        counts[0]
    );
    assert!(
        counts[1] >= newsdemon_expected - 25 && counts[1] <= newsdemon_expected + 25,
        "NewsDemon expected ~{}, got {}",
        newsdemon_expected,
        counts[1]
    );
}

#[test]
fn test_weighted_extreme_imbalance() {
    let mut selector = BackendSelector::new();

    // Extreme case: 1 vs 99 connections
    selector.add_backend(
        BackendId::from_index(0),
        ServerName::new("tiny".to_string()).unwrap(),
        create_backend("tiny", 1),
    );
    selector.add_backend(
        BackendId::from_index(1),
        ServerName::new("huge".to_string()).unwrap(),
        create_backend("huge", 99),
    );

    assert_eq!(selector.total_weight(), 100);

    // Issue 1000 requests
    let mut counts = [0; 2];
    for _ in 0..1000 {
        let backend = selector.route_command(ClientId::new(), "LIST").unwrap();
        counts[backend.as_index()] += 1;
    }

    // Tiny should get ~1% (10 out of 1000)
    // Huge should get ~99% (990 out of 1000)
    assert!(
        counts[0] >= 5 && counts[0] <= 15,
        "Tiny backend expected ~10, got {}",
        counts[0]
    );
    assert!(
        counts[1] >= 985 && counts[1] <= 995,
        "Huge backend expected ~990, got {}",
        counts[1]
    );
}

#[test]
fn test_weighted_consistency_across_runs() {
    // Ensure the weighted algorithm produces consistent results across multiple runs
    let mut selector = BackendSelector::new();

    selector.add_backend(
        BackendId::from_index(0),
        ServerName::new("b0".to_string()).unwrap(),
        create_backend("b0", 30),
    );
    selector.add_backend(
        BackendId::from_index(1),
        ServerName::new("b1".to_string()).unwrap(),
        create_backend("b1", 70),
    );

    // Run the test multiple times to ensure consistency
    for run in 0..5 {
        let mut counts = [0; 2];
        for _ in 0..1000 {
            let backend = selector.route_command(ClientId::new(), "LIST").unwrap();
            counts[backend.as_index()] += 1;
        }

        // Each run should produce ~30% and ~70% distribution
        let ratio_0 = counts[0] as f64 / 1000.0;
        let ratio_1 = counts[1] as f64 / 1000.0;

        assert!(
            (ratio_0 - 0.30).abs() < 0.05,
            "Run {}: Backend 0 ratio {:.2} not close to 0.30",
            run,
            ratio_0
        );
        assert!(
            (ratio_1 - 0.70).abs() < 0.05,
            "Run {}: Backend 1 ratio {:.2} not close to 0.70",
            run,
            ratio_1
        );
    }
}

#[test]
fn test_zero_weight_backend_handled() {
    let mut selector = BackendSelector::new();

    // Add a backend with 0 max_connections (edge case)
    selector.add_backend(
        BackendId::from_index(0),
        ServerName::new("zero".to_string()).unwrap(),
        create_backend("zero", 0),
    );

    assert_eq!(selector.total_weight(), 0);

    // Should fail gracefully when routing
    let result = selector.route_command(ClientId::new(), "LIST");
    assert!(result.is_err());
}

#[test]
fn test_mixed_zero_and_nonzero_weights() {
    let mut selector = BackendSelector::new();

    selector.add_backend(
        BackendId::from_index(0),
        ServerName::new("zero".to_string()).unwrap(),
        create_backend("zero", 0),
    );
    selector.add_backend(
        BackendId::from_index(1),
        ServerName::new("normal".to_string()).unwrap(),
        create_backend("normal", 10),
    );

    assert_eq!(selector.total_weight(), 10);

    // All requests should go to the non-zero backend
    for _ in 0..100 {
        let backend = selector.route_command(ClientId::new(), "LIST").unwrap();
        assert_eq!(backend.as_index(), 1);
    }
}

#[test]
fn test_weighted_with_varying_pool_sizes() {
    let mut selector = BackendSelector::new();

    // Real-world diverse pool sizes
    let pool_sizes = vec![5, 10, 15, 20, 25, 30];
    let total: usize = pool_sizes.iter().sum();

    for (i, &size) in pool_sizes.iter().enumerate() {
        selector.add_backend(
            BackendId::from_index(i),
            ServerName::new(format!("backend-{}", i)).unwrap(),
            create_backend(&format!("backend-{}", i), size),
        );
    }

    assert_eq!(selector.total_weight(), total);

    // Issue many requests
    let num_requests = total * 10;
    let mut counts = vec![0; pool_sizes.len()];

    for _ in 0..num_requests {
        let backend = selector.route_command(ClientId::new(), "LIST").unwrap();
        counts[backend.as_index()] += 1;
    }

    // Verify each backend gets approximately its weighted share
    for (i, &size) in pool_sizes.iter().enumerate() {
        let expected = (size as f64 / total as f64) * num_requests as f64;
        let tolerance = expected * 0.10; // 10% tolerance

        assert!(
            (counts[i] as f64 - expected).abs() < tolerance,
            "Backend {} (size {}) expected ~{:.0}, got {} (diff: {:.0})",
            i,
            size,
            expected,
            counts[i],
            (counts[i] as f64 - expected).abs()
        );
    }
}

#[test]
fn test_weighted_single_backend() {
    let mut selector = BackendSelector::new();

    selector.add_backend(
        BackendId::from_index(0),
        ServerName::new("only".to_string()).unwrap(),
        create_backend("only", 42),
    );

    assert_eq!(selector.total_weight(), 42);

    // All requests should go to the only backend
    for _ in 0..100 {
        let backend = selector.route_command(ClientId::new(), "LIST").unwrap();
        assert_eq!(backend.as_index(), 0);
    }
}

#[test]
fn test_weighted_distribution_precision() {
    let mut selector = BackendSelector::new();

    // Use prime numbers to test for modulo bias
    selector.add_backend(
        BackendId::from_index(0),
        ServerName::new("b0".to_string()).unwrap(),
        create_backend("b0", 13),
    );
    selector.add_backend(
        BackendId::from_index(1),
        ServerName::new("b1".to_string()).unwrap(),
        create_backend("b1", 17),
    );

    assert_eq!(selector.total_weight(), 30);

    // Issue exactly total_weight * N requests for perfect distribution test
    let cycles = 10;
    let num_requests = selector.total_weight() * cycles;
    let mut counts = [0; 2];

    for _ in 0..num_requests {
        let backend = selector.route_command(ClientId::new(), "LIST").unwrap();
        counts[backend.as_index()] += 1;
    }

    // With perfect cycling, should get exactly 13N and 17N
    assert_eq!(counts[0], 13 * cycles);
    assert_eq!(counts[1], 17 * cycles);
}
