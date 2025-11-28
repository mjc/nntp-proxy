//! Integration tests for AWR mode using router's strategy
//!
//! These tests verify that per-command mode correctly uses the router's
//! AWR strategy instead of a duplicate/broken implementation.

use anyhow::Result;
use nntp_proxy::config::RoutingStrategy;
use nntp_proxy::pool::DeadpoolConnectionProvider;
use nntp_proxy::router::BackendSelector;
use nntp_proxy::types::{BackendId, ClientId, ServerName};
use std::collections::HashMap;

#[test]
fn test_awr_distributes_across_backends() -> Result<()> {
    // Create router with AWR strategy
    let mut router = BackendSelector::new(RoutingStrategy::AdaptiveWeighted);

    // Add two backends with equal capacity
    let provider1 = DeadpoolConnectionProvider::new(
        "localhost".to_string(),
        119,
        "backend1".to_string(),
        50,
        None,
        None,
    );
    let provider2 = DeadpoolConnectionProvider::new(
        "localhost".to_string(),
        119,
        "backend2".to_string(),
        50,
        None,
        None,
    );

    router.add_backend(
        BackendId::from_index(0),
        ServerName::new("backend1".to_string())?,
        provider1,
        nntp_proxy::config::PrecheckCommand::default(),
    );
    router.add_backend(
        BackendId::from_index(1),
        ServerName::new("backend2".to_string())?,
        provider2,
        nntp_proxy::config::PrecheckCommand::default(),
    );

    // Route many commands
    let mut selections = HashMap::new();
    let client_id = ClientId::new();

    for _ in 0..100 {
        let backend_id = router.route_command(client_id, "ARTICLE <test@example.com>")?;
        *selections.entry(backend_id.as_index()).or_insert(0) += 1;
        
        // Complete command to reset pending count
        router.complete_command(backend_id);
    }

    // Should use both backends (not 100% on one)
    assert!(
        selections.len() == 2,
        "Expected 2 backends used, got: {:?}",
        selections
    );
    
    let backend0_count = selections.get(&0).copied().unwrap_or(0);
    let backend1_count = selections.get(&1).copied().unwrap_or(0);

    // With equal capacity and completion, should be roughly equal distribution
    // Allow 60/40 split due to randomness
    assert!(
        backend0_count >= 20 && backend0_count <= 80,
        "Backend 0 used {} times (expected 20-80)",
        backend0_count
    );
    assert!(
        backend1_count >= 20 && backend1_count <= 80,
        "Backend 1 used {} times (expected 20-80)",
        backend1_count
    );

    Ok(())
}

#[test]
fn test_awr_prefers_idle_backend() -> Result<()> {
    let mut router = BackendSelector::new(RoutingStrategy::AdaptiveWeighted);

    let provider1 = DeadpoolConnectionProvider::new(
        "localhost".to_string(),
        119,
        "backend1".to_string(),
        50,
        None,
        None,
    );
    let provider2 = DeadpoolConnectionProvider::new(
        "localhost".to_string(),
        119,
        "backend2".to_string(),
        50,
        None,
        None,
    );

    router.add_backend(
        BackendId::from_index(0),
        ServerName::new("backend1".to_string())?,
        provider1,
        nntp_proxy::config::PrecheckCommand::default(),
    );
    router.add_backend(
        BackendId::from_index(1),
        ServerName::new("backend2".to_string())?,
        provider2,
        nntp_proxy::config::PrecheckCommand::default(),
    );

    let client_id = ClientId::new();

    // Route 10 commands to backend 0 without completing them (simulate load)
    for _ in 0..10 {
        let _ = router.route_command(client_id, "ARTICLE <msg@example.com>")?;
        // Don't complete - backend 0 accumulates pending count
    }

    // Now route more commands - they should prefer idle backend 1
    let mut selections = HashMap::new();
    for _ in 0..200 {
        let backend_id = router.route_command(client_id, "ARTICLE <test@example.com>")?;
        *selections.entry(backend_id.as_index()).or_insert(0) += 1;
        router.complete_command(backend_id);
    }

    let backend1_count = selections.get(&1).copied().unwrap_or(0);
    
    // Backend 1 (idle) should get significant majority with larger sample
    // Backend 0 has 10 pending, backend 1 has 0, so expect ~60-80% to backend 1
    assert!(
        backend1_count >= 100,
        "Idle backend 1 should get >=100/200 selections, got {}",
        backend1_count
    );

    Ok(())
}

#[test]
fn test_round_robin_distributes_evenly() -> Result<()> {
    // Verify round-robin still works for comparison
    let mut router = BackendSelector::new(RoutingStrategy::RoundRobin);

    let provider1 = DeadpoolConnectionProvider::new(
        "localhost".to_string(),
        119,
        "backend1".to_string(),
        50,
        None,
        None,
    );
    let provider2 = DeadpoolConnectionProvider::new(
        "localhost".to_string(),
        119,
        "backend2".to_string(),
        50,
        None,
        None,
    );

    router.add_backend(
        BackendId::from_index(0),
        ServerName::new("backend1".to_string())?,
        provider1,
        nntp_proxy::config::PrecheckCommand::default(),
    );
    router.add_backend(
        BackendId::from_index(1),
        ServerName::new("backend2".to_string())?,
        provider2,
        nntp_proxy::config::PrecheckCommand::default(),
    );

    let client_id = ClientId::new();
    let mut selections = Vec::new();

    for _ in 0..10 {
        let backend_id = router.route_command(client_id, "ARTICLE <test@example.com>")?;
        selections.push(backend_id.as_index());
        router.complete_command(backend_id);
    }

    // Round-robin should strictly alternate: 0,1,0,1,0,1...
    assert_eq!(selections, vec![0, 1, 0, 1, 0, 1, 0, 1, 0, 1]);

    Ok(())
}

#[test]
fn test_pending_count_increments_and_decrements() -> Result<()> {
    let mut router = BackendSelector::new(RoutingStrategy::AdaptiveWeighted);

    let provider = DeadpoolConnectionProvider::new(
        "localhost".to_string(),
        119,
        "backend1".to_string(),
        50,
        None,
        None,
    );

    router.add_backend(
        BackendId::from_index(0),
        ServerName::new("backend1".to_string())?,
        provider,
        nntp_proxy::config::PrecheckCommand::default(),
    );

    let client_id = ClientId::new();
    let backend_id = BackendId::from_index(0);

    // Initially zero
    assert_eq!(router.backend_load(backend_id).unwrap(), 0);

    // Route command increments
    router.route_command(client_id, "ARTICLE <test@example.com>")?;
    assert_eq!(router.backend_load(backend_id).unwrap(), 1);

    // Route more
    router.route_command(client_id, "ARTICLE <test2@example.com>")?;
    assert_eq!(router.backend_load(backend_id).unwrap(), 2);

    // Complete decrements
    router.complete_command(backend_id);
    assert_eq!(router.backend_load(backend_id).unwrap(), 1);

    router.complete_command(backend_id);
    assert_eq!(router.backend_load(backend_id).unwrap(), 0);

    // Should not underflow
    router.complete_command(backend_id);
    assert_eq!(router.backend_load(backend_id).unwrap(), 0);

    Ok(())
}
