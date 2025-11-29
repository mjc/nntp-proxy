//! Retry logic for backend command execution
//!
//! Handles 430 (article not found) retries across multiple backends.
//! Uses availability bitmaps to track which backends have been tried.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use tracing::{debug, warn};

use crate::cache::{ArticleLocationCache, BackendAvailability};
use crate::pool::DeadpoolConnectionProvider;
use crate::router::BackendSelector;
use crate::types::{BackendId, protocol::MessageId};

/// Default timeout for backend operations
/// Back to 15s - precheck now uses separate runtime to avoid contention
pub const BACKEND_TIMEOUT: Duration = Duration::from_secs(15);

/// Find next available backend using AWR scoring
///
/// Selects the best backend from remaining available backends based on:
/// - Connection pool availability
/// - Current load (pending requests)
/// - Pool saturation
/// - Transfer performance (if metrics available)
#[inline]
pub fn find_next_available_backend(
    remaining: &BackendAvailability,
    router: &BackendSelector,
    metrics: Option<&crate::metrics::MetricsCollector>,
) -> Option<BackendId> {
    router.route_command_with_availability(remaining, metrics)
}

/// Acquire pooled connection with timeout
#[inline]
pub async fn acquire_connection_with_timeout(
    provider: &DeadpoolConnectionProvider,
    backend: BackendId,
    timeout: Duration,
) -> Result<deadpool::managed::Object<crate::pool::deadpool_connection::TcpManager>> {
    use crate::pool::ConnectionProvider;
    let pool_status = provider.status();
    tokio::time::timeout(timeout, provider.get_pooled_connection())
        .await
        .map_err(|_| {
            let waiting = pool_status.max_size.get() - pool_status.available.get();
            anyhow!(
                "Backend {:?} pool acquire timeout after {:?} (pool: {}/{} available, {} waiting)",
                backend,
                timeout,
                pool_status.available,
                pool_status.max_size,
                waiting
            )
        })?
        .map_err(|e| anyhow!("Backend {:?} pool error: {}", backend, e))
}

/// Check if error is retryable (430 article not found)
#[inline]
pub fn is_article_not_found_error(result: &Result<()>) -> bool {
    result
        .as_ref()
        .err()
        .is_some_and(|e| e.to_string().to_lowercase().contains("430"))
}

/// Update location cache for backend availability
#[inline]
fn spawn_cache_update(
    message_id: &MessageId<'_>,
    cache: &Arc<ArticleLocationCache>,
    backend: BackendId,
    available: bool,
) {
    let cache = Arc::clone(cache);
    let msg_id_owned = message_id.to_owned();
    tokio::spawn(async move {
        cache.update(&msg_id_owned, backend, available).await;
    });
}

/// Update location cache for failed backend (deprecated - use with_precheck version)
#[inline]
#[allow(dead_code)]
pub fn update_cache_for_failed_backend(
    message_id: Option<&MessageId<'_>>,
    location_cache: Option<&Arc<ArticleLocationCache>>,
    backend: BackendId,
) {
    message_id.zip(location_cache).inspect(|(msg_id, cache)| {
        spawn_cache_update(msg_id, cache, backend, false);
        debug!(
            "Updating location cache: {} -> backend {:?} (NOT available)",
            msg_id, backend
        );
    });
}

/// Update location cache for failed backend with precheck comparison
#[inline]
pub fn update_cache_for_failed_backend_with_precheck(
    message_id: Option<&MessageId<'_>>,
    location_cache: Option<&Arc<ArticleLocationCache>>,
    backend: BackendId,
    precheck_said_available: bool,
    precheck_command: Option<crate::config::PrecheckCommand>,
    metrics: Option<&crate::metrics::MetricsCollector>,
) {
    message_id.zip(location_cache).inspect(|(msg_id, cache)| {
        spawn_cache_update(msg_id, cache, backend, false);
        if precheck_said_available {
            let cmd_str = precheck_command
                .map(|c| format!(" ({})", c))
                .unwrap_or_default();
            warn!(
                "PRECHECK FALSE POSITIVE{}: {} -> backend {:?} - precheck said AVAILABLE but download returned 430",
                cmd_str, msg_id, backend
            );
            // Record metric based on which command was used
            if let Some(m) = metrics {
                use crate::config::PrecheckCommand;
                match precheck_command {
                    Some(PrecheckCommand::Stat) => m.record_precheck_stat_false_positive(backend),
                    Some(PrecheckCommand::Head) => m.record_precheck_head_false_positive(backend),
                    _ => {} // Auto mode or unknown - don't record
                }
            }
        } else {
            debug!(
                "Updating location cache: {} -> backend {:?} (NOT available, precheck was correct)",
                msg_id, backend
            );
        }
    });
}

