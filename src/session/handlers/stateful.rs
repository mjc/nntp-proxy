//! Stateful 1:1 routing mode handler

use crate::session::{ClientSession, common};
use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::warn;

use crate::command::CommandHandler;
use crate::constants::buffer::{COMMAND, READER_CAPACITY};
use crate::types::{BackendToClientBytes, ClientToBackendBytes, TransferMetrics};

impl ClientSession {
    /// Handle a client connection with a dedicated backend connection (stateful 1:1 mode)
    ///
    /// # Metrics Reporting
    ///
    /// If `backend_id` is provided and metrics are enabled, this handler will:
    /// - Report byte counts periodically during long-running sessions
    /// - Enable real-time throughput monitoring in the TUI
    /// - Still return final totals at session end for accuracy
    pub async fn handle_with_pooled_backend<T>(
        &self,
        client_stream: TcpStream,
        backend_conn: T,
    ) -> Result<TransferMetrics>
    where
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        self.handle_with_pooled_backend_impl(client_stream, backend_conn, None)
            .await
    }

    /// Handle a client connection with periodic metrics reporting
    ///
    /// This version accepts a backend_id for per-backend metrics tracking.
    pub async fn handle_with_pooled_backend_and_metrics<T>(
        &self,
        client_stream: TcpStream,
        backend_conn: T,
        backend_id: crate::types::BackendId,
    ) -> Result<TransferMetrics>
    where
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        self.handle_with_pooled_backend_impl(client_stream, backend_conn, Some(backend_id))
            .await
    }

    /// Internal implementation with optional periodic metrics reporting
    async fn handle_with_pooled_backend_impl<T>(
        &self,
        mut client_stream: TcpStream,
        backend_conn: T,
        backend_id: Option<crate::types::BackendId>,
    ) -> Result<TransferMetrics>
    where
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        use tokio::io::BufReader;

        // Split streams for independent read/write
        let (client_read, mut client_write) = client_stream.split();
        let (mut backend_read, mut backend_write) = tokio::io::split(backend_conn);
        let mut client_reader = BufReader::with_capacity(READER_CAPACITY, client_read);

        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_to_client_bytes = BackendToClientBytes::zero();

        // Track last reported values for incremental metrics updates
        let mut last_reported_c2b = ClientToBackendBytes::zero();
        let mut last_reported_b2c = BackendToClientBytes::zero();

        // Reuse line buffer to avoid per-iteration allocations
        let mut line = String::with_capacity(COMMAND);

        // Auth state: username from AUTHINFO USER command
        let mut auth_username: Option<String> = None;

        // PERFORMANCE: Cache authenticated state to avoid atomic loads after auth succeeds
        // Auth is disabled or auth happens once, then we skip checks for rest of session
        let mut skip_auth_check = !self.auth_handler.is_enabled();

        // Counter for periodic metrics flush (every N iterations)
        let mut iteration_count = 0u32;
        const METRICS_FLUSH_INTERVAL: u32 = 100; // Flush every 1000 commands

        loop {
            line.clear();
            let mut buffer = self.buffer_pool.acquire().await;

            // Periodically flush metrics for long-running sessions
            iteration_count += 1;
            if iteration_count >= METRICS_FLUSH_INTERVAL {
                if let Some(bid) = backend_id {
                    self.flush_incremental_metrics(
                        bid,
                        client_to_backend_bytes,
                        backend_to_client_bytes,
                        &mut last_reported_c2b,
                        &mut last_reported_b2c,
                    );
                }
                iteration_count = 0;
            }

            tokio::select! {
                // Read command from client
                result = client_reader.read_line(&mut line) => {
                    match result {
                        Ok(0) => break,
                        Ok(_) => {
                            // PERFORMANCE OPTIMIZATION: Skip auth checking after first auth
                            // Auth happens ONCE per session, then thousands of ARTICLE commands follow
                            //
                            // Cache the authenticated state to avoid atomic loads on every command.
                            // Once authenticated, we never go back, so caching is safe.
                            skip_auth_check = self.is_authenticated_cached(skip_auth_check);
                            if skip_auth_check {
                                // Already authenticated - just forward everything (HOT PATH)
                                backend_write.write_all(line.as_bytes()).await?;
                                client_to_backend_bytes = client_to_backend_bytes.add(line.len());
                            } else {
                                // Not yet authenticated and auth is enabled - check for auth commands
                                use crate::command::CommandAction;
                                let action = CommandHandler::classify(&line);
                                match action {
                                    CommandAction::ForwardStateless => {
                                        // Reject all non-auth commands before authentication
                                        use crate::protocol::AUTH_REQUIRED_FOR_COMMAND;
                                        client_write.write_all(AUTH_REQUIRED_FOR_COMMAND).await?;
                                        backend_to_client_bytes = backend_to_client_bytes.add(AUTH_REQUIRED_FOR_COMMAND.len());
                                    }
                                    CommandAction::InterceptAuth(auth_action) => {
                                        backend_to_client_bytes = backend_to_client_bytes.add_u64(
                                            match crate::session::common::handle_auth_command(
                                                &self.auth_handler,
                                                auth_action,
                                                &mut client_write,
                                                &mut auth_username,
                                                &self.authenticated,
                                            )
                                            .await?
                                            {
                                                crate::session::common::AuthResult::Authenticated(bytes) => {
                                                    common::on_authentication_success(
                                                        self.client_addr,
                                                        auth_username.clone(),
                                                        &crate::config::RoutingMode::Stateful,
                                                        &self.metrics,
                                                        self.connection_stats(),
                                                        |username| self.set_username(username),
                                                    );

                                                    skip_auth_check = true;
                                                    bytes
                                                }
                                                crate::session::common::AuthResult::NotAuthenticated(bytes) => bytes,
                                            }.as_u64(),
                                        );
                                    }
                                    CommandAction::Reject(response) => {
                                        // Send rejection response inline
                                        client_write.write_all(response.as_bytes()).await?;
                                        backend_to_client_bytes = backend_to_client_bytes.add(response.len());
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
                n = buffer.read_from(&mut backend_read) => {
                    match n {
                        Ok(0) => {
                            break; // Backend disconnected
                        }
                        Ok(n) => {
                            client_write.write_all(&buffer[..n]).await?;
                            backend_to_client_bytes = backend_to_client_bytes.add(n);
                        }
                        Err(e) => {
                            warn!("Error reading from backend for client {}: {}", self.client_addr, e);
                            break;
                        }
                    }
                }
            }
        }

        // Report final metrics deltas before session ends
        if let Some(bid) = backend_id {
            self.report_final_metrics(
                bid,
                client_to_backend_bytes,
                backend_to_client_bytes,
                last_reported_c2b.as_u64(),
                last_reported_b2c.as_u64(),
            );
        }

        Ok(TransferMetrics {
            client_to_backend: client_to_backend_bytes,
            backend_to_client: backend_to_client_bytes,
        })
    }
}
