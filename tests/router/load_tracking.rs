//! Load tracking and monitoring tests

use super::*;

#[test]
fn test_backend_load_tracking() {
    let mut router = BackendSelector::new();
    let client_id = ClientId::new();
    let backend_id = BackendId::from_index(0);
    let provider = create_test_provider();

    router.add_backend(
        backend_id,
        ServerName::try_new("test".to_string()).unwrap(),
        provider,
    );

    // Initially no load
    assert_eq!(router.backend_load(backend_id).map(|c| c.get()), Some(0));

    // Route a command
    router.route_command(client_id, "LIST").unwrap();
    assert_eq!(router.backend_load(backend_id).map(|c| c.get()), Some(1));

    // Route another
    router.route_command(client_id, "LIST").unwrap();
    assert_eq!(router.backend_load(backend_id).map(|c| c.get()), Some(2));

    // Complete one
    router.complete_command(backend_id);
    assert_eq!(router.backend_load(backend_id).map(|c| c.get()), Some(1));

    // Complete another
    router.complete_command(backend_id);
    assert_eq!(router.backend_load(backend_id).map(|c| c.get()), Some(0));
}
