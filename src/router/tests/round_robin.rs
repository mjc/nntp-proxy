//! Round-robin selection strategy tests

use super::*;
use crate::types::ServerName;

#[test]
fn test_round_robin_selection() {
    let mut router = BackendSelector::new();
    let client_id = ClientId::new();

    // Add 3 backends
    for i in 0..3 {
        let backend_id = BackendId::from_index(i);
        let provider = create_test_provider();
        router.add_backend(
            backend_id,
            ServerName::new(format!("backend-{}", i)).unwrap(),
            provider,
        );
    }

    // Route 6 commands and verify round-robin
    let backend1 = router.route_command(client_id, "LIST\r\n").unwrap();
    let backend2 = router.route_command(client_id, "DATE\r\n").unwrap();
    let backend3 = router.route_command(client_id, "HELP\r\n").unwrap();
    let backend4 = router.route_command(client_id, "LIST\r\n").unwrap();
    let backend5 = router.route_command(client_id, "DATE\r\n").unwrap();
    let backend6 = router.route_command(client_id, "HELP\r\n").unwrap();

    // Should cycle through backends in order
    assert_eq!(backend1.as_index(), 0);
    assert_eq!(backend2.as_index(), 1);
    assert_eq!(backend3.as_index(), 2);
    assert_eq!(backend4.as_index(), 0); // Wraps around
    assert_eq!(backend5.as_index(), 1);
    assert_eq!(backend6.as_index(), 2);
}

#[test]
fn test_load_balancing_fairness() {
    let mut router = BackendSelector::new();
    let client_id = ClientId::new();

    // Add 3 backends
    for i in 0..3 {
        router.add_backend(
            BackendId::from_index(i),
            ServerName::new(format!("backend-{}", i)).unwrap(),
            create_test_provider(),
        );
    }

    // Route 9 commands
    let mut backend_counts = vec![0, 0, 0];
    for _ in 0..9 {
        let backend_id = router.route_command(client_id, "LIST\r\n").unwrap();
        backend_counts[backend_id.as_index()] += 1;
    }

    // Each backend should get 3 commands (perfect round-robin)
    assert_eq!(backend_counts, vec![3, 3, 3]);
}
