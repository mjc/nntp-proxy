//! Adaptive precheck for concurrent backend queries.
//!
//! Queries all backends simultaneously, uses first successful response.
//! Results cached to avoid redundant queries on future requests.

use std::sync::Arc;

use crate::cache::{ArticleAvailability, CachedArticle, UnifiedCache};
use crate::metrics::MetricsCollector;
use crate::pool::BufferPool;
use crate::protocol::{RequestContext, StatusCode};
use crate::router::BackendSelector;
use crate::session::backend;
use crate::session::handlers::should_sample_backend_timing;
use crate::types::{BackendId, MessageId};
use futures::{StreamExt, stream::FuturesUnordered};

#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub(crate) enum PrecheckHit {
    Payload(crate::cache::CacheIngestResponse),
    Availability(StatusCode),
}

/// Result of querying a backend for an article.
#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub(crate) enum QueryResult {
    Found(BackendId, PrecheckHit),
    Missing(BackendId),
    Error,
}

/// Shared dependencies for precheck operations.
#[derive(Clone, Copy)]
pub struct PrecheckDeps<'a> {
    pub router: &'a Arc<BackendSelector>,
    pub cache: &'a Arc<UnifiedCache>,
    pub buffer_pool: &'a BufferPool,
    pub metrics: &'a MetricsCollector,
    pub cache_articles: bool,
}

#[derive(Clone)]
struct OwnedDeps {
    router: Arc<BackendSelector>,
    cache: Arc<UnifiedCache>,
    buffer_pool: BufferPool,
    metrics: MetricsCollector,
    cache_articles: bool,
}

impl PrecheckDeps<'_> {
    fn clone_deps(&self) -> OwnedDeps {
        OwnedDeps {
            router: Arc::clone(self.router),
            cache: Arc::clone(self.cache),
            buffer_pool: self.buffer_pool.clone(),
            metrics: self.metrics.clone(),
            cache_articles: self.cache_articles,
        }
    }
}

async fn cache_precheck_hit(
    cache: &UnifiedCache,
    msg_id: MessageId<'static>,
    backend_id: BackendId,
    hit: PrecheckHit,
    tier: crate::cache::ttl::CacheTier,
) {
    match hit {
        PrecheckHit::Payload(data) => {
            cache.upsert_ingest(msg_id, data, backend_id, tier).await;
        }
        PrecheckHit::Availability(status_code) => {
            cache
                .record_backend_has_status(msg_id, status_code, backend_id, tier)
                .await;
        }
    }
}

async fn query_backend(
    deps: &OwnedDeps,
    backend_id: BackendId,
    request: &RequestContext,
) -> QueryResult {
    let Some(provider) = deps.router.backend_provider(backend_id) else {
        return QueryResult::Error;
    };

    // Track pending count for load balancing
    deps.router.mark_backend_pending(backend_id);

    // Retry once on backend error (fresh connection on second attempt)
    let query_result: QueryResult = crate::session::retry::retry_once!(
        execute_backend_query(deps, provider, backend_id, request).await,
        backend = backend_id.as_index()
    )
    .unwrap_or(QueryResult::Error);

    // Always decrement pending count when done
    deps.router.complete_command(backend_id);

    query_result
}

/// Execute a single backend query attempt
///
/// Returns Ok(QueryResult) on successful communication (even if article not found).
/// Returns Err on connection errors (caller should retry).
async fn execute_backend_query(
    deps: &OwnedDeps,
    provider: &crate::pool::DeadpoolConnectionProvider,
    backend_id: BackendId,
    request: &RequestContext,
) -> Result<QueryResult, ()> {
    let Ok(conn_raw) = provider.get_pooled_connection().await else {
        return Ok(QueryResult::Error);
    };
    let mut conn = crate::pool::ConnectionGuard::new(conn_raw, provider.clone());

    let mut buffer = deps.buffer_pool.acquire();

    let response = if should_sample_backend_timing() {
        backend::send_request_timed(&mut **conn, request, &mut buffer)
            .await
            .map(|(response, ttfb, send, recv)| (response, Some((ttfb, send, recv))))
    } else {
        backend::send_request(&mut **conn, request, &mut buffer)
            .await
            .map(|response| (response, None))
    };

    // Use shared backend request execution with sampled timing
    match response {
        Ok((cmd_response, timings)) => {
            let Some(status_code) = cmd_response.status_code() else {
                // Invalid response - conn drops → remove_with_cooldown
                return Err(());
            };

            let response = build_precheck_hit(
                deps,
                request,
                status_code,
                &mut conn,
                &mut buffer,
                cmd_response.bytes_read,
            )
            .await?;

            let result = classify_precheck_result(deps, backend_id, status_code, timings, response);

            let _ = conn.release(); // response received; connection healthy, return to pool
            Ok(result)
        }
        Err(_) => {
            // Connection error - conn drops → remove_with_cooldown
            Err(())
        }
    }
}

