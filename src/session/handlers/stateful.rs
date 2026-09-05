//! Stateful 1:1 routing mode handler
//!
//! Bidirectional proxy: each client gets a dedicated backend connection.

use crate::command::{AuthenticationAccess, CommandHandler, CommandPlan, RejectResponse};
use crate::protocol::{RequestContext, RequestKind, RequestRouteClass};
use crate::session::state::StatefulReadMode;
use crate::session::{ClientSession, common};
use crate::types::TransferMetrics;
use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tracing::{debug, error, warn};

use crate::constants::buffer::READER_CAPACITY;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::session) enum StatefulConnectionDisposition {
    Reusable,
    RetireClient,
    RetireBackend,
}

#[must_use]
pub(in crate::session) struct StatefulLoopResult {
    metrics: TransferMetrics,
    disposition: StatefulConnectionDisposition,
}

impl StatefulLoopResult {
    fn new(metrics: TransferMetrics, disposition: StatefulConnectionDisposition) -> Self {
        Self {
            metrics,
            disposition,
        }
    }

    pub(in crate::session) const fn disposition(&self) -> StatefulConnectionDisposition {
        self.disposition
    }

    pub(in crate::session) const fn into_metrics(self) -> TransferMetrics {
        self.metrics
    }
}

impl std::ops::Deref for StatefulLoopResult {
    type Target = TransferMetrics;

    fn deref(&self) -> &Self::Target {
        &self.metrics
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatefulSessionExit {
    ClientDisconnected,
    ClientReadError,
    BackendDisconnected,
    BackendReadError,
}

impl StatefulSessionExit {
    fn disposition(
        self,
        state: &crate::session::state::SessionLoopState,
    ) -> StatefulConnectionDisposition {
        match self {
            Self::ClientDisconnected
                if !state.has_pending_backend_replies() && !state.has_deferred_replies() =>
            {
                StatefulConnectionDisposition::Reusable
            }
            Self::ClientDisconnected | Self::ClientReadError => {
                StatefulConnectionDisposition::RetireClient
            }
            Self::BackendDisconnected | Self::BackendReadError => {
                StatefulConnectionDisposition::RetireBackend
            }
        }
    }
}

enum StatefulClientLine {
    Eof,
    Oversized,
    Invalid,
    Ready,
}

#[derive(Debug)]
struct StatefulCommandReader {
    line_buf: [u8; crate::protocol::MAX_COMMAND_LINE_OCTETS],
    line_len: usize,
    draining_oversized: bool,
    parsed_request: Option<RequestContext>,
}

impl StatefulCommandReader {
    const fn new() -> Self {
        Self {
            line_buf: [0; crate::protocol::MAX_COMMAND_LINE_OCTETS],
            line_len: 0,
            draining_oversized: false,
            parsed_request: None,
        }
    }

    fn take_request(&mut self) -> Option<RequestContext> {
        self.parsed_request.take()
    }

    async fn read_next<R>(
        &mut self,
        reader: &mut BufReader<R>,
    ) -> std::io::Result<StatefulClientLine>
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        loop {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                if self.draining_oversized {
                    self.draining_oversized = false;
                    return Ok(StatefulClientLine::Oversized);
                }
                if self.line_len == 0 {
                    return Ok(StatefulClientLine::Eof);
                }

                self.parsed_request = RequestContext::parse(&self.line_buf[..self.line_len]);
                self.line_len = 0;
                return Ok(if self.parsed_request.is_some() {
                    StatefulClientLine::Ready
                } else {
                    StatefulClientLine::Invalid
                });
            }

            let newline = memchr::memchr(b'\n', available);
            let take = newline.map_or(available.len(), |pos| pos + 1);

            if self.draining_oversized {
                reader.consume(take);
                if newline.is_some() {
                    self.draining_oversized = false;
                    return Ok(StatefulClientLine::Oversized);
                }
                continue;
            }

            if self.line_len + take > crate::protocol::MAX_COMMAND_LINE_OCTETS {
                reader.consume(take);
                self.line_len = 0;
                if newline.is_some() {
                    return Ok(StatefulClientLine::Oversized);
                }
                self.draining_oversized = true;
                continue;
            }

            self.line_buf[self.line_len..self.line_len + take].copy_from_slice(&available[..take]);
            reader.consume(take);
            self.line_len += take;

            if newline.is_some() {
                self.parsed_request = RequestContext::parse(&self.line_buf[..self.line_len]);
                self.line_len = 0;
                return Ok(if self.parsed_request.is_some() {
                    StatefulClientLine::Ready
                } else {
                    StatefulClientLine::Invalid
                });
            }
        }
    }
}

