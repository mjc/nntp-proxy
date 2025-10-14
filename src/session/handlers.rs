//! Session handler implementations
//!
//! This module contains the main session handling methods split from mod.rs:
//! - handle_with_pooled_backend: Standard 1:1 mode
//! - handle_per_command_routing: Per-command and hybrid routing modes
//! - switch_to_stateful_mode: Hybrid mode transition to stateful
//! - route_and_execute_command: Single command routing
//! - execute_command_on_backend: Command execution with pipelined streaming

use super::ClientSession;
use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, error, info, warn};

use crate::auth::AuthHandler;
use crate::command::{AuthAction, CommandAction, CommandHandler, NntpCommand};
use crate::config::RoutingMode;
use crate::constants::buffer::COMMAND_SIZE;
use crate::protocol::{
    BACKEND_ERROR, COMMAND_NOT_SUPPORTED_STATELESS, CONNECTION_CLOSING, PROXY_GREETING_PCR,
};
use crate::router::BackendSelector;
use crate::streaming::StreamHandler;

use super::{backend, connection, streaming};

impl ClientSession {
    /// Handle a client connection with a dedicated backend connection (standard 1:1 mode)
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
                                    client_write.write_all(COMMAND_NOT_SUPPORTED_STATELESS).await?;
                                    backend_to_client_bytes += COMMAND_NOT_SUPPORTED_STATELESS.len() as u64;
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
        use tokio::io::BufReader;

        debug!(
            "Client {} starting per-command routing session",
            self.client_addr
        );

        let router = self
            .router
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Per-command routing mode requires a router"))?;

        let (client_read, mut client_write) = client_stream.split();
        let mut client_reader = BufReader::new(client_read);

        let mut client_to_backend_bytes = 0u64;
        let mut backend_to_client_bytes = 0u64;

        // Send initial greeting to client
        debug!(
            "Client {} sending greeting: {} | hex: {:02x?}",
            self.client_addr,
            String::from_utf8_lossy(PROXY_GREETING_PCR),
            PROXY_GREETING_PCR
        );

        if let Err(e) = client_write.write_all(PROXY_GREETING_PCR).await {
            debug!(
                "Client {} failed to send greeting: {} (kind: {:?}). \
                 This suggests the client disconnected immediately after connecting.",
                self.client_addr,
                e,
                e.kind()
            );
            return Err(e.into());
        }
        backend_to_client_bytes += PROXY_GREETING_PCR.len() as u64;

        debug!(
            "Client {} sent greeting successfully, entering command loop",
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
                        self.client_addr,
                        n,
                        trimmed,
                        command.as_bytes()
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
                        backend_to_client_bytes += CONNECTION_CLOSING.len() as u64;
                        debug!("Client {} sent QUIT, closing connection", self.client_addr);
                        break;
                    }

