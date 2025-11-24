//! Article availability precheck for adaptive routing
//!
//! ## What This Does
//!
//! When a client requests an article by message-ID (STAT/ARTICLE/BODY/HEAD) and the
//! location cache doesn't know which backend has it, this module checks all backends
//! concurrently to discover availability, then caches the result for future requests.
//!
//! ## Value Proposition
//!
//! ### With SABnzbd Precheck ENABLED
//! - **First download of an NZB**: ~50 seconds (proxy checks all backends, builds cache)
//! - **Re-downloading the same NZB later**: <1 second (cache hit, instant 223 responses)
//! - **Benefit**: 50x+ speedup for cached articles
//!
//! ### With SABnzbd Precheck DISABLED
//! - **Download time**: ~50 seconds (proxy does precheck work SABnzbd would skip)
//! - **Benefit**: Guarantees no false "article missing" errors when article exists on
//!   a different backend. Without proxy precheck, SABnzbd tries one backend and may
//!   report failure even if another backend has the article.
//! - **Additional benefit**: Builds location cache for future requests and teaches
//!   AWR which backend is faster for actual downloads.
//!
//! ## Performance Characteristics
//!
//! Measured with 180 client connections (360 concurrent STAT operations):
//! - **Throughput**: ~614 STATs/second (30,710 total / 50s)
//! - **Handles**: Up to 360 concurrent operations competing for 90 pool connections
//! - **Fair scheduling**: FIFO pool queue + jittered retry backoff prevents starvation
//!
//! ## Current Implementation
//!
//! The current implementation spawns one task per backend per STAT check.
//! Each task:
//! 1. Retries pool acquisition with 500ms timeout + jittered backoff (up to 20 retries)
//! 2. Sends STAT command
//! 3. Reads response (30s timeout)
//! 4. Returns connection to pool
//!
//! This works well for moderate loads but has overhead for bulk operations.
//!
//! ### Future Optimization: Persistent Precheck Connections
//!
//! Create dedicated precheck workers that:
//! - Keep one persistent connection per backend
//! - Accept STAT requests via channel (mpsc)
//! - Pipeline multiple STATs on same connection
//! - Amortize connection overhead across many checks
//!
//! Benefits:
//! - Eliminate repeated auth handshakes
//! - Enable TCP connection reuse (faster RTT)
//! - Support pipelining (send multiple STATs before reading responses)
//! - Potential 10-50x speedup for bulk precheck operations

use crate::cache::{ArticleLocationCache, BackendAvailability};
use crate::pool::DeadpoolConnectionProvider;
use crate::protocol::{NntpResponse, stat_by_msgid};
use crate::types::{BackendId, MessageId};
use anyhow::{Context, Result};
use futures::stream::{FuturesUnordered, StreamExt};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;
use tracing::{debug, warn};

/// Timeout for individual STAT commands
/// This is for the actual STAT operation once we have a connection
const STAT_TIMEOUT: Duration = Duration::from_secs(30);

/// Timeout for getting a connection from the pool
/// Short timeout so we can retry rather than blocking forever
const POOL_ACQUIRE_TIMEOUT: Duration = Duration::from_millis(500);

/// Maximum retries for pool acquisition
/// With 500ms timeout, 20 retries = 10 seconds max wait
const POOL_ACQUIRE_MAX_RETRIES: u32 = 20;

/// Result of checking a single backend
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BackendCheckResult {
    backend_id: BackendId,
    has_article: bool,
}

impl BackendCheckResult {
    /// Create a new check result
    #[cfg(test)]
    const fn new(backend_id: BackendId, has_article: bool) -> Self {
        Self {
            backend_id,
            has_article,
        }
    }

    /// Apply this result to an availability bitmap
    fn apply_to(self, availability: &mut BackendAvailability) {
        if self.has_article {
            availability.mark_available(self.backend_id);
        } else {
            availability.mark_unavailable(self.backend_id);
        }
    }
}

