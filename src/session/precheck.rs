//! Adaptive precheck for tier-aware concurrent backend queries.
//!
//! Queries one backend tier at a time, racing backends within that tier and using
//! the first successful response. Higher tiers are only queried after lower tiers
//! fail to produce a successful response.

use std::sync::Arc;

use crate::cache::{ArticleAvailability, CachedArticle, UnifiedCache};
use crate::metrics::MetricsCollector;
use crate::pool::BufferPool;
use crate::protocol::{RequestContext, StatusCode};
use crate::router::{ArticleBackend, BackendSelector};
use crate::session::backend;
use crate::session::handlers::should_sample_backend_timing;
use crate::types::{BackendId, MessageId};
use futures::{StreamExt, stream::FuturesUnordered};

#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq, Eq))]
#[allow(clippy::large_enum_variant)]
pub(crate) enum PrecheckHit {
    Payload(crate::cache::CacheIngestResponse),
    Availability(StatusCode),
}

impl PrecheckHit {
    #[must_use]
    const fn will_update_cache(&self, cache: &UnifiedCache) -> bool {
        match self {
            Self::Payload(_) => cache.stores_payload_responses(),
            Self::Availability(_) => cache.records_backend_has_status(),
        }
    }
}

// Keep the direct response inline to avoid allocating on adaptive precheck hits.
#[allow(clippy::large_enum_variant)]
pub(crate) enum PrecheckResponse {
    Cached(CachedArticle),
    Direct(crate::cache::CacheIngestResponse),
}

/// Result of querying a backend for an article.
#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq, Eq))]
#[allow(clippy::large_enum_variant)]
pub(crate) enum QueryResult {
    Found(BackendId, PrecheckHit),
    Missing(BackendId),
    Error,
}

#[derive(Debug)]
enum QueryAttemptResult {
    Found(Box<PrecheckHit>),
    Missing,
    Error,
}

#[derive(Debug, PartialEq, Eq)]
enum TierQuerySummary {
    Exhausted(ArticleAvailability),
    Inconclusive(ArticleAvailability),
}

impl TierQuerySummary {
    const fn is_exhausted(&self) -> bool {
        matches!(self, Self::Exhausted(_))
    }
}

fn summarize_tier_results(results: &[QueryResult]) -> TierQuerySummary {
    let mut availability = ArticleAvailability::new();
    let mut exhausted = true;

    for result in results {
        match result {
            QueryResult::Missing(id) => {
                availability.record_missing(*id);
            }
            QueryResult::Found(_, _) => {
                exhausted = false;
            }
            QueryResult::Error => {
                exhausted = false;
            }
        }
    }

    if exhausted {
        TierQuerySummary::Exhausted(availability)
    } else {
        TierQuerySummary::Inconclusive(availability)
    }
}

#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq, Eq))]
#[allow(clippy::large_enum_variant)]
enum RacingQueryOutcome {
    Hit(BackendId, PrecheckHit),
    Missing,
    Inconclusive,
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
    backend: BackendId,
    hit: PrecheckHit,
    tier: crate::cache::ttl::CacheTier,
) {
    if !hit.will_update_cache(cache) {
        return;
    }

    match hit {
        PrecheckHit::Payload(data) => {
            cache.upsert_ingest(msg_id, data, backend, tier).await;
        }
        PrecheckHit::Availability(status_code) => {
            cache
                .record_backend_has_status(msg_id, status_code, backend, tier)
                .await;
        }
    }
}

async fn record_precheck_result(
    cache: &UnifiedCache,
    msg_id: MessageId<'static>,
    result: QueryResult,
) -> Option<(BackendId, PrecheckHit)> {
    match result {
        QueryResult::Found(backend, hit) => Some((backend, hit)),
        QueryResult::Missing(backend_id) => {
            cache.record_backend_missing(msg_id, backend_id).await;
            None
        }
        QueryResult::Error => None,
    }
}

async fn query_backend(
    deps: &OwnedDeps,
    backend: ArticleBackend,
    request: &RequestContext,
) -> QueryResult {
    let backend_id = backend.backend_id();
    let Some(provider) = deps.router.backend_provider(backend_id) else {
        return QueryResult::Error;
    };

    // Track pending count for load balancing
    deps.router.mark_backend_pending(backend_id);

    // Retry once on backend error (fresh connection on second attempt)
    let query_result = crate::session::retry::retry_once!(
        execute_backend_query(deps, provider, &backend, request).await,
        backend = backend_id.as_index()
    )
    .unwrap_or(QueryAttemptResult::Error);

    // Always decrement pending count when done
    deps.router.complete_command(backend_id);

    match query_result {
        QueryAttemptResult::Found(hit) => QueryResult::Found(backend_id, *hit),
        QueryAttemptResult::Missing => QueryResult::Missing(backend_id),
        QueryAttemptResult::Error => QueryResult::Error,
    }
}

