//! Connection pool prewarming functionality
//!
//! This module handles warming up connection pools by creating each backend's
//! configured startup connections concurrently, ensuring they're ready before
//! accepting clients.

use anyhow::Result;
use tracing::{debug, info, warn};

use crate::config::Server;
use crate::pool::DeadpoolConnectionProvider;

/// Connections to open at startup for `server`.
///
/// Validation already rejects values above `max_connections`; the cap still
/// matters for unvalidated configs because requests beyond the pool size would
/// wait on the pool instead of opening connections.
fn prewarm_target(server: &Server) -> usize {
    let max_connections = server.max_connections.get();
    server
        .prewarm_connections
        .map_or(max_connections, |count| count.min(max_connections))
}

/// Prewarm a single pool by creating `target` connections concurrently
async fn prewarm_single_pool(
    provider: DeadpoolConnectionProvider,
    server_name: String,
    target: usize,
) -> Result<usize> {
    if target == 0 {
        info!(
            "Not prewarming pool for '{}': connections open on demand",
            server_name
        );
        return Ok(0);
    }

    info!(
        "Prewarming pool for '{}' with {} connections",
        server_name, target
    );

    // Create all connections concurrently
    let tasks: Vec<_> = (0..target)
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
                            target,
                            server_name
                        );
                    })
                    .ok()
            })
        })
        .collect();

    // Wait for all connections and count successes
    let mut connections = Vec::with_capacity(target);
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
        server_name, created, target
    );

    Ok(created)
}

