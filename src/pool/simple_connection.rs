use async_trait::async_trait;

use crate::pool::connection_trait::{ConnectionProvider, PoolStatus};

/// Simple connection provider - creates optimized connections on demand
/// Can be easily replaced with a pooled implementation later
#[derive(Debug, Clone)]
#[allow(dead_code)] // Alternative connection provider, kept for future use
pub struct SimpleConnectionProvider {
    host: String,
    port: u16,
    name: String,
}

impl SimpleConnectionProvider {
    #[allow(dead_code)] // Alternative connection provider, kept for future use
    pub fn new(host: String, port: u16, name: String) -> Self {
        Self { host, port, name }
    }
}

#[async_trait]
impl ConnectionProvider for SimpleConnectionProvider {
    fn status(&self) -> PoolStatus {
        // Simple provider doesn't pool connections, so always shows max available
        PoolStatus {
            available: 1, // Always "available" since it creates on demand
            max_size: 1,  // No pooling limit
            created: 0,   // No persistent connections
        }
    }
}
