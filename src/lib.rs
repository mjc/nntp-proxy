use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, error, info, warn};

mod pool;
use pool::{BufferPool, ConnectionProvider, DeadpoolConnectionProvider};

/// Buffer configuration constants
const BUFFER_SIZE: usize = 256 * 1024; // 256KB buffers for high throughput
const BUFFER_POOL_SIZE: usize = 32; // Number of buffers in pool
const HIGH_THROUGHPUT_BUFFER_SIZE: usize = 256 * 1024; // 256KB for direct allocation

/// Connection pool configuration constants
const PREWARMING_BATCH_SIZE: usize = 5; // Create connections in batches of 5
const BATCH_DELAY_MS: u64 = 100; // Wait 100ms between prewarming batches

/// TCP socket buffer sizes for high-throughput transfers
const HIGH_THROUGHPUT_RECV_BUFFER: usize = 16 * 1024 * 1024; // 16MB
const HIGH_THROUGHPUT_SEND_BUFFER: usize = 16 * 1024 * 1024; // 16MB

/// NNTP response constants
const NNTP_PASSWORD_REQUIRED: &[u8] = b"381 Password required\r\n";
const NNTP_AUTH_ACCEPTED: &[u8] = b"281 Authentication accepted\r\n";
const NNTP_BACKEND_UNAVAILABLE: &[u8] = b"400 Backend server unavailable\r\n";
const NNTP_AUTH_FAILED: &[u8] = b"502 Authentication failed\r\n";

/// NNTP command classification for different handling strategies
#[derive(Debug, PartialEq)]
enum NntpCommand {
    /// Authentication commands (AUTHINFO USER/PASS) - intercepted locally
    AuthUser,
    AuthPass,
    /// Data retrieval commands that may trigger high-throughput mode
    DataRetrieval,
    /// Other commands that are forwarded normally
    Other,
}

impl NntpCommand {
    /// Classify an NNTP command based on its content
    fn classify(command: &str) -> Self {
        let trimmed = command.trim();

        if trimmed.starts_with("AUTHINFO USER") {
            Self::AuthUser
        } else if trimmed.starts_with("AUTHINFO PASS") {
            Self::AuthPass
        } else if trimmed.starts_with("ARTICLE")
            || trimmed.starts_with("BODY")
            || trimmed.starts_with("HEAD")
            || trimmed.starts_with("STAT")
        {
            Self::DataRetrieval
        } else {
            Self::Other
        }
    }
}