/// Execute a single backend query attempt
///
/// Returns Ok(QueryAttemptResult) on successful communication (even if article not found).
/// Returns Err on connection errors (caller should retry).
async fn execute_backend_query(
    deps: &OwnedDeps,
    provider: &crate::pool::DeadpoolConnectionProvider,
    backend: &ArticleBackend,
    request: &RequestContext,
) -> Result<QueryAttemptResult, ()> {
    let backend_id = backend.backend_id();
    let Ok(mut conn) = provider.checkout_connection_guard().await else {
        return Ok(QueryAttemptResult::Error);
    };

    let mut buffer = deps.buffer_pool.acquire();

    let response = if should_sample_backend_timing() {
        backend::execute_request_classified_timed(&mut **conn, request, &mut buffer)
            .await
            .map(|(response, ttfb, send, recv)| (response, Some((ttfb, send, recv))))
    } else {
        backend::execute_request_classified(&mut **conn, request, &mut buffer)
            .await
            .map(|response| (response, None))
    };

    // Use shared backend request execution with sampled timing
    match response {
        Ok((response, timings)) => {
            let Some(status_code) = response.status_code() else {
                response.log_warnings(&buffer, "adaptive-precheck", backend_id);
                conn.fail_backend();
                return Err(());
            };
            let single_line_payload = response
                .single_line_bytes(&buffer)
                .map(crate::cache::CacheIngestResponse::from);

            let response = build_precheck_hit(
                deps,
                request,
                status_code,
                single_line_payload,
                &mut conn,
                &mut buffer,
            )
            .await?;

            let result = classify_precheck_result(deps, backend, status_code, timings, response);

            let _ = conn.complete_success(); // response received; connection healthy, return to pool
            Ok(result)
        }
        Err(_) => {
            conn.fail_backend();
            Err(())
        }
    }
}

async fn build_precheck_hit(
    deps: &OwnedDeps,
    request: &RequestContext,
    status_code: StatusCode,
    single_line_payload: Option<crate::cache::CacheIngestResponse>,
    conn: &mut crate::pool::ConnectionGuard,
    buffer: &mut crate::pool::PooledBuffer,
) -> Result<PrecheckHit, ()> {
    if request.has_response_body(status_code) {
        return read_complete_precheck_hit(deps, status_code, conn, buffer).await;
    }

    if let Some(payload) = single_line_payload {
        Ok(PrecheckHit::Payload(payload))
    } else {
        Ok(PrecheckHit::Availability(status_code))
    }
}

async fn read_complete_precheck_hit(
    deps: &OwnedDeps,
    status_code: StatusCode,
    conn: &mut crate::pool::ConnectionGuard,
    buffer: &mut crate::pool::PooledBuffer,
) -> Result<PrecheckHit, ()> {
    let mut response = deps
        .cache_articles
        .then(crate::pool::ChunkedResponse::default);

    if let Some(response) = &mut response {
        let retained =
            crate::session::backend::capture_complete_multiline_response_chunked_optional(
                conn.as_mut(),
                buffer,
                &deps.buffer_pool,
                response,
            )
            .await
            .map_err(|_| ())?;
        if !retained {
            response.clear();
            return Ok(PrecheckHit::Availability(status_code));
        }
    } else {
        crate::session::backend::observe_complete_multiline_response(conn.as_mut(), buffer)
            .await
            .map_err(|_| ())?;
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
    backend: &ArticleBackend,
    status_code: StatusCode,
    timings: Option<(u64, u64, u64)>,
    response: PrecheckHit,
) -> QueryAttemptResult {
    let backend_id = backend.backend_id();
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
            QueryAttemptResult::Found(Box::new(response))
        }
        430 => {
            deps.metrics.record_error_4xx(backend_id);
            QueryAttemptResult::Missing
        }
        400..=499 => {
            deps.metrics.record_error_4xx(backend_id);
            QueryAttemptResult::Error
        }
        500..=599 => {
            deps.metrics.record_error_5xx(backend_id);
            QueryAttemptResult::Error
        }
        _ => {
            deps.metrics.record_error(backend_id);
            QueryAttemptResult::Error
        }
    }
}