                    // Check if command should be rejected or if we need to switch modes in hybrid routing
                    match CommandHandler::handle_command(&command) {
                        CommandAction::InterceptAuth(auth_action) => {
                            // Handle authentication locally
                            let response = match auth_action {
                                AuthAction::RequestPassword => AuthHandler::user_response(),
                                AuthAction::AcceptAuth => AuthHandler::pass_response(),
                            };
                            client_write.write_all(response).await?;
                            backend_to_client_bytes += response.len() as u64;
                        }
                        CommandAction::Reject(reason) => {
                            // In hybrid mode, stateful commands trigger a switch to stateful connection
                            if self.routing_mode == RoutingMode::Hybrid
                                && NntpCommand::classify(&command).is_stateful()
                            {
                                info!(
                                    "Client {} switching to stateful mode (command: {})",
                                    self.client_addr, trimmed
                                );

                                // Switch to stateful mode - acquire a dedicated backend connection
                                return self
                                    .switch_to_stateful_mode(
                                        client_reader,
                                        client_write,
                                        &command,
                                        client_to_backend_bytes,
                                        backend_to_client_bytes,
                                    )
                                    .await;
                            }

                            // In non-hybrid per-command mode, reject stateful commands
                            warn!(
                                "Rejecting command from client {}: {} ({})",
                                self.client_addr, trimmed, reason
                            );
                            client_write
                                .write_all(COMMAND_NOT_SUPPORTED_STATELESS)
                                .await?;
                            backend_to_client_bytes += COMMAND_NOT_SUPPORTED_STATELESS.len() as u64;
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
                                        connection::log_routing_error(
                                            self.client_addr,
                                            io_err,
                                            &command,
                                            client_to_backend_bytes,
                                            backend_to_client_bytes,
                                        );
                                    } else {
                                        error!(
                                            "Error routing command '{}' for client {}: {}. \
                                             Session stats: {} bytes sent to backend, {} bytes received from backend.",
                                            trimmed,
                                            self.client_addr,
                                            e,
                                            client_to_backend_bytes,
                                            backend_to_client_bytes
                                        );
                                    }

                                    // Try to send error response, but don't log failure if client is gone
                                    let _ = client_write.write_all(BACKEND_ERROR).await;
                                    backend_to_client_bytes += BACKEND_ERROR.len() as u64;

                                    // For debugging test connections and small transfers, log detailed info
                                    if client_to_backend_bytes + backend_to_client_bytes < 500 {
                                        debug!(
                                            "ERROR SUMMARY for small transfer - Client {}: \
                                             Command '{}' failed with {}. \
                                             Total session: {} bytes to backend, {} bytes from backend. \
                                             This appears to be a short session (test connection?). \
                                             Check debug logs above for full command/response hex dumps.",
                                            self.client_addr,
                                            trimmed,
                                            e,
                                            client_to_backend_bytes,
                                            backend_to_client_bytes
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    connection::log_client_error(
                        self.client_addr,
                        &e,
                        client_to_backend_bytes,
                        backend_to_client_bytes,
                    );
                    break;
                }
            }
        }

        // Log session summary for debugging, especially useful for test connections
        if client_to_backend_bytes + backend_to_client_bytes < 500 {
            debug!(
                "SESSION SUMMARY for Client {}: Small transfer completed successfully. \
                 {} bytes sent to backend, {} bytes received from backend. \
                 This appears to be a short session (likely test connection). \
                 Check debug logs above for individual command/response details.",
                self.client_addr, client_to_backend_bytes, backend_to_client_bytes
            );
        }

