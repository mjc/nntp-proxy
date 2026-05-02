//! Cache interaction helpers for per-command routing
//!
//! Handles serving responses from cache, spawning cache upserts,
//! and tier-aware cache operations.

use crate::cache::ArticleAvailability;
use crate::cache::ttl::CacheTier;
use crate::protocol::{RequestCacheStatus, RequestContext, RequestKind, StatusCode};
use crate::router::{BackendSelector, CommandGuard};
use crate::session::{ClientSession, precheck};
use crate::types::{BackendId, BackendToClientBytes, MessageId};
use anyhow::Result;
use std::sync::Arc;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tracing::debug;

/// Result of a cache lookup attempt in `try_serve_from_cache`.
///
/// Distinguishes between a full cache hit (response served), a partial hit
/// (entry existed but wasn't servable — availability info is available),
/// and a complete miss (no entry in cache at all).
pub(super) enum CacheLookupResult {
    /// Response was served directly from cache.
    Hit,
    /// Entry existed but wasn't servable. Carries the availability info
    /// so callers can skip a redundant `cache.get()`.
    PartialHit(ArticleAvailability),
    /// No entry in cache at all.
    Miss,
}

impl ClientSession {
    /// Try to serve from cache
    ///
    /// Returns `CacheLookupResult::Hit` if served, `PartialHit` if entry existed but
    /// wasn't servable (carries availability to avoid a redundant lookup), or `Miss`.
    pub(super) async fn try_serve_from_cache(
        &self,
        msg_id: Option<&crate::types::MessageId<'_>>,
        request: &mut RequestContext,
        router: &Arc<BackendSelector>,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        backend_to_client_bytes: &mut BackendToClientBytes,
    ) -> Result<CacheLookupResult> {
        let Some(msg_id_ref) = msg_id else {
            request.record_cache_status(RequestCacheStatus::Miss);
            return Ok(CacheLookupResult::Miss);
        };

        debug!(
            "Client {} checking cache for {}",
            self.client_addr, msg_id_ref
        );

        let Some(cached) = self.cache.get(msg_id_ref).await else {
            debug!("Cache MISS for message-ID: {}", msg_id_ref);
            request.record_cache_status(RequestCacheStatus::Miss);
            return Ok(CacheLookupResult::Miss);
        };

        // Extract availability before any early returns so we can pass it back
        let availability = cached.to_availability(router.backend_count());

        if !cached.has_availability_info() {
            debug!(
                "Cache entry exists for {} but no availability info (missing=0) - running precheck",
                msg_id_ref
            );
            request.record_cache_status(RequestCacheStatus::PartialHit);
            return Ok(CacheLookupResult::PartialHit(availability));
        }

        debug!(
            "Client {} cache HIT for {} (cache_articles={})",
            self.client_addr, msg_id_ref, self.cache_articles
        );

        // If full article caching enabled, try to serve from cache
        if !self.cache_articles {
            // Availability-only mode - spawn background precheck to update availability
            // then fall through to use availability info for routing
            if self.adaptive_precheck && request.is_stat() {
                precheck::spawn_background_precheck(
                    self.precheck_deps(router),
                    request.clone(),
                    msg_id_ref.to_owned(),
                );
            }
            request.record_cache_status(RequestCacheStatus::PartialHit);
            return Ok(CacheLookupResult::PartialHit(availability));
        }

        // Check if this is a complete article we can serve
        // Stubs from STAT/HEAD precheck or availability tracking should not be served
        // Exception: STAT can be answered from any cache entry (we just need to know it exists)
        let cmd_verb = request.verb();

        if !cmd_verb.eq_ignore_ascii_case(b"STAT") && !cached.is_complete_article() {
            debug!(
                "Client {} cache entry for {} is a stub (payload_len={}), fetching full article",
                self.client_addr,
                msg_id_ref,
                cached.payload_len()
            );
            request.record_cache_status(RequestCacheStatus::PartialHit);
            return Ok(CacheLookupResult::PartialHit(availability));
        }

        // Serve from cache, avoiding buffer copies for the common path.
        // STAT is synthesized (tiny response), everything else writes directly from the Arc buffer.
        if !cached.matches_command_type_verb_bytes(cmd_verb) {
            let status_code = cached.status_code().map_or(0, |c| c.as_u16());
            debug!(
                "Client {} cached response (code={}) can't serve command {:?}",
                self.client_addr,
                status_code,
                String::from_utf8_lossy(cmd_verb)
            );
            request.record_cache_status(RequestCacheStatus::PartialHit);
            return Ok(CacheLookupResult::PartialHit(availability));
        }

        let Some(bytes_written) =
            write_cached_article_response(client_write, &cached, cmd_verb, msg_id_ref).await?
        else {
            request.record_cache_status(RequestCacheStatus::PartialHit);
            return Ok(CacheLookupResult::PartialHit(availability));
        };
        *backend_to_client_bytes = backend_to_client_bytes.add(bytes_written);

        let backend_id = router.route(self.client_id)?;
        let guard = CommandGuard::new(router.clone(), backend_id);
        guard.complete();
        let status = cached_response_status(request).expect("cache hit has typed response status");
        request.record_cache_response(backend_id, status, bytes_written);
        Ok(CacheLookupResult::Hit)
    }

