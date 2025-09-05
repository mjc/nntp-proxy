use anyhow::Result;
use deadpool::managed::{Manager, Pool, PoolError};
use std::sync::Arc;
use tokio::net::TcpStream;
use tracing::{info, warn};

use crate::ServerConfig;

/// Pooled connection wrapper for deadpool compatibility
#[derive(Debug)]
pub struct PooledConnection {
    pub(crate) stream: TcpStream,
    pub(crate) server_name: String,
    pub(crate) authenticated: bool,
}

impl PooledConnection {
    pub fn new(
        stream: TcpStream,
        server_name: String,
        authenticated: bool,
    ) -> Self {
        Self {
            stream,
            server_name,
            authenticated,
        }
    }

    pub fn into_stream(self) -> TcpStream {
        self.stream
    }

    pub fn is_authenticated(&self) -> bool {
        self.authenticated
    }

    pub fn server_name(&self) -> &str {
        &self.server_name
    }
}

/// Custom deadpool manager for TCP connections
#[derive(Debug)]
pub struct TcpManager {
    server_config: Arc<ServerConfig>,
}

impl TcpManager {
    pub fn new(server_config: Arc<ServerConfig>) -> Self {
        Self { server_config }
    }

    /// Create an optimized TCP stream with performance tuning using socket2
    async fn create_optimized_tcp_stream(addr: &str) -> Result<TcpStream, std::io::Error> {
        use socket2::{Domain, Protocol, Socket, Type};
        use std::net::{SocketAddr, ToSocketAddrs};

        // Parse the address
        let socket_addr: SocketAddr = addr.to_socket_addrs()?.next().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "Invalid address")
        })?;

        // Create socket with socket2 for better control
        let socket = Socket::new(
            if socket_addr.is_ipv4() { Domain::IPV4 } else { Domain::IPV6 },
            Type::STREAM,
            Some(Protocol::TCP),
        )?;

        // Set socket buffer sizes for high throughput (2MB each)
        socket.set_recv_buffer_size(2 * 1024 * 1024)?;
        socket.set_send_buffer_size(2 * 1024 * 1024)?;

        // Enable keepalive for connection reuse
        socket.set_keepalive(true)?;

        // Set aggressive keepalive timing for high-performance scenarios
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            // Start probes after 60 seconds, probe every 10 seconds
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

impl Manager for TcpManager {
    type Type = TcpStream;
    type Error = anyhow::Error;

    fn create(&self) -> impl std::future::Future<Output = Result<Self::Type, Self::Error>> + Send + '_ {
        async move {
            let addr = format!("{}:{}", self.server_config.host, self.server_config.port);
            info!("Creating new connection to {} for deadpool", self.server_config.name);
            Self::create_optimized_tcp_stream(&addr).await
                .map_err(|e| anyhow::anyhow!("Failed to create connection: {}", e))
        }
    }

    fn recycle(
        &self,
        conn: &mut Self::Type,
        _metrics: &deadpool::managed::Metrics,
    ) -> impl std::future::Future<Output = deadpool::managed::RecycleResult<Self::Error>> + Send + '_ {
        async move {
            // For TCP connections, we'll do a simple health check
            // Try a non-blocking read to see if the connection is still alive
            let mut test_buf = [0u8; 1];
            match conn.try_read(&mut test_buf) {
                Ok(0) => {
                    // Connection was closed by server
                    warn!("Connection to {} was closed by server during recycle check", self.server_config.name);
                    Err(deadpool::managed::RecycleError::message("Connection closed by server"))
                }
                Ok(_) => {
                    // Got data, which might be normal for NNTP (server messages)
                    // We'll consider this OK for now and let the application handle it
                    info!("Connection to {} appears healthy (has buffered data)", self.server_config.name);
                    Ok(())
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // No data available, connection appears healthy
                    info!("Connection to {} appears healthy", self.server_config.name);
                    Ok(())
                }
                Err(e) => {
                    // Connection error
                    warn!("Connection to {} failed health check: {}", self.server_config.name, e);
                    Err(deadpool::managed::RecycleError::message("Connection health check failed"))
                }
            }
        }
    }
}

/// Deadpool-based connection pool wrapper
#[derive(Debug, Clone)]
pub struct ConnectionPool {
    pool: Pool<TcpManager>,
    server_config: Arc<ServerConfig>,
}

impl ConnectionPool {
    pub fn new(server_config: Arc<ServerConfig>) -> Result<Self, anyhow::Error> {
        let manager = TcpManager::new(server_config.clone());
        
        let pool = Pool::builder(manager)
            .max_size(server_config.max_connections as usize)
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to create connection pool: {}", e))?;
        
        info!("Created deadpool connection pool for {} with max {} connections", 
              server_config.name, server_config.max_connections);
        
        Ok(Self {
            pool,
            server_config,
        })
    }

    /// Get a connection from the pool
    pub async fn get(&self) -> Result<deadpool::managed::Object<TcpManager>, PoolError<anyhow::Error>> {
        self.pool.get().await
    }

    /// Get pool status for monitoring
    pub fn status(&self) -> deadpool::Status {
        self.pool.status()
    }

    /// Prewarm connections
    pub async fn prewarm(&self, count: usize) -> Result<(), anyhow::Error> {
        let mut connections = Vec::new();
        let actual_count = count.min(self.server_config.max_connections as usize);
        
        info!("Prewarming {} connections for server {}", actual_count, self.server_config.name);
        
        for _ in 0..actual_count {
            match self.get().await {
                Ok(conn) => {
                    connections.push(conn);
                }
                Err(e) => {
                    warn!("Failed to prewarm connection for server {}: {}", self.server_config.name, e);
                    break;
                }
            }
        }
        
        let prewarmed = connections.len();
        drop(connections);
        info!("Completed prewarming for server {} with {} connections", 
              self.server_config.name, prewarmed);
        
        Ok(())
    }
}
