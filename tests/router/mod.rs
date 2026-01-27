//! Router integration tests

use nntp_proxy::pool::DeadpoolConnectionProvider;
use nntp_proxy::router::BackendSelector;
use nntp_proxy::types::{BackendId, ClientId, ServerName};

mod basic;
mod least_loaded;
mod load_tracking;
mod round_robin;
mod stateful;
mod tiered;
mod weighted;

/// Helper function to create a test provider for use in tests
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

/// Helper to create a test backend with specified max_connections
fn create_backend(name: &str, max_connections: usize) -> DeadpoolConnectionProvider {
    DeadpoolConnectionProvider::builder("localhost", 119)
        .name(name)
        .max_connections(max_connections)
        .build()
        .unwrap()
}
