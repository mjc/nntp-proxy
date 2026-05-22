//! Article routing with availability-aware backend selection
//!
//! Handles routing article commands across backends, using `ArticleAvailability`
//! to skip backends that have already returned 430 for a given article.

use crate::cache::ArticleAvailability;
use crate::router::BackendSelector;
use crate::session::ClientSession;
use crate::session::SessionError;
use crate::session::handlers::cache_operations::{
    CacheLookupResult, write_cached_article_response,
};
use crate::session::handlers::command_execution::{
    ArticleAttemptState, AuthoritativeArticleMissing, BackendAttemptResult, ResponseWriteParams,
    RetryAttemptKind,
};
use crate::types::{BackendToClientBytes, ClientToBackendBytes};
use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::Notify;
use tracing::{debug, warn};

use crate::protocol::RequestContext;
use crate::session::precheck;

/// Client-side write state shared across cache, precheck, and direct routing paths.
pub(super) struct RequestExecutionIo<'a> {
    pub(super) client_writer: &'a crate::session::SharedClientWriter,
    pub(super) backend_connection:
        &'a mut Option<(crate::types::BackendId, crate::pool::ConnectionGuard)>,
    pub(super) client_to_backend_bytes: &'a mut ClientToBackendBytes,
    pub(super) backend_to_client_bytes: &'a mut BackendToClientBytes,
}

/// Result of preparing a request before pipeline/direct backend execution.
pub(super) enum PreparedRequest {
    /// The response was already written to the client.
    Served,
    /// Continue with backend routing using resolved availability state.
    Continue {
        availability: Option<ArticleAvailability>,
    },
}

pub(super) struct OrderedPipelineGate {
    next: AtomicUsize,
    failed: AtomicBool,
    notify: Notify,
}

impl OrderedPipelineGate {
    pub(super) fn new() -> Self {
        Self {
            next: AtomicUsize::new(0),
            failed: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    async fn wait_turn(&self, index: usize) -> OrderedPipelineTurn<'_> {
        loop {
            if self.next.load(Ordering::Acquire) == index {
                return OrderedPipelineTurn {
                    gate: self,
                    advanced: false,
                };
            }

            let notified = self.notify.notified();
            if self.next.load(Ordering::Acquire) == index {
                return OrderedPipelineTurn {
                    gate: self,
                    advanced: false,
                };
            }
            notified.await;
        }
    }

    fn advance(&self) {
        self.next.fetch_add(1, Ordering::AcqRel);
        self.notify.notify_waiters();
    }

    fn is_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    fn fail(&self) {
        self.failed.store(true, Ordering::Release);
    }
}

struct OrderedPipelineTurn<'a> {
    gate: &'a OrderedPipelineGate,
    advanced: bool,
}

impl OrderedPipelineTurn<'_> {
    fn advance(mut self) {
        self.advanced = true;
        self.gate.advance();
    }

    fn fail_and_advance(mut self) {
        self.advanced = true;
        self.gate.fail();
        self.gate.advance();
    }
}

impl Drop for OrderedPipelineTurn<'_> {
    fn drop(&mut self) {
        if !self.advanced {
            self.gate.advance();
        }
    }
}

pub(super) struct OrderedLargeTransferResult {
    pub(super) additional_client_to_backend_bytes: ClientToBackendBytes,
    pub(super) backend_to_client_bytes: BackendToClientBytes,
}

impl ClientSession {
    /// Route a single request to a backend and execute it
    ///
    /// This function is `pub(super)` to allow reuse of per-command routing logic by sibling handler modules
    /// (such as `hybrid.rs`) that also need to route commands.
    pub(super) async fn route_and_execute_request(
        &self,
        router: Arc<BackendSelector>,
        request: &mut RequestContext,
        client_writer: &crate::session::SharedClientWriter,
        backend_connection: &mut Option<(crate::types::BackendId, crate::pool::ConnectionGuard)>,
        client_to_backend_bytes: &mut ClientToBackendBytes,
        backend_to_client_bytes: &mut BackendToClientBytes,
    ) -> Result<(), SessionError> {
        self.log_route_request(request);

        let mut io = RequestExecutionIo {
            client_writer,
            backend_connection,
            client_to_backend_bytes,
            backend_to_client_bytes,
        };
        let availability = match self
            .prepare_request_execution(&router, request, &mut io)
            .await?
        {
            PreparedRequest::Served => return Ok(()),
            PreparedRequest::Continue { availability } => availability,
        };

        self.execute_article_retry_loop(&router, request, availability, &mut io)
            .await
    }

