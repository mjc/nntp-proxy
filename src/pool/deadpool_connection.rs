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
    username: Option<String>,
    password: Option<String>,
}

impl TcpManager {
    pub fn new(host: String, port: u16, name: String, username: Option<String>, password: Option<String>) -> Self {
        Self { host, port, name, username, password }
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
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        
        debug!("Creating new TCP connection to {} for deadpool", self.name);
        let mut stream = self.create_optimized_tcp_stream().await?;
        
        // Read and consume the greeting
        let mut buffer = vec![0u8; 4096];
        let n = stream.read(&mut buffer).await?;
        let greeting = String::from_utf8_lossy(&buffer[..n]);
        debug!("Pool connection greeting for {}: {}", self.name, greeting.trim());
        
        if !greeting.starts_with("200") && !greeting.starts_with("201") {
            return Err(anyhow::anyhow!("Backend returned non-success greeting: {}", greeting.trim()));
        }
        
        // Authenticate if credentials are provided
        if let Some(username) = &self.username {
            // Send AUTHINFO USER
            let user_command = format!("AUTHINFO USER {}\r\n", username);
            stream.write_all(user_command.as_bytes()).await?;
            
            // Read response
            let n = stream.read(&mut buffer).await?;
            let response = String::from_utf8_lossy(&buffer[..n]);
            debug!("Pool connection AUTHINFO USER response for {}: {}", self.name, response.trim());
            
            if response.starts_with("281") {
                // Already authenticated with just username
                debug!("Pool connection for {} authenticated with username only", self.name);
            } else if response.starts_with("381") {
                // Password required
                if let Some(password) = &self.password {
                    let pass_command = format!("AUTHINFO PASS {}\r\n", password);
                    stream.write_all(pass_command.as_bytes()).await?;
                    
                    // Read final auth response
                    let n = stream.read(&mut buffer).await?;
                    let response = String::from_utf8_lossy(&buffer[..n]);
                    debug!("Pool connection AUTHINFO PASS response for {}: {}", self.name, response.trim());
                    
                    if !response.starts_with("281") {
                        return Err(anyhow::anyhow!("Authentication failed for {}: {}", self.name, response.trim()));
                    }
                    debug!("Pool connection for {} authenticated successfully", self.name);
                } else {
                    return Err(anyhow::anyhow!("Password required but not provided for {}", self.name));
                }
            } else {
                return Err(anyhow::anyhow!("Unexpected AUTHINFO USER response for {}: {}", self.name, response.trim()));
            }
        }
        
        Ok(stream)
    }

    async fn recycle(
        &self,
        conn: &mut TcpStream,
        _: &managed::Metrics,
    ) -> managed::RecycleResult<anyhow::Error> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::time::{timeout, Duration};
        
        // Quick health check to detect stale connections (idle timeout on backend)
        // Send DATE command which is stateless and has minimal overhead
        debug!("Recycling connection to {} - performing health check", self.name);
        
        // Set a reasonable timeout for the health check (3 seconds)
        // This balances detecting dead connections vs network latency
        let health_check = async {
            // Send DATE command
            conn.write_all(b"DATE\r\n").await?;
            conn.flush().await?;

            // Read response
            let mut reader = BufReader::new(conn);
            let mut response = String::new();
            reader.read_line(&mut response).await?;

            // Check if response indicates success (111 response)
            if response.starts_with("111") {
                debug!("Connection to {} is healthy", self.name);
                Ok(())
            } else {
                warn!("Connection to {} returned unexpected DATE response: {}", self.name, response.trim());
                Err(anyhow::anyhow!("Unexpected DATE response: {}", response.trim()))
            }
        };
        
        match timeout(Duration::from_secs(3), health_check).await {
            Ok(Ok(())) => {
                debug!("Connection to {} recycled successfully", self.name);
                Ok(())
            }
            Ok(Err(e)) => {
                warn!("Connection to {} failed health check: {}", self.name, e);
                Err(managed::RecycleError::Message(format!("Health check failed: {}", e).into()))
            }
            Err(_) => {
                warn!("Connection to {} health check timed out after 3s", self.name);
                Err(managed::RecycleError::Message("Health check timeout".into()))
            }
        }
    }

    fn detach(&self, _conn: &mut TcpStream) {
        // Connection is being removed from the pool
        // For simplicity, we'll let the connection drop naturally
        // The main graceful shutdown will handle QUIT commands
        debug!("Connection detached from pool for {}", self.name);
    }
}

type Pool = managed::Pool<TcpManager>;

/// Deadpool-based connection provider for high-performance connection pooling
#[derive(Debug, Clone)]
pub struct DeadpoolConnectionProvider {
    pool: Pool,
    name: String,
    username: Option<String>,
    password: Option<String>,
}

impl DeadpoolConnectionProvider {
    pub fn new(host: String, port: u16, name: String, max_size: usize, username: Option<String>, password: Option<String>) -> Self {
        let manager = TcpManager::new(host, port, name.clone(), username.clone(), password.clone());
        let pool = Pool::builder(manager)
            .max_size(max_size)
            .build()
            .expect("Failed to create connection pool");

        info!(
            "Created deadpool connection provider for '{}' with max {} connections",
            name, max_size
        );

        Self { pool, name, username, password }
    }

    /// Get a connection from the pool that is automatically returned when dropped
    /// This is the preferred method - connections stay in pool and are reused
    pub async fn get_pooled_connection(&self) -> Result<managed::Object<TcpManager>> {
        debug!(
            "Getting pooled connection from {}",
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
    fn status(&self) -> PoolStatus {
        let status = self.pool.status();
        PoolStatus {
            available: status.available,
            max_size: status.max_size,
            created: status.size,
        }
    }
}
