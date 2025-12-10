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

use crate::types::{BackendToClientBytes, ClientToBackendBytes, TransferMetrics};

impl ClientSession {
    /// Switch from per-command routing to stateful mode by acquiring a dedicated backend connection
    pub(super) async fn switch_to_stateful_mode(
        &self,
        mut client_reader: BufReader<ReadHalf<'_>>,
        mut client_write: WriteHalf<'_>,
        initial_command: &str,
        client_to_backend_bytes: ClientToBackendBytes,
        backend_to_client_bytes: BackendToClientBytes,
    ) -> Result<TransferMetrics> {
        use tokio::io::AsyncBufReadExt;

        // Update mode state (one-way transition from PerCommand to Stateful)
        self.mode_state.switch_to_stateful();

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
        self.stateful_session_started();

        // Record command in metrics
        self.record_command(backend_id);
        self.user_command();

        // Get buffer from pool for command execution
        let mut buffer = self.buffer_pool.acquire().await;

        // Track bytes for initial command
        let mut initial_cmd_bytes = ClientToBackendBytes::zero();
        let mut initial_resp_bytes = BackendToClientBytes::zero();

        // Execute the initial command that triggered the switch
        let (result, got_backend_data, cmd_bytes, resp_bytes) = self
            .execute_command_on_backend(
                &mut pooled_conn,
                initial_command,
                &mut client_write,
                backend_id,
                &mut initial_cmd_bytes,
                &mut initial_resp_bytes,
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
            self.user_bytes_sent(cmd_size);
            self.user_bytes_received(resp_size);
        }

        // Handle early error before we got backend data
        if let Err(ref e) = result
            && !got_backend_data
        {
            error!(
                "Failed to execute initial command after switching to stateful mode: {}",
                e
            );
            router.complete_command(backend_id);
            return result.map(|_| TransferMetrics {
                client_to_backend: client_to_backend_bytes,
                backend_to_client: backend_to_client_bytes,
            });
        }
        // Client disconnected while receiving data, backend is healthy - continue

        // Mark this command as complete (skipped if early return above)
        router.complete_command(backend_id);

        // Now enter standard stateful mode with the dedicated backend connection
        // This is the same as the standard 1:1 routing mode
        use crate::constants::buffer::COMMAND;

        // Add initial command bytes to running totals
        let mut client_to_backend = client_to_backend_bytes.add_u64(initial_cmd_bytes.into());

        let mut backend_to_client = backend_to_client_bytes.add_u64(initial_resp_bytes.into());

        // Track metrics incrementally for long-running sessions
        let mut iteration_count: u32 = 0;
        let mut last_reported_c2b = client_to_backend;
        let mut last_reported_b2c = backend_to_client;

        // Reuse command buffer for remaining session
        let mut command = String::with_capacity(COMMAND);

        // Reuse the buffer we already got for remaining commands

        // Process remaining commands on this dedicated connection
        loop {
            command.clear();

            match client_reader.read_line(&mut command).await {
                Ok(0) => break,
                Ok(n) => {
                    client_to_backend = client_to_backend.add(n);

                    // Handle QUIT locally
                    if let common::QuitStatus::Quit(bytes) =
                        common::handle_quit_command(&command, &mut client_write).await?
                    {
                        backend_to_client = backend_to_client.add_u64(bytes.into());
                        break;
                    }

                    // Execute on dedicated backend connection
                    let mut cmd_bytes = ClientToBackendBytes::new(command.len() as u64);
                    let mut resp_bytes = BackendToClientBytes::zero();

                    // Record command in metrics
                    self.record_command(backend_id);
                    self.user_command();

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
                        self.user_bytes_sent(cmd_size);
                        self.user_bytes_received(resp_size);
                    }

                    client_to_backend = client_to_backend.add_u64(cmd_bytes.into());
                    backend_to_client = backend_to_client.add_u64(resp_bytes.into());

                    if let Err(e) = result {
                        warn!(
                            "Error executing command in stateful mode for client {}: {}",
                            self.client_addr, e
                        );
                        break;
                    }

                    // Periodically flush metrics for long-running sessions
                    iteration_count += 1;
                    if iteration_count >= crate::constants::session::METRICS_FLUSH_INTERVAL {
                        self.flush_incremental_metrics(
                            backend_id,
                            client_to_backend,
                            backend_to_client,
                            &mut last_reported_c2b,
                            &mut last_reported_b2c,
                        );
                        iteration_count = 0;
                    }
                }
                Err(e) => {
                    use crate::session::connection;
                    use crate::types::TransferMetrics;

                    connection::log_client_error(
                        self.client_addr,
                        self.username().as_deref(),
                        &e,
                        TransferMetrics {
                            client_to_backend,
                            backend_to_client,
                        },
                    );
                    break;
                }
            }
        }

        // Track stateful session end
        self.stateful_session_ended();

        Ok(TransferMetrics {
            client_to_backend,
            backend_to_client,
        })
    }
}
