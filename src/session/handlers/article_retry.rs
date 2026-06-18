//! Availability-aware routing for article lookup commands.
//!
//! ARTICLE, BODY, HEAD, and STAT requests with a message-id share the same
//! negative availability facts: a backend that returned 430 for that article is
//! skipped until the entry expires.

use crate::cache::ArticleAvailability;
use crate::router::{BackendSelector, SuppressedBackends};
use crate::session::ClientSession;
use crate::session::SessionError;
use crate::session::handlers::cache_operations::{
    CacheLookupResult, write_cached_article_response,
};
use crate::session::handlers::command_execution::{
    ArticleAttemptState, AuthoritativeArticleMissing, BackendAttemptResult,
};
use crate::types::{BackendToClientBytes, ClientToBackendBytes};
use anyhow::Result;
use std::sync::Arc;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tracing::{debug, trace, warn};

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

impl ClientSession {
    /// Route a single lookup request to a backend and execute it.
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
        let preloaded_availability = if request.message_id_value().is_some() {
            Some(
                self.load_article_availability(
                    request.message_id_value().as_ref(),
                    router.backend_count(),
                )
                .await,
            )
        } else {
            None
        };
        if let Some(availability) = preloaded_availability.as_ref() {
            self.spawn_non_primary_tier_stat_prefetch(
                &router,
                request,
                availability,
                SuppressedBackends::empty(),
            );
        }
        let availability = match self
            .prepare_request_execution(&router, request, &mut io, preloaded_availability)
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
        preloaded_availability: Option<ArticleAvailability>,
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
        let availability = if let Some(availability) = availability {
            Some(availability)
        } else if let Some(availability) = preloaded_availability {
            Some(availability)
        } else if request.message_id_value().is_some() {
            Some(
                self.load_article_availability(
                    request.message_id_value().as_ref(),
                    router.backend_count(),
                )
                .await,
            )
        } else {
            None
        };
        if self
            .try_adaptive_precheck(router, request, io, availability.as_ref())
            .await?
        {
            return Ok(PreparedRequest::Served);
        }

