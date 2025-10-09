//! Client session management
//!
//! This module handles the lifecycle of a client connection, including
//! command processing, authentication interception, and data transfer.

use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, error, warn};

use crate::auth::AuthHandler;
use crate::command::{AuthAction, CommandAction, CommandHandler};
use crate::constants::buffer::{COMMAND_SIZE, STREAMING_CHUNK_SIZE};
use crate::constants::protocol::{
    BACKEND_ERROR, CONNECTION_CLOSING, PROXY_GREETING_PCR, TERMINATOR_TAIL_SIZE,
};
use crate::constants::stateless_proxy::NNTP_COMMAND_NOT_SUPPORTED;
use crate::pool::BufferPool;
use crate::router::BackendSelector;
use crate::streaming::StreamHandler;
use crate::types::ClientId;

/// Represents an active client session
pub struct ClientSession {
    client_addr: SocketAddr,
    buffer_pool: BufferPool,
    /// Unique identifier for this client
    client_id: ClientId,
    /// Optional router for per-command routing mode
    router: Option<Arc<BackendSelector>>,
}

impl ClientSession {
    /// Create a new client session for 1:1 mode
    #[must_use]
    pub fn new(client_addr: SocketAddr, buffer_pool: BufferPool) -> Self {
        Self {
            client_addr,
            buffer_pool,
            client_id: ClientId::new(),
            router: None,
        }
    }

    /// Create a new client session for per-command routing mode
    #[must_use]
    pub fn new_with_router(
        client_addr: SocketAddr,
        buffer_pool: BufferPool,
        router: Arc<BackendSelector>,
    ) -> Self {
        Self {
            client_addr,
            buffer_pool,
            client_id: ClientId::new(),
            router: Some(router),
        }
    }

    /// Get the unique client ID
    #[must_use]
    #[inline]
    pub fn client_id(&self) -> ClientId {
        self.client_id
    }

    /// Check if this session is using per-command routing
    #[must_use]
    #[inline]
    pub fn is_per_command_routing(&self) -> bool {
        self.router.is_some()
    }

    /// Handle client connection with a pooled backend connection
    /// This keeps the pooled connection object alive and returns it to the pool when done
    /// Intercepts authentication commands since backend connection is already authenticated
    pub async fn handle_with_pooled_backend<T>(
        &self,
        mut client_stream: TcpStream,
        backend_conn: T,
    ) -> Result<(u64, u64)>
    where
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        use tokio::io::BufReader;

        // Split streams for independent read/write
        let (client_read, mut client_write) = client_stream.split();
        let (mut backend_read, mut backend_write) = tokio::io::split(backend_conn);
        let mut client_reader = BufReader::new(client_read);

        let mut client_to_backend_bytes = 0u64;
        let mut backend_to_client_bytes = 0u64;

        // Reuse line buffer to avoid per-iteration allocations
        let mut line = String::with_capacity(COMMAND_SIZE);

        debug!("Client {} session loop starting", self.client_addr);

