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
            // Clone Arc references for each async task to satisfy Send + 'static bounds
            // Arc makes this cheap - only the pointer is cloned, not the underlying data
            let provider = provider.clone();
            let server_name = server_name.clone();

            tokio::spawn(async move {
                provider
                    .get_pooled_connection()
                    .await
                    .inspect(|_conn| {
                        debug!(
                            "Created connection {}/{} for '{}'",
                            i + 1,
                            max_connections,
                            server_name
                        );
                    })
                    .ok()
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

            tokio::spawn(prewarm_single_pool(provider, server_name, max_connections))
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
    use tokio::net::TcpListener;

    /// Helper to find an available port
    async fn find_available_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    }

    /// Spawn a simple mock NNTP server
    fn spawn_mock_server(port: u16) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let addr = format!("127.0.0.1:{}", port);
            let listener = match TcpListener::bind(&addr).await {
                Ok(l) => l,
                Err(_) => return,
            };

            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    // Send greeting
                    let _ = stream.write_all(b"200 Mock Server Ready\r\n").await;

                    // Handle commands
                    let mut buffer = [0; 1024];
                    while let Ok(n) = stream.read(&mut buffer).await {
                        if n == 0 {
                            break;
                        }
                        if buffer[..n].starts_with(b"QUIT") {
                            let _ = stream.write_all(b"205 Goodbye\r\n").await;
                            break;
                        }
                        let _ = stream.write_all(b"200 OK\r\n").await;
                    }
                });
            }
        })
    }

    #[tokio::test]
    async fn test_prewarm_pools_basic() {
        let port = find_available_port().await;

        // Start mock server
        let _server = spawn_mock_server(port);

        // Give server time to start
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let servers = vec![ServerConfig {
            name: "TestServer1".to_string(),
            host: "127.0.0.1".to_string(),
            port,
            max_connections: 2,
            username: None,
            password: None,
            use_tls: false,
            tls_verify_cert: true,
            tls_cert_path: None,
        }];

        let providers = servers
            .iter()
            .map(|s| {
                crate::pool::DeadpoolConnectionProvider::new(
                    s.host.clone(),
                    s.port,
                    s.name.clone(),
                    s.max_connections as usize,
                    s.username.clone(),
                    s.password.clone(),
                )
            })
            .collect::<Vec<_>>();

        // Prewarm the pools
        let result = prewarm_pools(&providers, &servers).await;

        // Should succeed with mock server
        assert!(result.is_ok());
    }
}