    /// Spawn async cache upsert task
    ///
    /// This is fire-and-forget - we don't wait for the cache to update.
    /// Used after successfully streaming a response to update availability tracking.
    ///
    /// The tier is used for tier-aware TTL (higher tier = longer TTL).
    pub(super) fn spawn_cache_upsert(
        &self,
        msg_id: &crate::types::MessageId<'_>,
        buffer: &[u8],
        backend_id: crate::types::BackendId,
        tier: CacheTier,
    ) {
        self.spawn_cache_upsert_buffer(msg_id, buffer.to_vec().into(), backend_id, tier);
    }

    /// Spawn async cache upsert task with owned hot-path storage.
    pub(super) fn spawn_cache_upsert_buffer(
        &self,
        msg_id: &crate::types::MessageId<'_>,
        buffer: crate::cache::CacheBuffer,
        backend_id: crate::types::BackendId,
        tier: CacheTier,
    ) {
        let cache_clone = self.cache.clone();
        let msg_id_owned = msg_id.to_owned();
        tokio::spawn(async move {
            cache_clone
                .upsert(msg_id_owned, buffer, backend_id, tier)
                .await;
        });
    }
    /// Get the tier for a backend, defaulting to 0 if router or backend not found.
    pub(super) fn tier_for_backend(&self, backend_id: BackendId) -> CacheTier {
        self.router
            .as_ref()
            .and_then(|r| r.get_tier(backend_id))
            .unwrap_or(0)
            .into()
    }

    /// Create precheck dependencies
    pub(super) const fn precheck_deps<'a>(
        &'a self,
        router: &'a Arc<BackendSelector>,
    ) -> precheck::PrecheckDeps<'a> {
        precheck::PrecheckDeps {
            router,
            cache: &self.cache,
            buffer_pool: &self.buffer_pool,
            metrics: &self.metrics,
            cache_articles: self.cache_articles,
        }
    }
}

fn cached_response_status(request: &RequestContext) -> Option<StatusCode> {
    let code = match request.kind() {
        RequestKind::Article => 220,
        RequestKind::Head => 221,
        RequestKind::Body => 222,
        RequestKind::Stat => 223,
        _ => return None,
    };
    Some(StatusCode::new(code))
}