/// Default maximum connections per server
fn default_max_connections() -> u32 {
    10
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

#[derive(Clone, Debug)]
pub struct NntpProxy {
    servers: Vec<ServerConfig>,
    current_index: Arc<AtomicUsize>,
    /// Connection providers per server - easily swappable implementation
    connection_providers: Vec<DeadpoolConnectionProvider>,
    /// Buffer pool for I/O operations
    buffer_pool: BufferPool,
    /// Track if pools have been prewarmed to avoid redundant prewarming
    pools_prewarmed: Arc<std::sync::atomic::AtomicBool>,
}

impl NntpProxy {
    pub fn new(config: Config) -> Result<Self> {
        if config.servers.is_empty() {
            anyhow::bail!("No servers configured");
        }

        // Create deadpool connection providers for each server
        let connection_providers: Vec<DeadpoolConnectionProvider> = config
            .servers
            .iter()
            .map(|server| {
                info!(
                    "Configuring deadpool connection provider for '{}'",
                    server.name
                );
                DeadpoolConnectionProvider::new(
                    server.host.clone(),
                    server.port,
                    server.name.clone(),
                    server.max_connections as usize,
                )
            })
            .collect();

        Ok(Self {
            servers: config.servers,
            current_index: Arc::new(AtomicUsize::new(0)),
            connection_providers,
            buffer_pool: BufferPool::new(BUFFER_SIZE, BUFFER_POOL_SIZE),
            pools_prewarmed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    /// Prewarm all connection pools by forcing pool to create max_connections
    /// Called on first client connection after idle period
    async fn prewarm_all_pools(&self) -> Result<()> {
        // Use compare_and_swap to ensure only one thread prewarms the pools
        if self
            .pools_prewarmed
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            // Another thread is already prewarming or has already prewarmed
            return Ok(());
        }

        info!("Prewarming all connection pools on first client connection...");

        // Spawn tasks to prewarm each pool concurrently
        let mut handles = Vec::new();

        for (i, server) in self.servers.iter().enumerate() {
            let provider = self.connection_providers[i].clone();
            let server_name = server.name.clone();
            let max_connections = server.max_connections as usize;

            let handle = tokio::spawn(async move {
                info!(
                    "Prewarming pool for '{}' with {} connections",
                    server_name, max_connections
                );

                // Limit concurrent prewarming to avoid overwhelming the server
                let mut total_created = 0;

                for batch_start in (0..max_connections).step_by(PREWARMING_BATCH_SIZE) {
                    let batch_end =
                        std::cmp::min(batch_start + PREWARMING_BATCH_SIZE, max_connections);
                    let mut batch_connections = Vec::new();

                    // Create batch connections concurrently
                    for i in batch_start..batch_end {
                        match provider.get_pooled_connection().await {
                            Ok(conn) => {
                                debug!(
                                    "Created prewarmed connection {}/{} for '{}'",
                                    i + 1,
                                    max_connections,
                                    server_name
                                );
                                batch_connections.push(conn);
                                total_created += 1;
                            }
                            Err(e) => {
                                warn!(
                                    "Failed to prewarm connection {}/{} for '{}': {}",
                                    i + 1,
                                    max_connections,
                                    server_name,
                                    e
                                );
                                // Continue with remaining connections in batch
                            }
                        }
                    }

                    // Drop batch connections (return to pool)
                    drop(batch_connections);

                    // Wait between batches to avoid overwhelming server
                    if batch_end < max_connections {
                        tokio::time::sleep(std::time::Duration::from_millis(BATCH_DELAY_MS)).await;
                    }
                }

                info!(
                    "Pool prewarming completed for '{}' - {}/{} connections ready",
                    server_name, total_created, max_connections
                );
                Ok::<(), anyhow::Error>(())
            });

            handles.push(handle);
        }

        // Wait for all prewarming tasks to complete
        for handle in handles {
            if let Err(e) = handle.await {
                warn!("Pool prewarming task failed: {}", e);
            }
        }

        // Mark pools as prewarmed
        self.pools_prewarmed.store(true, Ordering::Relaxed);
        info!("All connection pools have been prewarmed and are ready for use");

        Ok(())
    }
    pub async fn prewarm_connections(&self) -> Result<()> {
        info!("Testing connections to all backend servers...");
        for (i, server) in self.servers.iter().enumerate() {
            let provider = &self.connection_providers[i];
            // Test connection to ensure servers are reachable
            match provider.get_connection().await {
                Ok(_) => {
                    debug!("Successfully tested connection to {}", server.name);
                }
                Err(e) => {
                    warn!("Failed to test connection to {}: {}", server.name, e);
                }
            }
        }
        info!("Connection testing complete");
        Ok(())
    }

    /// Get the next server using round-robin
    pub fn next_server(&self) -> &ServerConfig {
        let index = self.current_index.fetch_add(1, Ordering::Relaxed);
        &self.servers[index % self.servers.len()]
    }

    /// Gracefully shutdown all connection pools
    pub async fn graceful_shutdown(&self) {
        info!("Initiating graceful shutdown of all connection pools...");

        for provider in &self.connection_providers {
            provider.graceful_shutdown().await;
        }

        info!("All connection pools have been shut down gracefully");
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
        debug!("New client connection from {}", client_addr);

        // Get the next backend server
        let server_idx = self.current_index.fetch_add(1, Ordering::Relaxed) % self.servers.len();
        let server = &self.servers[server_idx];
        info!(
            "Routing client {} to server {}:{}",
            client_addr, server.host, server.port
        );

        // Prewarm all connection pools on first connection after idle
        // This ensures subsequent requests can immediately use pooled connections
        if let Err(e) = self.prewarm_all_pools().await {
            warn!("Failed to prewarm connection pools: {}", e);
            // Continue anyway - this is an optimization, not a requirement
        }

        // Get the connection manager for this server and create connection
        let mut backend_stream = match self.connection_providers[server_idx].get_connection().await
        {
            Ok(stream) => {
                debug!("Retrieved connection from pool for {}", server.name);
                stream
            }
            Err(e) => {
                error!("Failed to connect to {}: {}", server.name, e);
                // Don't send greeting on connection failure - send error directly
                let _ = client_stream.write_all(NNTP_BACKEND_UNAVAILABLE).await;
                return Err(e);
            }
        };

        let backend_addr = format!("{}:{}", server.host, server.port);
        debug!("Connected to backend server {}", backend_addr);

        // Handle authentication and greeting forwarding
        if let (Some(username), Some(password)) = (&server.username, &server.password) {
            if let Err(e) = self
                .authenticate_backend_and_forward_greeting(
                    &mut backend_stream,
                    &mut client_stream,
                    username,
                    password,
                )
                .await
            {
                error!("Authentication failed for {}: {}", server.name, e);
                let _ = client_stream.write_all(NNTP_AUTH_FAILED).await;
                return Err(e);
            }
            debug!("Successfully authenticated connection to {}", server.name);
        } else {
            // No authentication needed, just read and forward the greeting
            if let Err(e) = self
                .forward_backend_greeting(&mut backend_stream, &mut client_stream)
                .await
            {
                error!("Failed to forward greeting from {}: {}", server.name, e);
                let _ = client_stream.write_all(NNTP_BACKEND_UNAVAILABLE).await;
                return Err(e);
            }
        }

        // Now implement intelligent proxying that handles client authentication
        // without passing it to the already-authenticated backend
        let copy_result = self
            .handle_client_with_auth_interception(client_stream, backend_stream)
            .await;

        // Connection will be automatically closed when backend_stream goes out of scope
        debug!("Connection to {} will be closed", server.name);

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
        debug!("Connection closed for client {}", client_addr);
        Ok(())
    }

    /// Forward the backend server's greeting to the client
    async fn forward_backend_greeting(
        &self,
        backend_stream: &mut TcpStream,
        client_stream: &mut TcpStream,
    ) -> Result<()> {
        let mut buffer = self.buffer_pool.get_buffer().await;

        // Read the server greeting
        let n = backend_stream.read(&mut buffer).await?;
        let greeting = &buffer[..n];
        let greeting_str = String::from_utf8_lossy(greeting);
        debug!("Backend greeting: {}", greeting_str.trim());

        if !greeting_str.starts_with("200") && !greeting_str.starts_with("201") {
            let error_msg = greeting_str.trim().to_string();
            self.buffer_pool.return_buffer(buffer).await;
            return Err(anyhow::anyhow!(
                "Server returned non-success greeting: {}",
                error_msg
            ));
        }

        // Forward greeting to client
        client_stream.write_all(greeting).await?;

        self.buffer_pool.return_buffer(buffer).await;
        Ok(())
    }

    /// Perform NNTP authentication and forward the final greeting to client
    async fn authenticate_backend_and_forward_greeting(
        &self,
        backend_stream: &mut TcpStream,
        client_stream: &mut TcpStream,
        username: &str,
        password: &str,
    ) -> Result<()> {
        // Use a buffer from our optimized pool instead of small allocations
        let mut buffer = self.buffer_pool.get_buffer().await;

        // Read the server greeting first and forward it
        let n = backend_stream.read(&mut buffer).await?;
        let greeting = &buffer[..n];
        let greeting_str = String::from_utf8_lossy(greeting);
        debug!("Backend greeting: {}", greeting_str.trim());

        if !greeting_str.starts_with("200") && !greeting_str.starts_with("201") {
            let error_msg = greeting_str.trim().to_string();
            self.buffer_pool.return_buffer(buffer).await;
            return Err(anyhow::anyhow!(
                "Server returned non-success greeting: {}",
                error_msg
            ));
        }

        // Forward greeting to client immediately
        client_stream.write_all(greeting).await?;

        // Now perform authentication on backend
        // Send AUTHINFO USER command
        let user_command = format!("AUTHINFO USER {}\r\n", username);
        backend_stream.write_all(user_command.as_bytes()).await?;

        // Read response
        let n = backend_stream.read(&mut buffer).await?;
        let response = String::from_utf8_lossy(&buffer[..n]);
        debug!("AUTHINFO USER response: {}", response.trim());

        // Should get 381 (password required) or 281 (authenticated)
        if response.starts_with("281") {
            // Already authenticated with just username
            self.buffer_pool.return_buffer(buffer).await;
            return Ok(());
        } else if !response.starts_with("381") {
            let error_msg = response.trim().to_string();
            self.buffer_pool.return_buffer(buffer).await;
            return Err(anyhow::anyhow!(
                "Unexpected response to AUTHINFO USER: {}",
                error_msg
            ));
        }

        // Send AUTHINFO PASS command
        let pass_command = format!("AUTHINFO PASS {}\r\n", password);
        backend_stream.write_all(pass_command.as_bytes()).await?;

        // Read final response
        let n = backend_stream.read(&mut buffer).await?;
        let response = String::from_utf8_lossy(&buffer[..n]);
        debug!("AUTHINFO PASS response: {}", response.trim());

        // Should get 281 (authenticated)
        let result = if response.starts_with("281") {
            Ok(())
        } else {
            let error_msg = response.trim().to_string();
            Err(anyhow::anyhow!("Authentication failed: {}", error_msg))
        };

        // Return buffer to pool
        self.buffer_pool.return_buffer(buffer).await;
        result
    }

    /// Handle client connection with authentication interception
    /// Client authenticates to proxy, proxy uses backend connection already authenticated
    async fn handle_client_with_auth_interception(
        &self,
        mut client_stream: TcpStream,
        mut backend_stream: TcpStream,
    ) -> Result<(u64, u64), anyhow::Error> {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

        // Apply high-throughput socket optimizations for large transfers
        if let Err(e) = Self::apply_high_throughput_optimizations(&client_stream, &backend_stream) {
            debug!("Failed to apply high-throughput optimizations: {}", e);
        }

        // Split streams for independent read/write
        let (client_read, mut client_write) = client_stream.split();
        let (mut backend_read, mut backend_write) = backend_stream.split();
        let mut client_reader = BufReader::new(client_read);

        let mut client_to_backend_bytes = 0u64;
        let mut backend_to_client_bytes = 0u64;

        // Handle the initial command/response phase where we intercept auth
        loop {
            let mut line = String::new();
            let mut buffer = self.buffer_pool.get_buffer().await;

            tokio::select! {
                // Read command from client
                result = client_reader.read_line(&mut line) => {
                    match result {
                        Ok(0) => {
                            self.buffer_pool.return_buffer(buffer).await;
                            break; // Client disconnected
                        }
                        Ok(_) => {
                            let trimmed = line.trim();
                            debug!("Client command: {}", trimmed);

                            // Classify and handle command based on type
                            match NntpCommand::classify(&line) {
                                NntpCommand::AuthUser => {
                                    // Client is trying to authenticate - respond positively
                                    client_write.write_all(NNTP_PASSWORD_REQUIRED).await?;
                                    backend_to_client_bytes += NNTP_PASSWORD_REQUIRED.len() as u64;
                                    debug!("Intercepted AUTHINFO USER, sent password request");
                                }
                                NntpCommand::AuthPass => {
                                    // Client is providing password - accept it
                                    client_write.write_all(NNTP_AUTH_ACCEPTED).await?;
                                    backend_to_client_bytes += NNTP_AUTH_ACCEPTED.len() as u64;
                                    debug!("Intercepted AUTHINFO PASS, authenticated client");
                                }
                                NntpCommand::DataRetrieval => {
                                    // Forward data command to backend
                                    backend_write.write_all(line.as_bytes()).await?;
                                    client_to_backend_bytes += line.len() as u64;
                                    debug!("Forwarding data command, switching to high-throughput mode");

                                    // Return the buffer before transitioning
                                    self.buffer_pool.return_buffer(buffer).await;

                                    // For high-throughput data transfer, use our optimized copy
                                    return self.handle_high_throughput_transfer(
                                        client_reader, client_write,
                                        backend_read, backend_write,
                                        client_to_backend_bytes,
                                        backend_to_client_bytes
                                    ).await;
                                }
                                NntpCommand::Other => {
                                    // Forward other commands to backend
                                    backend_write.write_all(line.as_bytes()).await?;
                                    client_to_backend_bytes += line.len() as u64;
                                }
                            }
                        }
                        Err(e) => {
                            warn!("Error reading from client: {}", e);
                            self.buffer_pool.return_buffer(buffer).await;
                            break;
                        }
                    }
                }

                // Read response from backend and forward to client (for non-auth commands)
                result = backend_read.read(&mut buffer) => {
                    match result {
                        Ok(0) => {
                            self.buffer_pool.return_buffer(buffer).await;
                            break; // Backend disconnected
                        }
                        Ok(n) => {
                            client_write.write_all(&buffer[..n]).await?;
                            backend_to_client_bytes += n as u64;
                        }
                        Err(e) => {
                            warn!("Error reading from backend: {}", e);
                            self.buffer_pool.return_buffer(buffer).await;
                            break;
                        }
                    }
                }
            }

            self.buffer_pool.return_buffer(buffer).await;
        }

        Ok((client_to_backend_bytes, backend_to_client_bytes))
    }

    /// Handle high-throughput data transfer after authentication is complete
    async fn handle_high_throughput_transfer(
        &self,
        mut client_reader: tokio::io::BufReader<tokio::net::tcp::ReadHalf<'_>>,
        mut client_write: tokio::net::tcp::WriteHalf<'_>,
        mut backend_read: tokio::net::tcp::ReadHalf<'_>,
        mut backend_write: tokio::net::tcp::WriteHalf<'_>,
        mut client_to_backend_bytes: u64,
        mut backend_to_client_bytes: u64,
    ) -> Result<(u64, u64), anyhow::Error> {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

        // Note: We would apply high-throughput optimizations here, but we need the original streams
        // The optimizations are best applied during connection creation in the deadpool manager
        debug!("Starting high-throughput data transfer");

        // Use direct buffer allocation for high-throughput to avoid pool overhead
        let mut direct_buffer = vec![0u8; HIGH_THROUGHPUT_BUFFER_SIZE];

        // Continue handling commands and large data responses
        loop {
            let mut line = String::new();

            tokio::select! {
                // Continue reading client commands
                result = client_reader.read_line(&mut line) => {
                    match result {
                        Ok(0) => break,
                        Ok(_) => {
                            // Forward all commands including QUIT to backend
                            // This maintains proper NNTP protocol flow
                            backend_write.write_all(line.as_bytes()).await?;
                            client_to_backend_bytes += line.len() as u64;
                        }
                        Err(e) => {
                            warn!("Error reading client command: {}", e);
                            break;
                        }
                    }
                }

                // Read large responses from backend with direct buffer (no pool overhead)
                result = backend_read.read(&mut direct_buffer) => {
                    match result {
                        Ok(0) => {
                            break;
                        }
                        Ok(n) => {
                            client_write.write_all(&direct_buffer[..n]).await?;
                            backend_to_client_bytes += n as u64;

                            // For very large transfers, ensure we keep reading efficiently
                            if n == direct_buffer.len() {
                                // Buffer was full, likely more data coming
                                // Continue optimized reading...
                            }
                        }
                        Err(e) => {
                            warn!("Error reading backend response: {}", e);
                            break;
                        }
                    }
                }
            }
        }

        Ok((client_to_backend_bytes, backend_to_client_bytes))
    }

    /// Set socket optimizations for high-throughput transfers using socket2
    fn set_high_throughput_optimizations(stream: &TcpStream) -> Result<(), std::io::Error> {
        use socket2::SockRef;

        let sock_ref = SockRef::from(stream);

        // Set larger buffer sizes for high throughput
        sock_ref.set_recv_buffer_size(HIGH_THROUGHPUT_RECV_BUFFER)?;
        sock_ref.set_send_buffer_size(HIGH_THROUGHPUT_SEND_BUFFER)?;

        // Keep Nagle's algorithm enabled for large transfers to reduce packet overhead
        // (socket2 doesn't expose some advanced TCP options like TCP_QUICKACK, TCP_CORK)
        // but the basic optimizations are sufficient for most use cases

        Ok(())
    }

    /// Apply aggressive socket optimizations for 1GB+ transfers
    pub fn apply_high_throughput_optimizations(
        client_stream: &TcpStream,
        backend_stream: &TcpStream,
    ) -> Result<(), std::io::Error> {
        debug!("Applying high-throughput socket optimizations");

        if let Err(e) = Self::set_high_throughput_optimizations(client_stream) {
            debug!("Failed to set client socket optimizations: {}", e);
        }
        if let Err(e) = Self::set_high_throughput_optimizations(backend_stream) {
            debug!("Failed to set backend socket optimizations: {}", e);
        }

        Ok(())
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

#[cfg(test)]
mod command_tests {
    use super::*;

    #[test]
    fn test_nntp_command_classification() {
        // Test authentication commands
        assert_eq!(
            NntpCommand::classify("AUTHINFO USER testuser"),
            NntpCommand::AuthUser
        );
        assert_eq!(
            NntpCommand::classify("AUTHINFO PASS testpass"),
            NntpCommand::AuthPass
        );
        assert_eq!(
            NntpCommand::classify("  AUTHINFO USER  whitespace  "),
            NntpCommand::AuthUser
        );

        // Test data retrieval commands
        assert_eq!(
            NntpCommand::classify("ARTICLE 12345"),
            NntpCommand::DataRetrieval
        );
        assert_eq!(
            NntpCommand::classify("BODY <message@example.com>"),
            NntpCommand::DataRetrieval
        );
        assert_eq!(
            NntpCommand::classify("HEAD 67890"),
            NntpCommand::DataRetrieval
        );
        assert_eq!(NntpCommand::classify("STAT"), NntpCommand::DataRetrieval);

        // Test other commands
        assert_eq!(NntpCommand::classify("HELP"), NntpCommand::Other);
        assert_eq!(NntpCommand::classify("LIST"), NntpCommand::Other);
        assert_eq!(NntpCommand::classify("GROUP alt.test"), NntpCommand::Other);
        assert_eq!(NntpCommand::classify("QUIT"), NntpCommand::Other);
        assert_eq!(NntpCommand::classify("UNKNOWN COMMAND"), NntpCommand::Other);
    }
}
