//! Backend command execution and response writing
//!
//! Handles executing commands on individual backends, including connection retry,
//! response validation, and writing backend responses to clients.

use crate::protocol::{RequestContext, RequestKind, RequestResponseMetadata, StatusCode};
use crate::router::{BackendSelector, CommandGuard};
use crate::session::SessionError;
use crate::session::response_buffer::{ResponseConnectionReuse, ResponseTransferError};
use crate::session::retry::retry_once;
use crate::session::routing::{
    CacheAction, MetricsAction, determine_cache_action_for_request,
    determine_metrics_action_for_request,
};
use crate::session::{ClientSession, backend};
use crate::types::{BackendId, BackendToClientBytes, ClientToBackendBytes};
use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tracing::{debug, warn};

const BACKEND_TIMING_SAMPLE_MASK: u64 = 0x0f;
static BACKEND_TIMING_SAMPLE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[inline]
pub fn should_sample_backend_timing() -> bool {
    BACKEND_TIMING_SAMPLE_COUNTER.fetch_add(1, Ordering::Relaxed) & BACKEND_TIMING_SAMPLE_MASK == 0
}

/// Result of attempting to execute a command on a backend
pub(super) enum BackendAttemptResult {
    /// Article found - response written successfully
    Success,
    /// Article not found (430) - try next backend
    /// Note: The 430 response is read and drained, just not stored
    ArticleNotFound { backend_id: BackendId },
    /// Backend unavailable or error - try next backend
    BackendUnavailable,
}

impl BackendAttemptResult {
    #[must_use]
    const fn success(
        request: &mut RequestContext,
        backend_id: BackendId,
        response: RequestResponseMetadata,
    ) -> Self {
        request.record_backend_response(backend_id, response);
        Self::Success
    }
}

/// Mutable state for an article backend attempt loop
///
/// Groups the mutable parameters that track retry state across
/// multiple `try_backend_for_article` calls.
pub(super) struct ArticleAttemptState<'a> {
    pub availability: &'a mut crate::cache::ArticleAvailability,
    pub client_to_backend_bytes: &'a mut ClientToBackendBytes,
    pub backend_connection: &'a mut Option<(BackendId, crate::pool::ConnectionGuard)>,
}

type BackendTimings = (u64, u64, u64);

/// Parameters describing the response to write to the client
#[derive(Clone, Copy)]
struct ResponseWriteParams<'a> {
    request: &'a RequestContext,
    msg_id: Option<&'a crate::types::MessageId<'a>>,
    status_code: StatusCode,
}

struct InvalidBackendResponseContext<'a> {
    provider: &'a crate::pool::DeadpoolConnectionProvider,
    request: &'a RequestContext,
    availability: &'a mut crate::cache::ArticleAvailability,
    buffer: &'a crate::pool::PooledBuffer,
    conn: crate::pool::ConnectionGuard,
}

type PreparedBackendAttempt = Option<(
    crate::pool::ConnectionGuard,
    backend::BackendResponseStatus,
    StatusCode,
    crate::pool::PooledBuffer,
)>;

/// Any client write failure after the full backend response is already buffered is terminal.
///
/// The backend connection is already clean at this point, but the client may have received a
/// partial prefix of the response. Retrying another backend on the same client socket would
/// splice responses together and corrupt NNTP response ordering.
const fn classify_response_write_err(e: std::io::Error) -> ResponseTransferError {
    ResponseTransferError::ClientDisconnect(e)
}

