//! Cache interaction helpers for per-command routing
//!
//! Handles serving responses from cache, spawning cache upserts,
//! and tier-aware cache operations.

use crate::command::classifier::{STAT_CASES, matches_any};
use crate::router::{BackendSelector, CommandGuard};
use crate::session::{ClientSession, precheck};
use crate::types::{BackendId, BackendToClientBytes};
use anyhow::Result;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tracing::debug;

impl ClientSession {
    /// Try to serve from cache
    ///
    /// Returns Some(backend_id) if served from cache, None if cache miss or availability-only mode
    pub(super) async fn try_serve_from_cache(
        &self,
        msg_id: &Option<crate::types::MessageId<'_>>,
        command: &str,
        router: &Arc<BackendSelector>,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        backend_to_client_bytes: &mut BackendToClientBytes,
    ) -> Result<Option<crate::types::BackendId>> {
        let Some(msg_id_ref) = msg_id.as_ref() else {
            return Ok(None);
        };

        debug!(
            "Client {} checking cache for {}",
            self.client_addr, msg_id_ref
        );

        let Some(cached) = self.cache.get(msg_id_ref).await else {
            debug!("Cache MISS for message-ID: {}", msg_id_ref);
            return Ok(None);
        };

        if !cached.has_availability_info() {
            debug!(
                "Cache entry exists for {} but no availability info (missing=0) - running precheck",
                msg_id_ref
            );
            return Ok(None);
        }

        debug!(
            "Client {} cache HIT for {} (cache_articles={})",
            self.client_addr, msg_id_ref, self.cache_articles
        );

        // If full article caching enabled, try to serve from cache
        if !self.cache_articles {
            // Availability-only mode - spawn background precheck to update availability
            // then fall through to use availability info for routing
            if self.adaptive_precheck {
                let bytes = command.as_bytes();
                let cmd_end = memchr::memchr(b' ', bytes).unwrap_or(bytes.len());
                if cmd_end >= 4 && matches_any(&bytes[..cmd_end], STAT_CASES) {
                    precheck::spawn_background_precheck(
                        self.precheck_deps(router),
                        command.to_string(),
                        msg_id_ref.to_owned(),
                    );
                }
            }
            return Ok(None);
        }

        // Check if this is a complete article we can serve
        // Stubs from STAT/HEAD precheck or availability tracking should not be served
        // Exception: STAT can be answered from any cache entry (we just need to know it exists)
        let cmd_verb = command
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();

        if cmd_verb != "STAT" && !cached.is_complete_article() {
            debug!(
                "Client {} cache entry for {} is a stub (len={}), fetching full article",
                self.client_addr,
                msg_id_ref,
                cached.buffer().len()
            );
            return Ok(None);
        }

        // Serve from cache, avoiding buffer copies for the common path.
        // STAT is synthesized (tiny response), everything else writes directly from the Arc buffer.
        let code = cached.status_code().map(|c| c.as_u16());
        let Some(status_code) = code else {
            debug!(
                "Client {} cached response has no valid status code",
                self.client_addr,
            );
            return Ok(None);
        };

        if !crate::cache::entry_helpers::matches_command_type_verb(status_code, &cmd_verb) {
            debug!(
                "Client {} cached response (code={}) can't serve '{}' command",
                self.client_addr, status_code, cmd_verb
            );
            return Ok(None);
        }

        // STAT: synthesize a small response (no buffer copy needed)
        // Direct serve: write directly from the Arc-backed buffer (zero-copy)
        let bytes_written = if cmd_verb == "STAT" {
            let stat_response = format!("223 0 {}\r\n", msg_id_ref.as_str());
            client_write.write_all(stat_response.as_bytes()).await?;
            stat_response.len()
        } else {
            // Validate before serving
            if !crate::cache::entry_helpers::is_valid_response(cached.buffer()) {
                tracing::warn!(
                    code = status_code,
                    len = cached.buffer().len(),
                    "Cached buffer failed validation, discarding"
                );
                return Ok(None);
            }
            let buf = cached.buffer();
            client_write.write_all(buf).await?;
            buf.len()
        };
        *backend_to_client_bytes = backend_to_client_bytes.add(bytes_written);

        let backend_id = router.route_command(self.client_id, command)?;
        let guard = CommandGuard::new(router.clone(), backend_id);
        guard.complete();
        Ok(Some(backend_id))
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
        buffer: Vec<u8>,
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
    pub(super) fn precheck_deps<'a>(
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
