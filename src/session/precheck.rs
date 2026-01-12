//! Adaptive precheck for concurrent backend queries.
//!
//! Queries all backends simultaneously, uses first successful response.
//! Results cached to avoid redundant queries on future requests.

use std::sync::Arc;

use tokio::io::AsyncReadExt;

use crate::cache::{ArticleAvailability, ArticleEntry, UnifiedCache};
use crate::metrics::MetricsCollector;
use crate::pool::BufferPool;
use crate::router::BackendSelector;
use crate::session::backend;
use crate::types::{BackendId, MessageId};

/// Result of querying a backend for an article.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryResult {
    Found(BackendId, Vec<u8>),
    Missing(BackendId),
    Error(BackendId),
}

/// Shared dependencies for precheck operations.
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

impl<'a> PrecheckDeps<'a> {
    fn to_owned(&self) -> OwnedDeps {
        OwnedDeps {
            router: Arc::clone(self.router),
            cache: Arc::clone(self.cache),
            buffer_pool: self.buffer_pool.clone(),
            metrics: self.metrics.clone(),
            cache_articles: self.cache_articles,
        }
    }
}

async fn query_backend(
    deps: &OwnedDeps,
    backend_id: BackendId,
    command: &str,
    multiline: bool,
) -> QueryResult {
    let Some(provider) = deps.router.backend_provider(backend_id) else {
        return QueryResult::Error(backend_id);
    };

    // Track pending count for load balancing
    deps.router.mark_backend_pending(backend_id);

    // Functional retry: try once, on error retry with fresh connection
    let result = execute_backend_query(deps, provider, backend_id, command, multiline).await;

    let query_result = match result {
        Ok(query_result) => query_result,
        Err(_first_error) => {
            tracing::debug!(
                backend = backend_id.as_index(),
                "Stale connection detected, retrying with fresh connection"
            );

            // Retry once with fresh connection
            execute_backend_query(deps, provider, backend_id, command, multiline)
                .await
                .unwrap_or(QueryResult::Error(backend_id))
        }
    };

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
    command: &str,
    multiline: bool,
) -> Result<QueryResult, ()> {
    let Ok(mut conn) = provider.get_pooled_connection().await else {
        return Ok(QueryResult::Error(backend_id));
    };

    let mut buffer = deps.buffer_pool.acquire().await;

    // Use shared backend command execution with timing
    match backend::send_command_timed(&mut *conn, command, &mut buffer).await {
        Ok((cmd_response, ttfb, send, recv)) => {
            let Some(status_code) = cmd_response.status_code() else {
                // Invalid response - drop connection
                crate::pool::remove_from_pool(conn);
                return Err(());
            };

            // For multiline responses, read remaining data
            let mut response = buffer[..cmd_response.bytes_read].to_vec();
            if multiline && cmd_response.is_multiline {
                while !response.ends_with(b".\r\n") {
                    match conn.as_mut().read(buffer.as_mut_slice()).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => response.extend_from_slice(&buffer[..n]),
                    }
                }
            }

            Ok(match status_code {
                220..=223 => {
                    // Record successful command and timing
                    tracing::debug!(
                        backend = backend_id.as_index(),
                        ttfb_us = ttfb,
                        "precheck recording command to metrics"
                    );
                    deps.metrics.record_command(backend_id);
                    deps.metrics.record_ttfb_micros(backend_id, ttfb);
                    deps.metrics.record_send_recv_micros(backend_id, send, recv);
                    let data = if deps.cache_articles || !multiline {
                        response
                    } else {
                        format!("{status_code}\r\n").into_bytes()
                    };
                    QueryResult::Found(backend_id, data)
                }
                430 => {
                    deps.metrics.record_command(backend_id);
                    deps.metrics.record_ttfb_micros(backend_id, ttfb);
                    deps.metrics.record_send_recv_micros(backend_id, send, recv);
                    deps.metrics.record_error_4xx(backend_id);
                    QueryResult::Missing(backend_id)
                }
                _ => QueryResult::Error(backend_id),
            })
        }
        Err(_) => {
            // Connection error - remove stale connection from pool
            crate::pool::remove_from_pool(conn);
            Err(())
        }
    }
}