/// Update location cache for successful backend (deprecated - use with_precheck version)
#[inline]
#[allow(dead_code)]
pub fn update_cache_for_successful_backend(
    message_id: Option<&MessageId<'_>>,
    location_cache: Option<&Arc<ArticleLocationCache>>,
    backend: BackendId,
) {
    message_id.zip(location_cache).inspect(|(msg_id, cache)| {
        spawn_cache_update(msg_id, cache, backend, true);
        debug!(
            "Updating location cache: {} -> backend {:?} (available)",
            msg_id, backend
        );
    });
}

/// Update location cache for successful backend with precheck comparison
#[inline]
pub fn update_cache_for_successful_backend_with_precheck(
    message_id: Option<&MessageId<'_>>,
    location_cache: Option<&Arc<ArticleLocationCache>>,
    backend: BackendId,
    precheck_said_unavailable: bool,
    precheck_command: Option<crate::config::PrecheckCommand>,
    metrics: Option<&crate::metrics::MetricsCollector>,
) {
    message_id.zip(location_cache).inspect(|(msg_id, cache)| {
        spawn_cache_update(msg_id, cache, backend, true);
        if precheck_said_unavailable {
            let cmd_str = precheck_command
                .map(|c| format!(" ({})", c))
                .unwrap_or_default();
            warn!(
                "PRECHECK FALSE NEGATIVE{}: {} -> backend {:?} - precheck said UNAVAILABLE but download SUCCEEDED",
                cmd_str, msg_id, backend
            );
            // Record metric based on which command was used
            if let Some(m) = metrics {
                use crate::config::PrecheckCommand;
                match precheck_command {
                    Some(PrecheckCommand::Stat) => m.record_precheck_stat_false_negative(backend),
                    Some(PrecheckCommand::Head) => m.record_precheck_head_false_negative(backend),
                    _ => {} // Auto mode or unknown - don't record
                }
            }
        } else {
            debug!(
                "Updating location cache: {} -> backend {:?} (available, precheck was correct)",
                msg_id, backend
            );
        }
    });
}

/// Initialize backend availability bitmap
///
/// If availability is already known (from precheck), use it.
/// Otherwise, create bitmap with all backends marked as available.
pub fn initialize_backend_bitmap(
    backend_availability: Option<BackendAvailability>,
    router: &BackendSelector,
) -> BackendAvailability {
    backend_availability.unwrap_or_else(|| {
        // No cache - create bitmap with all backends available
        (0..router.backend_count()).map(BackendId::from_index).fold(
            BackendAvailability::new(),
            |mut bitmap, id| {
                bitmap.mark_available(id);
                bitmap
            },
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_article_not_found_error() {
        let ok_result: Result<()> = Ok(());
        assert!(!is_article_not_found_error(&ok_result));

        let not_found: Result<()> = Err(anyhow!("430 article not found"));
        assert!(is_article_not_found_error(&not_found));

        let other_error: Result<()> = Err(anyhow!("500 internal error"));
        assert!(!is_article_not_found_error(&other_error));
    }

    #[test]
    fn test_find_next_available_backend() {
        use crate::pool::DeadpoolConnectionProvider;
        use crate::types::ServerName;

        // Create test router with 2 backends
        let mut router = BackendSelector::new(crate::config::RoutingStrategy::AdaptiveWeighted);

        let provider1 = DeadpoolConnectionProvider::new(
            "localhost".to_string(),
            119,
            "server1".to_string(),
            10,
            None,
            None,
        );
        let provider2 = DeadpoolConnectionProvider::new(
            "localhost".to_string(),
            119,
            "server2".to_string(),
            10,
            None,
            None,
        );

        router.add_backend(
            BackendId::from_index(0),
            ServerName::new("server1".to_string()).unwrap(),
            provider1,
            crate::config::PrecheckCommand::default(),
        );
        router.add_backend(
            BackendId::from_index(1),
            ServerName::new("server2".to_string()).unwrap(),
            provider2,
            crate::config::PrecheckCommand::default(),
        );

        // Test: both backends available, AWR should pick one
        let mut bitmap = BackendAvailability::new();
        bitmap.mark_available(BackendId::from_index(0));
        bitmap.mark_available(BackendId::from_index(1));

        let next = find_next_available_backend(&bitmap, &router, None);
        assert!(next.is_some());

        // Test: only backend 1 available
        bitmap.mark_unavailable(BackendId::from_index(0));
        let next = find_next_available_backend(&bitmap, &router, None);
        assert_eq!(next, Some(BackendId::from_index(1)));

        // Test: no backends available
        bitmap.mark_unavailable(BackendId::from_index(1));
        let next = find_next_available_backend(&bitmap, &router, None);
        assert_eq!(next, None);
    }
}
