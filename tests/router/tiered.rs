//! Tiered server selection tests
//!
//! Tests for tier-aware backend selection where servers with lower tier numbers
//! are tried first, and higher tier servers only get tried when lower tier ones
//! are exhausted (return 430 - article not found).

use super::*;
use nntp_proxy::cache::ArticleAvailability;
use nntp_proxy::config::BackendSelectionStrategy;

#[test]
fn test_tier_zero_selected_first() {
    let mut selector = BackendSelector::with_strategy(BackendSelectionStrategy::LeastLoaded);

    // Add tier 1 backend first
    selector.add_backend(
        BackendId::from_index(0),
        ServerName::try_new("backup".to_string()).unwrap(),
        create_backend("backup", 10),
        1, // tier 1 (backup)
    );

    // Add tier 0 backend second
    selector.add_backend(
        BackendId::from_index(1),
        ServerName::try_new("primary".to_string()).unwrap(),
        create_backend("primary", 10),
        0, // tier 0 (primary)
    );

    // Create availability to trigger tier filtering (tiering only applies to article requests)
    let availability = ArticleAvailability::new();

    // Despite being added second, tier 0 should be selected first
    let backend = selector
        .route_command_with_availability(ClientId::new(), "ARTICLE", Some(&availability))
        .unwrap();
    assert_eq!(
        backend.as_index(),
        1,
        "Tier 0 backend should be selected first regardless of addition order"
    );
}

#[test]
fn test_tier_escalation_on_430() {
    let mut selector = BackendSelector::with_strategy(BackendSelectionStrategy::LeastLoaded);

    // Add two tier 0 backends and one tier 1 backend
    selector.add_backend(
        BackendId::from_index(0),
        ServerName::try_new("primary-1".to_string()).unwrap(),
        create_backend("primary-1", 10),
        0, // tier 0
    );
    selector.add_backend(
        BackendId::from_index(1),
        ServerName::try_new("primary-2".to_string()).unwrap(),
        create_backend("primary-2", 10),
        0, // tier 0
    );
    selector.add_backend(
        BackendId::from_index(2),
        ServerName::try_new("backup".to_string()).unwrap(),
        create_backend("backup", 10),
        1, // tier 1
    );

    // Simulate both tier 0 backends returning 430
    let mut availability = ArticleAvailability::new();
    availability.record_missing(BackendId::from_index(0));
    availability.record_missing(BackendId::from_index(1));

    // Now routing should select tier 1 backend
    let backend = selector
        .route_command_with_availability(
            ClientId::new(),
            "ARTICLE <test@example.com>",
            Some(&availability),
        )
        .unwrap();
    assert_eq!(
        backend.as_index(),
        2,
        "Should escalate to tier 1 when all tier 0 backends are exhausted"
    );
}

#[test]
fn test_within_tier_load_balancing() {
    let mut selector = BackendSelector::with_strategy(BackendSelectionStrategy::LeastLoaded);

    // Add two backends in tier 0
    selector.add_backend(
        BackendId::from_index(0),
        ServerName::try_new("tier0-a".to_string()).unwrap(),
        create_backend("tier0-a", 10),
        0,
    );
    selector.add_backend(
        BackendId::from_index(1),
        ServerName::try_new("tier0-b".to_string()).unwrap(),
        create_backend("tier0-b", 10),
        0,
    );

    // Add one backend in tier 1 (should not be used)
    selector.add_backend(
        BackendId::from_index(2),
        ServerName::try_new("tier1".to_string()).unwrap(),
        create_backend("tier1", 10),
        1,
    );

    // Route 100 commands - all should go to tier 0 backends
    // Create availability to trigger tier filtering (tiering only applies to article requests)
    let availability = ArticleAvailability::new();
    let mut counts = [0; 3];
    for _ in 0..100 {
        let backend = selector
            .route_command_with_availability(ClientId::new(), "ARTICLE", Some(&availability))
            .unwrap();
        counts[backend.as_index()] += 1;
    }

    // Tier 0 backends should each get ~50 requests (least-loaded balancing)
    assert!(
        counts[0] >= 40 && counts[0] <= 60,
        "tier0-a should get ~50 requests, got {}",
        counts[0]
    );
    assert!(
        counts[1] >= 40 && counts[1] <= 60,
        "tier0-b should get ~50 requests, got {}",
        counts[1]
    );
    // Tier 1 should get nothing
    assert_eq!(
        counts[2], 0,
        "tier1 should not be used when tier0 is available"
    );
}

