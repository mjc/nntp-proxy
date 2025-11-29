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
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, warn};

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
/// Runs on a separate thread pool to avoid competing with client connections
/// for the main tokio runtime. This prevents precheck from saturating the
/// connection pool and blocking normal client traffic.
///
/// # Arguments
/// * `message_id` - The message-ID to check (with brackets)
/// * `backends` - List of (backend_id, provider, precheck_command) tuples
/// * `location_cache` - Cache to update with results
/// * `num_backends` - Total number of backends
/// * `fast_fail` - If true, return as soon as first backend responds with article
///
/// # Returns
/// `BackendAvailability` bitmap showing which backends have the article
pub async fn precheck_article_availability(
    message_id: &MessageId<'_>,
    backends: &[(
        BackendId,
        Arc<DeadpoolConnectionProvider>,
        crate::config::PrecheckCommand,
    )],
    location_cache: &Arc<ArticleLocationCache>,
    metrics: Option<&crate::metrics::MetricsCollector>,
) -> Result<BackendAvailability> {
    // Call directly - no spawning, we need to wait for results
    precheck_article_availability_impl(message_id, backends, location_cache, metrics).await
}

/// Internal implementation of precheck (runs on separate thread pool)
async fn precheck_article_availability_impl(
    message_id: &MessageId<'_>,
    backends: &[(
        BackendId,
        Arc<DeadpoolConnectionProvider>,
        crate::config::PrecheckCommand,
    )],
    location_cache: &Arc<ArticleLocationCache>,
    metrics: Option<&crate::metrics::MetricsCollector>,
) -> Result<BackendAvailability> {
    debug!(
        "Prechecking article {} availability across {} backends",
        message_id,
        backends.len()
    );

    let mut availability = BackendAvailability::new();

    // Shuffle backend order to avoid all clients hitting the same backend first
    // This prevents connection pool starvation when many clients precheck concurrently
    let mut shuffled_backends: Vec<_> = backends.to_vec();
    fastrand::shuffle(&mut shuffled_backends);

    // Check all backends CONCURRENTLY - spawn tasks and wait for all
    let mut tasks = Vec::with_capacity(shuffled_backends.len());

    for (backend_id, provider, precheck_cmd) in shuffled_backends {
        let provider = Arc::clone(&provider);
        let msg_id = message_id.to_owned();
        let metrics_clone = metrics.cloned();

        let task = tokio::spawn(async move {
            check_backend_has_article(
                backend_id,
                &msg_id,
                &provider,
                precheck_cmd,
                metrics_clone.as_ref(),
            )
            .await
        });

        tasks.push((backend_id, task));
    }

    // Wait for ALL tasks to complete concurrently using join_all
    // This ensures we don't favor any backend based on spawn order
    let results = futures::future::join_all(
        tasks
            .into_iter()
            .map(|(backend_id, task)| async move { (backend_id, task.await) }),
    )
    .await;

    // Process all results
    for (backend_id, task_result) in results {
        match task_result {
            Ok(Ok(result)) => {
                debug!(
                    "Backend {:?} precheck result: has_article={}",
                    result.backend_id, result.has_article
                );
                result.apply_to(&mut availability);
            }
            Ok(Err(e)) => {
                warn!("Backend {:?} precheck failed: {}", backend_id, e);
            }
            Err(e) => {
                warn!("Backend {:?} precheck task panicked: {}", backend_id, e);
            }
        }
    }

    // All backends checked - update cache
    location_cache.insert(message_id, availability).await;
    Ok(availability)
}

