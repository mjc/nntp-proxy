//! Backend command execution and response streaming
//!
//! Handles executing commands on individual backends, including connection retry,
//! response validation, and streaming multiline responses to clients.

use crate::protocol::{RequestContext, RequestResponseMetadata, StatusCode};
use crate::router::{BackendSelector, CommandGuard};
use crate::session::SessionError;
use crate::session::response_buffer::{BufferContext, StreamingError};
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
    /// Article found - response streamed successfully
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
}

type BackendTimings = (u64, u64, u64);

/// Parameters describing the response to stream to the client
#[derive(Clone, Copy)]
struct ResponseStreamParams<'a> {
    request: &'a RequestContext,
    msg_id: Option<&'a crate::types::MessageId<'a>>,
    status_code: StatusCode,
    first_chunk: &'a [u8],
}

struct InvalidBackendResponseContext<'a> {
    provider: &'a crate::pool::DeadpoolConnectionProvider,
    request: &'a RequestContext,
    availability: &'a mut crate::cache::ArticleAvailability,
    buffer: &'a crate::pool::PooledBuffer,
    conn: crate::pool::ConnectionGuard,
    bytes_read: usize,
}

type PreparedBackendAttempt = Option<(
    crate::pool::ConnectionGuard,
    backend::BackendFirstResponse,
    StatusCode,
    crate::pool::PooledBuffer,
)>;

