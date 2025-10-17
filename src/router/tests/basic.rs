//! Basic router functionality tests

use super::*;

#[test]
fn test_router_creation() {
    let router = BackendSelector::new();
    assert_eq!(router.backend_count(), 0);
}

#[test]
fn test_add_backend() {
    let mut router = BackendSelector::new();
    let backend_id = BackendId::from_index(0);
    let provider = create_test_provider();

    router.add_backend(backend_id, "test-backend".to_string(), provider);

    assert_eq!(router.backend_count(), 1);
}

#[test]
fn test_add_multiple_backends() {
    let mut router = BackendSelector::new();

    for i in 0..3 {
        let backend_id = BackendId::from_index(i);
        let provider = create_test_provider();
        router.add_backend(backend_id, format!("backend-{}", i), provider);
    }

    assert_eq!(router.backend_count(), 3);
}

#[test]
fn test_no_backends_fails() {
    let router = BackendSelector::new();
    let client_id = ClientId::new();
    let result = router.route_command_sync(client_id, "LIST\r\n");

    assert!(result.is_err());
}

#[test]
fn test_get_backend_provider() {
    let mut router = BackendSelector::new();
    let backend_id = BackendId::from_index(0);
    let provider = create_test_provider();

    router.add_backend(backend_id, "test".to_string(), provider);

    let retrieved = router.get_backend_provider(backend_id);
    assert!(retrieved.is_some());

    let fake_id = BackendId::from_index(999);
    assert!(router.get_backend_provider(fake_id).is_none());
}