enum AuthenticatedStatefulAction {
    Forward,
    RejectAuthAlreadyAuthenticated,
    InterceptCapabilities,
    Reject(RejectResponse),
}

fn classify_authenticated_stateful_action(
    request: &RequestContext,
    auth_access: AuthenticationAccess,
) -> AuthenticatedStatefulAction {
    match CommandHandler::classify_request(
        request,
        auth_access,
        crate::config::RoutingMode::Stateful,
    ) {
        CommandPlan::InterceptAuth(_) if auth_access.auth_enabled() => {
            AuthenticatedStatefulAction::RejectAuthAlreadyAuthenticated
        }
        CommandPlan::InterceptCapabilities => AuthenticatedStatefulAction::InterceptCapabilities,
        CommandPlan::Reject(response)
            if matches!(request.kind(), RequestKind::Post | RequestKind::Ihave)
                || request.route_class() == RequestRouteClass::Reject =>
        {
            AuthenticatedStatefulAction::Reject(response)
        }
        _ => AuthenticatedStatefulAction::Forward,
    }
}

impl ClientSession {
    async fn handle_authenticated_stateful_request<W, BW>(
        &self,
        request: &RequestContext,
        client_write: &mut W,
        backend_write: &mut BW,
        state: &mut crate::session::state::SessionLoopState,
    ) -> Result<()>
    where
        W: tokio::io::AsyncWrite + Unpin,
        BW: tokio::io::AsyncWrite + Unpin,
    {
        match classify_authenticated_stateful_action(request, state.auth_access) {
            AuthenticatedStatefulAction::Forward => {
                request.write_wire_to(backend_write).await?;
                backend_write.flush().await?;
                state.add_client_to_backend(request.request_wire_len().get());
                state.mark_backend_request_sent(request.kind());
            }
            AuthenticatedStatefulAction::RejectAuthAlreadyAuthenticated => {
                use crate::protocol::AUTH_ALREADY_AUTHENTICATED;
                if state.has_pending_backend_replies() {
                    state.push_deferred_reply(AUTH_ALREADY_AUTHENTICATED);
                } else {
                    client_write.write_all(AUTH_ALREADY_AUTHENTICATED).await?;
                    client_write.flush().await?;
                    state.add_backend_to_client(AUTH_ALREADY_AUTHENTICATED.len() as u64);
                }
            }
            AuthenticatedStatefulAction::InterceptCapabilities => {
                let capabilities =
                    crate::session::backend::capabilities_without_authinfo_response();
                if state.has_pending_backend_replies() {
                    state.push_deferred_reply(capabilities);
                } else {
                    client_write.write_all(capabilities).await?;
                    client_write.flush().await?;
                    state.add_backend_to_client(capabilities.len() as u64);
                }
            }
            AuthenticatedStatefulAction::Reject(response) => {
                if state.has_pending_backend_replies() {
                    state.push_deferred_reply(response.as_bytes());
                } else {
                    client_write.write_all(response.as_bytes()).await?;
                    client_write.flush().await?;
                    state.add_backend_to_client(response.len() as u64);
                }
            }
        }

        Ok(())
    }

    async fn read_stateful_backend_bytes<BR>(
        &self,
        backend_read: &mut BR,
    ) -> Result<Option<(crate::pool::PooledBuffer, usize)>>
    where
        BR: tokio::io::AsyncRead + Unpin,
    {
        let mut buffer = self.buffer_pool.acquire();
        match buffer.read_from(backend_read).await {
            Ok(0) => Ok(None),
            Ok(n) => Ok(Some((buffer, n))),
            Err(e) => Err(e.into()),
        }
    }

