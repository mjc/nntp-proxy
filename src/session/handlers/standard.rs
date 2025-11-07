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

        debug!("Client {} session loop starting", self.client_addr);

        // Handle the initial command/response phase where we intercept auth
        loop {
            line.clear();
            let mut buffer: Vec<u8> = self.buffer_pool.get_buffer().await;

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

                            // Handle command using centralized auth/reject logic
                            let action = CommandHandler::handle_command(&line);
                            match crate::session::forwarding::handle_intercepted_command(
                                action,
                                &line,
                                &mut client_write,
                                &self.auth_handler,
                                &self.client_addr,
                            )
                            .await?
                            {
                                Some(bytes) => {
                                    // Command was intercepted (auth or reject)
                                    backend_to_client_bytes.add(bytes);
                                }
                                None => {
                                    // Forward stateless commands to backend
                                    backend_write.write_all(line.as_bytes()).await?;
                                    client_to_backend_bytes.add(line.len());
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
                            backend_to_client_bytes.add(n);
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

        Ok((
            client_to_backend_bytes.as_u64(),
            backend_to_client_bytes.as_u64(),
        ))
    }
}
