//! Router and routing strategy tests
//!
//! This module contains tests for backend routing strategies, load balancing,
//! round-robin, weighted routing, tiered routing, and stateful routing modes.

use nntp_proxy::pool::DeadpoolConnectionProvider;
use nntp_proxy::router::BackendSelector;
use nntp_proxy::types::{BackendId, ClientId, ServerName};

pub mod backend_availability;
pub mod basic;
pub mod duplicate_greeting;
pub mod edge_cases;
pub mod least_loaded;
pub mod load_tracking;
pub mod modes;
pub mod retry_430;
pub mod round_robin;
pub mod stale_connection;
pub mod stateful;
pub mod tiered;
pub mod weighted;

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
