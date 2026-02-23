//! Backend command execution and response streaming
//!
//! Handles executing commands on individual backends, including connection retry,
//! response validation, and streaming multiline responses to clients.

use crate::is_client_disconnect_error;
use crate::router::{BackendSelector, CommandGuard};
use crate::session::retry::retry_once_on_stale;
use crate::session::routing::{
    CacheAction, MetricsAction, determine_cache_action, determine_metrics_action,
};
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
        bytes_written: u64,
    },
    /// Article not found (430) - try next backend
    /// Note: The 430 response is read and drained, just not stored
    ArticleNotFound { backend_id: BackendId },
    /// Backend unavailable or error - try next backend
    BackendUnavailable,
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
    command: &'a str,
    msg_id: Option<&'a crate::types::MessageId<'a>>,
    response_code: &'a crate::protocol::NntpResponse,
    is_multiline: bool,
    first_chunk: &'a [u8],
}

impl ClientSession {
    /// Try executing command on next available backend
    ///
    /// If the pooled connection is stale (connection error), automatically retries
    /// with a fresh connection before returning an error.
    pub(super) async fn try_backend_for_article(
        &self,
        router: &Arc<BackendSelector>,
        command: &str,
        msg_id: Option<&crate::types::MessageId<'_>>,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        state: &mut ArticleAttemptState<'_>,
    ) -> Result<BackendAttemptResult> {
        // Select least-loaded available backend
        let backend_id = router.route_command_with_availability(
            self.client_id,
            command,
            Some(state.availability),
        )?;

        // RAII guard ensures complete_command is called on all exit paths (clone Arc here)
        let guard = CommandGuard::new(router.clone(), backend_id);

        // Get connection provider
        let Some(provider) = router.backend_provider(backend_id) else {
            state.availability.record_missing(backend_id);
            // guard drops here → complete_command called automatically
            return Ok(BackendAttemptResult::BackendUnavailable);
        };

        // Retry once on stale connection (fresh connection on second attempt)
        let (conn, cmd_response, ttfb, send, recv) = retry_once_on_stale!(
            self.execute_backend_attempt(provider, backend_id, command, state.buffer)
                .await,
            client = self.client_addr,
            backend = backend_id.as_index()
        )?;

        self.record_timing_metrics(backend_id, ttfb, send, recv);
        *state.client_to_backend_bytes = state.client_to_backend_bytes.add(command.len());

        // Reject invalid responses - never forward garbage to client
        if cmd_response.response == crate::protocol::NntpResponse::Invalid {
            // Extract command verb for logging (avoid logging credentials in AUTHINFO/etc)
            let cmd_verb = command
                .split_whitespace()
                .next()
                .unwrap_or("UNKNOWN")
                .to_uppercase();

            // Safely clamp buffer slice to prevent panic on out-of-bounds bytes_read
            let bytes_to_read = cmd_response.bytes_read.min(state.buffer.len());

            tracing::warn!(
                client = %self.client_addr,
                backend = ?backend_id,
                command_verb = %cmd_verb,
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
            let cmd_for_log = command.trim().to_string();
            let provider_for_salvage = provider.clone();
            tokio::spawn(async move {
                tracing::debug!(
                    backend = ?backend_id,
                    command = %cmd_for_log,
                    "Attempting to salvage connection after Invalid response"
                );
                crate::pool::salvage_with_health_check(conn, provider_for_salvage).await;
            });

            // guard drops here → complete_command called automatically
            return Ok(BackendAttemptResult::BackendUnavailable);
        }

        // Handle 430 - article not found
        // Note: response is already read into buffer, keeping connection clean
        if cmd_response.is_430() {
            self.handle_430_availability(backend_id, state.availability);
            // guard drops here → complete_command called automatically
            return Ok(BackendAttemptResult::ArticleNotFound { backend_id });
        }

        // Success - stream response
        let mut conn = conn;
        let stream_ctx = streaming::StreamContext {
            client_addr: self.client_addr,
            backend_id,
            buffer_pool: &self.buffer_pool,
        };
        let bytes_written = self
            .stream_response_to_client(
                &mut conn,
                client_write,
                &stream_ctx,
                ResponseStreamParams {
                    command,
                    msg_id,
                    response_code: &cmd_response.response,
                    is_multiline: cmd_response.is_multiline,
                    first_chunk: &state.buffer[..cmd_response.bytes_read],
                },
            )
            .await
            .inspect_err(|e| {
                // guard drops here → complete_command called automatically
                // (prevents TUI in-flight count drift on streaming errors)

                // Only mark as backend error metrics if it's NOT a client disconnect.
                // Client disconnects are normal behavior and shouldn't penalize backends.
                if !is_client_disconnect_error(e) {
                    warn!(
                        client = %self.client_addr,
                        backend = backend_id.as_index(),
                        command = %command.trim(),
                        error = %e,
                        "Streaming error, removing connection from pool"
                    );
                    self.metrics.record_error(backend_id);
                    self.metrics.user_error(self.username());
                }
                provider.remove_with_cooldown(conn);
            })?;

        self.record_response_metrics(
            backend_id,
            &cmd_response.response,
            cmd_response.is_multiline,
            command.len() as u64,
            bytes_written,
        );

        // Explicitly complete the guard on the success path
        guard.complete();

        Ok(BackendAttemptResult::Success {
            backend_id,
            bytes_written,
        })
    }

    /// Execute a single backend attempt - get connection and execute command
    ///
    /// Returns the connection and response data on success.
    /// On error, the connection is removed from pool before returning.
    async fn execute_backend_attempt(
        &self,
        provider: &crate::pool::DeadpoolConnectionProvider,
        backend_id: crate::types::BackendId,
        command: &str,
        buffer: &mut crate::pool::PooledBuffer,
    ) -> Result<(
        deadpool::managed::Object<crate::pool::deadpool_connection::TcpManager>,
        backend::CommandResponse,
        u64,
        u64,
        u64,
    )> {
        let mut conn = provider.get_pooled_connection().await?;

        let result = self
            .execute_and_get_first_chunk(&mut conn, backend_id, command, buffer)
            .await;

        match result {
            Ok((cmd_response, ttfb, send, recv)) => Ok((conn, cmd_response, ttfb, send, recv)),
            Err(e) => {
                provider.remove_with_cooldown(conn);
                Err(e)
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
        command: &str,
        buffer: &mut crate::pool::PooledBuffer,
    ) -> Result<(backend::CommandResponse, u64, u64, u64)> {
        self.metrics.record_command(backend_id);
        self.metrics.user_command(self.username());

        let (response, ttfb, send, recv) =
            backend::send_command_timed(conn, command, buffer).await?;

        // Log any validation warnings
        response.log_warnings(&buffer[..response.bytes_read], self.client_addr, backend_id);

        Ok((response, ttfb, send, recv))
    }

    /// Stream response from backend to client and handle caching
    async fn stream_response_to_client(
        &self,
        pooled_conn: &mut deadpool::managed::Object<crate::pool::deadpool_connection::TcpManager>,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        ctx: &streaming::StreamContext<'_>,
        params: ResponseStreamParams<'_>,
    ) -> Result<u64> {
        // SAFETY: Caller must validate response before calling this function.
        // An invalid response (code 0) should never reach here.
        let status_code = params.response_code.status_code().ok_or_else(|| {
            // This should never happen - caller should reject Invalid responses
            tracing::error!("BUG: stream_response_to_client called with Invalid response");
            anyhow::anyhow!("Cannot stream invalid response")
        })?;
        let code = status_code.as_u16();

        let cache_action = determine_cache_action(
            params.command,
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
                let mut captured = self.buffer_pool.acquire_capture().await;
                let bytes = streaming::stream_and_capture_multiline_response(
                    &mut **pooled_conn,
                    client_write,
                    params.first_chunk,
                    ctx,
                    &mut captured,
                )
                .await?;

                if let Some(msg_id_ref) = params.msg_id {
                    debug!(
                        "Client {} caching full article for {} ({} bytes captured)",
                        self.client_addr,
                        msg_id_ref,
                        captured.len()
                    );
                }
                self.maybe_cache_upsert(params.msg_id, &captured, ctx.backend_id);
                Ok(bytes)
            }
            (true, CacheAction::TrackAvailability) => {
                let bytes = streaming::stream_multiline_response(
                    &mut **pooled_conn,
                    client_write,
                    params.first_chunk,
                    ctx,
                )
                .await?;

                // Extract first status line (~30-80 bytes) instead of copying
                // full first_chunk (8-64KB). The cache only needs the status
                // code to build an availability stub.
                // SmallVec keeps small status lines on stack, avoiding heap allocation
                // until the spawn boundary where .to_vec() happens inside spawn_cache_upsert.
                let stub = crate::cache::extract_status_line(params.first_chunk);
                self.maybe_cache_upsert(params.msg_id, &stub, ctx.backend_id);
                Ok(bytes)
            }
            (true, _) => {
                // Multiline but no caching
                streaming::stream_multiline_response(
                    &mut **pooled_conn,
                    client_write,
                    params.first_chunk,
                    ctx,
                )
                .await
            }
            (false, CacheAction::TrackStat) => {
                client_write.write_all(params.first_chunk).await?;
                self.maybe_cache_upsert(params.msg_id, b"223\r\n", ctx.backend_id);
                Ok(params.first_chunk.len() as u64)
            }
            (false, _) => {
                // Single-line, no caching
                client_write.write_all(params.first_chunk).await?;
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

    /// Record response metrics (errors, article sizes, command execution)
    fn record_response_metrics(
        &self,
        backend_id: crate::types::BackendId,
        response_code: &crate::protocol::NntpResponse,
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
