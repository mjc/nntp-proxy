//! Hybrid mode switching handler
//!
//! This module implements the transition from per-command routing to stateful
//! routing when a stateful command is encountered in hybrid mode.
//!
//! # Flow
//!
//! ```text
//! Per-Command Mode ──[stateful command]──> switch_to_stateful_mode()
//!                                                    │
//!                                          ┌─────────▼─────────┐
//!                                          │ 1. Acquire backend │
//!                                          │ 2. Send command    │
//!                                          │ 3. Hand off loop   │
//!                                          └─────────┬─────────┘
//!                                                    │
//!                                          run_stateful_proxy_loop()
//! ```

use crate::session::ClientSession;
use crate::types::TransferMetrics;
use anyhow::{Context, Result};
use tokio::io::BufReader;
use tracing::info;

/// Error context for hybrid mode operations
mod error {
    pub const ROUTER_REQUIRED: &str = "Hybrid mode requires a router";
    pub const BACKEND_NOT_FOUND: &str = "Backend not found";
}

/// The dedicated backend resources held from hybrid handoff through loop exit.
///
/// Keeping the connection, selected backend, and pending-command accounting
/// together prevents a partial handoff from accidentally returning only part
/// of its resources to the pool.
struct StatefulBackendLease {
    connection: crate::pool::ConnectionGuard,
    backend_id: crate::types::BackendId,
    _pending_command: crate::router::CommandGuard,
}

impl StatefulBackendLease {
    const fn new(
        connection: crate::pool::ConnectionGuard,
        backend_id: crate::types::BackendId,
        pending_command: crate::router::CommandGuard,
    ) -> Self {
        Self {
            connection,
            backend_id,
            _pending_command: pending_command,
        }
    }

    fn connection_mut(&mut self) -> &mut crate::pool::ConnectionGuard {
        &mut self.connection
    }

    const fn backend_id(&self) -> crate::types::BackendId {
        self.backend_id
    }

    fn finalize(
        self,
        disposition: crate::session::handlers::stateful::StatefulConnectionDisposition,
    ) {
        match disposition {
            crate::session::handlers::stateful::StatefulConnectionDisposition::RetireClient => {
                self.connection.fail_client();
            }
            crate::session::handlers::stateful::StatefulConnectionDisposition::RetireBackend => {
                self.connection.fail_backend();
            }
        }
    }
}

/// Backend resources and loop state after the triggering request is registered.
///
/// This bundle is the boundary between handoff setup and bidirectional proxying:
/// its existence means the dedicated connection has accepted the request and
/// the response ordering state already expects its reply.
struct PreparedStatefulLoop {
    backend: StatefulBackendLease,
    state: crate::session::state::SessionLoopState,
}

impl PreparedStatefulLoop {
    const fn new(
        backend: StatefulBackendLease,
        state: crate::session::state::SessionLoopState,
    ) -> Self {
        Self { backend, state }
    }

    fn into_parts(
        self,
    ) -> (
        StatefulBackendLease,
        crate::session::state::SessionLoopState,
    ) {
        (self.backend, self.state)
    }
}

/// RAII guard for stateful session metrics
///
/// Automatically calls `stateful_session_ended()` on drop.
/// Follows the same pattern as `CommandGuard` from `src/router/mod.rs`.
struct StatefulSessionGuard<'a> {
    metrics: &'a crate::metrics::MetricsCollector,
    ended: bool,
}

impl<'a> StatefulSessionGuard<'a> {
    /// Start a stateful session (calls `stateful_session_started`)
    fn start(metrics: &'a crate::metrics::MetricsCollector) -> Self {
        metrics.stateful_session_started();
        Self {
            metrics,
            ended: false,
        }
    }
}

impl Drop for StatefulSessionGuard<'_> {
    fn drop(&mut self) {
        if !self.ended {
            self.metrics.stateful_session_ended();
        }
    }
}

fn stateful_initial_client_bytes(
    carried_client_to_backend_bytes: u64,
    initial_request: &crate::command::StatefulHandoff,
) -> u64 {
    carried_client_to_backend_bytes + initial_request.request().request_wire_len().as_u64()
}

