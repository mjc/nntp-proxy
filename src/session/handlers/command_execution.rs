//! Backend command execution and response writing
//!
//! Handles executing commands on individual backends, including connection retry,
//! response validation, and writing backend responses to clients.

use super::BackendLease;

use crate::config::LowerTierStatPrefetch;
use crate::pool::admission::{ArticleLookupOutcome, FallbackOutcome};
use crate::protocol::{RequestContext, RequestKind, RequestResponseMetadata, StatusCode};
use crate::router::{ArticleBackend, BackendSelector, SuppressedBackends};
use crate::session::SessionError;
use crate::session::response_transfer::{
    BackendConnectionOutcome, ResponseConnectionReuse, ResponseTransferError,
};
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
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tracing::{debug, trace, warn};

const BACKEND_TIMING_SAMPLE_MASK: u64 = 0x0f;
static BACKEND_TIMING_SAMPLE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[inline]
pub(crate) fn should_sample_backend_timing() -> bool {
    BACKEND_TIMING_SAMPLE_COUNTER.fetch_add(1, Ordering::Relaxed) & BACKEND_TIMING_SAMPLE_MASK == 0
}

/// Result of attempting to execute a routed lookup on one backend.
pub(super) enum BackendAttemptResult {
    /// Backend response was accepted and written successfully.
    Success,
    /// Backend returned authoritative article-missing status (430).
    ///
    /// The response has been read and drained; retry may continue only within
    /// tier and availability constraints.
    ArticleNotFound {
        missing: AuthoritativeArticleMissing,
    },
    /// Backend was unavailable or returned an error that allows another attempt.
    ///
    /// This includes non-430 error replies to message-id lookups: they say the
    /// backend cannot serve this request now, not that the article is missing,
    /// so they suppress the backend for this request without recording a miss.
    BackendUnavailable,
    /// No backend is retryable without violating tier or availability rules.
    NoRetryableBackend,
    /// The request used its whole deadline before a backend answered.
    DeadlineExpired,
}

/// Mutable state for an article backend attempt loop
///
/// Groups the mutable parameters that track retry state across
/// multiple `try_backend_for_article` calls.
pub(super) struct ArticleAttemptState<'a> {
    pub request_origin: ArticleRequestOrigin,
    pub availability: &'a mut crate::cache::ArticleAvailability,
    pub client_to_backend_bytes: &'a mut ClientToBackendBytes,
    pub backend_connection: &'a mut Option<BackendLease>,
    pub unavailable_backends: &'a mut SuppressedBackends,
    pub backlogged: &'a mut BackloggedBackends,
    pub budget: crate::pool::admission::ArticleBudget,
}

/// Who already accounted for the client's command before this lookup ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ArticleRequestOrigin {
    /// A new client request; its command bytes are charged to the user here.
    Frontend,
    /// Recovery after a pinned hybrid reader backend answered `430`. The
    /// stateful loop already charged the user for the command it forwarded.
    ReaderAfterPinnedMiss,
}

/// Whether a request may still queue on a backend it skipped as backlogged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum LastResort {
    #[default]
    Available,
    /// A last-resort wait already timed out. The backends it would pick next
    /// are at least as busy, and the deadline has mostly gone into the wait,
    /// so the request continues as if last resorts were off.
    Spent,
}

/// Backends one request skipped as backlogged, kept apart from backends that
/// failed or answered `430` so only the skipped ones can be a last resort.
#[derive(Debug, Default)]
pub(super) struct BackloggedBackends {
    skipped: SuppressedBackends,
    waited: SuppressedBackends,
    last_resort: LastResort,
    floor: FloorDecisions,
}

/// One request's rescue-floor decisions, at most one per backend.
///
/// A request meets an under-floor backend in several places (the `STAT`
/// sweep, a direct attempt, the last-resort pick). Counting a pass-over in
/// each would tick the probe counter several times for one request, so the
/// documented one-probe-in-16-pass-overs rate would not hold; deciding once
/// and remembering the outcome keeps it.
#[derive(Debug, Default)]
struct FloorDecisions {
    /// Backends the request already decided the floor for.
    decided: SuppressedBackends,
    /// Of those, the backends it passed over: an idle-only checkout found no
    /// free connection and was not let through as a probe, or the probe was
    /// then skipped as backlogged.
    passed_over: SuppressedBackends,
}

/// How the rescue floor treats a backend for the rest of one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FloorDecision {
    /// Not decided yet; the next checkout decides.
    Undecided,
    /// Decided without a pass-over; treat it like any other backend.
    Used,
    /// Passed over; a last-resort pick treats it as an unlikely rescuer.
    PassedOver,
}

impl BackloggedBackends {
    /// Whether any skipped backend could still be waited on. One branch on
    /// the common path where nothing was skipped.
    fn has_candidates(&self) -> bool {
        self.last_resort == LastResort::Available && self.skipped.bits() & !self.waited.bits() != 0
    }

    /// Backends this request skipped as backlogged.
    pub(super) const fn skipped(&self) -> SuppressedBackends {
        self.skipped
    }

    fn is_candidate(&self, backend_id: BackendId) -> bool {
        self.skipped.contains(backend_id) && !self.waited.contains(backend_id)
    }

    fn start_wait(&mut self, backend_id: BackendId) {
        self.waited.suppress(backend_id);
    }

    fn floor_decision(&self, backend_id: BackendId) -> FloorDecision {
        if !self.floor.decided.contains(backend_id) {
            FloorDecision::Undecided
        } else if self.floor.passed_over.contains(backend_id) {
            FloorDecision::PassedOver
        } else {
            FloorDecision::Used
        }
    }

    /// Record the request's one rescue-floor decision for `backend_id`.
    fn decide_floor(&mut self, backend_id: BackendId, decision: FloorDecision) {
        self.floor.decided.suppress(backend_id);
        if decision == FloorDecision::PassedOver {
            self.floor.passed_over.suppress(backend_id);
        }
    }

    /// A backlog skip of `backend_id`, which is then a last-resort candidate.
    /// The skip of an idle-only checkout is the request's floor pass-over.
    fn note_backlog_skip(
        &mut self,
        backend_id: BackendId,
        budget: crate::pool::admission::CheckoutBudget,
    ) {
        self.skipped.suppress(backend_id);
        if budget.is_idle_only() {
            self.floor.passed_over.suppress(backend_id);
        }
    }

    /// Track why a checkout on `backend_id` failed: a backlog skip makes the
    /// backend a last-resort candidate, and a timed-out last-resort wait
    /// ends last resorts for the request.
    fn note_checkout_failure(
        &mut self,
        backend_id: BackendId,
        budget: crate::pool::admission::CheckoutBudget,
        error: &anyhow::Error,
    ) {
        use crate::connection_error::ConnectionError;
        match error.downcast_ref::<ConnectionError>() {
            Some(ConnectionError::Backlogged { .. }) => self.note_backlog_skip(backend_id, budget),
            Some(ConnectionError::PoolExhausted { .. }) if self.waited.contains(backend_id) => {
                self.last_resort = LastResort::Spent;
            }
            _ => {}
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum RetryAttemptKind {
    Direct,
}

impl RetryAttemptKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct retry",
        }
    }
}

pub(super) struct AuthoritativeArticleMissing {
    backend_id: BackendId,
    availability_slot: crate::cache::AvailabilitySlot,
}

impl AuthoritativeArticleMissing {
    pub(super) fn from_status_code(
        backend: ArticleBackend,
        status_code: StatusCode,
    ) -> Result<Self, ArticleBackend> {
        if status_code.as_u16() == 430 {
            Ok(Self {
                backend_id: backend.backend_id(),
                availability_slot: backend.availability_slot(),
            })
        } else {
            Err(backend)
        }
    }

    pub(super) const fn backend_id(&self) -> BackendId {
        self.backend_id
    }

    pub(super) const fn availability_slot(&self) -> crate::cache::AvailabilitySlot {
        self.availability_slot
    }
}

/// How a non-430 error reply to a message-id lookup leaves the backend session.
///
/// Some providers refuse lookups they cannot serve with codes other than 430
/// (for example `451 0 <message-id>`). Forwarding such a reply ends
/// the request even when another backend has the article, and recording it as
/// a 430 would poison availability for later requests, so the retry loop treats
/// it like a busy backend: suppressed for this request only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ArticleErrorReply {
    /// The provider declined this lookup but the session can serve the next one.
    SessionUsable,
    /// The reply says the session itself is unusable: the service is closing
    /// (400) or the pre-authenticated session lost its authorization (48x,
    /// 502). Reusing it would fail the next request the same way.
    SessionUnusable,
}

impl ArticleErrorReply {
    pub(super) fn classify(request: &RequestContext, status_code: StatusCode) -> Option<Self> {
        use crate::protocol::codes;

        if crate::command::CommandHandler::article_lookup_request(request).is_none()
            || !status_code.is_error()
            || status_code.is_article_missing()
        {
            return None;
        }
        Some(match status_code.as_u16() {
            codes::SERVICE_UNAVAILABLE
            | codes::AUTH_REQUIRED
            | codes::AUTH_REJECTED
            | codes::AUTH_OUT_OF_SEQUENCE
            | codes::ENCRYPTION_REQUIRED
            | codes::ACCESS_DENIED => Self::SessionUnusable,
            _ => Self::SessionUsable,
        })
    }
}

type BackendTimings = (u64, u64, u64);

/// Where an attempt gets its backend connection: the client's cached batch
/// lease when it is for the same backend, otherwise a pool checkout bounded by
/// `budget`.
pub(super) struct BackendAttemptConnection<'a> {
    pub(super) lease: &'a mut Option<BackendLease>,
    pub(super) budget: crate::pool::admission::CheckoutBudget,
}

/// How one direct attempt checks out its connection.
pub(super) struct AttemptCheckout {
    pub(super) budget: crate::pool::admission::CheckoutBudget,
    pub(super) is_retry_attempt: bool,
}

/// Parameters describing the response to write to the client
#[derive(Clone, Copy)]
pub(super) struct ResponseWriteParams<'a> {
    pub(super) request: &'a RequestContext,
    pub(super) article_request: Option<crate::command::ArticleLookupRequest<'a>>,
    pub(super) status_code: StatusCode,
}

/// Request-side context for writing a response whose status is owned by the
/// framed response operation. Keeping status out of this context prevents a
/// caller from pairing a response with an independently supplied status.
#[derive(Clone, Copy)]
pub(super) struct ResponseWriteContext<'a> {
    request_origin: ArticleRequestOrigin,
    request: &'a RequestContext,
    article_request: Option<crate::command::ArticleLookupRequest<'a>>,
}

/// Result of consuming one backend response for the client.
///
/// Returning the status with the byte count keeps the response metadata tied
/// to the same operation that consumed the framed response.
struct ResponseWriteResult {
    status_code: StatusCode,
    bytes_written: u64,
}

struct InvalidBackendResponseContext<'a> {
    provider: &'a crate::pool::DeadpoolConnectionProvider,
    request: &'a RequestContext,
    response: crate::session::multiline_framing::InvalidBackendResponse,
}

/// A successfully classified response together with the guard that owns its
/// backend exchange. Keeping these fields in one state object prevents status,
/// bytes, and connection ownership from being accidentally mixed at a call
/// site.
pub(super) struct PreparedBackendAttempt<'pool> {
    exchange: crate::session::backend::BackendResponseExchange<'pool>,
    timings: Option<BackendTimings>,
    /// When the command was sent, after any checkout wait: the start of the
    /// time this request kept the backend connection busy.
    sent_at: std::time::Instant,
}

impl PreparedBackendAttempt<'_> {
    fn status_code(&self) -> StatusCode {
        self.exchange
            .status_code()
            .expect("prepared backend attempts contain a classified response")
    }

    async fn observe(self) -> Result<crate::pool::ConnectionGuard, SessionError> {
        let Self { exchange, .. } = self;
        exchange
            .observe_and_reuse()
            .await
            .map_err(SessionError::from)
    }

    #[cfg(test)]
    fn for_test(
        conn: crate::pool::ConnectionGuard,
        response: crate::session::backend::ClassifiedResponse,
        pool: &crate::pool::BufferPool,
        backend_id: BackendId,
    ) -> PreparedBackendAttempt<'_> {
        PreparedBackendAttempt {
            exchange: crate::session::backend::BackendResponseExchange::new_for_test(
                conn, response, pool, backend_id,
            ),
            timings: None,
            sent_at: std::time::Instant::now(),
        }
    }
}

enum BackendReadAttemptError {
    Backend(anyhow::Error),
}

pub(super) enum AlreadySentResponseError {
    Read(anyhow::Error),
    Transfer(SessionError),
}

impl From<SessionError> for BackendReadAttemptError {
    fn from(err: SessionError) -> Self {
        Self::Backend(anyhow::Error::new(err))
    }
}

enum RetryStatProbeOutcome {
    Missing(BackendId),
    Present,
    Unavailable(BackendId),
    /// Skipped as backlogged: unavailable to this request like any busy
    /// backend, but still a last-resort candidate for the request itself.
    Backlogged {
        backend_id: BackendId,
        budget: crate::pool::admission::CheckoutBudget,
    },
}

