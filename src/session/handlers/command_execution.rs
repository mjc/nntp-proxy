//! Backend command execution and response streaming
//!
//! Handles executing commands on individual backends, including connection retry,
//! response validation, and streaming multiline responses to clients.

use crate::protocol::{RequestContext, RequestResponseMetadata};
use crate::router::{BackendSelector, CommandGuard};
use crate::session::SessionError;
use crate::session::retry::retry_once;
use crate::session::routing::{
    CacheAction, MetricsAction, determine_cache_action_for_request, determine_metrics_action,
};
use crate::session::streaming::StreamingError;
use crate::session::{ClientSession, backend, streaming};
use crate::types::{BackendId, BackendToClientBytes, ClientToBackendBytes};
use anyhow::Result;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tracing::{debug, warn};

/// Result of attempting to execute a command on a backend
pub(super) enum BackendAttemptResult {
    /// Article found - response streamed successfully
    Success {
        backend_id: BackendId,
        response: RequestResponseMetadata,
    },
    /// Article not found (430) - try next backend
    /// Note: The 430 response is read and drained, just not stored
    ArticleNotFound { backend_id: BackendId },
    /// Backend unavailable or error - try next backend
    BackendUnavailable,
}

impl BackendAttemptResult {
    #[must_use]
    const fn success(backend_id: BackendId, response: RequestResponseMetadata) -> Self {
        Self::Success {
            backend_id,
            response,
        }
    }
}

/// Mutable state for an article backend attempt loop
///
/// Groups the mutable parameters that track retry state across
/// multiple `try_backend_for_article` calls.
pub(super) struct ArticleAttemptState<'a> {
    pub availability: &'a mut crate::cache::ArticleAvailability,
    pub buffer: &'a mut crate::pool::PooledBuffer,
    pub client_to_backend_bytes: &'a mut ClientToBackendBytes,
}

/// Parameters describing the response to stream to the client
struct ResponseStreamParams<'a> {
    request: &'a RequestContext,
    msg_id: Option<&'a crate::types::MessageId<'a>>,
    response_code: &'a crate::protocol::NntpResponse,
    is_multiline: bool,
    first_chunk: &'a [u8],
}

/// Any client write failure after the full backend response is already buffered is terminal.
///
/// The backend connection is already clean at this point, but the client may have received a
/// partial prefix of the response. Retrying another backend on the same client socket would
/// splice responses together and corrupt NNTP framing.
fn classify_buffered_response_write_err(e: std::io::Error) -> StreamingError {
    StreamingError::ClientDisconnect(e)
}

