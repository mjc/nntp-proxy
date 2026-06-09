//! Backend command execution and response writing
//!
//! Handles executing commands on individual backends, including connection retry,
//! response validation, and writing backend responses to clients.

use crate::protocol::{RequestContext, RequestKind, RequestResponseMetadata, StatusCode};
use crate::router::{ArticleBackend, BackendSelector, SuppressedBackends};
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
use futures::stream::{FuturesUnordered, StreamExt};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tracing::{debug, trace, warn};

const BACKEND_TIMING_SAMPLE_MASK: u64 = 0x0f;
const RETRY_STAT_SWEEP_PROBE_TIMEOUT: Duration = Duration::from_millis(250);
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
    ArticleNotFound {
        missing: AuthoritativeArticleMissing,
    },
    /// Backend unavailable or error - try next backend
    BackendUnavailable,
    /// No backend is retryable without violating tier or availability rules
    NoRetryableBackend,
}

/// Mutable state for an article backend attempt loop
///
/// Groups the mutable parameters that track retry state across
/// multiple `try_backend_for_article` calls.
pub(super) struct ArticleAttemptState<'a> {
    pub availability: &'a mut crate::cache::ArticleAvailability,
    pub client_to_backend_bytes: &'a mut ClientToBackendBytes,
    pub backend_connection: &'a mut Option<(BackendId, crate::pool::ConnectionGuard)>,
    pub unavailable_backends: &'a mut SuppressedBackends,
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

pub(super) struct AuthoritativeArticleMissing {
    backend_id: BackendId,
}

impl AuthoritativeArticleMissing {
    pub(super) fn from_status_code(
        backend: ArticleBackend,
        status_code: StatusCode,
    ) -> Result<Self, ArticleBackend> {
        if status_code.as_u16() == 430 {
            Ok(Self {
                backend_id: backend.backend_id(),
            })
        } else {
            Err(backend)
        }
    }

    pub(super) const fn backend_id(&self) -> BackendId {
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
    Backend(anyhow::Error),
}

enum RetryStatProbeOutcome {
    Missing(BackendId),
    Present,
    Unavailable(BackendId),
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
    #[inline]
    const fn should_reuse_cached_batch_connection() -> bool {
        true
    }

    pub(super) fn retry_backend_provider<'a>(
        &self,
        router: &'a BackendSelector,
        backend: &ArticleBackend,
        request: &RequestContext,
        attempt: RetryAttemptKind,
    ) -> Option<&'a crate::pool::DeadpoolConnectionProvider> {
        let backend_id = backend.backend_id();
        let provider = router.backend_provider(backend_id);
        if provider.is_none() {
            trace!(
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
        is_retry_attempt: bool,
    ) -> Result<BackendAttemptResult, SessionError> {
        if let Some(delay) = router
            .queue_backpressure_delay_for_article(state.availability, *state.unavailable_backends)
        {
            tokio::time::sleep(delay).await;
        }
        let route_request = crate::router::RouteRequest::new(self.client_id)
            .with_availability(state.availability)
            .suppressing_backends(*state.unavailable_backends);
        let backend = match router.route(route_request) {
            Ok(backend) => backend,
            Err(err) => {
                debug!(
                    client = %self.client_addr,
                    error = %err,
                    "No eligible backend available for article retry"
                );
                return Ok(BackendAttemptResult::NoRetryableBackend);
            }
        };
        let backend_id = backend.backend_id();
        trace!(
            client = %self.client_addr,
            backend = backend_id.as_index(),
            command_verb = ?request.verb(),
            msg_id = ?request.message_id_value(),
            missing_bits = format_args!("{:08b}", state.availability.missing_bits()),
            "Article retry selected backend for direct attempt"
        );
        let guard = BackendSelector::guard_for_routed_backend(router.clone(), backend_id);
        let Some(provider) =
            self.retry_backend_provider(router, &backend, request, RetryAttemptKind::Direct)
        else {
            return Ok(BackendAttemptResult::BackendUnavailable);
        };

        let Some((mut conn, status_code, buffer)) = self
            .prepare_backend_attempt(provider, &backend, request, state, is_retry_attempt)
            .await?
        else {
            return Ok(BackendAttemptResult::BackendUnavailable);
        };

        let backend = match AuthoritativeArticleMissing::from_status_code(backend, status_code) {
            Ok(missing) => {
                trace!(
                    client = %self.client_addr,
                    backend = backend_id.as_index(),
                    command_verb = ?request.verb(),
                    msg_id = ?request.message_id_value(),
                    "Direct backend attempt returned 430 before writing response"
                );
                self.record_authoritative_article_missing(&missing, state.availability);
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
                return Ok(BackendAttemptResult::ArticleNotFound { missing });
            }
            Err(backend) => backend,
        };

        let msg_id = request.message_id_value();
        let response = match self
            .write_successful_retry_response(
                conn,
                client_writer,
                &backend,
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
                return Err(e);
            }
            Err(e) => {
                state.unavailable_backends.suppress(backend_id);
                return Err(e);
            }
        };
        guard.complete();
        request.record_backend_response(backend_id, response);
        Ok(BackendAttemptResult::Success)
    }

