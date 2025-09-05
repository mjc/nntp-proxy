use anyhow::Result;
use crossbeam::queue::SegQueue;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

use crate::ServerConfig;

/// Pooled connection wrapper
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

/// Connection pool for backend servers
#[derive(Debug, Clone)]
pub struct ConnectionPool {
    pool: Arc<SegQueue<TcpStream>>,
    max_connections: usize,
    active_connections: Arc<AtomicUsize>,
    initialized: Arc<AtomicBool>,
}

impl ConnectionPool {
    pub fn new(max_connections: usize) -> Self {
        Self {
            pool: Arc::new(SegQueue::new()),
            max_connections,
            active_connections: Arc::new(AtomicUsize::new(0)),
            initialized: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Pre-establish all connections on first request for maximum performance
    async fn initialize_connections(&self, server: &ServerConfig) -> Result<()> {
        // Use compare_exchange to ensure only one thread initializes
        if self
            .initialized
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            info!(
                "Pre-establishing {} connections to {}",
                self.max_connections, server.name
            );

            // Pre-establish all connections in parallel for faster startup
            let mut tasks = Vec::new();
            for i in 0..self.max_connections {
                let server_addr = format!("{}:{}", server.host, server.port);
                let server_name = server.name.clone();
                let pool = Arc::clone(&self.pool);
                let active_connections = Arc::clone(&self.active_connections);

                let task = tokio::spawn(async move {
                    match Self::create_optimized_tcp_stream(&server_addr).await {
                        Ok(stream) => {
                            pool.push(stream);
                            active_connections.fetch_add(1, Ordering::Relaxed);
                            debug!("Pre-established connection {} to {}", i + 1, server_name);
                            Ok(())
                        }
                        Err(e) => {
                            warn!(
                                "Failed to pre-establish connection {} to {}: {}",
                                i + 1,
                                server_name,
                                e
                            );
                            Err(e)
                        }
                    }
                });
                tasks.push(task);
            }

            // Wait for all connections to be established
            for task in tasks {
                let _ = task.await;
            }

            let established = self.active_connections.load(Ordering::Relaxed);
            info!(
                "Successfully pre-established {}/{} connections to {} in parallel",
                established, self.max_connections, server.name
            );
        }
        Ok(())
    }

    /// Get a connection from the pool or create a new one
    pub async fn get_connection(
        &self,
        server: &ServerConfig,
    ) -> Result<PooledConnection> {
        // Pre-establish all connections on first request
        if !self.initialized.load(Ordering::Acquire) {
            self.initialize_connections(server).await?;
        }

        // Try to get a connection from the pool
        if let Some(stream) = self.pool.pop() {
            // For NNTP connections, we should just trust that pooled connections are valid
            // The connection health will be validated during actual usage
            info!("Reusing pooled connection to {}", server.name);
            return Ok(PooledConnection {
                stream,
                server_name: server.name.clone(),
                authenticated: false, // Reset authentication state for safety
            });
        }

        // Create new connection - don't authenticate here, let the caller handle it
        info!("Creating new connection to {} for pooling", server.name);
        let backend_addr = format!("{}:{}", server.host, server.port);
        let stream = Self::create_optimized_tcp_stream(&backend_addr).await?;

        // Return unauthenticated connection - authentication will be handled by caller
        let pooled_conn = PooledConnection::new(
            stream,
            server.name.clone(),
            false,
        );
        Ok(pooled_conn)
    }

    /// Create an optimized TCP stream with performance tuning using socket2
    pub async fn create_optimized_tcp_stream(addr: &str) -> Result<TcpStream, std::io::Error> {
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

    /// Return a connection to the pool
    pub async fn return_connection(&self, conn: PooledConnection) {
        if self.active_connections.load(Ordering::Relaxed) >= self.max_connections {
            info!("Pool is full, closing connection to {}", conn.server_name);
            return; // Pool is full, just drop the connection
        }

        // For NNTP connections, we'll trust that the connection is still valid
        // Connection health will be validated on next use if needed
        info!("Returning connection to {} to pool", conn.server_name);
        self.pool.push(conn.stream);
        // Don't decrement active_connections here since we're keeping the connection
    }
}
