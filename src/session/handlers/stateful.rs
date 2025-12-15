//! Stateful 1:1 routing mode handler
//!
//! Bidirectional proxy: each client gets a dedicated backend connection.

use crate::session::metrics_ext::MetricsRecorder;
use crate::session::{ClientSession, common};
use crate::types::TransferMetrics;
use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tracing::{debug, error, warn};

use crate::constants::buffer::{COMMAND, READER_CAPACITY};

impl ClientSession {
    /// Handle stateful session - acquire backend and proxy bidirectionally
    pub async fn handle_stateful_session(
        &self,
        mut client_stream: TcpStream,
        backend_id: crate::types::BackendId,
        provider: &crate::pool::DeadpoolConnectionProvider,
        server_name: &str,
    ) -> Result<TransferMetrics> {
        use crate::protocol::BACKEND_UNAVAILABLE;

        // Acquire backend connection
        let mut backend_conn = match provider.get_pooled_connection().await {
            Ok(conn) => {
                debug!(server = server_name, "Got pooled connection");
                conn
            }
            Err(e) => {
                error!(server = server_name, client = %self.client_addr, error = %e, "Failed to get pooled connection");
                client_stream.write_all(BACKEND_UNAVAILABLE).await?;
                anyhow::bail!(
                    "Failed to get pooled connection for '{}': {}",
                    server_name,
                    e
                );
            }
        };

        // Split streams
        let (client_read, client_write) = client_stream.split();
        let client_reader = BufReader::with_capacity(READER_CAPACITY, client_read);
        let (backend_read, backend_write) = tokio::io::split(&mut *backend_conn);
        let state = common::SessionLoopState::new(self.auth_handler.is_enabled());

        self.run_stateful_proxy_loop(
            client_reader,
            client_write,
            backend_read,
            backend_write,
            state,
            backend_id,
        )
        .await
    }

    /// Core bidirectional proxy loop
    ///
    /// Used by both stateful mode and hybrid mode (after switching).
    pub(in crate::session) async fn run_stateful_proxy_loop<R, W, BR, BW>(
        &self,
        mut client_reader: BufReader<R>,
        mut client_write: W,
        mut backend_read: BR,
        mut backend_write: BW,
        mut state: common::SessionLoopState,
        backend_id: crate::types::BackendId,
    ) -> Result<TransferMetrics>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
        BR: tokio::io::AsyncRead + Unpin,
        BW: tokio::io::AsyncWrite + Unpin,
    {
        let mut line = String::with_capacity(COMMAND);

        loop {
            line.clear();
            let mut buffer = self.buffer_pool.acquire().await;

            // Periodic metrics flush
            if state.check_and_maybe_flush_metrics() {
                // Report byte deltas since last flush
                let delta_c2b = state
                    .client_to_backend
                    .as_u64()
                    .saturating_sub(state.last_reported_c2b.as_u64());
                let delta_b2c = state
                    .backend_to_client
                    .as_u64()
                    .saturating_sub(state.last_reported_b2c.as_u64());

                if delta_c2b > 0 {
                    self.metrics
                        .record_client_to_backend_bytes_for(backend_id, delta_c2b);
                    self.metrics
                        .user_bytes_sent(self.username().as_deref(), delta_c2b);
                }
                if delta_b2c > 0 {
                    self.metrics
                        .record_backend_to_client_bytes_for(backend_id, delta_b2c);
                    self.metrics
                        .user_bytes_received(self.username().as_deref(), delta_b2c);
                }

                state.last_reported_c2b = state.client_to_backend;
                state.last_reported_b2c = state.backend_to_client;
            }

            tokio::select! {
                // Client → Backend
                result = client_reader.read_line(&mut line) => {
                    match result {
                        Ok(0) => break, // Client disconnected
                        Ok(_) => {
                            state.skip_auth_check = self.is_authenticated_cached(state.skip_auth_check);

                            if state.skip_auth_check {
                                // Hot path: forward directly
                                backend_write.write_all(line.as_bytes()).await?;
                                state.add_client_to_backend(line.len());
                            } else {
                                // Auth path
                                let auth_result = common::handle_stateful_auth_check(
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
                                ).await?;
                                state.apply_auth_result(&auth_result);
                            }
                        }
                        Err(e) => {
                            warn!(client = %self.client_addr, error = %e, "Client read error");
                            break;
                        }
                    }
                }

                // Backend → Client
                result = buffer.read_from(&mut backend_read) => {
                    match result {
                        Ok(0) => break, // Backend disconnected
                        Ok(n) => {
                            client_write.write_all(&buffer[..n]).await?;
                            state.add_backend_to_client(n as u64);
                        }
                        Err(e) => {
                            warn!(client = %self.client_addr, error = %e, "Backend read error");
                            break;
                        }
                    }
                }
            }
        }

        // Final metrics - report any remaining byte deltas
        let delta_c2b = state
            .client_to_backend
            .as_u64()
            .saturating_sub(state.last_reported_c2b.as_u64());
        let delta_b2c = state
            .backend_to_client
            .as_u64()
            .saturating_sub(state.last_reported_b2c.as_u64());

        if delta_c2b > 0 {
            self.metrics
                .record_client_to_backend_bytes_for(backend_id, delta_c2b);
            self.metrics
                .user_bytes_sent(self.username().as_deref(), delta_c2b);
        }
        if delta_b2c > 0 {
            self.metrics
                .record_backend_to_client_bytes_for(backend_id, delta_b2c);
            self.metrics
                .user_bytes_received(self.username().as_deref(), delta_b2c);
        }

        self.metrics
            .user_connection_closed(self.username().as_deref());

        Ok(state.into_metrics())
    }
}