    async fn write_stateful_backend_bytes<W>(
        &self,
        client_write: &mut W,
        buffer: &crate::pool::PooledBuffer,
        len: usize,
        state: &mut crate::session::state::SessionLoopState,
    ) -> Result<()>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        for write in state.client_writes_for_backend_read(&buffer[..len]) {
            client_write.write_all(write.as_ref()).await?;
            state.add_backend_to_client(write.len() as u64);
        }
        client_write.flush().await?;
        Ok(())
    }

    /// Handle stateful session - acquire backend and proxy bidirectionally
    ///
    /// # Errors
    /// Returns an error if the proxy cannot acquire a backend connection, send an
    /// initial backend-unavailable response, or proxy bytes in either direction.
    pub async fn handle_stateful_session(
        &self,
        mut client_stream: TcpStream,
        backend_id: crate::types::BackendId,
        provider: &crate::pool::DeadpoolConnectionProvider,
        server_name: &str,
    ) -> Result<TransferMetrics, crate::session::SessionError> {
        use crate::protocol::BACKEND_UNAVAILABLE;

        // Acquire backend connection
        let mut conn_guard = match provider.checkout_connection_guard().await {
            Ok(conn_guard) => {
                debug!(server = server_name, "Got pooled connection");
                conn_guard
            }
            Err(e) => {
                error!(server = server_name, client = %self.client_addr, error = %e, "Failed to get pooled connection");
                client_stream
                    .write_all(BACKEND_UNAVAILABLE)
                    .await
                    .map_err(|ie| crate::session::SessionError::from(anyhow::Error::from(ie)))?;
                return Err(crate::session::SessionError::Backend(anyhow::anyhow!(
                    "Failed to get pooled connection for '{server_name}': {e}"
                )));
            }
        };

        // Split streams
        let (client_read, client_write) = client_stream.split();
        let client_reader = BufReader::with_capacity(READER_CAPACITY, client_read);
        let (backend_read, backend_write) = tokio::io::split(conn_guard.stream_mut());
        let state = crate::session::state::SessionLoopState::new(self.auth_handler.is_enabled());

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

        match result {
            Ok(outcome) => {
                let disposition = outcome.disposition();
                let metrics = outcome.into_metrics();
                match disposition {
                    StatefulConnectionDisposition::Reusable => {
                        let _conn = conn_guard.complete_success(
                            crate::session::backend::BackendResponseComplete::stateful_session(),
                        );
                    }
                    StatefulConnectionDisposition::RetireClient => conn_guard.fail_client(),
                    StatefulConnectionDisposition::RetireBackend => conn_guard.fail_backend(),
                }
                Ok(metrics)
            }
            Err(error) => Err(crate::session::SessionError::from(error)),
        }
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
    ) -> Result<StatefulLoopResult>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
        BR: tokio::io::AsyncRead + Unpin,
        BW: tokio::io::AsyncWrite + Unpin,
    {
        let mut command_reader = StatefulCommandReader::new();

        let exit = loop {
            // Periodic metrics flush
            if state.check_and_maybe_flush_metrics() {
                state.flush_byte_deltas(&self.metrics, backend_id, self.username());
            }

            let replies = state.take_ready_deferred_replies();
            if !replies.is_empty() {
                for reply in replies {
                    client_write.write_all(reply).await?;
                    state.add_backend_to_client(reply.len() as u64);
                }
                client_write.flush().await?;
                continue;
            }

            if matches!(state.read_mode(), StatefulReadMode::DrainBackendReplies) {
                match self.read_stateful_backend_bytes(&mut backend_read).await {
                    Ok(Some((buffer, len))) => {
                        self.write_stateful_backend_bytes(
                            &mut client_write,
                            &buffer,
                            len,
                            &mut state,
                        )
                        .await?;
                        continue;
                    }
                    Ok(None) => break StatefulSessionExit::BackendDisconnected,
                    Err(e) => {
                        warn!(client = %self.client_addr, error = %e, "Backend read error");
                        break StatefulSessionExit::BackendReadError;
                    }
                }
            }

            tokio::select! {
                // Client → Backend
                result = command_reader.read_next(&mut client_reader) => {
                    match result {
                        Ok(StatefulClientLine::Eof) => {
                            break StatefulSessionExit::ClientDisconnected;
                        }
                        Ok(StatefulClientLine::Oversized) => {
                            use crate::protocol::COMMAND_TOO_LONG;
                            client_write.write_all(COMMAND_TOO_LONG).await?;
                            client_write.flush().await?;
                            state.add_backend_to_client(COMMAND_TOO_LONG.len() as u64);
                            continue;
                        }
                        Ok(StatefulClientLine::Invalid) => {
                            client_write
                                .write_all(crate::protocol::COMMAND_SYNTAX_ERROR_RESPONSE)
                                .await?;
                            client_write.flush().await?;
                            state.add_backend_to_client(
                                crate::protocol::COMMAND_SYNTAX_ERROR_RESPONSE.len() as u64,
                            );
                            continue;
                        }
                        Ok(StatefulClientLine::Ready) => {
                            let request = command_reader
                                .take_request()
                                .expect("ready command should have parsed request");
                            state.auth_access = self.authentication_access(state.auth_access);

                            if state.auth_access.can_access_backend() {
                                self.handle_authenticated_stateful_request(
                                    &request,
                                    &mut client_write,
                                    &mut backend_write,
                                    &mut state,
                                )
                                .await?;
                            } else {
                                // Auth path
                                let auth_result = common::handle_stateful_auth_check(
                                    &request,
                                    &mut client_write,
                                    &mut state.auth_username,
                                    &common::AuthCheckContext {
                                        client_id: self.client_id(),
                                        auth_handler: &self.auth_handler,
                                        auth_state: &self.auth_state,
                                        routing_mode: &crate::config::RoutingMode::Stateful,
                                        connection_stats: self.connection_stats(),
                                    },
                                    self.client_addr,
                                    |username| self.set_username(username),
                                ).await?;
                                state.apply_auth_result(&auth_result);
                            }
                        }
                        Err(e) => {
                            warn!(client = %self.client_addr, error = %e, "Client read error");
                            break StatefulSessionExit::ClientReadError;
                        }
                    }
                }

                // Backend → Client
                result = self.read_stateful_backend_bytes(&mut backend_read) => {
                    match result {
                        Ok(Some((buffer, len))) => {
                            // Complete output after select! returns so client readiness cannot
                            // cancel a partially completed write.
                            self.write_stateful_backend_bytes(
                                &mut client_write,
                                &buffer,
                                len,
                                &mut state,
                            )
                            .await?;
                        }
                        Ok(None) => break StatefulSessionExit::BackendDisconnected,
                        Err(e) => {
                            warn!(client = %self.client_addr, error = %e, "Backend read error");
                            break StatefulSessionExit::BackendReadError;
                        }
                    }
                }
            }
        };

        // Final metrics - report any remaining byte deltas
        state.flush_byte_deltas(&self.metrics, backend_id, self.username());

        let disposition = exit.disposition(&state);
        Ok(StatefulLoopResult::new(state.into_metrics(), disposition))
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::task::{Context, Poll, Waker};
    use tokio::io::AsyncWrite;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::sync::Notify;

    use crate::auth::AuthHandler;
    use crate::metrics::MetricsCollector;
    use crate::pool::BufferPool;
    use crate::protocol::RequestKind;
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

    #[test]
    fn stateful_connection_reuse_requires_a_drained_exchange() {
        let clean = SessionLoopState::new(false);
        assert_eq!(
            super::StatefulSessionExit::ClientDisconnected.disposition(&clean),
            super::StatefulConnectionDisposition::Reusable
        );

        let mut pending = SessionLoopState::new(false);
        pending.mark_backend_request_sent(RequestKind::Date);
        assert_eq!(
            super::StatefulSessionExit::ClientDisconnected.disposition(&pending),
            super::StatefulConnectionDisposition::RetireClient
        );

        assert_eq!(
            super::StatefulSessionExit::BackendDisconnected.disposition(&clean),
            super::StatefulConnectionDisposition::RetireBackend
        );
    }

    struct GateWriterState {
        bytes: Vec<u8>,
        blocked: bool,
        first_write: bool,
        waker: Option<Waker>,
    }

    struct GateWriter {
        state: Arc<Mutex<GateWriterState>>,
        partial: Arc<Notify>,
        complete: Arc<Notify>,
        expected_len: usize,
    }

    impl GateWriter {
        fn new(expected_len: usize, partial: Arc<Notify>, complete: Arc<Notify>) -> Self {
            Self {
                state: Arc::new(Mutex::new(GateWriterState {
                    bytes: Vec::new(),
                    blocked: false,
                    first_write: true,
                    waker: None,
                })),
                partial,
                complete,
                expected_len,
            }
        }

        fn release(&self) {
            let waker = {
                let mut state = self.state.lock().expect("writer mutex poisoned");
                state.blocked = false;
                state.waker.take()
            };
            if let Some(waker) = waker {
                waker.wake();
            }
        }

        fn bytes(&self) -> Vec<u8> {
            self.state
                .lock()
                .expect("writer mutex poisoned")
                .bytes
                .clone()
        }

        fn control(&self) -> Self {
            Self {
                state: Arc::clone(&self.state),
                partial: Arc::clone(&self.partial),
                complete: Arc::clone(&self.complete),
                expected_len: self.expected_len,
            }
        }
    }

    impl AsyncWrite for GateWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            let this = self.get_mut();
            let mut state = this.state.lock().expect("writer mutex poisoned");
            if state.blocked {
                state.waker = Some(cx.waker().clone());
                return Poll::Pending;
            }

            if state.first_write && bytes.len() > 1 {
                let accepted = (bytes.len() / 2).max(1).min(bytes.len() - 1);
                state.bytes.extend_from_slice(&bytes[..accepted]);
                state.first_write = false;
                state.blocked = true;
                drop(state);
                this.partial.notify_one();
                return Poll::Ready(Ok(accepted));
            }

            state.bytes.extend_from_slice(bytes);
            let complete = state.bytes.len() >= this.expected_len;
            drop(state);
            if complete {
                this.complete.notify_one();
            }
            Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn backend_output_is_not_cancelled_by_ready_client_input() {
        let session = test_session();
        let backend_id = BackendId::from_index(0);
        let state = SessionLoopState::new(false);
        let first_response = b"111 first\r\n";
        let second_response = b"111 second\r\n";
        let expected = [first_response.as_slice(), second_response.as_slice()].concat();

        let (mut client_end, proxy_client_end) = tokio::io::duplex(4096);
        let (backend_end, proxy_backend_end) = tokio::io::duplex(4096);
        client_end.write_all(b"DATE\r\n").await.unwrap();

        let backend = tokio::spawn(async move {
            let (backend_read, mut backend_write) = tokio::io::split(backend_end);
            let mut reader = BufReader::new(backend_read);
            let mut line = String::new();
            for response in [first_response.as_slice(), second_response.as_slice()] {
                line.clear();
                reader.read_line(&mut line).await.unwrap();
                backend_write.write_all(response).await.unwrap();
            }
        });

        let partial = Arc::new(Notify::new());
        let complete = Arc::new(Notify::new());
        let writer = GateWriter::new(expected.len(), Arc::clone(&partial), Arc::clone(&complete));
        let writer_control = writer.control();
        let (proxy_client_read, _) = tokio::io::split(proxy_client_end);
        let client_reader = BufReader::new(proxy_client_read);
        let (backend_read, backend_write) = tokio::io::split(proxy_backend_end);

        let proxy = tokio::spawn(async move {
            session
                .run_stateful_proxy_loop(
                    client_reader,
                    writer,
                    backend_read,
                    backend_write,
                    state,
                    backend_id,
                )
                .await
        });

        writer_control.partial.notified().await;
        client_end.write_all(b"DATE\r\n").await.unwrap();
        writer_control.release();

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            writer_control.complete.notified(),
        )
        .await
        .expect("backend output did not complete");
        assert_eq!(writer_control.bytes(), expected);

        drop(client_end);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), proxy)
            .await
            .expect("stateful proxy did not stop")
            .expect("stateful proxy task panicked")
            .expect("stateful proxy failed");
        backend.await.unwrap();
    }

    #[tokio::test]
    async fn test_client_disconnect_returns_metrics() {
        let session = test_session();
        let backend_id = BackendId::from_index(0);
        let state = SessionLoopState::new(false); // auth disabled → unrestricted access

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
    async fn test_stateful_loop_forwards_non_utf8_command_bytes() {
        let session = test_session();
        let backend_id = BackendId::from_index(0);
        let state = SessionLoopState::new(false);
        let command = b"XFOO \xff\r\n";

        let (mut client_end, proxy_client_end) = tokio::io::duplex(4096);
        let (backend_end, proxy_backend_end) = tokio::io::duplex(4096);
        let (captured_tx, captured_rx) = tokio::sync::oneshot::channel();

        let backend = tokio::spawn(async move {
            let mut backend_end = backend_end;
            let mut buf = [0u8; 64];
            let n = tokio::io::AsyncReadExt::read(&mut backend_end, &mut buf)
                .await
                .unwrap();
            let _ = captured_tx.send(buf[..n].to_vec());
        });

        client_end.write_all(command).await.unwrap();
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

        backend.await.unwrap();

        assert_eq!(captured_rx.await.unwrap(), command);
        assert_eq!(result.client_to_backend.as_u64(), command.len() as u64);
    }

    #[tokio::test]
    async fn stateful_command_reader_rejects_oversized_line_without_buffer_growth() {
        let (mut client, proxy_client) = tokio::io::duplex(2048);
        let oversized = vec![b'x'; crate::protocol::MAX_COMMAND_LINE_OCTETS + 100];

        client.write_all(&oversized).await.unwrap();
        client.write_all(b"\nDATE\r\n").await.unwrap();

        let mut reader = BufReader::new(proxy_client);
        let mut command_reader = super::StatefulCommandReader::new();

        assert!(matches!(
            command_reader.read_next(&mut reader).await.unwrap(),
            super::StatefulClientLine::Oversized
        ));

        let line = command_reader.read_next(&mut reader).await.unwrap();
        let super::StatefulClientLine::Ready = line else {
            panic!("expected DATE request after oversized line");
        };
        let request = command_reader
            .take_request()
            .expect("ready line should have request");
        assert_eq!(request.kind(), crate::protocol::RequestKind::Date);
    }

    #[tokio::test]
    async fn stateful_command_reader_drains_oversized_line_across_reads() {
        let (mut client, proxy_client) = tokio::io::duplex(64);
        let mut reader = BufReader::new(proxy_client);
        let mut command_reader = super::StatefulCommandReader::new();

        let writer = tokio::spawn(async move {
            client
                .write_all(&vec![b'x'; crate::protocol::MAX_COMMAND_LINE_OCTETS + 1])
                .await
                .unwrap();
            client.write_all(b"still oversized").await.unwrap();
            client.write_all(b"\nHELP\r\n").await.unwrap();
        });

        assert!(matches!(
            command_reader.read_next(&mut reader).await.unwrap(),
            super::StatefulClientLine::Oversized
        ));

        let line = command_reader.read_next(&mut reader).await.unwrap();
        let super::StatefulClientLine::Ready = line else {
            panic!("expected HELP request after drained oversized line");
        };
        let request = command_reader
            .take_request()
            .expect("ready line should have request");
        assert_eq!(request.kind(), crate::protocol::RequestKind::Help);
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn stateful_loop_rejects_oversized_command_then_handles_next_command() {
        let session = test_session();
        let backend_id = BackendId::from_index(0);
        let state = SessionLoopState::new(false);

        let (mut client_end, proxy_client_end) = tokio::io::duplex(4096);
        let (backend_end, proxy_backend_end) = tokio::io::duplex(4096);

        let backend = tokio::spawn(async move {
            let (backend_read, mut backend_write) = tokio::io::split(backend_end);
            let mut reader = BufReader::new(backend_read);
            let mut line = String::new();

            while reader.read_line(&mut line).await.unwrap() > 0 {
                if line.eq_ignore_ascii_case("DATE\r\n") {
                    backend_write
                        .write_all(b"111 20260526101010\r\n")
                        .await
                        .unwrap();
                    break;
                }
                line.clear();
            }
        });

        let (proxy_client_read, proxy_client_write) = tokio::io::split(proxy_client_end);
        let client_reader = BufReader::new(proxy_client_read);
        let (backend_read, backend_write) = tokio::io::split(proxy_backend_end);

        let session_task = tokio::spawn(async move {
            session
                .run_stateful_proxy_loop(
                    client_reader,
                    proxy_client_write,
                    backend_read,
                    backend_write,
                    state,
                    backend_id,
                )
                .await
                .unwrap()
        });

        let oversized = vec![b'x'; crate::protocol::MAX_COMMAND_LINE_OCTETS + 100];
        client_end.write_all(&oversized).await.unwrap();
        client_end.write_all(b"\r\nDATE\r\n").await.unwrap();

        let mut client_reader = BufReader::new(client_end);
        let mut line = String::new();

        client_reader.read_line(&mut line).await.unwrap();
        assert_eq!(line, "501 Command too long\r\n");

        line.clear();
        client_reader.read_line(&mut line).await.unwrap();
        assert_eq!(line, "111 20260526101010\r\n");

        drop(client_reader);
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), session_task)
            .await
            .expect("stateful loop timed out")
            .unwrap();

        backend.await.unwrap();
        assert_eq!(result.client_to_backend.as_u64(), b"DATE\r\n".len() as u64);
        assert!(
            result.backend_to_client.as_u64()
                >= (crate::protocol::COMMAND_TOO_LONG.len() + "111 20260526101010\r\n".len())
                    as u64
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