impl ClientSession {
    /// Try executing command on next available backend
    ///
    /// If the pooled connection is stale (connection error), automatically retries
    /// with a fresh connection before returning an error.
    pub(super) async fn try_backend_for_article(
        &self,
        router: &Arc<BackendSelector>,
        request: &RequestContext,
        msg_id: Option<&crate::types::MessageId<'_>>,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        state: &mut ArticleAttemptState<'_>,
    ) -> Result<BackendAttemptResult, SessionError> {
        // Select least-loaded available backend
        let backend_id =
            router.route_with_availability(self.client_id, Some(state.availability))?;

        // RAII guard ensures complete_command is called on all exit paths (clone Arc here)
        let guard = CommandGuard::new(router.clone(), backend_id);

        // Get connection provider
        let Some(provider) = router.backend_provider(backend_id) else {
            state.availability.record_missing(backend_id);
            // guard drops here → complete_command called automatically
            return Ok(BackendAttemptResult::BackendUnavailable);
        };

        // Retry once on backend error (fresh connection on second attempt)
        let (conn, cmd_response, ttfb, send, recv) = retry_once!(
            self.execute_backend_attempt(provider, backend_id, request, state.buffer)
                .await,
            client = self.client_addr,
            backend = backend_id.as_index()
        )
        .map_err(SessionError::Backend)?;

        self.record_timing_metrics(backend_id, ttfb, send, recv);
        *state.client_to_backend_bytes = state.client_to_backend_bytes.add(request.wire_len());

        // Reject invalid responses - never forward garbage to client
        if cmd_response.response == crate::protocol::NntpResponse::Invalid {
            // Safely clamp buffer slice to prevent panic on out-of-bounds bytes_read
            let bytes_to_read = cmd_response.bytes_read.min(state.buffer.len());

            tracing::warn!(
                client = %self.client_addr,
                backend = ?backend_id,
                command_verb = %String::from_utf8_lossy(request.verb()),
                bytes_read = cmd_response.bytes_read,
                first_bytes_hex = %crate::session::backend::format_hex_preview(
                    &state.buffer[..bytes_to_read], 256
                ),
                first_bytes_utf8 = %String::from_utf8_lossy(
                    &state.buffer[..bytes_to_read.min(256)]
                ),
                "Backend returned invalid/unparseable response, attempting to salvage connection"
            );
            // Mark backend as unavailable for this article so we try next one
            state.availability.record_missing(backend_id);

            // Try to salvage connection with DATE health check
            // Spawn in background so client can retry immediately
            let cmd_for_log = String::from_utf8_lossy(request.verb()).into_owned();
            let provider_for_salvage = provider.clone();
            let conn_for_salvage = conn.release(); // hand off; salvage decides pool fate
            tokio::spawn(async move {
                tracing::debug!(
                    backend = ?backend_id,
                    command_verb = %cmd_for_log,
                    "Attempting to salvage connection after Invalid response"
                );
                crate::pool::salvage_with_health_check(conn_for_salvage, provider_for_salvage)
                    .await;
            });

            // guard drops here → complete_command called automatically
            return Ok(BackendAttemptResult::BackendUnavailable);
        }

        // Handle 430 - article not found
        // Note: response is already read into buffer, keeping connection clean
        if cmd_response.response.is_430() {
            self.handle_430_availability(backend_id, state.availability);
            let _ = conn.release(); // connection is healthy; return to pool
            return Ok(BackendAttemptResult::ArticleNotFound { backend_id });
        }

        // Success - stream response
        debug!(
            client = %self.client_addr,
            backend = backend_id.as_index(),
            command_verb = %String::from_utf8_lossy(request.verb()),
            first_chunk_bytes = cmd_response.bytes_read,
            response = ?cmd_response.response,
            is_multiline = cmd_response.is_multiline,
            "Streaming backend response to client"
        );
        let mut conn = conn;
        let stream_ctx = streaming::StreamContext {
            client_addr: self.client_addr,
            backend_id,
            buffer_pool: &self.buffer_pool,
        };
        let bytes_written = match self
            .stream_response_to_client(
                &mut conn,
                client_write,
                &stream_ctx,
                ResponseStreamParams {
                    request,
                    msg_id,
                    response_code: &cmd_response.response,
                    is_multiline: cmd_response.is_multiline,
                    first_chunk: &state.buffer[..cmd_response.bytes_read],
                },
            )
            .await
        {
            Ok(bytes) => bytes,
            Err(e) => {
                // guard drops here → complete_command called automatically
                // (prevents TUI in-flight count drift on streaming errors)
                if e.must_remove_connection() {
                    // Backend error or dirty disconnect (drain failed) —
                    // connection in unknown state; ConnectionGuard removes from pool on drop.
                    warn!(
                        client = %self.client_addr,
                        backend = backend_id.as_index(),
                        command_verb = %String::from_utf8_lossy(request.verb()),
                        error = %e,
                        "Streaming error, removing connection from pool"
                    );
                    self.metrics.record_error(backend_id);
                    self.metrics.user_error(self.username());
                } else {
                    // Client disconnect after a fully buffered response keeps the backend
                    // clean, but unexpected trailing bytes still make this pooled borrow
                    // unsafe to reuse across sessions.
                    if conn.has_leftover() {
                        warn!(
                            client = %self.client_addr,
                            backend = backend_id.as_index(),
                            command_verb = %String::from_utf8_lossy(request.verb()),
                            leftover_bytes = conn.leftover_len(),
                            "Buffered direct-path response ended with trailing backend bytes; retiring connection"
                        );
                    } else {
                        let _ = conn.release();
                    }
                }
                // SessionError::from(StreamingError) preserves ClientDisconnect signal.
                return Err(SessionError::from(e));
            }
        };

        debug!(
            client = %self.client_addr,
            backend = backend_id.as_index(),
            msg_id = ?msg_id,
            bytes_written = bytes_written,
            "Article streaming complete"
        );
        self.record_response_metrics(
            backend_id,
            cmd_response.response,
            cmd_response.is_multiline,
            request.wire_len() as u64,
            bytes_written,
        );

        // Explicitly complete the guard on the success path
        guard.complete();
        if conn.has_leftover() {
            warn!(
                client = %self.client_addr,
                backend = backend_id.as_index(),
                command_verb = %String::from_utf8_lossy(request.verb()),
                leftover_bytes = conn.leftover_len(),
                "Direct per-command response left trailing backend bytes; retiring connection"
            );
        } else {
            let _ = conn.release(); // streaming completed; connection healthy, return to pool
        }

        Ok(BackendAttemptResult::success(
            backend_id,
            RequestResponseMetadata::new(
                cmd_response
                    .status_code()
                    .expect("valid streamed response has status code"),
                (bytes_written as usize).into(),
            ),
        ))
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
        buffer: &mut crate::pool::PooledBuffer,
    ) -> Result<(
        crate::pool::ConnectionGuard,
        backend::BackendResponse,
        u64,
        u64,
        u64,
    )> {
        let conn = provider.get_pooled_connection().await?;
        let mut guard = crate::pool::ConnectionGuard::new(conn, provider.clone());

        let result = self
            .execute_and_get_first_chunk(&mut guard, backend_id, request, buffer)
            .await;

        match result {
            Ok((cmd_response, ttfb, send, recv)) => Ok((guard, cmd_response, ttfb, send, recv)),
            Err(e) => Err(e), // guard drops → remove_with_cooldown
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
    ) -> Result<(backend::BackendResponse, u64, u64, u64)> {
        self.metrics.record_command(backend_id);
        self.metrics.user_command(self.username());

        let (response, ttfb, send, recv) =
            backend::send_request_timed(conn, request, buffer).await?;

        // Log any validation warnings
        response.log_warnings(&buffer[..response.bytes_read], self.client_addr, backend_id);

        Ok((response, ttfb, send, recv))
    }

    /// Stream response from backend to client and handle caching.
    ///
    /// Returns `StreamingError` so callers can decide the connection's pool fate
    /// without string/downcast inspection.
    async fn stream_response_to_client(
        &self,
        pooled_conn: &mut deadpool::managed::Object<crate::pool::deadpool_connection::TcpManager>,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        ctx: &streaming::StreamContext<'_>,
        params: ResponseStreamParams<'_>,
    ) -> Result<u64, StreamingError> {
        // SAFETY: Caller must validate response before calling this function.
        // An invalid response (code 0) should never reach here.
        let status_code = params
            .response_code
            .status_code()
            .ok_or_else(|| {
                // This should never happen - caller should reject Invalid responses
                tracing::error!("BUG: stream_response_to_client called with Invalid response");
                anyhow::anyhow!("Cannot stream invalid response")
            })
            .map_err(StreamingError::Io)?;
        let code = status_code.as_u16();

        let cache_action = determine_cache_action_for_request(
            params.request,
            code,
            params.is_multiline,
            self.cache_articles,
            params.msg_id.is_some(),
        );

        debug!(
            "stream_response_to_client: code={}, is_multiline={}, cache_articles={}, has_msg_id={}, action={:?}",
            code,
            params.is_multiline,
            self.cache_articles,
            params.msg_id.is_some(),
            cache_action
        );

        match (params.is_multiline, cache_action) {
            (true, CacheAction::CaptureArticle) => {
                let captured =
                    streaming::buffer_multiline_response(pooled_conn, params.first_chunk, ctx)
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
                self.maybe_cache_upsert_buffer(params.msg_id, captured.into(), ctx.backend_id);
                Ok(captured_len as u64)
            }
            (true, CacheAction::TrackAvailability) => {
                // Availability-only mode should not buffer the article body.
                // Keep first-byte latency and memory bounded by streaming directly,
                // then cache only the status-line stub after the terminator is seen.
                let stub = crate::cache::extract_status_line(params.first_chunk);
                let bytes = streaming::stream_multiline_response(
                    &mut **pooled_conn,
                    client_write,
                    params.first_chunk,
                    ctx,
                )
                .await?;
                self.maybe_cache_upsert_buffer(params.msg_id, stub.into(), ctx.backend_id);
                Ok(bytes)
            }
            (true, _) => {
                streaming::stream_multiline_response(
                    &mut **pooled_conn,
                    client_write,
                    params.first_chunk,
                    ctx,
                )
                .await
            }
            (false, CacheAction::TrackStat) => {
                // Single-line: backend already has complete response in first_chunk,
                // so any write failure is a client-side error (backend is always clean here).
                client_write
                    .write_all(params.first_chunk)
                    .await
                    .map_err(classify_buffered_response_write_err)?;
                self.maybe_cache_upsert(params.msg_id, b"223\r\n", ctx.backend_id);
                Ok(params.first_chunk.len() as u64)
            }
            (false, _) => {
                // Single-line, no caching.
                // Backend is clean regardless of outcome — response was fully read.
                client_write
                    .write_all(params.first_chunk)
                    .await
                    .map_err(classify_buffered_response_write_err)?;
                Ok(params.first_chunk.len() as u64)
            }
        }
    }

    /// Handle backend error (metrics and cleanup)
    /// Send standardized 430 response to client
    pub(super) async fn send_430_to_client(
        &self,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        backend_to_client_bytes: &mut BackendToClientBytes,
    ) -> Result<()> {
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

    /// Cache article data if message ID is present
    #[inline]
    fn maybe_cache_upsert(
        &self,
        msg_id: Option<&crate::types::MessageId<'_>>,
        data: &[u8],
        backend_id: BackendId,
    ) {
        if let Some(msg_id_ref) = msg_id {
            let tier = self.tier_for_backend(backend_id);
            self.spawn_cache_upsert(msg_id_ref, data, backend_id, tier);
        }
    }
    #[inline]
    fn maybe_cache_upsert_buffer(
        &self,
        msg_id: Option<&crate::types::MessageId<'_>>,
        data: crate::cache::CacheBuffer,
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
        response_code: crate::protocol::NntpResponse,
        is_multiline: bool,
        cmd_bytes: u64,
        resp_bytes: u64,
    ) {
        use crate::types::MetricsBytes;

        if let Some(code) = response_code.status_code() {
            match determine_metrics_action(code.as_u16(), is_multiline) {
                MetricsAction::Error4xx => self.metrics.record_error_4xx(backend_id),
                MetricsAction::Error5xx => self.metrics.record_error_5xx(backend_id),
                MetricsAction::Article => self.metrics.record_article(backend_id, resp_bytes),
                MetricsAction::None => {}
            }
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
    use crate::protocol::{RequestResponseMetadata, ResponseWireLen, StatusCode};
    use crate::types::BackendId;

    #[test]
    fn backend_attempt_success_carries_typed_response_metadata() {
        let backend_id = BackendId::from_index(1);
        let response = RequestResponseMetadata::new(StatusCode::new(220), ResponseWireLen::new(42));
        let result = BackendAttemptResult::success(backend_id, response);

        match result {
            BackendAttemptResult::Success {
                backend_id: actual_backend,
                response: actual_response,
            } => {
                assert_eq!(actual_backend, backend_id);
                assert_eq!(actual_response, response);
            }
            _ => panic!("expected success"),
        }
    }

    #[test]
    fn buffered_response_timeout_is_terminal_client_disconnect() {
        let err = std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out");
        let classified = classify_buffered_response_write_err(err);
        assert!(matches!(
            classified,
            crate::session::streaming::StreamingError::ClientDisconnect(_)
        ));
    }

    #[test]
    fn buffered_response_abort_is_terminal_client_disconnect() {
        let err = std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "aborted");
        let classified = classify_buffered_response_write_err(err);
        assert!(matches!(
            classified,
            crate::session::streaming::StreamingError::ClientDisconnect(_)
        ));
    }
}
