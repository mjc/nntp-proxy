//! Connection utilities for stateful session handling
//!
//! Handles bidirectional forwarding and error logging for client-backend connections.

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tracing::{debug, error, warn};

use crate::constants::buffer::COMMAND;
use crate::pool::BufferPool;
use crate::types::{BytesTransferred, TransferMetrics};

/// Result of bidirectional forwarding
pub enum ForwardResult {
    /// Normal disconnection
    NormalDisconnect(TransferMetrics),
    /// Backend error - connection should be removed from pool
    BackendError(TransferMetrics),
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
    client_to_backend_bytes: BytesTransferred,
    backend_to_client_bytes: BytesTransferred,
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

    let mut buffer_b2c = buffer_pool.get_buffer().await;
    let mut command = String::with_capacity(COMMAND);

    let mut c2b = client_to_backend_bytes;
    let mut b2c = backend_to_client_bytes;

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
                                return Ok(ForwardResult::BackendError(
                                    TransferMetrics {
                                        client_to_backend: c2b,
                                        backend_to_client: b2c,
                                    }
                                ));
                            }
                            debug!("Backend write error for client {}: {}", client_addr, err);
                            break;
                        }
                        c2b.add(n);
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
                        b2c.add(n);
                    }
                    Err(e) => {
                        let err: anyhow::Error = e.into();
                        if crate::pool::is_connection_error(&err) {
                            debug!("Backend connection error for client {} ({}), removing from pool", client_addr, err);
                            return Ok(ForwardResult::BackendError(
                                TransferMetrics {
                                    client_to_backend: c2b,
                                    backend_to_client: b2c,
                                }
                            ));
                        }
                        debug!("Backend read error for client {}: {}", client_addr, err);
                        break;
                    }
                }
            }
        }
    }

    Ok(ForwardResult::NormalDisconnect(TransferMetrics {
        client_to_backend: c2b,
        backend_to_client: b2c,
    }))
}

/// Log client disconnect/error with appropriate log level and context
pub fn log_client_error(
    client_addr: std::net::SocketAddr,
    error: &std::io::Error,
    metrics: TransferMetrics,
) {
    let (c2b, b2c) = metrics.as_tuple();
    match error.kind() {
        std::io::ErrorKind::UnexpectedEof => {
            debug!(
                "Client {} closed connection (EOF) | ↑{} ↓{}",
                client_addr,
                crate::formatting::format_bytes(c2b),
                crate::formatting::format_bytes(b2c)
            );
        }
        std::io::ErrorKind::BrokenPipe => {
            debug!(
                "Client {} connection broken pipe | ↑{} ↓{}",
                client_addr,
                crate::formatting::format_bytes(c2b),
                crate::formatting::format_bytes(b2c)
            );
        }
        std::io::ErrorKind::ConnectionReset => {
            warn!(
                "Client {} connection reset | ↑{} ↓{}",
                client_addr,
                crate::formatting::format_bytes(c2b),
                crate::formatting::format_bytes(b2c)
            );
        }
        _ => {
            warn!(
                "Error reading from client {}: {} ({:?}) | ↑{} ↓{}",
                client_addr,
                error,
                error.kind(),
                crate::formatting::format_bytes(c2b),
                crate::formatting::format_bytes(b2c)
            );
        }
    }
}

/// Log routing command error with detailed context
pub fn log_routing_error(
    client_addr: std::net::SocketAddr,
    error: &std::io::Error,
    command: &str,
    metrics: TransferMetrics,
    backend_id: crate::types::BackendId,
) {
    let (c2b, b2c) = metrics.as_tuple();
    let trimmed = command.trim();
    match error.kind() {
        std::io::ErrorKind::BrokenPipe => {
            warn!(
                "Client {} disconnected during '{}' → {:?} (broken pipe) | ↑{} ↓{} | Client closed connection early",
                client_addr,
                trimmed,
                backend_id,
                crate::formatting::format_bytes(c2b),
                crate::formatting::format_bytes(b2c)
            );
        }
        std::io::ErrorKind::ConnectionReset => {
            warn!(
                "Client {} connection reset during '{}' → {:?} | ↑{} ↓{} | Network issue or client crash",
                client_addr,
                trimmed,
                backend_id,
                crate::formatting::format_bytes(c2b),
                crate::formatting::format_bytes(b2c)
            );
        }
        std::io::ErrorKind::ConnectionAborted => {
            warn!(
                "Client {} connection aborted during '{}' → {:?} | ↑{} ↓{} | Check debug logs for details",
                client_addr,
                trimmed,
                backend_id,
                crate::formatting::format_bytes(c2b),
                crate::formatting::format_bytes(b2c)
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
                crate::formatting::format_bytes(c2b),
                crate::formatting::format_bytes(b2c)
            );
        }
    }
}