/// Query backends tier by tier, collecting results as they arrive.
async fn query_all_backends(
    deps: &OwnedDeps,
    request: &RequestContext,
    availability: &ArticleAvailability,
) -> Vec<QueryResult> {
    let mut results = Vec::with_capacity(deps.router.backend_count().get());

    for tier in deps.router.tiers() {
        let tier_results = query_tier_backends(deps, request, tier, availability).await;
        let found = tier_results
            .iter()
            .any(|result| matches!(result, QueryResult::Found(_, _)));
        let tier_exhausted = summarize_tier_results(&tier_results).is_exhausted();
        results.extend(tier_results);

        if found || !tier_exhausted {
            break;
        }
    }

    results
}

/// Query each tier racing, return first success immediately.
/// Background task updates cache with same-tier availability once the tier completes.
async fn query_all_backends_racing(
    deps: &OwnedDeps,
    request: &RequestContext,
    msg_id: &MessageId<'_>,
    initial_availability: &ArticleAvailability,
) -> RacingQueryOutcome {
    let mut results = Vec::with_capacity(deps.router.backend_count().get());

    for tier in deps.router.tiers() {
        let mut pending = spawn_backend_queries_for_tier(deps, request, tier, initial_availability);
        let tier_results_start = results.len();

        while let Some(result) = pending.next().await {
            let Ok(result) = result else {
                continue;
            };
            match result {
                QueryResult::Found(id, response) => {
                    let cache = deps.cache.clone();
                    let msg_id_owned = msg_id.to_owned();
                    tokio::spawn(async move {
                        while let Some(result) = pending.next().await {
                            let Ok(result) = result else {
                                continue;
                            };
                            let Some((backend, hit)) =
                                record_precheck_result(&cache, msg_id_owned.clone(), result).await
                            else {
                                continue;
                            };
                            let tier = crate::cache::ttl::CacheTier::new(0);
                            cache_precheck_hit(&cache, msg_id_owned.clone(), backend, hit, tier)
                                .await;
                        }
                    });
                    return RacingQueryOutcome::Hit(id, response);
                }
                QueryResult::Missing(backend_id) => {
                    deps.cache
                        .record_backend_missing(msg_id.to_owned(), backend_id)
                        .await;
                    results.push(QueryResult::Missing(backend_id));
                }
                QueryResult::Error => {
                    results.push(QueryResult::Error);
                }
            }
        }

        let tier_summary = summarize_tier_results(&results[tier_results_start..]);
        results.clear();
        if !tier_summary.is_exhausted() {
            return RacingQueryOutcome::Inconclusive;
        }
    }

    RacingQueryOutcome::Missing
}

async fn query_tier_backends(
    deps: &OwnedDeps,
    request: &RequestContext,
    tier: u8,
    availability: &ArticleAvailability,
) -> Vec<QueryResult> {
    let mut pending = spawn_backend_queries_for_tier(deps, request, tier, availability);
    let mut results = Vec::new();

    while let Some(result) = pending.next().await {
        if let Ok(result) = result {
            results.push(result);
        }
    }

    results
}

fn spawn_backend_queries_for_tier(
    deps: &OwnedDeps,
    request: &RequestContext,
    tier: u8,
    availability: &ArticleAvailability,
) -> FuturesUnordered<tokio::task::JoinHandle<QueryResult>> {
    deps.router
        .backend_ids_in_tier(tier)
        .filter_map(|id| ArticleBackend::from_availability(id, availability))
        .map(|backend| {
            let deps = deps.clone();
            let request = request.clone();
            tokio::spawn(async move { query_backend(&deps, backend, &request).await })
        })
        .collect()
}

