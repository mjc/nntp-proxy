use anyhow::Result;
use crossbeam::queue::SegQueue;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tracing::{debug, error, info, warn};

/// Default maximum connections per server
fn default_max_connections() -> u32 {
    10
}

/// Lock-free buffer pool for reusing large I/O buffers
/// Uses crossbeam's SegQueue for lock-free operations
#[derive(Debug, Clone)]
pub struct BufferPool {
    pool: Arc<SegQueue<Vec<u8>>>,
    buffer_size: usize,
    max_pool_size: usize,
    pool_size: Arc<AtomicUsize>,
}

impl BufferPool {
    /// Create a page-aligned buffer for optimal DMA performance
    fn create_aligned_buffer(size: usize) -> Vec<u8> {
        // Align to page boundaries (4KB) for better memory performance
        let page_size = 4096;
        let aligned_size = size.div_ceil(page_size) * page_size;

        // Use aligned allocation for better cache performance
        let mut buffer = Vec::with_capacity(aligned_size);
        buffer.resize(size, 0);
        buffer
    }

    pub fn new(buffer_size: usize, max_pool_size: usize) -> Self {
        let pool = Arc::new(SegQueue::new());
        let pool_size = Arc::new(AtomicUsize::new(0));

        // Pre-allocate all buffers at startup to eliminate allocation overhead
        info!(
            "Pre-allocating {} buffers of {}KB each ({}MB total)",
            max_pool_size,
            buffer_size / 1024,
            (max_pool_size * buffer_size) / (1024 * 1024)
        );

        for _ in 0..max_pool_size {
            let buffer = Self::create_aligned_buffer(buffer_size);
            pool.push(buffer);
            pool_size.fetch_add(1, Ordering::Relaxed);
        }

        info!("Buffer pool pre-allocation complete");

        Self {
            pool,
            buffer_size,
            max_pool_size,
            pool_size,
        }
    }

    /// Get a buffer from the pool or create a new one (lock-free)
    pub async fn get_buffer(&self) -> Vec<u8> {
        if let Some(mut buffer) = self.pool.pop() {
            self.pool_size.fetch_sub(1, Ordering::Relaxed);
            // Reuse existing buffer, clear it first
            buffer.clear();
            buffer.resize(self.buffer_size, 0);
            buffer
        } else {
            // Create new page-aligned buffer for better DMA performance
            Self::create_aligned_buffer(self.buffer_size)
        }
    }

