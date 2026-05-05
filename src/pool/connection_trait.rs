use crate::types::{AvailableConnections, CreatedConnections, MaxPoolSize};

/// Generic connection pool status information
#[derive(Debug, Clone)]
pub struct PoolStatus {
    pub available: AvailableConnections,
    pub max_size: MaxPoolSize,
    pub created: CreatedConnections,
}

/// Trait for connection management - makes it easy to swap implementations
pub trait ConnectionProvider: Send + Sync + Clone + std::fmt::Debug {
    /// Get current pool status for monitoring
    fn status(&self) -> PoolStatus;
}
