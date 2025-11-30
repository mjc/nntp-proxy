//! Tests for the router module

use super::*;

mod basic;
mod load_tracking;
mod round_robin;
mod stateful;
mod weighted;

/// Helper function to create a test provider for use in tests
pub(crate) fn create_test_provider() -> DeadpoolConnectionProvider {
    DeadpoolConnectionProvider::new(
        "localhost".to_string(),
        9999,
        "test".to_string(),
        2,
        None,
        None,
    )
}
