use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, error, info, warn};

mod pool;
use pool::{BufferPool, ConnectionManager};

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
    /// Connection managers per server (server_name -> manager)
    connection_managers: Arc<HashMap<String, ConnectionManager>>,
    /// Buffer pool for I/O operations
    buffer_pool: BufferPool,
}

impl NntpProxy {
    pub fn new(config: Config) -> Result<Self> {
        if config.servers.is_empty() {
            anyhow::bail!("No servers configured");
        }

        // Create connection managers for each server
        let mut connection_managers = HashMap::new();
        for server in &config.servers {
            let manager = ConnectionManager::new(Arc::new(server.clone()));
            connection_managers.insert(server.name.clone(), manager);
            info!(
                "Server '{}' configured with simple connections",
                server.name
            );
        }

        Ok(Self {
            servers: config.servers,
            current_index: Arc::new(AtomicUsize::new(0)),
            connection_managers: Arc::new(connection_managers),
            buffer_pool: BufferPool::new(256 * 1024, 32), // 256KB buffers better for 100MB files with more available
        })
    }

    /// Pre-warm connections to all servers (simplified - no pooling)
    pub async fn prewarm_connections(&self) -> Result<()> {
        info!("Testing connections to all backend servers...");
        for server in &self.servers {
            let manager = self.connection_managers.get(&server.name).unwrap();
            // Test connection to ensure servers are reachable
            match manager.create_connection().await {
                Ok(_) => {
                    info!("Successfully tested connection to {}", server.name);
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

        // Send greeting to client immediately (NNTP protocol requirement)
        if let Err(e) = client_stream.write_all(b"200 NNTP Service Ready\r\n").await {
            error!("Failed to send greeting to client: {}", e);
            return Err(e.into());
        }

        // Get the connection manager for this server
        let manager = self.connection_managers.get(&server.name)
            .ok_or_else(|| anyhow::anyhow!("No connection manager found for server {}", server.name))?;

        // Create a new connection for this request
        let mut backend_stream = match manager.create_connection().await {
            Ok(stream) => {
                info!("Created new connection to {}", server.name);
                stream
            }
            Err(e) => {
                error!("Failed to connect to {}: {}", server.name, e);
                let _ = client_stream
                    .write_all(b"400 Backend server unavailable\r\n")
                    .await;
                return Err(e);
            }
        };

        let backend_addr = format!("{}:{}", server.host, server.port);

        info!("Connected to backend server {}", backend_addr);

        // Authenticate proxy to backend using configured credentials
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

        // Now implement intelligent proxying that handles client authentication
        // without passing it to the already-authenticated backend
        let copy_result = self.handle_client_with_auth_interception(client_stream, backend_stream).await;

        // Connection will be automatically closed when backend_stream goes out of scope
        info!("Connection to {} will be closed", server.name);

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

    /// Handle client connection with authentication interception
    /// Client authenticates to proxy, proxy uses backend connection already authenticated
    async fn handle_client_with_auth_interception(
        &self,
        mut client_stream: TcpStream,
        mut backend_stream: TcpStream,
    ) -> Result<(u64, u64), anyhow::Error> {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        
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
                            
                            // Intercept authentication commands
                            if trimmed.starts_with("AUTHINFO USER") {
                                // Client is trying to authenticate - respond positively
                                let response = b"381 Password required\r\n";
                                client_write.write_all(response).await?;
                                backend_to_client_bytes += response.len() as u64;
                                debug!("Intercepted AUTHINFO USER, sent password request");
                            } else if trimmed.starts_with("AUTHINFO PASS") {
                                // Client is providing password - accept it
                                let response = b"281 Authentication accepted\r\n";
                                client_write.write_all(response).await?;
                                backend_to_client_bytes += response.len() as u64;
                                debug!("Intercepted AUTHINFO PASS, authenticated client");
                            } else if trimmed.starts_with("ARTICLE") || trimmed.starts_with("BODY") || 
                                     trimmed.starts_with("HEAD") || trimmed.starts_with("STAT") {
                                // Forward data command to backend
                                backend_write.write_all(line.as_bytes()).await?;
                                client_to_backend_bytes += line.len() as u64;
                                debug!("Forwarding data command, switching to high-throughput mode");
                                
                                // Return the buffer before transitioning
                                self.buffer_pool.return_buffer(buffer).await;
                                
                                // For high-throughput data transfer, use our optimized copy
                                // We need to carefully handle this transition...
                                // For now, let's continue with the select loop but optimize for large transfers
                                return self.handle_high_throughput_transfer(
                                    client_reader, client_write,
                                    backend_read, backend_write,
                                    client_to_backend_bytes,
                                    backend_to_client_bytes
                                ).await;
                            } else {
                                // Forward other commands to backend
                                backend_write.write_all(line.as_bytes()).await?;
                                client_to_backend_bytes += line.len() as u64;
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
        
        // Continue handling commands and large data responses
        loop {
            let mut line = String::new();
            let mut buffer = self.buffer_pool.get_buffer().await;
            
            tokio::select! {
                // Continue reading client commands
                result = client_reader.read_line(&mut line) => {
                    match result {
                        Ok(0) => {
                            self.buffer_pool.return_buffer(buffer).await;
                            break;
                        }
                        Ok(_) => {
                            // Forward command to backend
                            backend_write.write_all(line.as_bytes()).await?;
                            client_to_backend_bytes += line.len() as u64;
                        }
                        Err(e) => {
                            warn!("Error reading client command: {}", e);
                            self.buffer_pool.return_buffer(buffer).await;
                            break;
                        }
                    }
                }
                
                // Read large responses from backend with optimized buffer size
                result = backend_read.read(&mut buffer) => {
                    match result {
                        Ok(0) => {
                            self.buffer_pool.return_buffer(buffer).await;
                            break;
                        }
                        Ok(n) => {
                            client_write.write_all(&buffer[..n]).await?;
                            backend_to_client_bytes += n as u64;
                            
                            // For very large transfers, ensure we keep reading efficiently
                            if n == buffer.len() {
                                // Buffer was full, likely more data coming
                                // Continue optimized reading...
                            }
                        }
                        Err(e) => {
                            warn!("Error reading backend response: {}", e);
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