    pub(super) async fn parallel_retry_stat_sweep(
        &self,
        router: &Arc<BackendSelector>,
        request: &RequestContext,
        state: &mut ArticleAttemptState<'_>,
    ) -> Result<(), SessionError> {
        if !request.has_message_id()
            || request.is_stat()
            || !matches!(
                request.kind(),
                RequestKind::Article | RequestKind::Body | RequestKind::Head
            )
        {
            return Ok(());
        }

        let Some(stat_request) = Self::stat_probe_request(request) else {
            return Ok(());
        };

        let mut candidates = Vec::new();
        for tier in router.tiers() {
            for backend_id in router.backend_ids_in_tier(tier) {
                if state.unavailable_backends.contains(backend_id)
                    || !state.availability.should_try(backend_id)
                {
                    continue;
                }
                let Some(provider) = router.backend_provider(backend_id) else {
                    continue;
                };
                if provider.stat_missing_enabled() {
                    candidates.push((backend_id, provider.clone()));
                }
            }
        }

        if candidates.len() < 2 {
            return Ok(());
        }

        let mut probes = FuturesUnordered::new();
        for (backend_id, provider) in candidates {
            let router = Arc::clone(router);
            let stat_request = stat_request.clone();
            probes.push(async move {
                let probe = async {
                    let _guard = BackendSelector::guard_for_manual_backend(router, backend_id);
                    let conn = match provider.get_pooled_connection().await {
                        Ok(conn) => conn,
                        Err(_) => return RetryStatProbeOutcome::Unavailable(backend_id),
                    };
                    let mut conn = crate::pool::ConnectionGuard::new(conn, provider);
                    let mut buffer = self.buffer_pool.acquire();
                    let read =
                        backend::send_request(&mut **conn.get_mut(), &stat_request, &mut buffer)
                            .await;
                    let status_code = match read {
                        Ok(read) => read.status_code(),
                        Err(_) => {
                            conn.retire_with_cooldown();
                            return RetryStatProbeOutcome::Unavailable(backend_id);
                        }
                    };

                    if self
                        .capture_suppressed_430_response(
                            &mut conn,
                            backend_id,
                            &stat_request,
                            buffer,
                        )
                        .await
                        .is_err()
                    {
                        conn.retire_with_cooldown();
                        return RetryStatProbeOutcome::Unavailable(backend_id);
                    }
                    let _ = conn.release();

                    if status_code.is_some_and(|status| status.as_u16() == 430) {
                        RetryStatProbeOutcome::Missing(backend_id)
                    } else {
                        RetryStatProbeOutcome::Present
                    }
                };

                match tokio::time::timeout(RETRY_STAT_SWEEP_PROBE_TIMEOUT, probe).await {
                    Ok(outcome) => outcome,
                    Err(_) => RetryStatProbeOutcome::Unavailable(backend_id),
                }
            });
        }

        while let Some(outcome) = probes.next().await {
            match outcome {
                RetryStatProbeOutcome::Missing(backend_id) => {
                    let missing = AuthoritativeArticleMissing { backend_id };
                    self.record_authoritative_article_missing(&missing, state.availability);
                    if let Some(msg_id) = request.message_id_value() {
                        self.cache.record_backend_missing(msg_id, backend_id).await;
                    }
                }
                RetryStatProbeOutcome::Present => {}
                RetryStatProbeOutcome::Unavailable(backend_id) => {
                    state.unavailable_backends.suppress(backend_id);
                }
            }
        }

        Ok(())
    }

    pub(super) fn spawn_non_primary_tier_stat_prefetch(
        &self,
        router: &Arc<BackendSelector>,
        request: &RequestContext,
        availability: &crate::cache::ArticleAvailability,
        unavailable_backends: SuppressedBackends,
    ) {
        if !matches!(
            request.kind(),
            RequestKind::Article | RequestKind::Body | RequestKind::Head
        ) || request.is_stat()
            || !request.has_message_id()
        {
            return;
        }

        let Some(stat_request) = Self::stat_probe_request(request) else {
            return;
        };
        let Some(msg_id_text) = request.message_id().map(str::to_owned) else {
            return;
        };

        let mut candidates = Vec::new();
        for tier in router.tiers().filter(|tier| *tier > 0) {
            for backend_id in router.backend_ids_in_tier(tier) {
                if unavailable_backends.contains(backend_id) || !availability.should_try(backend_id)
                {
                    continue;
                }
                let Some(provider) = router.backend_provider(backend_id) else {
                    continue;
                };
                if provider.stat_missing_enabled() {
                    candidates.push((backend_id, provider.clone()));
                }
            }
        }
        if candidates.is_empty() {
            return;
        }

        let router = Arc::clone(router);
        let cache = Arc::clone(&self.cache);
        let buffer_pool = self.buffer_pool.clone();
        tokio::spawn(async move {
            let mut probes = FuturesUnordered::new();
            for (backend_id, provider) in candidates {
                let router = Arc::clone(&router);
                let cache = Arc::clone(&cache);
                let stat_request = stat_request.clone();
                let msg_id_text = msg_id_text.clone();
                let buffer_pool = buffer_pool.clone();
                probes.push(async move {
                    let _guard = BackendSelector::guard_for_manual_backend(router, backend_id);
                    let conn = match provider.get_pooled_connection().await {
                        Ok(conn) => conn,
                        Err(_) => return,
                    };
                    let mut conn = crate::pool::ConnectionGuard::new(conn, provider);
                    let mut buffer = buffer_pool.acquire();
                    let read =
                        backend::send_request(&mut **conn.get_mut(), &stat_request, &mut buffer)
                            .await;
                    let status_code = match read {
                        Ok(read) => read.status_code(),
                        Err(_) => {
                            conn.retire_with_cooldown();
                            return;
                        }
                    };
                    if crate::session::backend::observe_response(
                        &stat_request,
                        &mut buffer,
                        &mut conn,
                        &buffer_pool,
                        backend_id,
                    )
                    .await
                    .is_err()
                    {
                        conn.retire_with_cooldown();
                        return;
                    }
                    let _ = conn.release();

                    if status_code.is_some_and(|status| status.as_u16() == 430)
                        && let Ok(msg_id) = crate::types::MessageId::new(msg_id_text)
                    {
                        cache.record_backend_missing(msg_id, backend_id).await;
                    }
                });
            }
            while probes.next().await.is_some() {}
        });
    }

