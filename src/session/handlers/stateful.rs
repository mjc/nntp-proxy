//! Stateful 1:1 routing mode handler
//!
//! Bidirectional proxy: each client gets a dedicated backend connection.

use crate::command::{AuthenticationAccess, CommandHandler, CommandPlan, RejectResponse};
use crate::protocol::{RequestContext, RequestKind, RequestRouteClass};
use crate::session::core::ReaderMessageIdRecovery;
use crate::session::multiline_framing::{ClientWrite, OrderedClientAction};
use crate::session::state::StatefulReadMode;
use crate::session::{ClientSession, common};
use crate::types::TransferMetrics;
use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tracing::debug;

use crate::constants::buffer::READER_CAPACITY;

const MAX_UPSTREAM_PIPELINE_DEPTH: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::session) enum StatefulConnectionDisposition {
    RetireClient,
    RetireBackend,
}

/// Classify a clean backend close (EOF). A close while a reply is still
/// owed failed just like a reset and is a backend error; a close between
/// replies, or at or after `QUIT`, is a normal disconnect.
fn backend_eof_error(state: &crate::session::state::SessionLoopState) -> Option<anyhow::Error> {
    state
        .backend_owes_reply()
        .then(|| anyhow::anyhow!("backend closed the connection mid-response"))
}

/// Turn a clean backend close into the session exit. A close that answers
/// the client's `QUIT` is how RFC 3977 ends a session, so it must not count
/// as a backend failure: that would apply replacement cooldown and shrink
/// the pool after every normal session end.
fn backend_eof_exit(
    state: &crate::session::state::SessionLoopState,
) -> std::result::Result<StatefulSessionExit, StatefulLoopError> {
    if let Some(error) = backend_eof_error(state) {
        return Err(StatefulLoopError::backend(error));
    }
    if state.backend_closed_by_quit() {
        return Ok(StatefulSessionExit::QuitCompleted);
    }
    Ok(StatefulSessionExit::BackendDisconnected)
}

#[must_use]
pub(in crate::session) struct StatefulLoopResult {
    metrics: TransferMetrics,
    disposition: StatefulConnectionDisposition,
}

#[derive(Debug)]
pub(in crate::session) struct StatefulLoopError {
    source: anyhow::Error,
    disposition: StatefulConnectionDisposition,
}

impl StatefulLoopError {
    fn client(source: anyhow::Error) -> Self {
        Self {
            source,
            disposition: StatefulConnectionDisposition::RetireClient,
        }
    }

    fn backend(source: anyhow::Error) -> Self {
        Self {
            source,
            disposition: StatefulConnectionDisposition::RetireBackend,
        }
    }

    pub(in crate::session) const fn disposition(&self) -> StatefulConnectionDisposition {
        self.disposition
    }

    /// The session error, keeping the side the failure came from.
    ///
    /// The generic `anyhow` conversion decides by error kind: a reset or
    /// broken pipe becomes a client disconnect, anything else a backend
    /// error. In a stateful loop the side is known instead. A backend-side
    /// failure (reset, clean close mid-response, any I/O error) is a backend
    /// error; a client-side failure (reset, timeout, invalid data) is the
    /// client's and is never blamed on the backend.
    pub(in crate::session) fn into_session_error(self) -> crate::session::SessionError {
        use crate::session::SessionError;
        match self.disposition {
            StatefulConnectionDisposition::RetireBackend => SessionError::Backend(self.source),
            StatefulConnectionDisposition::RetireClient => SessionError::client_side(self.source),
        }
    }
}

impl std::fmt::Display for StatefulLoopError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(formatter)
    }
}

impl std::error::Error for StatefulLoopError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.source()
    }
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
    /// The backend closed after answering the client's `QUIT`.
    QuitCompleted,
    BackendDisconnected,
}

impl StatefulSessionExit {
    fn disposition(self) -> StatefulConnectionDisposition {
        match self {
            Self::ClientDisconnected | Self::ClientReadError | Self::QuitCompleted => {
                StatefulConnectionDisposition::RetireClient
            }
            // A backend read error is not an exit: it ends the loop as a
            // backend `StatefulLoopError`.
            Self::BackendDisconnected => StatefulConnectionDisposition::RetireBackend,
        }
    }
}

/// The status-line deadline of the backend reply at the front of the queue.
///
/// A pinned session forwards bytes as they come, so nothing else notices a
/// backend that accepted a command and went silent: the client would hang
/// until TCP gave up. The wait starts when the reply reaches the front, which
/// for an idle session is when its command was written; a pipelined reply
/// behind a long body starts its wait when that body ends. Body streaming is
/// never bounded.
#[derive(Debug, Default)]
struct StatusLineWait(
    Option<(
        crate::session::multiline_framing::PendingReplyId,
        tokio::time::Instant,
    )>,
);

