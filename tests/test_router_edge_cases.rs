//! Router edge case and error handling integration tests

use nntp_proxy::pool::DeadpoolConnectionProvider;
use nntp_proxy::router::BackendSelector;
use nntp_proxy::types::{BackendId, ClientId, ServerName};
use std::sync::Arc;

/// Helper function to create a test provider
fn create_test_provider() -> DeadpoolConnectionProvider {
    DeadpoolConnectionProvider::new(
        "localhost".to_string(),
        9999,
        "test".to_string(),
        2,
        None,
        None,
    )
}

#[test]
fn test_complete_command_on_empty_router() {
    let router = BackendSelector::new();
    let backend_id = BackendId::from_index(0);

    // Should not panic when completing command on non-existent backend
    router.complete_command(backend_id);
}

#[test]
fn test_complete_command_on_wrong_backend() {
    let mut router = BackendSelector::new();
    let backend_id = BackendId::from_index(0);

    router.add_backend(
        backend_id,
        ServerName::try_new("test".to_string()).unwrap(),
        create_test_provider(),
    );

    // Complete command on non-existent backend (different ID)
    let wrong_id = BackendId::from_index(999);
    router.complete_command(wrong_id);

    // Should not affect the real backend
    assert_eq!(router.backend_load(backend_id), Some(0));
}

#[test]
fn test_backend_load_for_nonexistent_backend() {
    let router = BackendSelector::new();
    let backend_id = BackendId::from_index(999);

    assert_eq!(router.backend_load(backend_id), None);
}

#[test]
fn test_stateful_count_for_nonexistent_backend() {
    let router = BackendSelector::new();
    let backend_id = BackendId::from_index(999);

    assert_eq!(router.stateful_count(backend_id), None);
}

#[test]
fn test_release_stateful_when_count_is_zero() {
    let mut router = BackendSelector::new();
    let backend_id = BackendId::from_index(0);

    router.add_backend(
        backend_id,
        ServerName::try_new("test".to_string()).unwrap(),
        create_test_provider(),
    );

    // Release without acquiring should not underflow (fetch_update prevents it)
    router.release_stateful(backend_id);
    assert_eq!(router.stateful_count(backend_id), Some(0));

    // Multiple releases should still be safe
    router.release_stateful(backend_id);
    router.release_stateful(backend_id);
    assert_eq!(router.stateful_count(backend_id), Some(0));
}

#[test]
fn test_release_stateful_on_nonexistent_backend() {
    let router = BackendSelector::new();
    let backend_id = BackendId::from_index(999);

    // Should not panic
    router.release_stateful(backend_id);
}

#[test]
fn test_excessive_complete_command_calls() {
    let mut router = BackendSelector::new();
    let client_id = ClientId::new();
    let backend_id = BackendId::from_index(0);

    router.add_backend(
        backend_id,
        ServerName::try_new("test".to_string()).unwrap(),
        create_test_provider(),
    );

    // Route one command
    router.route_command(client_id, "LIST").unwrap();
    assert_eq!(router.backend_load(backend_id), Some(1));

    // Complete it once
    router.complete_command(backend_id);
    assert_eq!(router.backend_load(backend_id), Some(0));

    // Excessive completes can cause the atomic counter to underflow (wrapping behavior)
    // This is known behavior for AtomicUsize - it will wrap around
    router.complete_command(backend_id);
    router.complete_command(backend_id);

    // After underflow, load will be very large (close to usize::MAX)
    let load = router.backend_load(backend_id).unwrap();
    // Check that it wrapped (should be > usize::MAX - 10)
    assert!(load > usize::MAX - 10, "Expected underflow, got {}", load);
}

#[test]
fn test_large_number_of_backends() {
    let mut router = BackendSelector::new();
    let client_id = ClientId::new();

    // Add 100 backends
    for i in 0..100 {
        router.add_backend(
            BackendId::from_index(i),
            ServerName::try_new(format!("backend-{}", i)).unwrap(),
            create_test_provider(),
        );
    }

    assert_eq!(router.backend_count(), 100);

    // Route 1000 commands
    for _ in 0..1000 {
        let backend_id = router.route_command(client_id, "LIST").unwrap();
        assert!(backend_id.as_index() < 100);
    }
}

#[test]
fn test_backend_provider_retrieval() {
    let mut router = BackendSelector::new();
    let backend_id = BackendId::from_index(0);

    router.add_backend(
        backend_id,
        ServerName::try_new("test".to_string()).unwrap(),
        create_test_provider(),
    );

    let provider = router.backend_provider(backend_id);
    assert!(provider.is_some());

    // Verify we can call methods on the provider
    let provider = provider.unwrap();
    assert_eq!(provider.max_size(), 2); // From create_test_provider()
}

