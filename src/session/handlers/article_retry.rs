//! Article routing with availability-aware backend selection
//!
//! Handles routing article commands across backends, using `ArticleAvailability`
//! to skip backends that have already returned 430 for a given article.

use crate::cache::ArticleAvailability;
use crate::router::backend_queue::QueuedContext;
use crate::router::{BackendSelector, CommandGuard};
use crate::session::ClientSession;
use crate::session::SessionError;
use crate::session::handlers::cache_operations::{
    CacheLookupResult, write_cached_article_response,
};
use crate::session::handlers::command_execution::{ArticleAttemptState, BackendAttemptResult};
use crate::session::routing::{CacheAction, determine_cache_action_for_request};
use crate::types::{BackendToClientBytes, ClientToBackendBytes};
use anyhow::Result;
use std::io;
use std::sync::Arc;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tracing::{debug, warn};

use crate::protocol::RequestContext;
use crate::session::precheck;

/// Client-side write state shared across cache, precheck, pipeline, and direct routing paths.
struct RequestExecutionIo<'a> {
    client_writer: &'a crate::session::SharedClientWriter,
    client_to_backend_bytes: &'a mut ClientToBackendBytes,
    backend_to_client_bytes: &'a mut BackendToClientBytes,
}

/// Result of preparing a request before pipeline/direct backend execution.
enum PreparedRequest {
    /// The response was already written to the client.
    Served,
    /// Continue with backend routing using resolved availability state.
    Continue {
        availability: Option<ArticleAvailability>,
    },
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
        client_to_backend_bytes: &mut ClientToBackendBytes,
        backend_to_client_bytes: &mut BackendToClientBytes,
    ) -> Result<(), SessionError> {
        self.log_route_request(request);

        let mut io = RequestExecutionIo {
            client_writer,
            client_to_backend_bytes,
            backend_to_client_bytes,
        };
        let mut availability = match self
            .prepare_request_execution(&router, request, &mut io)
            .await?
        {
            PreparedRequest::Served => return Ok(()),
            PreparedRequest::Continue { availability } => availability,
        };

        if self
            .try_pipeline_request(&router, request, &mut io, &mut availability)
            .await?
        {
            return Ok(());
        }

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

    async fn prepare_request_execution(
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

    async fn try_pipeline_request(
        &self,
        router: &Arc<BackendSelector>,
        request: &RequestContext,
        io: &mut RequestExecutionIo<'_>,
        availability: &mut Option<ArticleAvailability>,
    ) -> Result<bool, SessionError> {
        if let Ok(backend_id) =
            router.route_with_availability(self.client_id, availability.as_ref())
            && let Some(queue) = router.get_backend_queue(backend_id)
        {
            let guard = CommandGuard::new(router.clone(), backend_id);
            debug!(
                "Client {} using pipeline path for backend {:?}: kind={:?}, verb={:?}",
                self.client_addr,
                backend_id,
                request.kind(),
                request.verb()
            );

            let (tx, rx) = tokio::sync::oneshot::channel();
            let queued_context = if request.is_large_transfer() {
                let delivery = if self.article_buffer {
                    crate::router::backend_queue::PipelineDelivery::Buffered
                } else if self.cache_articles {
                    crate::router::backend_queue::PipelineDelivery::StreamAndCapture(
                        io.client_writer.clone(),
                    )
                } else {
                    crate::router::backend_queue::PipelineDelivery::StreamToClient(
                        io.client_writer.clone(),
                    )
                };
                QueuedContext::new(request.clone(), self.client_addr, tx, delivery)
            } else {
                QueuedContext::new(
                    request.clone(),
                    self.client_addr,
                    tx,
                    crate::router::backend_queue::PipelineDelivery::Buffered,
                )
            };

            match queue.try_enqueue(queued_context) {
                Ok(()) => {
                    self.metrics.record_pipeline_enqueue();

                    match rx.await {
                        Ok(Ok(mut completed))
                            if completed
                                .context
                                .response_metadata()
                                .is_some_and(|response| response.status().as_u16() != 430) =>
                        {
                            let completed_backend_id = completed
                                .context
                                .backend_id()
                                .expect("completed queued request records backend id");
                            let response = completed
                                .context
                                .response_metadata()
                                .expect("completed queued request records response metadata");
                            let cache_action = determine_cache_action_for_request(
                                &completed.context,
                                response.status(),
                                self.cache_articles,
                                completed.context.has_message_id(),
                            );
                            if let Some(payload) = completed.context.take_response_payload() {
                                if !completed.response_streamed {
                                    let mut client_write = io.client_writer.lock().await;
                                    payload
                                        .write_all_to(&mut *client_write)
                                        .await
                                        .map_err(|e| SessionError::from(anyhow::Error::from(e)))?;
                                }

                                if matches!(cache_action, CacheAction::CaptureArticle)
                                    && let Some(msg_id) = completed.context.message_id_value()
                                {
                                    let tier = self.tier_for_backend(completed_backend_id);
                                    self.spawn_cache_upsert_buffer(
                                        &msg_id,
                                        payload.into(),
                                        completed_backend_id,
                                        tier,
                                    );
                                }
                            }
                            if matches!(cache_action, CacheAction::TrackAvailability)
                                && let Some(msg_id) = completed.context.message_id_value()
                                && !completed
                                    .context
                                    .cache_records_backend_has_article(completed_backend_id)
                            {
                                self.spawn_cache_upsert_availability(
                                    &msg_id,
                                    response.status(),
                                    completed_backend_id,
                                    self.tier_for_backend(completed_backend_id),
                                );
                            }
                            *io.backend_to_client_bytes =
                                io.backend_to_client_bytes.add(response.wire_len().get());
                            *io.client_to_backend_bytes = io
                                .client_to_backend_bytes
                                .add(completed.context.request_wire_len().get());
                            self.metrics.record_pipeline_complete();
                            guard.complete();
                            return Ok(true);
                        }
                        Ok(Ok(completed)) => {
                            let completed_backend_id = completed
                                .context
                                .backend_id()
                                .expect("completed queued request records backend id");
                            debug!(
                                "Client {} pipeline got 430 from backend {:?}, falling through to retry loop",
                                self.client_addr, completed_backend_id
                            );
                            self.handle_430_availability(
                                completed_backend_id,
                                availability.get_or_insert_default(),
                            );
                            self.metrics.record_pipeline_complete();
                        }
                        Ok(Err(crate::router::backend_queue::PipelineError::ClientDisconnect)) => {
                            return Err(SessionError::ClientDisconnect(io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                "client disconnected during pipelined stream",
                            )));
                        }
                        Ok(Err(e)) => {
                            debug!(
                                "Client {} pipeline error for backend {:?}: {}",
                                self.client_addr, backend_id, e
                            );
                        }
                        Err(_) => {
                            debug!(
                                "Client {} pipeline worker dropped response channel",
                                self.client_addr
                            );
                        }
                    }
                }
                Err(e) => {
                    debug!(
                        "Client {} pipeline queue full for backend {:?}: {}",
                        self.client_addr, backend_id, e
                    );
                }
            }
        }

        Ok(false)
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

        let mut buffer = self.buffer_pool.acquire();
        let mut availability = availability.unwrap_or_default();
        debug!(
            "Client {} availability routing: missing_bits={:08b}, backend_count={}",
            self.client_addr,
            availability.missing_bits(),
            router.backend_count().get()
        );

        while !availability.all_exhausted(router.backend_count()) {
            let attempt = {
                let mut client_write = io.client_writer.lock().await;
                self.try_backend_for_article(
                    router,
                    request,
                    &mut *client_write,
                    &mut ArticleAttemptState {
                        availability: &mut availability,
                        buffer: &mut buffer,
                        client_to_backend_bytes: io.client_to_backend_bytes,
                    },
                )
                .await
            };
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
                Err(e @ SessionError::ClientDisconnect(_)) => {
                    debug!(
                        "Client {} disconnected during article retry for {:?}",
                        self.client_addr,
                        request.message_id_value()
                    );
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
    pub(super) fn handle_430_availability(
        &self,
        backend_id: crate::types::BackendId,
        availability: &mut crate::cache::ArticleAvailability,
    ) {
        availability.record_missing(backend_id);

        // Track 430 responses in 4xx metrics for visibility in TUI
        // While 430 is normal retry behavior (not a failure), users want to see
        // these counted to understand backend article distribution
        self.metrics.record_error_4xx(backend_id);
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