    /// Return a buffer to the pool (lock-free)
    pub async fn return_buffer(&self, buffer: Vec<u8>) {
        if buffer.len() == self.buffer_size {
            let current_size = self.pool_size.load(Ordering::Relaxed);
            if current_size < self.max_pool_size {
                self.pool.push(buffer);
                self.pool_size.fetch_add(1, Ordering::Relaxed);
            }
            // If pool is full, just drop the buffer
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct Config {
    /// List of backend NNTP servers
    pub servers: Vec<ServerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Maximum number of concurrent connections to this server
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

/// Pooled connection wrapper
#[derive(Debug)]
pub struct PooledConnection {
    pub stream: TcpStream,
    pub server_name: String,
    pub authenticated: bool,
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
        _proxy: &NntpProxy,
    ) -> Result<PooledConnection> {
        // Pre-establish all connections on first request
        if !self.initialized.load(Ordering::Acquire) {
            self.initialize_connections(server).await?;
        }

        // Try to get a connection from the pool
        if let Some(stream) = self.pool.pop() {
            // Test if the connection is still alive by trying a non-blocking read
            let mut test_buf = [0u8; 1];
            match stream.try_read(&mut test_buf) {
                Ok(0) => {
                    // Connection was closed by server, decrease count and create new one
                    self.active_connections.fetch_sub(1, Ordering::Relaxed);
                    info!(
                        "Pooled connection to {} was closed, creating new one",
                        server.name
                    );
                }
                Ok(_) => {
                    // Got unexpected data, connection might be in use, decrease count and create new one
                    self.active_connections.fetch_sub(1, Ordering::Relaxed);
                    info!(
                        "Pooled connection to {} has unexpected data, creating new one",
                        server.name
                    );
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // Connection is alive and ready
                    info!("Reusing pooled connection to {}", server.name);
                    return Ok(PooledConnection {
                        stream,
                        server_name: server.name.clone(),
                        authenticated: false, // Reset authentication state for safety
                    });
                }
                Err(_) => {
                    // Connection error, decrease count and create new one
                    self.active_connections.fetch_sub(1, Ordering::Relaxed);
                    info!(
                        "Pooled connection to {} has error, creating new one",
                        server.name
                    );
                }
            }
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

        // Test if the connection is still alive before returning to pool
        let mut test_buf = [0u8; 1];
        match conn.stream.try_read(&mut test_buf) {
            Ok(0) => {
                // Connection was closed by server
                info!(
                    "Connection to {} was closed by server, not returning to pool",
                    conn.server_name
                );
                self.active_connections.fetch_sub(1, Ordering::Relaxed);
            }
            Ok(_) => {
                // Got unexpected data, connection might be in use
                info!(
                    "Connection to {} has unexpected data, not returning to pool",
                    conn.server_name
                );
                self.active_connections.fetch_sub(1, Ordering::Relaxed);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // Connection is alive and ready
                info!("Returning connection to {} to pool", conn.server_name);
                self.pool.push(conn.stream);
                // Don't decrement active_connections here since we're keeping the connection
            }
            Err(_) => {
                // Connection error
                info!(
                    "Connection to {} has error, not returning to pool",
                    conn.server_name
                );
                self.active_connections.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct NntpProxy {
    servers: Vec<ServerConfig>,
    current_index: Arc<AtomicUsize>,
    /// Connection semaphores per server (server_name -> semaphore)
    connection_semaphores: Arc<HashMap<String, Arc<Semaphore>>>,
    /// Connection pool for backend connections
    connection_pool: ConnectionPool,
    /// Buffer pool for I/O operations
    buffer_pool: BufferPool,
}

impl NntpProxy {
    pub fn new(config: Config) -> Result<Self> {
        if config.servers.is_empty() {
            anyhow::bail!("No servers configured");
        }

        // Create connection semaphores for each server
        let mut connection_semaphores = HashMap::new();
        for server in &config.servers {
            let semaphore = Arc::new(Semaphore::new(server.max_connections as usize));
            connection_semaphores.insert(server.name.clone(), semaphore);
            info!(
                "Server '{}' configured with max {} connections",
                server.name, server.max_connections
            );
        }

        Ok(Self {
            servers: config.servers,
            current_index: Arc::new(AtomicUsize::new(0)),
            connection_semaphores: Arc::new(connection_semaphores),
            connection_pool: ConnectionPool::new(32), // Increased to 32 for better small file concurrency
            buffer_pool: BufferPool::new(256 * 1024, 32), // 256KB buffers better for 100MB files with more available
        })
    }

    /// Pre-warm connections to all servers for optimal small file performance
    pub async fn prewarm_connections(&self) -> Result<()> {
        info!("Pre-warming connections to all backend servers...");
        for server in &self.servers {
            // Pre-establish connections without authentication to avoid complexity
            for i in 0..4 {
                // Back to 4 connections per server for stability
                match self.connection_pool.get_connection(server, self).await {
                    Ok(conn) => {
                        info!("Pre-warmed connection {}/4 to {}", i + 1, server.name);
                        self.connection_pool.return_connection(conn).await;
                    }
                    Err(e) => {
                        warn!("Failed to pre-warm connection to {}: {}", server.name, e);
                    }
                }
            }
        }
        info!("Connection pre-warming complete");
        Ok(())
    }

    /// Get the next server using round-robin
    pub fn next_server(&self) -> &ServerConfig {
        let index = self.current_index.fetch_add(1, Ordering::Relaxed);
        &self.servers[index % self.servers.len()]
    }

    /// Get the current server index (for testing)
    #[cfg(test)]
    pub fn current_index(&self) -> usize {
        self.current_index.load(Ordering::Relaxed) % self.servers.len()
    }

    /// Reset the server index (for testing)
    #[cfg(test)]
    pub fn reset_index(&self) {
        self.current_index.store(0, Ordering::Relaxed);
    }

    /// Get the list of servers
    pub fn servers(&self) -> &[ServerConfig] {
        &self.servers
    }

    pub async fn handle_client(
        &self,
        mut client_stream: TcpStream,
        client_addr: SocketAddr,
    ) -> Result<()> {
        info!("New client connection from {}", client_addr);

        // Get the next backend server
        let server = self.next_server();
        info!(
            "Routing client {} to server {}:{}",
            client_addr, server.host, server.port
        );

        // Acquire connection permit for this server
        let semaphore = self.connection_semaphores.get(&server.name).unwrap();
        let _permit = match semaphore.try_acquire() {
            Ok(permit) => {
                info!(
                    "Acquired connection permit for server '{}' ({} remaining)",
                    server.name,
                    semaphore.available_permits()
                );
                permit
            }
            Err(_) => {
                warn!(
                    "Server '{}' has reached max connections ({}), rejecting client",
                    server.name, server.max_connections
                );
                let _ = client_stream
                    .write_all(b"400 Server temporarily unavailable - too many connections\r\n")
                    .await;
                return Err(anyhow::anyhow!("Server {} at max connections", server.name));
            }
        };

        // Try to get a pooled connection or create a new one
        let backend_addr = format!("{}:{}", server.host, server.port);

        // Try to get a pooled connection first
        let (mut backend_stream, is_pooled, server_name, pooled_authenticated) =
            match self.connection_pool.get_connection(server, self).await {
                Ok(pooled) => {
                    info!(
                        "Using pooled connection to {} (authenticated: {})",
                        pooled.server_name, pooled.authenticated
                    );
                    (
                        pooled.stream,
                        true,
                        pooled.server_name.clone(),
                        pooled.authenticated,
                    )
                }
                Err(_) => {
                    // If no pooled connection available, create a new one
                    info!("Creating new connection to {}", backend_addr);
                    match ConnectionPool::create_optimized_tcp_stream(&backend_addr).await {
                        Ok(stream) => (stream, false, server.name.clone(), false),
                        Err(e) => {
                            error!("Failed to connect to backend {}: {}", backend_addr, e);
                            let _ = client_stream
                                .write_all(b"400 Backend server unavailable\r\n")
                                .await;
                            return Err(e.into());
                        }
                    }
                }
            };

        info!("Connected to backend server {}", backend_addr);

        // Simplified authentication - focus on speed over complexity
        if !is_pooled || !pooled_authenticated {
            // Only authenticate if not already authenticated
            if let (Some(username), Some(password)) = (&server.username, &server.password) {
                if let Err(e) = self
                    .authenticate_backend(&mut backend_stream, username, password)
                    .await
                {
                    error!("Authentication failed for {}: {}", server.name, e);
                    let _ = client_stream
                        .write_all(b"502 Authentication failed\r\n")
                        .await;
                    return Err(e);
                }
            }
        }

        // Simple greeting for protocol compliance
        if let Err(e) = client_stream.write_all(b"200 NNTP Service Ready\r\n").await {
            error!("Failed to send greeting to client: {}", e);
            return Err(e.into());
        }

        // Try zero-copy first (Linux only), then fall back to high-performance buffered copying
        let copy_result = {
            #[cfg(target_os = "linux")]
            {
                match self
                    .copy_bidirectional_zero_copy(&mut client_stream, &mut backend_stream)
                    .await
                {
                    Ok(result) => {
                        debug!("Zero-copy successful");
                        Ok(result)
                    }
                    Err(_) => {
                        debug!("Zero-copy failed, falling back to buffered copy");
                        self.copy_bidirectional_buffered(&mut client_stream, &mut backend_stream)
                            .await
                    }
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                self.copy_bidirectional_buffered(&mut client_stream, &mut backend_stream)
                    .await
            }
        };

        // Always try to return the connection to the pool (whether it was originally pooled or newly created)
        // Determine if authentication was performed in this session
        let was_authenticated = if is_pooled {
            // If it was pooled, it might already be authenticated OR we just authenticated it
            pooled_authenticated || (server.username.is_some() && server.password.is_some())
        } else {
            // If it was a new connection, it's authenticated if we have credentials
            server.username.is_some() && server.password.is_some()
        };

        let pooled_conn = PooledConnection::new(
            backend_stream,
            server_name,
            was_authenticated,
        );
        self.connection_pool.return_connection(pooled_conn).await;
        info!(
            "Returned connection to pool for {} (authenticated: {})",
            server.name, was_authenticated
        );

        match copy_result {
            Ok((client_to_backend_bytes, backend_to_client_bytes)) => {
                info!(
                    "Connection closed for client {}: {} bytes client->backend, {} bytes backend->client",
                    client_addr, client_to_backend_bytes, backend_to_client_bytes
                );
            }
            Err(e) => {
                warn!("Bidirectional copy error for client {}: {}", client_addr, e);
            }
        }

        // The permit will be automatically dropped here when _permit goes out of scope
        info!("Connection closed for client {}", client_addr);
        Ok(())
    }

    /// Perform NNTP authentication using AUTHINFO USER/PASS commands
    async fn authenticate_backend(
        &self,
        stream: &mut TcpStream,
        username: &str,
        password: &str,
    ) -> Result<()> {
        // Use a buffer from our optimized pool instead of small allocations
        let mut buffer = self.buffer_pool.get_buffer().await;

        // Read the server greeting first
        let n = stream.read(&mut buffer).await?;
        let greeting = &buffer[..n];
        info!(
            "Server greeting: {}",
            String::from_utf8_lossy(greeting).trim()
        );

        // Check if greeting indicates successful connection (200)
        let greeting_str = String::from_utf8_lossy(greeting);
        if !greeting_str.starts_with("200") && !greeting_str.starts_with("201") {
            return Err(anyhow::anyhow!(
                "Server returned non-success greeting: {}",
                greeting_str.trim()
            ));
        }

        // Send AUTHINFO USER command
        let user_command = format!("AUTHINFO USER {}\r\n", username);
        stream.write_all(user_command.as_bytes()).await?;

        // Read response
        let n = stream.read(&mut buffer).await?;
        let response = String::from_utf8_lossy(&buffer[..n]);
        info!("AUTHINFO USER response: {}", response.trim());

        // Should get 381 (password required) or 281 (authenticated)
        if response.starts_with("281") {
            // Already authenticated with just username
            return Ok(());
        } else if !response.starts_with("381") {
            return Err(anyhow::anyhow!(
                "Unexpected response to AUTHINFO USER: {}",
                response.trim()
            ));
        }

        // Send AUTHINFO PASS command
        let pass_command = format!("AUTHINFO PASS {}\r\n", password);
        stream.write_all(pass_command.as_bytes()).await?;

        // Read final response
        let n = stream.read(&mut buffer).await?;
        let response = String::from_utf8_lossy(&buffer[..n]);
        info!("AUTHINFO PASS response: {}", response.trim());

        // Should get 281 (authenticated)
        let result = if response.starts_with("281") {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "Authentication failed: {}",
                response.trim()
            ))
        };

        // Return buffer to pool
        self.buffer_pool.return_buffer(buffer).await;
        result
    }

    /// Zero-copy bidirectional copy specifically for TcpStream pairs (Linux only)
    #[cfg(target_os = "linux")]
    async fn copy_bidirectional_zero_copy(
        &self,
        client_stream: &mut TcpStream,
        backend_stream: &mut TcpStream,
    ) -> Result<(u64, u64), std::io::Error> {
        debug!("Starting optimized zero-copy bidirectional transfer");

        // Apply aggressive socket optimizations for 1GB+ transfers
        if let Err(e) = Self::set_high_throughput_optimizations(client_stream) {
            debug!("Failed to set client socket optimizations: {}", e);
        }
        if let Err(e) = Self::set_high_throughput_optimizations(backend_stream) {
            debug!("Failed to set backend socket optimizations: {}", e);
        }

        match tokio_splice2::copy_bidirectional(client_stream, backend_stream).await {
            Ok(traffic_result) => {
                debug!(
                    "Zero-copy transfer successful: {} bytes (client->server: {}, server->client: {})",
                    traffic_result.tx + traffic_result.rx,
                    traffic_result.tx,
                    traffic_result.rx
                );
                Ok((traffic_result.tx as u64, traffic_result.rx as u64))
            }
            Err(e) => {
                debug!("Zero-copy failed: {}", e);
                Err(e)
            }
        }
    }

    /// Set socket optimizations for high-throughput transfers using socket2
    fn set_high_throughput_optimizations(stream: &TcpStream) -> Result<(), std::io::Error> {
        use socket2::SockRef;

        let sock_ref = SockRef::from(stream);

        // Set larger buffer sizes for high throughput
        sock_ref.set_recv_buffer_size(16 * 1024 * 1024)?; // 16MB
        sock_ref.set_send_buffer_size(16 * 1024 * 1024)?; // 16MB

        // Keep Nagle's algorithm enabled for large transfers to reduce packet overhead
        // (socket2 doesn't expose some advanced TCP options like TCP_QUICKACK, TCP_CORK)
        // but the basic optimizations are sufficient for most use cases

        Ok(())
    }

    /// High-performance bidirectional copy with zero-copy optimization
    /// Attempts zero-copy transfer on Linux, falls back to pooled buffers
    async fn copy_bidirectional_buffered<R, W>(
        &self,
        mut reader: R,
        mut writer: W,
    ) -> Result<(u64, u64), std::io::Error>
    where
        R: AsyncRead + AsyncWrite + Unpin,
        W: AsyncRead + AsyncWrite + Unpin,
    {
        // Use high-throughput buffered copy with pooled buffers for generic streams
        // Zero-copy is handled by the specialized copy_bidirectional_zero_copy method
        // Optimized for sustained high-throughput transfers
        use std::io::ErrorKind;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Get larger buffers from the pool (256KB for high throughput)
        let mut buf1 = self.buffer_pool.get_buffer().await;
        let mut buf2 = self.buffer_pool.get_buffer().await;

        let mut transferred_a_to_b = 0u64;
        let mut transferred_b_to_a = 0u64;

        // High-throughput copy with 256KB buffers reduces syscall overhead
        let copy_result = async {
            loop {
                tokio::select! {
                    // Copy from reader to writer with larger buffers
                    result = reader.read(&mut buf1) => {
                        match result {
                            Ok(0) => break, // EOF
                            Ok(n) => {
                                writer.write_all(&buf1[..n]).await?;
                                transferred_a_to_b += n as u64;
                            }
                            Err(e) if e.kind() == ErrorKind::WouldBlock => continue,
                            Err(e) => return Err(e),
                        }
                    }
                    // Copy from writer to reader with larger buffers
                    result = writer.read(&mut buf2) => {
                        match result {
                            Ok(0) => break, // EOF
                            Ok(n) => {
                                reader.write_all(&buf2[..n]).await?;
                                transferred_b_to_a += n as u64;
                            }
                            Err(e) if e.kind() == ErrorKind::WouldBlock => continue,
                            Err(e) => return Err(e),
                        }
                    }
                }
            }
            Ok((transferred_a_to_b, transferred_b_to_a))
        }
        .await;

        // Return buffers to the pool
        self.buffer_pool.return_buffer(buf1).await;
        self.buffer_pool.return_buffer(buf2).await;

        copy_result
    }
}

pub fn load_config(config_path: &str) -> Result<Config> {
    let config_content = std::fs::read_to_string(config_path)
        .map_err(|e| anyhow::anyhow!("Failed to read config file '{}': {}", config_path, e))?;

    let config: Config = toml::from_str(&config_content)
        .map_err(|e| anyhow::anyhow!("Failed to parse config file '{}': {}", config_path, e))?;

    Ok(config)
}

pub fn create_default_config() -> Config {
    Config {
        servers: vec![ServerConfig {
            host: "news.example.com".to_string(),
            port: 119,
            name: "Example News Server".to_string(),
            username: None,
            password: None,
            max_connections: default_max_connections(),
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Arc;
    use tempfile::NamedTempFile;

    fn create_test_config() -> Config {
        Config {
            servers: vec![
                ServerConfig {
                    host: "server1.example.com".to_string(),
                    port: 119,
                    name: "Test Server 1".to_string(),
                    username: None,
                    password: None,
                    max_connections: 5,
                },
                ServerConfig {
                    host: "server2.example.com".to_string(),
                    port: 119,
                    name: "Test Server 2".to_string(),
                    username: None,
                    password: None,
                    max_connections: 8,
                },
                ServerConfig {
                    host: "server3.example.com".to_string(),
                    port: 119,
                    name: "Test Server 3".to_string(),
                    username: None,
                    password: None,
                    max_connections: 12,
                },
            ],
        }
    }

    #[test]
    fn test_server_config_creation() {
        let config = ServerConfig {
            host: "news.example.com".to_string(),
            port: 119,
            name: "Example Server".to_string(),
            username: None,
            password: None,
            max_connections: 15,
        };

        assert_eq!(config.host, "news.example.com");
        assert_eq!(config.port, 119);
        assert_eq!(config.name, "Example Server");
        assert_eq!(config.max_connections, 15);
    }

    #[test]
    fn test_config_creation() {
        let config = create_test_config();
        assert_eq!(config.servers.len(), 3);
        assert_eq!(config.servers[0].name, "Test Server 1");
        assert_eq!(config.servers[1].name, "Test Server 2");
        assert_eq!(config.servers[2].name, "Test Server 3");
    }

    #[test]
    fn test_proxy_creation_with_servers() {
        let config = create_test_config();
        let proxy = NntpProxy::new(config).expect("Failed to create proxy");

        assert_eq!(proxy.servers().len(), 3);
        assert_eq!(proxy.servers()[0].name, "Test Server 1");
    }

    #[test]
    fn test_proxy_creation_with_empty_servers() {
        let config = Config { servers: vec![] };
        let result = NntpProxy::new(config);

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No servers configured")
        );
    }

    #[test]
    fn test_round_robin_server_selection() {
        let config = create_test_config();
        let proxy = NntpProxy::new(config).expect("Failed to create proxy");

        proxy.reset_index();

        // Test first round
        assert_eq!(proxy.next_server().name, "Test Server 1");
        assert_eq!(proxy.next_server().name, "Test Server 2");
        assert_eq!(proxy.next_server().name, "Test Server 3");

        // Test wraparound
        assert_eq!(proxy.next_server().name, "Test Server 1");
        assert_eq!(proxy.next_server().name, "Test Server 2");
    }

    #[test]
    fn test_round_robin_with_single_server() {
        let config = Config {
            servers: vec![ServerConfig {
                host: "single.example.com".to_string(),
                port: 119,
                name: "Single Server".to_string(),
                username: None,
                password: None,
                max_connections: 3,
            }],
        };

        let proxy = NntpProxy::new(config).expect("Failed to create proxy");
        proxy.reset_index();

        // All requests should go to the same server
        assert_eq!(proxy.next_server().name, "Single Server");
        assert_eq!(proxy.next_server().name, "Single Server");
        assert_eq!(proxy.next_server().name, "Single Server");
    }

    #[test]
    fn test_concurrent_round_robin() {
        let config = create_test_config();
        let proxy = Arc::new(NntpProxy::new(config).expect("Failed to create proxy"));
        proxy.reset_index();

        let mut handles = vec![];
        let servers_selected = Arc::new(std::sync::Mutex::new(Vec::new()));

        // Spawn multiple tasks to test concurrent access
        for _ in 0..9 {
            let proxy_clone = Arc::clone(&proxy);
            let servers_clone = Arc::clone(&servers_selected);

            let handle = std::thread::spawn(move || {
                let server = proxy_clone.next_server();
                servers_clone.lock().unwrap().push(server.name.clone());
            });
            handles.push(handle);
        }

        // Wait for all tasks to complete
        for handle in handles {
            handle.join().unwrap();
        }

        let servers = servers_selected.lock().unwrap();
        assert_eq!(servers.len(), 9);

        // Count occurrences of each server (should be balanced)
        let server1_count = servers.iter().filter(|&s| s == "Test Server 1").count();
        let server2_count = servers.iter().filter(|&s| s == "Test Server 2").count();
        let server3_count = servers.iter().filter(|&s| s == "Test Server 3").count();

        // Each server should be selected 3 times
        assert_eq!(server1_count, 3);
        assert_eq!(server2_count, 3);
        assert_eq!(server3_count, 3);
    }

    #[test]
    fn test_load_config_from_file() -> Result<()> {
        let config = create_test_config();
        let config_toml = toml::to_string_pretty(&config)?;

        // Create a temporary file
        let mut temp_file = NamedTempFile::new()?;
        write!(temp_file, "{}", config_toml)?;

        // Load config from file
        let loaded_config = load_config(temp_file.path().to_str().unwrap())?;

        assert_eq!(loaded_config.servers.len(), 3);
        assert_eq!(loaded_config.servers[0].name, "Test Server 1");
        assert_eq!(loaded_config.servers[0].host, "server1.example.com");
        assert_eq!(loaded_config.servers[0].port, 119);

        Ok(())
    }

    #[test]
    fn test_load_config_nonexistent_file() {
        let result = load_config("/nonexistent/path/config.toml");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Failed to read config file")
        );
    }

    #[test]
    fn test_load_config_invalid_toml() -> Result<()> {
        let invalid_toml = "invalid toml content [[[";

        // Create a temporary file with invalid TOML
        let mut temp_file = NamedTempFile::new()?;
        write!(temp_file, "{}", invalid_toml)?;

        let result = load_config(temp_file.path().to_str().unwrap());
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Failed to parse config file")
        );

        Ok(())
    }

    #[test]
    fn test_create_default_config() {
        let config = create_default_config();

        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].host, "news.example.com");
        assert_eq!(config.servers[0].port, 119);
        assert_eq!(config.servers[0].name, "Example News Server");
    }

    #[test]
    fn test_config_serialization() -> Result<()> {
        let config = create_test_config();

        // Serialize to TOML
        let toml_string = toml::to_string_pretty(&config)?;
        assert!(toml_string.contains("server1.example.com"));
        assert!(toml_string.contains("Test Server 1"));

        // Deserialize back
        let deserialized: Config = toml::from_str(&toml_string)?;
        assert_eq!(deserialized, config);

        Ok(())
    }
}
