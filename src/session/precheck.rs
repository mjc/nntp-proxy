//! Adaptive precheck for concurrent backend queries.
//!
//! Queries all backends simultaneously, uses first successful response.
//! Results cached to avoid redundant queries on future requests.

use std::sync::Arc;

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::cache::ArticleCache;
use crate::metrics::MetricsCollector;
use crate::pool::BufferPool;
use crate::router::BackendSelector;
use crate::types::{BackendId, BackendToClientBytes, ClientId, MessageId};

/// Result of querying a backend for an article.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryResult {
    /// Backend has the article (220-223 response)
    Found(BackendId, Vec<u8>),
    /// Backend doesn't have it (430 response)
    Missing(BackendId),
    /// Query failed (connection error, protocol error, etc.)
    Error(BackendId),
}

/// Whether to read multiline responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseMode {
    SingleLine,
    Multiline,
}

/// Shared dependencies for precheck operations.
pub struct PrecheckDeps<'a> {
    pub router: &'a Arc<BackendSelector>,
    pub cache: &'a Arc<ArticleCache>,
    pub buffer_pool: &'a BufferPool,
    pub metrics: Option<&'a MetricsCollector>,
    pub cache_articles: bool,
}

/// Owned version for spawned tasks.
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
    mode: ResponseMode,
) -> QueryResult {
    // Get connection
    let Some(provider) = deps.router.backend_provider(backend_id) else {
        return QueryResult::Error(backend_id);
    };
    let Ok(mut conn) = provider.get_pooled_connection().await else {
        return QueryResult::Error(backend_id);
    };

    // Send command
    let mut buffer = deps.buffer_pool.acquire().await;
    if conn.as_mut().write_all(command.as_bytes()).await.is_err() {
        return QueryResult::Error(backend_id);
    }

    // Read response
    let Ok(n) = buffer.read_from(&mut *conn).await else {
        return QueryResult::Error(backend_id);
    };
    if n == 0 {
        return QueryResult::Error(backend_id);
    }

    let mut response = buffer[..n].to_vec();

    // Parse status code
    let parsed = crate::protocol::NntpResponse::parse(&response);
    let Some(status_code) = parsed.status_code().map(|c| c.as_u16()) else {
        return QueryResult::Error(backend_id);
    };

    // Read multiline if needed
    if mode == ResponseMode::Multiline && parsed.is_multiline() {
        while !response.ends_with(b".\r\n") {
            match conn.as_mut().read(buffer.as_mut_slice()).await {
                Ok(0) | Err(_) => break,
                Ok(n) => response.extend_from_slice(&buffer[..n]),
            }
        }
    }

    // Classify response
    match status_code {
        220..=223 => {
            // For availability-only mode, just cache status code
            let cache_data = if deps.cache_articles || mode == ResponseMode::SingleLine {
                response
            } else {
                format!("{status_code}\r\n").into_bytes()
            };
            QueryResult::Found(backend_id, cache_data)
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

async fn query_backends(
    deps: &OwnedDeps,
    backends: impl Iterator<Item = BackendId>,
    command: &str,
    mode: ResponseMode,
) -> Vec<QueryResult> {
    let tasks: Vec<_> = backends
        .map(|id| {
            let deps = deps.clone();
            let cmd = command.to_string();
            tokio::spawn(async move { query_backend(&deps, id, &cmd, mode).await })
        })
        .collect();

    futures::future::join_all(tasks)
        .await
        .into_iter()
        .filter_map(Result::ok)
        .collect()
}

fn first_found(results: &[QueryResult]) -> Option<(BackendId, &[u8])> {
    results.iter().find_map(|r| match r {
        QueryResult::Found(id, data) => Some((*id, data.as_slice())),
        _ => None,
    })
}

async fn cache_results(cache: &ArticleCache, msg_id: &MessageId<'_>, results: &[QueryResult]) {
    for result in results {
        match result {
            QueryResult::Found(id, data) => {
                cache.upsert(msg_id.to_owned(), data.clone(), *id).await;
            }
            QueryResult::Missing(id) => {
                cache.record_backend_missing(msg_id.to_owned(), *id).await;
            }
            QueryResult::Error(_) => {}
        }
    }
}

/// Spawn background precheck. Results go to cache only.
pub fn spawn_background_stat_precheck(deps: PrecheckDeps<'_>, command: String, msg_id: MessageId<'static>) {
    let owned = deps.to_owned();
    tokio::spawn(async move {
        let backend_count = owned.router.backend_count();
        let backends: Vec<_> = owned
            .cache
            .get(&msg_id)
            .await
            .map(|entry| entry.available_backends(backend_count))
            .unwrap_or_else(|| {
                (0..backend_count.get())
                    .map(BackendId::from_index)
                    .collect()
            });

        if backends.is_empty() {
            return;
        }

        let results = query_backends(
            &owned,
            backends.into_iter(),
            &command,
            ResponseMode::SingleLine,
        )
        .await;
        cache_results(&owned.cache, &msg_id, &results).await;
    });
}

pub async fn handle_stat_precheck(
    deps: &PrecheckDeps<'_>,
    client_id: ClientId,
    command: &str,
    msg_id: &MessageId<'_>,
    client: &mut tokio::net::tcp::WriteHalf<'_>,
) -> Result<BackendId> {
    let owned = deps.to_owned();
    let backends = (0..owned.router.backend_count().get()).map(BackendId::from_index);
    let results = query_backends(&owned, backends, command, ResponseMode::SingleLine).await;
    cache_results(&owned.cache, msg_id, &results).await;

    match first_found(&results) {
        Some((backend_id, response)) => {
            client.write_all(response).await?;
            deps.router.complete_command(backend_id);
            Ok(backend_id)
        }
        None => {
            client.write_all(crate::protocol::NO_SUCH_ARTICLE).await?;
            let backend_id = deps.router.route_command(client_id, command)?;
            deps.router.complete_command(backend_id);
            Ok(backend_id)
        }
    }
}

pub async fn handle_head_precheck(
    deps: &PrecheckDeps<'_>,
    client_id: ClientId,
    command: &str,
    msg_id: &MessageId<'_>,
    client: &mut tokio::net::tcp::WriteHalf<'_>,
    bytes_sent: &mut BackendToClientBytes,
) -> Result<BackendId> {
    let owned = deps.to_owned();
    let backends = (0..owned.router.backend_count().get()).map(BackendId::from_index);
    let results = query_backends(&owned, backends, command, ResponseMode::Multiline).await;
    cache_results(&owned.cache, msg_id, &results).await;

    match first_found(&results) {
        Some((backend_id, response)) => {
            *bytes_sent = bytes_sent.add(response.len());
            client.write_all(response).await?;
            deps.router.complete_command(backend_id);
            Ok(backend_id)
        }
        None => {
            *bytes_sent = bytes_sent.add(crate::protocol::NO_SUCH_ARTICLE.len());
            client.write_all(crate::protocol::NO_SUCH_ARTICLE).await?;
            let backend_id = deps.router.route_command(client_id, command)?;
            deps.router.complete_command(backend_id);
            Ok(backend_id)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_result_variants() {
        let found = QueryResult::Found(BackendId::from_index(0), b"220 ok\r\n".to_vec());
        let missing = QueryResult::Missing(BackendId::from_index(1));
        let error = QueryResult::Error(BackendId::from_index(2));

        assert!(matches!(found, QueryResult::Found(_, _)));
        assert!(matches!(missing, QueryResult::Missing(_)));
        assert!(matches!(error, QueryResult::Error(_)));
    }

    #[test]
    fn first_found_selects_first() {
        let results = vec![
            QueryResult::Missing(BackendId::from_index(0)),
            QueryResult::Found(BackendId::from_index(1), b"first".to_vec()),
            QueryResult::Found(BackendId::from_index(2), b"second".to_vec()),
        ];

        let (id, data) = first_found(&results).unwrap();
        assert_eq!(id, BackendId::from_index(1));
        assert_eq!(data, b"first");
    }

    #[test]
    fn first_found_none_when_all_missing() {
        let results = vec![
            QueryResult::Missing(BackendId::from_index(0)),
            QueryResult::Error(BackendId::from_index(1)),
        ];

        assert!(first_found(&results).is_none());
    }

    #[test]
    fn first_found_none_when_empty() {
        assert!(first_found(&[]).is_none());
    }
}
