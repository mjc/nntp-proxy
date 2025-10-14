//! Connection utilities for stateful session handling
//!
//! Handles bidirectional forwarding and error logging for client-backend connections.

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tracing::{debug, error, warn};

use crate::constants::buffer::COMMAND_SIZE;
use crate::pool::BufferPool;

/// Result of bidirectional forwarding
pub enum ForwardResult {
    /// Normal disconnection
    NormalDisconnect {
        client_to_backend: u64,
        backend_to_client: u64,
    },
    /// Backend error - connection should be removed from pool
    BackendError {
        client_to_backend: u64,
        backend_to_client: u64,
    },
}

/// Bidirectional forwarding between client and backend in stateful mode
///
/// Uses tokio::select! to forward data in both directions until either side disconnects.
/// Returns ForwardResult indicating whether connection should be removed from pool.
#[allow(clippy::too_many_arguments)]
pub async fn bidirectional_forward<R, W, B>(
    client_reader: &mut R,
    client_write: &mut W,
    pooled_conn: &mut B,
    buffer_pool: &BufferPool,
    client_addr: std::net::SocketAddr,
    mut client_to_backend_bytes: u64,
    mut backend_to_client_bytes: u64,
) -> Result<ForwardResult>
where
    R: AsyncBufReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
    B: AsyncReadExt + AsyncWriteExt + Unpin,
{
    debug!(
        "Client {} entering stateful bidirectional forwarding",
        client_addr
    );

    let buffer_c2b = buffer_pool.get_buffer().await;
    let mut buffer_b2c = buffer_pool.get_buffer().await;
    let mut command = String::with_capacity(COMMAND_SIZE);

    loop {
        tokio::select! {
            // Read from client and forward to backend
            result = client_reader.read_line(&mut command) => {
                match result {
                    Ok(0) => {
                        debug!("Client {} disconnected from stateful session", client_addr);
                        break;
                    }
                    Ok(n) => {
                        if let Err(e) = pooled_conn.write_all(command.as_bytes()).await {
                            let err: anyhow::Error = e.into();
                            if crate::pool::is_connection_error(&err) {
                                debug!("Backend write error for client {} ({}), removing connection from pool", client_addr, err);
                                buffer_pool.return_buffer(buffer_c2b).await;
                                buffer_pool.return_buffer(buffer_b2c).await;
                                return Ok(ForwardResult::BackendError {
                                    client_to_backend: client_to_backend_bytes,
                                    backend_to_client: backend_to_client_bytes,
                                });
                            }
                            debug!("Backend write error for client {}: {}", client_addr, err);
                            break;
                        }
                        client_to_backend_bytes += n as u64;
                        command.clear();
                    }
                    Err(e) => {
                        debug!("Client {} read error in stateful mode: {}", client_addr, e);
                        break;
                    }
                }
            }

            // Read from backend and forward to client
            result = pooled_conn.read(&mut buffer_b2c) => {
                match result {
                    Ok(0) => {
                        debug!("Backend disconnected while in stateful mode for client {}", client_addr);
                        break;
                    }
                    Ok(n) => {
                        if let Err(e) = client_write.write_all(&buffer_b2c[..n]).await {
                            debug!("Client write error for {}: {}", client_addr, e);
                            break;
                        }
                        backend_to_client_bytes += n as u64;
                    }
                    Err(e) => {
                        let err: anyhow::Error = e.into();
                        if crate::pool::is_connection_error(&err) {
                            debug!("Backend connection error for client {} ({}), removing from pool", client_addr, err);
                            buffer_pool.return_buffer(buffer_c2b).await;
                            buffer_pool.return_buffer(buffer_b2c).await;
                            return Ok(ForwardResult::BackendError {
                                client_to_backend: client_to_backend_bytes,
                                backend_to_client: backend_to_client_bytes,
                            });
                        }
                        debug!("Backend read error for client {}: {}", client_addr, err);
                        break;
                    }
                }
            }
        }
    }

    buffer_pool.return_buffer(buffer_c2b).await;
    buffer_pool.return_buffer(buffer_b2c).await;

    Ok(ForwardResult::NormalDisconnect {
        client_to_backend: client_to_backend_bytes,
        backend_to_client: backend_to_client_bytes,
    })
}

/// Log client disconnect/error with appropriate log level and context
pub fn log_client_error(
    client_addr: std::net::SocketAddr,
    error: &std::io::Error,
    client_to_backend_bytes: u64,
    backend_to_client_bytes: u64,
) {
    match error.kind() {
        std::io::ErrorKind::UnexpectedEof => {
            debug!(
                "Client {} closed connection (EOF) | ↑{} ↓{}",
                client_addr,
                crate::formatting::format_bytes(client_to_backend_bytes),
                crate::formatting::format_bytes(backend_to_client_bytes)
            );
        }
        std::io::ErrorKind::BrokenPipe => {
            debug!(
                "Client {} connection broken pipe | ↑{} ↓{}",
                client_addr,
                crate::formatting::format_bytes(client_to_backend_bytes),
                crate::formatting::format_bytes(backend_to_client_bytes)
            );
        }
        std::io::ErrorKind::ConnectionReset => {
            warn!(
                "Client {} connection reset | ↑{} ↓{}",
                client_addr,
                crate::formatting::format_bytes(client_to_backend_bytes),
                crate::formatting::format_bytes(backend_to_client_bytes)
            );
        }
        _ => {
            warn!(
                "Error reading from client {}: {} ({:?}) | ↑{} ↓{}",
                client_addr,
                error,
                error.kind(),
                crate::formatting::format_bytes(client_to_backend_bytes),
                crate::formatting::format_bytes(backend_to_client_bytes)
            );
        }
    }
}

/// Log routing command error with detailed context
pub fn log_routing_error(
    client_addr: std::net::SocketAddr,
    error: &std::io::Error,
    command: &str,
    client_to_backend_bytes: u64,
    backend_to_client_bytes: u64,
    backend_id: crate::types::BackendId,
) {
    let trimmed = command.trim();
    match error.kind() {
        std::io::ErrorKind::BrokenPipe => {
            warn!(
                "Client {} disconnected during '{}' → {:?} (broken pipe) | ↑{} ↓{} | Client closed connection early",
                client_addr,
                trimmed,
                backend_id,
                crate::formatting::format_bytes(client_to_backend_bytes),
                crate::formatting::format_bytes(backend_to_client_bytes)
            );
        }
        std::io::ErrorKind::ConnectionReset => {
            warn!(
                "Client {} connection reset during '{}' → {:?} | ↑{} ↓{} | Network issue or client crash",
                client_addr,
                trimmed,
                backend_id,
                crate::formatting::format_bytes(client_to_backend_bytes),
                crate::formatting::format_bytes(backend_to_client_bytes)
            );
        }
        std::io::ErrorKind::ConnectionAborted => {
            warn!(
                "Client {} connection aborted during '{}' → {:?} | ↑{} ↓{} | Check debug logs for details",
                client_addr,
                trimmed,
                backend_id,
                crate::formatting::format_bytes(client_to_backend_bytes),
                crate::formatting::format_bytes(backend_to_client_bytes)
            );
        }
        _ => {
            error!(
                "Client {} error during '{}' → {:?}: {} ({:?}) | ↑{} ↓{} | Check debug logs",
                client_addr,
                trimmed,
                backend_id,
                error,
                error.kind(),
                crate::formatting::format_bytes(client_to_backend_bytes),
                crate::formatting::format_bytes(backend_to_client_bytes)
            );
        }
    }
}
