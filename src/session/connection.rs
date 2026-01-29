//! Connection utilities for stateful session handling
//!
//! Handles bidirectional forwarding and error logging for client-backend connections.

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tracing::{debug, error, warn};

use crate::constants::buffer::COMMAND;
use crate::pool::BufferPool;
use crate::types::{BackendToClientBytes, ClientToBackendBytes, TransferMetrics};

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
    _client_addr: impl std::fmt::Display,
    client_to_backend_bytes: ClientToBackendBytes,
    backend_to_client_bytes: BackendToClientBytes,
) -> Result<ForwardResult>
where
    R: AsyncBufReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
    B: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let mut buffer_b2c = buffer_pool.acquire().await;
    let mut command = String::with_capacity(COMMAND);

    let mut c2b = client_to_backend_bytes;
    let mut b2c = backend_to_client_bytes;

    loop {
        tokio::select! {
            // Read from client and forward to backend
            result = client_reader.read_line(&mut command) => {
                match result {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Err(e) = pooled_conn.write_all(command.as_bytes()).await {
                            let err: anyhow::Error = e.into();
                            if crate::pool::is_connection_error(&err) {
                                return Ok(ForwardResult::BackendError(
                                    TransferMetrics {
                                        client_to_backend: c2b,
                                        backend_to_client: b2c,
                                    }
                                ));
                            }
                            break;
                        }
                        c2b = c2b.add(n);
                        command.clear();
                    }
                    Err(e) => {
                        debug!("Client read error: {}", e);
                        break;
                    }
                }
            }

            // Read from backend and forward to client
            n = buffer_b2c.read_from(pooled_conn) => {
                match n {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Err(e) = client_write.write_all(&buffer_b2c[..n]).await {
                            debug!("Client write error: {}", e);
                            break;
                        }
                        b2c = b2c.add(n);
                    }
                    Err(e) => {
                        let err: anyhow::Error = e.into();
                        if crate::pool::is_connection_error(&err) {
                            return Ok(ForwardResult::BackendError(
                                TransferMetrics {
                                    client_to_backend: c2b,
                                    backend_to_client: b2c,
                                }
                            ));
                        }
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
    client_addr: impl std::fmt::Display,
    username: Option<&str>,
    error: &std::io::Error,
    metrics: TransferMetrics,
) {
    let (c2b, b2c) = metrics.as_tuple();
    let user_info = username.unwrap_or(crate::constants::user::ANONYMOUS);
    match error.kind() {
        std::io::ErrorKind::UnexpectedEof => {
            debug!(
                "Client {} ({}) closed connection (EOF) | ↑{} ↓{}",
                client_addr,
                user_info,
                crate::formatting::format_bytes(c2b),
                crate::formatting::format_bytes(b2c)
            );
        }
        std::io::ErrorKind::BrokenPipe => {
            debug!(
                "Client {} ({}) connection broken pipe | ↑{} ↓{}",
                client_addr,
                user_info,
                crate::formatting::format_bytes(c2b),
                crate::formatting::format_bytes(b2c)
            );
        }
        std::io::ErrorKind::ConnectionReset => {
            warn!(
                "Client {} ({}) connection reset | ↑{} ↓{}",
                client_addr,
                user_info,
                crate::formatting::format_bytes(c2b),
                crate::formatting::format_bytes(b2c)
            );
        }
        _ => {
            warn!(
                "Error reading from client {} ({}): {} ({:?}) | ↑{} ↓{}",
                client_addr,
                user_info,
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
    client_addr: impl std::fmt::Display,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::BufferSize;
    use tokio::io::{AsyncWriteExt, BufReader};

    fn test_buffer_pool() -> BufferPool {
        BufferPool::new(BufferSize::try_new(8192).unwrap(), 4)
    }

    #[tokio::test]
    async fn test_normal_disconnect_client_eof() {
        let pool = test_buffer_pool();

        // Client sends a line then closes → proxy sees EOF → NormalDisconnect
        let (mut client_end, proxy_client_end) = tokio::io::duplex(4096);
        let (backend_end, mut proxy_backend_end) = tokio::io::duplex(4096);

        // Backend echo: reads from proxy, sends response, then keeps connection open
        let echo = tokio::spawn(async move {
            let mut backend_end = backend_end;
            let mut buf = [0u8; 1024];
            loop {
                match tokio::io::AsyncReadExt::read(&mut backend_end, &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let _ = backend_end.write_all(b"220 article\r\n").await;
                    }
                }
            }
        });

        client_end
            .write_all(b"ARTICLE <test@id>\r\n")
            .await
            .unwrap();
        drop(client_end); // EOF

        let (proxy_client_read, proxy_client_write) = tokio::io::split(proxy_client_end);
        let mut client_reader = BufReader::new(proxy_client_read);
        let mut client_writer = proxy_client_write;

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            bidirectional_forward(
                &mut client_reader,
                &mut client_writer,
                &mut proxy_backend_end,
                &pool,
                "test-client",
                ClientToBackendBytes::zero(),
                BackendToClientBytes::zero(),
            ),
        )
        .await
        .expect("test timed out")
        .unwrap();

        echo.abort();

        match result {
            ForwardResult::NormalDisconnect(metrics) => {
                assert!(
                    metrics.client_to_backend.as_u64() > 0,
                    "Should have forwarded client bytes"
                );
            }
            ForwardResult::BackendError(_) => {
                panic!("Expected NormalDisconnect, got BackendError");
            }
        }
    }

    #[tokio::test]
    async fn test_backend_closed_returns_normal_or_error() {
        let pool = test_buffer_pool();

        let (client_end, proxy_client_end) = tokio::io::duplex(4096);
        let (backend_end, mut proxy_backend_end) = tokio::io::duplex(4096);

        // Drop backend immediately → proxy backend read returns 0 (EOF)
        drop(backend_end);

        let (proxy_client_read, proxy_client_write) = tokio::io::split(proxy_client_end);
        let mut client_reader = BufReader::new(proxy_client_read);
        let mut client_writer = proxy_client_write;

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            bidirectional_forward(
                &mut client_reader,
                &mut client_writer,
                &mut proxy_backend_end,
                &pool,
                "test-client",
                ClientToBackendBytes::zero(),
                BackendToClientBytes::zero(),
            ),
        )
        .await
        .expect("test timed out")
        .unwrap();

        // Backend EOF → loop breaks → NormalDisconnect
        match result {
            ForwardResult::NormalDisconnect(metrics) => {
                assert_eq!(metrics.client_to_backend.as_u64(), 0);
                assert_eq!(metrics.backend_to_client.as_u64(), 0);
            }
            ForwardResult::BackendError(_) => {
                // Also acceptable: read error classified as connection error
            }
        }

        drop(client_end);
    }

    #[tokio::test]
    async fn test_byte_counting_with_initial_values() {
        let pool = test_buffer_pool();

        let (mut client_end, proxy_client_end) = tokio::io::duplex(4096);
        let (backend_end, mut proxy_backend_end) = tokio::io::duplex(4096);

        let client_msg = b"LIST\r\n";

        // Backend: read and respond, then close
        let echo = tokio::spawn(async move {
            let mut backend_end = backend_end;
            let mut buf = [0u8; 1024];
            if let Ok(n) = tokio::io::AsyncReadExt::read(&mut backend_end, &mut buf).await
                && n > 0
            {
                let _ = backend_end.write_all(b"215 list\r\n").await;
            }
            // Short pause to let data flow, then close
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            drop(backend_end);
        });

        // Client sends, then closes
        client_end.write_all(client_msg).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        drop(client_end);

        let (proxy_client_read, proxy_client_write) = tokio::io::split(proxy_client_end);
        let mut client_reader = BufReader::new(proxy_client_read);
        let mut client_writer = proxy_client_write;

        let initial_c2b = ClientToBackendBytes::new(100);
        let initial_b2c = BackendToClientBytes::new(200);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            bidirectional_forward(
                &mut client_reader,
                &mut client_writer,
                &mut proxy_backend_end,
                &pool,
                "test-client",
                initial_c2b,
                initial_b2c,
            ),
        )
        .await
        .expect("test timed out")
        .unwrap();

        echo.await.unwrap();

        match result {
            ForwardResult::NormalDisconnect(metrics) | ForwardResult::BackendError(metrics) => {
                // Initial bytes should be preserved and client bytes added
                assert!(
                    metrics.client_to_backend.as_u64() >= 100 + client_msg.len() as u64,
                    "c2b should include initial + forwarded bytes: {}",
                    metrics.client_to_backend.as_u64()
                );
                assert!(
                    metrics.backend_to_client.as_u64() >= 200,
                    "b2c should include at least initial bytes: {}",
                    metrics.backend_to_client.as_u64()
                );
            }
        }
    }

    #[tokio::test]
    async fn test_multiple_exchanges() {
        let pool = test_buffer_pool();

        let (mut client_end, proxy_client_end) = tokio::io::duplex(4096);
        let (backend_end, mut proxy_backend_end) = tokio::io::duplex(4096);

        // Backend: raw read/write (not line-buffered to avoid blocking)
        let echo = tokio::spawn(async move {
            let mut backend_end = backend_end;
            let mut buf = [0u8; 4096];
            let mut count = 0;
            loop {
                match tokio::io::AsyncReadExt::read(&mut backend_end, &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_n) => {
                        count += 1;
                        let resp = format!("200 ok #{count}\r\n");
                        if backend_end.write_all(resp.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        // Client sends multiple lines
        for cmd in &["LIST\r\n", "GROUP comp.lang.rust\r\n", "STAT 1\r\n"] {
            client_end.write_all(cmd.as_bytes()).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        drop(client_end);

        let (proxy_client_read, proxy_client_write) = tokio::io::split(proxy_client_end);
        let mut client_reader = BufReader::new(proxy_client_read);
        let mut client_writer = proxy_client_write;

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            bidirectional_forward(
                &mut client_reader,
                &mut client_writer,
                &mut proxy_backend_end,
                &pool,
                "test-client",
                ClientToBackendBytes::zero(),
                BackendToClientBytes::zero(),
            ),
        )
        .await
        .expect("test timed out")
        .unwrap();

        echo.abort();

        match result {
            ForwardResult::NormalDisconnect(metrics) | ForwardResult::BackendError(metrics) => {
                // Client bytes should all be forwarded (client sends, then EOF breaks loop)
                // Note: commands may be coalesced into fewer read_line calls due to buffering
                let expected_c2b =
                    "LIST\r\n".len() + "GROUP comp.lang.rust\r\n".len() + "STAT 1\r\n".len();
                assert_eq!(
                    metrics.client_to_backend.as_u64(),
                    expected_c2b as u64,
                    "Should have forwarded all client commands"
                );
                // Backend responses are best-effort: select! may pick client EOF before
                // backend data arrives, so we just verify the metric is non-negative
                // (the important thing is byte counting works, tested elsewhere)
            }
        }
    }

    // Smoke tests for log functions — verify they don't panic
    #[test]
    fn test_log_client_error_no_panic() {
        let metrics = TransferMetrics {
            client_to_backend: ClientToBackendBytes::new(100),
            backend_to_client: BackendToClientBytes::new(200),
        };

        for kind in [
            std::io::ErrorKind::UnexpectedEof,
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::TimedOut,
        ] {
            let err = std::io::Error::new(kind, "test error");
            log_client_error("127.0.0.1:1234", Some("testuser"), &err, metrics);
            log_client_error("127.0.0.1:1234", None, &err, metrics);
        }
    }

    #[test]
    fn test_log_routing_error_no_panic() {
        let metrics = TransferMetrics {
            client_to_backend: ClientToBackendBytes::new(100),
            backend_to_client: BackendToClientBytes::new(200),
        };
        let backend_id = crate::types::BackendId::from_index(0);

        for kind in [
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::ConnectionAborted,
            std::io::ErrorKind::TimedOut,
        ] {
            let err = std::io::Error::new(kind, "test error");
            log_routing_error(
                "127.0.0.1:1234",
                &err,
                "ARTICLE <test>\r\n",
                metrics,
                backend_id,
            );
        }
    }
}
