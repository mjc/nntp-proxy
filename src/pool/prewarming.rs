//! Connection pool prewarming functionality
//!
//! This module handles warming up connection pools by creating connections
//! in batches to avoid overwhelming backend servers during startup.

use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{debug, info, warn};

use crate::config::ServerConfig;
use crate::pool::DeadpoolConnectionProvider;
use crate::protocol::{BATCH_DELAY_MS, PREWARMING_BATCH_SIZE};

/// Manages connection pool prewarming state and operations
#[derive(Debug, Clone)]
pub struct PoolPrewarmer {
    /// Connection providers for each backend
    providers: Arc<Vec<DeadpoolConnectionProvider>>,
    /// Server configurations
    servers: Arc<Vec<ServerConfig>>,
    /// Track if pools have been prewarmed to avoid redundant prewarming
    prewarmed: Arc<AtomicBool>,
}

impl PoolPrewarmer {
    /// Create a new pool prewarmer
    pub fn new(
        providers: Arc<Vec<DeadpoolConnectionProvider>>,
        servers: Arc<Vec<ServerConfig>>,
    ) -> Self {
        Self {
            providers,
            servers,
            prewarmed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Prewarm a single pool by creating connections in batches
    async fn prewarm_single_pool(
        provider: DeadpoolConnectionProvider,
        server_name: String,
        max_connections: usize,
    ) -> Result<()> {
        info!(
            "Prewarming pool for '{}' with {} connections",
            server_name, max_connections
        );

        let mut total_created = 0;

        for batch_start in (0..max_connections).step_by(PREWARMING_BATCH_SIZE) {
            let batch_end = std::cmp::min(batch_start + PREWARMING_BATCH_SIZE, max_connections);
            let mut batch_connections = Vec::new();

            // Create batch connections concurrently
            for i in batch_start..batch_end {
                match provider.get_pooled_connection().await {
                    Ok(conn) => {
                        debug!(
                            "Created prewarmed connection {}/{} for '{}'",
                            i + 1,
                            max_connections,
                            server_name
                        );
                        batch_connections.push(conn);
                        total_created += 1;
                    }
                    Err(e) => {
                        warn!(
                            "Failed to prewarm connection {}/{} for '{}': {}",
                            i + 1,
                            max_connections,
                            server_name,
                            e
                        );
                    }
                }
            }

            // Drop batch connections (return to pool)
            drop(batch_connections);

            // Wait between batches to avoid overwhelming server
            if batch_end < max_connections {
                tokio::time::sleep(std::time::Duration::from_millis(BATCH_DELAY_MS)).await;
            }
        }

        info!(
            "Pool prewarming completed for '{}' - {}/{} connections ready",
            server_name, total_created, max_connections
        );
        Ok(())
    }

    /// Prewarm all connection pools by forcing pool to create max_connections
    /// Called on first client connection after idle period
    pub async fn prewarm_all(&self) -> Result<()> {
        // Use compare_and_swap to ensure only one thread prewarms the pools
        if self
            .prewarmed
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Ok(()); // Another thread is already prewarming
        }

        info!("Prewarming all connection pools on first client connection...");

        // Spawn tasks to prewarm each pool concurrently
        let handles: Vec<_> = self
            .servers
            .iter()
            .enumerate()
            .map(|(i, server)| {
                let provider = self.providers[i].clone();
                let server_name = server.name.clone();
                let max_connections = server.max_connections as usize;

                tokio::spawn(Self::prewarm_single_pool(
                    provider,
                    server_name,
                    max_connections,
                ))
            })
            .collect();

        // Wait for all prewarming tasks to complete
        for handle in handles {
            if let Err(e) = handle.await {
                warn!("Pool prewarming task failed: {}", e);
            }
        }

        self.prewarmed.store(true, Ordering::Relaxed);
        info!("All connection pools have been prewarmed and are ready for use");

        Ok(())
    }

    /// Test connections to all backend servers
    pub async fn test_connections(&self) -> Result<()> {
        info!("Testing connections to all backend servers...");
        for (i, server) in self.servers.iter().enumerate() {
            let provider = &self.providers[i];
            // Test connection to ensure servers are reachable
            match provider.get_pooled_connection().await {
                Ok(_conn) => {
                    debug!("Successfully tested connection to {}", server.name);
                    // Connection returns to pool when _conn is dropped
                }
                Err(e) => {
                    warn!("Failed to test connection to {}: {}", server.name, e);
                }
            }
        }
        info!("Connection testing complete");
        Ok(())
    }
}
