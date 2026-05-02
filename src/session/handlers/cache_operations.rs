//! Cache interaction helpers for per-command routing
//!
//! Handles serving responses from cache, spawning cache upserts,
//! and tier-aware cache operations.

use crate::cache::ArticleAvailability;
use crate::protocol::RequestContext;
use crate::router::{BackendSelector, CommandGuard};
use crate::session::{ClientSession, precheck};
use crate::types::{BackendId, BackendToClientBytes};
use anyhow::Result;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tracing::debug;

/// Result of a cache lookup attempt in `try_serve_from_cache`.
///
/// Distinguishes between a full cache hit (response served), a partial hit
/// (entry existed but wasn't servable — availability info is available),
/// and a complete miss (no entry in cache at all).
pub(super) enum CacheLookupResult {
    /// Response was served directly from cache.
    Hit(BackendId),
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
        request: &RequestContext,
        router: &Arc<BackendSelector>,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        backend_to_client_bytes: &mut BackendToClientBytes,
    ) -> Result<CacheLookupResult> {
        let Some(msg_id_ref) = msg_id else {
            return Ok(CacheLookupResult::Miss);
        };

        debug!(
            "Client {} checking cache for {}",
            self.client_addr, msg_id_ref
        );

        let Some(cached) = self.cache.get(msg_id_ref).await else {
            debug!("Cache MISS for message-ID: {}", msg_id_ref);
            return Ok(CacheLookupResult::Miss);
        };

        // Extract availability before any early returns so we can pass it back
        let availability = cached.to_availability(router.backend_count());

        if !cached.has_availability_info() {
            debug!(
                "Cache entry exists for {} but no availability info (missing=0) - running precheck",
                msg_id_ref
            );
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
            return Ok(CacheLookupResult::PartialHit(availability));
        }

        let Some(response) = cached.response_parts_for_command_bytes(cmd_verb, msg_id_ref.as_str())
        else {
            return Ok(CacheLookupResult::PartialHit(availability));
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
        *backend_to_client_bytes = backend_to_client_bytes.add(bytes_written);

        let backend_id = router.route(self.client_id)?;
        let guard = CommandGuard::new(router.clone(), backend_id);
        guard.complete();
        Ok(CacheLookupResult::Hit(backend_id))
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
        tier: u8,
    ) {
        self.spawn_cache_upsert_buffer(msg_id, buffer.to_vec().into(), backend_id, tier);
    }

    /// Spawn async cache upsert task with owned hot-path storage.
    pub(super) fn spawn_cache_upsert_buffer(
        &self,
        msg_id: &crate::types::MessageId<'_>,
        buffer: crate::cache::CacheBuffer,
        backend_id: crate::types::BackendId,
        tier: u8,
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
    pub(super) fn tier_for_backend(&self, backend_id: BackendId) -> u8 {
        self.router
            .as_ref()
            .and_then(|r| r.get_tier(backend_id))
            .unwrap_or(0)
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
