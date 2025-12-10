//! Adaptive precheck for article availability
//!
//! This module implements parallel STAT/HEAD queries across all backends to quickly
//! determine which backend has an article before retrieving it.

use crate::cache::ArticleCache;
use crate::router::BackendSelector;
use crate::types::{BackendId, BackendToClientBytes, MessageId};
use anyhow::Result;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::WriteHalf;
use tracing::debug;

/// Calculate adaptive timeout based on average TTFB across backends
///
/// Returns TTFB³, with a minimum of 5 seconds.
/// This prevents slow backends from blocking while allowing reasonable TTFB detection.
pub(crate) fn calculate_adaptive_timeout(
    metrics: Option<&crate::metrics::MetricsCollector>,
) -> std::time::Duration {
    metrics
        .and_then(|m| {
            let snapshot = m.snapshot(None);
            let avg_ttfb_ms: f64 = snapshot
                .backend_stats
                .iter()
                .filter_map(|b| b.average_ttfb_ms())
                .sum::<f64>()
                / snapshot.backend_stats.len().max(1) as f64;

            (avg_ttfb_ms > 0.0).then(|| {
                let ttfb_cubed = (avg_ttfb_ms / 1000.0).powi(3);
                std::time::Duration::from_secs_f64(ttfb_cubed)
            })
        })
        .unwrap_or(std::time::Duration::from_secs(5))
        .max(std::time::Duration::from_secs(5))
}

/// Handle STAT precheck: query all backends in parallel, return first success
pub async fn handle_stat_precheck(
    router: Arc<BackendSelector>,
    buffer_pool: &crate::pool::BufferPool,
    metrics: Option<&crate::metrics::MetricsCollector>,
    command: &str,
    _msg_id: &MessageId<'_>,
    client_write: &mut WriteHalf<'_>,
) -> Result<BackendId> {
    enum StatResult {
        Has(BackendId, Vec<u8>), // 220-223: Article exists
        #[allow(dead_code)]
        Missing(BackendId, Vec<u8>), // 430 or other error
    }

    // Calculate adaptive timeout: TTFB * 2
    let timeout = calculate_adaptive_timeout(metrics);

    // Query all backends concurrently
    let backend_queries = (0..router.backend_count().get())
        .map(BackendId::from_index)
        .map(|backend_id| {
            let (router, buffer_pool, command) =
                (router.clone(), buffer_pool.clone(), command.to_string());

            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;

                // Get connection and send STAT command (no timeout on write)
                let provider = router.backend_provider(backend_id)?;
                let mut conn = provider.get_pooled_connection().await.ok()?;
                let mut buffer = buffer_pool.acquire().await;

                // Send command without timeout
                conn.as_mut().write_all(command.as_bytes()).await.ok()?;

                // Timeout only on first byte read (TTFB²)
                let read_result = tokio::time::timeout(timeout, async {
                    let n = buffer.read_from(&mut *conn).await.ok()?;
                    if n == 0 {
                        return None;
                    }
                    Some((n, buffer[..n].to_vec()))
                })
                .await;

                let (_bytes_read, response_bytes) = match read_result {
                    Ok(Some(data)) => data,
                    Ok(None) => return None,
                    Err(_) => {
                        debug!(
                            "STAT query to backend {} timed out waiting for first byte after {:?}",
                            backend_id.as_index(),
                            timeout
                        );
                        return None;
                    }
                };

                let response = crate::protocol::NntpResponse::parse(&response_bytes);
                let status_code = response.status_code()?.as_u16();

                // 220-223 = article exists, 430 = not found
                if (220..=223).contains(&status_code) {
                    Some(StatResult::Has(backend_id, response_bytes))
                } else {
                    Some(StatResult::Missing(backend_id, response_bytes))
                }
            })
        })
        .collect::<Vec<_>>();

    // Wait for first successful response
    let mut first_success: Option<(BackendId, Vec<u8>)> = None;

    for query in backend_queries {
        if let Ok(Some(result)) = query.await {
            match result {
                StatResult::Has(backend_id, response) => {
                    first_success = Some((backend_id, response));
                    break;
                }
                StatResult::Missing(..) => {
                    // Keep waiting for a successful response
                    continue;
                }
            }
        }
    }

    // Send response to client
    match first_success {
        Some((backend_id, response)) => {
            client_write.write_all(&response).await?;
            router.complete_command(backend_id);
            Ok(backend_id)
        }
        None => {
            // All backends returned 430 or failed
            client_write
                .write_all(crate::protocol::NO_SUCH_ARTICLE)
                .await?;
            let backend_id = router.route_command(crate::types::ClientId::new(), command)?;
            router.complete_command(backend_id);
            Ok(backend_id)
        }
    }
}