impl ClientSession {
    /// Switch from per-command routing to stateful mode
    ///
    /// This is a one-way transition that:
    /// 1. Acquires a dedicated backend connection
    /// 2. Forwards the initial stateful command
    /// 3. Delegates to the stateful proxy loop for the remainder of the session
    ///
    /// # Arguments
    /// * `client_reader` - Buffered reader for client commands
    /// * `client_write` - Write half for sending responses to client
    /// * `initial_request` - The typed stateful request that triggered the switch
    /// * `client_to_backend_bytes` - Bytes already transferred client→backend
    /// * `backend_to_client_bytes` - Bytes already transferred backend→client
    ///
    /// # Errors
    /// Returns error if router unavailable, backend unreachable, or connection fails
    pub(super) async fn switch_to_stateful_mode<R, W>(
        &mut self,
        client_reader: BufReader<R>,
        client_write: W,
        initial_request: crate::command::StatefulHandoff,
        client_to_backend_bytes: u64,
        backend_to_client_bytes: u64,
    ) -> Result<TransferMetrics, crate::session::SessionError>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let mut backend = self
            .acquire_stateful_backend()
            .await
            .context("Failed to acquire backend for stateful mode")?;

        // Start stateful session metrics tracking
        let _session_guard = StatefulSessionGuard::start(&self.metrics);

        info!(
            client = %self.client_addr,
            backend = ?backend.backend_id(),
            "Switched to stateful mode"
        );

        // Forward the triggering request (response handled by proxy loop)
        if let Err(error) = initial_request
            .request()
            .send_to(backend.connection_mut().stream_mut())
            .await
            .context("Failed to send initial request to backend")
        {
            backend.finalize(
                crate::session::handlers::stateful::StatefulConnectionDisposition::RetireBackend,
            );
            return Err(crate::session::SessionError::from(error));
        }

        // Build initial state with carried-over byte counts
        let initial_bytes =
            stateful_initial_client_bytes(client_to_backend_bytes, &initial_request);
        let mut state = crate::session::state::SessionLoopState::from_initial_bytes(
            initial_bytes,
            backend_to_client_bytes,
            self.auth_handler.is_enabled(),
        );
        state.mark_backend_request_sent(initial_request.request().kind());

        match self.mode_state.switch_to_stateful() {
            crate::session::ModeTransition::Switched => {}
            transition => {
                backend.finalize(
                    crate::session::handlers::stateful::StatefulConnectionDisposition::RetireBackend,
                );
                return Err(crate::session::SessionError::Backend(anyhow::anyhow!(
                    "stateful handoff entered from invalid mode: {transition:?}"
                )));
            }
        }

        let prepared = PreparedStatefulLoop::new(backend, state);

        let (mut backend, state) = prepared.into_parts();
        let backend_id = backend.backend_id();
        let (backend_read, backend_write) = tokio::io::split(backend.connection_mut().stream_mut());

        // Delegate to stateful loop (handles all remaining commands + responses)
        let result = self
            .run_stateful_proxy_loop(
                client_reader,
                client_write,
                backend_read,
                backend_write,
                state,
                backend_id,
            )
            .await;

        // pending_guard automatically calls complete_command via Drop

        // Metrics guard automatically ends session via Drop
        match result {
            Ok(outcome) => {
                let disposition = outcome.disposition();
                let metrics = outcome.into_metrics();
                backend.finalize(disposition);
                Ok(metrics)
            }
            Err(error) => {
                let disposition = error.disposition();
                let source = error.into_source();
                backend.finalize(disposition);
                Err(crate::session::SessionError::from(source))
            }
        }
    }

    /// Acquire a dedicated backend connection for stateful mode
    ///
    /// Routes the client to a backend, then gets a pooled connection.
    /// Returns a lease owning the connection, backend ID, and `CommandGuard`
    /// that decrements `pending_count` on drop. Creating the guard immediately after
    /// routing ensures the count is decremented even if `get_pooled_connection`
    /// fails.
    async fn acquire_stateful_backend(&self) -> Result<StatefulBackendLease> {
        let router = self
            .router
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!(error::ROUTER_REQUIRED))?;

        let backend_id = router.route(crate::router::RouteRequest::new(self.client_id))?;

        // Guard pending_count immediately — if get_pooled_connection fails,
        // the guard drops and decrements automatically
        let pending_guard =
            crate::router::BackendSelector::guard_for_routed_backend(router.clone(), backend_id);

        let provider = router
            .backend_provider(backend_id)
            .ok_or_else(|| anyhow::anyhow!("{}: {:?}", error::BACKEND_NOT_FOUND, backend_id))?;

        let provider = provider.clone();
        let conn_guard = provider.checkout_connection_guard().await?.activate();

        Ok(StatefulBackendLease::new(
            conn_guard,
            backend_id,
            pending_guard,
        ))
    }
}