async fn build_precheck_hit(
    deps: &OwnedDeps,
    request: &RequestContext,
    status_code: StatusCode,
    conn: &mut crate::pool::ConnectionGuard,
    buffer: &mut crate::pool::PooledBuffer,
    bytes_read: usize,
) -> Result<PrecheckHit, ()> {
    if request.expects_multiline_response(status_code) {
        return read_multiline_precheck_hit(deps, status_code, conn, buffer, bytes_read).await;
    }

    if deps.cache_articles {
        Ok(PrecheckHit::Payload(
            crate::cache::CacheIngestResponse::from(&buffer[..bytes_read]),
        ))
    } else {
        Ok(PrecheckHit::Availability(status_code))
    }
}

async fn read_multiline_precheck_hit(
    deps: &OwnedDeps,
    status_code: StatusCode,
    conn: &mut crate::pool::ConnectionGuard,
    buffer: &mut crate::pool::PooledBuffer,
    bytes_read: usize,
) -> Result<PrecheckHit, ()> {
    use crate::session::streaming::tail_buffer::{TailBuffer, TerminatorStatus};

    let mut response = deps
        .cache_articles
        .then(crate::pool::ChunkedResponse::default);
    if let Some(response) = &mut response {
        response.extend_from_slice(&deps.buffer_pool, &buffer[..bytes_read]);
    }
    let mut tail = TailBuffer::default();
    match tail.detect_terminator(buffer.as_ref()) {
        TerminatorStatus::FoundAt(pos) => {
            // pos is after the terminator (terminator included in [..pos])
            if pos < bytes_read && conn.stash_leftover(&buffer[pos..bytes_read]).is_err() {
                return Err(());
            }
            if let Some(response) = &mut response {
                response.truncate(pos);
            }
        }
        TerminatorStatus::NotFound => {
            tail.update(buffer.as_ref());
            loop {
                match buffer.read_from(conn.as_mut()).await {
                    Ok(0) | Err(_) => return Err(()),
                    Ok(n) => match tail.detect_terminator(&buffer[..n]) {
                        TerminatorStatus::FoundAt(pos) => {
                            if pos < n && conn.stash_leftover(&buffer[pos..n]).is_err() {
                                return Err(());
                            }
                            if let Some(response) = &mut response {
                                response.extend_from_slice(&deps.buffer_pool, &buffer[..pos]);
                            }
                            break;
                        }
                        TerminatorStatus::NotFound => {
                            tail.update(&buffer[..n]);
                            if let Some(response) = &mut response {
                                response.extend_from_slice(&deps.buffer_pool, &buffer[..n]);
                            }
                        }
                    },
                }
            }
        }
    }
    if let Some(response) = response {
        Ok(PrecheckHit::Payload(
            crate::cache::CacheIngestResponse::Chunked(response),
        ))
    } else {
        Ok(PrecheckHit::Availability(status_code))
    }
}

fn classify_precheck_result(
    deps: &OwnedDeps,
    backend_id: BackendId,
    status_code: StatusCode,
    timings: Option<(u64, u64, u64)>,
    response: PrecheckHit,
) -> QueryResult {
    deps.metrics.record_command(backend_id);
    if let Some((ttfb, send, recv)) = timings {
        deps.metrics.record_ttfb_micros(backend_id, ttfb);
        deps.metrics.record_send_recv_micros(backend_id, send, recv);
    }

    match status_code.as_u16() {
        220..=223 => {
            tracing::debug!(
                backend = backend_id.as_index(),
                "precheck recording command to metrics"
            );
            QueryResult::Found(backend_id, response)
        }
        430 => {
            deps.metrics.record_error_4xx(backend_id);
            QueryResult::Missing(backend_id)
        }
        _ => QueryResult::Error,
    }
}

/// Query all backends, collecting results as they arrive
async fn query_all_backends(deps: &OwnedDeps, request: &RequestContext) -> Vec<QueryResult> {
    let mut pending = spawn_backend_queries(deps, request);
    let mut results = Vec::with_capacity(deps.router.backend_count().get());

    while let Some(result) = pending.next().await {
        if let Ok(result) = result {
            results.push(result);
        }
    }

    results
}