    fn log_route_request(&self, request: &RequestContext) {
        debug!(
            "Client {} ENTERED route_and_execute_request: kind={:?}, verb={:?}",
            self.client_addr,
            request.kind(),
            request.verb()
        );
        debug!(
            "Client {} msg_id={:?}, cache_articles={}",
            self.client_addr,
            request.message_id(),
            self.cache_articles
        );
    }

    pub(super) async fn prepare_request_execution(
        &self,
        router: &Arc<BackendSelector>,
        request: &mut RequestContext,
        io: &mut RequestExecutionIo<'_>,
    ) -> Result<PreparedRequest, SessionError> {
        let availability = {
            let mut client_write = io.client_writer.lock().await;
            match self
                .try_serve_from_cache(
                    request,
                    router,
                    &mut *client_write,
                    io.backend_to_client_bytes,
                )
                .await?
            {
                CacheLookupResult::Hit => return Ok(PreparedRequest::Served),
                CacheLookupResult::PartialHit => request
                    .cache_availability()
                    .map(Self::request_cache_availability),
                CacheLookupResult::Miss => None,
            }
        };
        let availability = match availability {
            Some(availability) => Some(availability),
            None if request.message_id_value().is_some() => Some(
                self.load_article_availability(
                    request.message_id_value().as_ref(),
                    router.backend_count(),
                )
                .await,
            ),
            None => None,
        };
        if self.try_adaptive_precheck(router, request, io).await? {
            return Ok(PreparedRequest::Served);
        }

        Ok(PreparedRequest::Continue { availability })
    }

    const fn request_cache_availability(
        availability: crate::protocol::RequestCacheAvailability,
    ) -> ArticleAvailability {
        ArticleAvailability::from_bits(availability.checked_bits(), availability.missing_bits())
    }

    async fn try_adaptive_precheck(
        &self,
        router: &Arc<BackendSelector>,
        request: &RequestContext,
        io: &mut RequestExecutionIo<'_>,
    ) -> Result<bool, SessionError> {
        if !self.adaptive_precheck || !(request.is_stat() || request.is_head()) {
            return Ok(false);
        }
        let Some(msg_id) = request.message_id_value() else {
            return Ok(false);
        };

        let deps = self.precheck_deps(router);
        let mut client_write = io.client_writer.lock().await;
        let bytes_written = if let Some(entry) = precheck::precheck(&deps, request, &msg_id).await {
            if let Some(write) = write_cached_article_response(
                &mut *client_write,
                &entry,
                request.kind(),
                msg_id.as_str(),
            )
            .await
            .map_err(|e| SessionError::from(anyhow::Error::from(e)))?
            {
                write.wire_len.get()
            } else {
                Self::write_no_such_article_response(&mut *client_write).await?
            }
        } else {
            Self::write_no_such_article_response(&mut *client_write).await?
        };
        *io.backend_to_client_bytes = io.backend_to_client_bytes.add(bytes_written);
        Ok(true)
    }

    async fn write_no_such_article_response<W>(client_write: &mut W) -> Result<usize, SessionError>
    where
        W: AsyncWrite + Unpin,
    {
        client_write
            .write_all(crate::protocol::NO_SUCH_ARTICLE)
            .await
            .map(|()| crate::protocol::NO_SUCH_ARTICLE.len())
            .map_err(|e| SessionError::from(anyhow::Error::from(e)))
    }

    async fn write_backend_error_response<W>(client_write: &mut W) -> Result<usize, SessionError>
    where
        W: AsyncWrite + Unpin,
    {
        client_write
            .write_all(crate::protocol::BACKEND_ERROR)
            .await
            .map(|()| crate::protocol::BACKEND_ERROR.len())
            .map_err(|e| SessionError::from(anyhow::Error::from(e)))
    }

