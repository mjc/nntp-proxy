//! Client session management
//!
//! This module handles the lifecycle of a client connection, including
//! command processing, authentication interception, and data transfer.

use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tracing::{debug, warn};

use crate::auth::AuthHandler;
use crate::command::{AuthAction, CommandAction, CommandHandler};
use crate::pool::BufferPool;
use crate::protocol::NNTP_COMMAND_NOT_SUPPORTED;
use crate::router::RequestRouter;
use crate::streaming::StreamHandler;
use crate::types::ClientId;

/// Represents an active client session
pub struct ClientSession {
    client_addr: SocketAddr,
    buffer_pool: BufferPool,
    /// Unique identifier for this client
    client_id: ClientId,
    /// Optional router for multiplexed connections
    router: Option<Arc<RequestRouter>>,
}

impl ClientSession {
    /// Create a new client session (1:1 mode, no router)
    pub fn new(client_addr: SocketAddr, buffer_pool: BufferPool) -> Self {
        Self {
            client_addr,
            buffer_pool,
            client_id: ClientId::new(),
            router: None,
        }
    }

    /// Create a new client session with router for multiplexing
    #[allow(dead_code)]
    pub fn new_with_router(
        client_addr: SocketAddr,
        buffer_pool: BufferPool,
        router: Arc<RequestRouter>,
    ) -> Self {
        Self {
            client_addr,
            buffer_pool,
            client_id: ClientId::new(),
            router: Some(router),
        }
    }

    /// Get the client ID
    #[allow(dead_code)]
    pub fn client_id(&self) -> ClientId {
        self.client_id
    }

    /// Check if this session is using multiplexed mode
    #[allow(dead_code)]
    pub fn is_multiplexed(&self) -> bool {
        self.router.is_some()
    }

    /// Handle client connection with authentication interception
    /// Client authenticates to proxy, proxy uses backend connection already authenticated
    pub async fn handle_with_backend(
        &self,
        mut client_stream: TcpStream,
        mut backend_stream: TcpStream,
    ) -> Result<(u64, u64)> {
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
                            debug!("Client {} command: {}", self.client_addr, trimmed);

                            // Handle command using CommandHandler
                            match CommandHandler::handle_command(&line) {
                                CommandAction::InterceptAuth(auth_action) => {
                                    let response = match auth_action {
                                        AuthAction::RequestPassword => AuthHandler::user_response(),
                                        AuthAction::AcceptAuth => AuthHandler::pass_response(),
                                    };
                                    client_write.write_all(response).await?;
                                    backend_to_client_bytes += response.len() as u64;
                                    debug!("Intercepted auth command for client {}", self.client_addr);
                                }
                                CommandAction::Reject(_reason) => {
                                    warn!("Rejecting command from client {}: {}", self.client_addr, trimmed);
                                    client_write.write_all(NNTP_COMMAND_NOT_SUPPORTED).await?;
                                    backend_to_client_bytes += NNTP_COMMAND_NOT_SUPPORTED.len() as u64;
                                }
                                CommandAction::ForwardHighThroughput => {
                                    // Forward article retrieval by message-ID to backend
                                    backend_write.write_all(line.as_bytes()).await?;
                                    client_to_backend_bytes += line.len() as u64;
                                    debug!("Client {} switching to high-throughput mode", self.client_addr);

                                    // Return the buffer before transitioning
                                    self.buffer_pool.return_buffer(buffer).await;

                                    // For high-throughput data transfer, use optimized handler
                                    return StreamHandler::high_throughput_transfer(
                                        client_reader,
                                        client_write,
                                        backend_read,
                                        backend_write,
                                        client_to_backend_bytes,
                                        backend_to_client_bytes,
                                    ).await;
                                }
                                CommandAction::ForwardStateless => {
                                    // Forward stateless commands to backend
                                    backend_write.write_all(line.as_bytes()).await?;
                                    client_to_backend_bytes += line.len() as u64;
                                }
                            }
                        }
                        Err(e) => {
                            warn!("Error reading from client {}: {}", self.client_addr, e);
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
                            warn!("Error reading from backend for client {}: {}", self.client_addr, e);
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

    /// Route a single command through the router (for multiplexed mode)
    /// This demonstrates the multiplexing architecture without full implementation
    #[allow(dead_code)]
    pub async fn route_command(&self, command: &str) -> Result<()> {
        if let Some(router) = &self.router {
            // Route the command through the router
            let (_request_id, backend_id) = router.route_command(self.client_id, command).await?;

            debug!(
                "Client {} ({:?}) routed command to backend {:?}: {}",
                self.client_addr,
                self.client_id,
                backend_id,
                command.trim()
            );

            Ok(())
        } else {
            anyhow::bail!("Session not configured for multiplexing - no router available")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn test_client_session_creation() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(1024, 4);
        let session = ClientSession::new(addr, buffer_pool.clone());

        assert_eq!(session.client_addr.port(), 8080);
        assert_eq!(
            session.client_addr.ip(),
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))
        );
    }