pub(super) async fn write_cached_article_response<W>(
    client_write: &mut W,
    cached: &crate::cache::ArticleEntry,
    cmd_verb: &[u8],
    msg_id: &MessageId<'_>,
) -> std::io::Result<Option<usize>>
where
    W: AsyncWrite + Unpin,
{
    let Some(response) = cached.response_parts_for_command_bytes(cmd_verb, msg_id.as_str()) else {
        return Ok(None);
    };
    let bytes_written = response.len();
    client_write.write_all(response.status_line()).await?;
    match response.payload_slices() {
        crate::cache::CachedArticlePayloadSlices::None => {}
        crate::cache::CachedArticlePayloadSlices::Article { headers, body } => {
            client_write.write_all(headers).await?;
            client_write.write_all(b"\r\n\r\n").await?;
            client_write.write_all(body).await?;
            client_write.write_all(b"\r\n.\r\n").await?;
        }
        crate::cache::CachedArticlePayloadSlices::Head { headers } => {
            client_write.write_all(headers).await?;
            client_write.write_all(b"\r\n.\r\n").await?;
        }
        crate::cache::CachedArticlePayloadSlices::Body { body } => {
            client_write.write_all(body).await?;
            client_write.write_all(b"\r\n.\r\n").await?;
        }
    }
    Ok(Some(bytes_written))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthHandler;
    use crate::cache::UnifiedCache;
    use crate::metrics::MetricsCollector;
    use crate::pool::{BufferPool, DeadpoolConnectionProvider};
    use crate::types::{BufferSize, ClientAddress, ServerName};
    use std::net::SocketAddr;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    use tokio::net::{TcpListener, TcpStream};

    fn test_session() -> ClientSession {
        let addr: SocketAddr = "127.0.0.1:0".parse().expect("valid address");
        ClientSession::builder(
            ClientAddress::from(addr),
            BufferPool::new(BufferSize::try_new(1024).expect("valid buffer size"), 1),
            Arc::new(AuthHandler::new(None, None).expect("auth disabled")),
            MetricsCollector::new(1),
        )
        .with_cache(Arc::new(UnifiedCache::memory(
            1024,
            Duration::from_secs(60),
            true,
        )))
        .build()
    }

    async fn tcp_write_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener address");
        let connect = TcpStream::connect(addr);
        let accept = listener.accept();
        let (client, server) = tokio::join!(connect, accept);
        (
            client.expect("connect client"),
            server.expect("accept client").0,
        )
    }

    #[tokio::test]
    async fn cache_miss_is_recorded_on_request_context() {
        let session = test_session();
        let router = Arc::new(BackendSelector::new());
        let mut metrics = BackendToClientBytes::zero();
        let (mut client, _server) = tcp_write_pair().await;
        let (_read, mut write) = client.split();
        let mut request = RequestContext::from_request_line("ARTICLE <missing@example>\r\n");
        let msg_id = request
            .message_id_value()
            .map(|msg_id| msg_id.to_owned())
            .expect("message id");

        let result = session
            .try_serve_from_cache(
                Some(&msg_id),
                &mut request,
                &router,
                &mut write,
                &mut metrics,
            )
            .await
            .expect("lookup succeeds");

        assert!(matches!(result, CacheLookupResult::Miss));
        assert_eq!(request.cache_status(), Some(RequestCacheStatus::Miss));
        assert_eq!(metrics, BackendToClientBytes::zero());
    }

    #[tokio::test]
    async fn request_without_message_id_records_cache_miss() {
        let session = test_session();
        let router = Arc::new(BackendSelector::new());
        let mut metrics = BackendToClientBytes::zero();
        let (mut client, _server) = tcp_write_pair().await;
        let (_read, mut write) = client.split();
        let mut request = RequestContext::from_request_line("DATE\r\n");

        let result = session
            .try_serve_from_cache(None, &mut request, &router, &mut write, &mut metrics)
            .await
            .expect("lookup succeeds");

        assert!(matches!(result, CacheLookupResult::Miss));
        assert_eq!(request.cache_status(), Some(RequestCacheStatus::Miss));
        assert_eq!(metrics, BackendToClientBytes::zero());
    }

    #[tokio::test]
    async fn cache_hit_records_response_metadata_on_request_context() {
        let session = test_session();
        let msg_id = MessageId::new("<hit@example>".to_string()).expect("valid message id");
        let expected = b"220 0 <hit@example>\r\nHeader: v\r\n\r\nBody\r\n.\r\n";
        session
            .cache
            .upsert(
                msg_id.clone(),
                expected.to_vec(),
                BackendId::from_index(0),
                0.into(),
            )
            .await;

        let mut router = BackendSelector::new();
        let backend_id = BackendId::from_index(0);
        router.add_backend(
            backend_id,
            ServerName::try_new("cache-hit-backend".to_string()).expect("server name"),
            DeadpoolConnectionProvider::new(
                "127.0.0.1".to_string(),
                119,
                "cache-hit-backend".to_string(),
                1,
                None,
                None,
            ),
            0,
            None,
        );
        let router = Arc::new(router);

        let mut metrics = BackendToClientBytes::zero();
        let (mut client, mut server) = tcp_write_pair().await;
        let (_read, mut write) = client.split();
        let mut request = RequestContext::from_request_line("ARTICLE <hit@example>\r\n");

        let result = session
            .try_serve_from_cache(
                Some(&msg_id),
                &mut request,
                &router,
                &mut write,
                &mut metrics,
            )
            .await
            .expect("lookup succeeds");

        let mut written = vec![0; expected.len()];
        server
            .read_exact(&mut written)
            .await
            .expect("cached response written");

        assert!(matches!(result, CacheLookupResult::Hit));
        assert_eq!(written, expected);
        assert_eq!(request.cache_status(), Some(RequestCacheStatus::Hit));
        assert_eq!(request.backend_id(), Some(backend_id));
        assert_eq!(request.response_status(), Some(StatusCode::new(220)));
        assert_eq!(request.response_wire_len(), Some(expected.len()));
        assert_eq!(metrics, BackendToClientBytes::zero().add(expected.len()));
    }

    #[test]
    fn cached_response_status_matches_synthesized_article_response() {
        let cases = [
            ("ARTICLE <id@example>\r\n", 220),
            ("HEAD <id@example>\r\n", 221),
            ("BODY <id@example>\r\n", 222),
            ("STAT <id@example>\r\n", 223),
        ];

        for (line, status) in cases {
            let request = RequestContext::from_request_line(line);

            assert_eq!(
                cached_response_status(&request),
                Some(StatusCode::new(status))
            );
        }
    }
}
