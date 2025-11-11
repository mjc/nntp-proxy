//! Connection pool abstraction for NNTP connections
//!
//! This module defines the core `ConnectionPool` trait which provides a generic
//! interface for managing pooled NNTP connections. This abstraction enables:
//! - Easy mocking for testing
//! - Swappable pool implementations
//! - Testability without real network connections

use crate::stream::ConnectionStream;
use anyhow::Result;
use async_trait::async_trait;
use std::fmt::Debug;

/// Connection pool abstraction for async NNTP connections
///
/// This trait provides a generic interface for connection pooling, allowing
/// different implementations (real pools, mocks, test doubles) to be used
/// interchangeably throughout the codebase.
///
/// # Examples
///
/// ```no_run
/// use nntp_proxy::pool::{ConnectionPool, DeadpoolConnectionProvider};
/// use async_trait::async_trait;
///
/// async fn example(pool: impl ConnectionPool) -> anyhow::Result<()> {
///     let conn = pool.get().await?;
///     // Use connection...
///     Ok(())
/// }
/// ```
#[async_trait]
pub trait ConnectionPool: Send + Sync + Debug {
    /// Get a connection from the pool
    ///
    /// Returns a connection that will be automatically returned to the pool
    /// when dropped. May create a new connection if none are available.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Connection creation fails
    /// - Pool is exhausted and cannot create more connections
    /// - Network errors occur during connection establishment
    async fn get(&self) -> Result<ConnectionStream>;

    /// Get the name/identifier of this connection pool
    ///
    /// Used for logging and metrics to distinguish between different backend servers.
    fn name(&self) -> &str;

    /// Get current pool statistics
    ///
    /// Returns information about pool usage including available connections,
    /// maximum size, and total created connections.
    fn status(&self) -> super::PoolStatus;

    /// Get the backend host this pool connects to
    fn host(&self) -> &str;

    /// Get the backend port this pool connects to
    fn port(&self) -> u16;
}

/// Mock connection pool for testing
///
/// This implementation allows tests to inject pre-configured responses
/// without requiring actual network connections.
///
/// # Examples
///
/// ```
/// use nntp_proxy::pool::{ConnectionPool, MockConnectionPool};
///
/// #[tokio::test]
/// async fn test_with_mock_pool() {
///     let pool = MockConnectionPool::new("test-server");
///     // Use pool in tests...
/// }
/// ```
#[derive(Debug, Clone)]
pub struct MockConnectionPool {
    name: String,
    host: String,
    port: u16,
}

impl MockConnectionPool {
    /// Create a new mock connection pool
    ///
    /// # Arguments
    /// * `name` - Identifier for this pool (used in logging)
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            host: "mock.example.com".to_string(),
            port: 119,
        }
    }

    /// Set the mock host
    #[must_use]
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = host.into();
        self
    }

    /// Set the mock port
    #[must_use]
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }
}

#[async_trait]
impl ConnectionPool for MockConnectionPool {
    async fn get(&self) -> Result<ConnectionStream> {
        // For testing: create a mock stream pair
        // Real tests would inject specific behavior here
        Err(anyhow::anyhow!(
            "MockConnectionPool::get() - tests should override this"
        ))
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn status(&self) -> super::PoolStatus {
        use crate::types::{AvailableConnections, CreatedConnections, MaxPoolSize};
        super::PoolStatus {
            available: AvailableConnections::zero(),
            max_size: MaxPoolSize::new(0),
            created: CreatedConnections::zero(),
        }
    }

    fn host(&self) -> &str {
        &self.host
    }

    fn port(&self) -> u16 {
        self.port
    }
}

#[cfg(test)]
mod tests;