    fn release_cached_backend_connection(
        backend_connection: &mut Option<(crate::types::BackendId, crate::pool::ConnectionGuard)>,
    ) {
        if let Some((_backend_id, conn)) = backend_connection.take() {
            let _ = conn.release();
        }
    }

    async fn execute_article_retry_loop(
        &self,
        router: &Arc<BackendSelector>,
        request: &mut RequestContext,
        availability: Option<ArticleAvailability>,
        io: &mut RequestExecutionIo<'_>,
    ) -> Result<(), SessionError> {
        debug!(
            "Client {} starting availability routing for request kind={:?}, verb={:?}",
            self.client_addr,
            request.kind(),
            request.verb()
        );

        let mut availability = availability.unwrap_or_default();
        let mut unavailable_backends = 0;
        debug!(
            "Client {} availability routing: missing_bits={:08b}, backend_count={}",
            self.client_addr,
            availability.missing_bits(),
            router.backend_count().get()
        );

        while !availability.all_exhausted(router.backend_count()) {
            let attempt = self
                .try_backend_for_article(
                    router,
                    request,
                    io.client_writer,
                    &mut ArticleAttemptState {
                        availability: &mut availability,
                        client_to_backend_bytes: io.client_to_backend_bytes,
                        backend_connection: io.backend_connection,
                        unavailable_backends: &mut unavailable_backends,
                    },
                )
                .await;
            match attempt {
                Ok(BackendAttemptResult::Success) => {
                    let response = request
                        .response_metadata()
                        .expect("successful direct attempt records response metadata");
                    *io.backend_to_client_bytes =
                        io.backend_to_client_bytes.add(response.wire_len().get());
                    let msg_id = request.message_id_value();
                    self.sync_availability_if_needed(msg_id.as_ref(), &availability)
                        .await;
                    return Ok(());
                }
                Ok(BackendAttemptResult::ArticleNotFound { backend_id }) => {
                    debug!(
                        "Client {} backend {:?} returned 430 during retry",
                        self.client_addr, backend_id
                    );
                }
                Ok(BackendAttemptResult::BackendUnavailable) => {}
                Ok(BackendAttemptResult::NoRetryableBackend) => {
                    let msg_id = request.message_id_value();
                    self.sync_availability_if_needed(msg_id.as_ref(), &availability)
                        .await;
                    let bytes_written = {
                        let mut client_write = io.client_writer.lock().await;
                        Self::write_backend_error_response(&mut *client_write).await?
                    };
                    *io.backend_to_client_bytes = io.backend_to_client_bytes.add(bytes_written);
                    return Ok(());
                }
                Err(e @ SessionError::ClientDisconnect(_)) => {
                    debug!(
                        "Client {} disconnected during article retry for {:?}",
                        self.client_addr,
                        request.message_id_value()
                    );
                    let msg_id = request.message_id_value();
                    self.sync_availability_if_needed(msg_id.as_ref(), &availability)
                        .await;
                    return Err(e);
                }
                Err(SessionError::Backend(e)) => {
                    warn!(
                        client = %self.client_addr,
                        error = %e,
                        "Backend error during article retry (will try next backend)"
                    );
                }
            }
        }

        debug!(
            "Client {} all backends exhausted for {:?}, sending 430",
            self.client_addr,
            request.message_id_value()
        );
        let msg_id = request.message_id_value();
        self.sync_availability_if_needed(msg_id.as_ref(), &availability)
            .await;
        {
            let mut client_write = io.client_writer.lock().await;
            self.send_430_to_client(&mut *client_write, io.backend_to_client_bytes)
                .await?;
        }

        Ok(())
    }

