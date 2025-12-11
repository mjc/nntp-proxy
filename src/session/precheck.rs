//! Adaptive precheck utilities
//!
//! This module provides utilities for adaptive prechecking - querying multiple
//! backends concurrently to find article availability before committing to a
//! single backend.
//!
//! # Architecture
//!
//! Prechecking allows the proxy to:
//! 1. Query all backends concurrently for article availability
//! 2. Cache results to avoid redundant queries
//! 3. Return the first successful response to the client
//!
//! This is particularly useful for STAT and HEAD commands where we want to
//! quickly determine which backend has an article.

use std::sync::Arc;

use anyhow::Result;
use tokio::io::AsyncWriteExt;
use tracing::debug;

use crate::cache::ArticleCache;
use crate::metrics::MetricsCollector;
use crate::pool::BufferPool;
use crate::router::BackendSelector;
use crate::session::backend;
use crate::types::{BackendId, BackendToClientBytes, MessageId};

/// Calculate adaptive timeout based on average TTFB across backends
///
/// Returns a timeout based on TTFB³, with a minimum of 5 seconds.
/// This prevents slow backends from blocking while allowing reasonable TTFB detection.
pub fn calculate_adaptive_timeout(metrics: Option<&MetricsCollector>) -> std::time::Duration {
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

/// Dependencies needed for precheck operations
pub struct PrecheckContext {
    pub router: Arc<BackendSelector>,
    pub cache: Arc<ArticleCache>,
    pub buffer_pool: BufferPool,
    pub metrics: Option<MetricsCollector>,
    pub cache_articles: bool,
}

/// Result of a single backend precheck query
enum PrecheckResult {
    /// Backend has the article (2xx response)
    Has(BackendId, Vec<u8>),
    /// Backend doesn't have the article (430 or error)
    Missing(BackendId, Vec<u8>),
}

/// Spawn a background STAT precheck to update cache
///
/// Only checks backends that haven't been tried yet (not marked as missing or having).
/// This keeps cache fresh while avoiding duplicate queries to backends we already know about.
pub fn spawn_background_stat_precheck(
    ctx: PrecheckContext,
    command: String,
    msg_id: MessageId<'static>,
    client_addr: crate::types::ClientAddress,
) {
    let client_addr = *client_addr.as_socket_addr();
    tokio::spawn(async move {
        // Get current cache entry to see which backends to check
        let backends_to_check: Vec<_> = if let Some(entry) = ctx.cache.get(&msg_id).await {
            // Only check backends we haven't tried yet
            entry.available_backends(ctx.router.backend_count())
        } else {
            // Cache was evicted, check all backends
            (0..ctx.router.backend_count().get())
                .map(BackendId::from_index)
                .collect()
        };

        // Check only untried backends
        for backend_id in backends_to_check {
            let provider = match ctx.router.backend_provider(backend_id) {
                Some(p) => p,
                None => continue,
            };

            let mut conn = match provider.get_pooled_connection().await {
                Ok(c) => c,
                Err(_) => continue,
            };

            let mut buffer = ctx.buffer_pool.acquire().await;

            let result = backend::send_command_and_read_first_chunk(
                &mut *conn,
                &command,
                backend_id,
                client_addr,
                &mut buffer,
            )
            .await;

            if let Ok((n, response_code, _, _, _, _)) = result
                && let Some(code_num) = response_code.status_code().map(|c| c.as_u16())
            {
                let response = buffer[..n].to_vec();

                match code_num {
                    430 => {
                        ctx.cache
                            .record_backend_missing(msg_id.clone(), backend_id, response)
                            .await;
                        if let Some(ref m) = ctx.metrics {
                            m.record_command(backend_id);
                            m.record_error_4xx(backend_id);
                        }
                    }
                    220..=223 => {
                        ctx.cache.upsert(msg_id.clone(), response, backend_id).await;
                    }
                    _ => {}
                }
            }
        }
    });
}

/// Handle STAT command with adaptive prechecking
///
/// Queries all backends concurrently for article availability, updates cache
/// with all results, and returns the first successful response to the client.
///
/// This eliminates cache race conditions by:
/// 1. Spawning concurrent STAT queries to ALL backends
/// 2. Waiting for ALL queries to complete
/// 3. Updating cache serially with all results
/// 4. Returning first successful response to client
pub async fn handle_stat_precheck(
    ctx: &PrecheckContext,
    client_id: crate::types::ClientId,
    command: &str,
    msg_id: &MessageId<'_>,
    client_write: &mut tokio::net::tcp::WriteHalf<'_>,
) -> Result<BackendId> {
    let timeout = calculate_adaptive_timeout(ctx.metrics.as_ref());

    // Query all backends concurrently
    let backend_queries = (0..ctx.router.backend_count().get())
        .map(BackendId::from_index)
        .map(|backend_id| {
            let (router, buffer_pool, metrics, command) = (
                ctx.router.clone(),
                ctx.buffer_pool.clone(),
                ctx.metrics.clone(),
                command.to_string(),
            );

            tokio::spawn(async move {
                // Get connection and send STAT command
                let provider = router.backend_provider(backend_id)?;
                let mut conn = provider.get_pooled_connection().await.ok()?;
                let mut buffer = buffer_pool.acquire().await;

                // Send command without timeout
                conn.as_mut().write_all(command.as_bytes()).await.ok()?;

                // Timeout only on first byte read
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
                            backend = backend_id.as_index(),
                            timeout_ms = timeout.as_millis(),
                            "STAT query timed out waiting for first byte"
                        );
                        return None;
                    }
                };

                // Parse response code
                let response_code = crate::protocol::NntpResponse::parse(&response_bytes);
                let status_code = response_code.status_code()?.as_u16();

                Some(match status_code {
                    220..=223 => PrecheckResult::Has(backend_id, response_bytes),
                    430 => {
                        if let Some(m) = metrics.as_ref() {
                            m.record_command(backend_id);
                            m.record_error_4xx(backend_id);
                        }
                        PrecheckResult::Missing(backend_id, response_bytes)
                    }
                    _ => PrecheckResult::Missing(backend_id, response_bytes),
                })
            })
        })
        .collect::<Vec<_>>();

    // Wait for all backends to respond
    let all_results = futures::future::try_join_all(backend_queries)
        .await
        .unwrap_or_default()
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

    // Find first success
    let first_success = all_results.iter().find_map(|result| match result {
        PrecheckResult::Has(backend_id, response) => Some((*backend_id, response.clone())),
        _ => None,
    });

    // Update cache serially (all backends reported, no races)
    for result in all_results {
        match result {
            PrecheckResult::Has(backend_id, response) => {
                ctx.cache
                    .upsert(msg_id.to_owned(), response, backend_id)
                    .await;
            }
            PrecheckResult::Missing(backend_id, response) => {
                ctx.cache
                    .record_backend_missing(msg_id.to_owned(), backend_id, response)
                    .await;
            }
        }
    }

    // Send response to client
    match first_success {
        Some((backend_id, response)) => {
            client_write.write_all(&response).await?;
            ctx.router.complete_command(backend_id);
            Ok(backend_id)
        }
        None => {
            // All backends returned 430 or failed
            client_write
                .write_all(crate::protocol::NO_SUCH_ARTICLE)
                .await?;
            let backend_id = ctx.router.route_command(client_id, command)?;
            ctx.router.complete_command(backend_id);
            Ok(backend_id)
        }
    }
}