    #[test]
    fn test_client_session_with_different_ports() {
        let buffer_pool = BufferPool::new(1024, 4);

        let addr1 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let session1 = ClientSession::new(addr1, buffer_pool.clone());

        let addr2 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 9090);
        let session2 = ClientSession::new(addr2, buffer_pool.clone());

        assert_ne!(session1.client_addr.port(), session2.client_addr.port());
        assert_eq!(session1.client_addr.port(), 8080);
        assert_eq!(session2.client_addr.port(), 9090);
    }

    #[test]
    fn test_client_session_with_ipv6() {
        let buffer_pool = BufferPool::new(1024, 4);
        let addr = SocketAddr::new(IpAddr::V6("::1".parse().unwrap()), 8119);
        let session = ClientSession::new(addr, buffer_pool);

        assert_eq!(session.client_addr.port(), 8119);
        assert!(session.client_addr.is_ipv6());
    }

    #[test]
    fn test_buffer_pool_cloning() {
        let buffer_pool = BufferPool::new(8192, 10);
        let buffer_pool_clone = buffer_pool.clone();

        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 1234);
        let _session1 = ClientSession::new(addr, buffer_pool);
        let _session2 = ClientSession::new(addr, buffer_pool_clone);

        // Both sessions should work with the same underlying pool
    }

    #[test]
    fn test_session_addr_formatting() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 5555);
        let buffer_pool = BufferPool::new(1024, 4);
        let session = ClientSession::new(addr, buffer_pool);

        let addr_str = format!("{}", session.client_addr);
        assert!(addr_str.contains("10.0.0.1"));
        assert!(addr_str.contains("5555"));
    }

    #[test]
    fn test_multiple_sessions_same_buffer_pool() {
        let buffer_pool = BufferPool::new(4096, 8);
        let sessions: Vec<_> = (0..5)
            .map(|i| {
                let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8000 + i);
                ClientSession::new(addr, buffer_pool.clone())
            })
            .collect();

        assert_eq!(sessions.len(), 5);
        for (i, session) in sessions.iter().enumerate() {
            assert_eq!(session.client_addr.port(), 8000 + i as u16);
        }
    }

    #[test]
    fn test_loopback_address() {
        let buffer_pool = BufferPool::new(1024, 4);
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8119);
        let session = ClientSession::new(addr, buffer_pool);

        assert!(session.client_addr.ip().is_loopback());
    }

    #[test]
    fn test_unspecified_address() {
        let buffer_pool = BufferPool::new(1024, 4);
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
        let session = ClientSession::new(addr, buffer_pool);

        assert!(session.client_addr.ip().is_unspecified());
        assert_eq!(session.client_addr.port(), 0);
    }

    #[test]
    fn test_session_without_router() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(1024, 4);
        let session = ClientSession::new(addr, buffer_pool);

        assert!(!session.is_multiplexed());
        assert_eq!(session.client_addr.port(), 8080);
    }

    #[test]
    fn test_session_with_router() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(1024, 4);
        let router = Arc::new(RequestRouter::new());
        let session = ClientSession::new_with_router(addr, buffer_pool, router);

        assert!(session.is_multiplexed());
        assert_eq!(session.client_addr.port(), 8080);
    }

    #[test]
    fn test_client_id_uniqueness() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(1024, 4);

        let session1 = ClientSession::new(addr, buffer_pool.clone());
        let session2 = ClientSession::new(addr, buffer_pool);

        // Each session should have a unique client ID
        assert_ne!(session1.client_id(), session2.client_id());
    }

    #[tokio::test]
    async fn test_route_command_without_router_fails() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(1024, 4);
        let session = ClientSession::new(addr, buffer_pool);

        let result = session.route_command("LIST\r\n").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no router"));
    }

    #[tokio::test]
    async fn test_route_command_with_router() {
        use crate::pool::DeadpoolConnectionProvider;
        use crate::types::BackendId;

        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(1024, 4);

        // Create router with a backend
        let mut router = RequestRouter::new();
        let provider = DeadpoolConnectionProvider::new(
            "localhost".to_string(),
            9999,
            "test-backend".to_string(),
            2,
        );
        router.add_backend(BackendId::from_index(0), "test".to_string(), provider);

        let session = ClientSession::new_with_router(addr, buffer_pool, Arc::new(router));

        // Should successfully route command
        let result = session.route_command("LIST\r\n").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_multiple_commands_routed() {
        use crate::pool::DeadpoolConnectionProvider;
        use crate::types::BackendId;

        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(1024, 4);

        // Create router with multiple backends
        let mut router = RequestRouter::new();
        for i in 0..3 {
            let provider = DeadpoolConnectionProvider::new(
                "localhost".to_string(),
                9999 + i,
                format!("backend-{}", i),
                2,
            );
            router.add_backend(
                BackendId::from_index(i as usize),
                format!("backend-{}", i),
                provider,
            );
        }

        let session = ClientSession::new_with_router(addr, buffer_pool, Arc::new(router));

        // Route multiple commands
        for _ in 0..5 {
            let result = session.route_command("LIST\r\n").await;
            assert!(result.is_ok());
        }
    }
}