/// Query all backends racing, return first success immediately.
/// Background task updates cache with full availability once all backends complete.
async fn query_all_backends_racing(
    deps: &OwnedDeps,
    request: &RequestContext,
    msg_id: &MessageId<'_>,
) -> Option<(BackendId, PrecheckHit)> {
    let backend_count = deps.router.backend_count();
    let mut pending = spawn_backend_queries(deps, request);

    let mut results = Vec::with_capacity(backend_count.get());

    // Collect results until we find a success
    while let Some(result) = pending.next().await {
        let Ok(result) = result else {
            continue;
        };
        match result {
            QueryResult::Found(id, response) => {
                // Spawn background task to complete remaining backends and update cache
                let cache = deps.cache.clone();
                let msg_id_owned = msg_id.to_owned();
                let msg_id_log = msg_id.to_string();
                let handle = tokio::spawn(async move {
                    // Collect remaining results
                    while let Some(result) = pending.next().await {
                        if let Ok(result) = result {
                            results.push(result);
                        }
                    }
                    // Build availability from all results and sync to cache
                    let (_, mut availability) = summarize(results);
                    availability.record_has(id);
                    cache.sync_availability(msg_id_owned, &availability).await;
                });

                // Log task failures in the background
                tokio::spawn(async move {
                    if let Err(e) = handle.await {
                        tracing::error!(
                            "Background precheck sync task panicked for {}: {}",
                            msg_id_log,
                            e
                        );
                    }
                });
                return Some((id, response));
            }
            _ => {
                results.push(result);
            }
        }
    }

    // No success found - all backends returned 430
    None
}

fn spawn_backend_queries(
    deps: &OwnedDeps,
    request: &RequestContext,
) -> FuturesUnordered<tokio::task::JoinHandle<QueryResult>> {
    (0..deps.router.backend_count().get())
        .map(BackendId::from_index)
        .map(|id| {
            let deps = deps.clone();
            let request = request.clone();
            tokio::spawn(async move { query_backend(&deps, id, &request).await })
        })
        .collect()
}

/// Extract first found response and build availability from results.
///
/// # NNTP Semantics
/// 430 responses are authoritative (never false negatives), 2xx are not.
/// See `crate::cache::article` module docs for full explanation.
fn summarize(results: Vec<QueryResult>) -> (Option<(BackendId, PrecheckHit)>, ArticleAvailability) {
    let mut availability = ArticleAvailability::new();
    let mut found = None;

    for r in results {
        match r {
            QueryResult::Found(id, response) => {
                availability.record_has(id);
                if found.is_none() {
                    found = Some((id, response));
                }
            }
            QueryResult::Missing(id) => {
                availability.record_missing(id);
            }
            QueryResult::Error => {}
        }
    }

    (found, availability)
}

/// Store precheck results in cache.
///
/// If found, upserts with data. Then syncs full availability state.
/// Returns the cache entry if article was found.
/// Precheck all backends for an article.
///
/// Queries concurrently, returns first successful response immediately.
/// Remaining backends complete in background to update full availability.
///
/// Skips backend queries entirely if we already have a complete article cached.
pub async fn precheck(
    deps: &PrecheckDeps<'_>,
    request: &RequestContext,
    msg_id: &MessageId<'_>,
) -> Option<CachedArticle> {
    // Check cache first - if we have a complete article, return it immediately
    if let Some(cached) = deps.cache.get(msg_id).await
        && cached.is_complete_article()
    {
        return Some(cached);
    }

    let owned = deps.clone_deps();
    let found = query_all_backends_racing(&owned, request, msg_id).await;

    // Cache the found result and return it
    if let Some((backend_id, data)) = found {
        // NOTE: We intentionally use tier 0 for articles found via racing queries.
        // Racing all backends can cause higher-tier backends to respond slightly faster,
        // incorrectly caching an article as "higher tier" (longer TTL) even if a lower-tier
        // backend also has it but responded slower. Using tier 0 conservatively ensures
        // we don't overestimate TTL. Regular routing will discover higher-tier availability.
        let tier = crate::cache::ttl::CacheTier::new(0);
        // Move data into upsert, then retrieve the entry via cache.get().
        // The lookup is sub-microsecond vs 750KB memcpy from cloning.
        cache_precheck_hit(&owned.cache, msg_id.to_owned(), backend_id, data, tier).await;
        owned.cache.get(msg_id).await
    } else {
        // No article found - availability is synced by background task
        None
    }
}