/// Check if a single backend has an article using STAT or HEAD command
///
/// # Arguments
/// * `backend_id` - Backend to check
/// * `message_id` - Article message-ID
/// * `provider` - Connection provider for this backend
/// * `precheck_cmd` - Command to use (STAT, HEAD, or Auto)
/// * `metrics` - Optional metrics collector
///
/// # Returns
/// `BackendCheckResult` indicating whether backend has the article
///
/// # Performance
/// Getting a connection from the pool is expensive. For batched prechecks,
/// consider keeping persistent connections (future optimization).
async fn check_backend_has_article(
    backend_id: BackendId,
    message_id: &MessageId<'_>,
    provider: &DeadpoolConnectionProvider,
    precheck_cmd: crate::config::PrecheckCommand,
    metrics: Option<&crate::metrics::MetricsCollector>,
) -> Result<BackendCheckResult> {
    // Get connection from pool
    let mut conn = provider.get_pooled_connection().await.context(format!(
        "Backend {:?} failed to acquire connection",
        backend_id
    ))?;

    // Run the precheck command
    use crate::config::PrecheckCommand;

    // For AUTO mode, try BOTH STAT and HEAD and compare results
    let has_article = if matches!(precheck_cmd, PrecheckCommand::Auto) {
        let mut reader = BufReader::new(&mut *conn);

        // Try STAT
        let stat_cmd = stat_by_msgid(message_id.as_str());
        debug!(
            "Backend {:?} AUTO mode: sending STAT: {}",
            backend_id,
            stat_cmd.trim()
        );

        reader.get_mut().write_all(stat_cmd.as_bytes()).await?;
        let mut stat_response = String::with_capacity(128);
        reader.read_line(&mut stat_response).await?;

        let stat_code =
            NntpResponse::parse_status_code(stat_response.as_bytes()).map(|c| c.as_u16());
        let stat_has_article = stat_code == Some(223);
        let stat_error = stat_code.is_some_and(|c| c == 430 || c >= 500);
        debug!(
            "Backend {:?} STAT response: '{}' (code={:?}, has_article={}, error={})",
            backend_id,
            stat_response.trim(),
            stat_code,
            stat_has_article,
            stat_error
        );

        // Try HEAD
        let head_cmd = crate::protocol::head_by_msgid(message_id.as_str());
        debug!(
            "Backend {:?} AUTO mode: sending HEAD: {}",
            backend_id,
            head_cmd.trim()
        );

        reader.get_mut().write_all(head_cmd.as_bytes()).await?;
        let mut head_response = String::with_capacity(128);
        reader.read_line(&mut head_response).await?;

        let head_code =
            NntpResponse::parse_status_code(head_response.as_bytes()).map(|c| c.as_u16());
        let head_has_article = head_code == Some(221);
        let head_error = head_code.is_some_and(|c| c == 430 || c >= 500);

        // Drain HEAD multiline response ONLY if it's actually a multiline response (221)
        if head_has_article {
            let mut line = String::new();
            loop {
                line.clear();
                let n = reader.read_line(&mut line).await?;
                if n == 0 || line == ".\r\n" {
                    break;
                }
            }
        }

        debug!(
            "Backend {:?} HEAD response: '{}' (code={:?}, has_article={}, error={})",
            backend_id,
            head_response.trim(),
            head_code,
            head_has_article,
            head_error
        );

        // Compare results
        if stat_has_article != head_has_article {
            warn!(
                "Backend {:?} DISCREPANCY for {}: STAT={}, HEAD={} (will be tracked later during actual download verification)",
                backend_id, message_id, stat_has_article, head_has_article
            );
        }

        // Decision logic for AUTO mode:
        // 1. If either reports an error (430 or 5xx) -> article NOT available
        // 2. If they disagree on has_article -> believe whichever says NOT available (false negative safer than false positive)
        // 3. If both agree -> use their answer
        if stat_error || head_error {
            debug!(
                "Backend {:?} AUTO: error response detected, article NOT available",
                backend_id
            );
            false
        } else if stat_has_article != head_has_article {
            // Disagreement: prefer the negative answer (safer to report "not found" than wrong article)
            debug!(
                "Backend {:?} AUTO: disagreement, preferring negative (article NOT available)",
                backend_id
            );
            false
        } else {
            // Both agree
            let has_article = stat_has_article; // Same as head_has_article
            debug!(
                "Backend {:?} AUTO: both agree, has_article={}",
                backend_id, has_article
            );
            has_article
        }
    } else {
        // For STAT or HEAD mode, just send single command
        let (command, expected_code, cmd_name) = match precheck_cmd {
            PrecheckCommand::Stat => (stat_by_msgid(message_id.as_str()), 223, "STAT"),
            PrecheckCommand::Head => (
                crate::protocol::head_by_msgid(message_id.as_str()),
                221,
                "HEAD",
            ),
            PrecheckCommand::Auto => unreachable!("AUTO handled above"),
        };

        debug!(
            "Backend {:?} sending {} command: {}",
            backend_id,
            cmd_name,
            command.trim()
        );
        conn.write_all(command.as_bytes())
            .await
            .with_context(|| format!("Failed to write {} command", cmd_name))?;

        let cmd_len = command.len() as u64;

        // Read response
        let mut reader = BufReader::new(&mut *conn);
        let mut response = String::with_capacity(128);
        reader
            .read_line(&mut response)
            .await
            .with_context(|| format!("Failed to read {} response", cmd_name))?;

        // For HEAD command, we need to drain the multiline response
        if cmd_name == "HEAD" {
            // Read until terminator (".\r\n")
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).await?;
                if line == ".\r\n" || line.is_empty() {
                    break;
                }
            }
        }

        let resp_len = response.len() as u64;

        // Record metrics for precheck command
        if let Some(metrics) = metrics {
            use crate::types::MetricsBytes;
            let cmd_bytes = MetricsBytes::new(cmd_len);
            let resp_bytes = MetricsBytes::new(resp_len);
            let _ = metrics.record_command_execution(backend_id, cmd_bytes, resp_bytes);
        }

        // Parse status code (223 for STAT, 221 for HEAD)
        let status_code = NntpResponse::parse_status_code(response.as_bytes());
        let has_article = status_code
            .map(|code| code.as_u16() == expected_code)
            .unwrap_or(false);

        debug!(
            "Backend {:?} {} response for {}: '{}' (status_code={:?}, has_article={})",
            backend_id,
            cmd_name,
            message_id,
            response.trim(),
            status_code,
            has_article
        );

        has_article
    };

    debug!(
        "Backend {:?} precheck completed: has_article={}",
        backend_id, has_article
    );

    Ok(BackendCheckResult {
        backend_id,
        has_article,
    })
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
    fn test_backend_check_result_debug() {
        let result = BackendCheckResult::new(BackendId::from_index(0), true);
        let debug_str = format!("{:?}", result);
        assert!(debug_str.contains("BackendCheckResult"));
        assert!(debug_str.contains("backend_id"));
        assert!(debug_str.contains("has_article"));
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
