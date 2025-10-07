//! Connection pool prewarming functionality
//!
//! This module handles warming up connection pools by creating all connections
//! concurrently at startup, ensuring they're ready before accepting clients.

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
    ) -> Result<usize> {
        info!(
            "Prewarming pool for '{}' with {} connections",
            server_name, max_connections
        );

        // Create all connections concurrently
        let tasks: Vec<_> = (0..max_connections)
            .map(|i| {
                let provider = provider.clone();
                let server_name = server_name.clone();
                
                tokio::spawn(async move {
                    provider.get_pooled_connection().await.map(|conn| {
                        debug!(
                            "Created connection {}/{} for '{}'",
                            i + 1, max_connections, server_name
                        );
                        conn
                    }).ok()
                })
            })
            .collect();

        // Wait for all connections and count successes
        let mut connections = Vec::with_capacity(max_connections);
        for task in tasks {
            if let Ok(Some(conn)) = task.await {
                connections.push(conn);
            }
        }

        let created = connections.len();
        
        // Drop all connections - they return to pool as available
        drop(connections);
        
        info!(
            "Pool '{}' ready: {}/{} connections created",
            server_name, created, max_connections
        );
        
        Ok(created)
    }

    /// Prewarm all connection pools before accepting clients
    /// This is the only method you need to call - it creates all connections concurrently
    pub async fn prewarm_all(&self) -> Result<()> {
        // Ensure we only prewarm once
        if self.prewarmed.swap(true, Ordering::Relaxed) {
            return Ok(()); // Already prewarmed
        }

        info!("Prewarming all connection pools...");

        // Prewarm all pools concurrently
        let tasks: Vec<_> = self.servers
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

        // Wait for all pools and collect results
        let mut total_created = 0;
        let mut total_expected = 0;
        
        for (task, server) in tasks.into_iter().zip(self.servers.iter()) {
            total_expected += server.max_connections as usize;
            match task.await {
                Ok(Ok(created)) => total_created += created,
                Ok(Err(e)) => warn!("Failed to prewarm pool for '{}': {}", server.name, e),
                Err(e) => warn!("Prewarming task panicked for '{}': {}", server.name, e),
            }
        }

        info!(
            "Prewarming complete: {}/{} connections ready across all pools",
            total_created, total_expected
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerConfig;

    #[tokio::test]
    async fn test_pool_prewarmer_creation() {
        let servers = vec![
            ServerConfig {
                name: "TestServer1".to_string(),
                host: "example.com".to_string(),
                port: 119,
                max_connections: 10,
                username: None,
                password: None,
            },
        ];

        let providers = servers
            .iter()
            .map(|s| {
                crate::pool::DeadpoolConnectionProvider::new(
                    s.host.clone(),
                    s.port,
                    s.name.clone(),
                    10,
                    s.username.clone(),
                    s.password.clone(),
                )
            })
            .collect::<Vec<_>>();

        let prewarmer = PoolPrewarmer::new(&providers, &servers);
        
        // Verify it was created
        assert_eq!(prewarmer.providers.len(), 1);
        assert_eq!(prewarmer.servers.len(), 1);
        assert!(!prewarmer.prewarmed.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn test_prewarmer_idempotent() {
        let servers = vec![
            ServerConfig {
                name: "TestServer".to_string(),
                host: "localhost".to_string(),
                port: 9999, // Non-existent port
                max_connections: 2,
                username: None,
                password: None,
            },
        ];

        let providers = servers
            .iter()
            .map(|s| {
                crate::pool::DeadpoolConnectionProvider::new(
                    s.host.clone(),
                    s.port,
                    s.name.clone(),
                    2,
                    s.username.clone(),
                    s.password.clone(),
                )
            })
            .collect::<Vec<_>>();

        let prewarmer = PoolPrewarmer::new(&providers, &servers);

        // First call should attempt prewarming
        let result1 = prewarmer.prewarm_all().await;
        assert!(result1.is_ok()); // Should not fail even if connections fail

        // Second call should return immediately
        let result2 = prewarmer.prewarm_all().await;
        assert!(result2.is_ok());

        // Flag should be set
        assert!(prewarmer.prewarmed.load(Ordering::Relaxed));
    }
}
