//! Standard 1:1 routing mode handler

use crate::session::ClientSession;
use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, warn};

use crate::command::CommandHandler;
use crate::constants::buffer::COMMAND;
use crate::types::BytesTransferred;

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

        let mut client_to_backend_bytes = BytesTransferred::zero();
        let mut backend_to_client_bytes = BytesTransferred::zero();

        // Reuse line buffer to avoid per-iteration allocations
        let mut line = String::with_capacity(COMMAND);

        // Auth state: username from AUTHINFO USER command
        let mut auth_username: Option<String> = None;

        // PERFORMANCE: Cache authenticated state to avoid atomic loads after auth succeeds
        // Auth is disabled or auth happens once, then we skip checks for rest of session
        let mut skip_auth_check = !self.auth_handler.is_enabled();

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
                            break; // Client disconnected
                        }
                        Ok(n) => {
                            debug!("Client {} sent {} bytes: {:?}", self.client_addr, n, line.trim());
                            let trimmed = line.trim();
                            debug!("Client {} command: {}", self.client_addr, trimmed);

                            // PERFORMANCE OPTIMIZATION: Skip auth checking after first auth
                            // Auth happens ONCE per session, then thousands of ARTICLE commands follow
                            //
                            // Cache the authenticated state to avoid atomic loads on every command.
                            // Once authenticated, we never go back, so caching is safe.
                            skip_auth_check = skip_auth_check || self.authenticated.load(std::sync::atomic::Ordering::Acquire);
                            if skip_auth_check {
                                // Already authenticated - just forward everything (HOT PATH)
                                backend_write.write_all(line.as_bytes()).await?;
                                client_to_backend_bytes.add(line.len());
                            } else {
                                // Not yet authenticated and auth is enabled - check for auth commands
                                use crate::command::CommandAction;
                                let action = CommandHandler::handle_command(&line);
                                match action {
                                    CommandAction::ForwardStateless => {
                                        // Reject all non-auth commands before authentication
                                        let response = b"480 Authentication required\r\n";
                                        client_write.write_all(response).await?;
                                        backend_to_client_bytes.add(response.len());
                                    }
                                    CommandAction::InterceptAuth(auth_action) => {
                                        // Store username if this is AUTHINFO USER
                                        if let crate::command::AuthAction::RequestPassword(ref username) = auth_action {
                                            auth_username = Some(username.clone());
                                        }

                                        // Handle auth and validate
                                        let (bytes, auth_success) = self
                                            .auth_handler
                                            .handle_auth_command(auth_action, &mut client_write, auth_username.as_deref())
                                            .await?;
                                        backend_to_client_bytes.add(bytes);

                                        if auth_success {
                                            self.authenticated.store(true, std::sync::atomic::Ordering::Release);
                                        }
                                    }
                                    CommandAction::Reject(response) => {
                                        // Send rejection response inline
                                        client_write.write_all(response.as_bytes()).await?;
                                        backend_to_client_bytes.add(response.len());
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            warn!("Error reading from client {}: {}", self.client_addr, e);
                            break;
                        }
                    }
                }

                // Read response from backend and forward to client (for non-auth commands)
                result = backend_read.read(&mut buffer) => {
                    match result {
                        Ok(0) => {
                            break; // Backend disconnected
                        }
                        Ok(n) => {
                            client_write.write_all(&buffer[..n]).await?;
                            backend_to_client_bytes.add(n);
                        }
                        Err(e) => {
                            warn!("Error reading from backend for client {}: {}", self.client_addr, e);
                            break;
                        }
                    }
                }
            }
        }

        Ok((
            client_to_backend_bytes.as_u64(),
            backend_to_client_bytes.as_u64(),
        ))
    }
}