#[test]
fn test_partial_tier_exhaustion() {
    let mut selector = BackendSelector::with_strategy(BackendSelectionStrategy::LeastLoaded);

    // Add two tier 0 backends
    selector.add_backend(
        BackendId::from_index(0),
        ServerName::try_new("primary-1".to_string()).unwrap(),
        create_backend("primary-1", 10),
        0,
    );
    selector.add_backend(
        BackendId::from_index(1),
        ServerName::try_new("primary-2".to_string()).unwrap(),
        create_backend("primary-2", 10),
        0,
    );

    // Add tier 1 backend
    selector.add_backend(
        BackendId::from_index(2),
        ServerName::try_new("backup".to_string()).unwrap(),
        create_backend("backup", 10),
        1,
    );

    // Mark only one tier 0 backend as missing
    let mut availability = ArticleAvailability::new();
    availability.record_missing(BackendId::from_index(0));

    // Should still select from tier 0 (the remaining one)
    let backend = selector
        .route_command_with_availability(
            ClientId::new(),
            "ARTICLE <test@example.com>",
            Some(&availability),
        )
        .unwrap();
    assert_eq!(
        backend.as_index(),
        1,
        "Should use remaining tier 0 backend, not escalate to tier 1"
    );
}

#[test]
fn test_multiple_tiers() {
    let mut selector = BackendSelector::with_strategy(BackendSelectionStrategy::LeastLoaded);

    // Tier 0: primary
    selector.add_backend(
        BackendId::from_index(0),
        ServerName::try_new("primary".to_string()).unwrap(),
        create_backend("primary", 10),
        0,
    );

    // Tier 1: secondary
    selector.add_backend(
        BackendId::from_index(1),
        ServerName::try_new("secondary".to_string()).unwrap(),
        create_backend("secondary", 10),
        1,
    );

    // Tier 2: tertiary
    selector.add_backend(
        BackendId::from_index(2),
        ServerName::try_new("tertiary".to_string()).unwrap(),
        create_backend("tertiary", 10),
        2,
    );

    // Mark tier 0 and tier 1 as missing
    let mut availability = ArticleAvailability::new();
    availability.record_missing(BackendId::from_index(0));
    availability.record_missing(BackendId::from_index(1));

    // Should escalate to tier 2
    let backend = selector
        .route_command_with_availability(
            ClientId::new(),
            "ARTICLE <test@example.com>",
            Some(&availability),
        )
        .unwrap();
    assert_eq!(
        backend.as_index(),
        2,
        "Should escalate through tiers to reach tier 2"
    );
}

#[test]
fn test_default_tier_is_zero() {
    // When using Server::builder, tier should default to 0
    use nntp_proxy::config::Server;
    use nntp_proxy::types::Port;

    let server = Server::builder("example.com", Port::try_new(119).unwrap())
        .build()
        .unwrap();

    assert_eq!(server.tier, 0, "Default tier should be 0");
}

#[test]
fn test_tiered_weighted_round_robin() {
    let mut selector = BackendSelector::with_strategy(BackendSelectionStrategy::WeightedRoundRobin);

    // Tier 0: two backends with different weights
    selector.add_backend(
        BackendId::from_index(0),
        ServerName::try_new("tier0-small".to_string()).unwrap(),
        create_backend("tier0-small", 10), // weight 10
        0,
    );
    selector.add_backend(
        BackendId::from_index(1),
        ServerName::try_new("tier0-large".to_string()).unwrap(),
        create_backend("tier0-large", 30), // weight 30
        0,
    );

    // Tier 1: should not be used
    selector.add_backend(
        BackendId::from_index(2),
        ServerName::try_new("tier1".to_string()).unwrap(),
        create_backend("tier1", 100),
        1,
    );

    // Route 400 commands - all should go to tier 0 with weighted distribution
    // Create availability to trigger tier filtering (tiering only applies to article requests)
    let availability = ArticleAvailability::new();
    let mut counts = [0; 3];
    for _ in 0..400 {
        let backend = selector
            .route_command_with_availability(ClientId::new(), "ARTICLE", Some(&availability))
            .unwrap();
        counts[backend.as_index()] += 1;
    }

    // tier0-small should get 10/40 = 25% = ~100 requests
    // tier0-large should get 30/40 = 75% = ~300 requests
    assert!(
        counts[0] >= 80 && counts[0] <= 120,
        "tier0-small should get ~100 requests (25%), got {}",
        counts[0]
    );
    assert!(
        counts[1] >= 280 && counts[1] <= 320,
        "tier0-large should get ~300 requests (75%), got {}",
        counts[1]
    );
    assert_eq!(counts[2], 0, "tier1 should not be used");
}
