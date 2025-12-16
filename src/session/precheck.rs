//! Adaptive precheck for concurrent backend queries.
//!
//! Queries all backends simultaneously, uses first successful response.
//! Results cached to avoid redundant queries on future requests.

use std::sync::Arc;

use tokio::io::AsyncReadExt;

use crate::cache::{ArticleAvailability, ArticleCache, ArticleEntry};
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
    pub cache: &'a Arc<ArticleCache>,
    pub buffer_pool: &'a BufferPool,
    pub metrics: Option<&'a MetricsCollector>,
    pub cache_articles: bool,
}

#[derive(Clone)]
struct OwnedDeps {
    router: Arc<BackendSelector>,
    cache: Arc<ArticleCache>,
    buffer_pool: BufferPool,
    metrics: Option<MetricsCollector>,
    cache_articles: bool,
}

impl<'a> PrecheckDeps<'a> {
    fn to_owned(&self) -> OwnedDeps {
        OwnedDeps {
            router: Arc::clone(self.router),
            cache: Arc::clone(self.cache),
            buffer_pool: self.buffer_pool.clone(),
            metrics: self.metrics.cloned(),
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
    let Ok(mut conn) = provider.get_pooled_connection().await else {
        return QueryResult::Error(backend_id);
    };

    let mut buffer = deps.buffer_pool.acquire().await;

    // Use shared backend command execution
    let cmd_response = match backend::send_command(&mut *conn, command, &mut buffer).await {
        Ok(r) => r,
        Err(_) => return QueryResult::Error(backend_id),
    };

    let Some(status_code) = cmd_response.status_code() else {
        return QueryResult::Error(backend_id);
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

    match status_code {
        220..=223 => {
            let data = if deps.cache_articles || !multiline {
                response
            } else {
                format!("{status_code}\r\n").into_bytes()
            };
            QueryResult::Found(backend_id, data)
        }
        430 => {
            if let Some(m) = &deps.metrics {
                m.record_command(backend_id);
                m.record_error_4xx(backend_id);
            }
            QueryResult::Missing(backend_id)
        }
        _ => QueryResult::Error(backend_id),
    }
}

async fn query_all_backends(deps: &OwnedDeps, command: &str, multiline: bool) -> Vec<QueryResult> {
    let tasks: Vec<_> = (0..deps.router.backend_count().get())
        .map(BackendId::from_index)
        .map(|id| {
            let deps = deps.clone();
            let cmd = command.to_string();
            tokio::spawn(async move { query_backend(&deps, id, &cmd, multiline).await })
        })
        .collect();

    futures::future::join_all(tasks)
        .await
        .into_iter()
        .filter_map(Result::ok)
        .collect()
}

/// Extract first found response and build availability from results.
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
async fn cache_results(
    cache: &ArticleCache,
    msg_id: &MessageId<'_>,
    found: Option<(BackendId, Vec<u8>)>,
    availability: ArticleAvailability,
) -> Option<ArticleEntry> {
    let has_article = found.is_some();

    if let Some((backend_id, data)) = found {
        cache.upsert(msg_id.to_owned(), data, backend_id).await;
    }
    cache
        .sync_availability(msg_id.to_owned(), &availability)
        .await;

    // Return cached entry if we found something
    if has_article {
        cache.get(msg_id).await
    } else {
        None
    }
}

/// Precheck all backends for an article.
///
/// Queries concurrently, stores results in cache.
/// Returns Some(entry) if any backend had it, None if all 430'd.
pub async fn precheck(
    deps: &PrecheckDeps<'_>,
    command: &str,
    msg_id: &MessageId<'_>,
    multiline: bool,
) -> Option<ArticleEntry> {
    let owned = deps.to_owned();
    let results = query_all_backends(&owned, command, multiline).await;
    let (found, availability) = summarize(results);
    cache_results(&owned.cache, msg_id, found, availability).await
}

/// Spawn background precheck. Results go to cache only.
pub fn spawn_background_precheck(
    deps: PrecheckDeps<'_>,
    command: String,
    msg_id: MessageId<'static>,
) {
    let owned = deps.to_owned();
    tokio::spawn(async move {
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