impl StatusLineWait {
    /// The deadline for `awaiting`, starting it on the first wait for a new
    /// front reply. One clock read per reply that has to wait for its status.
    fn deadline_for(
        &mut self,
        awaiting: Option<crate::session::multiline_framing::PendingReplyId>,
    ) -> Option<tokio::time::Instant> {
        let Some(reply) = awaiting else {
            self.0 = None;
            return None;
        };
        match self.0 {
            Some((armed, deadline)) if armed == reply => Some(deadline),
            _ => {
                let deadline = crate::session::backend::StatusLineDeadline::from_now().instant();
                self.0 = Some((reply, deadline));
                Some(deadline)
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

/// The four stream halves of a stateful session, borrowed for one loop run.
struct StatefulLoopIo<'a, R, W, BR, BW> {
    client_reader: &'a mut BufReader<R>,
    client_write: &'a mut W,
    backend_read: &'a mut BR,
    backend_write: &'a mut BW,
}

impl ClientSession {
    async fn handle_authenticated_stateful_request<W, BW>(
        &self,
        request: RequestContext,
        client_write: &mut W,
        backend_write: &mut BW,
        state: &mut crate::session::state::SessionLoopState,
    ) -> std::result::Result<(), StatefulLoopError>
    where
        W: tokio::io::AsyncWrite + Unpin,
        BW: tokio::io::AsyncWrite + Unpin,
    {
        let action = classify_authenticated_stateful_action(&request, state.auth_access);
        if !matches!(action, AuthenticatedStatefulAction::Forward) {
            // Answered here, so these bytes never reach the pinned backend.
            state.add_local_from_client(request.request_wire_len().get());
        }
        match action {
            AuthenticatedStatefulAction::Forward => {
                request
                    .write_wire_to(backend_write)
                    .await
                    .map_err(|error| StatefulLoopError::backend(error.into()))?;
                state.add_client_to_backend(request.request_wire_len().get());
                if self.reader_message_id_recovery == ReaderMessageIdRecovery::RoutedAfterPinnedMiss
                    && CommandHandler::article_lookup_request(&request).is_some()
                {
                    state.mark_recoverable_backend_request_sent(request);
                } else {
                    state.mark_backend_request_sent(request.kind());
                }
            }
            AuthenticatedStatefulAction::RejectAuthAlreadyAuthenticated => {
                use crate::protocol::AUTH_ALREADY_AUTHENTICATED;
                if state.has_pending_backend_replies() {
                    state.push_deferred_reply(AUTH_ALREADY_AUTHENTICATED);
                } else {
                    client_write
                        .write_all(AUTH_ALREADY_AUTHENTICATED)
                        .await
                        .map_err(|error| StatefulLoopError::client(error.into()))?;
                    client_write
                        .flush()
                        .await
                        .map_err(|error| StatefulLoopError::client(error.into()))?;
                    state.add_local_to_client(AUTH_ALREADY_AUTHENTICATED.len() as u64);
                }
            }
            AuthenticatedStatefulAction::InterceptCapabilities => {
                let capabilities =
                    crate::session::backend::capabilities_without_authinfo_response();
                if state.has_pending_backend_replies() {
                    state.push_deferred_reply(capabilities);
                } else {
                    client_write
                        .write_all(capabilities)
                        .await
                        .map_err(|error| StatefulLoopError::client(error.into()))?;
                    client_write
                        .flush()
                        .await
                        .map_err(|error| StatefulLoopError::client(error.into()))?;
                    state.add_local_to_client(capabilities.len() as u64);
                }
            }
            AuthenticatedStatefulAction::Reject(response) => {
                if state.has_pending_backend_replies() {
                    state.push_deferred_reply(response.as_bytes());
                } else {
                    client_write
                        .write_all(response.as_bytes())
                        .await
                        .map_err(|error| StatefulLoopError::client(error.into()))?;
                    client_write
                        .flush()
                        .await
                        .map_err(|error| StatefulLoopError::client(error.into()))?;
                    state.add_local_to_client(response.len() as u64);
                }
            }
        }

        Ok(())
    }

    /// Read the next backend bytes, giving up at `status_deadline` when the
    /// next reply's status line is still owed.
    async fn read_stateful_backend_bytes<BR>(
        &self,
        backend_read: &mut BR,
        status_deadline: Option<tokio::time::Instant>,
    ) -> Result<Option<(crate::pool::PooledBuffer, usize)>>
    where
        BR: tokio::io::AsyncRead + Unpin,
    {
        let mut buffer = self.buffer_pool.acquire();
        let read = match status_deadline {
            Some(deadline) => tokio::time::timeout_at(deadline, buffer.read_from(backend_read))
                .await
                .map_err(|_| {
                    anyhow::anyhow!(
                        "Backend sent no complete status line within {:?}",
                        crate::constants::timeout::BACKEND_READ
                    )
                })?,
            None => buffer.read_from(backend_read).await,
        };
        match read {
            Ok(0) => Ok(None),
            Ok(n) => Ok(Some((buffer, n))),
            Err(e) => Err(e.into()),
        }
    }

    /// Charge the pinned backend for bytes it sent that the client never got
    /// because a client write failed: the provider still delivered them. The
    /// session's client and user totals count only what was written.
    fn record_unsent_backend_bytes(&self, backend_id: crate::types::BackendId, bytes: u64) {
        if bytes > 0 {
            self.metrics
                .record_backend_to_client_bytes_for(backend_id, bytes);
        }
    }

    async fn write_stateful_backend_bytes<W>(
        &self,
        client_write: &mut W,
        buffer: &crate::pool::PooledBuffer,
        len: usize,
        state: &mut crate::session::state::SessionLoopState,
        backend_id: crate::types::BackendId,
    ) -> std::result::Result<(), StatefulLoopError>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        match self.reader_message_id_recovery {
            ReaderMessageIdRecovery::PinnedOnly => {
                let writes = state.client_writes_for_backend_read(&buffer[..len]);
                // Counted before writing so a failed client write still
                // records what the backend delivered.
                self.metrics
                    .record_response_tally(backend_id, state.take_reply_outcomes());
                let mut writes = writes.into_iter();
                while let Some(write) = writes.next() {
                    if let Err(error) = client_write.write_all(&write).await {
                        let unsent = std::iter::once(write)
                            .chain(writes)
                            .map(|write| match write {
                                ClientWrite::Backend(bytes) => bytes.len() as u64,
                                ClientWrite::Local(_) => 0,
                            })
                            .sum();
                        self.record_unsent_backend_bytes(backend_id, unsent);
                        return Err(StatefulLoopError::client(error.into()));
                    }
                    match write {
                        ClientWrite::Backend(bytes) => {
                            state.add_backend_to_client(bytes.len() as u64);
                        }
                        ClientWrite::Local(reply) => state.add_local_to_client(reply.len() as u64),
                    }
                }
            }
            ReaderMessageIdRecovery::RoutedAfterPinnedMiss => {
                let actions = state.client_actions_for_backend_read(&buffer[..len]);
                self.metrics
                    .record_response_tally(backend_id, state.take_reply_outcomes());
                let mut actions = actions.into_iter();
                while let Some(action) = actions.next() {
                    // Backend bytes of the failed action, if the client never
                    // got them; a recovery already charged its withheld reply.
                    let (result, failed_unsent) = match action {
                        OrderedClientAction::Write(write) => {
                            let result = client_write.write_all(write.as_ref()).await;
                            if result.is_ok() {
                                state.add_backend_to_client(write.len() as u64);
                            }
                            (result.map_err(anyhow::Error::from), write.len() as u64)
                        }
                        OrderedClientAction::WriteLocal(reply) => {
                            let result = client_write.write_all(reply).await;
                            if result.is_ok() {
                                state.add_local_to_client(reply.len() as u64);
                            }
                            (result.map_err(anyhow::Error::from), 0)
                        }
                        OrderedClientAction::Recover {
                            mut request,
                            reply,
                            withheld_len,
                        } => {
                            // Report pinned-backend bytes before the recovered
                            // transfer advances the session watermarks.
                            state.flush_byte_deltas(&self.metrics, backend_id, self.username());
                            // The withheld reply never reaches the client, so
                            // it is pinned-backend traffic only.
                            self.metrics
                                .record_backend_to_client_bytes_for(backend_id, withheld_len);
                            let result = self
                                .recover_reader_message_id_miss(
                                    &mut request,
                                    client_write,
                                    backend_id,
                                    reply,
                                )
                                .await
                                .map(|transfer| state.add_reported_transfer(transfer))
                                .map_err(anyhow::Error::from);
                            (result, 0)
                        }
                    };
                    if let Err(error) = result {
                        let unsent = failed_unsent
                            + actions
                                .map(|action| match action {
                                    OrderedClientAction::Write(write) => write.len() as u64,
                                    OrderedClientAction::Recover { withheld_len, .. } => {
                                        withheld_len
                                    }
                                    OrderedClientAction::WriteLocal(_) => 0,
                                })
                                .sum::<u64>();
                        self.record_unsent_backend_bytes(backend_id, unsent);
                        return Err(StatefulLoopError::client(error));
                    }
                }
            }
        }
        client_write
            .flush()
            .await
            .map_err(|error| StatefulLoopError::client(error.into()))?;
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
                conn_guard.activate()
            }
            Err(e) => {
                if matches!(
                    e,
                    crate::connection_error::ConnectionError::LoginBackoff { .. }
                ) {
                    // Expected on every checkout until the backoff window closes.
                    debug!(server = server_name, client = %self.client_addr, error = %e, "Backend in login backoff");
                } else {
                    // The pool already logged why the checkout failed.
                    debug!(server = server_name, client = %self.client_addr, error = %e, "Failed to get pooled connection");
                }
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
                    StatefulConnectionDisposition::RetireClient => conn_guard.fail_client(),
                    StatefulConnectionDisposition::RetireBackend => conn_guard.fail_backend(),
                }
                Ok(metrics)
            }
            Err(error) => {
                match error.disposition() {
                    StatefulConnectionDisposition::RetireClient => conn_guard.fail_client(),
                    StatefulConnectionDisposition::RetireBackend => conn_guard.fail_backend(),
                }
                Err(error.into_session_error())
            }
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
    ) -> std::result::Result<StatefulLoopResult, StatefulLoopError>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
        BR: tokio::io::AsyncRead + Unpin,
        BW: tokio::io::AsyncWrite + Unpin,
    {
        // The loop lives in its own function so its `?` exits still reach
        // the final byte flush below: an erroring session must report the
        // same totals as one that ends cleanly.
        let exit = self
            .stateful_loop_until_exit(
                StatefulLoopIo {
                    client_reader: &mut client_reader,
                    client_write: &mut client_write,
                    backend_read: &mut backend_read,
                    backend_write: &mut backend_write,
                },
                &mut state,
                backend_id,
            )
            .await;

        // Final metrics - report any remaining byte deltas
        state.flush_byte_deltas(&self.metrics, backend_id, self.username());
        let (received, sent) = state.unrecorded_client_bytes();
        self.metrics.record_client_session_bytes(received, sent);
        let exit = exit?;

        let disposition = exit.disposition();
        Ok(StatefulLoopResult::new(state.into_metrics(), disposition))
    }