    pub(super) async fn execute_ordered_large_transfer_request(
        &self,
        router: Arc<BackendSelector>,
        request: &RequestContext,
        client_writer: crate::session::SharedClientWriter,
        order: Arc<OrderedPipelineGate>,
        order_index: usize,
    ) -> Result<OrderedLargeTransferResult, SessionError> {
        debug!(
            client = %self.client_addr,
            order_index,
            command_verb = ?request.verb(),
            msg_id = ?request.message_id_value(),
            "Starting ordered large-transfer pipeline request"
        );

        let msg_id = request.message_id_value();
        let mut availability = self
            .load_article_availability(msg_id.as_ref(), router.backend_count())
            .await;
        let mut backend_connection = None;
        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_to_client_bytes = BackendToClientBytes::zero();
        let mut unavailable_backends = 0;

        while !availability.all_exhausted(router.backend_count()) {
            let attempt = match self
                .prepare_ordered_large_transfer_attempt(
                    &router,
                    request,
                    &mut availability,
                    &mut client_to_backend_bytes,
                    &mut backend_connection,
                    &mut unavailable_backends,
                )
                .await
            {
                Ok(attempt) => attempt,
                Err(err) => {
                    Self::release_cached_backend_connection(&mut backend_connection);
                    let turn = order.wait_turn(order_index).await;
                    turn.fail_and_advance();
                    return Err(err);
                }
            };

            let Some((backend_id, guard, mut conn, status_code, buffer)) = attempt else {
                continue;
            };

            if let Some(missing) =
                AuthoritativeArticleMissing::from_status_code(backend_id, status_code)
            {
                self.record_authoritative_article_missing(missing, &mut availability);
                if let Some(msg_id) = request.message_id_value() {
                    self.cache.record_backend_missing(msg_id, backend_id).await;
                }
                if let Err(err) = self
                    .capture_suppressed_430_response(&mut conn, backend_id, request, buffer)
                    .await
                {
                    let turn = order.wait_turn(order_index).await;
                    turn.fail_and_advance();
                    return Err(err);
                }
                self.release_or_reuse_connection(
                    conn,
                    backend_id,
                    request,
                    Some(&mut backend_connection),
                );
                continue;
            }

            let msg_id = request.message_id_value();
            let response = {
                let turn = order.wait_turn(order_index).await;
                if order.is_failed() {
                    turn.advance();
                    return Err(SessionError::Backend(anyhow::anyhow!(
                        "ordered pipeline request aborted after earlier slot error"
                    )));
                }
                let mut client_write = client_writer.lock().await;
                let response = self
                    .write_successful_backend_response(
                        conn,
                        &mut *client_write,
                        backend_id,
                        buffer,
                        ResponseWriteParams {
                            request,
                            msg_id: msg_id.as_ref(),
                            status_code,
                        },
                        Some(&mut backend_connection),
                    )
                    .await;
                if response.is_err() {
                    turn.fail_and_advance();
                } else {
                    turn.advance();
                }
                response
            };

            let response = match response {
                Ok(response) => response,
                Err(e @ SessionError::ClientDisconnect(_)) => {
                    order.fail();
                    self.sync_availability_if_needed(msg_id.as_ref(), &availability)
                        .await;
                    return Err(e);
                }
                Err(e) => {
                    order.fail();
                    return Err(e);
                }
            };

            Self::record_successful_availability(backend_id, status_code, &mut availability);
            guard.complete();
            self.sync_availability_if_needed(msg_id.as_ref(), &availability)
                .await;
            Self::release_cached_backend_connection(&mut backend_connection);
            backend_to_client_bytes = backend_to_client_bytes.add(response.wire_len().get());
            return Ok(OrderedLargeTransferResult {
                additional_client_to_backend_bytes: client_to_backend_bytes.saturating_sub(
                    ClientToBackendBytes::zero().add(request.request_wire_len().get()),
                ),
                backend_to_client_bytes,
            });
        }

        let msg_id = request.message_id_value();
        self.sync_availability_if_needed(msg_id.as_ref(), &availability)
            .await;
        {
            let turn = order.wait_turn(order_index).await;
            if order.is_failed() {
                turn.advance();
                return Err(SessionError::Backend(anyhow::anyhow!(
                    "ordered pipeline request aborted after earlier slot error"
                )));
            }
            let mut client_write = client_writer.lock().await;
            let response = self
                .send_430_to_client(&mut *client_write, &mut backend_to_client_bytes)
                .await;
            if let Err(err) = response {
                turn.fail_and_advance();
                return Err(SessionError::from(err));
            }
            turn.advance();
        }

        Self::release_cached_backend_connection(&mut backend_connection);

        Ok(OrderedLargeTransferResult {
            additional_client_to_backend_bytes: client_to_backend_bytes
                .saturating_sub(ClientToBackendBytes::zero().add(request.request_wire_len().get())),
            backend_to_client_bytes,
        })
    }