/// Prewarm all connection pools before accepting clients
/// Creates each pool's startup connections concurrently across all pools
///
/// # Errors
/// Returns any task join failure or backend prewarming error from the
/// underlying providers.
pub async fn prewarm_pools(
    providers: &[DeadpoolConnectionProvider],
    servers: &[Server],
) -> Result<()> {
    info!("Prewarming all connection pools...");

    // Prewarm all pools concurrently
    let tasks: Vec<_> = servers
        .iter()
        .enumerate()
        .map(|(i, server)| {
            let provider = providers[i].clone();
            let server_name = server.name.to_string();
            let target = prewarm_target(server);

            tokio::spawn(prewarm_single_pool(provider, server_name, target))
        })
        .collect();

    // Wait for all pools and collect results
    let mut total_created = 0;
    let mut total_expected = 0;

    for (task, server) in tasks.into_iter().zip(servers.iter()) {
        total_expected += prewarm_target(server);
        match task.await {
            Ok(Ok(created)) => total_created += created,
            Ok(Err(e)) => warn!(
                "Failed to prewarm pool for '{}': {}",
                server.name.as_str(),
                e
            ),
            Err(e) => warn!(
                "Prewarming task panicked for '{}': {}",
                server.name.as_str(),
                e
            ),
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
    use crate::config::Server;
    use tokio::net::TcpListener;

    /// Spawn a simple mock NNTP server
    async fn spawn_mock_server_on_random_port() -> (u16, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
        });
        (port, handle)
    }

    #[tokio::test]
    async fn test_prewarm_pools_basic() {
        let (port, _server) = spawn_mock_server_on_random_port().await;

        let servers = vec![
            Server::builder("127.0.0.1", crate::types::Port::try_new(port).unwrap())
                .name("TestServer1")
                .max_connections(crate::types::MaxConnections::try_new(2).unwrap())
                .build()
                .unwrap(),
        ];

        let providers = servers
            .iter()
            .map(|s| {
                crate::pool::DeadpoolConnectionProvider::new(
                    s.host.to_string(),
                    s.port.get(),
                    s.name.to_string(),
                    s.max_connections.get(),
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

    #[tokio::test]
    async fn test_prewarm_pools_multiple_servers() {
        // Start two mock servers
        let (port1, _server1) = spawn_mock_server_on_random_port().await;
        let (port2, _server2) = spawn_mock_server_on_random_port().await;

        let servers = vec![
            Server::builder("127.0.0.1", crate::types::Port::try_new(port1).unwrap())
                .name("Server1")
                .max_connections(crate::types::MaxConnections::try_new(2).unwrap())
                .build()
                .unwrap(),
            Server::builder("127.0.0.1", crate::types::Port::try_new(port2).unwrap())
                .name("Server2")
                .max_connections(crate::types::MaxConnections::try_new(1).unwrap())
                .build()
                .unwrap(),
        ];

        let providers = servers
            .iter()
            .map(|s| {
                crate::pool::DeadpoolConnectionProvider::new(
                    s.host.to_string(),
                    s.port.get(),
                    s.name.to_string(),
                    s.max_connections.get(),
                    s.username.clone(),
                    s.password.clone(),
                )
            })
            .collect::<Vec<_>>();

        let result = prewarm_pools(&providers, &servers).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_prewarm_pools_with_unreachable_server() {
        // Use a port that won't have a server (99999 is invalid)
        let bad_port = 65500; // High port unlikely to be in use

        let servers = vec![
            Server::builder("127.0.0.1", crate::types::Port::try_new(bad_port).unwrap())
                .name("UnreachableServer")
                .max_connections(crate::types::MaxConnections::try_new(1).unwrap())
                .build()
                .unwrap(),
        ];

        let providers = servers
            .iter()
            .map(|s| {
                crate::pool::DeadpoolConnectionProvider::new(
                    s.host.to_string(),
                    s.port.get(),
                    s.name.to_string(),
                    s.max_connections.get(),
                    s.username.clone(),
                    s.password.clone(),
                )
            })
            .collect::<Vec<_>>();

        // Prewarming should not fail even if server is unreachable
        // It logs warnings but returns Ok
        let result = prewarm_pools(&providers, &servers).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_prewarm_pools_empty() {
        // Test with no servers
        let servers = vec![];
        let providers = vec![];

        let result = prewarm_pools(&providers, &servers).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_prewarm_single_pool_success() {
        let (port, _server) = spawn_mock_server_on_random_port().await;

        let provider = crate::pool::DeadpoolConnectionProvider::new(
            "127.0.0.1".to_string(),
            port,
            "TestServer".to_string(),
            3,
            None,
            None,
        );

        let result = prewarm_single_pool(provider, "TestServer".to_string(), 3).await;

        assert!(result.is_ok());
        let created = result.unwrap();
        assert_eq!(created, 3);
    }

    #[tokio::test]
    async fn test_prewarm_single_pool_single_connection() {
        let (port, _server) = spawn_mock_server_on_random_port().await;

        let provider = crate::pool::DeadpoolConnectionProvider::new(
            "127.0.0.1".to_string(),
            port,
            "TestServer".to_string(),
            1,
            None,
            None,
        );

        let result = prewarm_single_pool(provider, "TestServer".to_string(), 1).await;

        assert!(result.is_ok());
        let created = result.unwrap();
        assert_eq!(created, 1);
    }

    #[tokio::test]
    async fn test_prewarm_single_pool_zero_connections() {
        let provider = crate::pool::DeadpoolConnectionProvider::new(
            "127.0.0.1".to_string(),
            1,
            "TestServer".to_string(),
            0, // Zero connections
            None,
            None,
        );

        let result = prewarm_single_pool(provider, "TestServer".to_string(), 0).await;

        assert!(result.is_ok());
        let created = result.unwrap();
        assert_eq!(created, 0);
    }

    #[tokio::test]
    async fn test_prewarm_pools_mixed_success_failure() {
        let (good_port, _server) = spawn_mock_server_on_random_port().await;
        let bad_port = 65499;

        let servers = vec![
            Server::builder("127.0.0.1", crate::types::Port::try_new(good_port).unwrap())
                .name("GoodServer")
                .max_connections(crate::types::MaxConnections::try_new(1).unwrap())
                .build()
                .unwrap(),
            Server::builder("127.0.0.1", crate::types::Port::try_new(bad_port).unwrap())
                .name("BadServer")
                .max_connections(crate::types::MaxConnections::try_new(1).unwrap())
                .build()
                .unwrap(),
        ];

        let providers = servers
            .iter()
            .map(|s| {
                crate::pool::DeadpoolConnectionProvider::new(
                    s.host.to_string(),
                    s.port.get(),
                    s.name.to_string(),
                    s.max_connections.get(),
                    s.username.clone(),
                    s.password.clone(),
                )
            })
            .collect::<Vec<_>>();

        // Should still return Ok even with partial failures
        let result = prewarm_pools(&providers, &servers).await;
        assert!(result.is_ok());
    }

    fn server_with_prewarm(port: u16, max: usize, prewarm: Option<usize>) -> Server {
        let builder = Server::builder("127.0.0.1", crate::types::Port::try_new(port).unwrap())
            .name("PrewarmServer")
            .max_connections(crate::types::MaxConnections::try_new(max).unwrap());
        match prewarm {
            Some(count) => builder.prewarm_connections(count),
            None => builder,
        }
        .build()
        .unwrap()
    }

    #[test]
    fn prewarm_target_defaults_to_whole_pool_and_caps_at_max() {
        assert_eq!(prewarm_target(&server_with_prewarm(119, 8, None)), 8);
        assert_eq!(prewarm_target(&server_with_prewarm(119, 8, Some(3))), 3);
        assert_eq!(prewarm_target(&server_with_prewarm(119, 8, Some(0))), 0);
        assert_eq!(prewarm_target(&server_with_prewarm(119, 8, Some(50))), 8);
    }

    /// Mock backend that counts accepted TCP connections.
    async fn spawn_counting_server() -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = accepted.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let _ = stream.write_all(b"200 Mock Server Ready\r\n").await;
                    let mut buffer = [0; 1024];
                    while let Ok(n) = stream.read(&mut buffer).await {
                        if n == 0 || stream.write_all(b"200 OK\r\n").await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        (port, accepted)
    }

    async fn connections_opened_by_prewarm(max: usize, prewarm: Option<usize>) -> usize {
        let (port, accepted) = spawn_counting_server().await;
        let servers = vec![server_with_prewarm(port, max, prewarm)];
        let providers = servers
            .iter()
            .map(|s| {
                crate::pool::DeadpoolConnectionProvider::new(
                    s.host.to_string(),
                    s.port.get(),
                    s.name.to_string(),
                    s.max_connections.get(),
                    s.username.clone(),
                    s.password.clone(),
                )
            })
            .collect::<Vec<_>>();

        prewarm_pools(&providers, &servers).await.unwrap();
        accepted.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[tokio::test]
    async fn prewarm_opens_whole_pool_by_default() {
        assert_eq!(connections_opened_by_prewarm(4, None).await, 4);
    }

    #[tokio::test]
    async fn prewarm_opens_only_configured_count() {
        assert_eq!(connections_opened_by_prewarm(4, Some(1)).await, 1);
    }

    #[tokio::test]
    async fn prewarm_zero_opens_nothing() {
        assert_eq!(connections_opened_by_prewarm(4, Some(0)).await, 0);
    }
}
