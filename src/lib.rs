//! # NNTP Proxy Library
//!
//! A high-performance, stateless NNTP proxy server implementation designed
//! for future connection multiplexing capabilities.
//!
//! ## Architecture
//!
//! The proxy is organized into several modules for clean separation of concerns:
//!
//! - **auth**: Authentication handling for both client and backend connections
//! - **command**: NNTP command parsing and classification
//! - **config**: Configuration loading and management
//! - **pool**: Connection and buffer pooling for high performance
//! - **protocol**: NNTP protocol constants and response parsing
//! - **types**: Core type definitions (ClientId, RequestId, BackendId)
//!
//! ## Design Philosophy
//!
//! This proxy operates in **stateless mode**, rejecting commands that require
//! maintaining session state (like GROUP, NEXT, LAST). This design enables:
//!
//! - Simpler architecture with clear separation of concerns
//! - Future multiplexing where multiple clients share backend connections
//! - Easy testing and maintenance of individual components
//!
//! ## Current Implementation
//!
//! The current implementation uses a 1:1 client-to-backend connection model
//! with connection pooling. The modular architecture is designed to support
//! future conversion to a true multiplexing proxy.

use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tracing::{debug, error, info, warn};

// Module declarations
mod auth;
mod command;
mod config;
mod network;
mod pool;
mod protocol;
mod session;
mod streaming;
mod types;

// Public exports
pub use config::{Config, ServerConfig, create_default_config, load_config};

// Internal imports
use auth::BackendAuthenticator;
use network::SocketOptimizer;
use pool::{BufferPool, ConnectionProvider, DeadpoolConnectionProvider};
use protocol::*;
use session::ClientSession;

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

        // Create a client session to handle the connection
        let session = ClientSession::new(client_addr, self.buffer_pool.clone());
        
        // Apply socket optimizations for high-throughput
        if let Err(e) = SocketOptimizer::apply_to_streams(&client_stream, &backend_stream) {
            debug!("Failed to apply socket optimizations: {}", e);
        }

        // Handle the session with the backend connection
        let copy_result = session.handle_with_backend(client_stream, backend_stream).await;

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
                warn!("Session error for client {}: {}", client_addr, e);
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
        BackendAuthenticator::forward_greeting(backend_stream, client_stream, &self.buffer_pool).await
    }

    /// Perform NNTP authentication and forward the final greeting to client
    async fn authenticate_backend_and_forward_greeting(
        &self,
        backend_stream: &mut TcpStream,
        client_stream: &mut TcpStream,
        username: &str,
        password: &str,
    ) -> Result<()> {
        BackendAuthenticator::authenticate_and_forward_greeting(
            backend_stream,
            client_stream,
            username,
            password,
            &self.buffer_pool,
        ).await
    }

    // Note: Client session handling moved to session module
    // Note: Streaming/transfer logic moved to streaming module
    // Note: Socket optimization moved to network module
}

// Note: load_config and create_default_config moved to config module

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

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

    // Note: Config and ServerConfig tests moved to config module

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

    // Note: Config loading and serialization tests moved to config module
}

// Note: Command classification tests moved to command::classifier module
