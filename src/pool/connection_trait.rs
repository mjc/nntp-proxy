use anyhow::Result;
use async_trait::async_trait;
use tokio::net::TcpStream;

/// Trait for connection management - makes it easy to swap implementations
#[async_trait]
pub trait ConnectionProvider: Send + Sync + Clone + std::fmt::Debug {
    /// Get a connection to the backend server
    async fn get_connection(&self) -> Result<TcpStream>;
}