#[cfg(test)]
mod tests {
    use crate::auth::AuthHandler;
    use crate::command::{AuthenticationAccess, CommandHandler};
    use crate::config::RoutingMode;
    use crate::metrics::MetricsCollector;
    use crate::pool::{BufferPool, DeadpoolConnectionProvider};
    use crate::protocol::RequestContext;
    use crate::router::BackendSelector;
    use crate::session::{ClientSession, SessionMode};
    use crate::types::{BufferSize, ClientAddress, ServerName};
    use proptest::prelude::*;
    use std::sync::Arc;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    fn handoff_fixture(
        case: usize,
        body_len: usize,
    ) -> (Vec<u8>, Vec<u8>, crate::command::StatefulHandoff) {
        let (request, response) =
            crate::session::multiline_framing::framing_pipeline_fixture(case, body_len);
        let parsed = RequestContext::parse(&request).expect("valid fixture request");
        let handoff = CommandHandler::prepare_stateful_handoff(
            parsed,
            AuthenticationAccess::Unrestricted,
            RoutingMode::Hybrid,
        )
        .expect("fixture request should trigger hybrid handoff");
        (request, response, handoff)
    }

    fn test_router(port: u16, max_connections: usize) -> Arc<BackendSelector> {
        let provider = DeadpoolConnectionProvider::builder("127.0.0.1", port)
            .max_connections(max_connections)
            .build()
            .unwrap();
        let mut router = BackendSelector::new();
        router.add_backend(
            ServerName::try_new("test-backend".to_owned()).unwrap(),
            provider,
            0,
        );
        Arc::new(router)
    }

    fn test_hybrid_session(router: Arc<BackendSelector>) -> ClientSession {
        let addr: std::net::SocketAddr = "127.0.0.1:9999".parse().unwrap();
        ClientSession::new_with_router(
            ClientAddress::from(addr),
            BufferPool::new(BufferSize::try_new(8192).unwrap(), 4),
            router,
            RoutingMode::Hybrid,
            Arc::new(AuthHandler::new(None, None).unwrap()),
            MetricsCollector::new(1),
        )
    }

    #[test]
    fn test_error_messages_are_descriptive() {
        use super::error::*;
        assert!(ROUTER_REQUIRED.contains("router"));
        assert!(BACKEND_NOT_FOUND.contains("Backend"));
    }

    #[test]
    fn stateful_initial_client_bytes_uses_typed_wire_len() {
        let request = RequestContext::parse(b"group alt.test\r\n").expect("valid request line");
        let handoff = CommandHandler::prepare_stateful_handoff(
            request,
            crate::command::AuthenticationAccess::Unrestricted,
            RoutingMode::Hybrid,
        )
        .expect("GROUP must prepare a stateful handoff");

        assert_eq!(
            super::stateful_initial_client_bytes(10, &handoff),
            10 + "group alt.test\r\n".len() as u64
        );
    }

