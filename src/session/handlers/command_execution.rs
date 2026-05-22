//! Backend command execution and response writing
//!
//! Handles executing commands on individual backends, including connection retry,
//! response validation, and writing backend responses to clients.

use crate::protocol::{RequestContext, RequestKind, RequestResponseMetadata, StatusCode};
use crate::router::{BackendSelector, CommandGuard};
use crate::session::SessionError;
use crate::session::response_transfer::{ResponseConnectionReuse, ResponseTransferError};
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
    /// No backend is retryable without violating tier or availability rules
    NoRetryableBackend,
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
    pub unavailable_backends: &'a mut u8,
}

#[derive(Clone, Copy)]
pub(super) enum RetryAttemptKind {
    Direct,
    OrderedPipeline,
}

impl RetryAttemptKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct retry",
            Self::OrderedPipeline => "ordered pipeline retry",
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct AuthoritativeArticleMissing {
    backend_id: BackendId,
}

impl AuthoritativeArticleMissing {
    pub(super) fn from_status_code(backend_id: BackendId, status_code: StatusCode) -> Option<Self> {
        (status_code.as_u16() == 430).then_some(Self { backend_id })
    }

    pub(super) const fn backend_id(self) -> BackendId {
        self.backend_id
    }
}

type BackendTimings = (u64, u64, u64);

/// Parameters describing the response to write to the client
#[derive(Clone, Copy)]
pub(super) struct ResponseWriteParams<'a> {
    pub(super) request: &'a RequestContext,
    pub(super) msg_id: Option<&'a crate::types::MessageId<'a>>,
    pub(super) status_code: StatusCode,
}

struct InvalidBackendResponseContext<'a> {
    provider: &'a crate::pool::DeadpoolConnectionProvider,
    request: &'a RequestContext,
    buffer: &'a crate::pool::PooledBuffer,
    conn: crate::pool::ConnectionGuard,
}

pub(super) type PreparedBackendAttempt = Option<(
    crate::pool::ConnectionGuard,
    StatusCode,
    crate::pool::PooledBuffer,
)>;

type ExecutedBackendAttempt = (
    crate::pool::ConnectionGuard,
    crate::session::backend::BackendReadResult,
    crate::pool::PooledBuffer,
    Option<BackendTimings>,
);

type BackendReadAttempt = (
    crate::session::backend::BackendReadResult,
    crate::pool::PooledBuffer,
    Option<BackendTimings>,
);

enum BackendReadAttemptError {
    BufferPoolExhausted,
    Backend(anyhow::Error),
}

/// Any client write failure after the full backend response is already owned is terminal.
///
/// The backend connection is already clean at this point, but the client may have received a
/// partial prefix of the response. Retrying another backend on the same client socket would
/// splice responses together and corrupt NNTP response ordering.
const fn classify_response_write_err(e: std::io::Error) -> ResponseTransferError {
    ResponseTransferError::ClientDisconnect(e)
}

