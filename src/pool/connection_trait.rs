use crate::types::{AvailableConnections, CreatedConnections, MaxPoolSize};
use async_trait::async_trait;

/// Generic connection pool status information
#[derive(Debug, Clone)]
#[allow(dead_code)] // Used in greetings and monitoring
pub struct PoolStatus {
    pub available: AvailableConnections,
    pub max_size: MaxPoolSize,
    pub created: CreatedConnections,
}

/// Trait for connection management - makes it easy to swap implementations
#[async_trait]
pub trait ConnectionProvider: Send + Sync + Clone + std::fmt::Debug {
    /// Get current pool status for monitoring
    #[allow(dead_code)] // Used for client greetings
    fn status(&self) -> PoolStatus;
}
