//! Stateful 1:1 routing mode handler
//!
//! Bidirectional proxy: each client gets a dedicated backend connection.

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
        let state = crate::session::state::SessionLoopState::new(self.auth_handler.is_enabled());

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
        mut state: crate::session::state::SessionLoopState,
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
                state.flush_byte_deltas(&self.metrics, backend_id, self.username().as_deref());
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
        state.flush_byte_deltas(&self.metrics, backend_id, self.username().as_deref());

        self.metrics
            .user_connection_closed(self.username().as_deref());

        Ok(state.into_metrics())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use tokio::io::{AsyncWriteExt, BufReader};

    use crate::auth::AuthHandler;
    use crate::metrics::MetricsCollector;
    use crate::pool::BufferPool;
    use crate::session::ClientSession;
    use crate::session::state::SessionLoopState;
    use crate::types::{BackendId, BufferSize, ClientAddress};

    fn test_session() -> ClientSession {
        let addr: std::net::SocketAddr = "127.0.0.1:9999".parse().unwrap();
        let buffer_pool = BufferPool::new(BufferSize::try_new(8192).unwrap(), 4);
        let auth_handler = Arc::new(AuthHandler::new(None, None).unwrap());
        let metrics = MetricsCollector::new(1);
        ClientSession::builder(
            ClientAddress::from(addr),
            buffer_pool,
            auth_handler,
            metrics,
        )
        .build()
    }

    #[tokio::test]
    async fn test_client_disconnect_returns_metrics() {
        let session = test_session();
        let backend_id = BackendId::from_index(0);
        let state = SessionLoopState::new(false); // auth disabled → skip_auth_check = true

        let (mut client_end, proxy_client_end) = tokio::io::duplex(4096);
        let (backend_end, proxy_backend_end) = tokio::io::duplex(4096);

        // Backend echo: read and respond
        let echo = tokio::spawn(async move {
            let mut backend_end = backend_end;
            let mut buf = [0u8; 4096];
            loop {
                match tokio::io::AsyncReadExt::read(&mut backend_end, &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let _ = backend_end.write_all(b"200 ok\r\n").await;
                    }
                }
            }
        });

        // Client sends a command then closes
        client_end.write_all(b"LIST\r\n").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        drop(client_end);

        let (proxy_client_read, proxy_client_write) = tokio::io::split(proxy_client_end);
        let client_reader = BufReader::new(proxy_client_read);
        let (backend_read, backend_write) = tokio::io::split(proxy_backend_end);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            session.run_stateful_proxy_loop(
                client_reader,
                proxy_client_write,
                backend_read,
                backend_write,
                state,
                backend_id,
            ),
        )
        .await
        .expect("test timed out")
        .unwrap();

        echo.abort();

        assert!(
            result.client_to_backend.as_u64() > 0,
            "Should have forwarded client bytes: {}",
            result.client_to_backend.as_u64()
        );
    }

    #[tokio::test]
    async fn test_backend_disconnect_returns_metrics() {
        let session = test_session();
        let backend_id = BackendId::from_index(0);
        let state = SessionLoopState::new(false);

        let (client_end, proxy_client_end) = tokio::io::duplex(4096);
        let (backend_end, proxy_backend_end) = tokio::io::duplex(4096);

        // Drop backend immediately → EOF
        drop(backend_end);

        let (proxy_client_read, proxy_client_write) = tokio::io::split(proxy_client_end);
        let client_reader = BufReader::new(proxy_client_read);
        let (backend_read, backend_write) = tokio::io::split(proxy_backend_end);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            session.run_stateful_proxy_loop(
                client_reader,
                proxy_client_write,
                backend_read,
                backend_write,
                state,
                backend_id,
            ),
        )
        .await
        .expect("test timed out")
        .unwrap();

        // No data was exchanged
        assert_eq!(result.client_to_backend.as_u64(), 0);
        assert_eq!(result.backend_to_client.as_u64(), 0);

        drop(client_end);
    }

    #[tokio::test]
    async fn test_auth_disabled_skips_auth_check() {
        let session = test_session(); // auth disabled by default (None, None)
        let backend_id = BackendId::from_index(0);
        let state = SessionLoopState::new(false); // auth disabled

        let (mut client_end, proxy_client_end) = tokio::io::duplex(4096);
        let (backend_end, proxy_backend_end) = tokio::io::duplex(4096);

        // Backend: just read and discard
        let echo = tokio::spawn(async move {
            let mut backend_end = backend_end;
            let mut buf = [0u8; 4096];
            loop {
                match tokio::io::AsyncReadExt::read(&mut backend_end, &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {} // Just consume, don't respond
                }
            }
        });

        // Client sends AUTHINFO command (should be forwarded directly since auth is disabled)
        client_end
            .write_all(b"AUTHINFO USER test\r\n")
            .await
            .unwrap();
        drop(client_end);

        let (proxy_client_read, proxy_client_write) = tokio::io::split(proxy_client_end);
        let client_reader = BufReader::new(proxy_client_read);
        let (backend_read, backend_write) = tokio::io::split(proxy_backend_end);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            session.run_stateful_proxy_loop(
                client_reader,
                proxy_client_write,
                backend_read,
                backend_write,
                state,
                backend_id,
            ),
        )
        .await
        .expect("test timed out")
        .unwrap();

        echo.abort();

        // When auth is disabled, the AUTHINFO command is forwarded directly to backend
        assert_eq!(
            result.client_to_backend.as_u64(),
            b"AUTHINFO USER test\r\n".len() as u64,
            "AUTHINFO should be forwarded when auth is disabled"
        );
    }

    #[tokio::test]
    async fn test_byte_accumulation() {
        let session = test_session();
        let backend_id = BackendId::from_index(0);
        let state = SessionLoopState::new(false).with_initial_bytes(100, 200);

        let (mut client_end, proxy_client_end) = tokio::io::duplex(4096);
        let (backend_end, proxy_backend_end) = tokio::io::duplex(4096);

        let echo = tokio::spawn(async move {
            let mut backend_end = backend_end;
            let mut buf = [0u8; 4096];
            loop {
                match tokio::io::AsyncReadExt::read(&mut backend_end, &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let _ = backend_end.write_all(b"200 ok\r\n").await;
                    }
                }
            }
        });

        client_end.write_all(b"LIST\r\n").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        drop(client_end);

        let (proxy_client_read, proxy_client_write) = tokio::io::split(proxy_client_end);
        let client_reader = BufReader::new(proxy_client_read);
        let (backend_read, backend_write) = tokio::io::split(proxy_backend_end);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            session.run_stateful_proxy_loop(
                client_reader,
                proxy_client_write,
                backend_read,
                backend_write,
                state,
                backend_id,
            ),
        )
        .await
        .expect("test timed out")
        .unwrap();

        echo.abort();

        // Initial bytes + forwarded bytes
        assert!(
            result.client_to_backend.as_u64() >= 100 + b"LIST\r\n".len() as u64,
            "c2b should include initial + forwarded: {}",
            result.client_to_backend.as_u64()
        );
        assert!(
            result.backend_to_client.as_u64() >= 200,
            "b2c should include at least initial bytes: {}",
            result.backend_to_client.as_u64()
        );
    }
}