        // Handle the initial command/response phase where we intercept auth
        loop {
            line.clear();
            let mut buffer = self.buffer_pool.get_buffer().await;

            tokio::select! {
                // Read command from client
                result = client_reader.read_line(&mut line) => {
                    match result {
                        Ok(0) => {
                            debug!("Client {} disconnected (0 bytes read)", self.client_addr);
                            self.buffer_pool.return_buffer(buffer).await;
                            break; // Client disconnected
                        }
                        Ok(n) => {
                            debug!("Client {} sent {} bytes: {:?}", self.client_addr, n, line.trim());
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

    /// Handle a client connection with per-command routing
    /// Each command is routed independently to potentially different backends
    pub async fn handle_per_command_routing(
        &self,
        mut client_stream: TcpStream,
    ) -> Result<(u64, u64)> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let router = self
            .router
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Per-command routing mode requires a router"))?;

        let (client_read, mut client_write) = client_stream.split();
        let mut client_reader = BufReader::new(client_read);

        let mut client_to_backend_bytes = 0u64;
        let mut backend_to_client_bytes = 0u64;

        // Send initial greeting to client
        client_write.write_all(PROXY_GREETING_PCR).await?;
        backend_to_client_bytes += PROXY_GREETING_PCR.len() as u64;

        debug!(
            "Client {} sent greeting, entering command loop",
            self.client_addr
        );

        // Reuse command buffer to avoid allocations per command
        let mut command = String::with_capacity(COMMAND_SIZE);

        // Process commands one at a time
        loop {
            command.clear();

            match client_reader.read_line(&mut command).await {
                Ok(0) => {
                    debug!("Client {} disconnected", self.client_addr);
                    break; // Client disconnected
                }
                Ok(n) => {
                    client_to_backend_bytes += n as u64;
                    let trimmed = command.trim();

                    debug!(
                        "Client {} received command ({} bytes): {} | hex: {:02x?}",
                        self.client_addr, n, trimmed, command.as_bytes()
                    );

                    // Handle QUIT locally
                    if trimmed.eq_ignore_ascii_case("QUIT") {
                        // Send closing message - ignore errors if client already disconnected, but log for debugging
                        if let Err(e) = client_write.write_all(CONNECTION_CLOSING).await {
                            debug!(
                                "Failed to write CONNECTION_CLOSING to client {}: {}",
                                self.client_addr, e
                            );
                        }
                        if let Err(e) = client_write.flush().await {
                            debug!(
                                "Failed to flush CONNECTION_CLOSING to client {}: {}",
                                self.client_addr, e
                            );
                        }
                        backend_to_client_bytes += CONNECTION_CLOSING.len() as u64;
                        debug!("Client {} sent QUIT, closing connection", self.client_addr);
                        break;
                    }

                    // Check if command should be rejected (stateful commands)
                    match CommandHandler::handle_command(&command) {
                        CommandAction::InterceptAuth(auth_action) => {
                            // Handle authentication locally
                            let response = match auth_action {
                                AuthAction::RequestPassword => AuthHandler::user_response(),
                                AuthAction::AcceptAuth => AuthHandler::pass_response(),
                            };
                            client_write.write_all(response).await?;
                            backend_to_client_bytes += response.len() as u64;
                            continue;
                        }
                        CommandAction::Reject(reason) => {
                            warn!(
                                "Rejecting command from client {}: {} ({})",
                                self.client_addr, trimmed, reason
                            );
                            client_write.write_all(NNTP_COMMAND_NOT_SUPPORTED).await?;
                            backend_to_client_bytes += NNTP_COMMAND_NOT_SUPPORTED.len() as u64;
                            continue;
                        }
                        CommandAction::ForwardStateless | CommandAction::ForwardHighThroughput => {
                            // Route this command to a backend
                            match self
                                .route_and_execute_command(
                                    router,
                                    &command,
                                    &mut client_write,
                                    &mut client_to_backend_bytes,
                                    &mut backend_to_client_bytes,
                                )
                                .await
                            {
                                Ok(()) => {}
                                Err(e) => {
                                    // Provide detailed context for broken pipe errors
                                    if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
                                        match io_err.kind() {
                                            std::io::ErrorKind::BrokenPipe => {
                                                warn!(
                                                    "Client {} disconnected unexpectedly while routing command '{}' (broken pipe). \
                                                     Session stats: {} bytes sent to backend, {} bytes received from backend. \
                                                     This usually indicates the client closed the connection before receiving the response.",
                                                    self.client_addr, trimmed, client_to_backend_bytes, backend_to_client_bytes
                                                );
                                            }
                                            std::io::ErrorKind::ConnectionReset => {
                                                warn!(
                                                    "Client {} connection reset while routing command '{}'. \
                                                     Session stats: {} bytes sent to backend, {} bytes received from backend. \
                                                     This usually indicates a network issue or client crash.",
                                                    self.client_addr, trimmed, client_to_backend_bytes, backend_to_client_bytes
                                                );
                                            }
                                            std::io::ErrorKind::ConnectionAborted => {
                                                warn!(
                                                    "Client {} connection aborted while routing command '{}'. \
                                                     Session stats: {} bytes sent to backend, {} bytes received from backend. \
                                                     This usually indicates the connection was terminated by the local system.",
                                                    self.client_addr, trimmed, client_to_backend_bytes, backend_to_client_bytes
                                                );
                                            }
                                            _ => {
                                                error!(
                                                    "I/O error routing command '{}' for client {}: {} (kind: {:?}). \
                                                     Session stats: {} bytes sent to backend, {} bytes received from backend.",
                                                    trimmed, self.client_addr, e, io_err.kind(), client_to_backend_bytes, backend_to_client_bytes
                                                );
                                            }
                                        }
                                    } else {
                                        error!(
                                            "Error routing command '{}' for client {}: {}. \
                                             Session stats: {} bytes sent to backend, {} bytes received from backend.",
                                            trimmed, self.client_addr, e, client_to_backend_bytes, backend_to_client_bytes
                                        );
                                    }
                                    
                                    // Try to send error response, but don't log failure if client is gone
                                    let _ = client_write.write_all(BACKEND_ERROR).await;
                                    backend_to_client_bytes += BACKEND_ERROR.len() as u64;
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    // Provide detailed context for client read errors
                    match e.kind() {
                        std::io::ErrorKind::UnexpectedEof => {
                            debug!(
                                "Client {} closed connection (EOF). Session stats: {} bytes sent to backend, {} bytes received from backend.",
                                self.client_addr, client_to_backend_bytes, backend_to_client_bytes
                            );
                        }
                        std::io::ErrorKind::BrokenPipe => {
                            debug!(
                                "Client {} connection broken pipe while reading. Session stats: {} bytes sent to backend, {} bytes received from backend.",
                                self.client_addr, client_to_backend_bytes, backend_to_client_bytes
                            );
                        }
                        std::io::ErrorKind::ConnectionReset => {
                            warn!(
                                "Client {} connection reset while reading. Session stats: {} bytes sent to backend, {} bytes received from backend.",
                                self.client_addr, client_to_backend_bytes, backend_to_client_bytes
                            );
                        }
                        _ => {
                            warn!(
                                "Error reading from client {}: {} (kind: {:?}). Session stats: {} bytes sent to backend, {} bytes received from backend.",
                                self.client_addr, e, e.kind(), client_to_backend_bytes, backend_to_client_bytes
                            );
                        }
                    }
                    break;
                }
            }
        }

        Ok((client_to_backend_bytes, backend_to_client_bytes))
    }

    /// Route a single command to a backend and execute it
    async fn route_and_execute_command(
        &self,
        router: &BackendSelector,
        command: &str,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        client_to_backend_bytes: &mut u64,
        backend_to_client_bytes: &mut u64,
    ) -> Result<()> {
        use tokio::io::AsyncWriteExt;

        // Route the command to get a backend (lock-free!)
        let backend_id = router.route_command_sync(self.client_id, command)?;

        debug!(
            "Client {} routed command to backend {:?}: {}",
            self.client_addr,
            backend_id,
            command.trim()
        );

        // Get a connection from the router's backend pool
        let provider = router
            .get_backend_provider(backend_id)
            .ok_or_else(|| anyhow::anyhow!("Backend {:?} not found", backend_id))?;

        debug!(
            "Client {} getting pooled connection for backend {:?}",
            self.client_addr, backend_id
        );
        // Use get_pooled_connection() to get a connection that auto-returns to pool
        // The pool's recycle() method will health-check connections before reuse
        // so we don't get stale connections that timed out on the backend
        let mut pooled_conn = provider.get_pooled_connection().await?;
        debug!(
            "Client {} got pooled connection for backend {:?}",
            self.client_addr, backend_id
        );

        // Connection from pool is already authenticated - no need to consume greeting or auth again

        // Forward the command to the backend
        debug!(
            "Client {} forwarding command to backend {:?} ({} bytes): {} | hex: {:02x?}",
            self.client_addr,
            backend_id,
            command.len(),
            command.trim(),
            command.as_bytes()
        );
        pooled_conn.write_all(command.as_bytes()).await?;
        pooled_conn.flush().await?;
        *client_to_backend_bytes += command.len() as u64;
        debug!(
            "Client {} command sent and flushed to backend {:?}",
            self.client_addr, backend_id
        );

        // Read the response from the backend
        debug!(
            "Client {} reading response from backend {:?}",
            self.client_addr, backend_id
        );

        // Use direct reading from backend - no split() to avoid mutex overhead
        use tokio::io::AsyncReadExt;

        let mut chunk = vec![0u8; STREAMING_CHUNK_SIZE];
        let mut total_bytes = 0;

        // Read first chunk to determine response type
        let n = pooled_conn.read(&mut chunk).await?;
        if n == 0 {
            return Err(anyhow::anyhow!("Backend connection closed unexpectedly"));
        }
        
        debug!(
            "Client {} received backend response chunk ({} bytes): {} | hex: {:02x?}",
            self.client_addr, n,
            String::from_utf8_lossy(&chunk[..n.min(100)]), // Show first 100 bytes max
            &chunk[..n.min(32)] // Show first 32 bytes in hex
        );

        // Find first newline to determine if multiline
        let first_newline = chunk[..n].iter().position(|&b| b == b'\n').unwrap_or(n);
        let is_multiline =
            first_newline >= 3 && chunk[0] == b'2' && !(chunk[1] == b'0' && chunk[2] == b'5');

        // Log first line (best effort)
        if let Ok(first_line_str) = std::str::from_utf8(&chunk[..first_newline.min(n)]) {
            debug!(
                "Client {} got first line from backend {:?}: {}",
                self.client_addr,
                backend_id,
                first_line_str.trim()
            );
        }

        // Write first chunk directly to client
        debug!(
            "Client {} sending first chunk ({} bytes): {} | hex: {:02x?}",
            self.client_addr, n, 
            String::from_utf8_lossy(&chunk[..n.min(100)]), // Show first 100 bytes max
            &chunk[..n.min(32)] // Show first 32 bytes in hex
        );
        client_write.write_all(&chunk[..n]).await?;
        total_bytes += n;

        if is_multiline {
            // Fast check if terminator is in first chunk (check end only)
            let has_terminator = if n >= 5 {
                chunk[n - 5..n] == *b"\r\n.\r\n" || (n >= 3 && chunk[n - 3..n] == *b"\n.\n")
            } else {
                n >= 3 && chunk[..n] == *b"\n.\n"
            };

            if !has_terminator {
                // For multiline responses, use pipelined streaming
                // Prepare double buffering for concurrent read/write
                let mut chunk1 = chunk; // Reuse first buffer
                let mut chunk2 = vec![0u8; STREAMING_CHUNK_SIZE]; // Second buffer for pipelining

                let mut tail: [u8; TERMINATOR_TAIL_SIZE] = [0; TERMINATOR_TAIL_SIZE]; // Fixed-size tail for span detection
                let mut tail_len: usize = 0; // How much of tail is valid

                // Initialize tail with last bytes of first chunk (already written above)
                if n >= TERMINATOR_TAIL_SIZE {
                    tail.copy_from_slice(&chunk1[n - TERMINATOR_TAIL_SIZE..n]);
                    tail_len = TERMINATOR_TAIL_SIZE;
                } else if n > 0 {
                    tail[..n].copy_from_slice(&chunk1[..n]);
                    tail_len = n;
                }

                // Check terminator in first chunk (already written)
                let first_has_term = if n >= 5 {
                    chunk1[n - 5..n] == *b"\r\n.\r\n" || (n >= 3 && chunk1[n - 3..n] == *b"\n.\n")
                } else {
                    n >= 3 && chunk1[..n] == *b"\n.\n"
                };

                if !first_has_term {
                    // First chunk didn't have terminator, continue reading
                    let mut current_chunk = &mut chunk1;
                    let mut next_chunk = &mut chunk2;

                    // Read next chunk and start loop
                    let mut current_n = pooled_conn.read(next_chunk).await?;
                    if current_n > 0 {
                        std::mem::swap(&mut current_chunk, &mut next_chunk);

                        loop {
                            // Write current chunk to client
                            debug!(
                                "Client {} sending streaming chunk ({} bytes): {} | hex: {:02x?}",
                                self.client_addr, current_n,
                                String::from_utf8_lossy(&current_chunk[..current_n.min(100)]), // Show first 100 bytes max
                                &current_chunk[..current_n.min(32)] // Show first 32 bytes in hex
                            );
                            client_write.write_all(&current_chunk[..current_n]).await?;
                            total_bytes += current_n;

                            // Check terminator in chunk we just wrote
                            let has_term = if current_n >= 5 {
                                current_chunk[current_n - 5..current_n] == *b"\r\n.\r\n"
                                    || (current_n >= 3
                                        && current_chunk[current_n - 3..current_n] == *b"\n.\n")
                            } else {
                                current_n >= 3 && current_chunk[..current_n] == *b"\n.\n"
                            };

                            if has_term {
                                break; // Done! We already wrote the final chunk
                            }

                            // Check boundary spanning terminator (ONLY if current chunk is small enough)
                            // This is rare - only check if terminator could span from previous chunk
                            let has_spanning_term = if tail_len >= 2 && (1..=4).contains(&current_n)
                            {
                                // Build combined view: tail + start of current chunk
                                let mut check_buf = [0u8; 9]; // max: 4 tail + 5 current
                                check_buf[..tail_len].copy_from_slice(&tail[..tail_len]);
                                let curr_copy = current_n.min(5);
                                check_buf[tail_len..tail_len + curr_copy]
                                    .copy_from_slice(&current_chunk[..curr_copy]);
                                let total = tail_len + curr_copy;

                                (total >= 5 && check_buf[total - 5..total] == *b"\r\n.\r\n")
                                    || (total >= 3 && check_buf[total - 3..total] == *b"\n.\n")
                            } else {
                                false
                            };

                            if has_spanning_term {
                                break; // Done! We already wrote the final chunk
                            }

                            // Update tail for next iteration (only last 4 bytes)
                            if current_n >= TERMINATOR_TAIL_SIZE {
                                tail.copy_from_slice(
                                    &current_chunk[current_n - TERMINATOR_TAIL_SIZE..current_n],
                                );
                                tail_len = TERMINATOR_TAIL_SIZE;
                            } else if current_n > 0 {
                                tail[..current_n].copy_from_slice(&current_chunk[..current_n]);
                                tail_len = current_n;
                            }

                            // Read next chunk
                            let next_n = pooled_conn.read(next_chunk).await?;
                            if next_n == 0 {
                                break; // EOF
                            }

                            // Swap buffers for next iteration
                            std::mem::swap(&mut current_chunk, &mut next_chunk);
                            current_n = next_n;
                        }
                    }
                }
            }
        }

        debug!(
            "Client {} forwarded response ({} bytes) to client",
            self.client_addr, total_bytes
        );
        *backend_to_client_bytes += total_bytes as u64;

        // Complete the request - decrement pending count (lock-free!)
        router.complete_command_sync(backend_id);

        Ok(())
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

        assert!(!session.is_per_command_routing());
        assert_eq!(session.client_addr.port(), 8080);
    }

    #[test]
    fn test_session_with_router() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(1024, 4);
        let router = Arc::new(BackendSelector::new());
        let session = ClientSession::new_with_router(addr, buffer_pool, router);

        assert!(session.is_per_command_routing());
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
    async fn test_quit_command_per_command_routing() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        // Start a mock server for the backend
        let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_addr = backend_listener.local_addr().unwrap();

        // Spawn mock backend server
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = backend_listener.accept().await {
                // Send greeting
                let _ = stream.write_all(b"200 Mock Server Ready\r\n").await;

                // Read and discard any commands, keep connection alive briefly
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await;
            }
        });

        // Give server time to start
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Create a router with the mock backend
        let mut router = BackendSelector::new();
        let provider = crate::pool::DeadpoolConnectionProvider::new(
            "127.0.0.1".to_string(),
            backend_addr.port(),
            "test-backend".to_string(),
            2,
            None,
            None,
        );
        router.add_backend(
            crate::types::BackendId::from_index(0),
            "test-backend".to_string(),
            provider,
        );

        // Create client connection
        let client_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client_addr = client_listener.local_addr().unwrap();

        // Create session
        let buffer_pool = BufferPool::new(1024, 4);
        let session = ClientSession::new_with_router(client_addr, buffer_pool, Arc::new(router));

        // Spawn client that sends QUIT and immediately closes
        let client_handle = tokio::spawn(async move {
            let mut client = tokio::net::TcpStream::connect(client_addr).await.unwrap();

            // Read greeting
            let mut greeting = [0u8; 256];
            let n = client.read(&mut greeting).await.unwrap();
            assert!(n > 0);

            // Send QUIT command
            client.write_all(b"QUIT\r\n").await.unwrap();

            // Try to read response (might fail if we close too fast, which is fine)
            let mut response = [0u8; 256];
            let _ = client.read(&mut response).await;

            // Close connection immediately (simulating SABnzbd behavior)
            drop(client);
        });

        // Accept client connection
        let (client_stream, _) = client_listener.accept().await.unwrap();

        // Handle the session - should not return an error despite client closing
        let result = session.handle_per_command_routing(client_stream).await;

        // Should succeed (not propagate broken pipe error)
        assert!(
            result.is_ok(),
            "QUIT handling should not return error: {:?}",
            result
        );

        if let Ok((sent, received)) = result {
            // Should have sent QUIT command
            assert!(sent > 0, "Should have sent bytes (QUIT command)");
            // Should have received greeting and possibly closing message
            assert!(received > 0, "Should have received bytes (greeting)");
        }

        // Wait for client to finish
        let _ = client_handle.await;
    }

    #[tokio::test]
    async fn test_quit_command_closes_connection_cleanly() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        // Start a mock backend
        let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_addr = backend_listener.local_addr().unwrap();

        tokio::spawn(async move {
            if let Ok((mut stream, _)) = backend_listener.accept().await {
                let _ = stream.write_all(b"200 Ready\r\n").await;
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await;
            }
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Create router
        let mut router = BackendSelector::new();
        let provider = crate::pool::DeadpoolConnectionProvider::new(
            "127.0.0.1".to_string(),
            backend_addr.port(),
            "test".to_string(),
            1,
            None,
            None,
        );
        router.add_backend(
            crate::types::BackendId::from_index(0),
            "test".to_string(),
            provider,
        );

        // Create client
        let client_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client_addr = client_listener.local_addr().unwrap();

        let buffer_pool = BufferPool::new(1024, 4);
        let session = ClientSession::new_with_router(client_addr, buffer_pool, Arc::new(router));

        // Client that sends QUIT and waits for response
        let client_handle = tokio::spawn(async move {
            let mut client = tokio::net::TcpStream::connect(client_addr).await.unwrap();

            // Read greeting
            let mut buf = [0u8; 256];
            let n = client.read(&mut buf).await.unwrap();
            assert!(n > 0, "Should receive greeting");

            // Send QUIT
            client.write_all(b"QUIT\r\n").await.unwrap();

            // Read closing response
            let n = client.read(&mut buf).await.unwrap();

            // Should receive "205 Connection closing"
            let response = String::from_utf8_lossy(&buf[..n]);
            assert!(
                response.contains("205"),
                "Should receive 205 closing response"
            );
        });

        let (client_stream, _) = client_listener.accept().await.unwrap();
        let result = session.handle_per_command_routing(client_stream).await;

        assert!(result.is_ok(), "Session should handle QUIT cleanly");

        client_handle.await.unwrap();
    }
}
