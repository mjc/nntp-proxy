use anyhow::Result;
use async_trait::async_trait;
use deadpool::managed;
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

use crate::pool::connection_trait::{ConnectionProvider, PoolStatus};

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
        let domain = if socket_addr.is_ipv4() {
            Domain::IPV4
        } else {
            Domain::IPV6
        };
        let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;

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

        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            // Fallback for non-Linux platforms
            let keepalive = socket2::TcpKeepalive::new()
                .with_time(std::time::Duration::from_secs(60))
                .with_interval(std::time::Duration::from_secs(10));
            socket.set_tcp_keepalive(&keepalive)?;
        }

        // Disable Nagle's algorithm for low latency
        socket.set_tcp_nodelay(true)?;

        // Set reuse address for quick restart
        socket.set_reuse_address(true)?;

        // Note: set_reuse_port is not available in socket2 0.5 on all platforms
        // It's primarily a Linux feature anyway

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

    async fn recycle(
        &self,
        _conn: &mut TcpStream,
        _: &managed::Metrics,
    ) -> managed::RecycleResult<anyhow::Error> {
        // For NNTP connections, we don't do invasive health checks that might consume data
        // The connection will be tested when actually used for communication
        debug!("Connection to {} recycled successfully", self.name);
        Ok(())
    }

    fn detach(&self, _conn: &mut TcpStream) {
        // Connection is being removed from the pool
        // For simplicity, we'll let the connection drop naturally
        // The main graceful shutdown will handle QUIT commands
        debug!("Connection detached from pool for {}", self.name);
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

        info!(
            "Created deadpool connection provider for '{}' with max {} connections",
            name, max_size
        );

        Self { pool, name }
    }

    /// Get a connection from the pool that can be returned (for prewarming)
    /// Unlike get_connection(), this doesn't use Object::take() so the connection can return to pool
    pub async fn get_pooled_connection(&self) -> Result<managed::Object<TcpManager>> {
        debug!(
            "Getting pooled connection for prewarming from {}",
            self.name
        );
        self.pool
            .get()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get pooled connection: {}", e))
    }

    /// Gracefully shutdown the pool by sending QUIT to all idle connections
    pub async fn graceful_shutdown(&self) {
        info!(
            "Gracefully shutting down connection pool for '{}'",
            self.name
        );

        let status = self.pool.status();
        let idle_connections = status.available;

        info!(
            "Sending QUIT to {} idle connections for '{}'",
            idle_connections, self.name
        );

        // Try to get and send QUIT to idle connections with a short timeout
        for _ in 0..idle_connections {
            // Use a very short timeout to only get immediately available connections
            let timeout = std::time::Duration::from_millis(1);
            let mut timeouts = managed::Timeouts::new();
            timeouts.wait = Some(timeout);

            if let Ok(conn_obj) = self.pool.timeout_get(&timeouts).await {
                use deadpool::managed::Object;
                use tokio::io::AsyncWriteExt;

                // Take the connection from the pool permanently
                let mut conn = Object::take(conn_obj);

                // Send QUIT command
                if let Err(e) = conn.write_all(b"QUIT\r\n").await {
                    debug!(
                        "Failed to send QUIT to connection for '{}': {}",
                        self.name, e
                    );
                } else {
                    debug!("Sent QUIT to connection for '{}'", self.name);
                }
                // Connection will be dropped here, closing it
            } else {
                break; // No more idle connections available
            }
        }

        // Close the pool to prevent new connections
        self.pool.close();

        info!("Connection pool closed for '{}'", self.name);
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
                warn!(
                    "Failed to get connection from pool for {}: {}",
                    self.name, e
                );
                Err(anyhow::anyhow!("Pool connection failed: {}", e))
            }
        }
    }

    fn status(&self) -> PoolStatus {
        let status = self.pool.status();
        PoolStatus {
            available: status.available,
            max_size: status.max_size,
            created: status.size,
        }
    }
}