const fn can_write_without_owned_response(
    request: &RequestContext,
    cache_action: CacheAction,
) -> bool {
    matches!(
        request.kind(),
        RequestKind::Article | RequestKind::Body | RequestKind::Head
    ) && matches!(
        cache_action,
        CacheAction::TrackAvailability | CacheAction::None
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResponseOwnership {
    NotOwned,
    Owned,
}

impl ResponseOwnership {
    const fn for_request(request: &RequestContext, cache_action: CacheAction) -> Self {
        match can_write_without_owned_response(request, cache_action) {
            true => Self::NotOwned,
            false => Self::Owned,
        }
    }
}

impl ClientSession {
    /// Try executing command on next available backend
    ///
    /// If the pooled connection is stale (connection error), automatically retries
    /// with a fresh connection before returning an error.
    pub(super) async fn try_backend_for_article<W>(
        &self,
        router: &Arc<BackendSelector>,
        request: &mut RequestContext,
        client_write: &mut W,
        state: &mut ArticleAttemptState<'_>,
    ) -> Result<BackendAttemptResult, SessionError>
    where
        W: AsyncWrite + Unpin,
    {
        let backend_id =
            match router.route_with_availability(self.client_id, Some(state.availability)) {
                Ok(backend_id) => backend_id,
                Err(err) => {
                    debug!(
                        client = %self.client_addr,
                        error = %err,
                        "No eligible backend available for article retry"
                    );
                    return Ok(BackendAttemptResult::BackendUnavailable);
                }
            };
        debug!(
            client = %self.client_addr,
            backend = backend_id.as_index(),
            command_verb = ?request.verb(),
            msg_id = ?request.message_id_value(),
            missing_bits = format_args!("{:08b}", state.availability.missing_bits()),
            "Article retry selected backend for direct attempt"
        );
        let guard = CommandGuard::new(router.clone(), backend_id);
        let Some(provider) = router.backend_provider(backend_id) else {
            state.availability.record_missing(backend_id);
            debug!(
                client = %self.client_addr,
                backend = backend_id.as_index(),
                command_verb = ?request.verb(),
                msg_id = ?request.message_id_value(),
                "Selected backend had no provider; treating as unavailable"
            );
            return Ok(BackendAttemptResult::BackendUnavailable);
        };

        let Some((mut conn, _cmd_response, status_code, buffer)) = self
            .prepare_backend_attempt(provider, backend_id, request, state)
            .await?
        else {
            return Ok(BackendAttemptResult::BackendUnavailable);
        };

        if status_code.as_u16() == 430 {
            debug!(
                client = %self.client_addr,
                backend = backend_id.as_index(),
                command_verb = ?request.verb(),
                msg_id = ?request.message_id_value(),
                "Direct backend attempt returned 430 before writing response"
            );
            self.handle_430_availability(backend_id, state.availability);
            self.buffer_suppressed_430_response(
                &mut conn,
                backend_id,
                request,
                buffer,
                status_code,
            )
            .await?;
            self.release_or_reuse_connection(
                conn,
                backend_id,
                request,
                Some(state.backend_connection),
            );
            return Ok(BackendAttemptResult::ArticleNotFound { backend_id });
        }

        let msg_id = request.message_id_value();
        let response = self
            .write_successful_backend_response(
                conn,
                client_write,
                backend_id,
                buffer,
                ResponseWriteParams {
                    request,
                    msg_id: msg_id.as_ref(),
                    status_code,
                },
                Some(state.backend_connection),
            )
            .await?;
        guard.complete();
        Ok(BackendAttemptResult::success(request, backend_id, response))
    }

    async fn prepare_backend_attempt(
        &self,
        provider: &crate::pool::DeadpoolConnectionProvider,
        backend_id: BackendId,
        request: &RequestContext,
        state: &mut ArticleAttemptState<'_>,
    ) -> Result<PreparedBackendAttempt, SessionError> {
        let request_wire_len = request.request_wire_len().get();
        debug!(
            client = %self.client_addr,
            backend = backend_id.as_index(),
            command_verb = ?request.verb(),
            msg_id = ?request.message_id_value(),
            request_wire_len,
            "Preparing direct backend attempt"
        );
        let (conn, cmd_response, buffer, timings) = retry_once!(
            self.execute_backend_attempt(provider, backend_id, request, state.backend_connection)
                .await,
            client = self.client_addr,
            backend = backend_id.as_index()
        )
        .map_err(SessionError::Backend)?;

        if let Some((ttfb, send, recv)) = timings {
            self.record_timing_metrics(backend_id, ttfb, send, recv);
        }
        *state.client_to_backend_bytes = state.client_to_backend_bytes.add(request_wire_len);

        let Some(status_code) = cmd_response.status_code() else {
            debug!(
                client = %self.client_addr,
                backend = backend_id.as_index(),
                command_verb = ?request.verb(),
                msg_id = ?request.message_id_value(),
                "Backend attempt read bytes but could not parse a status code"
            );
            self.handle_invalid_backend_response(
                backend_id,
                InvalidBackendResponseContext {
                    provider,
                    request,
                    availability: state.availability,
                    buffer: &buffer,
                    conn,
                },
            );
            return Ok(None);
        };
        debug!(
            client = %self.client_addr,
            backend = backend_id.as_index(),
            command_verb = ?request.verb(),
            msg_id = ?request.message_id_value(),
            status_code = status_code.as_u16(),
            "Backend attempt parsed first response successfully"
        );

        Ok(Some((conn, cmd_response, status_code, buffer)))
    }

    async fn buffer_suppressed_430_response(
        &self,
        conn: &mut crate::pool::ConnectionGuard,
        backend_id: BackendId,
        request: &RequestContext,
        mut buffer: crate::pool::PooledBuffer,
        status_code: StatusCode,
    ) -> Result<(), SessionError> {
        let mut suppressed = crate::pool::ChunkedResponse::default();
        crate::session::multiline_framing::InitialResponseCapture::from_buffer(
            request,
            status_code,
            &mut buffer,
            conn,
            &mut suppressed,
            &self.buffer_pool,
            backend_id,
        )
        .capture()
        .await
        .map_err(SessionError::from)
    }

    fn handle_invalid_backend_response(
        &self,
        backend_id: BackendId,
        ctx: InvalidBackendResponseContext<'_>,
    ) {
        let InvalidBackendResponseContext {
            provider,
            request,
            availability,
            buffer,
            conn,
        } = ctx;
        let bytes_to_read = buffer.initialized();
        tracing::warn!(
            client = %self.client_addr,
            backend = ?backend_id,
            command_verb = ?request.verb(),
            first_bytes_hex = %crate::session::backend::format_hex_preview(
                &buffer[..bytes_to_read], 256
            ),
            first_bytes_utf8 = %String::from_utf8_lossy(&buffer[..bytes_to_read.min(256)]),
            "Backend returned invalid/unparseable response, attempting to salvage connection"
        );
        availability.record_missing(backend_id);

        let request_kind = request.kind();
        let provider_for_salvage = provider.clone();
        let conn_for_salvage = conn.release();
        tokio::spawn(async move {
            tracing::debug!(
                backend = ?backend_id,
                request_kind = ?request_kind,
                "Attempting to salvage connection after Invalid response"
            );
            crate::pool::salvage_with_health_check(conn_for_salvage, provider_for_salvage).await;
        });
    }

    #[allow(clippy::too_many_arguments)]
    async fn write_successful_backend_response<W>(
        &self,
        mut conn: crate::pool::ConnectionGuard,
        client_write: &mut W,
        backend_id: BackendId,
        mut first_buffer: crate::pool::PooledBuffer,
        params: ResponseWriteParams<'_>,
        backend_connection: Option<&mut Option<(BackendId, crate::pool::ConnectionGuard)>>,
    ) -> Result<RequestResponseMetadata, SessionError>
    where
        W: AsyncWrite + Unpin,
    {
        let has_response_body = params.request.has_response_body(params.status_code);
        debug!(
            client = %self.client_addr,
            backend = backend_id.as_index(),
            command_verb = ?params.request.verb(),
            status_code = params.status_code.as_u16(),
            has_response_body,
            "Writing backend response to client"
        );
        let bytes_written = match self
            .write_response_to_client(
                &mut conn,
                client_write,
                backend_id,
                &mut first_buffer,
                params,
            )
            .await
        {
            Ok(bytes) => bytes,
            Err(e) => {
                return Err(self.handle_response_transfer_error(
                    conn,
                    backend_id,
                    params.request,
                    e,
                ));
            }
        };
        if let Err(e) = client_write.flush().await {
            return Err(self.handle_response_transfer_error(
                conn,
                backend_id,
                params.request,
                classify_response_write_err(e),
            ));
        }

        debug!(
            client = %self.client_addr,
            backend = backend_id.as_index(),
            msg_id = ?params.msg_id,
            bytes_written,
            "Backend response write complete"
        );
        self.record_response_metrics(
            backend_id,
            params.request,
            params.status_code,
            params.request.request_wire_len().as_u64(),
            bytes_written,
        );
        self.release_or_reuse_connection(conn, backend_id, params.request, backend_connection);

        Ok(RequestResponseMetadata::new(
            params.status_code,
            usize::try_from(bytes_written).unwrap_or(usize::MAX).into(),
        ))
    }

    fn handle_response_transfer_error(
        &self,
        conn: crate::pool::ConnectionGuard,
        backend_id: BackendId,
        request: &RequestContext,
        error: ResponseTransferError,
    ) -> SessionError {
        if error.must_remove_connection() {
            warn!(
                client = %self.client_addr,
                backend = backend_id.as_index(),
                command_verb = ?request.verb(),
                error = %error,
                "Response write error, removing connection from pool"
            );
            self.metrics.record_error(backend_id);
            self.metrics.user_error(self.username());
        } else if let ResponseConnectionReuse::QueuedBytes { len } =
            crate::session::response_buffer::connection_reuse_after_response(&conn)
        {
            warn!(
                client = %self.client_addr,
                backend = backend_id.as_index(),
                command_verb = ?request.verb(),
                queued_bytes = len,
                "Response write left queued backend bytes; retiring connection"
            );
        } else {
            debug!(
                client = %self.client_addr,
                backend = backend_id.as_index(),
                command_verb = ?request.verb(),
                "Response write error left backend connection reusable; releasing it back to pool"
            );
            let _ = conn.release();
        }

        SessionError::from(error)
    }

    fn release_or_reuse_connection(
        &self,
        conn: crate::pool::ConnectionGuard,
        backend_id: BackendId,
        request: &RequestContext,
        backend_connection: Option<&mut Option<(BackendId, crate::pool::ConnectionGuard)>>,
    ) {
        if let ResponseConnectionReuse::QueuedBytes { len } =
            crate::session::response_buffer::connection_reuse_after_response(&conn)
        {
            warn!(
                client = %self.client_addr,
                backend = backend_id.as_index(),
                command_verb = ?request.verb(),
                queued_bytes = len,
                "Direct per-command response left queued backend bytes; retiring connection"
            );
        } else if let Some(slot) = backend_connection
            && slot.is_none()
        {
            debug!(
                client = %self.client_addr,
                backend = backend_id.as_index(),
                command_verb = ?request.verb(),
                "Direct per-command response finished cleanly; keeping backend connection for this client batch"
            );
            *slot = Some((backend_id, conn));
        } else {
            debug!(
                client = %self.client_addr,
                backend = backend_id.as_index(),
                command_verb = ?request.verb(),
                "Direct per-command response finished cleanly; releasing backend connection"
            );
            let _ = conn.release();
        }
    }

    /// Execute a single backend attempt - get connection and execute command
    ///
    /// Returns the connection and response data on success.
    /// On error, the connection is removed from pool before returning.
    async fn execute_backend_attempt(
        &self,
        provider: &crate::pool::DeadpoolConnectionProvider,
        backend_id: crate::types::BackendId,
        request: &RequestContext,
        backend_connection: &mut Option<(BackendId, crate::pool::ConnectionGuard)>,
    ) -> Result<(
        crate::pool::ConnectionGuard,
        backend::BackendResponseStatus,
        crate::pool::PooledBuffer,
        Option<BackendTimings>,
    )> {
        debug!(
            client = %self.client_addr,
            backend = backend_id.as_index(),
            command_verb = ?request.verb(),
            msg_id = ?request.message_id_value(),
            "Starting pool checkout for direct backend attempt"
        );
        let mut guard = match backend_connection.take() {
            Some((cached_backend_id, guard)) if cached_backend_id == backend_id => {
                debug!(
                    client = %self.client_addr,
                    backend = backend_id.as_index(),
                    command_verb = ?request.verb(),
                    msg_id = ?request.message_id_value(),
                    "Reusing backend connection for direct backend attempt"
                );
                guard
            }
            Some(cached) => {
                let (cached_backend_id, cached_conn) = cached;
                debug!(
                    client = %self.client_addr,
                    cached_backend = cached_backend_id.as_index(),
                    backend = backend_id.as_index(),
                    command_verb = ?request.verb(),
                    msg_id = ?request.message_id_value(),
                    "Releasing cached backend connection before switching backend"
                );
                let _ = cached_conn.release();
                let conn = provider.get_pooled_connection().await?;
                debug!(
                    client = %self.client_addr,
                    backend = backend_id.as_index(),
                    command_verb = ?request.verb(),
                    msg_id = ?request.message_id_value(),
                    "Pool checkout succeeded for direct backend attempt"
                );
                crate::pool::ConnectionGuard::new(conn, provider.clone())
            }
            None => {
                let conn = provider.get_pooled_connection().await?;
                debug!(
                    client = %self.client_addr,
                    backend = backend_id.as_index(),
                    command_verb = ?request.verb(),
                    msg_id = ?request.message_id_value(),
                    "Pool checkout succeeded for direct backend attempt"
                );
                crate::pool::ConnectionGuard::new(conn, provider.clone())
            }
        };
        let mut buffer = self.buffer_pool.acquire();

        debug!(
            client = %self.client_addr,
            backend = backend_id.as_index(),
            command_verb = ?request.verb(),
            msg_id = ?request.message_id_value(),
            "Sending request to backend and waiting for first response bytes"
        );
        let result = self
            .execute_and_read_initial_response(&mut guard, backend_id, request, &mut buffer)
            .await;

        match result {
            Ok((cmd_response, timings)) => Ok((guard, cmd_response, buffer, timings)),
            Err(e) => {
                debug!(
                    client = %self.client_addr,
                    backend = backend_id.as_index(),
                    command_verb = ?request.verb(),
                    error = %e,
                    "Backend attempt failed before response completed; dropping pooled connection"
                );
                Err(e) // guard drops → remove_with_cooldown
            }
        }
    }

    /// Execute command on a connection and read initial response.
    ///
    /// Takes `&mut ConnectionStream` (not a pool object) so it can be called
    /// directly after pool checkout.
    async fn execute_and_read_initial_response(
        &self,
        conn: &mut crate::stream::ConnectionStream,
        backend_id: crate::types::BackendId,
        request: &RequestContext,
        buffer: &mut crate::pool::PooledBuffer,
    ) -> Result<(backend::BackendResponseStatus, Option<BackendTimings>)> {
        self.metrics.record_command(backend_id);
        self.metrics.user_command(self.username());

        let (response, timings) = if should_sample_backend_timing() {
            let (response, ttfb, send, recv) =
                backend::send_request_timed(conn, request, buffer).await?;
            (response, Some((ttfb, send, recv)))
        } else {
            (backend::send_request(conn, request, buffer).await?, None)
        };

        // Log any validation warnings
        response.log_warnings(buffer, self.client_addr, backend_id);
        debug!(
            client = %self.client_addr,
            backend = backend_id.as_index(),
            command_verb = ?request.verb(),
            msg_id = ?request.message_id_value(),
            parsed_status = ?response.status_code(),
            "Backend returned first response bytes for direct attempt"
        );

        Ok((response, timings))
    }

    /// Write response from backend to client and handle caching.
    ///
    /// Returns `ResponseTransferError` so callers can decide the connection's pool fate
    /// without string/downcast inspection.
    async fn write_response_to_client<W>(
        &self,
        pooled_conn: &mut deadpool::managed::Object<crate::pool::deadpool_connection::TcpManager>,
        client_write: &mut W,
        backend_id: BackendId,
        first_buffer: &mut crate::pool::PooledBuffer,
        params: ResponseWriteParams<'_>,
    ) -> Result<u64, ResponseTransferError>
    where
        W: AsyncWrite + Unpin,
    {
        let cache_action = determine_cache_action_for_request(
            params.request,
            params.status_code,
            self.cache_articles,
            params.msg_id.is_some(),
        );

        debug!(
            "write_response_to_client: code={}, cache_articles={}, has_msg_id={}, action={:?}",
            params.status_code.as_u16(),
            self.cache_articles,
            params.msg_id.is_some(),
            cache_action
        );

        let (bytes_written, captured) =
            match ResponseOwnership::for_request(params.request, cache_action) {
                ResponseOwnership::NotOwned => {
                    let bytes = self
                        .write_response_without_ownership(
                            pooled_conn,
                            client_write,
                            backend_id,
                            first_buffer,
                            params,
                        )
                        .await?;
                    (bytes, None)
                }
                ResponseOwnership::Owned => {
                    let (bytes, response) = self
                        .write_owned_response_to_client(
                            pooled_conn,
                            client_write,
                            backend_id,
                            first_buffer,
                            params,
                        )
                        .await?;
                    (bytes, Some(response))
                }
            };

        self.apply_cache_action(cache_action, params, backend_id, captured);
        Ok(bytes_written)
    }

    fn apply_cache_action(
        &self,
        cache_action: CacheAction,
        params: ResponseWriteParams<'_>,
        backend_id: BackendId,
        captured: Option<crate::pool::ChunkedResponse>,
    ) {
        match (cache_action, params.msg_id, captured) {
            (CacheAction::CaptureArticle, msg_id, Some(response)) => {
                self.maybe_cache_upsert_buffer(msg_id, response.into(), backend_id);
            }
            (CacheAction::TrackAvailability, Some(msg_id), _)
                if !params.request.cache_records_backend_has_article(backend_id) =>
            {
                self.spawn_cache_upsert_availability(
                    msg_id,
                    params.status_code,
                    backend_id,
                    self.tier_for_backend(backend_id),
                );
            }
            (CacheAction::TrackStat, msg_id, _) => {
                self.maybe_cache_upsert_buffer(
                    msg_id,
                    crate::cache::CacheIngestResponse::from(b"223\r\n".as_slice()),
                    backend_id,
                );
            }
            (CacheAction::None, _, _)
            | (CacheAction::TrackAvailability, _, _)
            | (CacheAction::CaptureArticle, _, None) => {}
        }
    }

    async fn write_response_without_ownership<W>(
        &self,
        pooled_conn: &mut deadpool::managed::Object<crate::pool::deadpool_connection::TcpManager>,
        client_write: &mut W,
        backend_id: BackendId,
        first_buffer: &mut crate::pool::PooledBuffer,
        params: ResponseWriteParams<'_>,
    ) -> Result<u64, ResponseTransferError>
    where
        W: AsyncWrite + Unpin,
    {
        crate::session::multiline_framing::InitialResponseWrite {
            request: params.request,
            status: params.status_code,
            io_buffer: first_buffer,
            conn: pooled_conn,
            writer: client_write,
            backend_id,
        }
        .write()
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn write_owned_response_to_client<W>(
        &self,
        pooled_conn: &mut deadpool::managed::Object<crate::pool::deadpool_connection::TcpManager>,
        client_write: &mut W,
        backend_id: BackendId,
        first_buffer: &mut crate::pool::PooledBuffer,
        params: ResponseWriteParams<'_>,
    ) -> Result<(u64, crate::pool::ChunkedResponse), ResponseTransferError>
    where
        W: AsyncWrite + Unpin,
    {
        let mut captured = crate::pool::ChunkedResponse::default();
        crate::session::multiline_framing::InitialResponseCapture::from_buffer(
            params.request,
            params.status_code,
            first_buffer,
            pooled_conn,
            &mut captured,
            &self.buffer_pool,
            backend_id,
        )
        .capture()
        .await?;
        self.log_body_response_buffered(backend_id, params, captured.len());
        captured
            .write_all_to(client_write)
            .await
            .map_err(classify_response_write_err)?;
        self.log_body_response_written(backend_id, params, captured.len());
        if let Some(msg_id_ref) = params.msg_id {
            debug!(
                "Client {} caching full article for {} ({} bytes captured)",
                self.client_addr,
                msg_id_ref,
                captured.len()
            );
        }
        Ok((captured.len() as u64, captured))
    }

    fn log_body_response_buffered(
        &self,
        backend_id: BackendId,
        params: ResponseWriteParams<'_>,
        response_len: usize,
    ) {
        if matches!(params.request.kind(), RequestKind::Body) {
            debug!(
                client = %self.client_addr,
                backend = backend_id.as_index(),
                command_verb = ?params.request.verb(),
                status_code = params.status_code.as_u16(),
                response_len,
                "Buffered BODY response for direct client delivery"
            );
        }
    }

    fn log_body_response_written(
        &self,
        backend_id: BackendId,
        params: ResponseWriteParams<'_>,
        bytes_written: usize,
    ) {
        if matches!(params.request.kind(), RequestKind::Body) {
            debug!(
                client = %self.client_addr,
                backend = backend_id.as_index(),
                command_verb = ?params.request.verb(),
                status_code = params.status_code.as_u16(),
                bytes_written,
                "Wrote buffered BODY response to direct client"
            );
        }
    }

    /// Handle backend error (metrics and cleanup)
    /// Send standardized 430 response to client
    pub(super) async fn send_430_to_client<W>(
        &self,
        client_write: &mut W,
        backend_to_client_bytes: &mut BackendToClientBytes,
    ) -> Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        client_write
            .write_all(crate::protocol::NO_SUCH_ARTICLE)
            .await?;
        *backend_to_client_bytes =
            backend_to_client_bytes.add(crate::protocol::NO_SUCH_ARTICLE.len());
        Ok(())
    }

    /// Record timing metrics for a backend response
    fn record_timing_metrics(
        &self,
        backend_id: crate::types::BackendId,
        ttfb: u64,
        send: u64,
        recv: u64,
    ) {
        self.metrics.record_ttfb_micros(backend_id, ttfb);
        self.metrics.record_send_recv_micros(backend_id, send, recv);
    }

    #[inline]
    fn maybe_cache_upsert_buffer(
        &self,
        msg_id: Option<&crate::types::MessageId<'_>>,
        data: crate::cache::CacheIngestResponse,
        backend_id: BackendId,
    ) {
        if let Some(msg_id_ref) = msg_id {
            let tier = self.tier_for_backend(backend_id);
            self.spawn_cache_upsert_buffer(msg_id_ref, data, backend_id, tier);
        }
    }

    /// Record response metrics (errors, article sizes, command execution)
    fn record_response_metrics(
        &self,
        backend_id: crate::types::BackendId,
        request: &RequestContext,
        status_code: StatusCode,
        cmd_bytes: u64,
        resp_bytes: u64,
    ) {
        use crate::types::MetricsBytes;

        match determine_metrics_action_for_request(request, status_code) {
            MetricsAction::Error4xx => self.metrics.record_error_4xx(backend_id),
            MetricsAction::Error5xx => self.metrics.record_error_5xx(backend_id),
            MetricsAction::Article => self.metrics.record_article(backend_id, resp_bytes),
            MetricsAction::None => {}
        }

        let cmd_bytes_metric = MetricsBytes::new(cmd_bytes);
        let resp_bytes_metric = MetricsBytes::new(resp_bytes);
        let _ =
            self.metrics
                .record_command_execution(backend_id, cmd_bytes_metric, resp_bytes_metric);
        self.metrics.user_bytes_sent(self.username(), cmd_bytes);
        self.metrics
            .user_bytes_received(self.username(), resp_bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::BackendAttemptResult;
    use super::ResponseWriteParams;
    use super::classify_response_write_err;
    use crate::auth::AuthHandler;
    use crate::metrics::MetricsCollector;
    use crate::pool::{BufferPool, ConnectionGuard, DeadpoolConnectionProvider};
    use crate::protocol::{RequestContext, RequestResponseMetadata, ResponseWireLen, StatusCode};
    use crate::types::{BackendId, BufferSize, ClientAddress};
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};
    use tokio::io::AsyncWrite;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    fn request_context(line: &[u8]) -> RequestContext {
        RequestContext::parse(line).expect("valid request line")
    }

    fn test_session() -> crate::session::ClientSession {
        let addr: std::net::SocketAddr = "127.0.0.1:9999".parse().unwrap();
        let buffer_pool = BufferPool::new(BufferSize::try_new(8192).unwrap(), 4);
        let auth_handler = Arc::new(AuthHandler::new(None, None).unwrap());
        let metrics = MetricsCollector::new(1);
        crate::session::ClientSession::builder(
            ClientAddress::from(addr),
            buffer_pool,
            auth_handler,
            metrics,
        )
        .build()
    }

    async fn spawn_greeting_server() -> (u16, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accept_count = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&accept_count);

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                count.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let (read_half, mut write_half) = stream.into_split();
                    let mut reader = BufReader::new(read_half);

                    if write_half.write_all(b"200 Ready\r\n").await.is_err() {
                        return;
                    }

                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                let cmd = line.trim().to_ascii_uppercase();
                                if cmd == "COMPRESS DEFLATE" {
                                    let _ = write_half.write_all(b"500 Not supported\r\n").await;
                                } else if cmd.starts_with("MODE") {
                                    let _ = write_half.write_all(b"200 Posting allowed\r\n").await;
                                } else if cmd.starts_with("QUIT") {
                                    let _ = write_half.write_all(b"205 Goodbye\r\n").await;
                                    break;
                                } else if cmd.starts_with("DATE") {
                                    let _ = write_half.write_all(b"111 20240101000000\r\n").await;
                                } else {
                                    let _ = write_half.write_all(b"200 OK\r\n").await;
                                }
                            }
                        }
                    }
                });
            }
        });

        (port, accept_count)
    }

    struct FlushFailWriter {
        writes: Vec<u8>,
    }

    impl FlushFailWriter {
        fn new() -> Self {
            Self { writes: Vec::new() }
        }
    }

    impl AsyncWrite for FlushFailWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.writes.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    struct WriteFailWriter;

    impl AsyncWrite for WriteFailWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn backend_attempt_success_records_success() {
        let backend_id = BackendId::from_index(1);
        let response = RequestResponseMetadata::new(StatusCode::new(220), ResponseWireLen::new(42));
        let mut request = request_context(b"ARTICLE <test@example.com>\r\n");
        let result = BackendAttemptResult::success(&mut request, backend_id, response);

        assert!(matches!(result, BackendAttemptResult::Success));
    }

    #[test]
    fn backend_attempt_success_records_request_context() {
        let backend_id = BackendId::from_index(1);
        let response = RequestResponseMetadata::new(StatusCode::new(220), ResponseWireLen::new(42));
        let mut request = request_context(b"ARTICLE <test@example.com>\r\n");

        let result = BackendAttemptResult::success(&mut request, backend_id, response);

        assert!(matches!(result, BackendAttemptResult::Success));
        assert_eq!(request.backend_id(), Some(backend_id));
        assert_eq!(request.response_metadata(), Some(response));
    }

    #[test]
    fn buffered_response_timeout_is_terminal_client_disconnect() {
        let err = std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out");
        let classified = classify_response_write_err(err);
        assert!(matches!(
            classified,
            crate::session::response_buffer::ResponseTransferError::ClientDisconnect(_)
        ));
    }

    #[test]
    fn buffered_response_abort_is_terminal_client_disconnect() {
        let err = std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "aborted");
        let classified = classify_response_write_err(err);
        assert!(matches!(
            classified,
            crate::session::response_buffer::ResponseTransferError::ClientDisconnect(_)
        ));
    }

    #[tokio::test]
    async fn flush_disconnect_after_buffered_response_reuses_backend_connection() {
        let session = test_session();
        let (port, accept_count) = spawn_greeting_server().await;
        let provider = DeadpoolConnectionProvider::builder("127.0.0.1", port)
            .max_connections(5)
            .build()
            .unwrap();
        let conn = provider.get_pooled_connection().await.unwrap();
        assert_eq!(accept_count.load(Ordering::SeqCst), 1);

        let guard = ConnectionGuard::new(conn, provider.clone());
        let request = request_context(b"STAT <test@example.com>\r\n");
        let mut client_write = FlushFailWriter::new();
        let mut first_buffer = BufferPool::new(BufferSize::try_new(8192).unwrap(), 1).acquire();
        first_buffer.copy_from_slice(b"223 0 <test@example.com> status\r\n");

        let err = session
            .write_successful_backend_response(
                guard,
                &mut client_write,
                BackendId::from_index(0),
                first_buffer,
                ResponseWriteParams {
                    request: &request,
                    msg_id: None,
                    status_code: StatusCode::new(223),
                },
                None,
            )
            .await
            .expect_err("flush should fail with client disconnect");

        assert!(matches!(
            err,
            crate::session::SessionError::ClientDisconnect(_)
        ));
        assert_eq!(client_write.writes, b"223 0 <test@example.com> status\r\n");

        let _reused = provider.get_pooled_connection().await.unwrap();
        assert_eq!(
            accept_count.load(Ordering::SeqCst),
            1,
            "client-side flush failure after a clean buffered response must not retire the backend connection"
        );
    }

    #[tokio::test]
    async fn write_disconnect_before_complete_response_body_retires_backend_connection() {
        let session = test_session();
        let (port, accept_count) = spawn_greeting_server().await;
        let provider = DeadpoolConnectionProvider::builder("127.0.0.1", port)
            .max_connections(5)
            .build()
            .unwrap();
        let conn = provider.get_pooled_connection().await.unwrap();
        assert_eq!(accept_count.load(Ordering::SeqCst), 1);

        let guard = ConnectionGuard::new(conn, provider.clone());
        let request = request_context(b"ARTICLE <test@example.com>\r\n");
        let mut client_write = WriteFailWriter;
        let mut first_buffer = BufferPool::new(BufferSize::try_new(8192).unwrap(), 1).acquire();
        let initial_read = b"220 Article follows\r\npartial body chunk\r\n";
        first_buffer.copy_from_slice(initial_read);

        let err = session
            .write_successful_backend_response(
                guard,
                &mut client_write,
                BackendId::from_index(0),
                first_buffer,
                ResponseWriteParams {
                    request: &request,
                    msg_id: None,
                    status_code: StatusCode::new(220),
                },
                None,
            )
            .await
            .expect_err("client write should fail before backend response is drained");

        assert!(matches!(
            err,
            crate::session::SessionError::ClientDisconnect(_)
        ));

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let _fresh = provider.get_pooled_connection().await.unwrap();
        assert_eq!(
            accept_count.load(Ordering::SeqCst),
            2,
            "client disconnect before complete response body leaves the backend write dirty"
        );
    }
}
