//! Stateful connection reservation tests

use super::*;
use crate::types::ServerName;
use std::sync::Arc;

#[test]
fn test_stateful_connection_reservation() {
    let mut router = BackendSelector::new();
    let backend_id = BackendId::from_index(0);

    // Create test provider with max_connections = 3 (so max stateful = 2)
    let provider = DeadpoolConnectionProvider::new(
        "localhost".to_string(),
        9999,
        "test".to_string(),
        3, // max_connections = 3, so max stateful = 2
        None,
        None,
    );

    router.add_backend(
        backend_id,
        ServerName::new("test-backend".to_string()).unwrap(),
        provider,
    );

    // Should be able to acquire 2 stateful connections
    assert!(router.try_acquire_stateful(backend_id));
    assert_eq!(router.stateful_count(backend_id), Some(1));

    assert!(router.try_acquire_stateful(backend_id));
    assert_eq!(router.stateful_count(backend_id), Some(2));

    // Third attempt should fail (max_connections - 1 = 2)
    assert!(!router.try_acquire_stateful(backend_id));
    assert_eq!(router.stateful_count(backend_id), Some(2));

    // Release one connection and try again
    router.release_stateful(backend_id);
    assert_eq!(router.stateful_count(backend_id), Some(1));
    assert!(router.try_acquire_stateful(backend_id));
    assert_eq!(router.stateful_count(backend_id), Some(2));
}

#[test]
fn test_stateful_connection_concurrent_access() {
    let mut router = BackendSelector::new();
    let backend_id = BackendId::from_index(0);

    router.add_backend(
        backend_id,
        ServerName::new("test-backend".to_string()).unwrap(),
        create_test_provider(),
    );

    // Simulate concurrent access with multiple threads
    let router_arc = Arc::new(router);
    let handles: Vec<_> = (0..10)
        .map(|_| {
            let router_clone = Arc::clone(&router_arc);
            std::thread::spawn(move || {
                // Try to acquire and immediately release
                if router_clone.try_acquire_stateful(backend_id) {
                    // Simulate some work
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    router_clone.release_stateful(backend_id);
                    1
                } else {
                    0
                }
            })
        })
        .collect();

    let total_acquired: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();

    // At least some threads should have succeeded
    assert!(total_acquired > 0);
    // Final count should be 0 (all released)
    assert_eq!(router_arc.stateful_count(backend_id), Some(0));
}

#[test]
fn test_stateful_connection_multiple_backends() {
    let mut router = BackendSelector::new();

    // Add multiple backends
    let backend1 = BackendId::from_index(0);
    let backend2 = BackendId::from_index(1);

    // Create test providers with max_connections = 3 (so max stateful = 2 each)
    let provider1 = DeadpoolConnectionProvider::new(
        "localhost".to_string(),
        9999,
        "test-1".to_string(),
        3, // max_connections = 3, so max stateful = 2
        None,
        None,
    );
    let provider2 = DeadpoolConnectionProvider::new(
        "localhost".to_string(),
        9998,
        "test-2".to_string(),
        3, // max_connections = 3, so max stateful = 2
        None,
        None,
    );

    router.add_backend(
        backend1,
        ServerName::new("backend-1".to_string()).unwrap(),
        provider1,
    );
    router.add_backend(
        backend2,
        ServerName::new("backend-2".to_string()).unwrap(),
        provider2,
    );

    // Each backend should have independent stateful counters
    assert!(router.try_acquire_stateful(backend1));
    assert!(router.try_acquire_stateful(backend2));

    assert_eq!(router.stateful_count(backend1), Some(1));
    assert_eq!(router.stateful_count(backend2), Some(1));

    // Should be able to max out both backends independently
    assert!(router.try_acquire_stateful(backend1)); // backend1 now at 2/2
    assert!(router.try_acquire_stateful(backend2)); // backend2 now at 2/2

    assert!(!router.try_acquire_stateful(backend1)); // Should fail
    assert!(!router.try_acquire_stateful(backend2)); // Should fail

    // Release from backend1, should not affect backend2
    router.release_stateful(backend1);
    assert_eq!(router.stateful_count(backend1), Some(1));
    assert_eq!(router.stateful_count(backend2), Some(2));

    assert!(router.try_acquire_stateful(backend1)); // Should work
    assert!(!router.try_acquire_stateful(backend2)); // Should still fail
}

#[test]
fn test_stateful_connection_invalid_backend() {
    let router = BackendSelector::new();
    let invalid_backend = BackendId::from_index(999);

    // Operations on non-existent backend should handle gracefully
    assert!(!router.try_acquire_stateful(invalid_backend));
    assert_eq!(router.stateful_count(invalid_backend), None);

    // Release on non-existent backend should not panic
    router.release_stateful(invalid_backend);
}

#[test]
fn test_stateful_reservation_edge_cases() {
    let mut router = BackendSelector::new();
    let backend_id = BackendId::from_index(0);

    let provider = DeadpoolConnectionProvider::new(
        "localhost".to_string(),
        9999,
        "test".to_string(),
        3, // max_connections = 3, so max stateful = 2
        None,
        None,
    );

    router.add_backend(
        backend_id,
        ServerName::new("test-backend".to_string()).unwrap(),
        provider,
    );

    // Test multiple releases (should not go negative)
    router.release_stateful(backend_id);
    router.release_stateful(backend_id);
    router.release_stateful(backend_id);

    // Count should stay at 0
    assert_eq!(router.stateful_count(backend_id), Some(0));

    // Should still be able to acquire after over-releasing
    assert!(router.try_acquire_stateful(backend_id));
    assert_eq!(router.stateful_count(backend_id), Some(1));
}