    async fn prepare_ordered_large_transfer_attempt(
        &self,
        router: &Arc<BackendSelector>,
        request: &RequestContext,
        availability: &mut crate::cache::ArticleAvailability,
        client_to_backend_bytes: &mut ClientToBackendBytes,
        backend_connection: &mut Option<(crate::types::BackendId, crate::pool::ConnectionGuard)>,
        unavailable_backends: &mut u8,
    ) -> Result<
        Option<(
            crate::types::BackendId,
            crate::router::CommandGuard,
            crate::pool::ConnectionGuard,
            crate::protocol::StatusCode,
            crate::pool::PooledBuffer,
        )>,
        SessionError,
    > {
        let backend_id = match router.route_with_availability_suppressing(
            self.client_id,
            Some(availability),
            *unavailable_backends,
        ) {
            Ok(backend_id) => backend_id,
            Err(err) => {
                debug!(
                    client = %self.client_addr,
                    error = %err,
                    command_verb = ?request.verb(),
                    msg_id = ?request.message_id_value(),
                    "No eligible backend available for ordered pipeline attempt"
                );
                return Err(SessionError::Backend(anyhow::anyhow!(err)));
            }
        };
        let guard = crate::router::CommandGuard::new(router.clone(), backend_id);
        let Some(provider) = self.retry_backend_provider(
            router,
            backend_id,
            request,
            RetryAttemptKind::OrderedPipeline,
        ) else {
            *unavailable_backends |= backend_id.availability_bit();
            return Err(SessionError::Backend(anyhow::anyhow!(
                "selected backend had no provider: {:?}",
                backend_id
            )));
        };
        let mut state = ArticleAttemptState {
            availability,
            client_to_backend_bytes,
            backend_connection,
            unavailable_backends,
        };
        let prepared = self
            .prepare_backend_attempt(provider, backend_id, request, &mut state)
            .await;
        let Some((conn, status_code, buffer)) = (match prepared {
            Ok(prepared) => prepared,
            Err(SessionError::Backend(err)) => {
                *state.unavailable_backends |= backend_id.availability_bit();
                debug!(
                    client = %self.client_addr,
                    backend = backend_id.as_index(),
                    unavailable_backends = format_args!("{:08b}", *state.unavailable_backends),
                    command_verb = ?request.verb(),
                    msg_id = ?request.message_id_value(),
                    error = %err,
                    "Ordered pipeline backend attempt failed; trying next eligible backend"
                );
                return Ok(None);
            }
            Err(err) => return Err(err),
        }) else {
            return Ok(None);
        };

        Ok(Some((backend_id, guard, conn, status_code, buffer)))
    }

    /// Load article availability from cache or create fresh tracker
    pub(super) async fn load_article_availability(
        &self,
        msg_id: Option<&crate::types::MessageId<'_>>,
        backend_count: crate::router::BackendCount,
    ) -> crate::cache::ArticleAvailability {
        match msg_id {
            Some(msg_id_ref) => self
                .cache
                .get(msg_id_ref)
                .await
                .map(|entry| {
                    let avail = entry.to_availability(backend_count);
                    debug!("Client {} loaded availability for {}: checked_bits={:08b}, missing_bits={:08b}",
                        self.client_addr, msg_id_ref, avail.checked_bits(), avail.missing_bits());
                    avail
                })
                .unwrap_or_default(),
            None => crate::cache::ArticleAvailability::new(),
        }
    }

    /// Record 430 response in availability tracker.
    ///
    /// Note: `complete_command` is handled by [`crate::router::CommandGuard`] RAII, not here.
    pub(super) fn record_authoritative_article_missing(
        &self,
        missing: AuthoritativeArticleMissing,
        availability: &mut crate::cache::ArticleAvailability,
    ) {
        let backend_id = missing.backend_id();
        availability.record_missing(backend_id);

        // Track 430 responses in 4xx metrics for visibility in TUI
        // While 430 is normal retry behavior (not a failure), users want to see
        // these counted to understand backend article distribution
        self.metrics.record_error_4xx(backend_id);
    }