/// Classify a client write failure after the backend response is owned.
///
/// A disconnect can leave a fully read backend connection reusable. Any other
/// write failure retires that connection.
fn classify_response_write_err(e: std::io::Error) -> ResponseTransferError {
    ResponseTransferError::from_client_write(e)
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
    fn cached_batch_connection_is_healthy(
        &self,
        guard: &mut crate::pool::ConnectionGuard,
        backend_id: BackendId,
        request: &RequestContext,
    ) -> bool {
        match crate::pool::health_check::check_tcp_alive(guard.health_check_stream_mut()) {
            Ok(()) => true,
            Err(err) => {
                warn!(
                    client = %self.client_addr,
                    backend = backend_id.as_index(),
                    command_verb = ?request.verb(),
                    msg_id = ?request.message_id_value(),
                    connection_type = guard.connection_type(),
                    pending_bytes = guard.pending_bytes_len(),
                    error = %err,
                    "Cached batch connection failed pre-reuse health check; retiring it"
                );
                false
            }
        }
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
    pub(super) async fn try_backend_for_article<W: AsyncWrite + Unpin>(
        &self,
        router: &Arc<BackendSelector>,
        request: &mut RequestContext,
        client_writer: &mut W,
        state: &mut ArticleAttemptState<'_>,
        is_retry_attempt: bool,
    ) -> Result<BackendAttemptResult, SessionError> {
        // Only message-ID requests are numbering-independent. Anything else
        // (for example LIST ACTIVE and LIST COUNTS) can carry article numbers,
        // so it must come from a backend whose numbering matches stateful sessions.
        let excluded_backends = if request.has_message_id() {
            SuppressedBackends::empty()
        } else {
            router.stateful_exclusions()
        };
        let (suppressed, checkout) = match Self::last_resort_backend(
            router,
            state,
            excluded_backends,
        ) {
            Some(backend_id) => {
                state.backlogged.start_wait(backend_id);
                debug!(
                    client = %self.client_addr,
                    backend = backend_id.as_index(),
                    command_verb = ?request.verb(),
                    msg_id = ?request.message_id_value(),
                    "Every backend left in the tier is backlogged; queueing on the shortest wait"
                );
                (
                    SuppressedBackends::all_except(backend_id),
                    state.budget.last_resort_checkout(),
                )
            }
            None => {
                if let Some(delay) = router.queue_backpressure_delay_for_article(
                    state.availability,
                    state.unavailable_backends.union(excluded_backends),
                ) {
                    tokio::time::sleep(delay).await;
                }
                (*state.unavailable_backends, state.budget.checkout())
            }
        };
        let lookup = if crate::command::CommandHandler::article_lookup_request(request).is_some() {
            crate::router::RouteLookup::Article
        } else {
            crate::router::RouteLookup::Other
        };
        let route_request = crate::router::RouteRequest::new(self.client_id)
            .with_availability(state.availability)
            .for_lookup(lookup)
            .suppressing_backends(suppressed)
            .excluding_backends(excluded_backends);
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
        if state.budget.is_expired() {
            return Ok(self.article_deadline_expired(provider, backend_id, request));
        }
        // A lookup after another backend already answered 430 measures how
        // often this backend rescues a miss.
        let article_lookup =
            crate::command::CommandHandler::article_lookup_request(request).is_some();
        let fallback_lookup = article_lookup && state.availability.missing_bits() != 0;
        let checkout = Self::rescue_floor_checkout(
            router,
            provider,
            backend_id,
            state.backlogged,
            fallback_lookup,
            checkout,
        );

        let Some(attempt) = self
            .prepare_backend_attempt(
                provider,
                &backend,
                request,
                state,
                AttemptCheckout {
                    budget: checkout,
                    is_retry_attempt,
                },
            )
            .await?
        else {
            // A checkout cut short by the deadline ends the request here, so
            // the expiry is counted against the backend that used it up.
            if state.budget.is_expired() {
                return Ok(self.article_deadline_expired(provider, backend_id, request));
            }
            return Ok(BackendAttemptResult::BackendUnavailable);
        };

        let status_code = attempt.status_code();

        if let Ok(missing) = AuthoritativeArticleMissing::from_status_code(backend, status_code) {
            trace!(
                client = %self.client_addr,
                backend = backend_id.as_index(),
                command_verb = ?request.verb(),
                msg_id = ?request.message_id_value(),
                "Direct backend attempt returned 430 before writing response"
            );
            if fallback_lookup {
                provider.note_fallback_lookup(FallbackOutcome::NotFound);
            }
            if article_lookup {
                provider.note_article_lookup(ArticleLookupOutcome::Missing);
            }
            self.record_authoritative_article_missing(&missing, state.availability);
            if let Some(article_request) =
                crate::command::CommandHandler::article_lookup_request(request)
            {
                self.cache
                    .record_availability_missing(
                        article_request.message_id(),
                        missing.availability_slot(),
                    )
                    .await;
            }
            let conn = attempt.observe().await?;
            self.release_or_reuse_connection(
                conn,
                backend_id,
                request,
                Some(state.backend_connection),
            );
            return Ok(BackendAttemptResult::ArticleNotFound { missing });
        }

        if let Some(error_reply) = ArticleErrorReply::classify(request, status_code) {
            // A refusal of the lookup itself says the backend will not serve
            // the article; a broken session says nothing about the article.
            if error_reply == ArticleErrorReply::SessionUsable {
                if fallback_lookup {
                    provider.note_fallback_lookup(FallbackOutcome::NotFound);
                }
                if article_lookup {
                    provider.note_article_lookup(ArticleLookupOutcome::Missing);
                }
            }
            self.discard_article_error_reply(attempt, error_reply, request, state)
                .await?;
            return Ok(BackendAttemptResult::BackendUnavailable);
        }
        if fallback_lookup {
            provider.note_fallback_lookup(FallbackOutcome::Found);
        }
        if article_lookup {
            provider.note_article_lookup(ArticleLookupOutcome::Found);
        }

        let sent_at = attempt.sent_at;
        let delivers_article = matches!(request.kind(), RequestKind::Article | RequestKind::Body);
        let mut timed_writer = crate::io_util::StallTimer::new(&mut *client_writer);
        let response = match self
            .write_successful_retry_response(
                attempt,
                &mut timed_writer,
                ResponseWriteContext {
                    request_origin: state.request_origin,
                    request,
                    article_request: crate::command::CommandHandler::article_lookup_request(
                        request,
                    ),
                },
                state.backend_connection,
            )
            .await
        {
            Ok(response) => response,
            Err(e @ SessionError::ClientDisconnect(_)) => {
                // A client that stalls and then abandons the transfer held
                // the connection; count it even though nothing was delivered.
                provider
                    .delivery()
                    .record_client_stall(timed_writer.stalled());
                return Err(e);
            }
            Err(e) => {
                provider
                    .delivery()
                    .record_client_stall(timed_writer.stalled());
                state.unavailable_backends.suppress(backend_id);
                return Err(e);
            }
        };
        if delivers_article {
            provider.delivery().record_article(
                response.wire_len().get() as u64,
                sent_at.elapsed(),
                timed_writer.stalled(),
                std::time::Instant::now(),
            );
        }
        guard.complete();
        request.record_backend_response(backend_id, response);
        Ok(BackendAttemptResult::Success)
    }

    /// The backend to queue on when backlog skipping has left no other
    /// backend to try in the tier that should serve the request.
    ///
    /// That tier is the first one with a backend that has not answered `430`,
    /// the same tier the router is held to. If any of its backends can still
    /// be tried normally there is no last resort: skipping found an
    /// alternative, which is what it is for. Otherwise the skipped backend
    /// with the shortest estimated wait is picked; a backend that failed for
    /// this request is never one.
    ///
    /// Backends excluded from this request are invisible here, as they are to
    /// the router's tier escalation.
    ///
    /// Picking can count a rescue-floor pass-over, so it records the
    /// request's floor decisions; a backend already decided is not counted
    /// again.
    fn last_resort_backend(
        router: &BackendSelector,
        state: &mut ArticleAttemptState<'_>,
        excluded_backends: SuppressedBackends,
    ) -> Option<BackendId> {
        if state.budget.last_resort() != crate::pool::admission::BacklogLastResort::WaitShortest
            || !state.backlogged.has_candidates()
        {
            return None;
        }
        let admission = router.article_admission();
        // The rescue floor only applies to fallback lookups: before any
        // backend has answered 430 the article may well exist.
        let fallback_lookup = state.availability.missing_bits() != 0;
        let can_try = |backend_id: BackendId| {
            !excluded_backends.contains(backend_id)
                && router
                    .availability_slot(backend_id)
                    .is_some_and(|slot| state.availability.should_try_slot(slot))
        };
        let tiers: smallvec::SmallVec<[u8; 4]> = router.tiers().collect();
        for (index, &tier) in tiers.iter().enumerate() {
            let mut tier_can_serve = false;
            let mut shortest: Option<(std::time::Duration, BackendId)> = None;
            let mut shortest_unlikely: Option<(std::time::Duration, BackendId)> = None;
            for backend_id in router.backend_ids_in_tier(tier) {
                if !can_try(backend_id) {
                    continue;
                }
                tier_can_serve = true;
                if !state.unavailable_backends.contains(backend_id) {
                    return None;
                }
                if !state.backlogged.is_candidate(backend_id) {
                    continue;
                }
                let Some(provider) = router.backend_provider(backend_id) else {
                    continue;
                };
                let wait = provider.estimated_wait();
                let passed_over = fallback_lookup
                    && provider.is_unlikely_to_rescue(&admission)
                    && match state.backlogged.floor_decision(backend_id) {
                        FloorDecision::PassedOver => true,
                        FloorDecision::Used => false,
                        FloorDecision::Undecided => {
                            let decision = if provider.take_rescue_probe() {
                                FloorDecision::Used
                            } else {
                                FloorDecision::PassedOver
                            };
                            state.backlogged.decide_floor(backend_id, decision);
                            decision == FloorDecision::PassedOver
                        }
                    };
                let slot = if passed_over {
                    &mut shortest_unlikely
                } else {
                    &mut shortest
                };
                if slot.is_none_or(|(best, _)| wait < best) {
                    *slot = Some((wait, backend_id));
                }
            }
            if tier_can_serve {
                if shortest.is_some() {
                    return shortest.map(|(_, backend_id)| backend_id);
                }
                // Passing over an unlikely rescuer must not move the request
                // to a lower tier before this tier has answered 430; only
                // skip it when no lower tier could serve the request anyway.
                let lower_tier_can_serve = tiers[index + 1..].iter().any(|&lower| {
                    router.backend_ids_in_tier(lower).any(|backend_id| {
                        can_try(backend_id) && !state.unavailable_backends.contains(backend_id)
                    })
                });
                return if lower_tier_can_serve {
                    shortest_unlikely.map(|(_, backend_id)| backend_id)
                } else {
                    None
                };
            }
        }
        None
    }

    /// The checkout budget for `backend_id` once the rescue floor is applied.
    ///
    /// Once another backend has answered `430`, a backend under the floor is
    /// used only when a connection is free now; otherwise it is skipped like
    /// a backlogged backend, so dead articles do not queue on it. The
    /// checkout itself enforces this ([`BacklogPolicy::IdleOnly`]), so a
    /// burst of lookups cannot all claim one idle connection. Only the first
    /// checkout in a request decides; a backend the request already used, for
    /// example one the `STAT` sweep found the article on, is checked out
    /// normally.
    ///
    /// The floor follows backlog skipping: with skipping off, or for a last
    /// resort that already chose to wait, the budget is unchanged.
    ///
    /// [`BacklogPolicy::IdleOnly`]: crate::pool::admission::BacklogPolicy::IdleOnly
    fn rescue_floor_checkout(
        router: &BackendSelector,
        provider: &crate::pool::DeadpoolConnectionProvider,
        backend_id: BackendId,
        backlogged: &mut BackloggedBackends,
        fallback_lookup: bool,
        checkout: crate::pool::admission::CheckoutBudget,
    ) -> crate::pool::admission::CheckoutBudget {
        if !fallback_lookup
            || !matches!(
                checkout.backlog(),
                crate::pool::admission::BacklogPolicy::SkipAbove { .. }
            )
            || !provider.is_unlikely_to_rescue(&router.article_admission())
            || backlogged.floor_decision(backend_id) != FloorDecision::Undecided
        {
            return checkout;
        }
        // A skip below upgrades this to a pass-over.
        backlogged.decide_floor(backend_id, FloorDecision::Used);
        checkout.idle_only(crate::pool::admission::RescueProbes::Counted)
    }

    fn article_deadline_expired(
        &self,
        provider: &crate::pool::DeadpoolConnectionProvider,
        backend_id: BackendId,
        request: &RequestContext,
    ) -> BackendAttemptResult {
        provider.note_deadline_expiry();
        debug!(
            client = %self.client_addr,
            backend = backend_id.as_index(),
            command_verb = ?request.verb(),
            msg_id = ?request.message_id_value(),
            "Article request deadline expired before a backend answered"
        );
        BackendAttemptResult::DeadlineExpired
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
        if state.budget.is_expired() {
            return Ok(());
        }
        let probe_budget = state.budget.probe();

        let mut candidates = Vec::new();
        for tier in router.tiers() {
            for backend_id in router.backend_ids_in_tier(tier) {
                if state.unavailable_backends.contains(backend_id)
                    || !router
                        .availability_slot(backend_id)
                        .is_some_and(|slot| state.availability.should_try_slot(slot))
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

        // The sweep runs after a 430, which makes each probe a fallback lookup.
        let fallback_lookup = state.availability.missing_bits() != 0;
        let mut probes = FuturesUnordered::new();
        for (backend_id, provider) in candidates {
            let router = Arc::clone(router);
            let stat_request = stat_request.clone_for_background_probe();
            let budget = Self::rescue_floor_checkout(
                &router,
                &provider,
                backend_id,
                state.backlogged,
                fallback_lookup,
                probe_budget,
            );
            probes.push(async move {
                let _guard = BackendSelector::guard_for_manual_backend(router, backend_id);
                let conn = match provider.checkout_for_attempt(budget).await {
                    Ok(conn) => conn.activate(),
                    Err(crate::connection_error::ConnectionError::Backlogged { .. }) => {
                        return RetryStatProbeOutcome::Backlogged { backend_id, budget };
                    }
                    Err(_) => return RetryStatProbeOutcome::Unavailable(backend_id),
                };
                let buffer = self.buffer_pool.acquire();
                let exchange = match backend::execute_request_exchange(
                    conn,
                    &stat_request,
                    buffer,
                    &self.buffer_pool,
                    backend_id,
                )
                .await
                {
                    Ok(exchange) => exchange,
                    Err(_) => return RetryStatProbeOutcome::Unavailable(backend_id),
                };

                let status_code = exchange.status_code();
                let Ok(conn) = exchange.observe_and_reuse().await else {
                    return RetryStatProbeOutcome::Unavailable(backend_id);
                };
                let release = release_observed_probe_connection(conn, backend_id);

                // The STAT reply was fully framed before any stray bytes, so a
                // 430 stays authoritative even when the socket must be retired.
                // The sweep only runs after a 430, so every answer here is a
                // fallback lookup for the rescue rate.
                if status_code.is_some_and(|status| status.as_u16() == 430) {
                    provider.note_fallback_lookup(FallbackOutcome::NotFound);
                    provider.note_article_lookup(ArticleLookupOutcome::Missing);
                    RetryStatProbeOutcome::Missing(backend_id)
                } else if release.is_err() {
                    RetryStatProbeOutcome::Unavailable(backend_id)
                } else {
                    if status_code.is_some_and(|status| status.is_success()) {
                        provider.note_fallback_lookup(FallbackOutcome::Found);
                        provider.note_article_lookup(ArticleLookupOutcome::Found);
                    }
                    RetryStatProbeOutcome::Present
                }
            });
        }

        while let Some(outcome) = probes.next().await {
            match outcome {
                RetryStatProbeOutcome::Missing(backend_id) => {
                    let missing = AuthoritativeArticleMissing {
                        backend_id,
                        availability_slot: router
                            .availability_slot(backend_id)
                            .expect("probe backend is registered"),
                    };
                    self.record_authoritative_article_missing(&missing, state.availability);
                    if let Some(article_request) =
                        crate::command::CommandHandler::article_lookup_request(request)
                    {
                        self.cache
                            .record_availability_missing(
                                article_request.message_id(),
                                missing.availability_slot(),
                            )
                            .await;
                    }
                }
                RetryStatProbeOutcome::Present => {}
                RetryStatProbeOutcome::Unavailable(backend_id) => {
                    state.unavailable_backends.suppress(backend_id);
                }
                RetryStatProbeOutcome::Backlogged { backend_id, budget } => {
                    state.unavailable_backends.suppress(backend_id);
                    state.backlogged.note_backlog_skip(backend_id, budget);
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
        if !matches!(request.kind(), RequestKind::Article | RequestKind::Body)
            || request.is_stat()
            || !request.has_message_id()
        {
            return;
        }

        match router.lower_tier_stat_prefetch() {
            LowerTierStatPrefetch::FirstAttempt => {}
            LowerTierStatPrefetch::AfterPrimaryMiss
                if router.has_primary_tier_miss(availability) => {}
            LowerTierStatPrefetch::AfterPrimaryMiss | LowerTierStatPrefetch::Off => return,
        }

        let Some(stat_request) = Self::stat_probe_request(request) else {
            return;
        };
        let Some(msg_id_text) = request.message_id().map(str::to_owned) else {
            return;
        };

        let admission = router.article_admission();
        let probe_budget = admission.probe();
        // In after-primary-miss mode every probe follows a 430; in first-attempt
        // mode only those with a cached miss do.
        let fallback_lookup = availability.missing_bits() != 0;
        let mut candidates = Vec::new();
        for tier in router.tiers().filter(|tier| *tier > 0) {
            for backend_id in router.backend_ids_in_tier(tier) {
                if unavailable_backends.contains(backend_id)
                    || !router
                        .availability_slot(backend_id)
                        .is_some_and(|slot| availability.should_try_slot(slot))
                {
                    continue;
                }
                let Some(provider) = router.backend_provider(backend_id) else {
                    continue;
                };
                if provider.stat_missing_enabled() {
                    // After a 430, an under-floor backend takes only a free
                    // connection, as in the request's own attempts. A
                    // background probe is not the request's attempt, so its
                    // pass-overs do not count toward rescue probes.
                    let budget = if fallback_lookup && provider.is_unlikely_to_rescue(&admission) {
                        probe_budget.idle_only(crate::pool::admission::RescueProbes::Uncounted)
                    } else {
                        probe_budget
                    };
                    candidates.push((backend_id, provider.clone(), budget));
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
            for (backend_id, provider, budget) in candidates {
                let router = Arc::clone(&router);
                let cache = Arc::clone(&cache);
                let stat_request = stat_request.clone_for_background_probe();
                let msg_id_text = msg_id_text.clone();
                let buffer_pool = buffer_pool.clone();
                probes.push(async move {
                    let availability_slot = router
                        .availability_slot(backend_id)
                        .expect("probe backend is registered");
                    let _guard = BackendSelector::guard_for_manual_backend(router, backend_id);
                    let conn = match provider.checkout_for_attempt(budget).await {
                        Ok(conn) => conn.activate(),
                        Err(_) => return,
                    };
                    let buffer = buffer_pool.acquire();
                    let exchange = match backend::execute_request_exchange(
                        conn,
                        &stat_request,
                        buffer,
                        &buffer_pool,
                        backend_id,
                    )
                    .await
                    {
                        Ok(exchange) => exchange,
                        Err(_) => return,
                    };
                    let status_code = exchange.status_code();
                    let Ok(conn) = exchange.observe_and_reuse().await else {
                        return;
                    };
                    // Retirement does not invalidate the already-framed reply,
                    // so a 430 is still recorded below.
                    let _ = release_observed_probe_connection(conn, backend_id);

                    if let Some(status) = status_code {
                        let outcome = if status.is_article_missing() {
                            Some(ArticleLookupOutcome::Missing)
                        } else if status.is_success() {
                            Some(ArticleLookupOutcome::Found)
                        } else {
                            None
                        };
                        if let Some(outcome) = outcome {
                            provider.note_article_lookup(outcome);
                            if fallback_lookup {
                                provider.note_fallback_lookup(match outcome {
                                    ArticleLookupOutcome::Found => FallbackOutcome::Found,
                                    ArticleLookupOutcome::Missing => FallbackOutcome::NotFound,
                                });
                            }
                        }
                    }
                    if status_code.is_some_and(|status| status.as_u16() == 430)
                        && let Ok(msg_id) = crate::types::MessageId::new(msg_id_text)
                    {
                        cache
                            .record_availability_missing(msg_id, availability_slot)
                            .await;
                    }
                });
            }
            while probes.next().await.is_some() {}
        });
    }

    /// Consume a non-430 error reply without sending it to the client.
    ///
    /// The backend is suppressed before the response is drained so that a
    /// failed drain still keeps this request away from it.
    async fn discard_article_error_reply(
        &self,
        attempt: PreparedBackendAttempt<'_>,
        error_reply: ArticleErrorReply,
        request: &RequestContext,
        state: &mut ArticleAttemptState<'_>,
    ) -> Result<(), SessionError> {
        let backend_id = attempt.exchange.backend_id();
        let status_code = attempt.status_code();
        state.unavailable_backends.suppress(backend_id);
        self.record_article_error_reply_metrics(backend_id, request, status_code);

        let conn = attempt.observe().await?;
        match error_reply {
            ArticleErrorReply::SessionUsable => {
                debug!(
                    client = %self.client_addr,
                    backend = backend_id.as_index(),
                    command_verb = ?request.verb(),
                    msg_id = ?request.message_id_value(),
                    status_code = status_code.as_u16(),
                    unavailable_backends = format_args!("{:08b}", state.unavailable_backends.bits()),
                    "Backend refused article lookup with a non-430 error; trying another backend"
                );
                self.release_or_reuse_connection(
                    conn,
                    backend_id,
                    request,
                    Some(state.backend_connection),
                );
            }
            ArticleErrorReply::SessionUnusable => {
                warn!(
                    client = %self.client_addr,
                    backend = backend_id.as_index(),
                    command_verb = ?request.verb(),
                    msg_id = ?request.message_id_value(),
                    status_code = status_code.as_u16(),
                    "Backend reply says its session is unusable; retiring connection and trying another backend"
                );
                conn.fail_backend();
            }
        }
        Ok(())
    }

    /// Count a non-430 error reply that the proxy retried elsewhere instead of
    /// forwarding: in the error class it belongs to, and as a failover.
    pub(super) fn record_article_error_reply_metrics(
        &self,
        backend_id: BackendId,
        request: &RequestContext,
        status_code: StatusCode,
    ) {
        match determine_metrics_action_for_request(request, status_code) {
            MetricsAction::Error4xx => self.metrics.record_error_4xx(backend_id),
            MetricsAction::Error5xx => self.metrics.record_error_5xx(backend_id),
            MetricsAction::ArticleMissing => self.metrics.record_article_missing(backend_id),
            MetricsAction::Article | MetricsAction::None => {}
        }
        self.metrics.record_error_reply_failover(backend_id);
    }

    async fn write_successful_retry_response<W: AsyncWrite + Unpin>(
        &self,
        attempt: PreparedBackendAttempt<'_>,
        client_writer: &mut W,
        context: ResponseWriteContext<'_>,
        backend_connection: &mut Option<BackendLease>,
    ) -> Result<RequestResponseMetadata, SessionError> {
        self.write_successful_backend_response(
            attempt,
            client_writer,
            context,
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
        checkout: AttemptCheckout,
    ) -> Result<Option<PreparedBackendAttempt<'_>>, SessionError> {
        let AttemptCheckout {
            budget,
            is_retry_attempt,
        } = checkout;
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
        let attempt = match retry_once!(
            unless = same_backend_retry_cannot_succeed;
            self.execute_backend_attempt(
                provider,
                backend,
                request,
                BackendAttemptConnection {
                    lease: state.backend_connection,
                    budget,
                },
                is_retry_attempt,
            )
            .await,
            client = self.client_addr,
            backend = backend_id.as_index()
        ) {
            Ok(result) => result,
            Err(err) => {
                state.unavailable_backends.suppress(backend_id);
                state
                    .backlogged
                    .note_checkout_failure(backend_id, budget, &err);
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

        if let Some((ttfb, send, recv)) = attempt.timings {
            self.record_timing_metrics(backend_id, ttfb, send, recv);
            provider
                .delivery()
                .record_ttfb(std::time::Duration::from_micros(ttfb));
        }
        *state.client_to_backend_bytes = state.client_to_backend_bytes.add(request_wire_len);

        let status_code = match attempt.exchange.status_code() {
            Some(status_code) => status_code,
            None => {
                attempt.exchange.log_warnings(self.client_addr);
                trace!(
                    client = %self.client_addr,
                    backend = backend_id.as_index(),
                    command_verb = ?request.verb(),
                    msg_id = ?request.message_id_value(),
                    "Backend attempt read bytes but could not parse a status code"
                );
                let response = attempt.exchange.into_invalid_response();
                self.handle_invalid_backend_response(
                    backend_id,
                    InvalidBackendResponseContext {
                        provider,
                        request,
                        response,
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
            backend_read_bytes = attempt.exchange.received_len(),
            availability_missing_bits = format_args!("{:08b}", state.availability.missing_bits()),
            "Backend attempt received classifiable response bytes"
        );

        Ok(Some(attempt))
    }

    fn handle_invalid_backend_response(
        &self,
        backend_id: BackendId,
        ctx: InvalidBackendResponseContext<'_>,
    ) {
        let InvalidBackendResponseContext {
            provider,
            request,
            response,
        } = ctx;
        tracing::warn!(
            client = %self.client_addr,
            backend = ?backend_id,
            command_verb = ?request.verb(),
            first_bytes_hex = %crate::session::backend::format_hex_preview(
                response.bytes(), 256
            ),
            first_bytes_utf8 = %String::from_utf8_lossy(response.bytes()),
            "Backend returned invalid/unparseable response, attempting to salvage connection"
        );
        let request_kind = request.kind();
        let provider_for_salvage = provider.clone();
        let conn_for_salvage = response.into_connection_for_health_check();
        tokio::spawn(async move {
            tracing::debug!(
                backend = ?backend_id,
                request_kind = ?request_kind,
                "Attempting to salvage connection after Invalid response"
            );
            crate::pool::salvage_with_health_check(conn_for_salvage, provider_for_salvage).await;
        });
    }

    pub(super) async fn write_successful_backend_response<W>(
        &self,
        attempt: PreparedBackendAttempt<'_>,
        client_write: &mut W,
        context: ResponseWriteContext<'_>,
        backend_connection: Option<&mut Option<BackendLease>>,
    ) -> Result<RequestResponseMetadata, SessionError>
    where
        W: AsyncWrite + Unpin,
    {
        let PreparedBackendAttempt { mut exchange, .. } = attempt;
        let backend_id = exchange.backend_id();
        let backend_bytes = match exchange.receiving() {
            Ok(receiving) => receiving,
            Err(error) => {
                return Err(SessionError::from(error));
            }
        };
        let write_result = match self
            .write_response_to_client(client_write, backend_id, backend_bytes, context)
            .await
        {
            Ok(result) => result,
            Err(e) => {
                return Err(self.handle_response_transfer_error(
                    exchange.into_connection_after_failure(),
                    backend_id,
                    context.request,
                    e,
                ));
            }
        };
        let ResponseWriteResult {
            status_code,
            bytes_written,
        } = write_result;
        debug!(
            client = %self.client_addr,
            backend = backend_id.as_index(),
            command_verb = ?context.request.verb(),
            status_code = status_code.as_u16(),
            has_response_body = context.request.has_response_body(status_code),
            "Writing backend response to client"
        );
        if let Err(e) = client_write.flush().await {
            return Err(self.handle_response_transfer_error(
                exchange.into_connection(),
                backend_id,
                context.request,
                classify_response_write_err(e),
            ));
        }

        debug!(
            client = %self.client_addr,
            backend = backend_id.as_index(),
            msg_id = ?context.article_request,
            bytes_written,
            "Backend response write complete"
        );
        self.record_response_metrics(backend_id, context, status_code, bytes_written);
        self.release_or_reuse_connection(
            exchange.into_connection(),
            backend_id,
            context.request,
            backend_connection,
        );

        Ok(RequestResponseMetadata::new(
            status_code,
            usize::try_from(bytes_written).unwrap_or(usize::MAX).into(),
        ))
    }

    pub(super) fn handle_response_transfer_error(
        &self,
        mut conn: crate::pool::ConnectionGuard,
        backend_id: BackendId,
        request: &RequestContext,
        error: ResponseTransferError,
    ) -> SessionError {
        let reuse = crate::session::response_transfer::connection_reuse_after_response(&conn);
        match error.pool_fate(reuse) {
            BackendConnectionOutcome::BackendFailed => {
                warn!(
                    client = %self.client_addr,
                    backend = backend_id.as_index(),
                    command_verb = ?request.verb(),
                    error = %error,
                    "Response write error, removing connection from pool"
                );
                self.metrics.record_error(backend_id);
                self.metrics.user_error(self.username());
                conn.fail_backend();
            }
            BackendConnectionOutcome::BackendDirty => {
                let ResponseConnectionReuse::QueuedBytes { len } = reuse else {
                    unreachable!("dirty backend fate requires queued bytes");
                };
                warn!(
                    client = %self.client_addr,
                    backend = backend_id.as_index(),
                    command_verb = ?request.verb(),
                    queued_bytes = len,
                    "Response write left queued backend bytes; retiring connection"
                );
                conn.fail_client();
            }
            BackendConnectionOutcome::BackendHealthy => {
                debug!(
                    client = %self.client_addr,
                    backend = backend_id.as_index(),
                    command_verb = ?request.verb(),
                    "Response write error left backend connection reusable; releasing it back to pool"
                );
                conn.mark_response_complete();
                let _ = conn.complete_success();
            }
        }

        SessionError::from(error)
    }

    pub(super) fn release_or_reuse_connection(
        &self,
        conn: crate::pool::ConnectionGuard,
        backend_id: BackendId,
        request: &RequestContext,
        backend_connection: Option<&mut Option<BackendLease>>,
    ) {
        let reuse = crate::session::response_transfer::connection_reuse_after_response(&conn);
        match reuse.pool_fate() {
            BackendConnectionOutcome::BackendDirty => {
                let ResponseConnectionReuse::QueuedBytes { len } =
                    crate::session::response_transfer::connection_reuse_after_response(&conn)
                else {
                    unreachable!("dirty backend fate requires queued bytes");
                };
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
                conn.fail_client();
            }
            BackendConnectionOutcome::BackendHealthy => {
                if let Some(slot) = backend_connection
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
                    *slot = Some(BackendLease::new(backend_id, conn));
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
                    let _ = conn.complete_success();
                }
            }
            BackendConnectionOutcome::BackendFailed => {
                warn!(
                    client = %self.client_addr,
                    backend = backend_id.as_index(),
                    command_verb = ?request.verb(),
                    msg_id = ?request.message_id_value(),
                    "Refusing to reuse an incomplete backend response"
                );
                conn.fail_backend();
            }
        }
    }

    pub(super) async fn checkout_direct_backend_connection(
        &self,
        provider: &crate::pool::DeadpoolConnectionProvider,
        backend_id: crate::types::BackendId,
        request: &RequestContext,
        connection: BackendAttemptConnection<'_>,
    ) -> Result<crate::pool::ConnectionGuard> {
        let BackendAttemptConnection {
            lease: backend_connection,
            budget,
        } = connection;
        let checkout_status = provider.status_counts();
        let can_expand_or_use_idle =
            checkout_status.available > 0 || checkout_status.size < checkout_status.max_size;
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
            Some(BackendLease {
                backend_id: cached_backend_id,
                connection: mut guard,
                _idle: idle,
            }) if cached_backend_id == backend_id => {
                // The connection is in use from here on. An unbound field
                // would live to the end of the match, double-counting the
                // connection while the checkouts below await.
                drop(idle);
                if !self.cached_batch_connection_is_healthy(&mut guard, backend_id, request) {
                    guard.fail_backend();
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
                        "Requesting pooled connection after retiring unhealthy cached batch connection"
                    );
                    provider.checkout_for_attempt(budget).await?.activate()
                } else if can_expand_or_use_idle {
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
                    *backend_connection = Some(BackendLease::new(cached_backend_id, guard));
                    match provider.checkout_for_attempt(budget).await {
                        Ok(conn) => conn.activate(),
                        Err(err) => {
                            debug!(
                                client = %self.client_addr,
                                backend = backend_id.as_index(),
                                command_verb = ?request.verb(),
                                msg_id = ?request.message_id_value(),
                                error = %err,
                                "Idle pool checkout failed; falling back to cached batch connection"
                            );
                            backend_connection
                                .take()
                                .expect("cached backend connection should be present for fallback reuse")
                                .connection
                        }
                    }
                } else {
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
                }
            }
            Some(cached) => {
                let BackendLease {
                    backend_id: cached_backend_id,
                    connection: cached_conn,
                    _idle: idle,
                } = cached;
                drop(idle);
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
                let _ = cached_conn.complete_success();
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
                let conn = provider.checkout_for_attempt(budget).await?.activate();
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
                conn
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
                let conn = provider.checkout_for_attempt(budget).await?.activate();
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
                conn
            }
        };
        Ok(guard)
    }

    /// Execute a single backend attempt and retain its response with its guard.
    ///
    /// The returned owner is the only value that can continue, observe, or
    /// forward this response; callers cannot bind a response read to another
    /// connection after the fact. On error, the connection is removed from the
    /// pool before returning.
    async fn execute_backend_attempt(
        &self,
        provider: &crate::pool::DeadpoolConnectionProvider,
        backend: &ArticleBackend,
        request: &RequestContext,
        connection: BackendAttemptConnection<'_>,
        stat_probe_retry_only: bool,
    ) -> Result<PreparedBackendAttempt<'_>> {
        let backend_id = backend.backend_id();
        let guard = self
            .checkout_direct_backend_connection(provider, backend_id, request, connection)
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
        if Self::should_use_stat_missing_probe(provider, request, stat_probe_retry_only)
            && let Some(stat_request) = Self::stat_probe_request(request)
        {
            debug!(
                client = %self.client_addr,
                backend = backend.backend_id().as_index(),
                command_verb = ?request.verb(),
                msg_id = ?request.message_id_value(),
                "Running STAT miss probe before backend article fetch"
            );

            let probe = self
                .execute_and_read_response(guard, backend, &stat_request)
                .await
                .map_err(|BackendReadAttemptError::Backend(e)| e)?;
            if probe
                .exchange
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
                return Ok(probe);
            }

            let guard = probe.observe().await?;
            let attempt = self
                .execute_and_read_response(guard, backend, request)
                .await
                .map_err(|BackendReadAttemptError::Backend(e)| e)?;
            return Ok(attempt);
        }

        let attempt = self
            .execute_and_read_response(guard, backend, request)
            .await
            .map_err(|BackendReadAttemptError::Backend(e)| e)?;
        Ok(attempt)
    }

    /// Execute a command and classify its response while retaining the guard
    /// that supplied the bytes in the same owner.
    async fn execute_and_read_response(
        &self,
        conn: crate::pool::ConnectionGuard,
        backend: &ArticleBackend,
        request: &RequestContext,
    ) -> Result<PreparedBackendAttempt<'_>, BackendReadAttemptError> {
        let backend_id = backend.backend_id();
        self.metrics.record_command(backend_id);
        self.metrics.user_command(self.username());

        let buffer = self.buffer_pool.acquire();
        // Every direct attempt is timed (three clock reads): routing and the
        // delivery estimate need first-byte time and busy time per request,
        // not a 1-in-16 sample.
        let sent_at = std::time::Instant::now();
        let (exchange, ttfb, send, recv) = backend::execute_request_exchange_timed(
            conn,
            request,
            buffer,
            &self.buffer_pool,
            backend_id,
        )
        .await
        .map_err(BackendReadAttemptError::Backend)?;
        let result = (exchange, Some((ttfb, send, recv)));
        debug!(
            client = %self.client_addr,
            backend = backend_id.as_index(),
            command_verb = ?request.verb(),
            msg_id = ?request.message_id_value(),
            "Backend response classification completed for direct attempt"
        );

        let (exchange, timings) = result;
        Ok(PreparedBackendAttempt {
            exchange,
            timings,
            sent_at,
        })
    }

    pub(super) async fn forward_response_for_already_sent_request<W>(
        &self,
        conn: crate::pool::ConnectionGuard,
        client_write: &mut W,
        backend: &ArticleBackend,
        request: &mut RequestContext,
        availability: &mut crate::cache::ArticleAvailability,
        backend_to_client_bytes: &mut BackendToClientBytes,
    ) -> Result<crate::pool::ConnectionGuard, AlreadySentResponseError>
    where
        W: AsyncWrite + Unpin,
    {
        let backend_id = backend.backend_id();
        let mut exchange = backend::read_exchange_for_already_sent_request(
            conn,
            request,
            &self.buffer_pool,
            backend_id,
        )
        .await
        .map_err(AlreadySentResponseError::Read)?;
        let Some(status_code) = exchange.status_code() else {
            exchange.log_warnings(self.client_addr);
            exchange.fail_backend();
            return Err(AlreadySentResponseError::Read(anyhow::anyhow!(
                "backend returned an invalid response to an already-sent request"
            )));
        };

        if let Ok(missing) = AuthoritativeArticleMissing::from_status_code(*backend, status_code) {
            self.record_authoritative_article_missing(&missing, availability);
            if let Some(article_request) =
                crate::command::CommandHandler::article_lookup_request(request)
            {
                self.cache
                    .record_availability_missing(
                        article_request.message_id(),
                        missing.availability_slot(),
                    )
                    .await;
            }
            let response = exchange
                .receiving()
                .map_err(AlreadySentResponseError::Read)?;
            if let Err(error) = response.observe().await {
                let conn = exchange.into_connection_after_failure();
                return Err(AlreadySentResponseError::Transfer(
                    self.handle_response_transfer_error(conn, backend_id, request, error),
                ));
            }
            let conn = exchange.into_connection();
            if let Err(error) = self
                .send_430_to_client(client_write, backend_to_client_bytes)
                .await
            {
                return Err(AlreadySentResponseError::Transfer(
                    self.handle_response_transfer_error(
                        conn,
                        backend_id,
                        request,
                        classify_response_write_err(error),
                    ),
                ));
            }
            if let Err(error) = client_write.flush().await {
                return Err(AlreadySentResponseError::Transfer(
                    self.handle_response_transfer_error(
                        conn,
                        backend_id,
                        request,
                        classify_response_write_err(error),
                    ),
                ));
            }
            return Ok(conn);
        }

        let context = ResponseWriteContext {
            request_origin: ArticleRequestOrigin::Frontend,
            request,
            article_request: crate::command::CommandHandler::article_lookup_request(request),
        };
        let response = exchange
            .receiving()
            .map_err(AlreadySentResponseError::Read)?;
        let write_result = match self
            .write_response_to_client(client_write, backend_id, response, context)
            .await
        {
            Ok(result) => result,
            Err(error) => {
                let conn = exchange.into_connection_after_failure();
                return Err(AlreadySentResponseError::Transfer(
                    self.handle_response_transfer_error(conn, backend_id, request, error),
                ));
            }
        };
        let ResponseWriteResult {
            status_code: written_status_code,
            bytes_written,
        } = write_result;
        let conn = exchange.into_connection();
        if let Err(error) = client_write.flush().await {
            return Err(AlreadySentResponseError::Transfer(
                self.handle_response_transfer_error(
                    conn,
                    backend_id,
                    request,
                    classify_response_write_err(error),
                ),
            ));
        }
        self.record_response_metrics(backend_id, context, written_status_code, bytes_written);
        let response = RequestResponseMetadata::new(
            written_status_code,
            usize::try_from(bytes_written).unwrap_or(usize::MAX).into(),
        );
        request.record_backend_response(backend_id, response);
        *backend_to_client_bytes = backend_to_client_bytes.add(response.wire_len().get());
        Ok(conn)
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

    /// Write response from backend to client and handle caching.
    ///
    /// Returns `ResponseTransferError` so callers can decide the connection's pool fate
    /// without string/downcast inspection.
    async fn write_response_to_client<W>(
        &self,
        client_write: &mut W,
        backend_id: BackendId,
        backend_bytes: crate::session::backend::Receiving<'_>,
        context: ResponseWriteContext<'_>,
    ) -> Result<ResponseWriteResult, ResponseTransferError>
    where
        W: AsyncWrite + Unpin,
    {
        let status_code = backend_bytes.status_code().ok_or_else(|| {
            ResponseTransferError::Io(anyhow::anyhow!("cannot write an invalid backend response"))
        })?;
        let params = ResponseWriteParams {
            request: context.request,
            article_request: context.article_request,
            status_code,
        };
        let cache_action = determine_cache_action_for_request(
            params.article_request,
            params.status_code,
            self.cache.payload_policy(),
        );

        debug!(
            "write_response_to_client: code={}, payload_policy={:?}, has_msg_id={}, action={:?}",
            params.status_code.as_u16(),
            self.cache.payload_policy(),
            params.article_request.is_some(),
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
                        .write_response_without_retention(client_write, backend_bytes)
                        .await?;
                    (bytes, None)
                }
                ResponseRetention::RetainAfterWrite => {
                    let (bytes, response) = self
                        .write_response_with_retention(
                            client_write,
                            backend_bytes,
                            params,
                            backend_id,
                        )
                        .await?;
                    (bytes, response)
                }
            };

        if let Some(backend) = backend_after_write {
            self.apply_cache_action(cache_action, params, backend, captured);
        }
        Ok(ResponseWriteResult {
            status_code: params.status_code,
            bytes_written,
        })
    }

    fn apply_cache_action(
        &self,
        cache_action: CacheAction,
        params: ResponseWriteParams<'_>,
        backend: BackendId,
        captured: Option<crate::session::multiline_framing::CapturedChunkedResponse>,
    ) {
        let backend_id = backend;
        let msg_id = params
            .article_request
            .map(|article_request| article_request.message_id());
        match (cache_action, msg_id.as_ref(), captured) {
            (CacheAction::CaptureArticle, msg_id, Some(response)) => {
                self.maybe_cache_upsert_framed(msg_id, response.into_cache_ingest(), backend);
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
            (CacheAction::TrackStat, msg_id, _) if self.cache.stores_payload_responses() => {
                if let Some(msg_id_ref) = msg_id {
                    self.spawn_cache_record_stat(
                        msg_id_ref,
                        backend,
                        self.tier_for_backend(backend),
                    );
                }
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
        client_write: &mut W,
        backend_bytes: crate::session::backend::Receiving<'_>,
    ) -> Result<u64, ResponseTransferError>
    where
        W: AsyncWrite + Unpin,
    {
        backend_bytes.write(client_write).await
    }

    async fn write_response_with_retention<W>(
        &self,
        client_write: &mut W,
        backend_bytes: crate::session::backend::Receiving<'_>,
        params: ResponseWriteParams<'_>,
        backend_id: BackendId,
    ) -> Result<
        (
            u64,
            Option<crate::session::multiline_framing::CapturedChunkedResponse>,
        ),
        ResponseTransferError,
    >
    where
        W: AsyncWrite + Unpin,
    {
        let mut captured = crate::pool::ChunkedResponse::default();
        // Payload caching owns responses only while they stay within the
        // framer-owned retention limit. Larger responses are streamed through
        // without cache insertion so normal delivery and backend reuse can
        // continue.
        let (bytes_written, captured) = backend_bytes
            .capture_and_write(client_write, &mut captured)
            .await?;
        self.log_body_response_written(backend_id, params, bytes_written as usize);
        if captured.is_some()
            && let Some(article_request) = params.article_request
        {
            debug!(
                "Client {} caching full article for {} ({} bytes captured)",
                self.client_addr,
                article_request.message_id(),
                captured.as_ref().map_or(0, |response| response.len())
            );
        }
        if captured.is_some() {
            Ok((bytes_written, captured))
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
    ) -> std::io::Result<()>
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
    fn maybe_cache_upsert_framed(
        &self,
        msg_id: Option<&crate::types::MessageId<'_>>,
        data: crate::cache::FramedChunkedResponse,
        backend: BackendId,
    ) {
        if let Some(msg_id_ref) = msg_id {
            let backend_id = backend;
            let tier = self.tier_for_backend(backend_id);
            self.spawn_cache_upsert_framed(msg_id_ref, data, backend, tier);
        }
    }

    /// Record response metrics (errors, article sizes, command execution)
    fn record_response_metrics(
        &self,
        backend_id: crate::types::BackendId,
        context: ResponseWriteContext<'_>,
        status_code: StatusCode,
        resp_bytes: u64,
    ) {
        use crate::types::MetricsBytes;

        let request = context.request;
        let cmd_bytes = request.request_wire_len().as_u64();

        match determine_metrics_action_for_request(request, status_code) {
            MetricsAction::Error4xx => self.metrics.record_error_4xx(backend_id),
            MetricsAction::Error5xx => self.metrics.record_error_5xx(backend_id),
            MetricsAction::Article => self.metrics.record_article(backend_id, resp_bytes),
            MetricsAction::ArticleMissing => self.metrics.record_article_missing(backend_id),
            MetricsAction::None => {}
        }

        let cmd_bytes_metric = MetricsBytes::new(cmd_bytes);
        let resp_bytes_metric = MetricsBytes::new(resp_bytes);
        let _ = self
            .metrics
            .record_exchange_bytes(backend_id, cmd_bytes_metric, resp_bytes_metric);
        if context.request_origin == ArticleRequestOrigin::Frontend {
            self.metrics.user_bytes_sent(self.username(), cmd_bytes);
        }
        self.metrics
            .user_bytes_received(self.username(), resp_bytes);
    }
}

/// Failures the same backend will repeat a few milliseconds later, so a retry
/// there only adds a wait and two warnings; the router should move on to
/// another backend instead.
///
/// - A full or backlogged pool is still busy.
/// - A refused login puts the pool in login backoff for seconds, and a pool
///   already in backoff refuses new connections until it ends, so the retry
///   could only get [`ConnectionError::LoginBackoff`].
///
/// [`ConnectionError::LoginBackoff`]: crate::connection_error::ConnectionError::LoginBackoff
fn same_backend_retry_cannot_succeed(error: &anyhow::Error) -> bool {
    use crate::connection_error::ConnectionError;
    error
        .downcast_ref::<ConnectionError>()
        .is_some_and(|error| {
            matches!(
                error,
                ConnectionError::PoolExhausted { .. }
                    | ConnectionError::Backlogged { .. }
                    | ConnectionError::LoginBackoff { .. }
            ) || error.is_login_refusal()
        })
}

/// Hand a background probe's connection back to the pool once its `STAT`
/// reply has been fully observed.
///
/// `observe_and_reuse` leaves pool ownership with the caller. Dropping the
/// guard instead closes a healthy socket, counts it as a backend failure, and
/// shrinks the pool for the replacement cooldown, so every successful probe
/// used to cost a connection and a fresh TLS login.
///
/// Queued bytes after the reply mean the backend sent something unrequested,
/// so the socket cannot be trusted for another command and is retired.
fn release_observed_probe_connection(
    conn: crate::pool::ConnectionGuard,
    backend_id: BackendId,
) -> Result<(), SessionError> {
    let pending_bytes = conn.pending_bytes_len();
    if pending_bytes == 0 {
        drop(conn.complete_success());
        return Ok(());
    }
    warn!(
        backend = backend_id.as_index(),
        connection_type = conn.connection_type(),
        pending_bytes,
        "Background STAT probe left queued backend bytes; retiring connection"
    );
    conn.fail_backend();
    Err(SessionError::Backend(anyhow::anyhow!(
        "backend {} left {pending_bytes} queued bytes after a STAT probe",
        backend_id.as_index()
    )))
}

#[cfg(test)]
mod tests {
    use super::ArticleAttemptState;
    use super::ArticleErrorReply;
    use super::ArticleRequestOrigin;
    use super::AuthoritativeArticleMissing;
    use super::BackendAttemptResult;
    use super::BackendLease;
    use super::BackloggedBackends;
    use super::PreparedBackendAttempt;
    use super::ResponseWriteContext;
    use super::ResponseWriteParams;
    use super::classify_response_write_err;
    use crate::auth::AuthHandler;
    use crate::cache::{ArticleAvailability, UnifiedCache};
    use crate::config::LowerTierStatPrefetch;
    use crate::metrics::MetricsCollector;
    use crate::pool::admission::FallbackOutcome;
    use crate::pool::{BufferPool, DeadpoolConnectionProvider};
    use crate::protocol::{RequestContext, RequestKind, StatusCode};
    use crate::router::{ArticleBackend, BackendSelector, SuppressedBackends};
    use crate::session::SessionError;
    use crate::session::routing::CacheAction;
    use crate::types::{
        BackendId, BackendToClientBytes, BufferSize, ClientAddress, ClientToBackendBytes,
        MessageId, ServerName,
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

    fn finalize_backend_connection(backend_connection: &mut Option<BackendLease>) {
        if let Some(lease) = backend_connection.take() {
            lease.complete_success();
        }
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

    fn test_session_with_backend_metrics(backend_count: usize) -> crate::session::ClientSession {
        let addr: std::net::SocketAddr = "127.0.0.1:9999".parse().unwrap();
        let buffer_pool = BufferPool::new(BufferSize::try_new(8192).unwrap(), 4);
        let auth_handler = Arc::new(AuthHandler::new(None, None).unwrap());
        let metrics = MetricsCollector::new(backend_count);
        crate::session::ClientSession::builder(
            ClientAddress::from(addr),
            buffer_pool,
            auth_handler,
            metrics,
        )
        .build()
    }

    fn test_session_with_cache(cache: Arc<UnifiedCache>) -> crate::session::ClientSession {
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

    /// Backend that completes connection setup but never answers an article
    /// command. Returns the port, accepted connections, and article commands.
    async fn spawn_silent_article_server() -> (u16, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepts = Arc::new(AtomicUsize::new(0));
        let article_commands = Arc::new(AtomicUsize::new(0));
        let accept_count = Arc::clone(&accepts);
        let count = Arc::clone(&article_commands);

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                accept_count.fetch_add(1, Ordering::SeqCst);
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
                                let kind = RequestContext::parse(line.as_bytes())
                                    .map(|request| request.kind());
                                if matches!(kind, Some(RequestKind::Article | RequestKind::Body)) {
                                    count.fetch_add(1, Ordering::SeqCst);
                                } else {
                                    let _ = write_half.write_all(b"200 OK\r\n").await;
                                }
                            }
                        }
                    }
                });
            }
        });

        (port, accepts, article_commands)
    }

    async fn spawn_stat_missing_probe_server() -> (u16, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        spawn_stat_probe_server(b"430 Missing\r\n").await
    }

    /// The whole STAT reply goes out in one write so any unrequested bytes
    /// after the status line land in the same read and stay queued.
    async fn spawn_stat_probe_server(
        stat_reply: &'static [u8],
    ) -> (u16, Arc<AtomicUsize>, Arc<AtomicUsize>) {
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
                                    let _ = write_half.write_all(stat_reply).await;
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

    async fn spawn_delayed_stat_missing_probe_server(
        delay: Duration,
    ) -> (u16, Arc<AtomicUsize>, Arc<AtomicUsize>) {
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
                                    tokio::time::sleep(delay).await;
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

    async fn client_writer_pair() -> (
        crate::session::ClientWriter,
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
            crate::session::ClientWriter::new(proxy_write),
            client_read,
            client_write,
        )
    }

    async fn spawn_connection_failure_server() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        port
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
        router_with_tiered_backends_configured(
            backends,
            crate::pool::admission::ArticleAdmission::default(),
            LowerTierStatPrefetch::default(),
        )
    }

    fn router_with_tiered_backends_and_admission(
        backends: impl IntoIterator<Item = (DeadpoolConnectionProvider, u8)>,
        admission: crate::pool::admission::ArticleAdmission,
    ) -> Arc<BackendSelector> {
        router_with_tiered_backends_configured(
            backends,
            admission,
            LowerTierStatPrefetch::default(),
        )
    }

    fn router_with_tiered_backends_and_prefetch(
        prefetch: LowerTierStatPrefetch,
        backends: impl IntoIterator<Item = (DeadpoolConnectionProvider, u8)>,
    ) -> Arc<BackendSelector> {
        router_with_tiered_backends_configured(
            backends,
            crate::pool::admission::ArticleAdmission::default(),
            prefetch,
        )
    }

    fn router_with_tiered_backends_configured(
        backends: impl IntoIterator<Item = (DeadpoolConnectionProvider, u8)>,
        admission: crate::pool::admission::ArticleAdmission,
        prefetch: LowerTierStatPrefetch,
    ) -> Arc<BackendSelector> {
        let mut router = BackendSelector::new()
            .with_article_admission(admission)
            .with_lower_tier_stat_prefetch(prefetch);
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
            crate::session::response_transfer::ResponseTransferError::ClientWrite(_)
        ));
    }

    #[test]
    fn owned_response_abort_is_terminal_client_disconnect() {
        let err = std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "aborted");
        let classified = classify_response_write_err(err);
        assert!(matches!(
            classified,
            crate::session::response_transfer::ResponseTransferError::ClientWrite(_)
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
        let (mut client_writer, _client_read, _client_write) = client_writer_pair().await;

        let mut request = request_context(b"BODY <missing@example.com>\r\n");
        let mut availability = ArticleAvailability::new();
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_connection = None;
        let mut unavailable_backends = SuppressedBackends::empty();
        let mut state = ArticleAttemptState {
            request_origin: ArticleRequestOrigin::Frontend,
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
            backlogged: &mut BackloggedBackends::default(),
            budget: crate::pool::admission::ArticleAdmission::default().start_request(),
        };

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            session.try_backend_for_article(
                &router,
                &mut request,
                &mut client_writer,
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
        assert!(availability.is_missing_slot(crate::cache::AvailabilitySlot::new(0).unwrap()));
        assert_eq!(article_commands.load(Ordering::SeqCst), 1);
        finalize_backend_connection(&mut backend_connection);
    }

    /// Answers LIST and BODY, counting each, so tests can see which backend a
    /// request reached.
    async fn spawn_list_and_body_server() -> (u16, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let list_commands = Arc::new(AtomicUsize::new(0));
        let body_commands = Arc::new(AtomicUsize::new(0));
        let lists = Arc::clone(&list_commands);
        let bodies = Arc::clone(&body_commands);

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let lists = Arc::clone(&lists);
                let bodies = Arc::clone(&bodies);
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
                                let reply: &[u8] = match cmd.split(' ').next() {
                                    Some("LIST") => {
                                        lists.fetch_add(1, Ordering::SeqCst);
                                        b"215 list follows\r\nalt.test 10 1 y\r\n.\r\n"
                                    }
                                    Some("BODY") => {
                                        bodies.fetch_add(1, Ordering::SeqCst);
                                        b"222 0 <id@example.com>\r\nbody\r\n.\r\n"
                                    }
                                    Some("QUIT") => b"205 Goodbye\r\n",
                                    Some("COMPRESS") => b"500 Not supported\r\n",
                                    _ => b"200 OK\r\n",
                                };
                                if write_half.write_all(reply).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        });

        (port, list_commands, body_commands)
    }

    async fn attempt_once(
        session: &crate::session::ClientSession,
        router: &Arc<BackendSelector>,
        line: &[u8],
    ) -> BackendAttemptResult {
        let mut request = request_context(line);
        let (mut client_writer, _client_read, _client_write) = client_writer_pair().await;
        let mut availability = ArticleAvailability::new();
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_connection = None;
        let mut unavailable_backends = SuppressedBackends::empty();
        let mut state = ArticleAttemptState {
            request_origin: ArticleRequestOrigin::Frontend,
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
            backlogged: &mut BackloggedBackends::default(),
            budget: crate::pool::admission::ArticleAdmission::default().start_request(),
        };
        let result = session
            .try_backend_for_article(router, &mut request, &mut client_writer, &mut state, false)
            .await
            .expect("attempt should not hit a transport error");
        finalize_backend_connection(&mut backend_connection);
        result
    }

    #[tokio::test]
    async fn numbered_replies_avoid_stateful_excluded_backends_but_message_ids_do_not() {
        let session = test_session();
        let (allowed_port, allowed_lists, allowed_bodies) = spawn_list_and_body_server().await;
        let (excluded_port, excluded_lists, excluded_bodies) = spawn_list_and_body_server().await;
        let mut router =
            BackendSelector::with_strategy(crate::config::BackendSelectionStrategy::LeastLoaded);
        for (index, port) in [allowed_port, excluded_port].into_iter().enumerate() {
            router.add_backend(
                ServerName::try_new(format!("backend-{index}")).unwrap(),
                DeadpoolConnectionProvider::builder("127.0.0.1", port)
                    .max_connections(2)
                    .build()
                    .unwrap(),
                0,
            );
        }
        router.exclude_from_stateful_sessions(BackendId::from_index(1));
        // Load the allowed backend so least-loaded would otherwise pick the excluded one.
        for _ in 0..8 {
            router.mark_backend_pending(BackendId::from_index(0));
        }
        let router = Arc::new(router);

        for line in [
            b"LIST ACTIVE alt.test\r\n".as_slice(),
            b"LIST COUNTS alt.test\r\n".as_slice(),
            b"LIST\r\n".as_slice(),
        ] {
            let result = attempt_once(&session, &router, line).await;
            assert!(matches!(result, BackendAttemptResult::Success));
        }
        assert_eq!(allowed_lists.load(Ordering::SeqCst), 3);
        assert_eq!(excluded_lists.load(Ordering::SeqCst), 0);

        let result = attempt_once(&session, &router, b"BODY <id@example.com>\r\n").await;
        assert!(matches!(result, BackendAttemptResult::Success));
        assert_eq!(
            excluded_bodies.load(Ordering::SeqCst),
            1,
            "message-ID requests may still use the excluded, less-loaded backend"
        );
        assert_eq!(allowed_bodies.load(Ordering::SeqCst), 0);
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
        let (mut client_writer, _client_read, _client_write) = client_writer_pair().await;

        let mut request = request_context(b"BODY <missing@example.com>\r\n");
        let mut availability = ArticleAvailability::new();
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_connection = None;
        let mut unavailable_backends = SuppressedBackends::empty();
        let mut state = ArticleAttemptState {
            request_origin: ArticleRequestOrigin::Frontend,
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
            backlogged: &mut BackloggedBackends::default(),
            budget: crate::pool::admission::ArticleAdmission::default().start_request(),
        };

        let result = session
            .try_backend_for_article(&router, &mut request, &mut client_writer, &mut state, false)
            .await
            .expect("first attempt should be handled without transport error");

        assert!(matches!(result, BackendAttemptResult::Success));
        assert_eq!(stat_commands.load(Ordering::SeqCst), 0);
        assert_eq!(
            body_commands.load(Ordering::SeqCst),
            1,
            "first attempt should go directly to BODY/ARTICLE/HEAD without STAT probe"
        );
        finalize_backend_connection(&mut backend_connection);
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
            request_origin: ArticleRequestOrigin::Frontend,
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
            backlogged: &mut BackloggedBackends::default(),
            budget: crate::pool::admission::ArticleAdmission::default().start_request(),
        };

        session
            .parallel_retry_stat_sweep(&router, &request, &mut state)
            .await
            .expect("parallel retry STAT sweep should succeed");

        assert!(availability.is_missing_slot(crate::cache::AvailabilitySlot::new(0).unwrap()));
        assert!(availability.is_missing_slot(crate::cache::AvailabilitySlot::new(1).unwrap()));
        assert_eq!(stat0.load(Ordering::SeqCst), 1);
        assert_eq!(stat1.load(Ordering::SeqCst), 1);
        assert_eq!(body0.load(Ordering::SeqCst), 0);
        assert_eq!(body1.load(Ordering::SeqCst), 0);
    }

    fn assert_connection_returned_idle(router: &BackendSelector, backend: usize) {
        let status = router
            .backend_provider(BackendId::from_index(backend))
            .expect("backend is registered")
            .status_counts();
        assert_eq!(
            (status.size, status.available, status.max_size),
            (1, 1, 1),
            "backend {backend}: the probe connection must go back to the pool idle, not be closed"
        );
    }

    #[tokio::test]
    async fn retry_stat_sweep_returns_probe_connections_to_the_pool() {
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

        let request = request_context(b"BODY <missing@example.com>\r\n");
        let mut availability = ArticleAvailability::new();
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_connection = None;
        let mut unavailable_backends = SuppressedBackends::empty();
        let mut state = ArticleAttemptState {
            request_origin: ArticleRequestOrigin::Frontend,
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
            backlogged: &mut BackloggedBackends::default(),
            budget: crate::pool::admission::ArticleAdmission::default().start_request(),
        };
        session
            .parallel_retry_stat_sweep(&router, &request, &mut state)
            .await
            .expect("parallel retry STAT sweep should succeed");

        assert_eq!(stat0.load(Ordering::SeqCst), 1);
        assert_eq!(stat1.load(Ordering::SeqCst), 1);
        assert_connection_returned_idle(&router, 0);
        assert_connection_returned_idle(&router, 1);
    }

    #[tokio::test]
    async fn non_primary_stat_prefetch_returns_probe_connections_to_the_pool() {
        let session = test_session();
        let (port0, _stat0, _body0) = spawn_stat_missing_probe_server().await;
        let (port1, stat1, _body1) = spawn_stat_missing_probe_server().await;
        let (port2, stat2, _body2) = spawn_stat_missing_probe_server().await;
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
                    .stat_missing(true)
                    .build()
                    .unwrap(),
                1,
            ),
        ]);

        let request = request_context(b"BODY <missing@example.com>\r\n");
        session.spawn_non_primary_tier_stat_prefetch(
            &router,
            &request,
            &ArticleAvailability::new(),
            SuppressedBackends::empty(),
        );

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let probed = stat1.load(Ordering::SeqCst) == 1 && stat2.load(Ordering::SeqCst) == 1;
                let settled = [1, 2].iter().all(|&backend| {
                    router
                        .backend_provider(BackendId::from_index(backend))
                        .is_some_and(|provider| provider.status_counts().available == 1)
                });
                if probed && settled {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("prefetch probes should finish and return their connections");
        assert_connection_returned_idle(&router, 1);
        assert_connection_returned_idle(&router, 2);
    }

    /// Unrequested bytes queued behind the STAT reply. The 430 itself was
    /// fully framed, so it must still count while the socket is retired.
    const STAT_430_WITH_STRAY_REPLY: &[u8] = b"430 Missing\r\n223 0 <stray@example.com>\r\n";

    fn assert_connection_retired(router: &BackendSelector, backend: usize) {
        let status = router
            .backend_provider(BackendId::from_index(backend))
            .expect("backend is registered")
            .status_counts();
        assert_eq!(
            (status.size, status.available),
            (0, 0),
            "backend {backend}: a probe connection with queued bytes must be retired"
        );
    }

    #[tokio::test]
    async fn retry_stat_sweep_keeps_430_when_probe_connection_is_retired() {
        let session = test_session();
        let (port0, stat0, _body0) = spawn_stat_probe_server(STAT_430_WITH_STRAY_REPLY).await;
        let (port1, stat1, _body1) = spawn_stat_probe_server(STAT_430_WITH_STRAY_REPLY).await;
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
            backlogged: &mut BackloggedBackends::default(),
            request_origin: ArticleRequestOrigin::Frontend,
            budget: crate::pool::admission::ArticleAdmission::default().start_request(),
        };
        session
            .parallel_retry_stat_sweep(&router, &request, &mut state)
            .await
            .expect("parallel retry STAT sweep should succeed");

        assert_eq!(stat0.load(Ordering::SeqCst), 1);
        assert_eq!(stat1.load(Ordering::SeqCst), 1);
        assert!(availability.is_missing_slot(crate::cache::AvailabilitySlot::new(0).unwrap()));
        assert!(availability.is_missing_slot(crate::cache::AvailabilitySlot::new(1).unwrap()));
        assert_connection_retired(&router, 0);
        assert_connection_retired(&router, 1);
    }

    #[tokio::test]
    async fn non_primary_stat_prefetch_keeps_430_when_probe_connection_is_retired() {
        let session = test_session();
        let (port0, _stat0, _body0) = spawn_stat_missing_probe_server().await;
        let (port1, stat1, _body1) = spawn_stat_probe_server(STAT_430_WITH_STRAY_REPLY).await;
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
        session.spawn_non_primary_tier_stat_prefetch(
            &router,
            &request,
            &ArticleAvailability::new(),
            SuppressedBackends::empty(),
        );

        let msg_id = MessageId::new("<missing@example.com>".to_string()).expect("valid message id");
        let cached = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(cached) = session.cache.get(&msg_id).await {
                    break cached;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("prefetch should record the 430 despite retiring the connection");
        assert_eq!(stat1.load(Ordering::SeqCst), 1);
        assert!(
            cached
                .availability()
                .is_missing_slot(crate::cache::AvailabilitySlot::new(1).unwrap())
        );
        assert_connection_retired(&router, 1);
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
            request_origin: ArticleRequestOrigin::Frontend,
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
            backlogged: &mut BackloggedBackends::default(),
            budget: crate::pool::admission::ArticleAdmission::default().start_request(),
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
    async fn retry_stat_sweep_marks_slow_430_backends_missing() {
        let session = test_session();
        let (slow_port, slow_stat, slow_body) =
            spawn_delayed_stat_missing_probe_server(Duration::from_millis(350)).await;
        let (fast_port, fast_stat, fast_body) = spawn_stat_missing_probe_server().await;
        let router = router_with_tiered_backends([
            (
                DeadpoolConnectionProvider::builder("127.0.0.1", slow_port)
                    .max_connections(1)
                    .stat_missing(true)
                    .build()
                    .unwrap(),
                0,
            ),
            (
                DeadpoolConnectionProvider::builder("127.0.0.1", fast_port)
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
            request_origin: ArticleRequestOrigin::Frontend,
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
            backlogged: &mut BackloggedBackends::default(),
            budget: crate::pool::admission::ArticleAdmission::default().start_request(),
        };

        tokio::time::timeout(
            Duration::from_secs(2),
            session.parallel_retry_stat_sweep(&router, &request, &mut state),
        )
        .await
        .expect("parallel retry STAT sweep should not hang")
        .expect("parallel retry STAT sweep should succeed");

        assert!(
            state
                .availability
                .is_missing_slot(crate::cache::AvailabilitySlot::new(0).unwrap()),
            "slow STAT=430 backend should still be classified as missing"
        );
        assert!(
            state
                .availability
                .is_missing_slot(crate::cache::AvailabilitySlot::new(1).unwrap()),
            "fast STAT=430 backend should be classified as missing"
        );
        assert_eq!(
            state.unavailable_backends.bits(),
            0,
            "authoritative 430 probes should not suppress backends as unavailable"
        );
        assert_eq!(slow_stat.load(Ordering::SeqCst), 1);
        assert_eq!(fast_stat.load(Ordering::SeqCst), 1);
        assert_eq!(slow_body.load(Ordering::SeqCst), 0);
        assert_eq!(fast_body.load(Ordering::SeqCst), 0);
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
        let has_backend_1 = cached
            .availability()
            .is_missing_slot(crate::cache::AvailabilitySlot::new(1).unwrap());
        assert!(
            has_backend_1,
            "tier-1 backend should be marked missing from STAT=430"
        );
    }

    /// Serves one BODY through the real per-command request path with a
    /// primary that has the article and a lower tier that answers STAT `430`.
    /// Returns the lower-tier STAT count once background probes have settled.
    async fn lower_tier_stats_for_routed_body(prefetch: LowerTierStatPrefetch) -> usize {
        let session = test_session();
        let (primary_port, primary_body) = spawn_article_response_server(
            b"222 0 <found@example.com> body follows\r\npayload\r\n.\r\n",
        )
        .await;
        let (lower_port, lower_stat, lower_body) = spawn_stat_missing_probe_server().await;
        let router = router_with_tiered_backends_and_prefetch(
            prefetch,
            [
                (
                    DeadpoolConnectionProvider::builder("127.0.0.1", primary_port)
                        .max_connections(1)
                        .build()
                        .unwrap(),
                    0,
                ),
                (
                    DeadpoolConnectionProvider::builder("127.0.0.1", lower_port)
                        .max_connections(2)
                        .stat_missing(true)
                        .build()
                        .unwrap(),
                    1,
                ),
            ],
        );
        let (mut client_writer, _client_read, _client_write) = client_writer_pair().await;
        let mut request = request_context(b"BODY <found@example.com>\r\n");
        let mut backend_connection = None;
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_to_client_bytes = BackendToClientBytes::zero();

        tokio::time::timeout(
            Duration::from_secs(2),
            session.route_and_execute_request(
                Arc::clone(&router),
                &mut request,
                &mut client_writer,
                &mut backend_connection,
                &mut client_to_backend_bytes,
                &mut backend_to_client_bytes,
            ),
        )
        .await
        .expect("routed BODY should not hang")
        .expect("routed BODY should succeed");
        finalize_backend_connection(&mut backend_connection);

        assert_eq!(primary_body.load(Ordering::SeqCst), 1);
        assert_eq!(lower_body.load(Ordering::SeqCst), 0);
        settled_stat_count(&lower_stat).await
    }

    /// Lets background probes land, then reads the STAT count; a duplicate
    /// dispatch would arrive within the same window as the first.
    async fn settled_stat_count(stat_commands: &AtomicUsize) -> usize {
        let _ = tokio::time::timeout(Duration::from_secs(1), async {
            while stat_commands.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        stat_commands.load(Ordering::SeqCst)
    }

    #[tokio::test]
    async fn routed_body_request_prefetches_each_lower_tier_backend_once() {
        assert_eq!(
            lower_tier_stats_for_routed_body(LowerTierStatPrefetch::FirstAttempt).await,
            1,
            "one request must probe each lower-tier backend at most once"
        );
    }

    #[tokio::test]
    async fn routed_body_request_skips_lower_tier_prefetch_when_off() {
        assert_eq!(
            lower_tier_stats_for_routed_body(LowerTierStatPrefetch::Off).await,
            0
        );
    }

    #[tokio::test]
    async fn routed_body_request_skips_lower_tier_prefetch_without_primary_miss() {
        assert_eq!(
            lower_tier_stats_for_routed_body(LowerTierStatPrefetch::AfterPrimaryMiss).await,
            0
        );
    }

    #[tokio::test]
    async fn after_primary_miss_prefetch_probes_once_a_tier_zero_miss_is_known() {
        let session = test_session();
        let (port0, stat0, _body0) = spawn_stat_missing_probe_server().await;
        let (port1, stat1, _body1) = spawn_stat_missing_probe_server().await;
        let router = router_with_tiered_backends_and_prefetch(
            LowerTierStatPrefetch::AfterPrimaryMiss,
            [
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
            ],
        );
        let request = request_context(b"BODY <missing@example.com>\r\n");
        let mut availability = ArticleAvailability::new();
        availability.record_missing_slot(crate::cache::AvailabilitySlot::new(0).unwrap());

        session.spawn_non_primary_tier_stat_prefetch(
            &router,
            &request,
            &availability,
            SuppressedBackends::empty(),
        );

        assert_eq!(settled_stat_count(&stat1).await, 1);
        assert_eq!(stat0.load(Ordering::SeqCst), 0, "tier 0 must not be probed");
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

    #[tokio::test]
    async fn checkout_retires_unhealthy_cached_batch_connection_before_reuse() {
        let session = test_session();
        let (port, accept_count) = spawn_greeting_server().await;
        let provider = DeadpoolConnectionProvider::builder("127.0.0.1", port)
            .max_connections(2)
            .build()
            .unwrap();

        let request = request_context(b"BODY <cached-health@example.com>\r\n");
        let backend_id = BackendId::from_index(0);
        let conn = provider
            .checkout_connection_guard()
            .await
            .expect("initial pooled connection should be created")
            .activate();
        let mut backend_connection = Some(BackendLease::new(backend_id, conn));
        if let Some(lease) = backend_connection.as_mut() {
            lease
                .connection
                .get_mut()
                .queue_pending_bytes(b"stale")
                .expect("test should be able to inject queued bytes");
        }

        let guard = session
            .checkout_direct_backend_connection(
                &provider,
                backend_id,
                &request,
                super::BackendAttemptConnection {
                    lease: &mut backend_connection,
                    budget: crate::pool::admission::ArticleAdmission::default()
                        .start_request()
                        .checkout(),
                },
            )
            .await
            .expect("checkout should recover with a fresh pooled connection");
        drop(guard);

        assert_eq!(
            accept_count.load(Ordering::SeqCst),
            2,
            "checkout should discard the dead cached connection and open a fresh one"
        );
    }

    #[tokio::test]
    async fn first_attempt_skips_stat_missing_probe() {
        let session = test_session();
        let (port, stat_commands, body_commands) = spawn_stat_missing_probe_server().await;
        let provider = DeadpoolConnectionProvider::builder("127.0.0.1", port)
            .max_connections(1)
            .stat_missing(true)
            .build()
            .unwrap();
        let router = router_with_backend(provider);
        let (mut client_writer, _client_read, _client_write) = client_writer_pair().await;

        let mut request = request_context(b"BODY <missing@example.com>\r\n");
        let mut availability = ArticleAvailability::new();
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_connection = None;
        let mut unavailable_backends = SuppressedBackends::empty();
        let mut state = ArticleAttemptState {
            request_origin: ArticleRequestOrigin::Frontend,
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
            backlogged: &mut BackloggedBackends::default(),
            budget: crate::pool::admission::ArticleAdmission::default().start_request(),
        };

        let result = session
            .try_backend_for_article(&router, &mut request, &mut client_writer, &mut state, false)
            .await
            .expect("first BODY attempt should complete without transport error");

        assert!(matches!(result, BackendAttemptResult::Success));
        assert_eq!(
            stat_commands.load(Ordering::SeqCst),
            0,
            "STAT probe is retry-only and should not run on first attempt"
        );
        assert_eq!(body_commands.load(Ordering::SeqCst), 1);
        assert!(!availability.is_missing_slot(crate::cache::AvailabilitySlot::new(0).unwrap()));
        finalize_backend_connection(&mut backend_connection);
    }

    #[tokio::test]
    async fn track_stat_stores_synthetic_response_in_payload_cache() {
        let cache = Arc::new(UnifiedCache::memory(1024, Duration::from_secs(60)));
        let session = test_session_with_cache(cache.clone());
        let request = request_context(b"STAT <stat-disabled@example.com>\r\n");
        let msg_id =
            MessageId::new("<stat-disabled@example.com>".to_string()).expect("valid message id");

        session.apply_cache_action(
            CacheAction::TrackStat,
            ResponseWriteParams {
                request: &request,
                article_request: crate::command::CommandHandler::article_lookup_request(&request),
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
        let (mut client_writer, _client_read, _client_write) = client_writer_pair().await;

        let mut request = request_context(b"BODY <bad-response@example.com>\r\n");
        let mut availability = ArticleAvailability::new();
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_connection = None;
        let mut unavailable_backends = SuppressedBackends::empty();
        let mut state = ArticleAttemptState {
            request_origin: ArticleRequestOrigin::Frontend,
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
            backlogged: &mut BackloggedBackends::default(),
            budget: crate::pool::admission::ArticleAdmission::default().start_request(),
        };

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            session.try_backend_for_article(
                &router,
                &mut request,
                &mut client_writer,
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
    async fn full_pool_is_skipped_after_the_article_checkout_wait() {
        let session = test_session();
        let (port, _accepts) = spawn_greeting_server().await;
        let provider = DeadpoolConnectionProvider::builder("127.0.0.1", port)
            .max_connections(1)
            .build()
            .unwrap();
        let _held = provider.checkout_connection_guard().await.unwrap();
        let router = router_with_backend(provider);
        let (mut client_writer, _client_read, _client_write) = client_writer_pair().await;

        let mut request = request_context(b"BODY <busy-backend@example.com>\r\n");
        let mut availability = ArticleAvailability::new();
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_connection = None;
        let mut unavailable_backends = SuppressedBackends::empty();
        let mut state = ArticleAttemptState {
            request_origin: ArticleRequestOrigin::Frontend,
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
            backlogged: &mut BackloggedBackends::default(),
            budget: crate::pool::admission::ArticleAdmission::default().start_request(),
        };

        let started = std::time::Instant::now();
        let result = session
            .try_backend_for_article(&router, &mut request, &mut client_writer, &mut state, false)
            .await
            .expect("a busy backend is unavailable, not a session error");

        assert!(matches!(result, BackendAttemptResult::BackendUnavailable));
        assert!(
            started.elapsed() < crate::constants::pool::ARTICLE_CHECKOUT_WAIT * 2,
            "a busy backend costs one bounded wait, not a pool timeout or a retried wait"
        );
        assert_eq!(availability.missing_bits(), 0, "busy is not a 430 fact");
        assert_eq!(unavailable_backends.bits(), 0b0000_0001);
    }

    #[tokio::test]
    async fn backlogged_small_pool_is_skipped_without_waiting() {
        let session = test_session();
        let (small_port, small_accepts) = spawn_greeting_server().await;
        let small = DeadpoolConnectionProvider::builder("127.0.0.1", small_port)
            .max_connections(1)
            .build()
            .unwrap();
        let _held = small.checkout_connection_guard().await.unwrap();
        // One connection that takes 10 s per command cannot serve a new
        // checkout within the 3 s article checkout wait.
        small.record_service_time(Duration::from_secs(10));
        let good_response = b"222 0 <found@example.com> body follows\r\npayload\r\n.\r\n";
        let (good_port, good_article_commands) = spawn_article_response_server(good_response).await;
        let router = router_with_tiered_backends([
            (small.clone(), 0),
            (
                DeadpoolConnectionProvider::builder("127.0.0.1", good_port)
                    .max_connections(1)
                    .build()
                    .unwrap(),
                0,
            ),
        ]);
        let (mut client_writer, mut client_read, _client_write) = client_writer_pair().await;

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
            backlogged: &mut BackloggedBackends::default(),
            budget: crate::pool::admission::ArticleAdmission::default().start_request(),
            request_origin: ArticleRequestOrigin::Frontend,
        };

        let started = std::time::Instant::now();
        let first = session
            .try_backend_for_article(&router, &mut request, &mut client_writer, &mut state, false)
            .await
            .expect("a backlogged backend is unavailable, not a session error");
        assert!(matches!(first, BackendAttemptResult::BackendUnavailable));
        assert_eq!(state.unavailable_backends.bits(), 0b0000_0001);
        let second = session
            .try_backend_for_article(&router, &mut request, &mut client_writer, &mut state, true)
            .await
            .expect("the next backend serves the article");
        assert!(matches!(second, BackendAttemptResult::Success));

        assert!(
            started.elapsed() < crate::constants::pool::ARTICLE_CHECKOUT_WAIT / 3,
            "a backlogged pool is skipped, not waited on: {:?}",
            started.elapsed()
        );
        assert_eq!(availability.missing_bits(), 0, "a skip is not a 430 fact");
        assert_eq!(request.backend_id(), Some(BackendId::from_index(1)));
        assert_eq!(good_article_commands.load(Ordering::SeqCst), 1);
        assert_eq!(
            small_accepts.load(Ordering::SeqCst),
            1,
            "only the held connection"
        );
        let small_load = small.load().snapshot();
        assert_eq!(small_load.backlog_skips, 1);
        assert_eq!(small_load.busy_timeouts, 0);
        assert_eq!(
            small_load.last_resort_waits, 0,
            "a free alternative means no last-resort wait"
        );
        assert_eq!(small.status_counts().waiting, 0, "the skip never queued");
        finalize_backend_connection(&mut backend_connection);

        let mut written = vec![0; good_response.len()];
        tokio::time::timeout(Duration::from_secs(1), client_read.read_exact(&mut written))
            .await
            .expect("client response should be readable")
            .expect("client read should succeed");
        assert_eq!(written, good_response);
    }

    /// Two full single-connection pools in one tier, both skipped as
    /// backlogged: `slow` estimates 20 s, `fast` 10 s, both beyond the 3 s
    /// checkout wait. Only `fast` serves the article, and its held connection
    /// is returned after `release_after`.
    struct BackloggedTier {
        router: Arc<BackendSelector>,
        slow: DeadpoolConnectionProvider,
        fast: DeadpoolConnectionProvider,
        fast_article_commands: Arc<AtomicUsize>,
    }

    const FOUND_BODY: &[u8] = b"222 0 <found@example.com> body follows\r\npayload\r\n.\r\n";

    async fn backlogged_tier(release_after: Duration) -> BackloggedTier {
        let slow = backlogged_pool().await;
        slow.record_service_time(Duration::from_secs(20));
        let (fast_port, fast_article_commands) = spawn_article_response_server(FOUND_BODY).await;
        let fast = DeadpoolConnectionProvider::builder("127.0.0.1", fast_port)
            .max_connections(1)
            .build()
            .unwrap();
        let held = fast.checkout_connection_guard().await.unwrap();
        fast.record_service_time(Duration::from_secs(10));
        tokio::spawn(async move {
            tokio::time::sleep(release_after).await;
            drop(held);
        });
        let router = router_with_tiered_backends([(slow.clone(), 0), (fast.clone(), 0)]);
        BackloggedTier {
            router,
            slow,
            fast,
            fast_article_commands,
        }
    }

    /// Drive `try_backend_for_article` until it stops reporting a skipped
    /// backend, the way the retry loop does.
    async fn attempt_until_settled(
        router: &Arc<BackendSelector>,
        admission: crate::pool::admission::ArticleAdmission,
    ) -> (
        BackendAttemptResult,
        ArticleAvailability,
        RequestContext,
        tokio::net::tcp::OwnedReadHalf,
    ) {
        let session = test_session_with_backend_metrics(2);
        let (mut client_writer, client_read, _client_write) = client_writer_pair().await;
        let mut request = request_context(b"BODY <found@example.com>\r\n");
        let mut availability = ArticleAvailability::new();
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_connection = None;
        let mut unavailable_backends = SuppressedBackends::empty();
        let mut backlogged = BackloggedBackends::default();
        let mut state = ArticleAttemptState {
            request_origin: ArticleRequestOrigin::Frontend,
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
            backlogged: &mut backlogged,
            budget: admission.start_request(),
        };
        let mut is_retry = false;
        let result = loop {
            let attempt = session
                .try_backend_for_article(
                    router,
                    &mut request,
                    &mut client_writer,
                    &mut state,
                    is_retry,
                )
                .await
                .expect("busy backends are not a session error");
            if !matches!(attempt, BackendAttemptResult::BackendUnavailable) {
                break attempt;
            }
            is_retry = true;
        };
        finalize_backend_connection(&mut backend_connection);
        (result, availability, request, client_read)
    }

    #[tokio::test]
    async fn last_resort_queues_on_the_shortest_backlog_when_the_tier_is_all_backlogged() {
        let tier = backlogged_tier(Duration::from_millis(200)).await;

        let started = std::time::Instant::now();
        let (result, availability, request, mut client_read) = attempt_until_settled(
            &tier.router,
            crate::pool::admission::ArticleAdmission::default(),
        )
        .await;

        assert!(
            matches!(result, BackendAttemptResult::Success),
            "the request waits for the least busy backend instead of giving up"
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(200),
            "it waited for the slot to free"
        );
        assert!(
            elapsed < crate::constants::pool::ARTICLE_CHECKOUT_WAIT,
            "served as soon as the slot freed: {elapsed:?}"
        );
        assert_eq!(request.backend_id(), Some(BackendId::from_index(1)));
        assert_eq!(availability.missing_bits(), 0, "busy is not a 430 fact");
        assert_eq!(tier.fast_article_commands.load(Ordering::SeqCst), 1);

        let fast = tier.fast.load().snapshot();
        assert_eq!(fast.backlog_skips, 1, "skipped first like any backlog");
        assert_eq!(fast.last_resort_waits, 1);
        assert_eq!(fast.last_resort_connected, 1);
        assert_eq!(fast.last_resort_timeouts, 0);
        let slow = tier.slow.load().snapshot();
        assert_eq!(slow.backlog_skips, 1);
        assert_eq!(
            slow.last_resort_waits, 0,
            "the longer backlog is not waited on"
        );
        assert_eq!(tier.slow.status_counts().waiting, 0);

        let mut written = vec![0; FOUND_BODY.len()];
        tokio::time::timeout(Duration::from_secs(1), client_read.read_exact(&mut written))
            .await
            .expect("client response should be readable")
            .expect("client read should succeed");
        assert_eq!(written, FOUND_BODY);
    }

    #[tokio::test]
    async fn backend_that_refused_with_an_error_is_not_a_last_resort() {
        // The refusing backend is idle, so its estimated wait is zero: if a
        // refusal made it a candidate it would be picked over the backlog.
        let (refusing_port, refusing_commands) =
            spawn_article_response_server(b"451 0 <found@example.com>\r\n").await;
        let refusing = DeadpoolConnectionProvider::builder("127.0.0.1", refusing_port)
            .max_connections(1)
            .build()
            .unwrap();
        let (fast_port, fast_article_commands) = spawn_article_response_server(FOUND_BODY).await;
        let fast = DeadpoolConnectionProvider::builder("127.0.0.1", fast_port)
            .max_connections(1)
            .build()
            .unwrap();
        let held = fast.checkout_connection_guard().await.unwrap();
        fast.record_service_time(Duration::from_secs(10));
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            drop(held);
        });
        let router = router_with_tiered_backends([(refusing.clone(), 0), (fast.clone(), 0)]);

        let (result, availability, request, _client_read) =
            attempt_until_settled(&router, crate::pool::admission::ArticleAdmission::default())
                .await;

        assert!(matches!(result, BackendAttemptResult::Success));
        assert_eq!(request.backend_id(), Some(BackendId::from_index(1)));
        assert_eq!(
            refusing_commands.load(Ordering::SeqCst),
            1,
            "the refusing backend is asked once and never waited on"
        );
        assert_eq!(refusing.load().snapshot().last_resort_waits, 0);
        assert_eq!(fast.load().snapshot().last_resort_waits, 1);
        assert_eq!(fast.load().snapshot().last_resort_connected, 1);
        assert_eq!(fast_article_commands.load(Ordering::SeqCst), 1);
        assert_eq!(
            availability.missing_bits(),
            0,
            "a refusal is not a 430 fact"
        );
    }

    #[tokio::test]
    async fn give_up_ends_the_request_when_the_tier_is_all_backlogged() {
        let tier = backlogged_tier(Duration::from_millis(200)).await;

        let started = std::time::Instant::now();
        let (result, availability, _request, _client_read) =
            attempt_until_settled(&tier.router, give_up_admission()).await;

        assert!(matches!(result, BackendAttemptResult::NoRetryableBackend));
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "give-up never waits: {:?}",
            started.elapsed()
        );
        assert_eq!(availability.missing_bits(), 0);
        assert_eq!(tier.fast_article_commands.load(Ordering::SeqCst), 0);
        for provider in [&tier.fast, &tier.slow] {
            let load = provider.load().snapshot();
            assert_eq!(load.backlog_skips, 1);
            assert_eq!(load.last_resort_waits, 0);
        }
    }

    #[tokio::test]
    async fn last_resort_that_times_out_answers_the_unavailable_reply() {
        let provider = backlogged_pool().await;
        let checkout_wait = Duration::from_millis(200);
        let router = router_with_admission(
            provider.clone(),
            crate::pool::admission::ArticleAdmission::new(
                Some(crate::constants::pool::ARTICLE_DEADLINE),
                checkout_wait,
                crate::pool::admission::BacklogPolicy::SkipAbove { percent: 100 },
            ),
        );

        let (status, elapsed) =
            route_and_read_status(&router, b"BODY <last-resort-timeout@example.com>\r\n").await;

        assert_eq!(status, 503);
        assert!(elapsed >= checkout_wait, "the last resort waited");
        assert!(
            elapsed < checkout_wait * 3,
            "one bounded wait, not one per retry: {elapsed:?}"
        );
        let load = provider.load().snapshot();
        assert_eq!(load.backlog_skips, 1);
        assert_eq!(load.last_resort_waits, 1);
        assert_eq!(load.last_resort_connected, 0);
        assert_eq!(load.last_resort_timeouts, 1);
    }

    #[tokio::test]
    async fn last_resort_stays_in_the_preferred_tier() {
        let preferred = backlogged_pool().await;
        let (lower_port, lower_article_commands) =
            spawn_article_response_server(b"430 No such article\r\n").await;
        let lower = DeadpoolConnectionProvider::builder("127.0.0.1", lower_port)
            .max_connections(1)
            .build()
            .unwrap();
        let router = router_with_tiered_backends([(preferred.clone(), 0), (lower, 1)]);
        let admission = crate::pool::admission::ArticleAdmission::new(
            Some(crate::constants::pool::ARTICLE_DEADLINE),
            Duration::from_millis(200),
            crate::pool::admission::BacklogPolicy::SkipAbove { percent: 100 },
        );

        let (result, availability, _request, _client_read) =
            attempt_until_settled(&router, admission).await;

        assert!(matches!(result, BackendAttemptResult::NoRetryableBackend));
        assert_eq!(
            preferred.load().snapshot().last_resort_waits,
            1,
            "the busy preferred tier is waited on"
        );
        assert_eq!(preferred.load().snapshot().last_resort_timeouts, 1);
        assert_eq!(
            lower_article_commands.load(Ordering::SeqCst),
            0,
            "a busy, not missing, preferred tier never sends the lookup to a lower tier"
        );
        assert_eq!(availability.missing_bits(), 0);
    }

    /// Route one request through the full article retry loop and return the
    /// status the client received with how long the request took.
    async fn route_and_read_status(router: &Arc<BackendSelector>, line: &[u8]) -> (u16, Duration) {
        let session = test_session();
        let (mut client_writer, client_read, _client_write) = client_writer_pair().await;
        let mut request = request_context(line);
        let mut backend_connection = None;
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_to_client_bytes = BackendToClientBytes::zero();
        let started = std::time::Instant::now();
        session
            .route_and_execute_request(
                Arc::clone(router),
                &mut request,
                &mut client_writer,
                &mut backend_connection,
                &mut client_to_backend_bytes,
                &mut backend_to_client_bytes,
            )
            .await
            .expect("the request is answered, not a session error");
        let elapsed = started.elapsed();
        finalize_backend_connection(&mut backend_connection);

        let mut status_line = String::new();
        tokio::time::timeout(
            Duration::from_secs(1),
            BufReader::new(client_read).read_line(&mut status_line),
        )
        .await
        .expect("client response should be readable")
        .expect("client read should succeed");
        let status =
            StatusCode::parse(status_line.as_bytes()).expect("response line has a status code");
        (status.as_u16(), elapsed)
    }

    fn router_with_admission(
        provider: DeadpoolConnectionProvider,
        admission: crate::pool::admission::ArticleAdmission,
    ) -> Arc<BackendSelector> {
        let mut router = BackendSelector::new().with_article_admission(admission);
        router.add_backend(
            ServerName::try_new("backend-0".to_string()).unwrap(),
            provider,
            0,
        );
        Arc::new(router)
    }

    const SHORT_DEADLINE: Duration = Duration::from_millis(300);

    fn short_deadline_admission() -> crate::pool::admission::ArticleAdmission {
        crate::pool::admission::ArticleAdmission::new(
            Some(SHORT_DEADLINE),
            crate::constants::pool::ARTICLE_CHECKOUT_WAIT,
            crate::pool::admission::BacklogPolicy::SkipAbove { percent: 100 },
        )
    }

    /// A full pool with no service-time sample: nothing to skip on, so the
    /// request waits, and only its deadline can stop it early.
    async fn full_unsampled_pool() -> DeadpoolConnectionProvider {
        let (port, _accepts) = spawn_greeting_server().await;
        let provider = DeadpoolConnectionProvider::builder("127.0.0.1", port)
            .max_connections(1)
            .build()
            .unwrap();
        std::mem::forget(provider.checkout_connection_guard().await.unwrap());
        provider
    }

    /// A full pool whose single connection takes 10 s per command, so a
    /// checkout is skipped as backlogged at once.
    async fn backlogged_pool() -> DeadpoolConnectionProvider {
        let provider = full_unsampled_pool().await;
        provider.record_service_time(Duration::from_secs(10));
        provider
    }

    /// Same-tier backends sharing one admission policy, in the given order.
    fn router_with_admission_backends(
        providers: Vec<DeadpoolConnectionProvider>,
        admission: crate::pool::admission::ArticleAdmission,
    ) -> Arc<BackendSelector> {
        let mut router = BackendSelector::new().with_article_admission(admission);
        for (index, provider) in providers.into_iter().enumerate() {
            router.add_backend(
                ServerName::try_new(format!("backend-{index}")).unwrap(),
                provider,
                0,
            );
        }
        Arc::new(router)
    }

    #[tokio::test]
    async fn article_deadline_answers_503_in_bounded_time() {
        let provider = full_unsampled_pool().await;
        let router = router_with_admission(provider.clone(), short_deadline_admission());

        let (status, elapsed) =
            route_and_read_status(&router, b"BODY <deadline@example.com>\r\n").await;

        assert_eq!(status, 503, "nothing said the article is missing");
        assert!(
            elapsed >= SHORT_DEADLINE,
            "the request waited for the backend"
        );
        assert!(
            elapsed < crate::constants::pool::ARTICLE_CHECKOUT_WAIT / 2,
            "the deadline, not the checkout wait, bounds the request: {elapsed:?}"
        );
        assert_eq!(provider.load().snapshot().deadline_expiries, 1);
    }

    #[tokio::test]
    async fn unavailable_reply_430_applies_to_an_expired_deadline() {
        let provider = full_unsampled_pool().await;
        let router = router_with_admission(
            provider.clone(),
            short_deadline_admission()
                .with_unavailable_reply(crate::pool::admission::UnavailableReply::NoSuchArticle),
        );

        let (status, elapsed) =
            route_and_read_status(&router, b"BODY <deadline-430@example.com>\r\n").await;

        assert_eq!(status, 430, "an explicit 430 is honoured");
        assert!(elapsed < crate::constants::pool::ARTICLE_CHECKOUT_WAIT / 2);
        assert_eq!(provider.load().snapshot().deadline_expiries, 1);
    }

    /// A false 430 seen under load: one tier-0 backend answered 430, the
    /// slow tier-0 backend that had the article was still queued when the
    /// deadline ran out, and the client was told the article is missing.
    #[tokio::test]
    async fn deadline_after_a_peer_430_answers_503_while_a_same_tier_backend_was_queued() {
        let (missing_port, missing_lookups) =
            spawn_article_response_server(b"430 No such article\r\n").await;
        let missing = DeadpoolConnectionProvider::builder("127.0.0.1", missing_port)
            .max_connections(1)
            .build()
            .unwrap();
        let queued = full_unsampled_pool().await;
        let router = router_with_admission_backends(
            vec![missing, queued.clone()],
            short_deadline_admission(),
        );

        let (status, _) =
            route_and_read_status(&router, b"BODY <queued-peer@example.com>\r\n").await;

        assert_eq!(
            missing_lookups.load(Ordering::SeqCst),
            1,
            "the 430 peer was asked"
        );
        assert_eq!(queued.load().snapshot().deadline_expiries, 1);
        assert_eq!(
            status, 503,
            "a queued backend never answered, so the article is not known missing"
        );
    }

    fn give_up_admission() -> crate::pool::admission::ArticleAdmission {
        crate::pool::admission::ArticleAdmission::default()
            .with_backlog_last_resort(crate::pool::admission::BacklogLastResort::GiveUp)
    }

    #[tokio::test]
    async fn every_backend_skipped_answers_503_by_default() {
        let provider = backlogged_pool().await;
        let router = router_with_admission(provider.clone(), give_up_admission());

        let (status, elapsed) =
            route_and_read_status(&router, b"BODY <all-busy@example.com>\r\n").await;

        assert_eq!(status, 503);
        assert!(elapsed < crate::constants::pool::ARTICLE_CHECKOUT_WAIT / 3);
        assert_eq!(provider.load().snapshot().backlog_skips, 1);
    }

    #[tokio::test]
    async fn skipped_peer_after_a_430_answers_503_by_default() {
        let (missing_port, missing_lookups) =
            spawn_article_response_server(b"430 No such article\r\n").await;
        let missing = DeadpoolConnectionProvider::builder("127.0.0.1", missing_port)
            .max_connections(1)
            .build()
            .unwrap();
        let skipped = backlogged_pool().await;
        let router =
            router_with_admission_backends(vec![missing, skipped.clone()], give_up_admission());

        let (status, _) =
            route_and_read_status(&router, b"BODY <skipped-peer@example.com>\r\n").await;

        assert_eq!(missing_lookups.load(Ordering::SeqCst), 1);
        assert_eq!(skipped.load().snapshot().backlog_skips, 1);
        assert_eq!(status, 503, "the skipped backend was never asked");
    }

    /// A backend that answers 430 next to a busy one, in the same tier.
    async fn skipped_peer_router(
        busy: DeadpoolConnectionProvider,
        admission: crate::pool::admission::ArticleAdmission,
    ) -> Arc<BackendSelector> {
        router_with_admission_backends(vec![missing_server().await, busy], admission)
    }

    /// A backlogged backend whose last `samples` fallback lookups all had
    /// `outcome`.
    async fn backlogged_pool_with_rescue_history(
        outcome: FallbackOutcome,
        samples: u64,
    ) -> DeadpoolConnectionProvider {
        let provider = backlogged_pool().await;
        for _ in 0..samples {
            provider.note_fallback_lookup(outcome);
        }
        provider
    }

    async fn missing_server() -> DeadpoolConnectionProvider {
        let (missing_port, _) = spawn_article_response_server(b"430 No such article\r\n").await;
        DeadpoolConnectionProvider::builder("127.0.0.1", missing_port)
            .max_connections(1)
            .build()
            .unwrap()
    }

    /// Articles gone everywhere: the only unconfirmed backend is busy and has
    /// not had an article another backend lacked in its recent fallback
    /// lookups, so the first lookup answers 430 instead of waiting for it.
    #[tokio::test]
    async fn busy_backend_below_the_rescue_floor_does_not_hold_up_a_430() {
        let busy = backlogged_pool_with_rescue_history(FallbackOutcome::NotFound, 200).await;
        let router = skipped_peer_router(
            busy.clone(),
            crate::pool::admission::ArticleAdmission::default().with_rescue_floor_percent(2),
        )
        .await;

        let (status, elapsed) =
            route_and_read_status(&router, b"BODY <gone-everywhere@example.com>\r\n").await;

        assert_eq!(status, 430);
        assert!(elapsed < crate::constants::pool::ARTICLE_CHECKOUT_WAIT / 3);
        assert_eq!(
            busy.load().snapshot().last_resort_waits,
            0,
            "an unlikely rescuer is not a last resort"
        );
    }

    #[tokio::test]
    async fn busy_backend_that_rescues_is_still_waited_for() {
        let busy = backlogged_pool_with_rescue_history(FallbackOutcome::Found, 200).await;
        let router = skipped_peer_router(
            busy.clone(),
            give_up_admission().with_rescue_floor_percent(2),
        )
        .await;

        let (status, _) =
            route_and_read_status(&router, b"BODY <only-on-busy@example.com>\r\n").await;

        assert_eq!(status, 503, "this backend often has what others lack");
    }

    #[tokio::test]
    async fn rescue_floor_needs_enough_samples() {
        let busy = backlogged_pool_with_rescue_history(
            FallbackOutcome::NotFound,
            crate::pool::admission::MIN_FALLBACK_LOOKUPS - 1,
        )
        .await;
        let router =
            skipped_peer_router(busy, give_up_admission().with_rescue_floor_percent(2)).await;

        let (status, _) =
            route_and_read_status(&router, b"BODY <too-few-samples@example.com>\r\n").await;

        assert_eq!(status, 503);
    }

    #[tokio::test]
    async fn rescue_floor_off_by_default() {
        let busy = backlogged_pool_with_rescue_history(FallbackOutcome::NotFound, 200).await;
        let router = skipped_peer_router(busy, give_up_admission()).await;

        let (status, _) = route_and_read_status(&router, b"BODY <floor-off@example.com>\r\n").await;

        assert_eq!(status, 503);
    }

    /// Before any backend has answered 430 the article may exist, so a busy
    /// backend under the floor is still the last resort.
    #[tokio::test(start_paused = true)]
    async fn rescue_floor_does_not_apply_before_any_430() {
        let busy = backlogged_pool_with_rescue_history(FallbackOutcome::NotFound, 200).await;
        let router = router_with_admission(
            busy.clone(),
            crate::pool::admission::ArticleAdmission::default().with_rescue_floor_percent(2),
        );

        let _ = route_and_read_status(&router, b"BODY <fresh-article@example.com>\r\n").await;

        assert_eq!(busy.load().snapshot().last_resort_waits, 1);
    }

    /// Passing over a same-tier unlikely rescuer must not send the request
    /// down a tier before that backend answered 430.
    #[tokio::test(start_paused = true)]
    async fn rescue_floor_keeps_the_tier_rule() {
        let busy = backlogged_pool_with_rescue_history(FallbackOutcome::NotFound, 200).await;
        let (lower_port, lower_lookups) =
            spawn_article_response_server(b"430 No such article\r\n").await;
        let lower = DeadpoolConnectionProvider::builder("127.0.0.1", lower_port)
            .max_connections(1)
            .build()
            .unwrap();
        let router = router_with_tiered_backends_and_admission(
            vec![(missing_server().await, 0), (busy.clone(), 0), (lower, 1)],
            crate::pool::admission::ArticleAdmission::default().with_rescue_floor_percent(2),
        );

        let _ = route_and_read_status(&router, b"BODY <tier-rule@example.com>\r\n").await;

        assert_eq!(
            busy.load().snapshot().last_resort_waits,
            1,
            "waited on the tier-0 backend"
        );
        assert_eq!(
            lower_lookups.load(Ordering::SeqCst),
            0,
            "never reached tier 1"
        );
    }

    /// A backend that failed rather than being busy keeps the normal 503,
    /// whatever its rescue rate.
    #[tokio::test]
    async fn rescue_floor_needs_the_backend_to_be_busy() {
        let failing = DeadpoolConnectionProvider::builder(
            "127.0.0.1",
            spawn_connection_failure_server().await,
        )
        .max_connections(1)
        .build()
        .unwrap();
        for _ in 0..200 {
            failing.note_fallback_lookup(FallbackOutcome::NotFound);
        }
        let router =
            skipped_peer_router(failing, give_up_admission().with_rescue_floor_percent(2)).await;

        let (status, _) =
            route_and_read_status(&router, b"BODY <failing-not-busy@example.com>\r\n").await;

        assert_eq!(status, 503);
    }

    /// The busy estimate can say "free" while every connection is in use
    /// (here: no service-time sample yet). A backend under the rescue floor
    /// is still only used when a connection is idle, so the lookup answers
    /// 430 at once instead of queueing on it until the deadline.
    #[tokio::test]
    async fn unlikely_rescuer_without_an_idle_connection_is_skipped_even_when_not_estimated_busy() {
        let full = full_unsampled_pool().await;
        for _ in 0..200 {
            full.note_fallback_lookup(FallbackOutcome::NotFound);
        }
        let router = skipped_peer_router(
            full.clone(),
            short_deadline_admission().with_rescue_floor_percent(2),
        )
        .await;

        let (status, elapsed) =
            route_and_read_status(&router, b"BODY <no-idle-connection@example.com>\r\n").await;

        assert_eq!(status, 430);
        assert!(
            elapsed < SHORT_DEADLINE / 2,
            "did not wait for the full pool: {elapsed:?}"
        );
        assert_eq!(full.load().snapshot().deadline_expiries, 0);
    }

    fn floor_admission() -> crate::pool::admission::ArticleAdmission {
        crate::pool::admission::ArticleAdmission::default().with_rescue_floor_percent(2)
    }

    /// Give `provider` a rescue rate well under a 2% floor.
    fn put_under_rescue_floor(provider: &DeadpoolConnectionProvider) {
        for _ in 0..200 {
            provider.note_fallback_lookup(FallbackOutcome::NotFound);
        }
    }

    /// A `STAT`-capable backend under the rescue floor whose only connection
    /// is in use, with its `STAT` counter.
    async fn full_stat_backend_under_floor() -> (DeadpoolConnectionProvider, Arc<AtomicUsize>) {
        let (port, stats, _bodies) = spawn_stat_missing_probe_server().await;
        let provider = DeadpoolConnectionProvider::builder("127.0.0.1", port)
            .max_connections(1)
            .stat_missing(true)
            .build()
            .unwrap();
        std::mem::forget(provider.checkout_connection_guard().await.unwrap());
        put_under_rescue_floor(&provider);
        (provider, stats)
    }

    /// The idle check is part of the checkout: of a burst of fallback
    /// lookups, only one gets the under-floor backend's single connection,
    /// and the rest answer 430 at once instead of queueing behind it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn burst_of_fallback_lookups_uses_an_unlikely_rescuers_connection_once() {
        const BURST: usize = 6;
        let (missing_port, _) = spawn_article_response_server(b"430 No such article\r\n").await;
        let missing = DeadpoolConnectionProvider::builder("127.0.0.1", missing_port)
            .max_connections(BURST)
            .build()
            .unwrap();
        // The lookup that gets this backend's connection holds it, so every
        // other lookup in the burst finds it busy.
        let (silent_port, _accepts, silent_lookups) = spawn_silent_article_server().await;
        let unlikely = DeadpoolConnectionProvider::builder("127.0.0.1", silent_port)
            .max_connections(1)
            .build()
            .unwrap();
        put_under_rescue_floor(&unlikely);
        let router = router_with_tiered_backends_and_admission(
            vec![(missing, 0), (unlikely.clone(), 1)],
            floor_admission(),
        );

        let mut lookups = tokio::task::JoinSet::new();
        for index in 0..BURST {
            let router = Arc::clone(&router);
            lookups.spawn(async move {
                let line = format!("BODY <burst-{index}@example.com>\r\n");
                route_and_read_status(&router, line.as_bytes()).await
            });
        }
        let mut answered = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), async {
            while answered.len() < BURST - 1 {
                answered.push(lookups.join_next().await.unwrap().unwrap());
            }
        })
        .await
        .expect("every lookup but the one holding the connection answers");
        // The one that got the connection may still be opening it.
        tokio::time::timeout(Duration::from_secs(2), async {
            while silent_lookups.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("one lookup reaches the backend");
        lookups.abort_all();

        assert!(
            answered.iter().all(|&(status, _)| status == 430),
            "the rest skip it: {answered:?}"
        );
        assert_eq!(silent_lookups.load(Ordering::SeqCst), 1, "used once");
        let load = unlikely.load().snapshot();
        assert_eq!(load.backlog_skips, BURST as u64 - 1);
        assert_eq!(load.busy_timeouts, 0, "nobody waited for it");
        assert_eq!(load.last_resort_waits, 0);
    }

    /// A connection the session already holds to the backend costs no wait,
    /// so the floor does not skip a backend whose pool is otherwise full.
    #[tokio::test]
    async fn unlikely_rescuer_is_used_through_the_sessions_cached_connection() {
        let (port, lookups) = spawn_article_response_server(
            b"222 0 <found@example.com> body follows\r\npayload\r\n.\r\n",
        )
        .await;
        let unlikely = DeadpoolConnectionProvider::builder("127.0.0.1", port)
            .max_connections(1)
            .build()
            .unwrap();
        put_under_rescue_floor(&unlikely);
        let router = router_with_admission_backends(
            vec![missing_server().await, unlikely.clone()],
            floor_admission(),
        );
        let unlikely_id = BackendId::from_index(1);
        let cached = unlikely
            .checkout_connection_guard()
            .await
            .unwrap()
            .activate();
        assert_eq!(unlikely.status_counts().available, 0, "the pool is full");

        let session = test_session_with_backend_metrics(2);
        let (mut client_writer, _client_read, _client_write) = client_writer_pair().await;
        let mut request = request_context(b"BODY <found@example.com>\r\n");
        let mut availability = ArticleAvailability::new();
        availability.record_missing_slot(crate::cache::AvailabilitySlot::new(0).unwrap());
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_connection = Some(BackendLease::new(unlikely_id, cached));
        let mut unavailable_backends = SuppressedBackends::empty();
        let mut backlogged = BackloggedBackends::default();
        let mut state = ArticleAttemptState {
            request_origin: ArticleRequestOrigin::Frontend,
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
            backlogged: &mut backlogged,
            budget: floor_admission().start_request(),
        };
        let result = session
            .try_backend_for_article(&router, &mut request, &mut client_writer, &mut state, false)
            .await
            .expect("the cached connection serves the lookup");
        finalize_backend_connection(&mut backend_connection);

        assert!(matches!(result, BackendAttemptResult::Success));
        assert_eq!(lookups.load(Ordering::SeqCst), 1);
        assert_eq!(unlikely.load().snapshot().backlog_skips, 0);
        assert_eq!(unlikely.load().rescue_pass_overs(), 0);
    }

    /// One pass-over per request: the attempt that skipped the backend and
    /// the last-resort pick that later looks at it count once together, so
    /// the probe rate stays one in 16 requests.
    #[tokio::test(start_paused = true)]
    async fn rescue_floor_counts_one_pass_over_per_request() {
        use crate::pool::admission::RESCUE_PROBE_EVERY;

        let busy = backlogged_pool_with_rescue_history(FallbackOutcome::NotFound, 200).await;
        let (lower_port, _) = spawn_article_response_server(b"430 No such article\r\n").await;
        let lower = DeadpoolConnectionProvider::builder("127.0.0.1", lower_port)
            .max_connections(1)
            .build()
            .unwrap();
        let router = router_with_tiered_backends_and_admission(
            vec![(missing_server().await, 0), (busy.clone(), 0), (lower, 1)],
            floor_admission(),
        );

        for index in 0..RESCUE_PROBE_EVERY {
            let line = format!("BODY <one-pass-over-{index}@example.com>\r\n");
            let _ = route_and_read_status(&router, line.as_bytes()).await;
            assert_eq!(busy.load().rescue_pass_overs(), index + 1);
        }
        assert_eq!(
            busy.load().snapshot().last_resort_waits,
            RESCUE_PROBE_EVERY,
            "the tier rule still waits on it every time"
        );
    }

    /// With backlog skipping off nothing is skipped, and neither is a backend
    /// under the floor: the request waits on it as before.
    #[tokio::test]
    async fn rescue_floor_follows_backlog_skip_off() {
        let full = full_unsampled_pool().await;
        put_under_rescue_floor(&full);
        let router = skipped_peer_router(
            full.clone(),
            crate::pool::admission::ArticleAdmission::new(
                Some(SHORT_DEADLINE),
                crate::constants::pool::ARTICLE_CHECKOUT_WAIT,
                crate::pool::admission::BacklogPolicy::Queue,
            )
            .with_rescue_floor_percent(2),
        )
        .await;

        let (status, elapsed) =
            route_and_read_status(&router, b"BODY <backlog-skip-off@example.com>\r\n").await;

        assert_eq!(status, 503, "the busy backend never answered");
        assert!(elapsed >= SHORT_DEADLINE, "it was waited on");
        let load = full.load().snapshot();
        assert_eq!(load.backlog_skips, 0);
        assert_eq!(load.deadline_expiries, 1);
        assert_eq!(full.load().rescue_pass_overs(), 0);
    }

    /// The STAT sweep after a 430 takes an under-floor backend only if a
    /// connection is free, and the request's last-resort pick does not count
    /// that pass-over again.
    #[tokio::test]
    async fn retry_stat_sweep_takes_an_unlikely_rescuer_only_when_idle() {
        let session = test_session();
        let (missing_port, _, _) = spawn_stat_missing_probe_server().await;
        let (peer_port, peer_stats, _) = spawn_stat_missing_probe_server().await;
        let (unlikely, unlikely_stats) = full_stat_backend_under_floor().await;
        let stat_backend = |port| {
            DeadpoolConnectionProvider::builder("127.0.0.1", port)
                .max_connections(1)
                .stat_missing(true)
                .build()
                .unwrap()
        };
        let router = router_with_tiered_backends_and_admission(
            vec![
                (stat_backend(missing_port), 0),
                (stat_backend(peer_port), 0),
                (unlikely.clone(), 1),
            ],
            floor_admission(),
        );
        let unlikely_id = BackendId::from_index(2);

        let request = request_context(b"BODY <sweep-floor@example.com>\r\n");
        let mut availability = ArticleAvailability::new();
        availability.record_missing_slot(crate::cache::AvailabilitySlot::new(0).unwrap());
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_connection = None;
        let mut unavailable_backends = SuppressedBackends::empty();
        let mut backlogged = BackloggedBackends::default();
        let mut state = ArticleAttemptState {
            request_origin: ArticleRequestOrigin::Frontend,
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
            backlogged: &mut backlogged,
            budget: floor_admission().start_request(),
        };
        let started = std::time::Instant::now();
        session
            .parallel_retry_stat_sweep(&router, &request, &mut state)
            .await
            .expect("parallel retry STAT sweep should succeed");

        assert!(
            started.elapsed() < crate::constants::pool::STAT_PROBE_CHECKOUT_WAIT / 2,
            "did not wait for the full pool"
        );
        assert_eq!(peer_stats.load(Ordering::SeqCst), 1);
        assert_eq!(unlikely_stats.load(Ordering::SeqCst), 0);
        assert!(state.unavailable_backends.contains(unlikely_id));
        assert!(state.backlogged.is_candidate(unlikely_id));
        assert_eq!(
            state.backlogged.floor_decision(unlikely_id),
            super::FloorDecision::PassedOver
        );
        let load = unlikely.load().snapshot();
        assert_eq!((load.backlog_skips, load.busy_timeouts), (1, 0));
        assert_eq!(unlikely.load().rescue_pass_overs(), 1);

        let _ = super::ClientSession::last_resort_backend(
            &router,
            &mut state,
            SuppressedBackends::empty(),
        );
        assert_eq!(
            unlikely.load().rescue_pass_overs(),
            1,
            "the last-resort pick reuses the request's decision"
        );
    }

    /// The background lower-tier STAT probe after a 430 also takes an
    /// under-floor backend only if a connection is free, and does not use up
    /// a pass-over.
    #[tokio::test]
    async fn lower_tier_stat_prefetch_takes_an_unlikely_rescuer_only_when_idle() {
        let session = test_session();
        let (primary_port, _, _) = spawn_stat_missing_probe_server().await;
        let primary = DeadpoolConnectionProvider::builder("127.0.0.1", primary_port)
            .max_connections(1)
            .stat_missing(true)
            .build()
            .unwrap();
        let (unlikely, unlikely_stats) = full_stat_backend_under_floor().await;
        let router = router_with_tiered_backends_and_admission(
            vec![(primary, 0), (unlikely.clone(), 1)],
            floor_admission(),
        );
        let mut availability = ArticleAvailability::new();
        availability.record_missing_slot(crate::cache::AvailabilitySlot::new(0).unwrap());

        let started = std::time::Instant::now();
        session.spawn_non_primary_tier_stat_prefetch(
            &router,
            &request_context(b"BODY <prefetch-floor@example.com>\r\n"),
            &availability,
            SuppressedBackends::empty(),
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while unlikely.load().snapshot().backlog_skips == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the probe skips the full backend at once");

        assert!(started.elapsed() < crate::constants::pool::STAT_PROBE_CHECKOUT_WAIT / 2);
        assert_eq!(unlikely_stats.load(Ordering::SeqCst), 0);
        assert_eq!(unlikely.load().snapshot().busy_timeouts, 0);
        assert_eq!(unlikely.load().rescue_pass_overs(), 0, "not a pass-over");
    }

    /// A lookup after another backend's 430 is a fallback lookup and feeds
    /// the rescue rate of the backend that answered it.
    #[tokio::test]
    async fn fallback_lookups_feed_the_rescue_rate() {
        let (missing_port, _) = spawn_article_response_server(b"430 No such article\r\n").await;
        let first = DeadpoolConnectionProvider::builder("127.0.0.1", missing_port)
            .max_connections(1)
            .build()
            .unwrap();
        let (second_port, _) = spawn_article_response_server(b"430 No such article\r\n").await;
        let second = DeadpoolConnectionProvider::builder("127.0.0.1", second_port)
            .max_connections(1)
            .build()
            .unwrap();
        let router = router_with_admission_backends(
            vec![first.clone(), second.clone()],
            crate::pool::admission::ArticleAdmission::default(),
        );

        let (status, _) =
            route_and_read_status(&router, b"BODY <fallback-sample@example.com>\r\n").await;

        assert_eq!(status, 430);
        let total =
            first.load().snapshot().fallback_lookups + second.load().snapshot().fallback_lookups;
        assert_eq!(
            total, 1,
            "only the lookup after the first 430 is a fallback lookup"
        );
    }

    #[test]
    fn retry_is_skipped_only_for_failures_the_backend_would_repeat() {
        use crate::connection_error::ConnectionError;
        let backend = || "b".to_string();
        let skipped = [
            ConnectionError::PoolExhausted {
                backend: backend(),
                max_size: 1,
            },
            ConnectionError::LoginBackoff {
                backend: backend(),
                retry_in: Duration::from_secs(5),
            },
            ConnectionError::AuthenticationFailed {
                backend: backend(),
                response: "502 Too many connections".into(),
            },
            ConnectionError::ConnectionLimitExceeded {
                backend: backend(),
                response: "502 connection limit (10) reached".into(),
            },
        ];
        for error in skipped {
            let label = error.to_string();
            assert!(
                super::same_backend_retry_cannot_succeed(&anyhow::Error::from(error)),
                "{label}"
            );
        }

        // A dropped or timed-out connection may well work on a fresh one.
        let retried = [
            ConnectionError::IoError(std::io::Error::other("reset")),
            ConnectionError::IoError(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "connection setup timed out",
            )),
        ];
        for error in retried {
            let label = error.to_string();
            assert!(
                !super::same_backend_retry_cannot_succeed(&anyhow::Error::from(error)),
                "{label}"
            );
        }
        assert!(!super::same_backend_retry_cannot_succeed(&anyhow::anyhow!(
            "not a connection error"
        )));
    }

    /// The production noise: a backend refuses a login ("too many
    /// connections"), and the request retried it 1-9 ms later. The pool was
    /// already in login backoff, so the retry could only be denied. Now the
    /// refusal ends the attempt on that backend at once.
    #[tokio::test]
    async fn refused_login_is_not_retried_on_the_same_backend() {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut lines = BufReader::new(read).lines();
                    write.write_all(b"200 ready\r\n").await.unwrap();
                    let mut index = 0;
                    while let Ok(Some(_line)) = lines.next_line().await {
                        let reply: &[u8] = match index {
                            0 => b"381 password required\r\n",
                            _ => b"502 connection limit (10) reached\r\n",
                        };
                        index += 1;
                        if write.write_all(reply).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        let refusing = DeadpoolConnectionProvider::builder("127.0.0.1", port)
            .max_connections(1)
            .username("user")
            .password("pass")
            .build()
            .unwrap();
        let router = router_with_admission_backends(
            vec![refusing.clone()],
            crate::pool::admission::ArticleAdmission::default(),
        );

        let (status, _) =
            route_and_read_status(&router, b"BODY <limit-reached@example.com>\r\n").await;

        assert_eq!(status, 503, "a refused login is not a 430");
        let churn = refusing.connection_churn();
        assert_eq!(churn.login_refused, 1);
        assert_eq!(
            churn.backoff_denied, 0,
            "no second checkout may hit the backoff the refusal just started"
        );
    }

    /// Every direct article attempt feeds the answering backend's standing:
    /// a 430 pulls it down and a hit pulls it up.
    #[tokio::test]
    async fn article_attempts_feed_backend_standing() {
        use crate::pool::admission::{ArticleLookupOutcome, STANDING_FULL_PPM};
        let (missing_port, _) = spawn_article_response_server(b"430 No such article\r\n").await;
        let missing = DeadpoolConnectionProvider::builder("127.0.0.1", missing_port)
            .max_connections(1)
            .build()
            .unwrap();
        let router = router_with_admission_backends(
            vec![missing.clone()],
            crate::pool::admission::ArticleAdmission::default(),
        );
        let (status, _) =
            route_and_read_status(&router, b"BODY <standing-miss@example.com>\r\n").await;
        assert_eq!(status, 430);
        assert!(
            missing.article_standing_ppm() < STANDING_FULL_PPM,
            "a 430 lowers standing"
        );

        let (found_port, _) =
            spawn_article_response_server(b"222 0 <standing@example.com>\r\npayload\r\n.\r\n")
                .await;
        let found = DeadpoolConnectionProvider::builder("127.0.0.1", found_port)
            .max_connections(1)
            .build()
            .unwrap();
        // Knock it down first so the climb back is visible.
        for _ in 0..8 {
            found.note_article_lookup(ArticleLookupOutcome::Missing);
        }
        let found_before = found.article_standing_ppm();
        let router = router_with_admission_backends(
            vec![found.clone()],
            crate::pool::admission::ArticleAdmission::default(),
        );
        let (status, _) = route_and_read_status(&router, b"BODY <standing@example.com>\r\n").await;
        assert_eq!(status, 222);
        assert!(
            found.article_standing_ppm() > found_before,
            "a hit raises standing"
        );
    }

    /// Every direct article attempt is timed, so the backend's first-byte
    /// average is fed by real traffic rather than a 1-in-16 sample.
    #[tokio::test]
    async fn article_attempts_feed_first_byte_time() {
        let (port, _) =
            spawn_article_response_server(b"222 0 <timed@example.com>\r\npayload\r\n.\r\n").await;
        let provider = DeadpoolConnectionProvider::builder("127.0.0.1", port)
            .max_connections(1)
            .build()
            .unwrap();
        assert_eq!(
            provider
                .delivery()
                .snapshot(std::time::Instant::now())
                .ttfb_secs,
            None
        );
        let router = router_with_admission_backends(
            vec![provider.clone()],
            crate::pool::admission::ArticleAdmission::default(),
        );
        for id in 0..3 {
            let line = format!("BODY <timed-{id}@example.com>\r\n");
            let (status, _) = route_and_read_status(&router, line.as_bytes()).await;
            assert_eq!(status, 222);
        }
        assert!(
            provider
                .delivery()
                .snapshot(std::time::Instant::now())
                .ttfb_secs
                .is_some(),
            "unsampled timing records every attempt"
        );
    }

    /// The dead-release regression: a slow backend that is always busy never
    /// confirms the miss, so every retry got `503` and a dead release took
    /// minutes to fail. After two `503`s the next unfinished lookup answers
    /// `430`, because the only backend that answered said `430`.
    #[tokio::test]
    async fn repeated_unfinished_lookups_with_only_430_answers_become_430() {
        let router = skipped_peer_router(backlogged_pool().await, give_up_admission()).await;
        let mut statuses = Vec::new();
        for _ in 0..3 {
            let (status, _) =
                route_and_read_status(&router, b"BODY <dead-everywhere@example.com>\r\n").await;
            statuses.push(status);
        }
        assert_eq!(statuses, [503, 503, 430]);

        let (status, _) =
            route_and_read_status(&router, b"BODY <other-article@example.com>\r\n").await;
        assert_eq!(status, 503, "the count is per message-id");
    }

    #[tokio::test]
    async fn unfinished_replies_before_430_zero_keeps_answering_503() {
        let router = skipped_peer_router(
            backlogged_pool().await,
            give_up_admission().with_unfinished_replies_before_430(0),
        )
        .await;
        for _ in 0..4 {
            let (status, _) =
                route_and_read_status(&router, b"BODY <never-converted@example.com>\r\n").await;
            assert_eq!(status, 503);
        }
    }

    #[tokio::test]
    async fn unfinished_lookups_without_any_430_stay_503() {
        let provider = backlogged_pool().await;
        let router = router_with_admission(provider, give_up_admission());
        for _ in 0..4 {
            let (status, _) =
                route_and_read_status(&router, b"BODY <nobody-answered@example.com>\r\n").await;
            assert_eq!(status, 503, "no backend has said the article is missing");
        }
    }

    #[tokio::test]
    async fn unavailable_reply_430_applies_when_every_backend_is_skipped() {
        let provider = backlogged_pool().await;
        let router = router_with_admission(
            provider.clone(),
            give_up_admission()
                .with_unavailable_reply(crate::pool::admission::UnavailableReply::NoSuchArticle),
        );

        let (status, elapsed) =
            route_and_read_status(&router, b"BODY <all-busy-430@example.com>\r\n").await;

        assert_eq!(status, 430);
        assert!(elapsed < crate::constants::pool::ARTICLE_CHECKOUT_WAIT / 3);
        assert_eq!(provider.load().snapshot().backlog_skips, 1);
    }

    #[tokio::test]
    async fn authoritative_misses_answer_430_with_the_default_unavailable_reply() {
        let (port, article_commands) =
            spawn_article_response_server(b"430 No such article\r\n").await;
        let provider = DeadpoolConnectionProvider::builder("127.0.0.1", port)
            .max_connections(1)
            .build()
            .unwrap();
        let router = router_with_admission(
            provider,
            crate::pool::admission::ArticleAdmission::default(),
        );

        let (status, _) = route_and_read_status(&router, b"BODY <missing@example.com>\r\n").await;

        assert_eq!(status, 430, "a real miss on every backend is still a miss");
        assert_eq!(article_commands.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn connection_failure_suppresses_backend_for_current_request() {
        let session = test_session();
        let port = spawn_connection_failure_server().await;
        let provider = DeadpoolConnectionProvider::builder("127.0.0.1", port)
            .max_connections(1)
            .build()
            .unwrap();
        let router = router_with_backend(provider);
        let (mut client_writer, _client_read, _client_write) = client_writer_pair().await;

        let mut request = request_context(b"BODY <connection-failure@example.com>\r\n");
        let mut availability = ArticleAvailability::new();
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_connection = None;
        let mut unavailable_backends = SuppressedBackends::empty();
        let mut state = ArticleAttemptState {
            request_origin: ArticleRequestOrigin::Frontend,
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
            backlogged: &mut BackloggedBackends::default(),
            budget: crate::pool::admission::ArticleAdmission::default().start_request(),
        };

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            session.try_backend_for_article(
                &router,
                &mut request,
                &mut client_writer,
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
        let (mut client_writer, mut client_read, _client_write) = client_writer_pair().await;

        let mut request = request_context(b"BODY <found@example.com>\r\n");
        let mut availability = ArticleAvailability::new();
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_connection = None;
        let mut unavailable_backends = SuppressedBackends::empty();
        let mut state = ArticleAttemptState {
            request_origin: ArticleRequestOrigin::Frontend,
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
            backlogged: &mut BackloggedBackends::default(),
            budget: crate::pool::admission::ArticleAdmission::default().start_request(),
        };

        let first = session
            .try_backend_for_article(&router, &mut request, &mut client_writer, &mut state, false)
            .await
            .expect("invalid response should be handled");
        assert!(matches!(first, BackendAttemptResult::BackendUnavailable));
        assert_eq!(state.availability.missing_bits(), 0);
        assert_eq!(state.availability.missing_bits(), 0);
        assert_eq!(state.unavailable_backends.bits(), 0b0000_0001);

        let second = session
            .try_backend_for_article(&router, &mut request, &mut client_writer, &mut state, true)
            .await
            .expect("same-tier retry should succeed");
        assert!(matches!(second, BackendAttemptResult::Success));
        assert_eq!(bad_article_commands.load(Ordering::SeqCst), 1);
        assert_eq!(good_article_commands.load(Ordering::SeqCst), 1);
        assert_eq!(availability.missing_bits(), 0);
        assert_eq!(availability.missing_bits(), 0);
        assert_eq!(request.backend_id(), Some(BackendId::from_index(1)));
        finalize_backend_connection(&mut backend_connection);

        let mut written = vec![0; good_response.len()];
        tokio::time::timeout(Duration::from_secs(1), client_read.read_exact(&mut written))
            .await
            .expect("client response should be readable")
            .expect("client read should succeed");
        assert_eq!(written, good_response);
    }

    #[tokio::test(start_paused = true)]
    async fn setup_timeout_uses_the_next_backend() {
        let stalled_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stalled_addr = stalled_listener.local_addr().unwrap();
        let good_response = b"222 0 <found@example.com> body follows\r\npayload\r\n.\r\n";
        let (good_port, good_article_commands) = spawn_article_response_server(good_response).await;
        let stalled =
            DeadpoolConnectionProvider::builder(stalled_addr.ip().to_string(), stalled_addr.port())
                .max_connections(1)
                .build()
                .unwrap();
        let router = router_with_tiered_backends([
            (stalled.clone(), 0),
            (
                DeadpoolConnectionProvider::builder("127.0.0.1", good_port)
                    .max_connections(1)
                    .build()
                    .unwrap(),
                0,
            ),
        ]);
        let session = test_session();
        let (mut client_writer, mut client_read, _client_write) = client_writer_pair().await;
        let mut request = request_context(b"BODY <found@example.com>\r\n");
        let mut availability = ArticleAvailability::new();
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_connection = None;
        let mut unavailable_backends = SuppressedBackends::empty();

        let mut attempt = tokio::spawn(async move {
            let mut state = ArticleAttemptState {
                request_origin: ArticleRequestOrigin::Frontend,
                availability: &mut availability,
                client_to_backend_bytes: &mut client_to_backend_bytes,
                backend_connection: &mut backend_connection,
                unavailable_backends: &mut unavailable_backends,
                backlogged: &mut BackloggedBackends::default(),
                // Two 10 s setup timeouts outlast the default 20 s request
                // deadline; this test is about setup fallback, not the deadline.
                budget: crate::pool::admission::ArticleAdmission::new(
                    None,
                    crate::constants::pool::ARTICLE_CHECKOUT_WAIT,
                    crate::pool::admission::BacklogPolicy::Queue,
                )
                .start_request(),
            };
            let first = session
                .try_backend_for_article(
                    &router,
                    &mut request,
                    &mut client_writer,
                    &mut state,
                    false,
                )
                .await;
            let second = session
                .try_backend_for_article(
                    &router,
                    &mut request,
                    &mut client_writer,
                    &mut state,
                    true,
                )
                .await;
            let mut written = vec![0; good_response.len()];
            let read = client_read.read_exact(&mut written).await;
            (
                first,
                second,
                state.unavailable_backends.bits(),
                written,
                read,
                request.backend_id(),
            )
        });

        let (first_socket, _) = tokio::select! {
            accepted = stalled_listener.accept() => accepted.unwrap(),
            _ = &mut attempt => panic!("attempt finished before the stalled backend accepted"),
        };
        tokio::spawn(async move {
            let _held = first_socket;
            std::future::pending::<()>().await;
        });
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(
            !attempt.is_finished(),
            "setup timeout finished before its deadline"
        );

        tokio::time::advance(crate::constants::timeout::CONNECTION + Duration::from_millis(1))
            .await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_millis(10)).await;

        let (retry_socket, _) = tokio::select! {
            accepted = stalled_listener.accept() => accepted.unwrap(),
            _ = &mut attempt => panic!("attempt finished before the setup retry connected"),
        };
        tokio::spawn(async move {
            let _held = retry_socket;
            std::future::pending::<()>().await;
        });
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(
            !attempt.is_finished(),
            "the setup retry finished before its deadline"
        );

        tokio::time::advance(crate::constants::timeout::CONNECTION + Duration::from_millis(1))
            .await;
        for _ in 0..32 {
            if attempt.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            attempt.is_finished(),
            "the next backend was not reached after the setup deadline"
        );

        let (first, second, unavailable_bits, written, read, backend_id) =
            attempt.await.expect("backend fallback panicked");
        let first = first.expect("setup timeout should stay a backend failure");
        assert!(matches!(first, BackendAttemptResult::BackendUnavailable));
        assert_eq!(unavailable_bits, 0b0000_0001);
        assert_eq!(
            crate::pool::ConnectionProvider::status(&stalled)
                .created
                .get(),
            0,
            "a timed-out setup returned a connection to the pool"
        );
        let second = second.expect("the next backend should serve the article");
        assert!(matches!(second, BackendAttemptResult::Success));
        assert_eq!(backend_id, Some(BackendId::from_index(1)));
        assert_eq!(good_article_commands.load(Ordering::SeqCst), 1);
        read.expect("client read should succeed");
        assert_eq!(written, good_response);
    }

    #[tokio::test(start_paused = true)]
    async fn silent_backend_times_out_retries_fresh_and_is_not_pooled() {
        let (port, accepts, article_commands) = spawn_silent_article_server().await;
        let provider = DeadpoolConnectionProvider::builder("127.0.0.1", port)
            .max_connections(2)
            .build()
            .unwrap();
        let router = router_with_backend(provider.clone());
        let session = test_session();
        let (mut client_writer, _client_read, _client_write) = client_writer_pair().await;
        let mut request = request_context(b"BODY <silent@example.com>\r\n");
        let mut availability = ArticleAvailability::new();
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_connection = None;
        let mut unavailable_backends = SuppressedBackends::empty();
        let mut state = ArticleAttemptState {
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
            backlogged: &mut BackloggedBackends::default(),
            request_origin: ArticleRequestOrigin::Frontend,
            // Two 30 s first-byte timeouts outlast the default 20 s request
            // deadline; this test is about the fresh retry, not the deadline.
            budget: crate::pool::admission::ArticleAdmission::new(
                None,
                crate::constants::pool::ARTICLE_CHECKOUT_WAIT,
                crate::pool::admission::BacklogPolicy::Queue,
            )
            .start_request(),
        };

        let started = tokio::time::Instant::now();
        let result = tokio::time::timeout(
            crate::constants::timeout::BACKEND_READ * 4,
            session.try_backend_for_article(
                &router,
                &mut request,
                &mut client_writer,
                &mut state,
                false,
            ),
        )
        .await
        .expect("a silent backend must not hold the request forever")
        .expect("a first-byte timeout should stay a backend failure");

        assert!(matches!(result, BackendAttemptResult::BackendUnavailable));
        assert!(started.elapsed() >= crate::constants::timeout::BACKEND_READ * 2);
        assert_eq!(state.unavailable_backends.bits(), 0b0000_0001);
        assert_eq!(
            state.availability.missing_bits(),
            0,
            "a timeout is not a 430 fact"
        );
        assert_eq!(
            accepts.load(Ordering::SeqCst),
            2,
            "retry uses a fresh connection"
        );
        assert_eq!(article_commands.load(Ordering::SeqCst), 2);
        assert_eq!(
            crate::pool::ConnectionProvider::status(&provider)
                .created
                .get(),
            0,
            "a timed-out connection was returned to the pool"
        );
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
        let (mut client_writer, _client_read, _client_write) = client_writer_pair().await;

        let mut request = request_context(b"BODY <found@example.com>\r\n");
        let mut availability = ArticleAvailability::new();
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_connection = None;
        let mut unavailable_backends = SuppressedBackends::empty();
        let mut state = ArticleAttemptState {
            request_origin: ArticleRequestOrigin::Frontend,
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
            backlogged: &mut BackloggedBackends::default(),
            budget: crate::pool::admission::ArticleAdmission::default().start_request(),
        };

        let first = session
            .try_backend_for_article(&router, &mut request, &mut client_writer, &mut state, false)
            .await;
        assert!(matches!(first, Err(SessionError::Backend(_))));
        assert_eq!(state.availability.missing_bits(), 0);
        assert_eq!(state.availability.missing_bits(), 0);
        assert_eq!(state.unavailable_backends.bits(), 0b0000_0001);

        let second = session
            .try_backend_for_article(&router, &mut request, &mut client_writer, &mut state, true)
            .await
            .expect("same-tier retry should continue after backend EOF");
        assert!(matches!(second, BackendAttemptResult::Success));
        assert_eq!(bad_article_commands.load(Ordering::SeqCst), 1);
        assert_eq!(good_article_commands.load(Ordering::SeqCst), 1);
        assert_eq!(request.backend_id(), Some(BackendId::from_index(1)));
        finalize_backend_connection(&mut backend_connection);
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
        .build();
        let backend_id = BackendId::from_index(0);
        let request = request_context(b"ARTICLE <large@example.com>\r\n");
        let msg_id = crate::types::MessageId::new("<large@example.com>".to_string()).unwrap();

        session.apply_cache_action(
            crate::session::routing::CacheAction::CaptureArticle,
            ResponseWriteParams {
                request: &request,
                article_request: crate::command::CommandHandler::article_lookup_request(&request),
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
        let (mut client_writer, _client_read, _client_write) = client_writer_pair().await;

        let mut request = request_context(b"BODY <bad-response@example.com>\r\n");
        let mut availability = ArticleAvailability::new();
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_connection = None;
        let mut unavailable_backends = SuppressedBackends::empty();
        let mut state = ArticleAttemptState {
            request_origin: ArticleRequestOrigin::Frontend,
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
            backlogged: &mut BackloggedBackends::default(),
            budget: crate::pool::admission::ArticleAdmission::default().start_request(),
        };

        for (index, expected_mask) in [0b0000_0001, 0b0000_0011].into_iter().enumerate() {
            let result = session
                .try_backend_for_article(
                    &router,
                    &mut request,
                    &mut client_writer,
                    &mut state,
                    index > 0,
                )
                .await
                .expect("invalid response should be handled");
            assert!(matches!(result, BackendAttemptResult::BackendUnavailable));
            assert_eq!(state.unavailable_backends.bits(), expected_mask);
        }

        let exhausted = session
            .try_backend_for_article(&router, &mut request, &mut client_writer, &mut state, true)
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
        finalize_backend_connection(&mut backend_connection);
    }

    /// Route one BODY request through the full retry loop against a tier-0 pair
    /// plus a tier-1 backup, returning the first `reply_len` client-visible
    /// bytes and how many article commands the backup received.
    async fn route_with_unavailable_tier_peer(
        primary_reply: &'static [u8],
        reply_len: usize,
    ) -> (Vec<u8>, usize) {
        route_with_unavailable_tier_peer_and_admission(
            primary_reply,
            reply_len,
            crate::pool::admission::ArticleAdmission::default(),
        )
        .await
    }

    async fn route_with_unavailable_tier_peer_and_admission(
        primary_reply: &'static [u8],
        reply_len: usize,
        admission: crate::pool::admission::ArticleAdmission,
    ) -> (Vec<u8>, usize) {
        route_with_tier_peer_reply(primary_reply, b"bad reply\r\n", reply_len, admission).await
    }

    async fn route_with_tier_peer_reply(
        primary_reply: &'static [u8],
        peer_reply: &'static [u8],
        reply_len: usize,
        admission: crate::pool::admission::ArticleAdmission,
    ) -> (Vec<u8>, usize) {
        let session = test_session();
        let (primary_port, _) = spawn_article_response_server(primary_reply).await;
        let (bad_port, _) = spawn_article_response_server(peer_reply).await;
        let (backup_port, backup_article_commands) =
            spawn_article_response_server(b"222 0 <backup@example.com>\r\npayload\r\n.\r\n").await;
        let router = router_with_tiered_backends_and_admission(
            [primary_port, bad_port, backup_port].map(|port| {
                let tier = u8::from(port == backup_port);
                (
                    DeadpoolConnectionProvider::builder("127.0.0.1", port)
                        .max_connections(1)
                        .build()
                        .unwrap(),
                    tier,
                )
            }),
            admission,
        );
        let (mut client_writer, mut client_read, _client_write) = client_writer_pair().await;
        let mut request = request_context(b"BODY <tier-peer@example.com>\r\n");
        let mut backend_connection = None;
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_to_client_bytes = crate::types::BackendToClientBytes::zero();

        tokio::time::timeout(
            Duration::from_secs(5),
            session.route_and_execute_request(
                router,
                &mut request,
                &mut client_writer,
                &mut backend_connection,
                &mut client_to_backend_bytes,
                &mut backend_to_client_bytes,
            ),
        )
        .await
        .expect("retry loop should finish")
        .expect("retry loop should answer the client");
        finalize_backend_connection(&mut backend_connection);
        drop(client_writer);

        let mut reply = vec![0; reply_len];
        client_read.read_exact(&mut reply).await.unwrap();
        (reply, backup_article_commands.load(Ordering::SeqCst))
    }

    /// The STAT sweep's false 430s: one tier-0 backend answered 430 while a
    /// tier-0 peer refused with `451`. The refusal says nothing about the
    /// article, so neither it nor a 430 may reach the client.
    #[tokio::test]
    async fn real_430_with_refusing_tier_peer_answers_503_not_430_or_the_refusal() {
        let (reply, backup_article_commands) = route_with_tier_peer_reply(
            b"430 No such article\r\n",
            b"451 0 <tier-peer@example.com>\r\n",
            crate::protocol::BACKEND_ERROR.len(),
            crate::pool::admission::ArticleAdmission::default(),
        )
        .await;

        assert_eq!(
            reply,
            crate::protocol::BACKEND_ERROR,
            "a refusing peer never said the article is missing"
        );
        assert_eq!(
            backup_article_commands, 0,
            "a refusing tier-0 peer must still block escalation to tier 1"
        );
    }

    #[tokio::test]
    async fn real_430_with_only_unavailable_tier_peers_answers_503() {
        let (reply, backup_article_commands) = route_with_unavailable_tier_peer(
            b"430 No such article\r\n",
            crate::protocol::BACKEND_ERROR.len(),
        )
        .await;

        assert_eq!(
            reply,
            crate::protocol::BACKEND_ERROR,
            "one 430 does not make the article missing while a peer never answered"
        );
        assert_eq!(
            backup_article_commands, 0,
            "an unavailable tier-0 peer must still block escalation to tier 1"
        );
    }

    #[tokio::test]
    async fn configured_430_unavailable_reply_applies_after_a_partial_430() {
        let (reply, backup_article_commands) = route_with_unavailable_tier_peer_and_admission(
            b"430 No such article\r\n",
            crate::protocol::NO_SUCH_ARTICLE.len(),
            crate::pool::admission::ArticleAdmission::default()
                .with_unavailable_reply(crate::pool::admission::UnavailableReply::NoSuchArticle),
        )
        .await;

        assert_eq!(
            reply,
            crate::protocol::NO_SUCH_ARTICLE,
            "an explicit unavailable_reply = \"430\" is honoured"
        );
        assert_eq!(backup_article_commands, 0);
    }

    #[tokio::test]
    async fn no_430_with_only_unavailable_tier_peers_still_answers_503() {
        let (reply, backup_article_commands) = route_with_unavailable_tier_peer(
            b"bad primary reply\r\n",
            crate::protocol::BACKEND_ERROR.len(),
        )
        .await;

        assert_eq!(reply, crate::protocol::BACKEND_ERROR);
        assert_eq!(backup_article_commands, 0);
    }

    #[test]
    fn article_error_reply_covers_non_430_errors_on_message_id_lookups() {
        let body = request_context(b"BODY <id@example.com>\r\n");
        let classify = |request: &RequestContext, code| {
            ArticleErrorReply::classify(request, StatusCode::new(code))
        };

        for code in [403, 411, 412, 423, 451, 500, 501, 503] {
            assert_eq!(
                classify(&body, code),
                Some(ArticleErrorReply::SessionUsable),
                "{code} should fail over and keep the session"
            );
        }
        for code in [400, 480, 481, 482, 483, 502] {
            assert_eq!(
                classify(&body, code),
                Some(ArticleErrorReply::SessionUnusable),
                "{code} should fail over and retire the session"
            );
        }
        for code in [220, 221, 222, 223, 111, 381, 430] {
            assert_eq!(classify(&body, code), None, "{code} is not a refusal");
        }
        for line in [
            b"ARTICLE <id@example.com>\r\n".as_slice(),
            b"HEAD <id@example.com>\r\n",
            b"STAT <id@example.com>\r\n",
        ] {
            assert_eq!(
                classify(&request_context(line), 451),
                Some(ArticleErrorReply::SessionUsable)
            );
        }
        // Article numbers depend on the selected group, which differs per
        // backend, so a refusal there is the client's answer.
        assert_eq!(classify(&request_context(b"BODY 123\r\n"), 451), None);
        assert_eq!(classify(&request_context(b"GROUP alt.test\r\n"), 451), None);
    }

    #[tokio::test]
    async fn non_430_error_reply_retries_next_same_tier_without_recording_missing() {
        let session = test_session_with_backend_metrics(2);
        let (refusing_port, refusing_commands) =
            spawn_article_response_server(b"451 0 <found@example.com>\r\n").await;
        let good_response = b"222 0 <found@example.com> body follows\r\npayload\r\n.\r\n";
        let (good_port, good_commands) = spawn_article_response_server(good_response).await;
        let router = router_with_tiered_backends([
            (
                DeadpoolConnectionProvider::builder("127.0.0.1", refusing_port)
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
        let (mut client_writer, mut client_read, _client_write) = client_writer_pair().await;

        let mut request = request_context(b"BODY <found@example.com>\r\n");
        let mut availability = ArticleAvailability::new();
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_connection = None;
        let mut unavailable_backends = SuppressedBackends::empty();
        let mut state = ArticleAttemptState {
            request_origin: ArticleRequestOrigin::Frontend,
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
            backlogged: &mut BackloggedBackends::default(),
            budget: crate::pool::admission::ArticleAdmission::default().start_request(),
        };

        let first = session
            .try_backend_for_article(&router, &mut request, &mut client_writer, &mut state, false)
            .await
            .expect("451 reply should be handled");
        assert!(matches!(first, BackendAttemptResult::BackendUnavailable));
        assert_eq!(state.availability.missing_bits(), 0);
        assert_eq!(state.unavailable_backends.bits(), 0b0000_0001);
        assert_eq!(
            state
                .backend_connection
                .as_ref()
                .map(|lease| lease.backend_id),
            Some(BackendId::from_index(0)),
            "a clean refusal leaves the connection reusable"
        );
        assert_eq!(
            session
                .metrics
                .error_reply_failovers(BackendId::from_index(0)),
            1
        );

        let second = session
            .try_backend_for_article(&router, &mut request, &mut client_writer, &mut state, true)
            .await
            .expect("same-tier retry should succeed");
        assert!(matches!(second, BackendAttemptResult::Success));
        assert_eq!(refusing_commands.load(Ordering::SeqCst), 1);
        assert_eq!(good_commands.load(Ordering::SeqCst), 1);
        assert_eq!(availability.missing_bits(), 0);
        assert_eq!(request.backend_id(), Some(BackendId::from_index(1)));
        finalize_backend_connection(&mut backend_connection);

        let mut written = vec![0; good_response.len()];
        tokio::time::timeout(Duration::from_secs(1), client_read.read_exact(&mut written))
            .await
            .expect("client response should be readable")
            .expect("client read should succeed");
        assert_eq!(
            written, good_response,
            "the 451 must not reach the client ahead of the article"
        );
    }

    #[tokio::test]
    async fn auth_error_reply_retires_connection_and_suppresses_backend() {
        let session = test_session();
        let (port, article_commands) =
            spawn_article_response_server(b"480 Authentication required\r\n").await;
        let provider = DeadpoolConnectionProvider::builder("127.0.0.1", port)
            .max_connections(1)
            .build()
            .unwrap();
        let router = router_with_backend(provider.clone());
        let (mut client_writer, _client_read, _client_write) = client_writer_pair().await;

        let mut request = request_context(b"BODY <auth@example.com>\r\n");
        let mut availability = ArticleAvailability::new();
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_connection = None;
        let mut unavailable_backends = SuppressedBackends::empty();
        let mut state = ArticleAttemptState {
            request_origin: ArticleRequestOrigin::Frontend,
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
            backlogged: &mut BackloggedBackends::default(),
            budget: crate::pool::admission::ArticleAdmission::default().start_request(),
        };

        let result = session
            .try_backend_for_article(&router, &mut request, &mut client_writer, &mut state, false)
            .await
            .expect("480 reply should be handled");
        assert!(matches!(result, BackendAttemptResult::BackendUnavailable));
        assert_eq!(article_commands.load(Ordering::SeqCst), 1);
        assert_eq!(availability.missing_bits(), 0);
        assert_eq!(unavailable_backends.bits(), 0b0000_0001);
        assert!(
            backend_connection.is_none(),
            "a session that lost authorization must not be kept for the batch"
        );
        assert_eq!(
            provider.status_counts().size,
            0,
            "a session that lost authorization must not return to the pool"
        );
    }

    #[tokio::test]
    async fn non_430_error_replies_do_not_escalate_to_backup_tier() {
        let session = test_session();
        let (refusing0_port, refusing0_commands) =
            spawn_article_response_server(b"451 0 <refused@example.com>\r\n").await;
        let (refusing1_port, refusing1_commands) =
            spawn_article_response_server(b"503 Program fault\r\n").await;
        let (backup_port, backup_commands) =
            spawn_article_response_server(b"222 0 <refused@example.com>\r\npayload\r\n.\r\n").await;
        let router = router_with_tiered_backends([
            (
                DeadpoolConnectionProvider::builder("127.0.0.1", refusing0_port)
                    .max_connections(1)
                    .build()
                    .unwrap(),
                0,
            ),
            (
                DeadpoolConnectionProvider::builder("127.0.0.1", refusing1_port)
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
        let (mut client_writer, _client_read, _client_write) = client_writer_pair().await;

        let mut request = request_context(b"BODY <refused@example.com>\r\n");
        let mut availability = ArticleAvailability::new();
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_connection = None;
        let mut unavailable_backends = SuppressedBackends::empty();
        let mut state = ArticleAttemptState {
            request_origin: ArticleRequestOrigin::Frontend,
            availability: &mut availability,
            client_to_backend_bytes: &mut client_to_backend_bytes,
            backend_connection: &mut backend_connection,
            unavailable_backends: &mut unavailable_backends,
            backlogged: &mut BackloggedBackends::default(),
            budget: crate::pool::admission::ArticleAdmission::default().start_request(),
        };

        for (index, expected_mask) in [0b0000_0001, 0b0000_0011].into_iter().enumerate() {
            let result = session
                .try_backend_for_article(
                    &router,
                    &mut request,
                    &mut client_writer,
                    &mut state,
                    index > 0,
                )
                .await
                .expect("error reply should be handled");
            assert!(matches!(result, BackendAttemptResult::BackendUnavailable));
            assert_eq!(state.unavailable_backends.bits(), expected_mask);
            finalize_backend_connection(state.backend_connection);
        }

        let exhausted = session
            .try_backend_for_article(&router, &mut request, &mut client_writer, &mut state, true)
            .await
            .expect("tier exhaustion should be reported without transport failure");
        assert!(matches!(
            exhausted,
            BackendAttemptResult::NoRetryableBackend
        ));
        assert_eq!(refusing0_commands.load(Ordering::SeqCst), 1);
        assert_eq!(refusing1_commands.load(Ordering::SeqCst), 1);
        assert_eq!(
            backup_commands.load(Ordering::SeqCst),
            0,
            "a refusal is not a 430, so the backup tier stays closed"
        );
        assert_eq!(availability.missing_bits(), 0);
    }

    #[tokio::test]
    async fn flush_disconnect_after_owned_response_reuses_backend_connection() {
        let session = test_session();
        let (port, accept_count) = spawn_greeting_server().await;
        let provider = DeadpoolConnectionProvider::builder("127.0.0.1", port)
            .max_connections(5)
            .build()
            .unwrap();
        let conn = provider
            .checkout_connection_guard()
            .await
            .unwrap()
            .activate();
        assert_eq!(accept_count.load(Ordering::SeqCst), 1);

        let guard = conn;
        let request = request_context(b"STAT <test@example.com>\r\n");
        let mut client_write = FlushFailWriter::new();
        let mut backend_bytes = BufferPool::new(BufferSize::try_new(8192).unwrap(), 1).acquire();
        backend_bytes.copy_from_slice(b"223 0 <test@example.com> status\r\n");

        let backend_bytes = crate::session::backend::ClassifiedResponse::read_for_test(
            &mut tokio::io::empty(),
            &request,
            backend_bytes,
        )
        .await
        .unwrap();
        let attempt = PreparedBackendAttempt::for_test(
            guard,
            backend_bytes,
            &session.buffer_pool,
            BackendId::from_index(0),
        );
        let err = session
            .write_successful_backend_response(
                attempt,
                &mut client_write,
                ResponseWriteContext {
                    request_origin: ArticleRequestOrigin::Frontend,
                    request: &request,
                    article_request: None,
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

        let reused = provider.checkout_connection_guard().await.unwrap();
        reused.release();
        assert_eq!(
            accept_count.load(Ordering::SeqCst),
            1,
            "client-side flush failure after a clean owned response must not retire the backend connection"
        );
    }

    #[tokio::test]
    #[should_panic(expected = "cannot release a backend exchange before completing its response")]
    async fn dropping_a_receiving_response_cannot_claim_successful_completion() {
        let (port, _) = spawn_greeting_server().await;
        let provider = DeadpoolConnectionProvider::builder("127.0.0.1", port)
            .max_connections(5)
            .build()
            .unwrap();
        let conn = provider
            .checkout_connection_guard()
            .await
            .unwrap()
            .activate();
        let request = request_context(b"STAT <test@example.com>\r\n");
        let mut backend_bytes = BufferPool::new(BufferSize::try_new(8192).unwrap(), 1).acquire();
        backend_bytes.copy_from_slice(b"223 0 <test@example.com> status\r\n");
        let response = crate::session::backend::ClassifiedResponse::read_for_test(
            &mut tokio::io::empty(),
            &request,
            backend_bytes,
        )
        .await
        .unwrap();
        let pool = BufferPool::new(BufferSize::try_new(8192).unwrap(), 1);
        let mut exchange = crate::session::backend::BackendResponseExchange::new_for_test(
            conn,
            response,
            &pool,
            BackendId::from_index(0),
        );

        drop(exchange.receiving().unwrap());
        let _ = exchange.into_connection();
    }

    #[tokio::test]
    async fn write_disconnect_after_complete_retained_response_reuses_backend_connection() {
        let session = test_session();
        let (port, accept_count) = spawn_greeting_server().await;
        let provider = DeadpoolConnectionProvider::builder("127.0.0.1", port)
            .max_connections(5)
            .build()
            .unwrap();
        let conn = provider
            .checkout_connection_guard()
            .await
            .unwrap()
            .activate();
        assert_eq!(accept_count.load(Ordering::SeqCst), 1);

        let guard = conn;
        let request = request_context(b"ARTICLE <test@example.com>\r\n");
        let mut client_write = WriteFailWriter;
        let mut backend_bytes = BufferPool::new(BufferSize::try_new(8192).unwrap(), 1).acquire();
        backend_bytes.copy_from_slice(b"220 Article follows\r\nbody\r\n.\r\n");

        let backend_bytes = crate::session::backend::ClassifiedResponse::read_for_test(
            &mut tokio::io::empty(),
            &request,
            backend_bytes,
        )
        .await
        .unwrap();
        let attempt = PreparedBackendAttempt::for_test(
            guard,
            backend_bytes,
            &session.buffer_pool,
            BackendId::from_index(0),
        );
        let err = session
            .write_successful_backend_response(
                attempt,
                &mut client_write,
                ResponseWriteContext {
                    request_origin: ArticleRequestOrigin::Frontend,
                    request: &request,
                    article_request: crate::command::CommandHandler::article_lookup_request(
                        &request,
                    ),
                },
                None,
            )
            .await
            .expect_err("client write should fail after the framer completed the response");

        assert!(matches!(
            err,
            crate::session::SessionError::ClientDisconnect(_)
        ));

        let reused = provider.checkout_connection_guard().await.unwrap();
        reused.release();
        assert_eq!(
            accept_count.load(Ordering::SeqCst),
            1,
            "client write failure after a complete framed response must not retire the backend connection"
        );
    }

    #[tokio::test]
    async fn write_disconnect_with_packed_trailing_bytes_retires_backend_connection() {
        let session = test_session();
        let (port, accept_count) = spawn_greeting_server().await;
        let provider = DeadpoolConnectionProvider::builder("127.0.0.1", port)
            .max_connections(5)
            .build()
            .unwrap();
        let conn = provider
            .checkout_connection_guard()
            .await
            .unwrap()
            .activate();
        assert_eq!(accept_count.load(Ordering::SeqCst), 1);

        let guard = conn;
        let request = request_context(b"ARTICLE <test@example.com>\r\n");
        let mut client_write = WriteFailWriter;
        let mut backend_bytes = BufferPool::new(BufferSize::try_new(8192).unwrap(), 1).acquire();
        backend_bytes.copy_from_slice(
            b"220 Article follows\r\nbody\r\n.\r\n223 0 <next@example.com> status\r\n",
        );

        let backend_bytes = crate::session::backend::ClassifiedResponse::read_for_test(
            &mut tokio::io::empty(),
            &request,
            backend_bytes,
        )
        .await
        .unwrap();
        let attempt = PreparedBackendAttempt::for_test(
            guard,
            backend_bytes,
            &session.buffer_pool,
            BackendId::from_index(0),
        );
        let err = session
            .write_successful_backend_response(
                attempt,
                &mut client_write,
                ResponseWriteContext {
                    request_origin: ArticleRequestOrigin::Frontend,
                    request: &request,
                    article_request: None,
                },
                None,
            )
            .await
            .expect_err("packed trailing bytes plus client write failure must be terminal");

        assert!(matches!(
            err,
            crate::session::SessionError::ClientDisconnect(_)
        ));

        let replacement = provider.checkout_connection_guard().await.unwrap();
        replacement.release();
        assert_eq!(
            accept_count.load(Ordering::SeqCst),
            2,
            "client write failure with queued backend bytes must retire the connection"
        );
    }
}
