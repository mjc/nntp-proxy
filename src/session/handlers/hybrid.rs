//! Hybrid mode switching handler
//!
//! This module implements the transition from per-command routing to stateful
//! routing when a stateful command is encountered in hybrid mode.

use crate::session::ClientSession;
use crate::session::common;
use anyhow::Result;
use tokio::io::BufReader;
use tokio::net::tcp::{ReadHalf, WriteHalf};
use tracing::{error, info, warn};

use crate::types::{BytesTransferred, TransferMetrics};

impl ClientSession {
    /// Switch from per-command routing to stateful mode by acquiring a dedicated backend connection
    pub(super) async fn switch_to_stateful_mode(
        &self,
        mut client_reader: BufReader<ReadHalf<'_>>,
        mut client_write: WriteHalf<'_>,
        initial_command: &str,
        client_to_backend_bytes: BytesTransferred,
        backend_to_client_bytes: BytesTransferred,
    ) -> Result<TransferMetrics> {
        use tokio::io::AsyncBufReadExt;

        // Get router to select backend for stateful session
        let Some(router) = self.router.as_ref() else {
            anyhow::bail!("Hybrid mode requires a router");
        };

        // Route this first stateful command to get a backend
        let backend_id = router.route_command(self.client_id, initial_command)?;

        // Get provider for this backend
        let Some(provider) = router.backend_provider(backend_id) else {
            anyhow::bail!("Backend {:?} not found", backend_id);
        };

        // Get a dedicated connection from the pool
        let mut pooled_conn = provider.get_pooled_connection().await?;

        info!(
            "Client {} acquired stateful connection to backend {:?}",
            self.client_addr, backend_id
        );

        // Track stateful session start
        if let Some(ref metrics) = self.metrics {
            metrics.stateful_session_started();
        }

        // Record command in metrics
        if let Some(ref metrics) = self.metrics {
            metrics.record_command(backend_id);
            common::record_user_command(&self.metrics, self.username().as_deref());
        }

        // Get buffer from pool for command execution
        let mut buffer = self.buffer_pool.acquire().await;

        // Execute the initial command that triggered the switch
        let (result, got_backend_data, cmd_bytes, resp_bytes) = self
            .execute_command_on_backend(
                &mut pooled_conn,
                initial_command,
                &mut client_write,
                backend_id,
                &mut client_to_backend_bytes.clone(),
                &mut backend_to_client_bytes.clone(),
                &mut buffer,
            )
            .await;

        // Record metrics ONCE using type-safe API (prevents double-counting)
        if let Some(ref metrics) = self.metrics {
            // Peek byte counts before consuming
            let cmd_size = cmd_bytes.peek();
            let resp_size = resp_bytes.peek();

            // Record command + per-backend + global bytes in one call
            let _ = metrics.record_command_execution(backend_id, cmd_bytes, resp_bytes);

            // Record per-user metrics
            common::record_user_bytes(
                &self.metrics,
                self.username().as_deref(),
                cmd_size,
                resp_size,
            );
        }

        if let Err(ref e) = result {
            if !got_backend_data {
                error!(
                    "Failed to execute initial command after switching to stateful mode: {}",
                    e
                );
                router.complete_command(backend_id);
                return result.map(|_| TransferMetrics {
                    client_to_backend: client_to_backend_bytes,
                    backend_to_client: backend_to_client_bytes,
                });
            } else {
                // Client disconnected while receiving data, backend is healthy
            }
        }

        // Mark this command as complete
        router.complete_command(backend_id);

        // Now enter standard stateful mode with the dedicated backend connection
        // This is the same as the standard 1:1 routing mode
        use crate::constants::buffer::COMMAND;

        let mut client_to_backend = client_to_backend_bytes.as_u64();
        let mut backend_to_client = backend_to_client_bytes.as_u64();

        // Track metrics incrementally for long-running sessions
        const METRICS_FLUSH_INTERVAL: u32 = 100;
        let mut iteration_count: u32 = 0;
        let mut last_reported_c2b = client_to_backend;
        let mut last_reported_b2c = backend_to_client;

        // Cache username to avoid Arc cloning on every flush
        let username_cache = self.username();

        // Reuse command buffer for remaining session
        let mut command = String::with_capacity(COMMAND);

        // Reuse the buffer we already got for remaining commands

        // Process remaining commands on this dedicated connection
        loop {
            command.clear();

            match client_reader.read_line(&mut command).await {
                Ok(0) => break,
                Ok(n) => {
                    client_to_backend += n as u64;

                    // Handle QUIT locally
                    match common::handle_quit_command(&command, &mut client_write).await? {
                        common::QuitStatus::Quit(bytes) => {
                            backend_to_client += bytes.as_u64();
                            break;
                        }
                        common::QuitStatus::Continue => {
                            // Not a QUIT, continue processing
                        }
                    }

                    // Execute on dedicated backend connection
                    let mut cmd_bytes = BytesTransferred::zero();
                    let mut resp_bytes = BytesTransferred::zero();
                    cmd_bytes.add(command.len());

                    // Record command in metrics
                    if let Some(ref metrics) = self.metrics {
                        metrics.record_command(backend_id);
                        common::record_user_command(&self.metrics, self.username().as_deref());
                    }

                    let (result, _got_backend_data, unrecorded_cmd_bytes, unrecorded_resp_bytes) =
                        self.execute_command_on_backend(
                            &mut pooled_conn,
                            &command,
                            &mut client_write,
                            backend_id,
                            &mut cmd_bytes,
                            &mut resp_bytes,
                            &mut buffer,
                        )
                        .await;

                    // Record metrics ONCE using type-safe API (prevents double-counting)
                    if let Some(ref metrics) = self.metrics {
                        // Peek byte counts before consuming
                        let cmd_size = unrecorded_cmd_bytes.peek();
                        let resp_size = unrecorded_resp_bytes.peek();

                        // Record command + per-backend + global bytes in one call
                        let _ = metrics.record_command_execution(
                            backend_id,
                            unrecorded_cmd_bytes,
                            unrecorded_resp_bytes,
                        );

                        // Record per-user metrics
                        common::record_user_bytes(
                            &self.metrics,
                            self.username().as_deref(),
                            cmd_size,
                            resp_size,
                        );
                    }

                    client_to_backend += cmd_bytes.as_u64();
                    backend_to_client += resp_bytes.as_u64();

                    if let Err(e) = result {
                        warn!(
                            "Error executing command in stateful mode for client {}: {}",
                            self.client_addr, e
                        );
                        break;
                    }

                    // Periodically flush metrics for long-running sessions
                    iteration_count += 1;
                    if iteration_count >= METRICS_FLUSH_INTERVAL {
                        if let Some(ref metrics) = self.metrics {
                            let delta_c2b = client_to_backend.saturating_sub(last_reported_c2b);
                            let delta_b2c = backend_to_client.saturating_sub(last_reported_b2c);

                            if delta_c2b > 0 {
                                metrics.record_client_to_backend_bytes_for(backend_id, delta_c2b);
                            }
                            if delta_b2c > 0 {
                                metrics.record_backend_to_client_bytes_for(backend_id, delta_b2c);
                            }

                            // Report user metrics incrementally as well
                            if let Some(ref username) = username_cache {
                                if delta_c2b > 0 {
                                    metrics.user_bytes_sent(Some(&**username), delta_c2b);
                                }
                                if delta_b2c > 0 {
                                    metrics.user_bytes_received(Some(&**username), delta_b2c);
                                }
                            } else {
                                // Anonymous user
                                if delta_c2b > 0 {
                                    metrics.user_bytes_sent(None, delta_c2b);
                                }
                                if delta_b2c > 0 {
                                    metrics.user_bytes_received(None, delta_b2c);
                                }
                            }

                            last_reported_c2b = client_to_backend;
                            last_reported_b2c = backend_to_client;
                        }
                        iteration_count = 0;
                    }
                }
                Err(e) => {
                    use crate::session::connection;
                    use crate::types::TransferMetrics;

                    let mut c2b = BytesTransferred::zero();
                    let mut b2c = BytesTransferred::zero();
                    c2b.add(client_to_backend as usize);
                    b2c.add(backend_to_client as usize);

                    connection::log_client_error(
                        self.client_addr,
                        self.username().as_deref(),
                        &e,
                        TransferMetrics {
                            client_to_backend: c2b,
                            backend_to_client: b2c,
                        },
                    );
                    break;
                }
            }
        }

        let mut c2b = BytesTransferred::zero();
        let mut b2c = BytesTransferred::zero();
        c2b.add_u64(client_to_backend);
        b2c.add_u64(backend_to_client);

        // Track stateful session end
        if let Some(ref metrics) = self.metrics {
            metrics.stateful_session_ended();
        }

        Ok(TransferMetrics {
            client_to_backend: c2b,
            backend_to_client: b2c,
        })
    }
}
