use anyhow::Result;
use async_trait::async_trait;
use tokio::net::TcpStream;

/// Generic connection pool status information
#[derive(Debug, Clone)]
pub struct PoolStatus {
    pub available: usize,
    pub max_size: usize,
    pub created: usize,
}

/// Trait for connection management - makes it easy to swap implementations
#[async_trait]
pub trait ConnectionProvider: Send + Sync + Clone + std::fmt::Debug {
    /// Get a connection to the backend server
    async fn get_connection(&self) -> Result<TcpStream>;
    
    /// Get current pool status for monitoring
    fn status(&self) -> PoolStatus;
}