/// Extract first found response and build availability from results.
///
/// # NNTP Semantics
/// 430 responses are authoritative (never false negatives), 2xx are not.
/// See `crate::cache::article` module docs for full explanation.
#[cfg(test)]
fn summarize(results: Vec<QueryResult>) -> (Option<(BackendId, PrecheckHit)>, ArticleAvailability) {
    let mut availability = ArticleAvailability::new();
    let mut found = None;

    for r in results {
        match r {
            QueryResult::Found(id, response) => {
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
/// Precheck backends for an article.
///
/// Queries one tier at a time. Within a tier, queries concurrently and returns
/// the first successful response immediately. Remaining same-tier backends
/// complete in background to update availability.
///
/// Skips backend queries entirely if we already have a complete article cached.
pub(crate) async fn precheck(
    deps: &PrecheckDeps<'_>,
    request: &RequestContext,
    msg_id: &MessageId<'_>,
    availability: &ArticleAvailability,
) -> Option<PrecheckResponse> {
    // Check cache first - if we have a complete article, return it immediately
    if let Some(cached) = deps.cache.get(msg_id).await
        && cached.is_complete_article()
    {
        return Some(PrecheckResponse::Cached(cached));
    }

    let owned = deps.clone_deps();
    let outcome = query_all_backends_racing(&owned, request, msg_id, availability).await;

    // Cache the found result and return it
    match outcome {
        RacingQueryOutcome::Hit(backend, data) => {
            // NOTE: We intentionally use tier 0 for articles found via racing queries.
            // Racing all backends can cause higher-tier backends to respond slightly faster,
            // incorrectly caching an article as "higher tier" (longer TTL) even if a lower-tier
            // backend also has it but responded slower. Using tier 0 conservatively ensures
            // we don't overestimate TTL. Regular routing will discover higher-tier availability.
            match data {
                PrecheckHit::Payload(response) => {
                    if owned.cache.stores_payload_responses() {
                        let tier = crate::cache::ttl::CacheTier::new(0);
                        cache_precheck_hit(
                            &owned.cache,
                            msg_id.to_owned(),
                            backend,
                            PrecheckHit::Payload(response),
                            tier,
                        )
                        .await;
                        owned.cache.get(msg_id).await.map(PrecheckResponse::Cached)
                    } else {
                        Some(PrecheckResponse::Direct(response))
                    }
                }
                PrecheckHit::Availability(status_code) => {
                    if owned.cache.records_backend_has_status() {
                        let tier = crate::cache::ttl::CacheTier::new(0);
                        cache_precheck_hit(
                            &owned.cache,
                            msg_id.to_owned(),
                            backend,
                            PrecheckHit::Availability(status_code),
                            tier,
                        )
                        .await;
                    }
                    None
                }
            }
        }
        RacingQueryOutcome::Missing | RacingQueryOutcome::Inconclusive => None,
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

        let availability = owned
            .cache
            .get(&msg_id)
            .await
            .map(|entry| entry.to_availability(owned.router.backend_count()))
            .unwrap_or_default();
        let results = query_all_backends(&owned, &request, &availability).await;
        let mut found = None;

        for result in results {
            if let Some((backend_id, data)) =
                record_precheck_result(&owned.cache, msg_id.to_owned(), result).await
            {
                if found.is_none() {
                    found = Some((backend_id, data));
                } else {
                    let tier = crate::cache::ttl::CacheTier::new(0);
                    cache_precheck_hit(&owned.cache, msg_id.to_owned(), backend_id, data, tier)
                        .await;
                }
            }
        }

        if let Some((backend_id, data)) = found {
            // NOTE: We intentionally use tier 0 for articles found via background precheck.
            // When racing backends, we don't have visibility into which tier actually has
            // the article. Using tier 0 conservatively avoids overestimating TTL.
            let tier = crate::cache::ttl::CacheTier::new(0);
            if data.will_update_cache(&owned.cache) {
                cache_precheck_hit(&owned.cache, msg_id.to_owned(), backend_id, data, tier).await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    use crate::cache::UnifiedCache;
    use crate::metrics::MetricsCollector;
    use crate::pool::BufferPool;
    use crate::router::BackendSelector;
    use crate::types::BufferSize;

    fn eligible(backend_id: BackendId) -> ArticleBackend {
        let availability = ArticleAvailability::new();
        ArticleBackend::from_availability(backend_id, &availability)
            .expect("backend should be eligible")
    }

    fn test_owned_deps(num_backends: usize) -> OwnedDeps {
        OwnedDeps {
            router: Arc::new(BackendSelector::new()),
            cache: Arc::new(UnifiedCache::memory(100, Duration::from_secs(60))),
            buffer_pool: BufferPool::new(BufferSize::try_new(4096).unwrap(), 1),
            metrics: MetricsCollector::new(num_backends),
            cache_articles: true,
        }
    }

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
        assert_eq!(avail.missing_bits(), 0);
    }

    #[tokio::test]
    async fn record_precheck_result_records_missing_fact_immediately() {
        let cache = UnifiedCache::memory(100, Duration::from_secs(60));
        let msg_id = MessageId::from_borrowed("<precheck-missing@example.com>").unwrap();

        let found = record_precheck_result(
            &cache,
            msg_id.to_owned(),
            QueryResult::Missing(BackendId::from_index(0)),
        )
        .await;

        assert!(found.is_none());
        let cached = cache
            .get(&msg_id)
            .await
            .expect("missing result should update authoritative availability");
        assert!(!cached.should_try_backend(BackendId::from_index(0)));
    }

    #[tokio::test]
    async fn record_precheck_result_does_not_record_transport_error_as_missing() {
        let cache = UnifiedCache::memory(100, Duration::from_secs(60));
        let msg_id = MessageId::from_borrowed("<precheck-error@example.com>").unwrap();

        let found = record_precheck_result(&cache, msg_id.to_owned(), QueryResult::Error).await;

        assert!(found.is_none());
        assert!(cache.get(&msg_id).await.is_none());
    }

    #[test]
    fn summarize_tier_results_treats_empty_tier_as_exhausted() {
        let summary = summarize_tier_results(&[]);

        assert!(matches!(
            summary,
            TierQuerySummary::Exhausted(availability) if availability.missing_bits() == 0
        ));
    }

    #[test]
    fn summarize_tier_results_requires_every_backend_to_miss() {
        let summary = summarize_tier_results(&[
            QueryResult::Missing(BackendId::from_index(0)),
            QueryResult::Missing(BackendId::from_index(1)),
        ]);

        let TierQuerySummary::Exhausted(availability) = summary else {
            panic!("all-missing tier should be exhausted");
        };
        assert!(availability.is_missing(BackendId::from_index(0)));
        assert!(availability.is_missing(BackendId::from_index(1)));
    }

    #[test]
    fn summarize_tier_results_treats_partial_missing_with_error_as_inconclusive() {
        let summary = summarize_tier_results(&[
            QueryResult::Missing(BackendId::from_index(0)),
            QueryResult::Error,
        ]);

        let TierQuerySummary::Inconclusive(availability) = summary else {
            panic!("tier with backend error should be inconclusive");
        };
        assert!(availability.is_missing(BackendId::from_index(0)));
        assert!(!availability.is_missing(BackendId::from_index(1)));
    }

    #[test]
    fn classify_precheck_result_records_non_430_4xx_as_error() {
        let backend_id = BackendId::from_index(0);
        let deps = test_owned_deps(1);

        let result = classify_precheck_result(
            &deps,
            &eligible(backend_id),
            StatusCode::new(412),
            Some((11, 22, 33)),
            PrecheckHit::Availability(StatusCode::new(412)),
        );

        assert!(matches!(result, QueryAttemptResult::Error));
        let snapshot = deps.metrics.snapshot(None);
        let backend = &snapshot.backend_stats[0];
        assert_eq!(backend.total_commands.get(), 1);
        assert_eq!(backend.ttfb_count.get(), 1);
        assert_eq!(backend.errors_4xx.get(), 1);
        assert_eq!(backend.errors_5xx.get(), 0);
        assert_eq!(backend.errors.get(), 1);
    }

    #[test]
    fn classify_precheck_result_records_5xx_as_error() {
        let backend_id = BackendId::from_index(0);
        let deps = test_owned_deps(1);

        let result = classify_precheck_result(
            &deps,
            &eligible(backend_id),
            StatusCode::new(502),
            None,
            PrecheckHit::Availability(StatusCode::new(502)),
        );

        assert!(matches!(result, QueryAttemptResult::Error));
        let snapshot = deps.metrics.snapshot(None);
        let backend = &snapshot.backend_stats[0];
        assert_eq!(backend.total_commands.get(), 1);
        assert_eq!(backend.errors_4xx.get(), 0);
        assert_eq!(backend.errors_5xx.get(), 1);
        assert_eq!(backend.errors.get(), 1);
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

    async fn spawn_extra_response_precheck_server() -> std::net::SocketAddr {
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
                            .write_all(
                                b"220 0 <test@example.com>\r\nbody\r\n.\r\n430 No such article\r\n",
                            )
                            .await;
                        let _ = stream.shutdown().await;
                    });
                }
            }
        });

        addr
    }

    async fn spawn_large_article_precheck_server() -> std::net::SocketAddr {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            loop {
                if let Ok((mut stream, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        let _ = stream.write_all(b"200 mock\r\n").await;
                        let mut reader = BufReader::new(stream);
                        let mut command = Vec::new();
                        loop {
                            command.clear();
                            if reader.read_until(b'\n', &mut command).await.is_err() {
                                return;
                            }
                            if command.starts_with(b"ARTICLE ") {
                                break;
                            }
                            let response = if command.starts_with(b"DATE") {
                                b"111 20260526120000\r\n".as_slice()
                            } else {
                                b"200 mock setup complete\r\n".as_slice()
                            };
                            if reader.get_mut().write_all(response).await.is_err() {
                                return;
                            }
                        }
                        let oversized_payload = vec![b'x'; 4 * 1024 * 1024 + 1];
                        let stream = reader.get_mut();
                        let _ = stream.write_all(b"220 0 <test@example.com>\r\n").await;
                        let _ = stream.write_all(&oversized_payload).await;
                        let _ = stream.write_all(b"\r\n.\r\n").await;
                    });
                }
            }
        });

        addr
    }

    fn selector_with_backend(
        addr: std::net::SocketAddr,
        max_connections: usize,
    ) -> (BackendSelector, BackendId) {
        use crate::pool::DeadpoolConnectionProvider;
        use crate::types::ServerName;

        let mut selector = BackendSelector::new();
        let backend_id = BackendId::from_index(0);
        let provider = DeadpoolConnectionProvider::new(
            "127.0.0.1".to_string(),
            addr.port(),
            "test".to_string(),
            max_connections,
            None,
            None,
        );
        selector.add_backend(
            ServerName::try_new("test-server".to_string()).unwrap(),
            provider,
            0,
        );
        (selector, backend_id)
    }

    #[tokio::test]
    async fn query_backend_returns_error_on_truncated_response_body() {
        let addr = spawn_truncated_precheck_server().await;

        let (selector, backend_id) = selector_with_backend(addr, 2);

        let deps = OwnedDeps {
            router: Arc::new(selector),
            cache: Arc::new(UnifiedCache::memory(100, Duration::from_secs(60))),
            buffer_pool: BufferPool::new(BufferSize::try_new(4096).unwrap(), 2),
            metrics: MetricsCollector::new(1),
            cache_articles: true,
        };

        let request =
            RequestContext::parse(b"ARTICLE <test@example.com>\r\n").expect("valid request line");
        let result = query_backend(&deps, eligible(backend_id), &request).await;
        assert_eq!(result, QueryResult::Error);
    }

    #[tokio::test]
    async fn query_backend_returns_error_on_extra_precheck_response() {
        let addr = spawn_extra_response_precheck_server().await;

        let (selector, backend_id) = selector_with_backend(addr, 2);

        let deps = OwnedDeps {
            router: Arc::new(selector),
            cache: Arc::new(UnifiedCache::memory(100, Duration::from_secs(60))),
            buffer_pool: BufferPool::new(BufferSize::try_new(4096).unwrap(), 2),
            metrics: MetricsCollector::new(1),
            cache_articles: true,
        };

        let request =
            RequestContext::parse(b"ARTICLE <test@example.com>\r\n").expect("valid request line");
        let result = query_backend(&deps, eligible(backend_id), &request).await;
        assert_eq!(result, QueryResult::Error);
    }

    #[tokio::test]
    async fn query_backend_treats_oversized_precheck_hit_as_availability() {
        let addr = spawn_large_article_precheck_server().await;
        let (selector, backend_id) = selector_with_backend(addr, 1);

        let deps = OwnedDeps {
            router: Arc::new(selector),
            cache: Arc::new(UnifiedCache::memory(100, Duration::from_secs(60))),
            buffer_pool: BufferPool::new(BufferSize::try_new(65536).unwrap(), 2),
            metrics: MetricsCollector::new(1),
            cache_articles: true,
        };

        let request =
            RequestContext::parse(b"ARTICLE <test@example.com>\r\n").expect("valid request line");
        let result = query_backend(&deps, eligible(backend_id), &request).await;

        assert_eq!(
            result,
            QueryResult::Found(backend_id, PrecheckHit::Availability(StatusCode::new(220)))
        );
    }

    #[tokio::test]
    async fn cache_precheck_availability_hit_does_not_persist_positive_entry() {
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
