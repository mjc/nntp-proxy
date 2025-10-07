//! Connection pool prewarming functionality
//!
//! This module handles warming up connection pools by creating all connections
//! concurrently at startup, ensuring they're ready before accepting clients.

use anyhow::Result;
use tracing::{debug, info, warn};

use crate::config::ServerConfig;
use crate::pool::DeadpoolConnectionProvider;

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
/// Creates all connections concurrently across all pools
pub async fn prewarm_pools(
    providers: &[DeadpoolConnectionProvider],
    servers: &[ServerConfig],
) -> Result<()> {
    info!("Prewarming all connection pools...");

    // Prewarm all pools concurrently
    let tasks: Vec<_> = servers
        .iter()
        .enumerate()
        .map(|(i, server)| {
            let provider = providers[i].clone();
            let server_name = server.name.clone();
            let max_connections = server.max_connections as usize;

            tokio::spawn(prewarm_single_pool(
                provider,
                server_name,
                max_connections,
            ))
        })
        .collect();

    // Wait for all pools and collect results
    let mut total_created = 0;
    let mut total_expected = 0;
    
    for (task, server) in tasks.into_iter().zip(servers.iter()) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerConfig;

    #[tokio::test]
    async fn test_prewarm_pools_basic() {
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

        // Verify function accepts the arguments
        let result = prewarm_pools(&providers, &servers).await;
        
        // Should not fail even if connections fail (just logs warnings)
        assert!(result.is_ok());
    }
}