/// Any client write failure after the full backend response is already buffered is terminal.
///
/// The backend connection is already clean at this point, but the client may have received a
/// partial prefix of the response. Retrying another backend on the same client socket would
/// splice responses together and corrupt NNTP framing.
const fn classify_buffered_response_write_err(e: std::io::Error) -> StreamingError {
    StreamingError::ClientDisconnect(e)
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

        let Some((conn, cmd_response, status_code, buffer)) = self
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
                "Direct backend attempt returned 430 before streaming"
            );
            self.handle_430_availability(backend_id, state.availability);
            let _ = conn.release();
            return Ok(BackendAttemptResult::ArticleNotFound { backend_id });
        }

        let msg_id = request.message_id_value();
        let response = self
            .stream_successful_backend_response(
                conn,
                client_write,
                backend_id,
                ResponseStreamParams {
                    request,
                    msg_id: msg_id.as_ref(),
                    status_code,
                    first_chunk: &buffer[..cmd_response.bytes_read],
                },
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
            self.execute_backend_attempt(provider, backend_id, request)
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
                bytes_read = cmd_response.bytes_read,
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
                    bytes_read: cmd_response.bytes_read,
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
            bytes_read = cmd_response.bytes_read,
            "Backend attempt parsed first response successfully"
        );

        Ok(Some((conn, cmd_response, status_code, buffer)))
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
            bytes_read,
        } = ctx;
        let bytes_to_read = bytes_read.min(buffer.len());
        tracing::warn!(
            client = %self.client_addr,
            backend = ?backend_id,
            command_verb = ?request.verb(),
            bytes_read,
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

    async fn stream_successful_backend_response<W>(
        &self,
        mut conn: crate::pool::ConnectionGuard,
        client_write: &mut W,
        backend_id: BackendId,
        params: ResponseStreamParams<'_>,
    ) -> Result<RequestResponseMetadata, SessionError>
    where
        W: AsyncWrite + Unpin,
    {
        let is_multiline_body = params
            .request
            .expects_multiline_response(params.status_code);
        debug!(
            client = %self.client_addr,
            backend = backend_id.as_index(),
            command_verb = ?params.request.verb(),
            first_chunk_bytes = params.first_chunk.len(),
            status_code = params.status_code.as_u16(),
            is_multiline_body,
            "Streaming backend response to client"
        );
        let buffer_ctx = BufferContext {
            backend_id,
            buffer_pool: &self.buffer_pool,
        };
        let bytes_written = match self
            .stream_response_to_client(&mut conn, client_write, &buffer_ctx, params)
            .await
        {
            Ok(bytes) => bytes,
            Err(e) => return Err(self.handle_streaming_error(conn, backend_id, params.request, e)),
        };
        client_write
            .flush()
            .await
            .map_err(|e| SessionError::from(anyhow::Error::from(e)))?;

        debug!(
            client = %self.client_addr,
            backend = backend_id.as_index(),
            msg_id = ?params.msg_id,
            bytes_written,
            "Article streaming complete"
        );
        self.record_response_metrics(
            backend_id,
            params.request,
            params.status_code,
            params.request.request_wire_len().as_u64(),
            bytes_written,
        );
        self.release_or_retire_connection(conn, backend_id, params.request);

        Ok(RequestResponseMetadata::new(
            params.status_code,
            usize::try_from(bytes_written).unwrap_or(usize::MAX).into(),
        ))
    }

    fn handle_streaming_error(
        &self,
        conn: crate::pool::ConnectionGuard,
        backend_id: BackendId,
        request: &RequestContext,
        error: StreamingError,
    ) -> SessionError {
        if error.must_remove_connection() {
            warn!(
                client = %self.client_addr,
                backend = backend_id.as_index(),
                command_verb = ?request.verb(),
                error = %error,
                "Streaming error, removing connection from pool"
            );
            self.metrics.record_error(backend_id);
            self.metrics.user_error(self.username());
        } else if conn.has_leftover() {
            warn!(
                client = %self.client_addr,
                backend = backend_id.as_index(),
                command_verb = ?request.verb(),
                leftover_bytes = conn.leftover_len(),
                "Buffered direct-path response ended with trailing backend bytes; retiring connection"
            );
        } else {
            debug!(
                client = %self.client_addr,
                backend = backend_id.as_index(),
                command_verb = ?request.verb(),
                "Streaming error drained cleanly; releasing backend connection back to pool"
            );
            let _ = conn.release();
        }

        SessionError::from(error)
    }

    fn release_or_retire_connection(
        &self,
        conn: crate::pool::ConnectionGuard,
        backend_id: BackendId,
        request: &RequestContext,
    ) {
        if conn.has_leftover() {
            warn!(
                client = %self.client_addr,
                backend = backend_id.as_index(),
                command_verb = ?request.verb(),
                leftover_bytes = conn.leftover_len(),
                "Direct per-command response left trailing backend bytes; retiring connection"
            );
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
    ) -> Result<(
        crate::pool::ConnectionGuard,
        backend::BackendFirstResponse,
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
        let conn = provider.get_pooled_connection().await?;
        debug!(
            client = %self.client_addr,
            backend = backend_id.as_index(),
            command_verb = ?request.verb(),
            msg_id = ?request.message_id_value(),
            "Pool checkout succeeded for direct backend attempt"
        );
        let mut guard = crate::pool::ConnectionGuard::new(conn, provider.clone());
        let mut buffer = self.buffer_pool.acquire();

        debug!(
            client = %self.client_addr,
            backend = backend_id.as_index(),
            command_verb = ?request.verb(),
            msg_id = ?request.message_id_value(),
            "Sending request to backend and waiting for first response bytes"
        );
        let result = self
            .execute_and_get_first_chunk(&mut guard, backend_id, request, &mut buffer)
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

    /// Execute command on a connection and read first chunk.
    ///
    /// Takes `&mut ConnectionStream` (not a pool object) so it can be called
    /// by both the pool-checkout path and future pipeline workers.
    async fn execute_and_get_first_chunk(
        &self,
        conn: &mut crate::stream::ConnectionStream,
        backend_id: crate::types::BackendId,
        request: &RequestContext,
        buffer: &mut crate::pool::PooledBuffer,
    ) -> Result<(backend::BackendFirstResponse, Option<BackendTimings>)> {
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
        response.log_warnings(&buffer[..response.bytes_read], self.client_addr, backend_id);
        debug!(
            client = %self.client_addr,
            backend = backend_id.as_index(),
            command_verb = ?request.verb(),
            msg_id = ?request.message_id_value(),
            bytes_read = response.bytes_read,
            parsed_status = ?response.status_code(),
            "Backend returned first response bytes for direct attempt"
        );

        Ok((response, timings))
    }

    /// Stream response from backend to client and handle caching.
    ///
    /// Returns `StreamingError` so callers can decide the connection's pool fate
    /// without string/downcast inspection.
    async fn stream_response_to_client<W>(
        &self,
        pooled_conn: &mut deadpool::managed::Object<crate::pool::deadpool_connection::TcpManager>,
        client_write: &mut W,
        ctx: &BufferContext<'_>,
        params: ResponseStreamParams<'_>,
    ) -> Result<u64, StreamingError>
    where
        W: AsyncWrite + Unpin,
    {
        let code = params.status_code.as_u16();
        let is_multiline_body = params
            .request
            .expects_multiline_response(params.status_code);

        let cache_action = determine_cache_action_for_request(
            params.request,
            params.status_code,
            self.cache_articles,
            params.msg_id.is_some(),
        );

        debug!(
            "stream_response_to_client: code={}, is_multiline_body={}, cache_articles={}, has_msg_id={}, action={:?}",
            code,
            is_multiline_body,
            self.cache_articles,
            params.msg_id.is_some(),
            cache_action
        );

        if is_multiline_body {
            // Permanent policy: keep direct multiline replies fully buffered so
            // packed trailing bytes are stashed deterministically before the
            // connection can be reused or retired.
            return self
                .deliver_buffered_multiline_response(
                    pooled_conn,
                    client_write,
                    ctx,
                    params,
                    cache_action,
                )
                .await;
        }

        match cache_action {
            CacheAction::TrackStat => {
                let bytes = self
                    .write_single_line_response(pooled_conn, client_write, params.first_chunk)
                    .await?;
                self.maybe_cache_upsert_buffer(
                    params.msg_id,
                    crate::cache::CacheIngestResponse::from(b"223\r\n".as_slice()),
                    ctx.backend_id,
                );
                Ok(bytes)
            }
            _ => {
                self.write_single_line_response(pooled_conn, client_write, params.first_chunk)
                    .await
            }
        }
    }

    async fn deliver_buffered_multiline_response<W>(
        &self,
        pooled_conn: &mut deadpool::managed::Object<crate::pool::deadpool_connection::TcpManager>,
        client_write: &mut W,
        ctx: &BufferContext<'_>,
        params: ResponseStreamParams<'_>,
        cache_action: CacheAction,
    ) -> Result<u64, StreamingError>
    where
        W: AsyncWrite + Unpin,
    {
        let captured = crate::session::response_buffer::buffer_multiline_response(
            pooled_conn,
            params.first_chunk,
            ctx,
        )
        .await?;
        captured
            .write_all_to(client_write)
            .await
            .map_err(classify_buffered_response_write_err)?;
        if let Some(msg_id_ref) = params.msg_id {
            debug!(
                "Client {} caching full article for {} ({} bytes captured)",
                self.client_addr,
                msg_id_ref,
                captured.len()
            );
        }
        let captured_len = captured.len();
        match cache_action {
            CacheAction::CaptureArticle => {
                self.maybe_cache_upsert_buffer(params.msg_id, captured.into(), ctx.backend_id);
            }
            CacheAction::TrackAvailability => {
                if let Some(msg_id) = params.msg_id
                    && !params
                        .request
                        .cache_records_backend_has_article(ctx.backend_id)
                {
                    self.spawn_cache_upsert_availability(
                        msg_id,
                        params.status_code,
                        ctx.backend_id,
                        self.tier_for_backend(ctx.backend_id),
                    );
                }
            }
            CacheAction::TrackStat | CacheAction::None => {}
        }
        Ok(captured_len as u64)
    }

    async fn write_single_line_response<W>(
        &self,
        pooled_conn: &mut deadpool::managed::Object<crate::pool::deadpool_connection::TcpManager>,
        client_write: &mut W,
        response: &[u8],
    ) -> Result<u64, StreamingError>
    where
        W: AsyncWrite + Unpin,
    {
        let Some(end) = crate::session::backend::status_line_end(response) else {
            return Err(StreamingError::Io(anyhow::anyhow!(
                "Prefetched single-line response missing status line terminator"
            )));
        };
        if end < response.len() {
            let leftover = &response[end..];
            warn!(
                client = %self.client_addr,
                leftover_bytes = leftover.len(),
                "Prefetched single-line response included packed trailing bytes; stashing leftover so the backend connection is retired"
            );
            pooled_conn
                .stash_leftover(leftover)
                .map_err(StreamingError::Io)?;
        }

        client_write
            .write_all(&response[..end])
            .await
            .map_err(classify_buffered_response_write_err)?;
        Ok(end as u64)
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
    use super::classify_buffered_response_write_err;
    use crate::protocol::{RequestContext, RequestResponseMetadata, ResponseWireLen, StatusCode};
    use crate::types::BackendId;

    fn request_context(line: &[u8]) -> RequestContext {
        RequestContext::parse(line).expect("valid request line")
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
        let classified = classify_buffered_response_write_err(err);
        assert!(matches!(
            classified,
            crate::session::response_buffer::StreamingError::ClientDisconnect(_)
        ));
    }

    #[test]
    fn buffered_response_abort_is_terminal_client_disconnect() {
        let err = std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "aborted");
        let classified = classify_buffered_response_write_err(err);
        assert!(matches!(
            classified,
            crate::session::response_buffer::StreamingError::ClientDisconnect(_)
        ));
    }
}
