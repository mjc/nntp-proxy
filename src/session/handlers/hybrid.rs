//! Hybrid mode switching handler
//!
//! This module implements the transition from per-command routing to stateful
//! routing when a stateful command is encountered in hybrid mode.

use crate::session::ClientSession;
use anyhow::Result;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::tcp::{ReadHalf, WriteHalf};
use tracing::{debug, error, info, warn};

use crate::types::BytesTransferred;

impl ClientSession {
    /// Switch from per-command routing to stateful mode by acquiring a dedicated backend connection
    pub(super) async fn switch_to_stateful_mode(
        &self,
        mut client_reader: BufReader<ReadHalf<'_>>,
        mut client_write: WriteHalf<'_>,
        initial_command: &str,
        client_to_backend_bytes: BytesTransferred,
        backend_to_client_bytes: BytesTransferred,
    ) -> Result<(u64, u64)> {
        use tokio::io::AsyncBufReadExt;

        // Get router to select backend for stateful session
        let router = self
            .router
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Hybrid mode requires a router"))?;

        // Route this first stateful command to get a backend
        let backend_id = router.route_command_sync(self.client_id, initial_command)?;

        debug!(
            "Client {} switching to stateful mode, backend {:?} selected",
            self.client_addr, backend_id
        );

        // Get provider for this backend
        let provider = router
            .get_backend_provider(backend_id)
            .ok_or_else(|| anyhow::anyhow!("Backend {:?} not found", backend_id))?;

        // Get a dedicated connection from the pool
        let mut pooled_conn = provider.get_pooled_connection().await?;

        info!(
            "Client {} acquired stateful connection to backend {:?}",
            self.client_addr, backend_id
        );

        // Execute the initial command that triggered the switch
        let (result, got_backend_data) = self
            .execute_command_on_backend(
                &mut pooled_conn,
                initial_command,
                &mut client_write,
                backend_id,
                &mut client_to_backend_bytes.clone(),
                &mut backend_to_client_bytes.clone(),
            )
            .await;

        if let Err(ref e) = result {
            if !got_backend_data {
                error!(
                    "Failed to execute initial command after switching to stateful mode: {}",
                    e
                );
                router.complete_command_sync(backend_id);
                return result.map(|_| {
                    (
                        client_to_backend_bytes.as_u64(),
                        backend_to_client_bytes.as_u64(),
                    )
                });
            } else {
                // Client disconnected while receiving data, backend is healthy
                debug!(
                    "Client {} disconnected while receiving initial stateful response",
                    self.client_addr
                );
            }
        }

        // Mark this command as complete
        router.complete_command_sync(backend_id);

        debug!(
            "Client {} initial stateful command completed, entering dedicated connection mode",
            self.client_addr
        );

        // Now enter standard stateful mode with the dedicated backend connection
        // This is the same as the standard 1:1 routing mode
        use crate::constants::buffer::COMMAND;

        let mut client_to_backend = client_to_backend_bytes.as_u64();
        let mut backend_to_client = backend_to_client_bytes.as_u64();

        // Reuse command buffer for remaining session
        let mut command = String::with_capacity(COMMAND);

        // Process remaining commands on this dedicated connection
        loop {
            command.clear();

            match client_reader.read_line(&mut command).await {
                Ok(0) => {
                    debug!("Client {} disconnected in stateful mode", self.client_addr);
                    break;
                }
                Ok(n) => {
                    client_to_backend += n as u64;
                    let trimmed = command.trim();

                    debug!("Client {} stateful command: {}", self.client_addr, trimmed);

                    // Handle QUIT locally
                    if trimmed.eq_ignore_ascii_case("QUIT") {
                        use crate::protocol::CONNECTION_CLOSING;

                        if let Err(e) = client_write.write_all(CONNECTION_CLOSING).await {
                            debug!(
                                "Failed to write CONNECTION_CLOSING to client {}: {}",
                                self.client_addr, e
                            );
                        }
                        backend_to_client += CONNECTION_CLOSING.len() as u64;
                        debug!(
                            "Client {} sent QUIT in stateful mode, closing",
                            self.client_addr
                        );
                        break;
                    }

                    // Execute on dedicated backend connection
                    let mut cmd_bytes = BytesTransferred::zero();
                    let mut resp_bytes = BytesTransferred::zero();
                    cmd_bytes.add(command.len());

                    let (result, _got_backend_data) = self
                        .execute_command_on_backend(
                            &mut pooled_conn,
                            &command,
                            &mut client_write,
                            backend_id,
                            &mut cmd_bytes,
                            &mut resp_bytes,
                        )
                        .await;

                    client_to_backend += cmd_bytes.as_u64();
                    backend_to_client += resp_bytes.as_u64();

                    if let Err(e) = result {
                        warn!(
                            "Error executing command in stateful mode for client {}: {}",
                            self.client_addr, e
                        );
                        break;
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

        Ok((client_to_backend, backend_to_client))
    }
}