/// Handle HEAD precheck: query all backends in parallel, cache and return first success
#[allow(clippy::too_many_arguments)]
pub async fn handle_head_precheck(
    router: Arc<BackendSelector>,
    buffer_pool: &crate::pool::BufferPool,
    _cache: &Arc<ArticleCache>,
    metrics: Option<&crate::metrics::MetricsCollector>,
    cache_articles: bool,
    command: &str,
    _msg_id: &MessageId<'_>,
    client_write: &mut WriteHalf<'_>,
    backend_to_client_bytes: &mut BackendToClientBytes,
) -> Result<BackendId> {
    enum HeadResult {
        Has(BackendId, Vec<u8>), // 221: Headers retrieved
        #[allow(dead_code)]
        Missing(BackendId, Vec<u8>), // 430 or other error
    }

    // Calculate adaptive timeout: TTFB * 2
    let timeout = calculate_adaptive_timeout(metrics);

    // Query all backends concurrently
    let backend_queries = (0..router.backend_count().get())
        .map(BackendId::from_index)
        .map(|backend_id| {
            let (router, buffer_pool, command, cache_articles) = (
                router.clone(),
                buffer_pool.clone(),
                command.to_string(),
                cache_articles,
            );

            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};

                // Get connection and send HEAD command (no timeout on write)
                let provider = router.backend_provider(backend_id)?;
                let mut conn = provider.get_pooled_connection().await.ok()?;
                let mut buffer = buffer_pool.acquire().await;

                // Send command without timeout
                conn.as_mut().write_all(command.as_bytes()).await.ok()?;

                // Timeout only on first byte read (TTFB²)
                let read_result = tokio::time::timeout(timeout, async {
                    let n = buffer.read_from(&mut *conn).await.ok()?;
                    if n == 0 {
                        return None;
                    }
                    Some((n, buffer[..n].to_vec()))
                })
                .await;

                let (_bytes_read, mut response_bytes) = match read_result {
                    Ok(Some(data)) => data,
                    Ok(None) => return None,
                    Err(_) => {
                        debug!(
                            "HEAD query to backend {} timed out waiting for first byte after {:?}",
                            backend_id.as_index(),
                            timeout
                        );
                        return None;
                    }
                };

                let response = crate::protocol::NntpResponse::parse(&response_bytes);
                let status_code = response.status_code()?.as_u16();

                // For HEAD with caching, read full multiline response
                if status_code == 221 && cache_articles {
                    // Read until terminator
                    let mut accumulated = response_bytes.clone();
                    loop {
                        let mut chunk = vec![0u8; 8192];
                        match conn.as_mut().read(&mut chunk).await {
                            Ok(0) => break,
                            Ok(n) => {
                                accumulated.extend_from_slice(&chunk[..n]);
                                if accumulated.ends_with(b"\r\n.\r\n") {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    response_bytes = accumulated;
                }

                // 221 = headers exist, 430 = not found
                if status_code == 221 {
                    Some(HeadResult::Has(backend_id, response_bytes))
                } else {
                    Some(HeadResult::Missing(backend_id, response_bytes))
                }
            })
        })
        .collect::<Vec<_>>();

    // Wait for first successful response
    let mut first_success: Option<(BackendId, Vec<u8>)> = None;

    for query in backend_queries {
        if let Ok(Some(result)) = query.await {
            match result {
                HeadResult::Has(backend_id, response) => {
                    first_success = Some((backend_id, response));
                    break;
                }
                HeadResult::Missing(..) => {
                    // Keep waiting for a successful response
                    continue;
                }
            }
        }
    }

    // Send response to client
    match first_success {
        Some((backend_id, response)) => {
            client_write.write_all(&response).await?;
            *backend_to_client_bytes = backend_to_client_bytes.add(response.len());
            router.complete_command(backend_id);
            Ok(backend_id)
        }
        None => {
            // All backends returned 430 or failed
            let response = crate::protocol::NO_SUCH_ARTICLE;
            client_write.write_all(response).await?;
            *backend_to_client_bytes = backend_to_client_bytes.add(response.len());
            let backend_id = router.route_command(crate::types::ClientId::new(), command)?;
            router.complete_command(backend_id);
            Ok(backend_id)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_adaptive_timeout_no_metrics() {
        let timeout = calculate_adaptive_timeout(None);
        assert_eq!(timeout, std::time::Duration::from_secs(5));
    }

    #[test]
    fn test_calculate_adaptive_timeout_minimum() {
        // Even with 0 TTFB, should return minimum 5s
        let timeout = calculate_adaptive_timeout(None);
        assert!(timeout >= std::time::Duration::from_secs(5));
    }
}