const fn can_discard_response_after_write(
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
enum ResponseRetention {
    DiscardAfterWrite,
    RetainAfterWrite,
}

impl ResponseRetention {
    const fn for_request(request: &RequestContext, cache_action: CacheAction) -> Self {
        match can_discard_response_after_write(request, cache_action) {
            true => Self::DiscardAfterWrite,
            false => Self::RetainAfterWrite,
        }
    }
}

impl ClientSession {
    pub(super) fn retry_backend_provider<'a>(
        &self,
        router: &'a BackendSelector,
        backend_id: BackendId,
        request: &RequestContext,
        attempt: RetryAttemptKind,
    ) -> Option<&'a crate::pool::DeadpoolConnectionProvider> {
        let provider = router.backend_provider(backend_id);
        if provider.is_none() {
            debug!(
                client = %self.client_addr,
                backend = backend_id.as_index(),
                command_verb = ?request.verb(),
                msg_id = ?request.message_id_value(),
                attempt = attempt.as_str(),
                "Selected backend had no provider; treating backend as unavailable"
            );
        }
        provider
    }

    /// Try executing command on next available backend
    ///
    /// If the pooled connection is stale (connection error), automatically retries
    /// with a fresh connection before returning an error.
    pub(super) async fn try_backend_for_article(
        &self,
        router: &Arc<BackendSelector>,
        request: &mut RequestContext,
        client_writer: &crate::session::SharedClientWriter,
        state: &mut ArticleAttemptState<'_>,
    ) -> Result<BackendAttemptResult, SessionError> {
        let backend_id = match router.route_with_availability_suppressing(
            self.client_id,
            Some(state.availability),
            *state.unavailable_backends,
        ) {
            Ok(backend_id) => backend_id,
            Err(err) => {
                debug!(
                    client = %self.client_addr,
                    error = %err,
                    "No eligible backend available for article retry"
                );
                return Ok(BackendAttemptResult::NoRetryableBackend);
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
        let Some(provider) =
            self.retry_backend_provider(router, backend_id, request, RetryAttemptKind::Direct)
        else {
            return Ok(BackendAttemptResult::BackendUnavailable);
        };

        let Some((mut conn, status_code, buffer)) = self
            .prepare_backend_attempt(provider, backend_id, request, state)
            .await?
        else {
            return Ok(BackendAttemptResult::BackendUnavailable);
        };

        if let Some(missing) =
            AuthoritativeArticleMissing::from_status_code(backend_id, status_code)
        {
            debug!(
                client = %self.client_addr,
                backend = backend_id.as_index(),
                command_verb = ?request.verb(),
                msg_id = ?request.message_id_value(),
                "Direct backend attempt returned 430 before writing response"
            );
            self.record_authoritative_article_missing(missing, state.availability);
            if let Some(msg_id) = request.message_id_value() {
                self.cache.record_backend_missing(msg_id, backend_id).await;
            }
            self.capture_suppressed_430_response(&mut conn, backend_id, request, buffer)
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
        let response = match self
            .write_successful_retry_response(
                conn,
                client_writer,
                backend_id,
                buffer,
                ResponseWriteParams {
                    request,
                    msg_id: msg_id.as_ref(),
                    status_code,
                },
                state.backend_connection,
            )
            .await
        {
            Ok(response) => response,
            Err(e @ SessionError::ClientDisconnect(_)) => {
                self.sync_availability_if_needed(msg_id.as_ref(), state.availability)
                    .await;
                return Err(e);
            }
            Err(e) => return Err(e),
        };
        Self::record_successful_availability(backend_id, status_code, state.availability);
        guard.complete();
        Ok(BackendAttemptResult::success(request, backend_id, response))
    }

    async fn write_successful_retry_response(
        &self,
        conn: crate::pool::ConnectionGuard,
        client_writer: &crate::session::SharedClientWriter,
        backend_id: BackendId,
        buffer: crate::pool::PooledBuffer,
        params: ResponseWriteParams<'_>,
        backend_connection: &mut Option<(BackendId, crate::pool::ConnectionGuard)>,
    ) -> Result<RequestResponseMetadata, SessionError> {
        let mut client_write = client_writer.lock().await;
        self.write_successful_backend_response(
            conn,
            &mut *client_write,
            backend_id,
            buffer,
            params,
            Some(backend_connection),
        )
        .await
    }

    pub(super) async fn prepare_backend_attempt(
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
        let (conn, read_status, buffer, timings) = retry_once!(
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

        let status_code = match read_status.status_code() {
            Some(status_code) => status_code,
            None => {
                read_status.log_warnings(&buffer, self.client_addr, backend_id);
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
                        buffer: &buffer,
                        conn,
                    },
                );
                *state.unavailable_backends |= backend_id.availability_bit();
                return Ok(None);
            }
        };

        debug!(
            client = %self.client_addr,
            backend = backend_id.as_index(),
            command_verb = ?request.verb(),
            msg_id = ?request.message_id_value(),
            status_code = status_code.as_u16(),
            backend_read_bytes = buffer.initialized(),
            availability_missing_bits = format_args!("{:08b}", state.availability.missing_bits()),
            "Backend attempt received classifiable response bytes"
        );

        Ok(Some((conn, status_code, buffer)))
    }

    pub(super) async fn capture_suppressed_430_response(
        &self,
        conn: &mut crate::pool::ConnectionGuard,
        backend_id: BackendId,
        request: &RequestContext,
        mut buffer: crate::pool::PooledBuffer,
    ) -> Result<(), SessionError> {
        crate::session::backend::observe_response(
            request,
            &mut buffer,
            conn,
            &self.buffer_pool,
            backend_id,
        )
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
            buffer,
            conn,
        } = ctx;
        tracing::warn!(
            client = %self.client_addr,
            backend = ?backend_id,
            command_verb = ?request.verb(),
            first_bytes_hex = %crate::session::backend::format_hex_preview(
                buffer, 256
            ),
            first_bytes_utf8 = %String::from_utf8_lossy(buffer),
            "Backend returned invalid/unparseable response, attempting to salvage connection"
        );
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
    pub(super) async fn write_successful_backend_response<W>(
        &self,
        mut conn: crate::pool::ConnectionGuard,
        client_write: &mut W,
        backend_id: BackendId,
        backend_bytes: crate::pool::PooledBuffer,
        params: ResponseWriteParams<'_>,
        backend_connection: Option<&mut Option<(BackendId, crate::pool::ConnectionGuard)>>,
    ) -> Result<RequestResponseMetadata, SessionError>
    where
        W: AsyncWrite + Unpin,
    {
        let has_response_body = params.request.has_response_body(params.status_code);
        let status_code = params.status_code;
        debug!(
            client = %self.client_addr,
            backend = backend_id.as_index(),
            command_verb = ?params.request.verb(),
            status_code = status_code.as_u16(),
            has_response_body,
            "Writing backend response to client"
        );
        let bytes_written = match self
            .write_response_to_client(&mut conn, client_write, backend_id, backend_bytes, params)
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
            status_code,
            params.request.request_wire_len().as_u64(),
            bytes_written,
        );
        self.release_or_reuse_connection(conn, backend_id, params.request, backend_connection);

        Ok(RequestResponseMetadata::new(
            status_code,
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
            conn.retire_with_cooldown();
        } else if let ResponseConnectionReuse::QueuedBytes { len } =
            crate::session::response_transfer::connection_reuse_after_response(&conn)
        {
            warn!(
                client = %self.client_addr,
                backend = backend_id.as_index(),
                command_verb = ?request.verb(),
                queued_bytes = len,
                "Response write left queued backend bytes; retiring connection"
            );
            conn.retire_without_cooldown();
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

    pub(super) fn release_or_reuse_connection(
        &self,
        conn: crate::pool::ConnectionGuard,
        backend_id: BackendId,
        request: &RequestContext,
        backend_connection: Option<&mut Option<(BackendId, crate::pool::ConnectionGuard)>>,
    ) {
        if let ResponseConnectionReuse::QueuedBytes { len } =
            crate::session::response_transfer::connection_reuse_after_response(&conn)
        {
            warn!(
                client = %self.client_addr,
                backend = backend_id.as_index(),
                command_verb = ?request.verb(),
                msg_id = ?request.message_id_value(),
                connection_type = conn.connection_type(),
                pending_bytes = conn.pending_bytes_len(),
                queued_bytes = len,
                "Direct per-command response left queued backend bytes; retiring connection"
            );
            conn.retire_without_cooldown();
        } else if let Some(slot) = backend_connection
            && slot.is_none()
        {
            let status = conn.provider_status_counts();
            debug!(
                client = %self.client_addr,
                backend = backend_id.as_index(),
                command_verb = ?request.verb(),
                msg_id = ?request.message_id_value(),
                connection_type = conn.connection_type(),
                pending_bytes = conn.pending_bytes_len(),
                pool = %conn.provider_name(),
                pool_available = status.available,
                pool_size = status.size,
                pool_max_size = status.max_size,
                pool_waiting = status.waiting,
                "Direct per-command response finished cleanly; keeping backend connection for this client batch"
            );
            *slot = Some((backend_id, conn));
        } else {
            debug!(
                client = %self.client_addr,
                backend = backend_id.as_index(),
                command_verb = ?request.verb(),
                msg_id = ?request.message_id_value(),
                connection_type = conn.connection_type(),
                pending_bytes = conn.pending_bytes_len(),
                "Direct per-command response finished cleanly; releasing backend connection"
            );
            let _ = conn.release();
        }
    }

    async fn checkout_direct_backend_connection(
        &self,
        provider: &crate::pool::DeadpoolConnectionProvider,
        backend_id: crate::types::BackendId,
        request: &RequestContext,
        backend_connection: &mut Option<(BackendId, crate::pool::ConnectionGuard)>,
    ) -> Result<crate::pool::ConnectionGuard> {
        let checkout_status = provider.status_counts();
        debug!(
            client = %self.client_addr,
            backend = backend_id.as_index(),
            command_verb = ?request.verb(),
            msg_id = ?request.message_id_value(),
            pool = %provider.name(),
            pool_available = checkout_status.available,
            pool_size = checkout_status.size,
            pool_max_size = checkout_status.max_size,
            pool_waiting = checkout_status.waiting,
            "Starting pool checkout for direct backend attempt"
        );
        let guard = match backend_connection.take() {
            Some((cached_backend_id, guard)) if cached_backend_id == backend_id => {
                let checkout_status = provider.status_counts();
                if checkout_status.available == 0
                    && checkout_status.size >= checkout_status.max_size
                {
                    debug!(
                        client = %self.client_addr,
                        backend = backend_id.as_index(),
                        command_verb = ?request.verb(),
                        msg_id = ?request.message_id_value(),
                        pool = %provider.name(),
                        pool_available = checkout_status.available,
                        pool_size = checkout_status.size,
                        pool_max_size = checkout_status.max_size,
                        pool_waiting = checkout_status.waiting,
                        connection_type = guard.connection_type(),
                        pending_bytes = guard.pending_bytes_len(),
                        "Reusing backend connection for direct backend attempt because pool capacity is fully checked out"
                    );
                    guard
                } else if checkout_status.available == 0 {
                    debug!(
                        client = %self.client_addr,
                        backend = backend_id.as_index(),
                        command_verb = ?request.verb(),
                        msg_id = ?request.message_id_value(),
                        pool = %provider.name(),
                        pool_available = checkout_status.available,
                        pool_size = checkout_status.size,
                        pool_max_size = checkout_status.max_size,
                        pool_waiting = checkout_status.waiting,
                        connection_type = guard.connection_type(),
                        pending_bytes = guard.pending_bytes_len(),
                        "Checking out another backend connection before returning cached batch connection"
                    );
                    let released = guard.release();
                    match provider.get_pooled_connection().await {
                        Ok(conn) => {
                            drop(released);
                            let checkout_status = provider.status_counts();
                            debug!(
                                client = %self.client_addr,
                                backend = backend_id.as_index(),
                                command_verb = ?request.verb(),
                                msg_id = ?request.message_id_value(),
                                pool = %provider.name(),
                                pool_available = checkout_status.available,
                                pool_size = checkout_status.size,
                                pool_max_size = checkout_status.max_size,
                                pool_waiting = checkout_status.waiting,
                                connection_type = conn.connection_type(),
                                pending_bytes = conn.pending_bytes_len(),
                                "Pool checkout succeeded for direct backend attempt"
                            );
                            crate::pool::ConnectionGuard::new(conn, provider.clone())
                        }
                        Err(err) => {
                            debug!(
                                client = %self.client_addr,
                                backend = backend_id.as_index(),
                                command_verb = ?request.verb(),
                                msg_id = ?request.message_id_value(),
                                pool = %provider.name(),
                                error = %err,
                                connection_type = released.connection_type(),
                                pending_bytes = released.pending_bytes_len(),
                                "Pool checkout failed; reusing cached batch connection for direct backend attempt"
                            );
                            crate::pool::ConnectionGuard::new(released, provider.clone())
                        }
                    }
                } else {
                    debug!(
                        client = %self.client_addr,
                        backend = backend_id.as_index(),
                        command_verb = ?request.verb(),
                        msg_id = ?request.message_id_value(),
                        pool = %provider.name(),
                        pool_available = checkout_status.available,
                        pool_size = checkout_status.size,
                        pool_max_size = checkout_status.max_size,
                        pool_waiting = checkout_status.waiting,
                        connection_type = guard.connection_type(),
                        pending_bytes = guard.pending_bytes_len(),
                        "Releasing cached batch connection because backend pool has idle capacity"
                    );
                    let _ = guard.release();
                    let conn = provider.get_pooled_connection().await?;
                    let checkout_status = provider.status_counts();
                    debug!(
                        client = %self.client_addr,
                        backend = backend_id.as_index(),
                        command_verb = ?request.verb(),
                        msg_id = ?request.message_id_value(),
                        pool = %provider.name(),
                        pool_available = checkout_status.available,
                        pool_size = checkout_status.size,
                        pool_max_size = checkout_status.max_size,
                        pool_waiting = checkout_status.waiting,
                        connection_type = conn.connection_type(),
                        pending_bytes = conn.pending_bytes_len(),
                        "Pool checkout succeeded for direct backend attempt"
                    );
                    crate::pool::ConnectionGuard::new(conn, provider.clone())
                }
            }
            Some(cached) => {
                let (cached_backend_id, cached_conn) = cached;
                debug!(
                    client = %self.client_addr,
                    cached_backend = cached_backend_id.as_index(),
                    backend = backend_id.as_index(),
                    command_verb = ?request.verb(),
                    msg_id = ?request.message_id_value(),
                    connection_type = cached_conn.connection_type(),
                    pending_bytes = cached_conn.pending_bytes_len(),
                    "Releasing cached backend connection before switching backend"
                );
                let _ = cached_conn.release();
                let checkout_status = provider.status_counts();
                debug!(
                    client = %self.client_addr,
                    backend = backend_id.as_index(),
                    command_verb = ?request.verb(),
                    msg_id = ?request.message_id_value(),
                    pool = %provider.name(),
                    pool_available = checkout_status.available,
                    pool_size = checkout_status.size,
                    pool_max_size = checkout_status.max_size,
                    pool_waiting = checkout_status.waiting,
                    "Requesting pooled connection after backend switch"
                );
                let conn = provider.get_pooled_connection().await?;
                let checkout_status = provider.status_counts();
                debug!(
                    client = %self.client_addr,
                    backend = backend_id.as_index(),
                    command_verb = ?request.verb(),
                    msg_id = ?request.message_id_value(),
                    pool = %provider.name(),
                    pool_available = checkout_status.available,
                    pool_size = checkout_status.size,
                    pool_max_size = checkout_status.max_size,
                    pool_waiting = checkout_status.waiting,
                    connection_type = conn.connection_type(),
                    pending_bytes = conn.pending_bytes_len(),
                    "Pool checkout succeeded for direct backend attempt"
                );
                crate::pool::ConnectionGuard::new(conn, provider.clone())
            }
            None => {
                let checkout_status = provider.status_counts();
                debug!(
                    client = %self.client_addr,
                    backend = backend_id.as_index(),
                    command_verb = ?request.verb(),
                    msg_id = ?request.message_id_value(),
                    pool = %provider.name(),
                    pool_available = checkout_status.available,
                    pool_size = checkout_status.size,
                    pool_max_size = checkout_status.max_size,
                    pool_waiting = checkout_status.waiting,
                    "Requesting pooled connection"
                );
                let conn = provider.get_pooled_connection().await?;
                let checkout_status = provider.status_counts();
                debug!(
                    client = %self.client_addr,
                    backend = backend_id.as_index(),
                    command_verb = ?request.verb(),
                    msg_id = ?request.message_id_value(),
                    pool = %provider.name(),
                    pool_available = checkout_status.available,
                    pool_size = checkout_status.size,
                    pool_max_size = checkout_status.max_size,
                    pool_waiting = checkout_status.waiting,
                    connection_type = conn.connection_type(),
                    pending_bytes = conn.pending_bytes_len(),
                    "Pool checkout succeeded for direct backend attempt"
                );
                crate::pool::ConnectionGuard::new(conn, provider.clone())
            }
        };
        Ok(guard)
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
    ) -> Result<ExecutedBackendAttempt> {
        let mut guard = self
            .checkout_direct_backend_connection(provider, backend_id, request, backend_connection)
            .await?;
        debug!(
            client = %self.client_addr,
            backend = backend_id.as_index(),
            command_verb = ?request.verb(),
            msg_id = ?request.message_id_value(),
            connection_type = guard.connection_type(),
            pending_bytes = guard.pending_bytes_len(),
            "Sending request to backend and waiting for classifiable response bytes"
        );
        let result = self
            .execute_and_read_response(&mut guard, backend_id, request)
            .await;

        match result {
            Ok((read_status, buffer, timings)) => Ok((guard, read_status, buffer, timings)),
            Err(BackendReadAttemptError::BufferPoolExhausted) => {
                debug!(
                    client = %self.client_addr,
                    backend = backend_id.as_index(),
                    command_verb = ?request.verb(),
                    "Regular buffer pool exhausted on direct backend attempt; releasing backend connection"
                );
                let _ = guard.release();
                Err(anyhow::anyhow!(
                    "regular buffer pool exhausted on direct backend attempt"
                ))
            }
            Err(BackendReadAttemptError::Backend(e)) => {
                debug!(
                    client = %self.client_addr,
                    backend = backend_id.as_index(),
                    command_verb = ?request.verb(),
                    error = %e,
                    "Backend attempt failed before response completed; dropping pooled connection"
                );
                guard.retire_with_cooldown();
                Err(e)
            }
        }
    }

    /// Execute command on a connection and read enough bytes to classify the backend response.
    ///
    /// Takes `&mut ConnectionStream` (not a pool object) so it can be called
    /// directly after pool checkout.
    async fn execute_and_read_response(
        &self,
        conn: &mut crate::stream::ConnectionStream,
        backend_id: crate::types::BackendId,
        request: &RequestContext,
    ) -> Result<BackendReadAttempt, BackendReadAttemptError> {
        self.metrics.record_command(backend_id);
        self.metrics.user_command(self.username());

        let mut buffer = self
            .buffer_pool
            .try_acquire()
            .ok_or(BackendReadAttemptError::BufferPoolExhausted)?;
        let (response, timings) = if should_sample_backend_timing() {
            let (response, ttfb, send, recv) =
                backend::send_request_timed(conn, request, &mut buffer)
                    .await
                    .map_err(BackendReadAttemptError::Backend)?;
            (response, Some((ttfb, send, recv)))
        } else {
            let response = backend::send_request(conn, request, &mut buffer)
                .await
                .map_err(BackendReadAttemptError::Backend)?;
            (response, None)
        };
        debug!(
            client = %self.client_addr,
            backend = backend_id.as_index(),
            command_verb = ?request.verb(),
            msg_id = ?request.message_id_value(),
            "Backend response classification completed for direct attempt"
        );

        Ok((response, buffer, timings))
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
        backend_bytes: crate::pool::PooledBuffer,
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

        if matches!(cache_action, CacheAction::TrackAvailability) {
            self.apply_cache_action(cache_action, params, backend_id, None);
        }

        let (bytes_written, captured) =
            match ResponseRetention::for_request(params.request, cache_action) {
                ResponseRetention::DiscardAfterWrite => {
                    let bytes = self
                        .write_response_without_retention(
                            pooled_conn,
                            client_write,
                            backend_id,
                            backend_bytes,
                            params,
                        )
                        .await?;
                    (bytes, None)
                }
                ResponseRetention::RetainAfterWrite => {
                    let (bytes, response) = self
                        .write_response_with_retention(
                            pooled_conn,
                            client_write,
                            backend_id,
                            backend_bytes,
                            params,
                        )
                        .await?;
                    (bytes, Some(response))
                }
            };

        if !matches!(cache_action, CacheAction::TrackAvailability) {
            self.apply_cache_action(cache_action, params, backend_id, captured);
        }
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

    async fn write_response_without_retention<W>(
        &self,
        pooled_conn: &mut deadpool::managed::Object<crate::pool::deadpool_connection::TcpManager>,
        client_write: &mut W,
        backend_id: BackendId,
        mut backend_bytes: crate::pool::PooledBuffer,
        params: ResponseWriteParams<'_>,
    ) -> Result<u64, ResponseTransferError>
    where
        W: AsyncWrite + Unpin,
    {
        crate::session::multiline_framing::write_response(
            params.request,
            &mut backend_bytes,
            pooled_conn,
            client_write,
            &self.buffer_pool,
            backend_id,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn write_response_with_retention<W>(
        &self,
        pooled_conn: &mut deadpool::managed::Object<crate::pool::deadpool_connection::TcpManager>,
        client_write: &mut W,
        backend_id: BackendId,
        mut backend_bytes: crate::pool::PooledBuffer,
        params: ResponseWriteParams<'_>,
    ) -> Result<(u64, crate::pool::ChunkedResponse), ResponseTransferError>
    where
        W: AsyncWrite + Unpin,
    {
        let mut captured = crate::pool::ChunkedResponse::default();
        // Payload caching explicitly owns a complete response, but the capture
        // itself still goes through the backend facade so response splitting
        // remains centralized.
        crate::session::backend::capture_response(
            params.request,
            &mut backend_bytes,
            pooled_conn,
            &mut captured,
            &self.buffer_pool,
            backend_id,
        )
        .await?;
        self.log_body_response_captured(backend_id, params, captured.len());
        drop(backend_bytes);
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

    fn log_body_response_captured(
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
                "Captured BODY response for direct client delivery"
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
                "Wrote captured BODY response to direct client"
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
    use super::ArticleAttemptState;
    use super::AuthoritativeArticleMissing;
    use super::BackendAttemptResult;
    use super::ResponseWriteParams;
    use super::classify_response_write_err;
    use crate::auth::AuthHandler;
    use crate::cache::ArticleAvailability;
    use crate::metrics::MetricsCollector;
    use crate::pool::{BufferPool, ConnectionGuard, DeadpoolConnectionProvider};
    use crate::protocol::{RequestContext, RequestResponseMetadata, ResponseWireLen, StatusCode};
    use crate::router::BackendSelector;
    use crate::types::{BackendId, BufferSize, ClientAddress, ClientToBackendBytes, ServerName};
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};
    use std::time::Duration;
    use tokio::io::AsyncWrite;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};

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

    async fn spawn_article_response_server(response: &'static [u8]) -> (u16, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let article_commands = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&article_commands);

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let count = Arc::clone(&count);
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
                                if cmd.starts_with("MODE") {
                                    let _ = write_half.write_all(b"200 Posting allowed\r\n").await;
                                } else if cmd.starts_with("BODY") || cmd.starts_with("ARTICLE") {
                                    count.fetch_add(1, Ordering::SeqCst);
                                    let _ = write_half.write_all(response).await;
                                } else if cmd.starts_with("QUIT") {
                                    let _ = write_half.write_all(b"205 Goodbye\r\n").await;
                                    break;
                                } else {
                                    let _ = write_half.write_all(b"200 OK\r\n").await;
                                }
                            }
                        }
                    }
                });
            }
        });

        (port, article_commands)
    }

    async fn shared_client_writer_pair() -> (
        crate::session::SharedClientWriter,
        tokio::net::tcp::OwnedReadHalf,
        tokio::net::tcp::OwnedWriteHalf,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let (proxy_side, _) = listener.accept().await.unwrap();
        let (_proxy_read, proxy_write) = proxy_side.into_split();
        let (client_read, client_write) = client.into_split();

        (
            crate::session::SharedClientWriter::new(proxy_write),
            client_read,
            client_write,
        )
    }

    fn router_with_backend(provider: DeadpoolConnectionProvider) -> Arc<BackendSelector> {
        let mut router = BackendSelector::new();
        router.add_backend(
            BackendId::from_index(0),
            ServerName::try_new("backend-0".to_string()).unwrap(),
            provider,
            0,
        );
        Arc::new(router)
    }

    fn router_with_tiered_backends(
        backends: impl IntoIterator<Item = (DeadpoolConnectionProvider, u8)>,
    ) -> Arc<BackendSelector> {
        let mut router = BackendSelector::new();
        for (index, (provider, tier)) in backends.into_iter().enumerate() {
            router.add_backend(
                BackendId::from_index(index),
                ServerName::try_new(format!("backend-{index}")).unwrap(),
                provider,
                tier,
            );
        }
        Arc::new(router)
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
    fn owned_response_timeout_is_terminal_client_disconnect() {
        let err = std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out");
        let classified = classify_response_write_err(err);
        assert!(matches!(
            classified,
            crate::session::response_transfer::ResponseTransferError::ClientDisconnect(_)
        ));
    }

    #[test]
    fn owned_response_abort_is_terminal_client_disconnect() {
        let err = std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "aborted");
        let classified = classify_response_write_err(err);
        assert!(matches!(
            classified,
            crate::session::response_transfer::ResponseTransferError::ClientDisconnect(_)
        ));
    }

    #[test]
    fn authoritative_article_missing_only_classifies_430() {
        let backend_id = BackendId::from_index(2);
        let missing =
            AuthoritativeArticleMissing::from_status_code(backend_id, StatusCode::new(430));
        assert_eq!(
            missing.map(AuthoritativeArticleMissing::backend_id),
            Some(backend_id)
        );

        assert!(
            AuthoritativeArticleMissing::from_status_code(backend_id, StatusCode::new(400))
                .is_none()
        );
        assert!(
            AuthoritativeArticleMissing::from_status_code(backend_id, StatusCode::new(500))
                .is_none()
        );
        assert!(
            AuthoritativeArticleMissing::from_status_code(backend_id, StatusCode::new(222))
                .is_none()
        );
    }

    #[test]
    fn missing_availability_requires_authoritative_430_classification() {
        let session = test_session();
        let backend_id = BackendId::from_index(0);
        let mut availability = ArticleAvailability::new();

        assert!(
            AuthoritativeArticleMissing::from_status_code(backend_id, StatusCode::new(503))
                .is_none()
        );
        assert_eq!(availability.checked_bits(), 0);
        assert_eq!(availability.missing_bits(), 0);

        let missing =
            AuthoritativeArticleMissing::from_status_code(backend_id, StatusCode::new(430))
                .expect("430 is authoritative article-missing");
        session.record_authoritative_article_missing(missing, &mut availability);

        assert_eq!(availability.checked_bits(), 0b0000_0001);
        assert_eq!(availability.missing_bits(), 0b0000_0001);
    }

    #[tokio::test]
    async fn retry_430_attempt_does_not_wait_for_client_writer_lock() {
        let session = test_session();
        let (port, article_commands) = spawn_article_response_server(b"430 Missing\r\n").await;
        let provider = DeadpoolConnectionProvider::builder("127.0.0.1", port)
            .max_connections(1)
            .build()
            .unwrap();
        let router = router_with_backend(provider);
        let (client_writer, _client_read, _client_write) = shared_client_writer_pair().await;
        let held_writer = client_writer.lock().await;

        let mut request = request_context(b"BODY <missing@example.com>\r\n");
        let mut availability = ArticleAvailability::new();
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_connection = None;
        let mut unavailable_backends = 0;
        let mut state = ArticleAttemptState {
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
        };

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            session.try_backend_for_article(&router, &mut request, &client_writer, &mut state),
        )
        .await
        .expect("430 probe should not wait for the client writer lock")
        .expect("430 probe should complete without transport error");

        assert!(matches!(
            result,
            BackendAttemptResult::ArticleNotFound { backend_id }
                if backend_id == BackendId::from_index(0)
        ));
        assert!(availability.is_missing(BackendId::from_index(0)));
        assert_eq!(article_commands.load(Ordering::SeqCst), 1);

        drop(held_writer);
    }

    #[tokio::test]
    async fn invalid_backend_response_does_not_mark_article_missing() {
        let session = test_session();
        let (port, article_commands) =
            spawn_article_response_server(b"not a status line\r\n").await;
        let provider = DeadpoolConnectionProvider::builder("127.0.0.1", port)
            .max_connections(1)
            .build()
            .unwrap();
        let router = router_with_backend(provider);
        let (client_writer, _client_read, _client_write) = shared_client_writer_pair().await;

        let mut request = request_context(b"BODY <bad-response@example.com>\r\n");
        let mut availability = ArticleAvailability::new();
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_connection = None;
        let mut unavailable_backends = 0;
        let mut state = ArticleAttemptState {
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
        };

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            session.try_backend_for_article(&router, &mut request, &client_writer, &mut state),
        )
        .await
        .expect("invalid response attempt should not hang")
        .expect("invalid response attempt should be handled as unavailable");

        assert!(matches!(result, BackendAttemptResult::BackendUnavailable));
        assert_eq!(article_commands.load(Ordering::SeqCst), 1);
        assert_eq!(availability.checked_bits(), 0);
        assert_eq!(availability.missing_bits(), 0);
        assert_eq!(unavailable_backends, 0b0000_0001);
    }

    #[tokio::test]
    async fn invalid_backend_response_retries_next_same_tier_without_poisoning_availability() {
        let session = test_session();
        let (bad_port, bad_article_commands) =
            spawn_article_response_server(b"not a status line\r\n").await;
        let good_response = b"222 0 <found@example.com> body follows\r\npayload\r\n.\r\n";
        let (good_port, good_article_commands) = spawn_article_response_server(good_response).await;
        let router = router_with_tiered_backends([
            (
                DeadpoolConnectionProvider::builder("127.0.0.1", bad_port)
                    .max_connections(1)
                    .build()
                    .unwrap(),
                0,
            ),
            (
                DeadpoolConnectionProvider::builder("127.0.0.1", good_port)
                    .max_connections(1)
                    .build()
                    .unwrap(),
                0,
            ),
        ]);
        let (client_writer, mut client_read, _client_write) = shared_client_writer_pair().await;

        let mut request = request_context(b"BODY <found@example.com>\r\n");
        let mut availability = ArticleAvailability::new();
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_connection = None;
        let mut unavailable_backends = 0;
        let mut state = ArticleAttemptState {
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
        };

        let first = session
            .try_backend_for_article(&router, &mut request, &client_writer, &mut state)
            .await
            .expect("invalid response should be handled");
        assert!(matches!(first, BackendAttemptResult::BackendUnavailable));
        assert_eq!(state.availability.checked_bits(), 0);
        assert_eq!(state.availability.missing_bits(), 0);
        assert_eq!(*state.unavailable_backends, 0b0000_0001);

        let second = session
            .try_backend_for_article(&router, &mut request, &client_writer, &mut state)
            .await
            .expect("same-tier retry should succeed");
        assert!(matches!(second, BackendAttemptResult::Success));
        assert_eq!(bad_article_commands.load(Ordering::SeqCst), 1);
        assert_eq!(good_article_commands.load(Ordering::SeqCst), 1);
        assert_eq!(availability.checked_bits(), 0b0000_0010);
        assert_eq!(availability.missing_bits(), 0);
        assert_eq!(request.backend_id(), Some(BackendId::from_index(1)));

        let mut written = vec![0; good_response.len()];
        tokio::time::timeout(Duration::from_secs(1), client_read.read_exact(&mut written))
            .await
            .expect("client response should be readable")
            .expect("client read should succeed");
        assert_eq!(written, good_response);
    }

    #[tokio::test]
    async fn transient_invalid_replies_do_not_escalate_to_backup_tier() {
        let session = test_session();
        let (bad0_port, bad0_article_commands) =
            spawn_article_response_server(b"bad zero\r\n").await;
        let (bad1_port, bad1_article_commands) =
            spawn_article_response_server(b"bad one\r\n").await;
        let (backup_port, backup_article_commands) =
            spawn_article_response_server(b"222 0 <backup@example.com>\r\npayload\r\n.\r\n").await;
        let router = router_with_tiered_backends([
            (
                DeadpoolConnectionProvider::builder("127.0.0.1", bad0_port)
                    .max_connections(1)
                    .build()
                    .unwrap(),
                0,
            ),
            (
                DeadpoolConnectionProvider::builder("127.0.0.1", bad1_port)
                    .max_connections(1)
                    .build()
                    .unwrap(),
                0,
            ),
            (
                DeadpoolConnectionProvider::builder("127.0.0.1", backup_port)
                    .max_connections(1)
                    .build()
                    .unwrap(),
                1,
            ),
        ]);
        let (client_writer, _client_read, _client_write) = shared_client_writer_pair().await;

        let mut request = request_context(b"BODY <bad-response@example.com>\r\n");
        let mut availability = ArticleAvailability::new();
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_connection = None;
        let mut unavailable_backends = 0;
        let mut state = ArticleAttemptState {
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
        };

        for expected_mask in [0b0000_0001, 0b0000_0011] {
            let result = session
                .try_backend_for_article(&router, &mut request, &client_writer, &mut state)
                .await
                .expect("invalid response should be handled");
            assert!(matches!(result, BackendAttemptResult::BackendUnavailable));
            assert_eq!(*state.unavailable_backends, expected_mask);
        }

        let exhausted = session
            .try_backend_for_article(&router, &mut request, &client_writer, &mut state)
            .await
            .expect("tier exhaustion should be reported without transport failure");
        assert!(matches!(
            exhausted,
            BackendAttemptResult::NoRetryableBackend
        ));
        assert_eq!(bad0_article_commands.load(Ordering::SeqCst), 1);
        assert_eq!(bad1_article_commands.load(Ordering::SeqCst), 1);
        assert_eq!(backup_article_commands.load(Ordering::SeqCst), 0);
        assert_eq!(availability.checked_bits(), 0);
        assert_eq!(availability.missing_bits(), 0);
    }

    #[tokio::test]
    async fn successful_retry_attempt_waits_for_client_writer_only_to_emit_response() {
        let session = test_session();
        let response = b"222 0 <found@example.com> body follows\r\npayload\r\n.\r\n";
        let (port, article_commands) = spawn_article_response_server(response).await;
        let provider = DeadpoolConnectionProvider::builder("127.0.0.1", port)
            .max_connections(1)
            .build()
            .unwrap();
        let router = router_with_backend(provider);
        let (client_writer, mut client_read, _client_write) = shared_client_writer_pair().await;
        let held_writer = client_writer.lock().await;

        let mut request = request_context(b"BODY <found@example.com>\r\n");
        let mut availability = ArticleAvailability::new();
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_connection = None;
        let mut unavailable_backends = 0;
        let mut state = ArticleAttemptState {
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
        };

        let mut attempt = Box::pin(session.try_backend_for_article(
            &router,
            &mut request,
            &client_writer,
            &mut state,
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            while article_commands.load(Ordering::SeqCst) == 0 {
                tokio::select! {
                    _ = &mut attempt => {
                        panic!("successful attempt completed before client writer lock was released");
                    }
                    () = tokio::time::sleep(Duration::from_millis(5)) => {}
                }
            }
        })
        .await
        .expect("backend should receive article command while client writer is locked");

        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut attempt)
                .await
                .is_err(),
            "successful response should wait for the client writer lock before emitting"
        );

        drop(held_writer);
        let result = tokio::time::timeout(Duration::from_secs(1), attempt)
            .await
            .expect("successful attempt should finish once the writer lock is released")
            .expect("successful attempt should not fail");
        assert!(matches!(result, BackendAttemptResult::Success));
        assert_eq!(article_commands.load(Ordering::SeqCst), 1);
        assert_eq!(availability.checked_bits(), 0b0000_0001);
        assert_eq!(availability.missing_bits(), 0);
        assert!(availability.any_backend_has_article());

        let mut written = vec![0; response.len()];
        tokio::time::timeout(Duration::from_secs(1), client_read.read_exact(&mut written))
            .await
            .expect("client response should be readable")
            .expect("client read should succeed");
        assert_eq!(written, response);
    }

    #[tokio::test]
    async fn flush_disconnect_after_owned_response_reuses_backend_connection() {
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
        let mut backend_bytes = BufferPool::new(BufferSize::try_new(8192).unwrap(), 1).acquire();
        backend_bytes.copy_from_slice(b"223 0 <test@example.com> status\r\n");

        let err = session
            .write_successful_backend_response(
                guard,
                &mut client_write,
                BackendId::from_index(0),
                backend_bytes,
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
            "client-side flush failure after a clean owned response must not retire the backend connection"
        );
    }

    #[tokio::test]
    async fn write_disconnect_after_complete_response_reuses_backend_connection() {
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
        let mut backend_bytes = BufferPool::new(BufferSize::try_new(8192).unwrap(), 1).acquire();
        backend_bytes.copy_from_slice(b"220 Article follows\r\nbody\r\n.\r\n");

        let err = session
            .write_successful_backend_response(
                guard,
                &mut client_write,
                BackendId::from_index(0),
                backend_bytes,
                ResponseWriteParams {
                    request: &request,
                    msg_id: None,
                    status_code: StatusCode::new(220),
                },
                None,
            )
            .await
            .expect_err("client write should fail after the framer completed the response");

        assert!(matches!(
            err,
            crate::session::SessionError::ClientDisconnect(_)
        ));

        let _reused = provider.get_pooled_connection().await.unwrap();
        assert_eq!(
            accept_count.load(Ordering::SeqCst),
            1,
            "client write failure after a complete framed response must not retire the backend connection"
        );
    }
}
