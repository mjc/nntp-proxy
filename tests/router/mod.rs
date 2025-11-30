//! Router integration tests

use nntp_proxy::pool::DeadpoolConnectionProvider;
use nntp_proxy::router::BackendSelector;
use nntp_proxy::types::{BackendId, ClientId, ServerName};

mod basic;
mod load_tracking;
mod round_robin;
mod stateful;
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
