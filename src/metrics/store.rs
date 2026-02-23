//! Metrics persistence layer
//!
//! This module provides the `MetricsStore` which holds all persistable cumulative counters.
//! The store can be saved to and loaded from disk as JSON.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Per-process monotonic counter — ensures concurrent saves use distinct temp file names.
/// Two simultaneous saves will produce e.g. `stats.json.0.tmp` and `stats.json.1.tmp`,
/// so they cannot clobber each other before the final atomic rename.
static SAVE_SEQ: AtomicU64 = AtomicU64::new(0);

// ============================================================================
// Serializable Format Types
// ============================================================================

/// Version 1 of the stats file format
const STATS_FILE_VERSION: u32 = 1;

/// Top-level stats file structure (versioned for future migration)
#[derive(Debug, Serialize, Deserialize)]
struct StatsFile {
    version: u32,
    saved_at: String, // ISO 8601 timestamp
    global: PersistedGlobal,
    backends: SmallVec<[PersistedBackend; 8]>, // Stack-allocated for ≤8 backends
    users: SmallVec<[PersistedUser; 4]>,       // Stack-allocated for ≤4 users
    pipeline: PersistedPipeline,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PersistedGlobal {
    total_connections: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PersistedBackend {
    name: String,
    total_commands: u64,
    bytes_sent: u64,
    bytes_received: u64,
    errors: u64,
    errors_4xx: u64,
    errors_5xx: u64,
    article_bytes_total: u64,
    article_count: u64,
    ttfb_micros_total: u64,
    ttfb_count: u64,
    send_micros_total: u64,
    recv_micros_total: u64,
    connection_failures: u64,
    // Future v2 (retention tracking PR):
    //   oldest_served_article_age_secs, newest_missing_article_age_secs,
    //   retention_sample_count (for confidence interval calculation)
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PersistedUser {
    username: String,
    total_connections: u64,
    bytes_sent: u64,
    bytes_received: u64,
    total_commands: u64,
    errors: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PersistedPipeline {
    batches: u64,
    commands: u64,
    requests_queued: u64,
    requests_completed: u64,
}

// ============================================================================
// BackendStore - Persistable per-backend atomics
// ============================================================================

/// Persistable backend metrics storage (atomic counters only)
///
/// This struct contains only cumulative counters that should survive restarts.
/// Live gauges (active_connections, health_status) are kept separately on BackendMetrics.
#[derive(Debug)]
pub struct BackendStore {
    pub total_commands: AtomicU64,
    pub bytes_sent: AtomicU64,
    pub bytes_received: AtomicU64,
    pub errors: AtomicU64,
    pub errors_4xx: AtomicU64,
    pub errors_5xx: AtomicU64,
    pub article_bytes_total: AtomicU64,
    pub article_count: AtomicU64,
    pub ttfb_micros_total: AtomicU64,
    pub ttfb_count: AtomicU64,
    pub send_micros_total: AtomicU64,
    pub recv_micros_total: AtomicU64,
    pub connection_failures: AtomicU64,
}

impl Default for BackendStore {
    fn default() -> Self {
        Self {
            total_commands: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            bytes_received: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            errors_4xx: AtomicU64::new(0),
            errors_5xx: AtomicU64::new(0),
            article_bytes_total: AtomicU64::new(0),
            article_count: AtomicU64::new(0),
            ttfb_micros_total: AtomicU64::new(0),
            ttfb_count: AtomicU64::new(0),
            send_micros_total: AtomicU64::new(0),
            recv_micros_total: AtomicU64::new(0),
            connection_failures: AtomicU64::new(0),
        }
    }
}

impl BackendStore {
    /// Convert to serializable format (reads atomics with Relaxed ordering)
    pub(crate) fn to_persisted(&self, name: &str) -> PersistedBackend {
        PersistedBackend {
            name: name.to_string(),
            total_commands: self.total_commands.load(Ordering::Relaxed),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            bytes_received: self.bytes_received.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            errors_4xx: self.errors_4xx.load(Ordering::Relaxed),
            errors_5xx: self.errors_5xx.load(Ordering::Relaxed),
            article_bytes_total: self.article_bytes_total.load(Ordering::Relaxed),
            article_count: self.article_count.load(Ordering::Relaxed),
            ttfb_micros_total: self.ttfb_micros_total.load(Ordering::Relaxed),
            ttfb_count: self.ttfb_count.load(Ordering::Relaxed),
            send_micros_total: self.send_micros_total.load(Ordering::Relaxed),
            recv_micros_total: self.recv_micros_total.load(Ordering::Relaxed),
            connection_failures: self.connection_failures.load(Ordering::Relaxed),
        }
    }

    /// Restore from serializable format (writes atomics with Relaxed ordering)
    pub(crate) fn restore_from(&self, persisted: &PersistedBackend) {
        self.total_commands
            .store(persisted.total_commands, Ordering::Relaxed);
        self.bytes_sent
            .store(persisted.bytes_sent, Ordering::Relaxed);
        self.bytes_received
            .store(persisted.bytes_received, Ordering::Relaxed);
        self.errors.store(persisted.errors, Ordering::Relaxed);
        self.errors_4xx
            .store(persisted.errors_4xx, Ordering::Relaxed);
        self.errors_5xx
            .store(persisted.errors_5xx, Ordering::Relaxed);
        self.article_bytes_total
            .store(persisted.article_bytes_total, Ordering::Relaxed);
        self.article_count
            .store(persisted.article_count, Ordering::Relaxed);
        self.ttfb_micros_total
            .store(persisted.ttfb_micros_total, Ordering::Relaxed);
        self.ttfb_count
            .store(persisted.ttfb_count, Ordering::Relaxed);
        self.send_micros_total
            .store(persisted.send_micros_total, Ordering::Relaxed);
        self.recv_micros_total
            .store(persisted.recv_micros_total, Ordering::Relaxed);
        self.connection_failures
            .store(persisted.connection_failures, Ordering::Relaxed);
    }
}

// ============================================================================
// MetricsStore - Persistable metrics storage
// ============================================================================

/// Persistable metrics storage
///
/// This struct holds all cumulative counters that should survive restarts.
/// It can be serialized to JSON and restored on startup.
#[derive(Debug)]
pub struct MetricsStore {
    pub total_connections: AtomicU64,
    pub backend_stores: Vec<BackendStore>,
    /// User metrics (keyed by username) - uses DashMap for concurrent access
    pub user_metrics: dashmap::DashMap<String, UserMetrics>,
    pub pipeline_batches: AtomicU64,
    pub pipeline_commands: AtomicU64,
    pub pipeline_requests_queued: AtomicU64,
    pub pipeline_requests_completed: AtomicU64,
}

/// User metrics (internal storage)
#[derive(Debug, Clone, Default)]
pub struct UserMetrics {
    pub username: String,
    pub active_connections: usize, // Not persisted (live gauge)
    pub total_connections: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub total_commands: u64,
    pub errors: u64,
}

impl UserMetrics {
    pub fn new(username: String) -> Self {
        Self {
            username,
            active_connections: 0,
            total_connections: 0,
            bytes_sent: 0,
            bytes_received: 0,
            total_commands: 0,
            errors: 0,
        }
    }

    /// Convert to UserStats for snapshot (used by collector)
    pub(crate) fn to_user_stats(&self) -> crate::metrics::UserStats {
        use crate::metrics::types::{CommandCount, ErrorCount};
        use crate::types::{BytesPerSecondRate, BytesReceived, BytesSent, TotalConnections};
        crate::metrics::UserStats {
            username: self.username.clone(),
            active_connections: self.active_connections,
            total_connections: TotalConnections::new(self.total_connections),
            bytes_sent: BytesSent::new(self.bytes_sent),
            bytes_received: BytesReceived::new(self.bytes_received),
            total_commands: CommandCount::new(self.total_commands),
            errors: ErrorCount::new(self.errors),
            bytes_sent_per_sec: BytesPerSecondRate::ZERO,
            bytes_received_per_sec: BytesPerSecondRate::ZERO,
        }
    }

    fn to_persisted(&self) -> PersistedUser {
        PersistedUser {
            username: self.username.clone(),
            total_connections: self.total_connections,
            bytes_sent: self.bytes_sent,
            bytes_received: self.bytes_received,
            total_commands: self.total_commands,
            errors: self.errors,
        }
    }

    fn restore_from(&mut self, persisted: &PersistedUser) {
        self.total_connections = persisted.total_connections;
        self.bytes_sent = persisted.bytes_sent;
        self.bytes_received = persisted.bytes_received;
        self.total_commands = persisted.total_commands;
        self.errors = persisted.errors;
        // active_connections intentionally not restored (live gauge)
    }
}

impl MetricsStore {
    /// Create fresh store with N backends
    pub fn new(num_backends: usize) -> Self {
        Self {
            total_connections: AtomicU64::new(0),
            backend_stores: (0..num_backends).map(|_| BackendStore::default()).collect(),
            user_metrics: dashmap::DashMap::new(),
            pipeline_batches: AtomicU64::new(0),
            pipeline_commands: AtomicU64::new(0),
            pipeline_requests_queued: AtomicU64::new(0),
            pipeline_requests_completed: AtomicU64::new(0),
        }
    }

    /// Load from persisted file, mapping backends by server name
    ///
    /// Returns Ok(None) if file doesn't exist or is corrupt (logs warning, starts fresh).
    /// Returns Ok(Some(store)) on successful load.
    pub fn load(path: &Path, server_names: &[String]) -> Result<Option<Self>> {
        // File doesn't exist - start fresh
        if !path.exists() {
            tracing::info!("No stats file found at {:?}, starting fresh", path);
            return Ok(None);
        }

        // Read file
        let content = fs::read_to_string(path).context("Failed to read stats file")?;

        // Parse JSON
        let stats_file: StatsFile = match serde_json::from_str(&content) {
            Ok(file) => file,
            Err(e) => {
                tracing::warn!("Corrupt stats file at {:?} ({}), starting fresh", path, e);
                return Ok(None);
            }
        };

        // Check version
        if stats_file.version != STATS_FILE_VERSION {
            tracing::warn!(
                "Unknown stats format version {} (expected {}), starting fresh",
                stats_file.version,
                STATS_FILE_VERSION
            );
            return Ok(None);
        }

        // Create store with current backend count
        let store = Self::new(server_names.len());

        // Restore global counters
        store
            .total_connections
            .store(stats_file.global.total_connections, Ordering::Relaxed);

        // Restore pipeline stats
        store
            .pipeline_batches
            .store(stats_file.pipeline.batches, Ordering::Relaxed);
        store
            .pipeline_commands
            .store(stats_file.pipeline.commands, Ordering::Relaxed);
        store
            .pipeline_requests_queued
            .store(stats_file.pipeline.requests_queued, Ordering::Relaxed);
        store
            .pipeline_requests_completed
            .store(stats_file.pipeline.requests_completed, Ordering::Relaxed);

        // Restore backend stats by matching names
        for persisted_backend in &stats_file.backends {
            if let Some(index) = server_names
                .iter()
                .position(|name| name == &persisted_backend.name)
            {
                store.backend_stores[index].restore_from(persisted_backend);
                tracing::debug!(
                    "Restored backend stats for '{}' at index {}",
                    persisted_backend.name,
                    index
                );
            } else {
                tracing::debug!(
                    "Skipping backend '{}' (not in current config)",
                    persisted_backend.name
                );
            }
        }

        // Restore user stats
        for persisted_user in &stats_file.users {
            let mut user = UserMetrics::new(persisted_user.username.clone());
            user.restore_from(persisted_user);
            store
                .user_metrics
                .insert(persisted_user.username.clone(), user);
        }

        tracing::info!(
            "Loaded stats from {:?} (saved at {})",
            path,
            stats_file.saved_at
        );
        Ok(Some(store))
    }

    /// Save to file atomically (tmp + rename)
    pub fn save(&self, path: &Path, server_names: &[String]) -> Result<()> {
        // Build backends list with bounds-safe indexing
        let backends = server_names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                self.backend_stores
                    .get(i)
                    .with_context(|| {
                        format!(
                            "Backend store index {} out of bounds (stores: {}, names: {})",
                            i,
                            self.backend_stores.len(),
                            server_names.len()
                        )
                    })
                    .map(|store| store.to_persisted(name))
            })
            .collect::<Result<Vec<_>>>()?
            .into();

        // Create StatsFile
        let stats_file = StatsFile {
            version: STATS_FILE_VERSION,
            saved_at: chrono::Utc::now().to_rfc3339(),
            global: PersistedGlobal {
                total_connections: self.total_connections.load(Ordering::Relaxed),
            },
            backends,
            users: self
                .user_metrics
                .iter()
                .map(|entry| entry.value().to_persisted())
                .collect(),
            pipeline: PersistedPipeline {
                batches: self.pipeline_batches.load(Ordering::Relaxed),
                commands: self.pipeline_commands.load(Ordering::Relaxed),
                requests_queued: self.pipeline_requests_queued.load(Ordering::Relaxed),
                requests_completed: self.pipeline_requests_completed.load(Ordering::Relaxed),
            },
        };

        // Serialize to JSON
        let json =
            serde_json::to_string_pretty(&stats_file).context("Failed to serialize stats")?;

        // Write to a unique temp file to avoid racing with concurrent saves or shutdown saves.
        // Each call gets a distinct name (stats.json.N.tmp) so concurrent calls don't clobber
        // each other's in-progress write before atomically renaming to the final path.
        let seq = SAVE_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp_filename = format!(
            "{}.{}.tmp",
            path.file_name().unwrap_or_default().to_string_lossy(),
            seq
        );
        let tmp_path = path.with_file_name(tmp_filename);
        fs::write(&tmp_path, json)
            .with_context(|| format!("Failed to write stats to {:?}", tmp_path))?;

        // Best-effort atomic replace: remove existing file (for Windows compatibility) then rename temp file
        if path.exists() {
            fs::remove_file(path)
                .with_context(|| format!("Failed to remove existing stats file at {:?}", path))?;
        }
        fs::rename(&tmp_path, path)
            .with_context(|| format!("Failed to rename {:?} to {:?}", tmp_path, path))?;

        tracing::debug!("Saved stats to {:?}", path);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_backend_store_roundtrip() {
        let store = BackendStore::default();

        // Set some values
        store.total_commands.store(100, Ordering::Relaxed);
        store.bytes_sent.store(50000, Ordering::Relaxed);
        store.errors.store(5, Ordering::Relaxed);

        // Convert to persisted
        let persisted = store.to_persisted("test-backend");
        assert_eq!(persisted.name, "test-backend");
        assert_eq!(persisted.total_commands, 100);
        assert_eq!(persisted.bytes_sent, 50000);
        assert_eq!(persisted.errors, 5);

        // Restore to new store
        let new_store = BackendStore::default();
        new_store.restore_from(&persisted);
        assert_eq!(new_store.total_commands.load(Ordering::Relaxed), 100);
        assert_eq!(new_store.bytes_sent.load(Ordering::Relaxed), 50000);
        assert_eq!(new_store.errors.load(Ordering::Relaxed), 5);
    }

    #[test]
    fn test_user_metrics_roundtrip() {
        let mut user = UserMetrics::new("alice".to_string());
        user.total_connections = 10;
        user.bytes_sent = 1000;
        user.active_connections = 5; // Live gauge, should not be persisted

        let persisted = user.to_persisted();
        assert_eq!(persisted.username, "alice");
        assert_eq!(persisted.total_connections, 10);
        assert_eq!(persisted.bytes_sent, 1000);

        // Restore
        let mut new_user = UserMetrics::new("alice".to_string());
        new_user.restore_from(&persisted);
        assert_eq!(new_user.total_connections, 10);
        assert_eq!(new_user.bytes_sent, 1000);
        assert_eq!(new_user.active_connections, 0); // Not restored
    }

    #[test]
    fn test_metrics_store_save_load_roundtrip() {
        let temp_dir = TempDir::new().unwrap();
        let stats_path = temp_dir.path().join("stats.json");
        let server_names = vec!["backend1".to_string(), "backend2".to_string()];

        // Create and populate store
        let store = MetricsStore::new(2);
        store.total_connections.store(42, Ordering::Relaxed);
        store.backend_stores[0]
            .total_commands
            .store(100, Ordering::Relaxed);
        store.backend_stores[1]
            .total_commands
            .store(200, Ordering::Relaxed);
        store.pipeline_batches.store(10, Ordering::Relaxed);

        let mut user = UserMetrics::new("bob".to_string());
        user.total_connections = 5;
        store.user_metrics.insert("bob".to_string(), user);

        // Save
        store.save(&stats_path, &server_names).unwrap();

        // Load
        let loaded = MetricsStore::load(&stats_path, &server_names)
            .unwrap()
            .expect("Should load");

        // Verify
        assert_eq!(loaded.total_connections.load(Ordering::Relaxed), 42);
        assert_eq!(
            loaded.backend_stores[0]
                .total_commands
                .load(Ordering::Relaxed),
            100
        );
        assert_eq!(
            loaded.backend_stores[1]
                .total_commands
                .load(Ordering::Relaxed),
            200
        );
        assert_eq!(loaded.pipeline_batches.load(Ordering::Relaxed), 10);

        let bob = loaded.user_metrics.get("bob").unwrap();
        assert_eq!(bob.total_connections, 5);
    }

    #[test]
    fn test_metrics_store_backend_name_mapping() {
        let temp_dir = TempDir::new().unwrap();
        let stats_path = temp_dir.path().join("stats.json");

        // Original config: backend1, backend2, backend3
        let orig_names = vec![
            "backend1".to_string(),
            "backend2".to_string(),
            "backend3".to_string(),
        ];
        let store = MetricsStore::new(3);
        store.backend_stores[0]
            .total_commands
            .store(100, Ordering::Relaxed);
        store.backend_stores[1]
            .total_commands
            .store(200, Ordering::Relaxed);
        store.backend_stores[2]
            .total_commands
            .store(300, Ordering::Relaxed);
        store.save(&stats_path, &orig_names).unwrap();

        // New config: backend3, backend1 (backend2 removed, reordered)
        let new_names = vec!["backend3".to_string(), "backend1".to_string()];
        let loaded = MetricsStore::load(&stats_path, &new_names)
            .unwrap()
            .expect("Should load");

        // backend3 is now at index 0 (was index 2)
        assert_eq!(
            loaded.backend_stores[0]
                .total_commands
                .load(Ordering::Relaxed),
            300
        );
        // backend1 is now at index 1 (was index 0)
        assert_eq!(
            loaded.backend_stores[1]
                .total_commands
                .load(Ordering::Relaxed),
            100
        );
    }

    #[test]
    fn test_metrics_store_missing_file_returns_none() {
        let temp_dir = TempDir::new().unwrap();
        let stats_path = temp_dir.path().join("nonexistent.json");
        let server_names = vec!["backend1".to_string()];

        let result = MetricsStore::load(&stats_path, &server_names).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_metrics_store_corrupt_file_returns_none() {
        let temp_dir = TempDir::new().unwrap();
        let stats_path = temp_dir.path().join("corrupt.json");

        // Write invalid JSON
        fs::write(&stats_path, "{ not valid json }").unwrap();

        let server_names = vec!["backend1".to_string()];
        let result = MetricsStore::load(&stats_path, &server_names).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_metrics_store_backend_mismatch() {
        let temp_dir = TempDir::new().unwrap();
        let stats_path = temp_dir.path().join("stats.json");

        // Save with backend1, backend2
        let orig_names = vec!["backend1".to_string(), "backend2".to_string()];
        let store = MetricsStore::new(2);
        store.backend_stores[0]
            .total_commands
            .store(100, Ordering::Relaxed);
        store.backend_stores[1]
            .total_commands
            .store(200, Ordering::Relaxed);
        store.save(&stats_path, &orig_names).unwrap();

        // Load with completely different backends
        let new_names = vec!["backend3".to_string(), "backend4".to_string()];
        let loaded = MetricsStore::load(&stats_path, &new_names)
            .unwrap()
            .expect("Should load");

        // New backends should have zero stats (no name match)
        assert_eq!(
            loaded.backend_stores[0]
                .total_commands
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            loaded.backend_stores[1]
                .total_commands
                .load(Ordering::Relaxed),
            0
        );
    }

    #[test]
    fn test_metrics_store_save_with_too_many_names() {
        let temp_dir = TempDir::new().unwrap();
        let stats_path = temp_dir.path().join("stats.json");

        // Store with 1 backend, but 2 names supplied
        let store = MetricsStore::new(1);
        let server_names = vec!["backend1".to_string(), "backend2".to_string()];

        // save() returns Err when server_names.len() > backend_stores.len()
        let result = store.save(&stats_path, &server_names);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("out of bounds"), "error was: {err}");
    }
}