/// Query all backends, collecting results as they arrive
async fn query_all_backends(deps: &OwnedDeps, command: &str, multiline: bool) -> Vec<QueryResult> {
    use futures::StreamExt;

    let tasks: Vec<_> = (0..deps.router.backend_count().get())
        .map(BackendId::from_index)
        .map(|id| {
            let deps = deps.clone();
            let cmd = command.to_string();
            tokio::spawn(async move { query_backend(&deps, id, &cmd, multiline).await })
        })
        .collect();

    // Race all backends - collect results as they complete (fastest first)
    let task_count = tasks.len();
    futures::stream::iter(tasks)
        .buffer_unordered(task_count)
        .filter_map(|result| async move { result.ok() })
        .collect()
        .await
}

/// Query all backends racing, return first success immediately.
/// Background task updates cache with full availability once all backends complete.
async fn query_all_backends_racing(
    deps: &OwnedDeps,
    command: &str,
    msg_id: &MessageId<'_>,
    multiline: bool,
) -> Option<(BackendId, Vec<u8>)> {
    use futures::StreamExt;

    let tasks: Vec<_> = (0..deps.router.backend_count().get())
        .map(BackendId::from_index)
        .map(|id| {
            let deps = deps.clone();
            let cmd = command.to_string();
            tokio::spawn(async move { query_backend(&deps, id, &cmd, multiline).await })
        })
        .collect();

    let backend_count = deps.router.backend_count();

    // Process results as they complete, return first success immediately
    let mut pending = futures::stream::iter(tasks)
        .buffer_unordered(backend_count.get())
        .filter_map(|result| async move { result.ok() })
        .boxed();

    let mut results = Vec::with_capacity(backend_count.get());
    let mut first_found = None;

    // Collect results until we find a success
    while let Some(result) = pending.next().await {
        match &result {
            QueryResult::Found(id, response) if first_found.is_none() => {
                first_found = Some((*id, response.clone()));
                results.push(result);

                // Spawn background task to complete remaining backends and update cache
                let cache = deps.cache.clone();
                let msg_id_owned = msg_id.to_owned();
                tokio::spawn(async move {
                    // Collect remaining results
                    while let Some(result) = pending.next().await {
                        results.push(result);
                    }
                    // Build availability from all results and sync to cache
                    let (_, availability) = summarize(results);
                    cache.sync_availability(msg_id_owned, &availability).await;
                });
                return first_found;
            }
            _ => {
                results.push(result);
            }
        }
    }

    // No success found - all backends returned 430
    None
}

/// Extract first found response and build availability from results.
///
/// # NNTP Semantics
/// 430 responses are authoritative (never false negatives), 2xx are not.
/// See `crate::cache::article` module docs for full explanation.
fn summarize(results: Vec<QueryResult>) -> (Option<(BackendId, Vec<u8>)>, ArticleAvailability) {
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
            QueryResult::Error(_) => {}
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
    command: &str,
    msg_id: &MessageId<'_>,
    multiline: bool,
) -> Option<ArticleEntry> {
    // Check cache first - if we have a complete article, return it immediately
    if let Some(cached) = deps.cache.get(msg_id).await
        && cached.is_complete_article()
    {
        return Some(cached);
    }

    let owned = deps.to_owned();
    let found = query_all_backends_racing(&owned, command, msg_id, multiline).await;

    // Cache the found result and return it
    if let Some((backend_id, data)) = found {
        owned
            .cache
            .upsert(msg_id.to_owned(), data, backend_id)
            .await;
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
    command: String,
    msg_id: MessageId<'static>,
) {
    let owned = deps.to_owned();
    tokio::spawn(async move {
        // Check cache first - if we have a complete article, no need to query backends
        if let Some(cached) = owned.cache.get(&msg_id).await
            && cached.is_complete_article()
        {
            // Already have full article cached - nothing to do
            return;
        }

        let results = query_all_backends(&owned, &command, false).await;
        let (found, availability) = summarize(results);

        if let Some((backend_id, data)) = found {
            owned
                .cache
                .upsert(msg_id.to_owned(), data, backend_id)
                .await;
        }
        owned.cache.sync_availability(msg_id, &availability).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_finds_first() {
        let results = vec![
            QueryResult::Missing(BackendId::from_index(0)),
            QueryResult::Found(BackendId::from_index(1), b"first".to_vec()),
            QueryResult::Found(BackendId::from_index(2), b"second".to_vec()),
        ];
        let (found, avail) = summarize(results);
        assert_eq!(found, Some((BackendId::from_index(1), b"first".to_vec())));
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
}
