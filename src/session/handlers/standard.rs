//! Standard 1:1 routing mode handler

use crate::session::ClientSession;
use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, warn};

use crate::command::CommandHandler;
use crate::constants::buffer::{COMMAND, READER_CAPACITY};
use crate::types::{BytesTransferred, TransferMetrics};

impl ClientSession {
    /// Handle a client connection with a dedicated backend connection (standard 1:1 mode)
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
        backend_id: usize,
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
        backend_id: Option<usize>,
    ) -> Result<TransferMetrics>
    where
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        use tokio::io::BufReader;

        // Split streams for independent read/write
        let (client_read, mut client_write) = client_stream.split();
        let (mut backend_read, mut backend_write) = tokio::io::split(backend_conn);
        let mut client_reader = BufReader::with_capacity(READER_CAPACITY, client_read);

        let mut client_to_backend_bytes = BytesTransferred::zero();
        let mut backend_to_client_bytes = BytesTransferred::zero();

        // Track last reported values for incremental metrics updates
        let mut last_reported_c2b = 0u64;
        let mut last_reported_b2c = 0u64;

        // Reuse line buffer to avoid per-iteration allocations
        let mut line = String::with_capacity(COMMAND);

        // Auth state: username from AUTHINFO USER command
        let mut auth_username: Option<String> = None;

        // PERFORMANCE: Cache authenticated state to avoid atomic loads after auth succeeds
        // Auth is disabled or auth happens once, then we skip checks for rest of session
        let mut skip_auth_check = !self.auth_handler.is_enabled();

        debug!("Client {} session loop starting", self.client_addr);

        // Counter for periodic metrics flush (every N iterations)
        let mut iteration_count = 0u32;
        const METRICS_FLUSH_INTERVAL: u32 = 100; // Flush every 100 commands

        loop {
            line.clear();
            let mut buffer = self.buffer_pool.get_buffer().await;

            // Periodically flush metrics for long-running sessions
            iteration_count += 1;
            if iteration_count >= METRICS_FLUSH_INTERVAL {
                if let (Some(metrics), Some(bid)) = (self.metrics.as_ref(), backend_id) {
                    let current_c2b = client_to_backend_bytes.as_u64();
                    let current_b2c = backend_to_client_bytes.as_u64();

                    let delta_c2b = current_c2b.saturating_sub(last_reported_c2b);
                    let delta_b2c = current_b2c.saturating_sub(last_reported_b2c);

                    if delta_c2b > 0 {
                        metrics.record_client_to_backend_bytes_for(bid, delta_c2b);
                    }
                    if delta_b2c > 0 {
                        metrics.record_backend_to_client_bytes_for(bid, delta_b2c);
                    }

                    last_reported_c2b = current_c2b;
                    last_reported_b2c = current_b2c;
                }
                iteration_count = 0;
            }

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
                                        use crate::protocol::AUTH_REQUIRED_FOR_COMMAND;
                                        client_write.write_all(AUTH_REQUIRED_FOR_COMMAND).await?;
                                        backend_to_client_bytes.add(AUTH_REQUIRED_FOR_COMMAND.len());
                                    }
                                    CommandAction::InterceptAuth(auth_action) => {
                                        let result = crate::session::common::handle_auth_command(
                                            &self.auth_handler,
                                            auth_action,
                                            &mut client_write,
                                            &mut auth_username,
                                            &self.authenticated,
                                        )
                                        .await?;

                                        backend_to_client_bytes += result.bytes_written;
                                        if result.authenticated {
                                            // Store username after successful authentication
                                            self.set_username(auth_username.clone());

                                            // Record connection for aggregation (after auth so we have username)
                                            if let Some(stats) = self.connection_stats() {
                                                stats.record_connection(
                                                    auth_username.as_deref(),
                                                    "standard",
                                                );
                                            }

                                            // Track user connection in metrics
                                            if let Some(metrics) = self.metrics.as_ref() {
                                                metrics.user_connection_opened(auth_username.as_deref());
                                            }

                                            skip_auth_check = true;
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
                n = buffer.read_from(&mut backend_read) => {
                    match n {
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

        // Report final metrics deltas before session ends
        if let (Some(metrics), Some(bid)) = (self.metrics.as_ref(), backend_id) {
            let current_c2b = client_to_backend_bytes.as_u64();
            let current_b2c = backend_to_client_bytes.as_u64();

            let delta_c2b = current_c2b.saturating_sub(last_reported_c2b);
            let delta_b2c = current_b2c.saturating_sub(last_reported_b2c);

            if delta_c2b > 0 {
                metrics.record_client_to_backend_bytes_for(bid, delta_c2b);
            }
            if delta_b2c > 0 {
                metrics.record_backend_to_client_bytes_for(bid, delta_b2c);
            }

            // Track final per-user metrics
            if let Some(username) = self.username() {
                if delta_c2b > 0 {
                    metrics.user_bytes_sent(Some(&username), delta_c2b);
                }
                if delta_b2c > 0 {
                    metrics.user_bytes_received(Some(&username), delta_b2c);
                }
                metrics.user_connection_closed(Some(&username));
            } else {
                // Anonymous user
                if delta_c2b > 0 {
                    metrics.user_bytes_sent(None, delta_c2b);
                }
                if delta_b2c > 0 {
                    metrics.user_bytes_received(None, delta_b2c);
                }
                metrics.user_connection_closed(None);
            }
        }

        Ok(TransferMetrics {
            client_to_backend: client_to_backend_bytes,
            backend_to_client: backend_to_client_bytes,
        })
    }
}