    #[tokio::test]
    async fn failed_hybrid_handoff_does_not_commit_stateful_mode() {
        let addr: std::net::SocketAddr = "127.0.0.1:119".parse().expect("valid client address");
        let mut session = ClientSession::new_with_router(
            ClientAddress::from(addr),
            BufferPool::new(BufferSize::try_new(1024).expect("valid buffer size"), 1),
            Arc::new(BackendSelector::new()),
            RoutingMode::Hybrid,
            Arc::new(AuthHandler::new(None, None).expect("valid auth handler")),
            MetricsCollector::new(1),
        );
        let request = RequestContext::parse(b"GROUP alt.test\r\n").expect("valid request");
        let stateful_request = CommandHandler::prepare_stateful_handoff(
            request,
            crate::command::AuthenticationAccess::Unrestricted,
            RoutingMode::Hybrid,
        )
        .expect("GROUP starts a hybrid stateful handoff");
        let (client_write, client_read) = tokio::io::duplex(64);

        let result = session
            .switch_to_stateful_mode(
                BufReader::new(client_read),
                client_write,
                stateful_request,
                0,
                0,
            )
            .await;

        assert!(result.is_err());
        assert_eq!(session.mode(), SessionMode::PerCommand);
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(32))]
        #[test]
        fn prop_hybrid_handoff_preserves_backpressured_multiline_and_local_order(
            first_body_len in 128usize..4096,
            second_case in 3usize..5,
            second_body_len in 0usize..2048,
            reader_capacity in prop_oneof![1usize..40, proptest::prelude::Just(511), proptest::prelude::Just(512)],
        ) {
            let (first_request, first_response, handoff) = handoff_fixture(4, first_body_len);
            let (second_request, second_response, _) = handoff_fixture(second_case, second_body_len);
            let backend_second_request = second_request.clone();
            let expected = [
                first_response.as_slice(),
                crate::protocol::COMMAND_SYNTAX_ERROR_RESPONSE,
                second_response.as_slice(),
            ].concat();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let port = listener.local_addr().unwrap().port();
                    let router = test_router(port, 1);
                    let mut session = test_hybrid_session(router);
                    let backend = tokio::spawn(async move {
                        let (stream, _) = listener.accept().await.unwrap();
                        let (read, mut write) = tokio::io::split(stream);
                        write.write_all(b"200 test backend ready\r\n").await.unwrap();
                        let mut reader = BufReader::new(read);
                        let mut command = Vec::new();
                        reader.read_until(b'\n', &mut command).await.unwrap();
                        assert_eq!(command, b"MODE READER\r\n");
                        write.write_all(b"200 reader mode\r\n").await.unwrap();
                        command.clear();
                        reader.read_until(b'\n', &mut command).await.unwrap();
                        assert_eq!(command, b"COMPRESS DEFLATE\r\n");
                        write.write_all(b"500 compression unavailable\r\n").await.unwrap();
                        command.clear();
                        reader.read_until(b'\n', &mut command).await.unwrap();
                        assert_eq!(command, first_request);
                        write.write_all(&first_response).await.unwrap();
                        write.flush().await.unwrap();
                        command.clear();
                        reader.read_until(b'\n', &mut command).await.unwrap();
                        assert_eq!(command, backend_second_request);
                        write.write_all(&second_response).await.unwrap();
                        write.flush().await.unwrap();
                    });

                    let (mut client, proxy_client) = tokio::io::duplex(64);
                    let (proxy_read, proxy_write) = tokio::io::split(proxy_client);
                    let proxy = tokio::spawn(async move {
                        session.switch_to_stateful_mode(
                            BufReader::with_capacity(reader_capacity, proxy_read),
                            proxy_write,
                            handoff,
                            0,
                            0,
                        ).await
                    });

                    let mut actual = vec![0; 8];
                    client.read_exact(&mut actual).await.unwrap();
                    client.write_all(b"\r\n").await.unwrap();
                    client.write_all(&second_request).await.unwrap();

                    let mut remainder = vec![0; expected.len() - actual.len()];
                    client.read_exact(&mut remainder).await.unwrap();
                    actual.extend_from_slice(&remainder);
                    assert_eq!(actual, expected);

                    drop(client);
                    proxy.await.unwrap().unwrap();
                    backend.await.unwrap();
                }).await.expect("hybrid handoff backpressure case stalled");
            });
        }
    }

    #[tokio::test]
    async fn delayed_reply_after_client_disconnect_does_not_reuse_one_slot_backend() {
        let (old_request, old_response, old_handoff) = handoff_fixture(4, 128);
        let (new_request, new_response, new_handoff) = handoff_fixture(4, 192);
        let expected_second_response = new_response.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let accepted_by_server = Arc::clone(&accepted);
        let (first_seen_tx, first_seen_rx) = tokio::sync::oneshot::channel();
        let (release_old_tx, release_old_rx) = tokio::sync::oneshot::channel();
        let (old_attempted_tx, old_attempted_rx) = tokio::sync::oneshot::channel();
        let backend = tokio::spawn(async move {
            let (first, _) = listener.accept().await.unwrap();
            accepted_by_server.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let (first_read, mut first_write) = tokio::io::split(first);
            first_write
                .write_all(b"200 test backend ready\r\n")
                .await
                .unwrap();
            let mut first_reader = BufReader::new(first_read);
            let mut command = Vec::new();
            first_reader.read_until(b'\n', &mut command).await.unwrap();
            assert_eq!(command, b"MODE READER\r\n");
            first_write.write_all(b"200 reader mode\r\n").await.unwrap();
            command.clear();
            first_reader.read_until(b'\n', &mut command).await.unwrap();
            assert_eq!(command, b"COMPRESS DEFLATE\r\n");
            first_write
                .write_all(b"500 compression unavailable\r\n")
                .await
                .unwrap();
            command.clear();
            first_reader.read_until(b'\n', &mut command).await.unwrap();
            assert_eq!(command, old_request);
            let _ = first_seen_tx.send(());

            let _ = release_old_rx.await;
            let _ = first_write.write_all(&old_response).await;
            let _ = first_write.flush().await;
            let _ = old_attempted_tx.send(());

            command.clear();
            let count = match first_reader.read_until(b'\n', &mut command).await {
                Ok(count) => count,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::UnexpectedEof
                    ) =>
                {
                    0
                }
                Err(error) => panic!("unexpected first backend read error: {error}"),
            };
            if count > 0 {
                first_write.write_all(&new_response).await.unwrap();
                first_write.flush().await.unwrap();
            } else {
                let (second, _) = listener.accept().await.unwrap();
                accepted_by_server.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let (second_read, mut second_write) = tokio::io::split(second);
                second_write
                    .write_all(b"200 test backend ready\r\n")
                    .await
                    .unwrap();
                let mut second_reader = BufReader::new(second_read);
                command.clear();
                second_reader.read_until(b'\n', &mut command).await.unwrap();
                assert_eq!(command, b"MODE READER\r\n");
                second_write
                    .write_all(b"200 reader mode\r\n")
                    .await
                    .unwrap();
                command.clear();
                second_reader.read_until(b'\n', &mut command).await.unwrap();
                if command == b"COMPRESS DEFLATE\r\n" {
                    second_write
                        .write_all(b"500 compression unavailable\r\n")
                        .await
                        .unwrap();
                    command.clear();
                    second_reader.read_until(b'\n', &mut command).await.unwrap();
                }
                assert_eq!(command, new_request);
                second_write.write_all(&new_response).await.unwrap();
                second_write.flush().await.unwrap();
            }
        });

        let router = test_router(port, 1);
        let mut first_session = test_hybrid_session(Arc::clone(&router));
        let (client, proxy_client) = tokio::io::duplex(64);
        drop(client);
        let (proxy_read, proxy_write) = tokio::io::split(proxy_client);
        let first = tokio::spawn(async move {
            first_session
                .switch_to_stateful_mode(BufReader::new(proxy_read), proxy_write, old_handoff, 0, 0)
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), first_seen_rx)
            .await
            .expect("first backend request was not received")
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), first)
            .await
            .expect("disconnected hybrid session did not retire")
            .unwrap()
            .unwrap();
        release_old_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), old_attempted_rx)
            .await
            .expect("delayed old response was not attempted")
            .unwrap();

        let mut second_session = test_hybrid_session(router);
        let (mut client, proxy_client) = tokio::io::duplex(4096);
        let (proxy_read, proxy_write) = tokio::io::split(proxy_client);
        let second = tokio::spawn(async move {
            second_session
                .switch_to_stateful_mode(BufReader::new(proxy_read), proxy_write, new_handoff, 0, 0)
                .await
        });
        let mut actual = vec![0; expected_second_response.len()];
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.read_exact(&mut actual),
        )
        .await
        .expect("second hybrid reply did not arrive")
        .unwrap();
        assert_eq!(actual, expected_second_response);
        assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 2);
        drop(client);
        tokio::time::timeout(std::time::Duration::from_secs(5), second)
            .await
            .expect("second hybrid session did not stop")
            .unwrap()
            .unwrap();
        backend.await.unwrap();
    }
}