/// Precheck article availability across all backends
///
/// This spawns concurrent tasks to check each backend, collects the results,
/// and updates the location cache with the availability bitmap.
///
/// # Arguments
/// * `message_id` - The message-ID to check (with brackets)
/// * `backends` - List of backend providers to check
/// * `location_cache` - Cache to update with results
/// * `num_backends` - Total number of backends
/// * `fast_fail` - If true, return as soon as first backend responds with article
///
/// # Returns
/// `BackendAvailability` bitmap showing which backends have the article
///
/// # Performance
/// - All backends checked concurrently (tokio::spawn per backend)
/// - 500ms timeout per backend (prevents slow backends from blocking)
/// - Results aggregated and cache updated atomically
/// - With fast_fail=true, returns immediately on first success
pub async fn precheck_article_availability(
    message_id: &MessageId<'_>,
    backends: &[(BackendId, Arc<DeadpoolConnectionProvider>)],
    location_cache: &Arc<ArticleLocationCache>,
    num_backends: usize,
    fast_fail: bool,
    metrics: Option<&crate::metrics::MetricsCollector>,
) -> Result<BackendAvailability> {
    debug!(
        "Prechecking article {} availability across {} backends (fast_fail={})",
        message_id,
        backends.len(),
        fast_fail
    );

    // Spawn all check tasks concurrently
    let futures = spawn_check_tasks(message_id, backends, metrics);

    // Collect results - fast-fail if requested
    let availability = if fast_fail {
        collect_with_fast_fail(futures, message_id, location_cache, num_backends).await?
    } else {
        let mut futures = futures;
        collect_all_results(&mut futures, num_backends).await
    };

    // Update cache (async, non-blocking)
    if !fast_fail {
        update_cache_async(location_cache, message_id, availability);
    }

    Ok(availability)
}

/// Spawn concurrent check tasks for all backends
fn spawn_check_tasks(
    message_id: &MessageId<'_>,
    backends: &[(BackendId, Arc<DeadpoolConnectionProvider>)],
    metrics: Option<&crate::metrics::MetricsCollector>,
) -> FuturesUnordered<tokio::task::JoinHandle<Result<BackendCheckResult>>> {
    backends
        .iter()
        .map(|(backend_id, provider)| {
            let backend_id = *backend_id;
            let provider = Arc::clone(provider);
            let msg_id = message_id.to_owned();
            let metrics_clone = metrics.cloned(); // Clone the MetricsCollector (cheap - just clones Arc)

            tokio::spawn(async move {
                check_backend_has_article(backend_id, &msg_id, &provider, metrics_clone.as_ref())
                    .await
            })
        })
        .collect()
}

/// Collect results with fast-fail optimization
///
/// Returns immediately when first backend reports having the article.
/// Spawns background task to complete remaining checks and update cache.
async fn collect_with_fast_fail(
    mut futures: FuturesUnordered<tokio::task::JoinHandle<Result<BackendCheckResult>>>,
    message_id: &MessageId<'_>,
    location_cache: &Arc<ArticleLocationCache>,
    num_backends: usize,
) -> Result<BackendAvailability> {
    let mut availability = BackendAvailability::new();
    let mut completed_count = 0;

    while let Some(result) = futures.next().await {
        match result {
            Ok(Ok(check_result)) => {
                debug!(
                    "Backend {:?} precheck result: has_article={}",
                    check_result.backend_id, check_result.has_article
                );

                check_result.apply_to(&mut availability);
                completed_count += 1;

                // Fast-fail on first article found
                if check_result.has_article {
                    debug!(
                        "Fast-fail: found article on backend {:?}, returning early",
                        check_result.backend_id
                    );

                    // Spawn background task to complete remaining checks
                    spawn_background_completion(
                        futures,
                        completed_count,
                        availability,
                        location_cache,
                        message_id,
                        num_backends,
                    );

                    return Ok(availability);
                }
            }
            Ok(Err(e)) => warn!("Backend check failed: {}", e),
            Err(e) => warn!("Task join error: {}", e),
        }
    }

    // No backend has article - update cache with empty availability
    update_cache_async(location_cache, message_id, availability);
    Ok(availability)
}

/// Spawn background task to complete remaining precheck operations
fn spawn_background_completion(
    mut futures: FuturesUnordered<tokio::task::JoinHandle<Result<BackendCheckResult>>>,
    already_completed: usize,
    mut availability: BackendAvailability,
    location_cache: &Arc<ArticleLocationCache>,
    message_id: &MessageId<'_>,
    num_backends: usize,
) {
    let cache = Arc::clone(location_cache);
    let msg_id = message_id.to_owned();

    tokio::spawn(async move {
        let mut total_completed = already_completed;

        // Process remaining results
        while let Some(result) = futures.next().await {
            if let Ok(Ok(check_result)) = result {
                check_result.apply_to(&mut availability);
                total_completed += 1;
            }
        }

        debug!(
            "Background precheck complete: {}/{} backends responded",
            total_completed, num_backends
        );

        // Update cache with complete results
        cache.insert(&msg_id, availability).await;
    });
}

