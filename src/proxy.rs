//! NNTP Proxy implementation
//!
//! This module contains the main `NntpProxy` struct which orchestrates
//! connection handling, routing, and resource management.

use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tracing::{debug, error, info, warn};

use crate::config::{Config, ServerConfig};
use crate::network::SocketOptimizer;
use crate::pool::{BufferPool, ConnectionProvider, DeadpoolConnectionProvider, prewarm_pools};
use crate::constants::buffer::{BUFFER_SIZE, BUFFER_POOL_SIZE};
use crate::constants::stateless_proxy::*;
use crate::router;
use crate::session::ClientSession;
use crate::types;

#[derive(Debug, Clone)]
pub struct NntpProxy {
    servers: Arc<Vec<ServerConfig>>,
    /// Backend selector for round-robin load balancing
    router: Arc<router::BackendSelector>,
    /// Connection providers per server - easily swappable implementation
    connection_providers: Vec<DeadpoolConnectionProvider>,
    /// Buffer pool for I/O operations
    buffer_pool: BufferPool,
}

impl NntpProxy {
    pub fn new(config: Config) -> Result<Self> {
        if config.servers.is_empty() {
            anyhow::bail!("No servers configured in configuration");
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
                DeadpoolConnectionProvider::from_server_config(server)
            })
            .collect();

        let buffer_pool = BufferPool::new(BUFFER_SIZE, BUFFER_POOL_SIZE);

        let servers = Arc::new(config.servers);

        // Create backend selector and add all backends
        let router = Arc::new({
            use types::BackendId;
            connection_providers
                .iter()
                .enumerate()
                .fold(router::BackendSelector::new(), |mut r, (idx, provider)| {
                    let backend_id = BackendId::from_index(idx);
                    r.add_backend(backend_id, servers[idx].name.clone(), provider.clone());
                    r
                })
        });