    /// Proxy commands and replies until the session ends.
    async fn stateful_loop_until_exit<R, W, BR, BW>(
        &self,
        io: StatefulLoopIo<'_, R, W, BR, BW>,
        state: &mut crate::session::state::SessionLoopState,
        backend_id: crate::types::BackendId,
    ) -> std::result::Result<StatefulSessionExit, StatefulLoopError>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
        BR: tokio::io::AsyncRead + Unpin,
        BW: tokio::io::AsyncWrite + Unpin,
    {
        let StatefulLoopIo {
            client_reader,
            client_write,
            backend_read,
            backend_write,
        } = io;
        let mut command_reader = StatefulCommandReader::new();
        let mut status_wait = StatusLineWait::default();
        // Keep a small upstream window open while the client is producing
        // commands. Replies are still consumed and ordered by the framer below;
        // this counter only controls when buffered request bytes are flushed.
        let mut pending_backend_writes = 0usize;

        loop {
            // Periodic metrics flush
            if state.check_and_maybe_flush_metrics() {
                state.flush_byte_deltas(&self.metrics, backend_id, self.username());
            }

            let replies = state.take_ready_deferred_replies();
            if !replies.is_empty() {
                for reply in replies {
                    client_write
                        .write_all(reply)
                        .await
                        .map_err(|error| StatefulLoopError::client(error.into()))?;
                    state.add_local_to_client(reply.len() as u64);
                }
                client_write
                    .flush()
                    .await
                    .map_err(|error| StatefulLoopError::client(error.into()))?;
                continue;
            }

            if matches!(state.read_mode(), StatefulReadMode::DrainBackendReplies) {
                if pending_backend_writes != 0 {
                    backend_write
                        .flush()
                        .await
                        .map_err(|error| StatefulLoopError::backend(error.into()))?;
                    pending_backend_writes = 0;
                }
                let status_deadline =
                    status_wait.deadline_for(state.backend_reply_awaiting_status());
                match self
                    .read_stateful_backend_bytes(backend_read, status_deadline)
                    .await
                {
                    Ok(Some((buffer, len))) => {
                        self.write_stateful_backend_bytes(
                            client_write,
                            &buffer,
                            len,
                            state,
                            backend_id,
                        )
                        .await?;
                        continue;
                    }
                    Ok(None) => {
                        return backend_eof_exit(state);
                    }
                    Err(e) => {
                        debug!(client = %self.client_addr, error = %e, "Backend read error");
                        // A backend reset mid-transfer is a backend error, not a
                        // client disconnect; the caller still flushes metrics.
                        return Err(StatefulLoopError::backend(e));
                    }
                }
            }

            // Flush the buffered window before waiting for more client bytes.
            // A partial next command must not hold already complete requests.
            if pending_backend_writes != 0 && !client_reader.buffer().contains(&b'\n') {
                backend_write
                    .flush()
                    .await
                    .map_err(|error| StatefulLoopError::backend(error.into()))?;
                pending_backend_writes = 0;
            }

            let status_deadline = status_wait.deadline_for(state.backend_reply_awaiting_status());
            tokio::select! {
                // Client → Backend
                result = command_reader.read_next(client_reader) => {
                    match result {
                        Ok(StatefulClientLine::Eof) => {
                            return Ok(StatefulSessionExit::ClientDisconnected);
                        }
                        Ok(line @ (StatefulClientLine::Oversized | StatefulClientLine::Invalid)) => {
                            self.metrics.record_client_rejected_line();
                            let reply = match line {
                                StatefulClientLine::Oversized => crate::protocol::COMMAND_TOO_LONG,
                                _ => crate::protocol::COMMAND_SYNTAX_ERROR_RESPONSE,
                            };
                            state.push_deferred_reply(reply);
                            continue;
                        }
                        Ok(StatefulClientLine::Ready) => {
                            let request = command_reader
                                .take_request()
                                .expect("ready command should have parsed request");
                            self.metrics.record_client_command(request.kind());
                            state.auth_access = self.authentication_access(state.auth_access);

                            if state.auth_access.can_access_backend() {
                                self.handle_authenticated_stateful_request(
                                    request,
                                    client_write,
                                    backend_write,
                                    state,
                                )
                                .await?;
                                pending_backend_writes =
                                    pending_backend_writes.saturating_add(1);
                                if pending_backend_writes >= MAX_UPSTREAM_PIPELINE_DEPTH {
                                    backend_write
                                        .flush()
                                        .await
                                        .map_err(|error| StatefulLoopError::backend(error.into()))?;
                                    pending_backend_writes = 0;
                                }
                            } else {
                                // Auth path
                                let auth_result = common::handle_stateful_auth_check(
                                    &request,
                                    client_write,
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
                                )
                                .await
                                .map_err(StatefulLoopError::client)?;
                                state.add_local_from_client(request.request_wire_len().get());
                                state.apply_auth_result(&auth_result);
                            }
                        }
                        Err(e) => {
                            debug!(client = %self.client_addr, error = %e, "Client read error");
                            return Ok(StatefulSessionExit::ClientReadError);
                        }
                    }
                }

                // Backend → Client
                result = self.read_stateful_backend_bytes(backend_read, status_deadline) => {
                    match result {
                        Ok(Some((buffer, len))) => {
                            // Complete output after select! returns so client readiness cannot
                            // cancel a partially completed write.
                            self.write_stateful_backend_bytes(
                                client_write,
                                &buffer,
                                len,
                                state,
                                backend_id,
                            )
                            .await?;
                        }
                        Ok(None) => {
                            return backend_eof_exit(state);
                        }
                        Err(e) => {
                            debug!(client = %self.client_addr, error = %e, "Backend read error");
                            // A backend reset mid-transfer is a backend error, not a
                            // client disconnect; the caller still flushes metrics.
                            return Err(StatefulLoopError::backend(e));
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::task::{Context, Poll, Waker};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::sync::Notify;

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

    #[derive(Clone, Debug)]
    enum PipelineCommand {
        Date,
        SyntaxError,
        Oversized,
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn prop_compressed_stateful_pipeline_orders_mixed_local_errors(
            extra in prop::collection::vec(prop_oneof![
                Just(PipelineCommand::Date), Just(PipelineCommand::SyntaxError), Just(PipelineCommand::Oversized),
            ], 0..8),
            oversized_len in 511usize..800,
            reader_capacity in prop_oneof![1usize..32, Just(511), Just(512)],
        ) {
            let mut commands = vec![PipelineCommand::Date, PipelineCommand::SyntaxError, PipelineCommand::Oversized];
            commands.extend(extra);
            commands.push(PipelineCommand::Date);
            let mut wire = Vec::new();
            let mut expected = Vec::new();
            let mut backend_count = 0;
            for command in commands {
                match command {
                    PipelineCommand::Date => {
                        wire.extend_from_slice(b"DATE\r\n");
                        expected.extend_from_slice(format!("111 {backend_count:014}\r\n").as_bytes());
                        backend_count += 1;
                    }
                    PipelineCommand::SyntaxError => {
                        wire.extend_from_slice(b"\r\n");
                        expected.extend_from_slice(crate::protocol::COMMAND_SYNTAX_ERROR_RESPONSE);
                    }
                    PipelineCommand::Oversized => {
                        wire.extend(std::iter::repeat_n(b'X', oversized_len));
                        wire.extend_from_slice(b"\r\n");
                        expected.extend_from_slice(crate::protocol::COMMAND_TOO_LONG);
                    }
                }
            }
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let actual = runtime.block_on(async {
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    use tokio::io::AsyncReadExt;
                    let session = test_session();
                    let (mut client, proxy_client) = tokio::io::duplex(16_384);
                    let (backend, proxy_backend) = tokio::io::duplex(16_384);
                    let mut backend = crate::compression::DecompressStream::new(backend);
                    let proxy_backend = crate::compression::DecompressStream::new(proxy_backend);
                    let (client_read, client_write) = tokio::io::split(proxy_client);
                    let (backend_read, backend_write) = tokio::io::split(proxy_backend);
                    let proxy = tokio::spawn(async move {
                        session.run_stateful_proxy_loop(
                            BufReader::with_capacity(reader_capacity, client_read), client_write,
                            backend_read, backend_write, SessionLoopState::new(false), BackendId::from_index(0),
                        ).await
                    });
                    let client_exchange = async {
                        client.write_all(&wire).await.unwrap();
                        let mut actual = vec![0; expected.len()];
                        client.read_exact(&mut actual).await.unwrap();
                        drop(client);
                        actual
                    };
                    let backend_exchange = async {
                        for index in 0..backend_count {
                            let mut command = [0; 6];
                            backend.read_exact(&mut command).await.unwrap();
                            assert_eq!(&command, b"DATE\r\n");
                            backend.write_all(format!("111 {index:014}\r\n").as_bytes()).await.unwrap();
                            backend.flush().await.unwrap();
                        }
                    };
                    let (actual, ()) = tokio::join!(client_exchange, backend_exchange);
                    let result = proxy.await.unwrap().unwrap();
                    assert_eq!(result.disposition(), super::StatefulConnectionDisposition::RetireClient);
                    actual
                }).await.expect("mixed compressed pipeline stalled")
            });
            prop_assert_eq!(actual, expected);
        }

        #[test]
        fn prop_stateful_multiline_output_survives_client_readiness_during_backpressure(
            first_case in 0usize..5,
            first_body_len in 0usize..4096,
            second_case in 0usize..5,
            second_body_len in 0usize..4096,
            accepted_prefix in 1usize..64,
            backend_read_cap in 1usize..65,
            reader_capacity in prop_oneof![1usize..40, Just(511), Just(512)],
        ) {
            let (first_request, first_response) =
                crate::session::multiline_framing::framing_pipeline_fixture(first_case, first_body_len);
            let (second_request, second_response) =
                crate::session::multiline_framing::framing_pipeline_fixture(second_case, second_body_len);
            let expected = [
                first_response.as_slice(),
                crate::protocol::COMMAND_SYNTAX_ERROR_RESPONSE,
                second_response.as_slice(),
            ].concat();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            for compressed_backend in [false, true] {
                let backend_first_request = first_request.clone();
                let backend_second_request = second_request.clone();
                let backend_first_response = first_response.clone();
                let backend_second_response = second_response.clone();
                let (keep_backend_open_tx, keep_backend_open_rx) =
                    tokio::sync::oneshot::channel();
                runtime.block_on(async {
                    tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    let session = test_session();
                    let (mut client, proxy_client) = tokio::io::duplex(16_384);
                    let (backend, proxy_backend) = tokio::io::duplex(16_384);
                    let (backend, proxy_backend): (
                        Box<dyn crate::stream::AsyncStream>,
                        Box<dyn crate::stream::AsyncStream>,
                    ) = if compressed_backend {
                        (
                            Box::new(crate::compression::DecompressStream::new(backend)),
                            Box::new(crate::compression::DecompressStream::new(proxy_backend)),
                        )
                    } else {
                        (Box::new(backend), Box::new(proxy_backend))
                    };
                    let backend_task = tokio::spawn(async move {
                        let (backend_read, mut backend_write) = tokio::io::split(backend);
                        let mut reader = BufReader::new(backend_read);
                        let mut command = Vec::new();
                        reader.read_until(b'\n', &mut command).await.unwrap();
                        assert_eq!(command, backend_first_request);
                        backend_write
                            .write_all(&backend_first_response)
                            .await
                            .unwrap();
                        backend_write.flush().await.unwrap();
                        command.clear();
                        reader.read_until(b'\n', &mut command).await.unwrap();
                        assert_eq!(command, backend_second_request);
                        backend_write
                            .write_all(&backend_second_response)
                            .await
                            .unwrap();
                        backend_write.flush().await.unwrap();
                        let _ = keep_backend_open_rx.await;
                    });

                    let partial = Arc::new(Notify::new());
                    let complete = Arc::new(Notify::new());
                    let writer = GateWriter::with_first_accept_limit(
                        expected.len(),
                        Arc::clone(&partial),
                        Arc::clone(&complete),
                        Some(accepted_prefix),
                    );
                    let writer_control = writer.control();
                    let (proxy_client_read, _) = tokio::io::split(proxy_client);
                    let (backend_read, backend_write) = tokio::io::split(proxy_backend);
                    let backend_read_cap =
                        backend_read_cap.min((first_response.len() / 2).max(1));
                    let backend_read = ReadCap::new(backend_read, backend_read_cap);
                    let proxy = tokio::spawn(async move {
                        session.run_stateful_proxy_loop(
                            BufReader::with_capacity(reader_capacity, proxy_client_read),
                            writer,
                            backend_read,
                            backend_write,
                            SessionLoopState::new(false),
                            BackendId::from_index(0),
                        ).await
                    });

                    client.write_all(&first_request).await.unwrap();
                    tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        writer_control.partial.notified(),
                    ).await.expect("first backend response did not reach gated writer");

                    // These bytes become readable while the first response write
                    // is suspended; completing that response must retain priority.
                    client.write_all(b"\r\n").await.unwrap();
                    client.write_all(&second_request).await.unwrap();
                    writer_control.release();

                    tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        writer_control.complete.notified(),
                    ).await.expect("ordered multiline output did not finish");
                    assert_eq!(writer_control.bytes(), expected);
                    drop(client);
                    let result = tokio::time::timeout(std::time::Duration::from_secs(5), proxy)
                        .await.expect("stateful proxy did not stop")
                        .expect("stateful proxy task panicked")
                        .expect("stateful proxy failed");
                    assert_eq!(result.disposition(), super::StatefulConnectionDisposition::RetireClient);
                    let _ = keep_backend_open_tx.send(());
                    backend_task.await.unwrap();
                    })
                    .await
                    .expect("gated multiline property stalled");
                });
            }
        }
    }