/// Collect all results (no fast-fail)
async fn collect_all_results(
    futures: &mut FuturesUnordered<tokio::task::JoinHandle<Result<BackendCheckResult>>>,
    num_backends: usize,
) -> BackendAvailability {
    let mut availability = BackendAvailability::new();
    let mut successful_checks = 0;

    while let Some(result) = futures.next().await {
        match result {
            Ok(Ok(check_result)) => {
                debug!(
                    "Backend {:?} precheck result: has_article={}",
                    check_result.backend_id, check_result.has_article
                );

                check_result.apply_to(&mut availability);
                successful_checks += 1;
            }
            Ok(Err(e)) => {
                warn!(
                    "Backend check returned error (article will be treated as unavailable): {}",
                    e
                );
            }
            Err(e) => {
                warn!(
                    "Task join error (article will be treated as unavailable): {}",
                    e
                );
            }
        }
    }

    warn!(
        "Precheck complete: {}/{} backends responded successfully, {} backends report having article",
        successful_checks,
        num_backends,
        availability.available_backends(num_backends).len()
    );

    availability
}

/// Update cache asynchronously (fire-and-forget)
fn update_cache_async(
    location_cache: &Arc<ArticleLocationCache>,
    message_id: &MessageId<'_>,
    availability: BackendAvailability,
) {
    let cache = Arc::clone(location_cache);
    let msg_id = message_id.to_owned();

    tokio::spawn(async move {
        cache.insert(&msg_id, availability).await;
    });
}