        Ok(Self {
            servers,
            router,
            connection_providers,
            buffer_pool,
        })
    }

    /// Prewarm all connection pools before accepting clients
    /// Creates all connections concurrently and returns when ready
    pub async fn prewarm_connections(&self) -> Result<()> {
        prewarm_pools(&self.connection_providers, &self.servers).await
    }

    /// Gracefully shutdown all connection pools
    pub async fn graceful_shutdown(&self) {
        info!("Initiating graceful shutdown of all connection pools...");

        for provider in &self.connection_providers {
            provider.graceful_shutdown().await;
        }

        info!("All connection pools have been shut down gracefully");
    }

    /// Get the list of servers
    pub fn servers(&self) -> &[ServerConfig] {
        &self.servers
    }

    /// Get the router
    pub fn router(&self) -> &Arc<router::BackendSelector> {
        &self.router
    }

    /// Get the connection providers
    pub fn connection_providers(&self) -> &[DeadpoolConnectionProvider] {
        &self.connection_providers
    }

    /// Get the buffer pool
    pub fn buffer_pool(&self) -> &BufferPool {
        &self.buffer_pool
    }

    /// Common setup for client connections (greeting only, prewarming done at startup)
    async fn setup_client_connection(
        &self,
        client_stream: &mut TcpStream,
        client_addr: SocketAddr,
    ) -> Result<()> {
        // Send proxy greeting
        crate::protocol::send_proxy_greeting(client_stream, client_addr).await
    }

    pub async fn handle_client(
        &self,
        mut client_stream: TcpStream,
        client_addr: SocketAddr,
    ) -> Result<()> {
        debug!("New client connection from {}", client_addr);

        // Use a dummy ClientId and command for routing (synchronous 1:1 mapping)
        use types::ClientId;
        let client_id = ClientId::new();

        // Select backend using router's round-robin
        let backend_id = self.router.route_command_sync(client_id, "")?;
        let server_idx = backend_id.as_index();
        let server = &self.servers[server_idx];

        info!(
            "Routing client {} to backend {:?} ({}:{})",
            client_addr, backend_id, server.host, server.port
        );

        // Setup connection (prewarm and greeting)
        self.setup_client_connection(&mut client_stream, client_addr).await?;

        // Get pooled backend connection
        let pool_status = self.connection_providers[server_idx].status();
        debug!(
            "Pool status for {}: {}/{} available, {} created",
            server.name, pool_status.available, pool_status.max_size, pool_status.created
        );

        let backend_conn = match self.connection_providers[server_idx]
            .get_pooled_connection()
            .await
        {
            Ok(conn) => {
                debug!("Got pooled connection for {}", server.name);
                conn
            }
            Err(e) => {
                error!("Failed to get pooled connection for {} (client {}): {}", server.name, client_addr, e);
                let _ = client_stream.write_all(NNTP_BACKEND_UNAVAILABLE).await;
                return Err(anyhow::anyhow!(
                    "Failed to get pooled connection for backend '{}' (client {}): {}",
                    server.name,
                    client_addr,
                    e
                ));
            }
        };

        // Apply socket optimizations for high-throughput
        if let Err(e) = SocketOptimizer::apply_to_streams(&client_stream, &backend_conn) {
            debug!("Failed to apply socket optimizations: {}", e);
        }

        // Create session and handle connection
        let session = ClientSession::new(client_addr, self.buffer_pool.clone());
        debug!("Starting session for client {}", client_addr);

        let copy_result = session
            .handle_with_pooled_backend(client_stream, backend_conn)
            .await;

        debug!("Session completed for client {}", client_addr);

        // Complete the routing (decrement pending count)
        self.router.complete_command_sync(backend_id);

        // Log session results
        match copy_result {
            Ok((client_to_backend_bytes, backend_to_client_bytes)) => {
                info!(
                    "Connection closed for client {}: {} bytes sent, {} bytes received",
                    client_addr, client_to_backend_bytes, backend_to_client_bytes
                );
            }
            Err(e) => {
                warn!("Session error for client {}: {}", client_addr, e);
            }
        }

        debug!("Connection returned to pool for {}", server.name);
        Ok(())
    }

    /// Handle client connection using per-command routing mode
    ///
    /// This creates a session with the router, allowing commands from this client
    /// to be routed to different backends based on load balancing.
    pub async fn handle_client_per_command_routing(
        &self,
        mut client_stream: TcpStream,
        client_addr: SocketAddr,
    ) -> Result<()> {
        debug!("New per-command routing client connection from {}", client_addr);

        // Enable TCP_NODELAY for low latency
        let _ = client_stream.set_nodelay(true);

        // Setup connection (prewarm and greeting)
        self.setup_client_connection(&mut client_stream, client_addr).await?;

        // Create session with router for per-command routing
        let session = ClientSession::new_with_router(client_addr, self.buffer_pool.clone(), self.router.clone());

        info!(
            "Client {} (ID: {}) connected in per-command routing mode",
            client_addr,
            session.client_id()
        );

        // Handle the session with per-command routing
        let result = session.handle_per_command_routing(client_stream).await;

        // Log session results
        match result {
            Ok((client_to_backend, backend_to_client)) => {
                info!(
                    "Per-command routing session closed for {} (ID: {}): {} bytes sent, {} bytes received",
                    client_addr,
                    session.client_id(),
                    client_to_backend,
                    backend_to_client
                );
            }
            Err(e) => {
                warn!(
                    "Per-command routing session error for {} (ID: {}): {}",
                    client_addr,
                    session.client_id(),
                    e
                );
            }
        }

        debug!(
            "Per-command routing connection closed for {} (ID: {})",
            client_addr,
            session.client_id()
        );
        Ok(())
    }
}

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
            ..Default::default()
        }
    }

    #[test]
    fn test_proxy_creation_with_servers() {
        let config = create_test_config();
        let proxy = Arc::new(NntpProxy::new(config).expect("Failed to create proxy"));

        assert_eq!(proxy.servers().len(), 3);
        assert_eq!(proxy.servers()[0].name, "Test Server 1");
    }

    #[test]
    fn test_proxy_creation_with_empty_servers() {
        let config = Config {
            servers: vec![],
            ..Default::default()
        };
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
    fn test_proxy_has_router() {
        let config = create_test_config();
        let proxy = Arc::new(NntpProxy::new(config).expect("Failed to create proxy"));

        // Proxy should have a router with backends
        assert_eq!(proxy.router.backend_count(), 3);
    }
}
