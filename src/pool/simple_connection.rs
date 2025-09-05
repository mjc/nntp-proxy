use anyhow::Result;
use std::sync::Arc;
use tokio::net::TcpStream;
use tracing::{info, warn};

use crate::ServerConfig;

/// Simple connection manager - no pooling, just creates connections on demand
#[derive(Debug, Clone)]
pub struct ConnectionManager {
    server_config: Arc<ServerConfig>,
}

impl ConnectionManager {
    pub fn new(server_config: Arc<ServerConfig>) -> Self {
        Self { server_config }
    }

    /// Create a new optimized TCP connection
    pub async fn create_connection(&self) -> Result<TcpStream> {
        let addr = format!("{}:{}", self.server_config.host, self.server_config.port);
        info!("Creating new connection to {}", self.server_config.name);
        
        // Use tokio's built-in connect which handles hostname resolution and async connection properly
        let stream = TcpStream::connect(&addr).await?;
        
        // Apply optimizations after connection is established
        Self::optimize_tcp_stream(&stream).await?;
        
        Ok(stream)
    }

    /// Apply TCP optimizations to an existing stream
    async fn optimize_tcp_stream(stream: &TcpStream) -> Result<()> {
        // Set TCP_NODELAY for lower latency
        if let Err(e) = stream.set_nodelay(true) {
            warn!("Failed to set TCP_NODELAY: {}", e);
        }

        Ok(())
    }
}
