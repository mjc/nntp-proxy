use anyhow::Result;
use async_trait::async_trait;
use tokio::net::TcpStream;

/// Connection type that indicates whether authentication is needed
#[derive(Debug)]
pub enum ConnectionType {
    /// Fresh connection that needs full authentication
    Fresh(TcpStream),
    /// Pre-authenticated pooled connection that can skip auth
    Pooled(TcpStream),
}

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
    /// Get a connection to the backend server
    async fn get_connection(&self) -> Result<TcpStream>;
    
    /// Get a connection with type information for authentication optimization
    async fn get_typed_connection(&self) -> Result<ConnectionType> {
        // Default implementation treats all connections as fresh
        Ok(ConnectionType::Fresh(self.get_connection().await?))
    }
    
    /// Get current pool status for monitoring
    #[allow(dead_code)] // Used for client greetings
    fn status(&self) -> PoolStatus;
}
