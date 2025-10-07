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

/// Manages connection pool prewarming state and operations
#[derive(Debug, Clone)]
pub struct PoolPrewarmer {
    /// Connection providers for each backend
    providers: Vec<DeadpoolConnectionProvider>,
    /// Server configurations  
    servers: Vec<ServerConfig>,
    /// Track if pools have been prewarmed to avoid redundant prewarming
    /// Wrapped in Arc to share state across clones
    prewarmed: Arc<AtomicBool>,
}

impl PoolPrewarmer {
    /// Create a new pool prewarmer
    pub fn new(
        providers: &[DeadpoolConnectionProvider],
        servers: &[ServerConfig],
    ) -> Self {
        Self {
            providers: providers.to_vec(),
            servers: servers.to_vec(),
            prewarmed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Prewarm a single pool by creating all connections concurrently
    async fn prewarm_single_pool(
        provider: DeadpoolConnectionProvider,
        server_name: String,
        max_connections: usize,
    ) -> Result<()> {
        info!(
            "Prewarming pool for '{}' with {} connections (concurrent creation)",
            server_name, max_connections
        );

        // Create all connections concurrently by spawning tasks
        let mut tasks = Vec::with_capacity(max_connections);
        for i in 0..max_connections {
            let provider = provider.clone();
            let server_name = server_name.clone();
            
            tasks.push(tokio::spawn(async move {
                match provider.get_pooled_connection().await {
                    Ok(conn) => {
                        debug!(
                            "Created prewarmed connection {}/{} for '{}'",
                            i + 1,
                            max_connections,
                            server_name
                        );
                        Some(conn)
                    }
                    Err(e) => {
                        warn!(
                            "Failed to prewarm connection {}/{} for '{}': {}",
                            i + 1,
                            max_connections,
                            server_name,
                            e
                        );
                        None
                    }
                }
            }));
        }

        // Wait for all tasks and collect connections
        let mut connections = Vec::new();
        for task in tasks {
            if let Ok(Some(conn)) = task.await {
                connections.push(conn);
            }
        }

        let total_created = connections.len();
        
        // Drop all connections - they all return to pool as available
        drop(connections);
        
        info!(
            "Pool prewarming completed for '{}' - {}/{} connections ready",
            server_name, total_created, max_connections
        );
        Ok(())
    }

    /// Prewarm all connection pools by forcing pool to create max_connections
    /// Should be called before accepting client connections
    pub async fn prewarm_all(&self) -> Result<()> {
        // Use compare_and_swap to ensure only one thread prewarms the pools
        if self
            .prewarmed
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Ok(()); // Already prewarmed
        }

        info!("Prewarming all connection pools before accepting clients...");

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
