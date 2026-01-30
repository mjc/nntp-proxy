//! Backend command execution and response streaming
//!
//! Handles executing commands on individual backends, including connection retry,
//! response validation, and streaming multiline responses to clients.

use crate::router::{BackendSelector, CommandGuard};
use crate::session::routing::{
    CacheAction, MetricsAction, determine_cache_action, determine_metrics_action,
    is_430_status_code,
};
use crate::session::{ClientSession, backend, streaming};
use crate::types::{BackendId, BackendToClientBytes, ClientToBackendBytes};
use anyhow::Result;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tracing::debug;

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

impl ClientSession {
    /// Try executing command on next available backend
    ///
    /// If the pooled connection is stale (connection error), automatically retries
    /// with a fresh connection before returning an error.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn try_backend_for_article(
        &self,
        router: Arc<BackendSelector>,
        command: &str,
        msg_id: Option<&crate::types::MessageId<'_>>,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        availability: &mut crate::cache::ArticleAvailability,
        buffer: &mut crate::pool::PooledBuffer,
        client_to_backend_bytes: &mut ClientToBackendBytes,
    ) -> Result<BackendAttemptResult> {
        // Select least-loaded available backend
        let backend_id =
            router.route_command_with_availability(self.client_id, command, Some(availability))?;

        // RAII guard ensures complete_command is called on all exit paths
        let guard = CommandGuard::new(router.clone(), backend_id);

        // Get connection provider
        let Some(provider) = router.backend_provider(backend_id) else {
            availability.record_missing(backend_id);
            // guard drops here → complete_command called automatically
            return Ok(BackendAttemptResult::BackendUnavailable);
        };

        // Functional retry: try first attempt, on connection error retry once with fresh connection
        let attempt_result = self
            .execute_backend_attempt(provider, backend_id, command, buffer)
            .await;

        let (conn, n, response_code, is_multiline, ttfb, send, recv) = match attempt_result {
            Ok(result) => result,
            Err(first_error) => {
                // First attempt failed - retry once with fresh connection
                debug!(
                    "Client {} stale connection to backend {}, retrying with fresh connection",
                    self.client_addr,
                    backend_id.as_index()
                );

                self.execute_backend_attempt(provider, backend_id, command, buffer)
                    .await
                    .map_err(|_| first_error)? // On retry failure, guard drops → complete_command
            }
        };

        self.record_timing_metrics(backend_id, ttfb, send, recv);
        *client_to_backend_bytes = client_to_backend_bytes.add(command.len());

        // Reject invalid responses - never forward garbage to client
        if response_code == crate::protocol::NntpResponse::Invalid {
            tracing::error!(
                backend_id = backend_id.as_index(),
                first_bytes = ?&buffer[..n.min(64)],
                "Backend returned invalid/unparseable response, rejecting"
            );
            // Mark backend as unavailable for this article so we try next one
            availability.record_missing(backend_id);
            // guard drops here → complete_command called automatically
            crate::pool::remove_from_pool(conn);
            return Ok(BackendAttemptResult::BackendUnavailable);
        }

        // Handle 430 - article not found
        // Note: response is already read into buffer, keeping connection clean
        if self.is_430_response(&response_code) {
            self.handle_430_availability(backend_id, availability);
            // guard drops here → complete_command called automatically
            return Ok(BackendAttemptResult::ArticleNotFound { backend_id });
        }

        // Success - stream response
        let mut conn = conn;
        let bytes_written = match self
            .stream_response_to_client(
                &mut conn,
                client_write,
                backend_id,
                command,
                msg_id,
                &response_code,
                is_multiline,
                &buffer[..n],
                n,
            )
            .await
        {
            Ok(bytes) => bytes,
            Err(e) => {
                // guard drops here → complete_command called automatically
                // (prevents TUI in-flight count drift on streaming errors)

                // Only mark as backend error metrics if it's NOT a client disconnect.
                // Client disconnects are normal behavior and shouldn't penalize backends.
                if !crate::session::error_classification::ErrorClassifier::is_client_disconnect(&e)
                {
                    self.metrics.record_error(backend_id);
                    self.metrics.user_error(self.username().as_deref());
                }
                crate::pool::remove_from_pool(conn);
                return Err(e);
            }
        };

        self.record_response_metrics(
            backend_id,
            &response_code,
            is_multiline,
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
        usize,
        crate::protocol::NntpResponse,
        bool,
        u64,
        u64,
        u64,
    )> {
        let mut conn = provider.get_pooled_connection().await?;

        match self
            .execute_and_get_first_chunk(&mut conn, backend_id, command, buffer)
            .await
        {
            Ok((n, response_code, is_multiline, ttfb, send, recv)) => {
                Ok((conn, n, response_code, is_multiline, ttfb, send, recv))
            }
            Err(e) => {
                crate::pool::remove_from_pool(conn);
                Err(e)
            }
        }
    }

    /// Execute command on backend and read first chunk
    async fn execute_and_get_first_chunk(
        &self,
        pooled_conn: &mut deadpool::managed::Object<crate::pool::deadpool_connection::TcpManager>,
        backend_id: crate::types::BackendId,
        command: &str,
        buffer: &mut crate::pool::PooledBuffer,
    ) -> Result<(usize, crate::protocol::NntpResponse, bool, u64, u64, u64)> {
        self.metrics.record_command(backend_id);
        self.metrics.user_command(self.username().as_deref());

        let (response, ttfb, send, recv) =
            backend::send_command_timed(&mut **pooled_conn, command, buffer).await?;

        // Log any validation warnings
        backend::log_warnings(
            &response.warnings,
            &buffer[..response.bytes_read],
            response.bytes_read,
            self.client_addr,
            backend_id,
        );

        Ok((
            response.bytes_read,
            response.response,
            response.is_multiline,
            ttfb,
            send,
            recv,
        ))
    }

    /// Check if response is 430 (article not found)
    pub(super) fn is_430_response(&self, response_code: &crate::protocol::NntpResponse) -> bool {
        response_code
            .status_code()
            .is_some_and(|code| is_430_status_code(code.as_u16()))
    }

    /// Stream response from backend to client and handle caching
    #[allow(clippy::too_many_arguments)]
    async fn stream_response_to_client(
        &self,
        pooled_conn: &mut deadpool::managed::Object<crate::pool::deadpool_connection::TcpManager>,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        backend_id: crate::types::BackendId,
        command: &str,
        msg_id: Option<&crate::types::MessageId<'_>>,
        response_code: &crate::protocol::NntpResponse,
        is_multiline: bool,
        first_chunk: &[u8],
        first_chunk_size: usize,
    ) -> Result<u64> {
        // SAFETY: Caller must validate response before calling this function.
        // An invalid response (code 0) should never reach here.
        let Some(status_code) = response_code.status_code() else {
            // This should never happen - caller should reject Invalid responses
            tracing::error!("BUG: stream_response_to_client called with Invalid response");
            anyhow::bail!("Cannot stream invalid response");
        };
        let code = status_code.as_u16();

        let cache_action = determine_cache_action(
            command,
            code,
            is_multiline,
            self.cache_articles,
            msg_id.is_some(),
        );

        debug!(
            "stream_response_to_client: code={}, is_multiline={}, cache_articles={}, has_msg_id={}, action={:?}",
            code,
            is_multiline,
            self.cache_articles,
            msg_id.is_some(),
            cache_action
        );

        match (is_multiline, cache_action) {
            (true, CacheAction::CaptureArticle) => {
                let mut captured = Vec::with_capacity(first_chunk.len() * 2);
                let bytes = streaming::stream_and_capture_multiline_response(
                    &mut **pooled_conn,
                    client_write,
                    first_chunk,
                    first_chunk_size,
                    self.client_addr,
                    backend_id,
                    &self.buffer_pool,
                    &mut captured,
                )
                .await?;

                if let Some(msg_id_ref) = msg_id {
                    debug!(
                        "Client {} caching full article for {} ({} bytes captured)",
                        self.client_addr,
                        msg_id_ref,
                        captured.len()
                    );
                    let tier = self.tier_for_backend(backend_id);
                    self.spawn_cache_upsert(msg_id_ref, captured, backend_id, tier);
                }
                Ok(bytes)
            }
            (true, CacheAction::TrackAvailability) => {
                let bytes = streaming::stream_multiline_response(
                    &mut **pooled_conn,
                    client_write,
                    first_chunk,
                    first_chunk_size,
                    self.client_addr,
                    backend_id,
                    &self.buffer_pool,
                )
                .await?;

                if let Some(msg_id_ref) = msg_id {
                    let tier = self.tier_for_backend(backend_id);
                    self.spawn_cache_upsert(msg_id_ref, first_chunk.to_vec(), backend_id, tier);
                }
                Ok(bytes)
            }
            (true, _) => {
                // Multiline but no caching
                streaming::stream_multiline_response(
                    &mut **pooled_conn,
                    client_write,
                    first_chunk,
                    first_chunk_size,
                    self.client_addr,
                    backend_id,
                    &self.buffer_pool,
                )
                .await
            }
            (false, CacheAction::TrackStat) => {
                client_write.write_all(first_chunk).await?;
                if let Some(msg_id_ref) = msg_id {
                    let tier = self.tier_for_backend(backend_id);
                    self.spawn_cache_upsert(msg_id_ref, b"223\r\n".to_vec(), backend_id, tier);
                }
                Ok(first_chunk_size as u64)
            }
            (false, _) => {
                // Single-line, no caching
                client_write.write_all(first_chunk).await?;
                Ok(first_chunk_size as u64)
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
        self.metrics
            .user_bytes_sent(self.username().as_deref(), cmd_bytes);
        self.metrics
            .user_bytes_received(self.username().as_deref(), resp_bytes);
    }
}