#[test]
fn test_single_backend_round_robin() {
    let mut router = BackendSelector::new();
    let client_id = ClientId::new();
    let backend_id = BackendId::from_index(0);

    router.add_backend(
        backend_id,
        ServerName::try_new("solo".to_string()).unwrap(),
        create_test_provider(),
    );

    // All commands should route to the same backend
    for _ in 0..10 {
        let selected = router.route_command(client_id, "LIST").unwrap();
        assert_eq!(selected, backend_id);
    }
}

#[test]
fn test_stateful_acquisition_with_max_connections_1() {
    let mut router = BackendSelector::new();
    let backend_id = BackendId::from_index(0);

    // max_connections = 1 means max_stateful = 0 (need to reserve 1 for PCR)
    let provider = DeadpoolConnectionProvider::new(
        "localhost".to_string(),
        9999,
        "minimal".to_string(),
        1, // max_connections = 1
        None,
        None,
    );

    router.add_backend(
        backend_id,
        ServerName::try_new("minimal-backend".to_string()).unwrap(),
        provider,
    );

    // Should never be able to acquire stateful (all reserved for PCR)
    assert!(!router.try_acquire_stateful(backend_id));
    assert_eq!(router.stateful_count(backend_id), Some(0));
}

#[test]
fn test_concurrent_route_command_calls() {
    let mut router = BackendSelector::new();

    // Add 3 backends
    for i in 0..3 {
        router.add_backend(
            BackendId::from_index(i),
            ServerName::try_new(format!("backend-{}", i)).unwrap(),
            create_test_provider(),
        );
    }

    let router_arc = Arc::new(router);
    let handles: Vec<_> = (0..100)
        .map(|_| {
            let router_clone = Arc::clone(&router_arc);
            std::thread::spawn(move || {
                let client_id = ClientId::new();
                router_clone.route_command(client_id, "LIST").unwrap()
            })
        })
        .collect();

    let mut backend_counts = vec![0usize; 3];
    for handle in handles {
        let backend_id = handle.join().unwrap();
        backend_counts[backend_id.as_index()] += 1;
    }

    // All 100 commands should be distributed
    let total: usize = backend_counts.iter().sum();
    assert_eq!(total, 100);

    // Each backend should get roughly equal distribution (allow ±5 variance)
    for count in &backend_counts {
        assert!(
            *count >= 28 && *count <= 38,
            "Unexpected distribution: {:?}",
            backend_counts
        );
    }
}

#[test]
fn test_backend_count_with_no_backends() {
    let router = BackendSelector::new();
    assert_eq!(router.backend_count(), 0);
}

#[test]
fn test_default_constructor() {
    let router = BackendSelector::default();
    assert_eq!(router.backend_count(), 0);
}

#[test]
fn test_stateful_acquire_release_interleaved() {
    let mut router = BackendSelector::new();
    let backend_id = BackendId::from_index(0);

    let provider = DeadpoolConnectionProvider::new(
        "localhost".to_string(),
        9999,
        "test".to_string(),
        5, // max_connections = 5, so max_stateful = 4
        None,
        None,
    );

    router.add_backend(
        backend_id,
        ServerName::try_new("test".to_string()).unwrap(),
        provider,
    );

    // Acquire, release, acquire pattern
    assert!(router.try_acquire_stateful(backend_id));
    assert_eq!(router.stateful_count(backend_id), Some(1));

    assert!(router.try_acquire_stateful(backend_id));
    assert_eq!(router.stateful_count(backend_id), Some(2));

    router.release_stateful(backend_id);
    assert_eq!(router.stateful_count(backend_id), Some(1));

    assert!(router.try_acquire_stateful(backend_id));
    assert_eq!(router.stateful_count(backend_id), Some(2));

    router.release_stateful(backend_id);
    router.release_stateful(backend_id);
    assert_eq!(router.stateful_count(backend_id), Some(0));
}

#[test]
fn test_wrap_around_with_large_counter() {
    let mut router = BackendSelector::new();
    let client_id = ClientId::new();

    // Add 2 backends
    for i in 0..2 {
        router.add_backend(
            BackendId::from_index(i),
            ServerName::try_new(format!("backend-{}", i)).unwrap(),
            create_test_provider(),
        );
    }

    // Route enough commands to potentially overflow smaller counter types
    for _ in 0..10000 {
        let backend_id = router.route_command(client_id, "LIST").unwrap();
        assert!(backend_id.as_index() < 2);
    }

    // Should still work correctly after many iterations
    let backend = router.route_command(client_id, "LIST").unwrap();
    assert!(backend.as_index() < 2);
}