        Ok(PreparedRequest::Continue { availability })
    }

    const fn request_cache_availability(
        availability: crate::protocol::RequestCacheAvailability,
    ) -> ArticleAvailability {
        ArticleAvailability::from_missing_bits(availability.missing_bits())
    }

    async fn try_adaptive_precheck(
        &self,
        router: &Arc<BackendSelector>,
        request: &RequestContext,
        io: &mut RequestExecutionIo<'_>,
        availability: Option<&ArticleAvailability>,
    ) -> Result<bool, SessionError> {
        if !self.adaptive_precheck || !(request.is_stat() || request.is_head()) {
            return Ok(false);
        }
        if request.is_head() && !self.cache_articles {
            return Ok(false);
        }
        let Some(msg_id) = request.message_id_value() else {
            return Ok(false);
        };
        let Some(availability) = availability else {
            return Ok(false);
        };

        let deps = self.precheck_deps(router);
        let Some(response) = precheck::precheck(&deps, request, &msg_id, availability).await else {
            return Ok(false);
        };

        let mut client_write = io.client_writer.lock().await;
        let bytes_written = match response {
            precheck::PrecheckResponse::Cached(entry) => {
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
                    return Ok(false);
                }
            }
            precheck::PrecheckResponse::Direct(response) => {
                Self::write_precheck_direct_response(&mut *client_write, response).await?
            }
        };
        *io.backend_to_client_bytes = io.backend_to_client_bytes.add(bytes_written);
        Ok(true)
    }

    async fn write_precheck_direct_response<W>(
        client_write: &mut W,
        response: crate::cache::CacheIngestResponse,
    ) -> Result<usize, SessionError>
    where
        W: AsyncWrite + Unpin,
    {
        let len = response.len();
        let write_result = match response {
            crate::cache::CacheIngestResponse::Owned(buffer) => {
                client_write.write_all(&buffer).await
            }
            crate::cache::CacheIngestResponse::Pooled(buffer) => {
                client_write.write_all(buffer.as_ref()).await
            }
            crate::cache::CacheIngestResponse::Chunked(response) => {
                for chunk in response.iter_chunks() {
                    if let Err(err) = client_write.write_all(chunk).await {
                        return Err(SessionError::from(anyhow::Error::from(err)));
                    }
                }
                Ok(())
            }
            crate::cache::CacheIngestResponse::Inline(buffer) => {
                client_write.write_all(buffer.as_slice()).await
            }
        };
        write_result.map_err(|e| SessionError::from(anyhow::Error::from(e)))?;
        client_write
            .flush()
            .await
            .map(|()| len)
            .map_err(|e| SessionError::from(anyhow::Error::from(e)))
    }

    async fn write_backend_error_response<W>(client_write: &mut W) -> Result<usize, SessionError>
    where
        W: AsyncWrite + Unpin,
    {
        client_write
            .write_all(crate::protocol::BACKEND_ERROR)
            .await
            .map_err(|e| SessionError::from(anyhow::Error::from(e)))?;
        client_write
            .flush()
            .await
            .map(|()| crate::protocol::BACKEND_ERROR.len())
            .map_err(|e| SessionError::from(anyhow::Error::from(e)))
    }

    async fn execute_article_retry_loop(
        &self,
        router: &Arc<BackendSelector>,
        request: &mut RequestContext,
        availability: Option<ArticleAvailability>,
        io: &mut RequestExecutionIo<'_>,
    ) -> Result<(), SessionError> {
        trace!(
            "Client {} starting availability routing for request kind={:?}, verb={:?}",
            self.client_addr,
            request.kind(),
            request.verb()
        );

        let mut availability = availability.unwrap_or_default();
        let mut unavailable_backends = SuppressedBackends::empty();
        let mut is_retry_attempt = false;
        let mut non_primary_tier_prefetch_started = false;
        let mut retry_stat_sweep_done = false;
        trace!(
            "Client {} availability routing: missing_bits={:08b}, backend_count={}",
            self.client_addr,
            availability.missing_bits(),
            router.backend_count().get()
        );

        while !availability.all_exhausted(router.backend_count()) {
            if !is_retry_attempt && !non_primary_tier_prefetch_started {
                self.spawn_non_primary_tier_stat_prefetch(
                    router,
                    request,
                    &availability,
                    unavailable_backends,
                );
                non_primary_tier_prefetch_started = true;
            }

            if is_retry_attempt && !retry_stat_sweep_done {
                self.parallel_retry_stat_sweep(
                    router,
                    request,
                    &mut ArticleAttemptState {
                        availability: &mut availability,
                        client_to_backend_bytes: io.client_to_backend_bytes,
                        backend_connection: io.backend_connection,
                        unavailable_backends: &mut unavailable_backends,
                    },
                )
                .await?;
                retry_stat_sweep_done = true;
            }

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
                    is_retry_attempt,
                )
                .await;
            match attempt {
                Ok(BackendAttemptResult::Success) => {
                    let response = request
                        .response_metadata()
                        .expect("successful direct attempt records response metadata");
                    *io.backend_to_client_bytes =
                        io.backend_to_client_bytes.add(response.wire_len().get());
                    return Ok(());
                }
                Ok(BackendAttemptResult::ArticleNotFound { missing }) => {
                    is_retry_attempt = true;
                    let backend_id = missing.backend_id();
                    trace!(
                        "Client {} backend {:?} returned 430 during retry",
                        self.client_addr, backend_id
                    );
                }
                Ok(BackendAttemptResult::BackendUnavailable) => {
                    is_retry_attempt = true;
                }
                Ok(BackendAttemptResult::NoRetryableBackend) => {
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

        trace!(
            "Client {} all backends exhausted for {:?}, sending 430",
            self.client_addr,
            request.message_id_value()
        );
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
                    debug!(
                        "Client {} loaded availability for {}: missing_bits={:08b}",
                        self.client_addr,
                        msg_id_ref,
                        avail.missing_bits()
                    );
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
        missing: &AuthoritativeArticleMissing,
        availability: &mut crate::cache::ArticleAvailability,
    ) {
        let backend_id = missing.backend_id();
        availability.record_missing(backend_id);

        // Track 430 responses in 4xx metrics for visibility in TUI
        // While 430 is normal retry behavior (not a failure), users want to see
        // these counted to understand backend article distribution
        self.metrics.record_error_4xx(backend_id);
    }
}

#[cfg(test)]
mod tests {
    use crate::cache::UnifiedCache;
    use crate::protocol::{RequestCacheAvailability, StatusCode};
    use crate::session::ClientSession;
    use crate::types::{BackendId, MessageId};
    use std::time::Duration;

    #[tokio::test]
    async fn status_fact_preserves_missing_backend_without_payload() {
        let cache = UnifiedCache::memory(1_000_000, Duration::from_secs(60));
        let msg_id = MessageId::from_borrowed("<ordered-success@example.com>").unwrap();

        cache
            .record_backend_missing(msg_id.clone(), BackendId::from_index(0))
            .await;
        cache
            .record_backend_has_status(
                msg_id.clone(),
                StatusCode::new(222),
                BackendId::from_index(1),
                crate::cache::ttl::CacheTier::new(0),
            )
            .await;
        cache.sync().await;

        let entry = cache
            .get(&msg_id)
            .await
            .expect("cache facts must preserve mixed success/missing availability");
        assert_eq!(entry.status_code(), StatusCode::new(222));
        assert_eq!(entry.payload_len().get(), 0);
        assert!(!entry.should_try_backend(BackendId::from_index(0)));
        assert!(entry.should_try_backend(BackendId::from_index(1)));
        assert_eq!(entry.availability().missing_bits(), 0b0000_0001);
    }

    #[test]
    fn request_cache_availability_discards_positive_checked_bits_for_retry() {
        let availability = ClientSession::request_cache_availability(
            RequestCacheAvailability::from_bits(0b0000_0110, 0b0000_0010),
        );

        assert!(availability.should_try(BackendId::from_index(0)));
        assert!(!availability.should_try(BackendId::from_index(1)));
        assert!(availability.should_try(BackendId::from_index(2)));
        assert_eq!(availability.missing_bits(), 0b0000_0010);
    }
}