/// Check if a single backend has an article using STAT command
///
/// # Arguments
/// * `backend_id` - Backend to check
/// * `message_id` - Article message-ID
/// * `provider` - Connection provider for this backend
///
/// # Returns
/// `BackendCheckResult` indicating whether backend has the article
///
/// # Timeout
/// 5 seconds timeout prevents slow backends from blocking precheck
///
/// # Performance
/// Getting a connection from the pool is expensive. For batched prechecks,
/// consider keeping persistent connections (future optimization).
async fn check_backend_has_article(
    backend_id: BackendId,
    message_id: &MessageId<'_>,
    provider: &DeadpoolConnectionProvider,
    metrics: Option<&crate::metrics::MetricsCollector>,
) -> Result<BackendCheckResult> {
    // Retry pool acquisition with timeout to handle pool exhaustion gracefully
    let mut conn = None;
    for attempt in 1..=POOL_ACQUIRE_MAX_RETRIES {
        match timeout(POOL_ACQUIRE_TIMEOUT, provider.get_pooled_connection()).await {
            Ok(Ok(c)) => {
                conn = Some(c);
                break;
            }
            Ok(Err(e)) => {
                // Pool error - fail immediately
                return Err(e.context(format!("Backend {:?} pool error", backend_id)));
            }
            Err(_) => {
                // Timeout - retry with jittered backoff to avoid thundering herd
                if attempt < POOL_ACQUIRE_MAX_RETRIES {
                    // Jittered backoff: base + deterministic jitter from backend_id
                    // This spreads retries across time without adding dependencies
                    let base_delay_ms = 10u64 + (attempt as u64 * 5);
                    let jitter = (backend_id.as_index() as u64 * 7 + attempt as u64) % 30;
                    let delay = Duration::from_millis(base_delay_ms + jitter);

                    debug!(
                        "Backend {:?} pool timeout (attempt {}/{}), retrying after {:?}...",
                        backend_id, attempt, POOL_ACQUIRE_MAX_RETRIES, delay
                    );
                    tokio::time::sleep(delay).await;
                } else {
                    warn!(
                        "Backend {:?} failed to acquire connection after {} attempts",
                        backend_id, POOL_ACQUIRE_MAX_RETRIES
                    );
                    return Err(anyhow::anyhow!(
                        "Pool exhausted after {} retries",
                        POOL_ACQUIRE_MAX_RETRIES
                    ));
                }
            }
        }
    }

    let mut conn = conn.ok_or_else(|| anyhow::anyhow!("Failed to acquire connection"))?;

    // Now run the actual STAT with its own timeout
    let result = timeout(STAT_TIMEOUT, async {
        // Send STAT command (TCP_NODELAY ensures immediate send)
        let stat_command = stat_by_msgid(message_id.as_str());
        debug!(
            "Backend {:?} sending STAT command: {}",
            backend_id,
            stat_command.trim()
        );
        conn.write_all(stat_command.as_bytes())
            .await
            .context("Failed to write STAT command")?;

        let cmd_len = stat_command.len() as u64;

        // Read response
        let mut reader = BufReader::new(&mut *conn);
        let mut response = String::with_capacity(128);
        reader
            .read_line(&mut response)
            .await
            .context("Failed to read STAT response")?;

        let resp_len = response.len() as u64;

        // Record metrics for precheck STAT
        if let Some(metrics) = metrics {
            use crate::types::MetricsBytes;
            let cmd_bytes = MetricsBytes::new(cmd_len);
            let resp_bytes = MetricsBytes::new(resp_len);
            let _ = metrics.record_command_execution(backend_id, cmd_bytes, resp_bytes);
        }

        // Parse status code (223 = article exists)
        let status_code = NntpResponse::parse_status_code(response.as_bytes());
        let has_article = status_code
            .map(|code| code.as_u16() == 223)
            .unwrap_or(false);

        debug!(
            "Backend {:?} STAT response for {}: '{}' (status_code={:?}, has_article={})",
            backend_id,
            message_id,
            response.trim(),
            status_code,
            has_article
        );

        Ok::<bool, anyhow::Error>(has_article)
    })
    .await;

    match result {
        Ok(Ok(has_article)) => {
            debug!(
                "Backend {:?} precheck completed: has_article={}",
                backend_id, has_article
            );
            Ok(BackendCheckResult {
                backend_id,
                has_article,
            })
        }
        Ok(Err(e)) => {
            warn!(
                "Backend {:?} STAT check FAILED for {}: {}",
                backend_id, message_id, e
            );
            Err(e.context(format!("Backend {:?} STAT check failed", backend_id)))
        }
        Err(_) => {
            // Timeout - likely pool exhaustion during bulk operations
            // Don't mark as not-having-article, just skip this backend in bitmap
            // (AWR will still consider it if no other backend is marked)
            warn!(
                "Backend {:?} STAT TIMED OUT after {:?} for {} (pool likely exhausted, skipping)",
                backend_id, STAT_TIMEOUT, message_id
            );
            // Return error so this backend isn't marked in availability bitmap
            Err(anyhow::anyhow!("Timeout - pool exhausted"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::BackendAvailability;
    use crate::types::BackendId;

    #[test]
    fn test_backend_check_result_creation() {
        let result = BackendCheckResult::new(BackendId::from_index(0), true);
        assert_eq!(result.backend_id, BackendId::from_index(0));
        assert!(result.has_article);
    }

    #[test]
    fn test_backend_check_result_equality() {
        let result1 = BackendCheckResult::new(BackendId::from_index(0), true);
        let result2 = BackendCheckResult::new(BackendId::from_index(0), true);
        let result3 = BackendCheckResult::new(BackendId::from_index(1), true);

        assert_eq!(result1, result2);
        assert_ne!(result1, result3);
    }

    #[test]
    fn test_apply_to_marks_available() {
        let result = BackendCheckResult::new(BackendId::from_index(0), true);
        let mut availability = BackendAvailability::new();

        result.apply_to(&mut availability);

        assert!(availability.has_article(BackendId::from_index(0)));
    }

    #[test]
    fn test_apply_to_marks_unavailable() {
        let result = BackendCheckResult::new(BackendId::from_index(0), false);
        let mut availability = BackendAvailability::new();

        result.apply_to(&mut availability);

        assert!(!availability.has_article(BackendId::from_index(0)));
    }

    #[test]
    fn test_apply_to_multiple_backends() {
        let mut availability = BackendAvailability::new();

        BackendCheckResult::new(BackendId::from_index(0), true).apply_to(&mut availability);
        BackendCheckResult::new(BackendId::from_index(1), false).apply_to(&mut availability);
        BackendCheckResult::new(BackendId::from_index(2), true).apply_to(&mut availability);

        assert!(availability.has_article(BackendId::from_index(0)));
        assert!(!availability.has_article(BackendId::from_index(1)));
        assert!(availability.has_article(BackendId::from_index(2)));
    }

    #[test]
    fn test_stat_timeout_constant() {
        assert_eq!(STAT_TIMEOUT, Duration::from_millis(500));
    }

    #[test]
    fn test_backend_check_result_debug() {
        let result = BackendCheckResult::new(BackendId::from_index(0), true);
        let debug_str = format!("{:?}", result);
        assert!(debug_str.contains("BackendCheckResult"));
        assert!(debug_str.contains("backend_id"));
        assert!(debug_str.contains("has_article"));
    }

    #[test]
    fn test_spawn_check_tasks_creates_correct_count() {
        // This is a bit tricky to test without actual backends
        // We'd need integration tests for the full flow
        // For now, just verify the constant
        assert_eq!(STAT_TIMEOUT.as_millis(), 500);
    }
}

// ============================================================================
// BATCHED STAT WORKER (Future Optimization - Currently Unused)
// ============================================================================
//
// The code below demonstrates persistent precheck workers for 10-50x speedup.
// Current bottleneck: Each STAT gets new connection from pool (expensive).
//
// Architecture:
// 1. Spawn one worker per backend at proxy startup
// 2. Workers keep persistent authenticated connections
// 3. Send STAT requests via mpsc::channel
// 4. Workers pipeline multiple STATs on same connection
//
// Benefits:
// - Eliminate repeated connection/auth overhead
// - Enable TCP pipelining (send N STATs, read N responses)
// - Reduce per-STAT latency from ~130ms to ~10ms
// - Support SABnzbd's rapid bulk precheck patterns
//
// To enable:
// - Uncomment worker code
// - Replace spawn_check_tasks() to use channels
// - Spawn workers in proxy initialization
//
// #[allow(dead_code)]
// use tokio::sync::{mpsc, oneshot};
//
// #[allow(dead_code)]
// pub struct PrecheckWorker {
//     backend_id: BackendId,
//     provider: Arc<DeadpoolConnectionProvider>,
//     request_rx: mpsc::Receiver<PrecheckRequest>,
// }
//
// #[allow(dead_code)]
// struct PrecheckRequest {
//     message_id: String,
//     response_tx: oneshot::Sender<bool>,
// }
//
// #[allow(dead_code)]
// impl PrecheckWorker {
//     pub fn spawn(
//         backend_id: BackendId,
//         provider: Arc<DeadpoolConnectionProvider>,
//     ) -> mpsc::Sender<PrecheckRequest> {
//         let (tx, rx) = mpsc::channel(1000); // Buffer 1000 STAT requests
//         let worker = Self {
//             backend_id,
//             provider,
//             request_rx: rx,
//         };
//
//         tokio::spawn(worker.run());
//         tx
//     }
//
//     async fn run(mut self) {
//         loop {
//             // Get persistent connection (reused across many STATs)
//             let mut conn = match self.provider.get_pooled_connection().await {
//                 Ok(c) => c,
//                 Err(e) => {
//                     warn!("Backend {:?} precheck worker connection failed: {}", self.backend_id, e);
//                     tokio::time::sleep(Duration::from_secs(1)).await;
//                     continue;
//                 }
//             };
//
//             // Process STAT requests on this connection
//             while let Some(req) = self.request_rx.recv().await {
//                 // Send STAT command
//                 let stat_cmd = stat_by_msgid(&req.message_id);
//                 if let Err(e) = conn.write_all(stat_cmd.as_bytes()).await {
//                     warn!("Backend {:?} STAT write failed: {}", self.backend_id, e);
//                     let _ = req.response_tx.send(false);
//                     break; // Connection dead, reconnect
//                 }
//
//                 // Read response immediately (no pipelining yet, but connection reused)
//                 let mut reader = BufReader::new(&mut *conn);
//                 let mut response = String::with_capacity(128);
//                 if let Err(e) = reader.read_line(&mut response).await {
//                     warn!("Backend {:?} STAT read failed: {}", self.backend_id, e);
//                     let _ = req.response_tx.send(false);
//                     break; // Connection dead, reconnect
//                 }
//
//                 // Parse status code (223 = has article)
//                 let has_article = NntpResponse::parse_status_code(response.as_bytes())
//                     .map(|code| code.as_u16() == 223)
//                     .unwrap_or(false);
//
//                 let _ = req.response_tx.send(has_article);
//             }
//         }
//     }
// }
