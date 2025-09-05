use anyhow::Result;
use tokio::net::TcpStream;
use tracing::info;

/// Simple connection provider - creates optimized connections on demand
/// Can be easily replaced with a pooled implementation later
#[derive(Debug, Clone)]
pub struct SimpleConnectionProvider {
    host: String,
    port: u16,
    name: String,
}

impl SimpleConnectionProvider {
    pub fn new(host: String, port: u16, name: String) -> Self {
        Self { host, port, name }
    }

    /// Create a new optimized TCP connection
    pub async fn get_connection(&self) -> Result<TcpStream> {
        info!("Creating connection to {}", self.name);

        let addr = format!("{}:{}", self.host, self.port);
        let stream = TcpStream::connect(&addr).await?;

        // Apply basic optimization
        let _ = stream.set_nodelay(true);

        Ok(stream)
    }
}
