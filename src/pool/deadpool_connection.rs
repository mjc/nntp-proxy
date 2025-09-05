use anyhow::Result;
use async_trait::async_trait;
use deadpool::managed;
use tokio::net::TcpStream;
use tracing::{info, debug, warn};

use crate::pool::connection_trait::ConnectionProvider;

/// TCP connection manager for deadpool
#[derive(Debug)]
pub struct TcpManager {
    host: String,
    port: u16,
    name: String,
}

impl TcpManager {
    pub fn new(host: String, port: u16, name: String) -> Self {
        Self { host, port, name }
    }

    /// Create an optimized TCP connection
    async fn create_optimized_tcp_stream(&self) -> Result<TcpStream, anyhow::Error> {
        use socket2::{Domain, Protocol, Socket, Type};
        use std::net::SocketAddr;

        // First try to resolve the hostname to an IP address
        let addr = format!("{}:{}", self.host, self.port);
        let socket_addrs: Vec<SocketAddr> = tokio::net::lookup_host(&addr).await?.collect();
        
        if socket_addrs.is_empty() {
            return Err(anyhow::anyhow!("No addresses found for {}", addr));
        }
        
        let socket_addr = socket_addrs[0]; // Use the first resolved address
        
        // Create socket with optimizations
        let domain = if socket_addr.is_ipv4() { Domain::IPV4 } else { Domain::IPV6 };
        let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
        
        // Enable keepalive
        {
            let keepalive = socket2::TcpKeepalive::new()
                .with_time(std::time::Duration::from_secs(60))
                .with_interval(std::time::Duration::from_secs(10));
            socket.set_tcp_keepalive(&keepalive)?;
        }

        // Disable Nagle's algorithm for low latency
        socket.set_nodelay(true)?;

        // Set reuse address for quick restart
        socket.set_reuse_address(true)?;

        // Connect to the target
        socket.connect(&socket_addr.into())?;

        // Convert socket2::Socket to tokio TcpStream
        let std_stream: std::net::TcpStream = socket.into();
        std_stream.set_nonblocking(true)?;
        let stream = TcpStream::from_std(std_stream)?;

        Ok(stream)
    }
}

impl managed::Manager for TcpManager {
    type Type = TcpStream;
    type Error = anyhow::Error;

    async fn create(&self) -> Result<TcpStream, anyhow::Error> {
        debug!("Creating new TCP connection to {} for deadpool", self.name);
        self.create_optimized_tcp_stream().await
    }

    async fn recycle(&self, conn: &mut TcpStream, _: &managed::Metrics) -> managed::RecycleResult<anyhow::Error> {
        // For TCP connections, do a simple health check
        let mut test_buf = [0u8; 1];
        match conn.try_read(&mut test_buf) {
            Ok(0) => {
                // Connection was closed by server
                warn!("Connection to {} was closed by server during recycle check", self.name);
                Err(managed::RecycleError::message("Connection closed by server"))
            }
            Ok(_) => {
                // Got data, which might be normal for NNTP (server messages)
                debug!("Connection to {} appears healthy (has buffered data)", self.name);
                Ok(())
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // No data available, connection appears healthy
                debug!("Connection to {} appears healthy", self.name);
                Ok(())
            }
            Err(e) => {
                // Connection error
                warn!("Connection to {} failed health check: {}", self.name, e);
                Err(managed::RecycleError::message("Connection health check failed"))
            }
        }
    }
}

type Pool = managed::Pool<TcpManager>;

/// Deadpool-based connection provider with connection pooling
#[derive(Debug, Clone)]
pub struct DeadpoolConnectionProvider {
    pool: Pool,
    name: String,
}

impl DeadpoolConnectionProvider {
    pub fn new(host: String, port: u16, name: String, max_size: usize) -> Self {
        let manager = TcpManager::new(host, port, name.clone());
        let pool = Pool::builder(manager)
            .max_size(max_size)
            .build()
            .expect("Failed to create connection pool");
        
        info!("Created deadpool connection provider for '{}' with max {} connections", name, max_size);
        
        Self { pool, name }
    }

    /// Get current pool status for monitoring
    pub fn status(&self) -> managed::Status {
        self.pool.status()
    }
}

#[async_trait]
impl ConnectionProvider for DeadpoolConnectionProvider {
    async fn get_connection(&self) -> Result<TcpStream> {
        debug!("Getting connection from pool for {}", self.name);
        
        match self.pool.get().await {
            Ok(conn) => {
                debug!("Retrieved connection from pool for {}", self.name);
                // Use Object::take() to permanently remove the connection from the pool
                // This is appropriate for a proxy where connections aren't reused
                use deadpool::managed::Object;
                let stream = Object::take(conn);
                Ok(stream)
            }
            Err(e) => {
                warn!("Failed to get connection from pool for {}: {}", self.name, e);
                Err(anyhow::anyhow!("Pool connection failed: {}", e))
            }
        }
    }
}
