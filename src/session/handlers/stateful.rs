//! Stateful 1:1 routing mode handler

use crate::session::{ClientSession, common};
use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::warn;

use crate::constants::buffer::{COMMAND, READER_CAPACITY};
use crate::types::TransferMetrics;

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

        // Initialize session loop state
        let mut state = common::SessionLoopState::new(self.auth_handler.is_enabled());

        // Reuse line buffer to avoid per-iteration allocations
        let mut line = String::with_capacity(COMMAND);

        loop {
            line.clear();
            let mut buffer = self.buffer_pool.acquire().await;

            // Periodically flush metrics for long-running sessions
            if state.check_and_maybe_flush_metrics()
                && let Some(bid) = backend_id
            {
                self.flush_incremental_metrics(
                    bid,
                    state.client_to_backend,
                    state.backend_to_client,
                    &mut state.last_reported_c2b,
                    &mut state.last_reported_b2c,
                );
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
                            state.skip_auth_check = self.is_authenticated_cached(state.skip_auth_check);
                            if state.skip_auth_check {
                                // Already authenticated - just forward everything (HOT PATH)
                                backend_write.write_all(line.as_bytes()).await?;
                                state.client_to_backend = state.client_to_backend.add(line.len());
                            } else {
                                // Not yet authenticated and auth is enabled - check for auth commands
                                match common::handle_stateful_auth_check(
                                    &line,
                                    &mut client_write,
                                    &mut state.auth_username,
                                    &self.auth_handler,
                                    &self.auth_state,
                                    &crate::config::RoutingMode::Stateful,
                                    &self.metrics,
                                    self.connection_stats(),
                                    self.client_addr,
                                    |username| self.set_username(username),
                                ).await? {
                                    common::AuthHandlerResult::Authenticated { bytes_written, skip_further_checks } => {
                                        state.backend_to_client = state.backend_to_client.add_u64(bytes_written);
                                        state.skip_auth_check = skip_further_checks;
                                    }
                                    common::AuthHandlerResult::NotAuthenticated { bytes_written } => {
                                        state.backend_to_client = state.backend_to_client.add_u64(bytes_written);
                                    }
                                    common::AuthHandlerResult::Rejected { bytes_written } => {
                                        state.backend_to_client = state.backend_to_client.add_u64(bytes_written);
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
                            state.backend_to_client = state.backend_to_client.add(n);
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
                state.client_to_backend,
                state.backend_to_client,
                state.last_reported_c2b.as_u64(),
                state.last_reported_b2c.as_u64(),
            );
        }

        Ok(TransferMetrics {
            client_to_backend: state.client_to_backend,
            backend_to_client: state.backend_to_client,
        })
    }

    /// Handle stateful routing session - get pooled connection and proxy through it
    ///
    /// This is the top-level handler for stateful mode that orchestrates:
    /// - Acquiring a pooled backend connection
    /// - Sending client error if connection unavailable
    /// - Proxying the full session through the dedicated backend
    /// - Auto-returning connection to pool on completion (via Drop)
    pub async fn handle_stateful_session(
        &self,
        mut client_stream: tokio::net::TcpStream,
        backend_id: crate::types::BackendId,
        connection_provider: &crate::pool::DeadpoolConnectionProvider,
        server_name: &str,
        enable_metrics: bool,
    ) -> Result<TransferMetrics> {
        use crate::protocol::BACKEND_UNAVAILABLE;
        use tracing::{debug, error};

        // Acquire backend connection
        let mut backend_conn = match connection_provider.get_pooled_connection().await {
            Ok(conn) => {
                debug!("Got pooled connection for {}", server_name);
                conn
            }
            Err(e) => {
                error!(
                    "Failed to get pooled connection for {} (client {}): {}",
                    server_name, self.client_addr, e
                );

                // Notify client before bailing
                client_stream.write_all(BACKEND_UNAVAILABLE).await?;

                anyhow::bail!(
                    "Failed to get pooled connection for backend '{}' (client {}): {}",
                    server_name,
                    self.client_addr,
                    e
                );
            }
        };

        // Proxy session through backend with optional metrics
        match enable_metrics {
            true => {
                self.handle_with_pooled_backend_and_metrics(
                    client_stream,
                    &mut *backend_conn,
                    backend_id,
                )
                .await
            }
            false => {
                self.handle_with_pooled_backend(client_stream, &mut *backend_conn)
                    .await
            }
        }
    }
}