/// Spawn background precheck. Results go to cache only.
///
/// Skips backend queries entirely if we already have a complete article cached.
pub fn spawn_background_precheck(
    deps: PrecheckDeps<'_>,
    request: RequestContext,
    msg_id: MessageId<'static>,
) {
    let owned = deps.clone_deps();
    tokio::spawn(async move {
        // Check cache first - if we have a complete article, no need to query backends
        if let Some(cached) = owned.cache.get(&msg_id).await
            && cached.is_complete_article()
        {
            // Already have full article cached - nothing to do
            return;
        }

        let results = query_all_backends(&owned, &request).await;
        let (found, availability) = summarize(results);

        if let Some((backend_id, data)) = found {
            // NOTE: We intentionally use tier 0 for articles found via background precheck.
            // When racing backends, we don't have visibility into which tier actually has
            // the article. Using tier 0 conservatively avoids overestimating TTL.
            let tier = crate::cache::ttl::CacheTier::new(0);
            cache_precheck_hit(&owned.cache, msg_id.to_owned(), backend_id, data, tier).await;
        }
        owned.cache.sync_availability(msg_id, &availability).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn summarize_finds_first() {
        let results = vec![
            QueryResult::Missing(BackendId::from_index(0)),
            QueryResult::Found(
                BackendId::from_index(1),
                PrecheckHit::Payload(b"first".to_vec().into()),
            ),
            QueryResult::Found(
                BackendId::from_index(2),
                PrecheckHit::Payload(b"second".to_vec().into()),
            ),
        ];
        let (found, avail) = summarize(results);
        assert_eq!(
            found,
            Some((
                BackendId::from_index(1),
                PrecheckHit::Payload(crate::cache::CacheIngestResponse::from(b"first".to_vec()))
            ))
        );
        assert!(avail.is_missing(BackendId::from_index(0)));
        assert!(!avail.is_missing(BackendId::from_index(1)));
        assert!(!avail.is_missing(BackendId::from_index(2)));
    }

    #[test]
    fn summarize_all_missing() {
        let results = vec![
            QueryResult::Missing(BackendId::from_index(0)),
            QueryResult::Missing(BackendId::from_index(1)),
        ];
        let (found, avail) = summarize(results);
        assert!(found.is_none());
        assert!(avail.is_missing(BackendId::from_index(0)));
        assert!(avail.is_missing(BackendId::from_index(1)));
    }

    #[test]
    fn summarize_empty() {
        let (found, avail) = summarize(vec![]);
        assert!(found.is_none());
        assert_eq!(avail.checked_bits(), 0);
    }

    async fn spawn_truncated_precheck_server() -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            loop {
                if let Ok((mut stream, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        let _ = stream.write_all(b"200 mock\r\n").await;
                        let mut buf = [0u8; 1024];
                        let _ = stream.read(&mut buf).await;
                        let _ = stream
                            .write_all(b"220 0 <test@example.com>\r\npartial body")
                            .await;
                        let _ = stream.shutdown().await;
                    });
                }
            }
        });

        addr
    }

    #[tokio::test]
    async fn query_backend_returns_error_on_truncated_multiline_response() {
        use crate::cache::UnifiedCache;
        use crate::metrics::MetricsCollector;
        use crate::pool::{BufferPool, DeadpoolConnectionProvider};
        use crate::router::BackendSelector;
        use crate::types::{BackendId, BufferSize, ServerName};

        let addr = spawn_truncated_precheck_server().await;

        let mut selector = BackendSelector::new();
        let backend_id = BackendId::from_index(0);
        let provider = DeadpoolConnectionProvider::new(
            "127.0.0.1".to_string(),
            addr.port(),
            "test".to_string(),
            2,
            None,
            None,
        );
        selector.add_backend(
            backend_id,
            ServerName::try_new("test-server".to_string()).unwrap(),
            provider,
            0,
        );

        let deps = OwnedDeps {
            router: Arc::new(selector),
            cache: Arc::new(UnifiedCache::memory(100, Duration::from_secs(60))),
            buffer_pool: BufferPool::new(BufferSize::try_new(4096).unwrap(), 2),
            metrics: MetricsCollector::new(1),
            cache_articles: true,
        };

        let request =
            RequestContext::parse(b"ARTICLE <test@example.com>\r\n").expect("valid request line");
        let result = query_backend(&deps, backend_id, &request).await;
        assert_eq!(result, QueryResult::Error);
    }

    #[tokio::test]
    async fn cache_precheck_availability_hit_does_not_persist_positive_entry() {
        use crate::cache::UnifiedCache;
        use crate::types::BackendId;

        let backend_id = BackendId::from_index(0);
        let cache = UnifiedCache::availability(std::time::Duration::MAX);
        let msg_id = MessageId::new("<test@example.com>".to_string()).unwrap();

        cache_precheck_hit(
            &cache,
            msg_id.to_owned(),
            backend_id,
            PrecheckHit::Availability(StatusCode::new(223)),
            crate::cache::ttl::CacheTier::new(0),
        )
        .await;

        assert!(
            cache.get(&msg_id).await.is_none(),
            "availability-only index should persist negatives, not optimistic positive hits"
        );
    }
}