/// Handle HEAD command with adaptive prechecking
///
/// Same pattern as STAT precheck but handles multiline responses.
/// Optionally caches full headers if cache_articles=true.
pub async fn handle_head_precheck(
    ctx: &PrecheckContext,
    client_id: crate::types::ClientId,
    command: &str,
    msg_id: &MessageId<'_>,
    client_write: &mut tokio::net::tcp::WriteHalf<'_>,
    backend_to_client_bytes: &mut BackendToClientBytes,
) -> Result<BackendId> {
    use tokio::io::AsyncReadExt;

    let timeout = calculate_adaptive_timeout(ctx.metrics.as_ref());

    // Query all backends concurrently
    let backend_queries = (0..ctx.router.backend_count().get())
        .map(BackendId::from_index)
        .map(|backend_id| {
            let (router, buffer_pool, metrics, command, cache_articles) = (
                ctx.router.clone(),
                ctx.buffer_pool.clone(),
                ctx.metrics.clone(),
                command.to_string(),
                ctx.cache_articles,
            );

            tokio::spawn(async move {
                // Get connection and send HEAD command
                let provider = router.backend_provider(backend_id)?;
                let mut conn = provider.get_pooled_connection().await.ok()?;
                let mut buffer = buffer_pool.acquire().await;

                // Send command without timeout
                conn.as_mut().write_all(command.as_bytes()).await.ok()?;

                // Timeout only on first byte read
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
                            backend = backend_id.as_index(),
                            timeout_ms = timeout.as_millis(),
                            "HEAD query timed out waiting for first byte"
                        );
                        return None;
                    }
                };

                // Parse response code
                let response_code = crate::protocol::NntpResponse::parse(&response_bytes);
                let status_code = response_code.status_code()?.as_u16();
                let is_multiline = response_code.is_multiline();

                Some(match status_code {
                    221 => {
                        // Read complete multiline response if needed
                        if is_multiline {
                            loop {
                                let n =
                                    conn.as_mut().read(buffer.as_mut_slice()).await.unwrap_or(0);
                                if n == 0 || response_bytes.ends_with(b".\r\n") {
                                    break;
                                }
                                response_bytes.extend_from_slice(&buffer[..n]);
                            }
                        }

                        // Cache full headers or just availability marker
                        let cache_data = if cache_articles {
                            response_bytes
                        } else {
                            b"221\r\n".to_vec()
                        };

                        PrecheckResult::Has(backend_id, cache_data)
                    }
                    430 => {
                        if let Some(m) = metrics.as_ref() {
                            m.record_command(backend_id);
                            m.record_error_4xx(backend_id);
                        }
                        PrecheckResult::Missing(backend_id, response_bytes)
                    }
                    _ => PrecheckResult::Missing(backend_id, response_bytes),
                })
            })
        })
        .collect::<Vec<_>>();

    // Wait for all backends to respond
    let all_results = futures::future::try_join_all(backend_queries)
        .await
        .unwrap_or_default()
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

    // Find first success
    let first_success = all_results.iter().find_map(|result| match result {
        PrecheckResult::Has(backend_id, response) => Some((*backend_id, response.clone())),
        _ => None,
    });

    // Update cache serially
    for result in all_results {
        match result {
            PrecheckResult::Has(backend_id, cache_data) => {
                ctx.cache
                    .upsert(msg_id.to_owned(), cache_data, backend_id)
                    .await;
            }
            PrecheckResult::Missing(backend_id, response) => {
                ctx.cache
                    .record_backend_missing(msg_id.to_owned(), backend_id, response)
                    .await;
            }
        }
    }

    // Send response to client
    match first_success {
        Some((backend_id, response)) => {
            client_write.write_all(&response).await?;
            *backend_to_client_bytes = backend_to_client_bytes.add(response.len());
            ctx.router.complete_command(backend_id);
            Ok(backend_id)
        }
        None => {
            let response = crate::protocol::NO_SUCH_ARTICLE;
            client_write.write_all(response).await?;
            *backend_to_client_bytes = backend_to_client_bytes.add(response.len());
            let backend_id = ctx.router.route_command(client_id, command)?;
            ctx.router.complete_command(backend_id);
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