        Ok((client_to_backend_bytes, backend_to_client_bytes))
    }

    /// Switch from per-command routing to stateful mode with a dedicated backend connection
    /// This is called in hybrid mode when a stateful command is detected
    async fn switch_to_stateful_mode(
        &self,
        mut client_reader: tokio::io::BufReader<tokio::net::tcp::ReadHalf<'_>>,
        mut client_write: tokio::net::tcp::WriteHalf<'_>,
        initial_command: &str,
        mut client_to_backend_bytes: u64,
        mut backend_to_client_bytes: u64,
    ) -> Result<(u64, u64)> {
        debug!(
            "Client {} acquiring dedicated backend connection for stateful mode",
            self.client_addr
        );

        let router = self
            .router
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Router not available"))?;

        // Get a backend for this session
        let backend_id = router.route_command_sync(self.client_id, initial_command)?;

        // Try to acquire a stateful slot (respects max_connections-1 limit)
        if !router.try_acquire_stateful(backend_id) {
            // All stateful slots are taken - reject this switch
            warn!(
                "Client {} cannot switch to stateful mode: backend {:?} stateful limit reached",
                self.client_addr, backend_id
            );
            const STATEFUL_SLOTS_ERROR: &[u8] =
                b"503 All stateful connection slots are in use. Try again later.\r\n";
            client_write.write_all(STATEFUL_SLOTS_ERROR).await?;
            backend_to_client_bytes += STATEFUL_SLOTS_ERROR.len() as u64;

            // Continue in per-command mode
            return Ok((client_to_backend_bytes, backend_to_client_bytes));
        }

        let provider = router.get_backend_provider(backend_id).ok_or_else(|| {
            // Release the slot if provider lookup fails
            router.release_stateful(backend_id);
            anyhow::anyhow!("Backend {:?} not found", backend_id)
        })?;

        // Acquire a dedicated connection from the pool
        let mut pooled_conn = match provider.get_pooled_connection().await {
            Ok(conn) => conn,
            Err(e) => {
                // Release the stateful slot on connection failure
                router.release_stateful(backend_id);
                return Err(e);
            }
        };

        info!(
            "Client {} switched to stateful mode with backend {:?}",
            self.client_addr, backend_id
        );

        // Use backend helper to send command and read first chunk
        let (chunk, n, _response_code, _is_multiline) =
            match backend::send_command_and_read_first_chunk(
                &mut *pooled_conn,
                initial_command,
                backend_id,
                self.client_addr,
            )
            .await
            {
                Ok(result) => result,
                Err(e) => {
                    if crate::pool::is_connection_error(&e) {
                        debug!(
                            "Backend error during stateful switch for client {} ({}), removing connection from pool",
                            self.client_addr, e
                        );
                        router.release_stateful(backend_id);
                        crate::pool::remove_from_pool(pooled_conn);
                    }
                    return Err(e);
                }
            };

        client_to_backend_bytes += initial_command.len() as u64;

        // Forward the initial response to client
        client_write.write_all(&chunk[..n]).await?;
        backend_to_client_bytes += n as u64;

        // Use connection helper for bidirectional forwarding
        let result = connection::bidirectional_forward(
            &mut client_reader,
            &mut client_write,
            &mut *pooled_conn,
            &self.buffer_pool,
            self.client_addr,
            client_to_backend_bytes,
            backend_to_client_bytes,
        )
        .await?;

        // Handle the result - remove from pool if backend error
        match result {
            connection::ForwardResult::BackendError {
                client_to_backend,
                backend_to_client,
            } => {
                router.release_stateful(backend_id);
                crate::pool::remove_from_pool(pooled_conn);
                client_to_backend_bytes = client_to_backend;
                backend_to_client_bytes = backend_to_client;
            }
            connection::ForwardResult::NormalDisconnect {
                client_to_backend,
                backend_to_client,
            } => {
                router.release_stateful(backend_id);
                client_to_backend_bytes = client_to_backend;
                backend_to_client_bytes = backend_to_client;
            }
        }

        info!(
            "Client {} stateful session ended: {} bytes sent, {} bytes received",
            self.client_addr, client_to_backend_bytes, backend_to_client_bytes
        );

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
        use crate::pool::{is_connection_error, remove_from_pool};

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

        let mut pooled_conn = provider.get_pooled_connection().await?;

        debug!(
            "Client {} got pooled connection for backend {:?}",
            self.client_addr, backend_id
        );

        // Execute the command - returns (result, got_backend_data)
        // If got_backend_data is true, we successfully communicated with backend
        let (result, got_backend_data) = self
            .execute_command_on_backend(
                &mut pooled_conn,
                command,
                client_write,
                backend_id,
                client_to_backend_bytes,
                backend_to_client_bytes,
            )
            .await;

        // Only remove backend connection if error occurred AND we didn't get data from backend
        // If we got data from backend, then any error is from writing to client
        if let Err(ref e) = result {
            if !got_backend_data && is_connection_error(e) {
                warn!(
                    "Backend connection error for client {}, backend {:?}: {} - removing connection from pool",
                    self.client_addr, backend_id, e
                );
                remove_from_pool(pooled_conn);
                router.complete_command_sync(backend_id);
                return result;
            } else if got_backend_data && is_connection_error(e) {
                debug!(
                    "Client {} disconnected while receiving data from backend {:?} - backend connection is healthy",
                    self.client_addr, backend_id
                );
            }
        }

        // Complete the request - decrement pending count (lock-free!)
        router.complete_command_sync(backend_id);

        result
    }

    /// Execute a command on a backend connection and stream the response to the client
    ///
    /// # Performance Critical Hot Path
    ///
    /// This function implements **pipelined streaming** for NNTP responses, which is essential
    /// for high-throughput article downloads. The double-buffering approach allows reading the
    /// next chunk from the backend while writing the current chunk to the client concurrently.
    ///
    /// **DO NOT refactor this to buffer entire responses** - that would kill performance:
    /// - Large articles (50MB+) would be fully buffered before streaming to client
    /// - No concurrent I/O = sequential read-then-write instead of pipelined read+write
    /// - Performance drops from 100+ MB/s to < 1 MB/s
    ///
    /// The complexity here is justified by the 100x+ performance gain on large transfers.
    ///
    /// # Return Value
    ///
    /// Returns `(Result<()>, got_backend_data)` where:
    /// - `got_backend_data = true` means we successfully read from backend before any error
    /// - This distinguishes backend failures (remove from pool) from client disconnects (keep backend)
    async fn execute_command_on_backend(
        &self,
        pooled_conn: &mut deadpool::managed::Object<crate::pool::deadpool_connection::TcpManager>,
        command: &str,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        backend_id: crate::types::BackendId,
        client_to_backend_bytes: &mut u64,
        backend_to_client_bytes: &mut u64,
    ) -> (Result<()>, bool) {
        let mut got_backend_data = false;

        // Send command and read first chunk
        let (chunk, n, _response_code, is_multiline) =
            match backend::send_command_and_read_first_chunk(
                &mut **pooled_conn,
                command,
                backend_id,
                self.client_addr,
            )
            .await
            {
                Ok(result) => {
                    got_backend_data = true;
                    result
                }
                Err(e) => return (Err(e), got_backend_data),
            };

        *client_to_backend_bytes += command.len() as u64;

        // Extract message-ID from command if present (for correlation with SABnzbd errors)
        let msgid = if let Some(start) = command.find('<') {
            if let Some(end) = command[start..].find('>') {
                Some(&command[start..start + end + 1])
            } else {
                None
            }
        } else {
            None
        };

        // For multiline responses, use pipelined streaming
        let bytes_written = if is_multiline {
            let log_msg = if let Some(id) = msgid {
                format!("Client {} ARTICLE {} -> multiline response (status code: {:?}), streaming {} bytes", 
                    self.client_addr, id, _response_code.status_code(), n)
            } else {
                format!("Client {} command '{}' -> multiline response (status code: {:?}), streaming {} bytes",
                    self.client_addr, command.trim(), _response_code.status_code(), n)
            };
            debug!("{}", log_msg);
            
            match streaming::stream_multiline_response(
                &mut **pooled_conn,
                client_write,
                &chunk,
                n,
                self.client_addr,
            )
            .await
            {
                Ok(bytes) => bytes,
                Err(e) => return (Err(e), got_backend_data),
            }
        } else {
            // Single-line response - just write the first chunk
            let log_msg = if let Some(id) = msgid {
                // 430 (No such article) and other 4xx errors are expected single-line responses
                if let Some(code) = _response_code.status_code() {
                    if code >= 400 && code < 500 {
                        format!("Client {} ARTICLE {} -> error response {} (single-line), writing {} bytes",
                            self.client_addr, id, code, n)
                    } else {
                        format!("Client {} ARTICLE {} -> UNUSUAL single-line response (status code: {:?}), writing {} bytes: {:02x?}",
                            self.client_addr, id, _response_code.status_code(), n, &chunk[..n.min(50)])
                    }
                } else {
                    format!("Client {} ARTICLE {} -> UNUSUAL single-line response (status code: {:?}), writing {} bytes: {:02x?}",
                        self.client_addr, id, _response_code.status_code(), n, &chunk[..n.min(50)])
                }
            } else {
                format!("Client {} command '{}' -> single-line response (status code: {:?}), writing {} bytes: {:02x?}",
                    self.client_addr, command.trim(), _response_code.status_code(), n, &chunk[..n.min(50)])
            };
            
            // Only warn if it's truly unusual (not a 4xx/5xx error response)
            if let Some(code) = _response_code.status_code() {
                if code >= 400 {
                    debug!("{}", log_msg); // Errors are expected, just debug
                } else if msgid.is_some() {
                    warn!("{}", log_msg); // ARTICLE with 2xx/3xx single-line is unusual
                } else {
                    debug!("{}", log_msg);
                }
            } else {
                debug!("{}", log_msg);
            }
            
            match client_write.write_all(&chunk[..n]).await {
                Ok(_) => n as u64,
                Err(e) => return (Err(e.into()), got_backend_data),
            }
        };

        *backend_to_client_bytes += bytes_written;

        if let Some(id) = msgid {
            debug!(
                "Client {} ARTICLE {} completed: wrote {} bytes to client",
                self.client_addr, id, bytes_written
            );
        }

        (Ok(()), got_backend_data)
    }
}