    #[test]
    fn loop_errors_keep_the_side_they_came_from() {
        use crate::session::SessionError;
        let reset =
            || anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::ConnectionReset));
        assert!(matches!(
            super::StatefulLoopError::backend(reset()).into_session_error(),
            SessionError::Backend(_)
        ));
        assert!(matches!(
            super::StatefulLoopError::client(reset()).into_session_error(),
            SessionError::ClientDisconnect(_)
        ));
        for kind in [
            std::io::ErrorKind::TimedOut,
            std::io::ErrorKind::InvalidData,
        ] {
            let error = anyhow::Error::from(std::io::Error::from(kind));
            assert!(
                matches!(
                    super::StatefulLoopError::client(error).into_session_error(),
                    SessionError::ClientDisconnect(_)
                ),
                "a client-side {kind:?} is not a backend error"
            );
        }
    }

    #[test]
    fn stateful_session_exit_never_assumes_reader_state_is_reset() {
        assert_eq!(
            super::StatefulSessionExit::ClientDisconnected.disposition(),
            super::StatefulConnectionDisposition::RetireClient
        );

        assert_eq!(
            super::StatefulSessionExit::ClientReadError.disposition(),
            super::StatefulConnectionDisposition::RetireClient
        );
        assert_eq!(
            super::StatefulSessionExit::QuitCompleted.disposition(),
            super::StatefulConnectionDisposition::RetireClient
        );
        assert_eq!(
            super::StatefulSessionExit::BackendDisconnected.disposition(),
            super::StatefulConnectionDisposition::RetireBackend
        );
    }

    struct ReadCap<R> {
        inner: R,
        max_read: usize,
    }

    impl<R> ReadCap<R> {
        fn new(inner: R, max_read: usize) -> Self {
            assert!(max_read > 0);
            Self { inner, max_read }
        }
    }

    impl<R: AsyncRead + Unpin> AsyncRead for ReadCap<R> {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let this = self.get_mut();
            let cap = this.max_read.min(buf.remaining());
            if cap == 0 {
                return Poll::Ready(Ok(()));
            }

            let (result, read) = {
                let unfilled = buf.initialize_unfilled_to(cap);
                let mut limited = ReadBuf::new(unfilled);
                let result = Pin::new(&mut this.inner).poll_read(cx, &mut limited);
                let read = if matches!(&result, Poll::Ready(Ok(()))) {
                    limited.filled().len()
                } else {
                    0
                };
                (result, read)
            };
            match result {
                Poll::Ready(Ok(())) => {
                    buf.advance(read);
                    Poll::Ready(Ok(()))
                }
                Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
                Poll::Pending => Poll::Pending,
            }
        }
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
        first_accept_limit: Option<usize>,
    }

    impl GateWriter {
        fn new(expected_len: usize, partial: Arc<Notify>, complete: Arc<Notify>) -> Self {
            Self::with_first_accept_limit(expected_len, partial, complete, None)
        }

        fn with_first_accept_limit(
            expected_len: usize,
            partial: Arc<Notify>,
            complete: Arc<Notify>,
            first_accept_limit: Option<usize>,
        ) -> Self {
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
                first_accept_limit,
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
                first_accept_limit: self.first_accept_limit,
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
                drop(state);
                this.partial.notify_one();
                return Poll::Pending;
            }

            if state.first_write && !bytes.is_empty() {
                let accepted = this
                    .first_accept_limit
                    .unwrap_or_else(|| (bytes.len() / 2).max(1))
                    .min(bytes.len());
                state.bytes.extend_from_slice(&bytes[..accepted]);
                state.first_write = false;
                state.blocked = true;
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
    async fn compressed_stateful_request_flushes_a_partial_window() {
        use tokio::io::AsyncReadExt;
        let session = test_session();
        let (mut client, proxy_client) = tokio::io::duplex(4096);
        let (backend, proxy_backend) = tokio::io::duplex(4096);
        let mut backend = crate::compression::DecompressStream::new(backend);
        let proxy_backend = crate::compression::DecompressStream::new(proxy_backend);
        let (client_read, client_write) = tokio::io::split(proxy_client);
        let (backend_read, backend_write) = tokio::io::split(proxy_backend);
        let proxy = tokio::spawn(async move {
            session
                .run_stateful_proxy_loop(
                    BufReader::new(client_read),
                    client_write,
                    backend_read,
                    backend_write,
                    SessionLoopState::new(false),
                    BackendId::from_index(0),
                )
                .await
        });
        for commands in [b"DATE\r\n".as_slice(), b"DATE\r\nDA".as_slice()] {
            client.write_all(commands).await.unwrap();
            let mut command = [0; 6];
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                backend.read_exact(&mut command),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(&command, b"DATE\r\n");
            backend.write_all(b"111 ready\r\n").await.unwrap();
            backend.flush().await.unwrap();
            let mut reply = [0; 11];
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                client.read_exact(&mut reply),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(&reply, b"111 ready\r\n");
        }
        drop(client);
        let _ = proxy.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn malformed_client_lines_wait_for_prior_backend_replies() {
        use tokio::io::AsyncReadExt;
        for invalid in [
            b"\r\n".to_vec(),
            [vec![b'X'; 600], b"\r\n".to_vec()].concat(),
        ] {
            let session = test_session();
            let (mut client, proxy_client) = tokio::io::duplex(4096);
            let (backend, proxy_backend) = tokio::io::duplex(4096);
            let (mut backend_read, mut backend_write) = tokio::io::split(backend);
            let (client_read, client_write) = tokio::io::split(proxy_client);
            let (proxy_backend_read, proxy_backend_write) = tokio::io::split(proxy_backend);
            let proxy = tokio::spawn(async move {
                session
                    .run_stateful_proxy_loop(
                        BufReader::new(client_read),
                        client_write,
                        proxy_backend_read,
                        proxy_backend_write,
                        SessionLoopState::new(false),
                        BackendId::from_index(0),
                    )
                    .await
            });
            client.write_all(b"DATE\r\n").await.unwrap();
            let mut command = [0; 6];
            backend_read.read_exact(&mut command).await.unwrap();
            assert_eq!(&command, b"DATE\r\n");
            client.write_all(&invalid).await.unwrap();
            // No backend reply has been emitted, so no later local reply is ready.
            let mut first = [0; 1];
            assert!(
                tokio::time::timeout(
                    std::time::Duration::from_millis(50),
                    client.read_exact(&mut first)
                )
                .await
                .is_err()
            );
            backend_write.write_all(b"111 earlier\r\n").await.unwrap();
            let expected_error: &[u8] = if invalid.len() > 512 {
                crate::protocol::COMMAND_TOO_LONG
            } else {
                crate::protocol::COMMAND_SYNTAX_ERROR_RESPONSE
            };
            let expected = [b"111 earlier\r\n".as_slice(), expected_error].concat();
            let mut actual = vec![0; expected.len()];
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                client.read_exact(&mut actual),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(actual, expected);
            drop(client);
            let _ = proxy.await.unwrap().unwrap();
        }
    }

    /// Run one stateful session where the backend answers each client line
    /// with the next reply and then closes, while the client stays connected.
    async fn run_until_backend_closes(
        client_lines: &'static [u8],
        backend_replies: &'static [&'static [u8]],
    ) -> std::result::Result<super::StatefulLoopResult, super::StatefulLoopError> {
        let session = test_session();
        let (mut client_end, proxy_client_end) = tokio::io::duplex(4096);
        let (backend_end, proxy_backend_end) = tokio::io::duplex(4096);
        client_end.write_all(client_lines).await.unwrap();

        let backend = tokio::spawn(async move {
            let (backend_read, mut backend_write) = tokio::io::split(backend_end);
            let mut reader = BufReader::new(backend_read);
            let mut line = String::new();
            for reply in backend_replies {
                line.clear();
                reader.read_line(&mut line).await.unwrap();
                backend_write.write_all(reply).await.unwrap();
            }
            // Dropping both halves closes the backend side cleanly.
        });

        let (proxy_client_read, proxy_client_write) = tokio::io::split(proxy_client_end);
        let (backend_read, backend_write) = tokio::io::split(proxy_backend_end);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            session.run_stateful_proxy_loop(
                BufReader::new(proxy_client_read),
                proxy_client_write,
                backend_read,
                backend_write,
                SessionLoopState::new(false),
                BackendId::from_index(0),
            ),
        )
        .await
        .expect("stateful loop must end when the backend closes");
        backend.await.unwrap();
        drop(client_end);
        result
    }

    #[tokio::test]
    async fn backend_close_after_quit_retires_without_backend_failure() {
        let result = run_until_backend_closes(b"QUIT\r\n", &[b"205 Goodbye\r\n"])
            .await
            .expect("a close that answers QUIT is a clean exit");
        assert_eq!(
            result.disposition(),
            super::StatefulConnectionDisposition::RetireClient,
            "a QUIT close must not apply backend replacement cooldown"
        );
    }

    #[tokio::test]
    async fn backend_close_after_pipelined_quit_retires_without_backend_failure() {
        let result = run_until_backend_closes(
            b"DATE\r\nQUIT\r\n",
            &[b"111 20260101000000\r\n", b"205 Goodbye\r\n"],
        )
        .await
        .expect("a close that answers QUIT is a clean exit");
        assert_eq!(
            result.disposition(),
            super::StatefulConnectionDisposition::RetireClient
        );
    }

    #[tokio::test]
    async fn backend_close_without_quit_remains_a_backend_failure() {
        let result = run_until_backend_closes(b"DATE\r\n", &[b"111 20260101000000\r\n"])
            .await
            .expect("a close between replies is not an error");
        assert_eq!(
            result.disposition(),
            super::StatefulConnectionDisposition::RetireBackend,
            "an unprompted backend close still counts against the backend"
        );
    }

    #[tokio::test]
    async fn backend_close_after_an_unrelated_205_remains_a_backend_failure() {
        // A 205 that answers DATE is not a QUIT reply.
        let result = run_until_backend_closes(b"DATE\r\n", &[b"205 closing\r\n"])
            .await
            .expect("a close between replies is not an error");
        assert_eq!(
            result.disposition(),
            super::StatefulConnectionDisposition::RetireBackend
        );
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
    async fn upstream_window_releases_two_commands_before_backend_replies() {
        let session = test_session();
        let state = SessionLoopState::new(false);
        let (mut client_end, proxy_client_end) = tokio::io::duplex(4096);
        let (backend_end, proxy_backend_end) = tokio::io::duplex(4096);

        let backend = tokio::spawn(async move {
            let (backend_read, mut backend_write) = tokio::io::split(backend_end);
            let mut reader = BufReader::new(backend_read);
            let mut first = String::new();
            let mut second = String::new();
            reader.read_line(&mut first).await.unwrap();
            reader.read_line(&mut second).await.unwrap();
            assert_eq!(first, "DATE\r\n");
            assert_eq!(second, "DATE\r\n");
            backend_write
                .write_all(b"111 first\r\n111 second\r\n")
                .await
                .unwrap();
        });

        client_end.write_all(b"DATE\r\nDATE\r\n").await.unwrap();
        let (proxy_client_read, proxy_client_write) = tokio::io::split(proxy_client_end);
        let client_reader = BufReader::new(proxy_client_read);
        let (backend_read, backend_write) = tokio::io::split(proxy_backend_end);
        let proxy = tokio::spawn(async move {
            session
                .run_stateful_proxy_loop(
                    client_reader,
                    proxy_client_write,
                    backend_read,
                    backend_write,
                    state,
                    BackendId::from_index(0),
                )
                .await
        });

        let mut responses = [0u8; b"111 first\r\n111 second\r\n".len()];
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            tokio::io::AsyncReadExt::read_exact(&mut client_end, &mut responses),
        )
        .await
        .expect("client did not receive both ordered replies")
        .unwrap();
        assert_eq!(&responses, b"111 first\r\n111 second\r\n");

        drop(client_end);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), proxy)
            .await
            .expect("stateful proxy did not stop")
            .expect("stateful proxy task panicked");
        backend.await.unwrap();
    }

    /// A client whose connection is gone: every write fails.
    struct DisconnectedClient;

    impl AsyncWrite for DisconnectedClient {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Bytes the pinned backend sent are its traffic even when the client
    /// is gone before they can be written: the provider still delivered
    /// them. The client and user received none of them.
    #[tokio::test]
    async fn backend_bytes_read_for_a_disconnected_client_are_backend_traffic() {
        const COMMAND: &[u8] = b"BODY <gone@example.com>\r\n";
        const REPLY: &[u8] = b"222 0 <gone@example.com>\r\nbody\r\n.\r\n";
        let mut actual = Vec::new();
        let mut expected = Vec::new();
        for failover in [false, true] {
            let context = format!("reader_message_id_failover={failover}");
            let addr: std::net::SocketAddr = "127.0.0.1:9999".parse().unwrap();
            let session = ClientSession::builder(
                ClientAddress::from(addr),
                BufferPool::new(BufferSize::try_new(8192).unwrap(), 4),
                Arc::new(AuthHandler::new(None, None).unwrap()),
                MetricsCollector::new(1),
            )
            .with_router(Arc::new(crate::router::BackendSelector::new()))
            .with_routing_mode(crate::config::RoutingMode::Hybrid)
            .with_reader_message_id_failover(failover)
            .build();

            let (mut client_end, proxy_client_end) = tokio::io::duplex(4096);
            let (backend_end, proxy_backend_end) = tokio::io::duplex(4096);
            client_end.write_all(COMMAND).await.unwrap();
            let backend = tokio::spawn(async move {
                let (backend_read, mut backend_write) = tokio::io::split(backend_end);
                let mut reader = BufReader::new(backend_read);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                // One write, so the proxy reads the whole reply at once.
                backend_write.write_all(REPLY).await.unwrap();
                // Hold the backend open until the proxy gives up.
                let _ = reader.read_line(&mut line).await;
            });

            let (backend_read, backend_write) = tokio::io::split(proxy_backend_end);
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                session.run_stateful_proxy_loop(
                    BufReader::new(proxy_client_end),
                    DisconnectedClient,
                    backend_read,
                    backend_write,
                    SessionLoopState::new(false),
                    BackendId::from_index(0),
                ),
            )
            .await
            .expect("a failed client write ends the loop");
            assert!(result.is_err(), "{context}: the client write failed");
            backend.abort();
            drop(client_end);

            let counters = session
                .metrics
                .backend_counters(BackendId::from_index(0))
                .unwrap();
            let snapshot = session.metrics.snapshot(None);
            let [user] = snapshot.user_stats.as_slice() else {
                panic!("{context}: expected one user");
            };
            actual.push((
                failover,
                counters.bytes_to_backend,
                counters.bytes_from_backend,
                user.bytes_received.as_u64(),
            ));
            expected.push((failover, COMMAND.len() as u64, REPLY.len() as u64, 0));
        }
        assert_eq!(
            actual, expected,
            "(failover, backend bytes sent, backend bytes received, user bytes received)"
        );
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

    /// Run a stateful loop whose backend answers each step in `script` after
    /// reading the client's lines. Returns the loop result, how long it ran,
    /// and everything the client received.
    async fn run_stateful_script(
        client_lines: &[u8],
        script: Vec<BackendStep>,
    ) -> (
        std::result::Result<super::StatefulLoopResult, super::StatefulLoopError>,
        std::time::Duration,
        Vec<u8>,
    ) {
        let session = test_session();
        let (client_end, proxy_client_end) = tokio::io::duplex(1 << 16);
        let (backend_end, proxy_backend_end) = tokio::io::duplex(1 << 16);
        let (mut client_read, mut client_write) = tokio::io::split(client_end);
        client_write.write_all(client_lines).await.unwrap();
        let client_lines_count = client_lines.iter().filter(|&&b| b == b'\n').count();

        let backend = tokio::spawn(async move {
            let (backend_read, mut backend_write) = tokio::io::split(backend_end);
            let mut reader = BufReader::new(backend_read);
            let mut line = String::new();
            for _ in 0..client_lines_count {
                line.clear();
                reader.read_line(&mut line).await.unwrap();
            }
            for step in script {
                match step {
                    BackendStep::Send(bytes) => backend_write.write_all(&bytes).await.unwrap(),
                    BackendStep::Wait(duration) => tokio::time::sleep(duration).await,
                    BackendStep::Hang => std::future::pending::<()>().await,
                }
            }
            // Dropping both halves closes the backend side cleanly.
        });
        let received = tokio::spawn(async move {
            // Everything the client saw until the proxy closed its side.
            let mut received = Vec::new();
            let mut chunk = [0u8; 4096];
            while let Ok(n) = tokio::io::AsyncReadExt::read(&mut client_read, &mut chunk).await {
                if n == 0 {
                    break;
                }
                received.extend_from_slice(&chunk[..n]);
            }
            received
        });

        let (proxy_client_read, proxy_client_write) = tokio::io::split(proxy_client_end);
        let (backend_read, backend_write) = tokio::io::split(proxy_backend_end);
        let started = tokio::time::Instant::now();
        let result = tokio::time::timeout(
            crate::constants::timeout::BACKEND_READ * 10,
            session.run_stateful_proxy_loop(
                BufReader::new(proxy_client_read),
                proxy_client_write,
                backend_read,
                backend_write,
                SessionLoopState::new(false),
                BackendId::from_index(0),
            ),
        )
        .await
        .expect("a silent backend must not hold a stateful session forever");
        let elapsed = started.elapsed();
        backend.abort();
        drop(client_write);
        let received = received.await.unwrap();
        (result, elapsed, received)
    }

    enum BackendStep {
        Send(Vec<u8>),
        Wait(std::time::Duration),
        Hang,
    }

    fn assert_status_deadline_backend_error(
        result: std::result::Result<super::StatefulLoopResult, super::StatefulLoopError>,
        elapsed: std::time::Duration,
    ) {
        let Err(error) = result else {
            panic!("a missing status line is a backend error");
        };
        assert_eq!(
            error.disposition(),
            super::StatefulConnectionDisposition::RetireBackend,
            "the connection may still receive the reply, so it is retired"
        );
        assert!(
            elapsed >= crate::constants::timeout::BACKEND_READ
                && elapsed
                    < crate::constants::timeout::BACKEND_READ + std::time::Duration::from_secs(1),
            "one status-line deadline from the command, got {elapsed:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stateful_silent_backend_hits_status_line_deadline() {
        let (result, elapsed, received) =
            run_stateful_script(b"BODY <silent@example.com>\r\n", vec![BackendStep::Hang]).await;
        assert_status_deadline_backend_error(result, elapsed);
        assert!(received.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn stateful_partial_status_line_hits_status_line_deadline() {
        let (result, elapsed, received) = run_stateful_script(
            b"BODY <partial@example.com>\r\n",
            vec![
                BackendStep::Send(b"22".to_vec()),
                BackendStep::Wait(std::time::Duration::from_secs(20)),
                BackendStep::Send(b"2 0".to_vec()),
                BackendStep::Hang,
            ],
        )
        .await;
        assert_status_deadline_backend_error(result, elapsed);
        assert_eq!(
            received, b"222 0",
            "pass-through bytes were still forwarded"
        );
    }

    /// The deadline covers only the status line. A body may stream for longer
    /// than the budget, and a pipelined reply behind it starts its wait when
    /// it reaches the front, not when its command was written.
    #[tokio::test(start_paused = true)]
    async fn stateful_status_line_deadline_never_bounds_body_streaming() {
        let (commands, [body_head, body_rest, date], expected) =
            crate::session::multiline_framing::slow_body_pipeline_fixture();
        let (result, _elapsed, received) = run_stateful_script(
            &commands,
            vec![
                BackendStep::Send(body_head),
                BackendStep::Wait(
                    crate::constants::timeout::BACKEND_READ + std::time::Duration::from_secs(10),
                ),
                BackendStep::Send(body_rest),
                BackendStep::Wait(
                    crate::constants::timeout::BACKEND_READ
                        .saturating_sub(std::time::Duration::from_secs(5)),
                ),
                BackendStep::Send(date),
            ],
        )
        .await;
        let _ = result.expect("a slow body and a later status line within budget are not errors");
        assert_eq!(received, expected);
    }

    /// With nothing owed, an idle session waits for its client as long as
    /// the client likes.
    #[tokio::test(start_paused = true)]
    async fn stateful_idle_session_has_no_backend_deadline() {
        let session = test_session();
        let (client_end, proxy_client_end) = tokio::io::duplex(4096);
        let (_backend_end, proxy_backend_end) = tokio::io::duplex(4096);
        let (proxy_client_read, proxy_client_write) = tokio::io::split(proxy_client_end);
        let (backend_read, backend_write) = tokio::io::split(proxy_backend_end);
        let proxy = tokio::spawn(async move {
            session
                .run_stateful_proxy_loop(
                    BufReader::new(proxy_client_read),
                    proxy_client_write,
                    backend_read,
                    backend_write,
                    SessionLoopState::new(false),
                    BackendId::from_index(0),
                )
                .await
                .map(|result| result.disposition())
                .map_err(|error| error.disposition())
        });
        tokio::time::sleep(crate::constants::timeout::BACKEND_READ * 3).await;
        assert!(!proxy.is_finished(), "an idle session was ended");
        drop(client_end);
        let disposition = proxy
            .await
            .unwrap()
            .expect("a client close is a clean exit");
        assert_eq!(
            disposition,
            super::StatefulConnectionDisposition::RetireClient
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
