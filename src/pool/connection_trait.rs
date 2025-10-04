use async_trait::async_trait;

/// Generic connection pool status information
#[derive(Debug, Clone)]
#[allow(dead_code)] // Used in greetings and monitoring
pub struct PoolStatus {
    pub available: usize,
    pub max_size: usize,
    pub created: usize,
}

/// Trait for connection management - makes it easy to swap implementations
#[async_trait]
pub trait ConnectionProvider: Send + Sync + Clone + std::fmt::Debug {
    /// Get current pool status for monitoring
    #[allow(dead_code)] // Used for client greetings
    fn status(&self) -> PoolStatus;
}