    async fn write_successful_retry_response(
        &self,
        conn: crate::pool::ConnectionGuard,
        client_writer: &crate::session::SharedClientWriter,
        backend: &ArticleBackend,
        buffer: crate::pool::PooledBuffer,
        params: ResponseWriteParams<'_>,
        backend_connection: &mut Option<(BackendId, crate::pool::ConnectionGuard)>,
    ) -> Result<RequestResponseMetadata, SessionError> {
        let mut client_write = client_writer.lock().await;
        self.write_successful_backend_response(
            conn,
            &mut *client_write,
            backend,
            buffer,
            params,
            Some(backend_connection),
        )
        .await
    }

    pub(super) async fn prepare_backend_attempt(
        &self,
        provider: &crate::pool::DeadpoolConnectionProvider,
        backend: &ArticleBackend,
        request: &RequestContext,
        state: &mut ArticleAttemptState<'_>,
        is_retry_attempt: bool,
    ) -> Result<PreparedBackendAttempt, SessionError> {
        let backend_id = backend.backend_id();
        let request_wire_len = request.request_wire_len().get();
        trace!(
            client = %self.client_addr,
            backend = backend_id.as_index(),
            command_verb = ?request.verb(),
            msg_id = ?request.message_id_value(),
            request_wire_len,
            "Preparing direct backend attempt"
        );
        let (conn, read_status, buffer, timings) = match retry_once!(
            self.execute_backend_attempt(
                provider,
                backend,
                request,
                state.backend_connection,
                is_retry_attempt,
            )
            .await,
            client = self.client_addr,
            backend = backend_id.as_index()
        ) {
            Ok(result) => result,
            Err(err) => {
                state.unavailable_backends.suppress(backend_id);
                debug!(
                    client = %self.client_addr,
                    backend = backend_id.as_index(),
                    unavailable_backends = format_args!("{:08b}", state.unavailable_backends.bits()),
                    command_verb = ?request.verb(),
                    msg_id = ?request.message_id_value(),
                    error = %err,
                    "Backend attempt failed after retry; suppressing backend for this request"
                );
                return Ok(None);
            }
        };

        if let Some((ttfb, send, recv)) = timings {
            self.record_timing_metrics(backend_id, ttfb, send, recv);
        }
        *state.client_to_backend_bytes = state.client_to_backend_bytes.add(request_wire_len);

        let status_code = match read_status.status_code() {
            Some(status_code) => status_code,
            None => {
                read_status.log_warnings(&buffer, self.client_addr, backend_id);
                trace!(
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
                state.unavailable_backends.suppress(backend_id);
                return Ok(None);
            }
        };

        trace!(
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
        backend: &ArticleBackend,
        backend_bytes: crate::pool::PooledBuffer,
        params: ResponseWriteParams<'_>,
        backend_connection: Option<&mut Option<(BackendId, crate::pool::ConnectionGuard)>>,
    ) -> Result<RequestResponseMetadata, SessionError>
    where
        W: AsyncWrite + Unpin,
    {
        let backend_id = backend.backend_id();
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
            .write_response_to_client(&mut conn, client_write, backend, backend_bytes, params)
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
                let can_expand_or_use_idle = checkout_status.available > 0
                    || checkout_status.size < checkout_status.max_size;
                if Self::should_reuse_cached_batch_connection() && !can_expand_or_use_idle {
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
                        connection_type = guard.connection_type(),
                        pending_bytes = guard.pending_bytes_len(),
                        "Reusing cached batch connection for direct backend attempt"
                    );
                    guard
                } else if Self::should_reuse_cached_batch_connection() {
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
                        "Using idle pool capacity before reusing cached batch connection"
                    );
                    *backend_connection = Some((cached_backend_id, guard));
                    match provider.get_pooled_connection().await {
                        Ok(conn) => crate::pool::ConnectionGuard::new(conn, provider.clone()),
                        Err(err) => {
                            debug!(
                                client = %self.client_addr,
                                backend = backend_id.as_index(),
                                command_verb = ?request.verb(),
                                msg_id = ?request.message_id_value(),
                                error = %err,
                                "Idle pool checkout failed; falling back to cached batch connection"
                            );
                            let (_, cached_guard) = backend_connection.take().expect(
                                "cached backend connection should be present for fallback reuse",
                            );
                            cached_guard
                        }
                    }
                } else {
                    unreachable!("cached batch connection reuse should remain enabled");
                }
            }
            Some(cached) => {
                let (cached_backend_id, cached_conn) = cached;
                trace!(
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
                trace!(
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
                trace!(
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
                trace!(
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
                trace!(
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
        backend: &ArticleBackend,
        request: &RequestContext,
        backend_connection: &mut Option<(BackendId, crate::pool::ConnectionGuard)>,
        stat_probe_retry_only: bool,
    ) -> Result<ExecutedBackendAttempt> {
        let backend_id = backend.backend_id();
        let mut guard = self
            .checkout_direct_backend_connection(provider, backend_id, request, backend_connection)
            .await?;
        trace!(
            client = %self.client_addr,
            backend = backend_id.as_index(),
            command_verb = ?request.verb(),
            msg_id = ?request.message_id_value(),
            connection_type = guard.connection_type(),
            pending_bytes = guard.pending_bytes_len(),
            "Sending request to backend and waiting for classifiable response bytes"
        );
        let result = self
            .execute_and_read_response_with_optional_stat_probe(
                &mut guard,
                provider,
                backend,
                request,
                stat_probe_retry_only,
            )
            .await;

        match result {
            Ok((read_status, buffer, timings)) => Ok((guard, read_status, buffer, timings)),
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
        backend: &ArticleBackend,
        request: &RequestContext,
    ) -> Result<BackendReadAttempt, BackendReadAttemptError> {
        let backend_id = backend.backend_id();
        self.metrics.record_command(backend_id);
        self.metrics.user_command(self.username());

        let mut buffer = self.buffer_pool.acquire();
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

    #[inline]
    fn should_use_stat_missing_probe(
        provider: &crate::pool::DeadpoolConnectionProvider,
        request: &RequestContext,
        stat_probe_retry_only: bool,
    ) -> bool {
        provider.stat_missing_enabled()
            && stat_probe_retry_only
            && request.has_message_id()
            && !request.is_stat()
            && matches!(
                request.kind(),
                RequestKind::Article | RequestKind::Body | RequestKind::Head
            )
    }

    #[inline]
    fn stat_probe_request(request: &RequestContext) -> Option<RequestContext> {
        request
            .has_message_id()
            .then(|| RequestContext::from_verb_args(b"STAT", request.args()))
    }

    async fn execute_and_read_response_with_optional_stat_probe(
        &self,
        conn: &mut crate::stream::ConnectionStream,
        provider: &crate::pool::DeadpoolConnectionProvider,
        backend: &ArticleBackend,
        request: &RequestContext,
        stat_probe_retry_only: bool,
    ) -> Result<BackendReadAttempt, BackendReadAttemptError> {
        if !Self::should_use_stat_missing_probe(provider, request, stat_probe_retry_only) {
            return self.execute_and_read_response(conn, backend, request).await;
        }

        let Some(stat_request) = Self::stat_probe_request(request) else {
            return self.execute_and_read_response(conn, backend, request).await;
        };

        debug!(
            client = %self.client_addr,
            backend = backend.backend_id().as_index(),
            command_verb = ?request.verb(),
            msg_id = ?request.message_id_value(),
            "Running STAT miss probe before backend article fetch"
        );
        let (probe_response, probe_buffer, probe_timings) = self
            .execute_and_read_response(conn, backend, &stat_request)
            .await?;
        if probe_response
            .status_code()
            .is_some_and(|status| status.as_u16() == 430)
        {
            debug!(
                client = %self.client_addr,
                backend = backend.backend_id().as_index(),
                command_verb = ?request.verb(),
                msg_id = ?request.message_id_value(),
                "STAT miss probe returned 430; skipping backend article fetch"
            );
            return Ok((probe_response, probe_buffer, probe_timings));
        }

        self.execute_and_read_response(conn, backend, request).await
    }

    /// Write response from backend to client and handle caching.
    ///
    /// Returns `ResponseTransferError` so callers can decide the connection's pool fate
    /// without string/downcast inspection.
    async fn write_response_to_client<W>(
        &self,
        pooled_conn: &mut deadpool::managed::Object<crate::pool::deadpool_connection::TcpManager>,
        client_write: &mut W,
        backend: &ArticleBackend,
        backend_bytes: crate::pool::PooledBuffer,
        params: ResponseWriteParams<'_>,
    ) -> Result<u64, ResponseTransferError>
    where
        W: AsyncWrite + Unpin,
    {
        let backend_id = backend.backend_id();
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

        let backend_after_write = if matches!(cache_action, CacheAction::TrackAvailability) {
            self.apply_cache_action(cache_action, params, backend_id, None);
            None
        } else {
            Some(backend_id)
        };

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
                    (bytes, response)
                }
            };

        if let Some(backend) = backend_after_write {
            self.apply_cache_action(cache_action, params, backend, captured);
        }
        Ok(bytes_written)
    }

    fn apply_cache_action(
        &self,
        cache_action: CacheAction,
        params: ResponseWriteParams<'_>,
        backend: BackendId,
        captured: Option<crate::pool::ChunkedResponse>,
    ) {
        let backend_id = backend;
        match (cache_action, params.msg_id, captured) {
            (CacheAction::CaptureArticle, msg_id, Some(response)) => {
                self.maybe_cache_upsert_buffer(msg_id, response.into(), backend);
            }
            (CacheAction::TrackAvailability, Some(msg_id), _)
            | (CacheAction::CaptureArticle, Some(msg_id), None)
                if !params.request.cache_records_backend_has_article(backend_id) =>
            {
                self.spawn_cache_upsert_availability(
                    msg_id,
                    params.status_code,
                    backend,
                    self.tier_for_backend(backend_id),
                );
            }
            (CacheAction::TrackStat, msg_id, _)
                if self.cache_articles && self.cache.stores_payload_responses() =>
            {
                self.maybe_cache_upsert_buffer(
                    msg_id,
                    crate::cache::CacheIngestResponse::from(b"223\r\n".as_slice()),
                    backend,
                );
            }
            (CacheAction::TrackStat, Some(msg_id), _)
                if !params.request.cache_records_backend_has_article(backend_id) =>
            {
                self.spawn_cache_upsert_availability(
                    msg_id,
                    params.status_code,
                    backend,
                    self.tier_for_backend(backend_id),
                );
            }
            (CacheAction::None, _, _)
            | (CacheAction::TrackAvailability, _, _)
            | (CacheAction::TrackStat, _, _)
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
    ) -> Result<(u64, Option<crate::pool::ChunkedResponse>), ResponseTransferError>
    where
        W: AsyncWrite + Unpin,
    {
        let mut captured = crate::pool::ChunkedResponse::default();
        // Payload caching owns responses only while they stay within the
        // framer-owned retention limit. Larger responses are streamed through
        // without cache insertion so normal delivery and backend reuse can
        // continue.
        let (bytes_written, retained) =
            crate::session::backend::write_response_with_optional_capture(
                params.request,
                &mut backend_bytes,
                pooled_conn,
                client_write,
                &mut captured,
                &self.buffer_pool,
                backend_id,
            )
            .await?;
        drop(backend_bytes);
        self.log_body_response_written(backend_id, params, bytes_written as usize);
        if retained && let Some(msg_id_ref) = params.msg_id {
            debug!(
                "Client {} caching full article for {} ({} bytes captured)",
                self.client_addr,
                msg_id_ref,
                captured.len()
            );
        }
        if retained {
            Ok((bytes_written, Some(captured)))
        } else {
            debug!(
                client = %self.client_addr,
                backend = backend_id.as_index(),
                command_verb = ?params.request.verb(),
                status_code = params.status_code.as_u16(),
                bytes_written,
                "Response exceeded retention limit; streamed without cache insertion"
            );
            Ok((bytes_written, None))
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
        backend: BackendId,
    ) {
        if let Some(msg_id_ref) = msg_id {
            let backend_id = backend;
            let tier = self.tier_for_backend(backend_id);
            self.spawn_cache_upsert_buffer(msg_id_ref, data, backend, tier);
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
    use crate::cache::{ArticleAvailability, UnifiedCache};
    use crate::metrics::MetricsCollector;
    use crate::pool::{BufferPool, ConnectionGuard, DeadpoolConnectionProvider};
    use crate::protocol::{RequestContext, StatusCode};
    use crate::router::{ArticleBackend, BackendSelector, SuppressedBackends};
    use crate::session::SessionError;
    use crate::session::routing::CacheAction;
    use crate::types::{
        BackendId, BufferSize, ClientAddress, ClientToBackendBytes, MessageId, ServerName,
    };
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};
    use std::time::Duration;
    use tokio::io::AsyncWrite;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    fn eligible(backend_id: BackendId) -> ArticleBackend {
        let availability = ArticleAvailability::new();
        ArticleBackend::from_availability(backend_id, &availability)
            .expect("backend should be eligible")
    }
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

    fn test_session_with_cache(
        cache: Arc<UnifiedCache>,
        cache_articles: bool,
    ) -> crate::session::ClientSession {
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
        .with_cache(cache)
        .with_cache_articles(cache_articles)
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

    async fn spawn_stat_missing_probe_server() -> (u16, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let stat_commands = Arc::new(AtomicUsize::new(0));
        let body_commands = Arc::new(AtomicUsize::new(0));
        let stat_count = Arc::clone(&stat_commands);
        let body_count = Arc::clone(&body_commands);

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let stat_count = Arc::clone(&stat_count);
                let body_count = Arc::clone(&body_count);
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
                                } else if cmd.starts_with("STAT ") {
                                    stat_count.fetch_add(1, Ordering::SeqCst);
                                    let _ = write_half.write_all(b"430 Missing\r\n").await;
                                } else if cmd.starts_with("BODY") || cmd.starts_with("ARTICLE") {
                                    body_count.fetch_add(1, Ordering::SeqCst);
                                    let _ = write_half.write_all(
                                        b"222 0 <found@example.com> body follows\r\npayload\r\n.\r\n",
                                    ).await;
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

        (port, stat_commands, body_commands)
    }

    async fn spawn_article_response_then_close_server(
        response: &'static [u8],
    ) -> (u16, Arc<AtomicUsize>) {
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
                                    let _ = write_half.shutdown().await;
                                    break;
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

    async fn unused_local_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    }

    fn router_with_backend(provider: DeadpoolConnectionProvider) -> Arc<BackendSelector> {
        let mut router = BackendSelector::new();
        router.add_backend(
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
        let missing = AuthoritativeArticleMissing::from_status_code(
            eligible(backend_id),
            StatusCode::new(430),
        );
        assert_eq!(missing.map(|missing| missing.backend_id()), Ok(backend_id));

        assert!(
            AuthoritativeArticleMissing::from_status_code(
                eligible(backend_id),
                StatusCode::new(400)
            )
            .is_err()
        );
        assert!(
            AuthoritativeArticleMissing::from_status_code(
                eligible(backend_id),
                StatusCode::new(500)
            )
            .is_err()
        );
        assert!(
            AuthoritativeArticleMissing::from_status_code(
                eligible(backend_id),
                StatusCode::new(222)
            )
            .is_err()
        );
    }

    #[test]
    fn missing_availability_requires_authoritative_430_classification() {
        let session = test_session();
        let backend_id = BackendId::from_index(0);
        let mut availability = ArticleAvailability::new();

        assert!(
            AuthoritativeArticleMissing::from_status_code(
                eligible(backend_id),
                StatusCode::new(503)
            )
            .is_err()
        );
        assert_eq!(availability.missing_bits(), 0);
        assert_eq!(availability.missing_bits(), 0);

        let missing = AuthoritativeArticleMissing::from_status_code(
            eligible(backend_id),
            StatusCode::new(430),
        )
        .expect("430 is authoritative article-missing");
        session.record_authoritative_article_missing(&missing, &mut availability);

        assert_eq!(availability.missing_bits(), 0b0000_0001);
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
        let mut unavailable_backends = SuppressedBackends::empty();
        let mut state = ArticleAttemptState {
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
        };

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            session.try_backend_for_article(
                &router,
                &mut request,
                &client_writer,
                &mut state,
                false,
            ),
        )
        .await
        .expect("430 probe should not wait for the client writer lock")
        .expect("430 probe should complete without transport error");

        assert!(matches!(
            result,
            BackendAttemptResult::ArticleNotFound { missing }
                if missing.backend_id() == BackendId::from_index(0)
        ));
        assert!(availability.is_missing(BackendId::from_index(0)));
        assert_eq!(article_commands.load(Ordering::SeqCst), 1);

        drop(held_writer);
    }

    #[tokio::test]
    async fn stat_missing_probe_is_skipped_on_first_attempt() {
        let session = test_session();
        let (port, stat_commands, body_commands) = spawn_stat_missing_probe_server().await;
        let provider = DeadpoolConnectionProvider::builder("127.0.0.1", port)
            .max_connections(1)
            .stat_missing(true)
            .build()
            .unwrap();
        let router = router_with_backend(provider);
        let (client_writer, _client_read, _client_write) = shared_client_writer_pair().await;

        let mut request = request_context(b"BODY <missing@example.com>\r\n");
        let mut availability = ArticleAvailability::new();
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_connection = None;
        let mut unavailable_backends = SuppressedBackends::empty();
        let mut state = ArticleAttemptState {
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
        };

        let result = session
            .try_backend_for_article(&router, &mut request, &client_writer, &mut state, false)
            .await
            .expect("first attempt should be handled without transport error");

        assert!(matches!(result, BackendAttemptResult::Success));
        assert_eq!(stat_commands.load(Ordering::SeqCst), 0);
        assert_eq!(
            body_commands.load(Ordering::SeqCst),
            1,
            "first attempt should go directly to BODY/ARTICLE/HEAD without STAT probe"
        );
    }

    #[tokio::test]
    async fn retry_stat_sweep_marks_missing_across_multiple_backends() {
        let session = test_session();
        let (port0, stat0, body0) = spawn_stat_missing_probe_server().await;
        let (port1, stat1, body1) = spawn_stat_missing_probe_server().await;
        let router = router_with_tiered_backends([
            (
                DeadpoolConnectionProvider::builder("127.0.0.1", port0)
                    .max_connections(1)
                    .stat_missing(true)
                    .build()
                    .unwrap(),
                0,
            ),
            (
                DeadpoolConnectionProvider::builder("127.0.0.1", port1)
                    .max_connections(1)
                    .stat_missing(true)
                    .build()
                    .unwrap(),
                1,
            ),
        ]);

        let request = request_context(b"BODY <missing@example.com>\r\n");
        let mut availability = ArticleAvailability::new();
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_connection = None;
        let mut unavailable_backends = SuppressedBackends::empty();
        let mut state = ArticleAttemptState {
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
        };

        session
            .parallel_retry_stat_sweep(&router, &request, &mut state)
            .await
            .expect("parallel retry STAT sweep should succeed");

        assert!(availability.is_missing(BackendId::from_index(0)));
        assert!(availability.is_missing(BackendId::from_index(1)));
        assert_eq!(stat0.load(Ordering::SeqCst), 1);
        assert_eq!(stat1.load(Ordering::SeqCst), 1);
        assert_eq!(body0.load(Ordering::SeqCst), 0);
        assert_eq!(body1.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn retry_stat_sweep_leaves_pending_counts_balanced() {
        let session = test_session();
        let (port0, _stat0, _body0) = spawn_stat_missing_probe_server().await;
        let (port1, _stat1, _body1) = spawn_stat_missing_probe_server().await;
        let router = router_with_tiered_backends([
            (
                DeadpoolConnectionProvider::builder("127.0.0.1", port0)
                    .max_connections(1)
                    .stat_missing(true)
                    .build()
                    .unwrap(),
                0,
            ),
            (
                DeadpoolConnectionProvider::builder("127.0.0.1", port1)
                    .max_connections(1)
                    .stat_missing(true)
                    .build()
                    .unwrap(),
                1,
            ),
        ]);

        let request = request_context(b"BODY <missing@example.com>\r\n");
        let mut availability = ArticleAvailability::new();
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_connection = None;
        let mut unavailable_backends = SuppressedBackends::empty();
        let mut state = ArticleAttemptState {
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
        };

        session
            .parallel_retry_stat_sweep(&router, &request, &mut state)
            .await
            .expect("parallel retry STAT sweep should succeed");

        assert_eq!(
            router
                .backend_load(BackendId::from_index(0))
                .expect("backend 0 should exist")
                .get(),
            0
        );
        assert_eq!(
            router
                .backend_load(BackendId::from_index(1))
                .expect("backend 1 should exist")
                .get(),
            0
        );
    }

    #[tokio::test]
    async fn first_attempt_prefetches_stat_only_on_non_primary_tiers_with_stat_missing_enabled() {
        let session = test_session();
        let (port0, stat0, body0) = spawn_stat_missing_probe_server().await;
        let (port1, stat1, body1) = spawn_stat_missing_probe_server().await;
        let (port2, stat2, body2) = spawn_stat_missing_probe_server().await;
        let router = router_with_tiered_backends([
            (
                DeadpoolConnectionProvider::builder("127.0.0.1", port0)
                    .max_connections(1)
                    .stat_missing(true)
                    .build()
                    .unwrap(),
                0,
            ),
            (
                DeadpoolConnectionProvider::builder("127.0.0.1", port1)
                    .max_connections(1)
                    .stat_missing(true)
                    .build()
                    .unwrap(),
                1,
            ),
            (
                DeadpoolConnectionProvider::builder("127.0.0.1", port2)
                    .max_connections(1)
                    .stat_missing(false)
                    .build()
                    .unwrap(),
                2,
            ),
        ]);

        let request = request_context(b"BODY <missing@example.com>\r\n");
        let availability = ArticleAvailability::new();
        let unavailable_backends = SuppressedBackends::empty();

        session.spawn_non_primary_tier_stat_prefetch(
            &router,
            &request,
            &availability,
            unavailable_backends,
        );

        tokio::time::timeout(Duration::from_secs(1), async {
            while stat1.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("upper-tier stat prefetch should complete");

        assert_eq!(stat0.load(Ordering::SeqCst), 0, "tier 0 must not be probed");
        assert_eq!(
            stat1.load(Ordering::SeqCst),
            1,
            "tier >0 with stat_missing=1 is probed"
        );
        assert_eq!(
            stat2.load(Ordering::SeqCst),
            0,
            "tier >0 with stat_missing=0 is skipped"
        );
        assert_eq!(body0.load(Ordering::SeqCst), 0);
        assert_eq!(body1.load(Ordering::SeqCst), 0);
        assert_eq!(body2.load(Ordering::SeqCst), 0);

        let msg_id = MessageId::new("<missing@example.com>".to_string()).expect("valid message id");
        let cached = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(cached) = session.cache.get(&msg_id).await {
                    break cached;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("prefetch should update availability index");
        let backend_1_missing = cached.availability().is_missing(BackendId::from_index(1));
        assert!(
            backend_1_missing,
            "tier-1 backend should be marked missing from STAT=430"
        );
    }

    #[tokio::test]
    async fn first_attempt_prefetch_respects_existing_availability_and_skips_known_missing_tier() {
        let session = test_session();
        let (port0, stat0, _body0) = spawn_stat_missing_probe_server().await;
        let (port1, stat1, _body1) = spawn_stat_missing_probe_server().await;
        let router = router_with_tiered_backends([
            (
                DeadpoolConnectionProvider::builder("127.0.0.1", port0)
                    .max_connections(1)
                    .stat_missing(true)
                    .build()
                    .unwrap(),
                0,
            ),
            (
                DeadpoolConnectionProvider::builder("127.0.0.1", port1)
                    .max_connections(1)
                    .stat_missing(true)
                    .build()
                    .unwrap(),
                1,
            ),
        ]);

        let request = request_context(b"BODY <already-missing@example.com>\r\n");
        let mut availability = ArticleAvailability::new();
        availability.record_missing(BackendId::from_index(1));

        session.spawn_non_primary_tier_stat_prefetch(
            &router,
            &request,
            &availability,
            SuppressedBackends::empty(),
        );

        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(stat0.load(Ordering::SeqCst), 0, "tier 0 must not be probed");
        assert_eq!(
            stat1.load(Ordering::SeqCst),
            0,
            "known-missing non-primary tier should be skipped by should_try gate"
        );
    }

    #[tokio::test]
    async fn first_attempt_prefetches_stat_for_head_requests_on_non_primary_tiers() {
        let session = test_session();
        let (port0, stat0, _body0) = spawn_stat_missing_probe_server().await;
        let (port1, stat1, _body1) = spawn_stat_missing_probe_server().await;
        let router = router_with_tiered_backends([
            (
                DeadpoolConnectionProvider::builder("127.0.0.1", port0)
                    .max_connections(1)
                    .stat_missing(true)
                    .build()
                    .unwrap(),
                0,
            ),
            (
                DeadpoolConnectionProvider::builder("127.0.0.1", port1)
                    .max_connections(1)
                    .stat_missing(true)
                    .build()
                    .unwrap(),
                1,
            ),
        ]);

        let request = request_context(b"HEAD <missing@example.com>\r\n");
        let availability = ArticleAvailability::new();
        session.spawn_non_primary_tier_stat_prefetch(
            &router,
            &request,
            &availability,
            SuppressedBackends::empty(),
        );

        tokio::time::timeout(Duration::from_secs(1), async {
            while stat1.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("HEAD prefetch should probe non-primary tier");

        assert_eq!(stat0.load(Ordering::SeqCst), 0, "tier 0 must not be probed");
        assert_eq!(stat1.load(Ordering::SeqCst), 1, "tier >0 should be probed");
    }

    #[test]
    fn stat_missing_probe_requires_retry_state() {
        let request = request_context(b"BODY <missing@example.com>\r\n");
        let provider = DeadpoolConnectionProvider::builder("127.0.0.1", 119)
            .stat_missing(true)
            .build()
            .expect("provider should build");

        assert!(!super::ClientSession::should_use_stat_missing_probe(
            &provider, &request, false,
        ));
        assert!(super::ClientSession::should_use_stat_missing_probe(
            &provider, &request, true,
        ));
    }

    #[test]
    fn direct_checkout_prefers_cached_batch_connection() {
        assert!(super::ClientSession::should_reuse_cached_batch_connection());
    }

    #[tokio::test]
    async fn track_stat_records_availability_when_article_cache_disabled() {
        let cache = Arc::new(UnifiedCache::memory(1024, Duration::from_secs(60)));
        let session = test_session_with_cache(cache.clone(), false);
        let request = request_context(b"STAT <stat-disabled@example.com>\r\n");
        let msg_id =
            MessageId::new("<stat-disabled@example.com>".to_string()).expect("valid message id");

        session.apply_cache_action(
            CacheAction::TrackStat,
            ResponseWriteParams {
                request: &request,
                msg_id: Some(&msg_id),
                status_code: StatusCode::new(223),
            },
            BackendId::from_index(0),
            None,
        );

        let cached = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(cached) = cache.get(&msg_id).await {
                    break cached;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("availability update should be recorded");

        assert_eq!(cached.status_code(), StatusCode::new(223));
        assert_eq!(cached.payload_len().get(), 0);
        assert!(!cached.has_availability_info());
        assert_eq!(cached.availability().missing_bits(), 0);
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
        let mut unavailable_backends = SuppressedBackends::empty();
        let mut state = ArticleAttemptState {
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
        };

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            session.try_backend_for_article(
                &router,
                &mut request,
                &client_writer,
                &mut state,
                false,
            ),
        )
        .await
        .expect("invalid response attempt should not hang")
        .expect("invalid response attempt should be handled as unavailable");

        assert!(matches!(result, BackendAttemptResult::BackendUnavailable));
        assert_eq!(article_commands.load(Ordering::SeqCst), 1);
        assert_eq!(availability.missing_bits(), 0);
        assert_eq!(availability.missing_bits(), 0);
        assert_eq!(unavailable_backends.bits(), 0b0000_0001);
    }

    #[tokio::test]
    async fn connection_failure_suppresses_backend_for_current_request() {
        let session = test_session();
        let port = unused_local_port().await;
        let provider = DeadpoolConnectionProvider::builder("127.0.0.1", port)
            .max_connections(1)
            .build()
            .unwrap();
        let router = router_with_backend(provider);
        let (client_writer, _client_read, _client_write) = shared_client_writer_pair().await;

        let mut request = request_context(b"BODY <connection-failure@example.com>\r\n");
        let mut availability = ArticleAvailability::new();
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_connection = None;
        let mut unavailable_backends = SuppressedBackends::empty();
        let mut state = ArticleAttemptState {
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
        };

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            session.try_backend_for_article(
                &router,
                &mut request,
                &client_writer,
                &mut state,
                false,
            ),
        )
        .await
        .expect("connection failure attempt should not hang")
        .expect("connection failure should be handled as unavailable");

        assert!(matches!(result, BackendAttemptResult::BackendUnavailable));
        assert_eq!(availability.missing_bits(), 0);
        assert_eq!(availability.missing_bits(), 0);
        assert_eq!(unavailable_backends.bits(), 0b0000_0001);
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
        let mut unavailable_backends = SuppressedBackends::empty();
        let mut state = ArticleAttemptState {
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
        };

        let first = session
            .try_backend_for_article(&router, &mut request, &client_writer, &mut state, false)
            .await
            .expect("invalid response should be handled");
        assert!(matches!(first, BackendAttemptResult::BackendUnavailable));
        assert_eq!(state.availability.missing_bits(), 0);
        assert_eq!(state.availability.missing_bits(), 0);
        assert_eq!(state.unavailable_backends.bits(), 0b0000_0001);

        let second = session
            .try_backend_for_article(&router, &mut request, &client_writer, &mut state, true)
            .await
            .expect("same-tier retry should succeed");
        assert!(matches!(second, BackendAttemptResult::Success));
        assert_eq!(bad_article_commands.load(Ordering::SeqCst), 1);
        assert_eq!(good_article_commands.load(Ordering::SeqCst), 1);
        assert_eq!(availability.missing_bits(), 0);
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
    async fn backend_eof_after_status_suppresses_backend_for_current_request() {
        let session = test_session();
        let (bad_port, bad_article_commands) = spawn_article_response_then_close_server(
            b"222 0 <found@example.com> body follows\r\npartial body\r\n",
        )
        .await;
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
        let (client_writer, _client_read, _client_write) = shared_client_writer_pair().await;

        let mut request = request_context(b"BODY <found@example.com>\r\n");
        let mut availability = ArticleAvailability::new();
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_connection = None;
        let mut unavailable_backends = SuppressedBackends::empty();
        let mut state = ArticleAttemptState {
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
        };

        let first = session
            .try_backend_for_article(&router, &mut request, &client_writer, &mut state, false)
            .await;
        assert!(matches!(first, Err(SessionError::Backend(_))));
        assert_eq!(state.availability.missing_bits(), 0);
        assert_eq!(state.availability.missing_bits(), 0);
        assert_eq!(state.unavailable_backends.bits(), 0b0000_0001);

        let second = session
            .try_backend_for_article(&router, &mut request, &client_writer, &mut state, true)
            .await
            .expect("same-tier retry should continue after backend EOF");
        assert!(matches!(second, BackendAttemptResult::Success));
        assert_eq!(bad_article_commands.load(Ordering::SeqCst), 1);
        assert_eq!(good_article_commands.load(Ordering::SeqCst), 1);
        assert_eq!(request.backend_id(), Some(BackendId::from_index(1)));
    }

    #[tokio::test]
    async fn uncaptured_article_cache_action_records_availability() {
        let addr: std::net::SocketAddr = "127.0.0.1:9999".parse().unwrap();
        let buffer_pool = BufferPool::new(BufferSize::try_new(8192).unwrap(), 4);
        let auth_handler = Arc::new(AuthHandler::new(None, None).unwrap());
        let metrics = MetricsCollector::new(1);
        let cache = Arc::new(crate::cache::UnifiedCache::memory(
            1000,
            Duration::from_secs(60),
        ));
        let session = crate::session::ClientSession::builder(
            ClientAddress::from(addr),
            buffer_pool,
            auth_handler,
            metrics,
        )
        .with_cache(cache)
        .with_cache_articles(true)
        .build();
        let backend_id = BackendId::from_index(0);
        let request = request_context(b"ARTICLE <large@example.com>\r\n");
        let msg_id = crate::types::MessageId::new("<large@example.com>".to_string()).unwrap();

        session.apply_cache_action(
            crate::session::routing::CacheAction::CaptureArticle,
            ResponseWriteParams {
                request: &request,
                msg_id: Some(&msg_id),
                status_code: StatusCode::new(220),
            },
            backend_id,
            None,
        );

        let cached = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(cached) = session.cache.get(&msg_id).await {
                    break cached;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("availability update should be recorded");

        assert_eq!(cached.status_code(), StatusCode::new(220));
        assert_eq!(cached.payload_len().get(), 0);
        assert!(!cached.has_availability_info());
        assert_eq!(cached.availability().missing_bits(), 0);
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
        let mut unavailable_backends = SuppressedBackends::empty();
        let mut state = ArticleAttemptState {
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
        };

        for (index, expected_mask) in [0b0000_0001, 0b0000_0011].into_iter().enumerate() {
            let result = session
                .try_backend_for_article(
                    &router,
                    &mut request,
                    &client_writer,
                    &mut state,
                    index > 0,
                )
                .await
                .expect("invalid response should be handled");
            assert!(matches!(result, BackendAttemptResult::BackendUnavailable));
            assert_eq!(state.unavailable_backends.bits(), expected_mask);
        }

        let exhausted = session
            .try_backend_for_article(&router, &mut request, &client_writer, &mut state, true)
            .await
            .expect("tier exhaustion should be reported without transport failure");
        assert!(matches!(
            exhausted,
            BackendAttemptResult::NoRetryableBackend
        ));
        assert_eq!(bad0_article_commands.load(Ordering::SeqCst), 1);
        assert_eq!(bad1_article_commands.load(Ordering::SeqCst), 1);
        assert_eq!(backup_article_commands.load(Ordering::SeqCst), 0);
        assert_eq!(availability.missing_bits(), 0);
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
        let mut unavailable_backends = SuppressedBackends::empty();
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
            false,
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
        assert_eq!(availability.missing_bits(), 0);
        assert_eq!(availability.missing_bits(), 0);
        assert_eq!(request.backend_id(), Some(BackendId::from_index(0)));

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
                &eligible(BackendId::from_index(0)),
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
                &eligible(BackendId::from_index(0)),
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