    pub(super) fn record_successful_availability(
        backend_id: crate::types::BackendId,
        status_code: crate::protocol::StatusCode,
        availability: &mut crate::cache::ArticleAvailability,
    ) {
        if status_code.is_success() {
            availability.record_has(backend_id);
        }
    }

    /// Sync availability to cache if a message ID is present.
    pub(super) async fn sync_availability_if_needed(
        &self,
        msg_id: Option<&crate::types::MessageId<'_>>,
        availability: &crate::cache::ArticleAvailability,
    ) {
        if availability.missing_bits() == 0 {
            return;
        }

        if let Some(msg_id_ref) = msg_id {
            self.cache
                .sync_availability(msg_id_ref.clone(), availability)
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::OrderedPipelineGate;
    use crate::cache::{ArticleAvailability, UnifiedCache};
    use crate::protocol::StatusCode;
    use crate::session::ClientSession;
    use crate::types::{BackendId, MessageId};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn ordered_pipeline_gate_releases_waiters_in_index_order() {
        let gate = Arc::new(OrderedPipelineGate::new());
        let (tx, mut rx) = mpsc::unbounded_channel();

        for index in [2, 0, 1] {
            let gate = gate.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                let turn = gate.wait_turn(index).await;
                tx.send(index).expect("receiver should remain open");
                turn.advance();
            });
        }
        drop(tx);

        assert_eq!(rx.recv().await, Some(0));
        assert_eq!(rx.recv().await, Some(1));
        assert_eq!(rx.recv().await, Some(2));
        assert_eq!(rx.recv().await, None);
    }

    #[tokio::test]
    async fn ordered_pipeline_gate_drop_advances_turn() {
        let gate = Arc::new(OrderedPipelineGate::new());
        let (tx, mut rx) = mpsc::unbounded_channel();

        let first = gate.wait_turn(0).await;
        let waiter = {
            let gate = gate.clone();
            tokio::spawn(async move {
                let turn = gate.wait_turn(1).await;
                tx.send(()).expect("receiver should remain open");
                turn.advance();
            })
        };

        drop(first);
        assert_eq!(rx.recv().await, Some(()));
        waiter.await.expect("waiter task should complete");
    }

    #[tokio::test]
    async fn ordered_pipeline_gate_failure_is_visible_to_later_turns() {
        let gate = Arc::new(OrderedPipelineGate::new());

        let first = gate.wait_turn(0).await;
        first.fail_and_advance();

        let second = gate.wait_turn(1).await;
        assert!(gate.is_failed());
        second.advance();
    }

    #[test]
    fn successful_availability_records_backend_has_article() {
        let mut availability = ArticleAvailability::new();
        let backend_id = BackendId::from_index(2);

        ClientSession::record_successful_availability(
            backend_id,
            StatusCode::new(222),
            &mut availability,
        );

        assert_eq!(availability.checked_bits(), 0b0000_0100);
        assert_eq!(availability.missing_bits(), 0);
        assert!(availability.any_backend_has_article());
    }

    #[test]
    fn non_success_availability_does_not_record_has_article() {
        let mut availability = ArticleAvailability::new();

        ClientSession::record_successful_availability(
            BackendId::from_index(1),
            StatusCode::new(430),
            &mut availability,
        );

        assert_eq!(availability.checked_bits(), 0);
        assert_eq!(availability.missing_bits(), 0);
        assert!(!availability.any_backend_has_article());
    }

    #[tokio::test]
    async fn successful_availability_prevents_sync_from_creating_missing_only_entry() {
        let cache = UnifiedCache::memory(100, Duration::from_secs(60));
        let msg_id = MessageId::from_borrowed("<ordered-success@example.com>").unwrap();
        let mut availability = ArticleAvailability::new();
        availability.record_missing(BackendId::from_index(0));

        ClientSession::record_successful_availability(
            BackendId::from_index(1),
            StatusCode::new(222),
            &mut availability,
        );
        cache.sync_availability(msg_id.clone(), &availability).await;
        cache.sync().await;

        assert!(
            cache.get(&msg_id).await.is_none(),
            "sync_availability must not create a missing-only entry after a retry success"
        );
    }
}
