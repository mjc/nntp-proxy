//! Load tracking and monitoring tests

use super::*;
use crate::types::ServerName;

#[test]
fn test_backend_load_tracking() {
    let mut router = BackendSelector::new();
    let client_id = ClientId::new();
    let backend_id = BackendId::from_index(0);
    let provider = create_test_provider();

    router.add_backend(
        backend_id,
        ServerName::new("test".to_string()).unwrap(),
        provider,
    );

    // Initially no load
    assert_eq!(router.backend_load(backend_id), Some(0));

    // Route a command
    router.route_command_sync(client_id, "LIST\r\n").unwrap();
    assert_eq!(router.backend_load(backend_id), Some(1));

    // Route another
    router.route_command_sync(client_id, "DATE\r\n").unwrap();
    assert_eq!(router.backend_load(backend_id), Some(2));

    // Complete one
    router.complete_command_sync(backend_id);
    assert_eq!(router.backend_load(backend_id), Some(1));

    // Complete the other
    router.complete_command_sync(backend_id);
    assert_eq!(router.backend_load(backend_id), Some(0));
}
