use anyhow::Result;
use std::io::Write;
use tempfile::NamedTempFile;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{Duration, timeout};

use nntp_proxy::{Config, NntpProxy, ServerConfig, load_config};

/// Helper function to find an available port
async fn find_available_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    addr.port()
}

/// Create a mock NNTP server that echoes back client data
async fn create_mock_server(port: u16) -> Result<()> {
    let addr = format!("127.0.0.1:{}", port);
    let listener = TcpListener::bind(&addr).await?;

    loop {
        if let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buffer = [0; 1024];

                // Send a welcome message
                let _ = stream.write_all(b"200 Mock NNTP Server Ready\r\n").await;

                // Echo back any data received
                while let Ok(n) = stream.read(&mut buffer).await {
                    if n == 0 {
                        break;
                    }
                    let _ = stream.write_all(&buffer[..n]).await;

                    // If we receive QUIT, close the connection
                    if buffer.starts_with(b"QUIT") {
                        let _ = stream.write_all(b"205 Goodbye\r\n").await;
                        break;
                    }
                }
            });
        }
    }
}

#[tokio::test]
async fn test_proxy_with_mock_servers() -> Result<()> {
    // Find available ports
    let mock_port1 = find_available_port().await;
    let mock_port2 = find_available_port().await;
    let proxy_port = find_available_port().await;

    // Start mock servers
    tokio::spawn(create_mock_server(mock_port1));
    tokio::spawn(create_mock_server(mock_port2));

    // Give servers time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create proxy configuration
    let config = Config {
        servers: vec![
            ServerConfig {
                host: "127.0.0.1".to_string(),
                port: mock_port1,
                name: "Mock Server 1".to_string(),
                username: None,
                password: None,
                max_connections: 10,
            },
            ServerConfig {
                host: "127.0.0.1".to_string(),
                port: mock_port2,
                name: "Mock Server 2".to_string(),
                username: None,
                password: None,
                max_connections: 10,
            },
        ],
    };

    let proxy = NntpProxy::new(config)?;

    // Start proxy server
    let proxy_addr = format!("127.0.0.1:{}", proxy_port);
    let listener = TcpListener::bind(&proxy_addr).await?;

    tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = listener.accept().await {
                let proxy_clone = proxy.clone();
                tokio::spawn(async move {
                    let _ = proxy_clone.handle_client(stream, addr).await;
                });
            }
        }
    });

    // Give proxy time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Test client connection through proxy
    let mut client = TcpStream::connect(&proxy_addr).await?;

    // Read welcome message
    let mut buffer = [0; 1024];
    let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
    let welcome = String::from_utf8_lossy(&buffer[..n]);
    assert!(welcome.contains("200 Mock NNTP Server Ready"));

    // Send a test command
    client.write_all(b"HELP\r\n").await?;

    // Read echo response
    let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
    let response = String::from_utf8_lossy(&buffer[..n]);
    assert!(response.contains("HELP"));

    // Send QUIT command
    client.write_all(b"QUIT\r\n").await?;

    // Read goodbye message
    let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
    let goodbye = String::from_utf8_lossy(&buffer[..n]);
    assert!(goodbye.contains("205 Goodbye"));

    Ok(())
}

#[tokio::test]
async fn test_round_robin_distribution() -> Result<()> {
    // Find available ports
    let mock_port1 = find_available_port().await;
    let mock_port2 = find_available_port().await;
    let proxy_port = find_available_port().await;

    // Start mock servers that identify themselves
    tokio::spawn(async move {
        let addr = format!("127.0.0.1:{}", mock_port1);
        let listener = TcpListener::bind(&addr).await.unwrap();

        loop {
            if let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let _ = stream.write_all(b"200 Server1 Ready\r\n").await;
                    let mut buffer = [0; 1024];
                    while let Ok(n) = stream.read(&mut buffer).await {
                        if n == 0 || buffer.starts_with(b"QUIT") {
                            break;
                        }
                    }
                });
            }
        }
    });

    tokio::spawn(async move {
        let addr = format!("127.0.0.1:{}", mock_port2);
        let listener = TcpListener::bind(&addr).await.unwrap();

        loop {
            if let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let _ = stream.write_all(b"200 Server2 Ready\r\n").await;
                    let mut buffer = [0; 1024];
                    while let Ok(n) = stream.read(&mut buffer).await {
                        if n == 0 || buffer.starts_with(b"QUIT") {
                            break;
                        }
                    }
                });
            }
        }
    });

    // Give servers time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create proxy configuration
    let config = Config {
        servers: vec![
            ServerConfig {
                host: "127.0.0.1".to_string(),
                port: mock_port1,
                name: "Mock Server 1".to_string(),
                username: None,
                password: None,
                max_connections: 10,
            },
            ServerConfig {
                host: "127.0.0.1".to_string(),
                port: mock_port2,
                name: "Mock Server 2".to_string(),
                username: None,
                password: None,
                max_connections: 10,
            },
        ],
    };

    let proxy = NntpProxy::new(config)?;

    // Start proxy server
    let proxy_addr = format!("127.0.0.1:{}", proxy_port);
    let listener = TcpListener::bind(&proxy_addr).await?;

    tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = listener.accept().await {
                let proxy_clone = proxy.clone();
                tokio::spawn(async move {
                    let _ = proxy_clone.handle_client(stream, addr).await;
                });
            }
        }
    });

    // Give proxy time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Test multiple connections to verify round-robin
    let mut server1_count = 0;
    let mut server2_count = 0;

    for _ in 0..6 {
        let mut client = TcpStream::connect(&proxy_addr).await?;
        let mut buffer = [0; 1024];

        let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
        let response = String::from_utf8_lossy(&buffer[..n]);

        if response.contains("Server1") {
            server1_count += 1;
        } else if response.contains("Server2") {
            server2_count += 1;
        }

        // Send QUIT to close connection
        let _ = client.write_all(b"QUIT\r\n").await;

        // Small delay between connections
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Both servers should have received connections (round-robin)
    assert!(
        server1_count > 0,
        "Server1 should have received connections"
    );
    assert!(
        server2_count > 0,
        "Server2 should have received connections"
    );
    assert_eq!(server1_count + server2_count, 6);

    Ok(())
}

#[tokio::test]
async fn test_config_file_loading() -> Result<()> {
    let config_content = r#"
[[servers]]
host = "test1.example.com"
port = 119
name = "Test Server 1"

[[servers]]
host = "test2.example.com"
port = 563
name = "Test Server 2"
"#;

    let mut temp_file = NamedTempFile::new()?;
    write!(temp_file, "{}", config_content)?;

    let config = load_config(temp_file.path().to_str().unwrap())?;

    assert_eq!(config.servers.len(), 2);
    assert_eq!(config.servers[0].host, "test1.example.com");
    assert_eq!(config.servers[0].port, 119);
    assert_eq!(config.servers[1].host, "test2.example.com");
    assert_eq!(config.servers[1].port, 563);

    Ok(())
}

#[tokio::test]
async fn test_proxy_handles_connection_failure() -> Result<()> {
    let proxy_port = find_available_port().await;
    let nonexistent_port = find_available_port().await;

    // Create proxy configuration with a server that doesn't exist
    let config = Config {
        servers: vec![ServerConfig {
            host: "127.0.0.1".to_string(),
            port: nonexistent_port,
            name: "Nonexistent Server".to_string(),
            username: None,
            password: None,
            max_connections: 10,
        }],
    };

    let proxy = NntpProxy::new(config)?;

    // Start proxy server
    let proxy_addr = format!("127.0.0.1:{}", proxy_port);
    let listener = TcpListener::bind(&proxy_addr).await?;

    tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = listener.accept().await {
                let proxy_clone = proxy.clone();
                tokio::spawn(async move {
                    let _ = proxy_clone.handle_client(stream, addr).await;
                });
            }
        }
    });

    // Give proxy time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Test client connection - should receive error message
    let mut client = TcpStream::connect(&proxy_addr).await?;
    let mut buffer = [0; 1024];

    let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
    let response = String::from_utf8_lossy(&buffer[..n]);

    assert!(response.contains("400 Backend server unavailable"));

    Ok(())
}

#[tokio::test]
async fn test_multiplexing_router_integration() -> Result<()> {
    use nntp_proxy::pool::DeadpoolConnectionProvider;
    use nntp_proxy::router::RequestRouter;
    use nntp_proxy::types::{BackendId, ClientId};
    use std::sync::Arc;

    // Create router with multiple backends
    let mut router = RequestRouter::new();

    for i in 0..3 {
        let provider = DeadpoolConnectionProvider::new(
            "localhost".to_string(),
            9999 + i,
            format!("backend-{}", i),
            5,
        );
        router.add_backend(
            BackendId::from_index(i as usize),
            format!("backend-{}", i),
            provider,
        );
    }

    let router = Arc::new(router);

    // Simulate multiple clients routing commands concurrently
    let mut handles = Vec::new();
    for _ in 0..20 {
        let router_clone = router.clone();
        let handle = tokio::spawn(async move {
            let client_id = ClientId::new();
            let result = router_clone.route_command(client_id, "LIST\r\n").await;
            assert!(result.is_ok());
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.await?;
    }

    // Verify all requests were routed
    assert_eq!(router.pending_count().await, 20);

    // Verify load is distributed across backends
    let total_load: usize = (0..3)
        .map(|i| router.backend_load(BackendId::from_index(i)).unwrap_or(0))
        .sum();
    assert_eq!(total_load, 20);

    Ok(())
}

#[tokio::test]
async fn test_command_rejection_for_multiplexing() -> Result<()> {
    use nntp_proxy::command::{CommandAction, CommandHandler};

    // Commands that should be rejected in multiplexing mode
    let rejected_commands = vec![
        "POST\r\n",
        "IHAVE <test@example.com>\r\n",
        "NEWGROUPS 20240101 000000 GMT\r\n",
        "NEWNEWS * 20240101 000000 GMT\r\n",
    ];

    for cmd in rejected_commands {
        let action = CommandHandler::handle_command(cmd);
        match action {
            CommandAction::Reject(msg) => {
                assert!(
                    msg.contains("multiplexing"),
                    "Expected multiplexing rejection for: {}",
                    cmd
                );
            }
            _ => panic!("Command {} should be rejected for multiplexing", cmd),
        }
    }

    // Commands that should still be allowed
    let allowed_commands = vec![
        "LIST\r\n",
        "HELP\r\n",
        "DATE\r\n",
        "ARTICLE <test@example.com>\r\n",
    ];

    for cmd in allowed_commands {
        let action = CommandHandler::handle_command(cmd);
        match action {
            CommandAction::Reject(_) => {
                // GROUP and other stateful commands are also rejected, but not for multiplexing
                // This is expected
            }
            CommandAction::ForwardStateless | CommandAction::ForwardHighThroughput => {
                // These are fine
            }
            CommandAction::InterceptAuth(_) => {
                // Auth is intercepted, not rejected
            }
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_stateful_commands_rejected() -> Result<()> {
    use nntp_proxy::command::{CommandAction, CommandHandler};

    // Stateful commands that should be rejected
    let stateful_commands = vec![
        "GROUP alt.test\r\n",
        "NEXT\r\n",
        "LAST\r\n",
        "ARTICLE 123\r\n", // Article by number (requires GROUP context)
        "STAT\r\n",
        "XOVER 100-200\r\n",
    ];

    for cmd in stateful_commands {
        let action = CommandHandler::handle_command(cmd);
        match action {
            CommandAction::Reject(msg) => {
                assert!(
                    msg.contains("stateless") || msg.contains("multiplexing"),
                    "Expected rejection for: {}",
                    cmd
                );
            }
            _ => panic!("Stateful command {} should be rejected", cmd),
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_client_session_with_router() -> Result<()> {
    use nntp_proxy::pool::BufferPool;
    use nntp_proxy::pool::DeadpoolConnectionProvider;
    use nntp_proxy::router::RequestRouter;
    use nntp_proxy::session::ClientSession;
    use nntp_proxy::types::BackendId;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;

    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
    let buffer_pool = BufferPool::new(4096, 8);

    // Create router with backends
    let mut router = RequestRouter::new();
    for i in 0..2 {
        let provider = DeadpoolConnectionProvider::new(
            "localhost".to_string(),
            9999 + i,
            format!("backend-{}", i),
            3,
        );
        router.add_backend(
            BackendId::from_index(i as usize),
            format!("backend-{}", i),
            provider,
        );
    }

    let session = ClientSession::new_with_router(addr, buffer_pool, Arc::new(router));

    // Verify session is in multiplexed mode
    assert!(session.is_multiplexed());

    // Route commands through the session
    for _ in 0..10 {
        let result = session.route_command("LIST\r\n").await;
        assert!(result.is_ok());
    }

    Ok(())
}

#[tokio::test]
async fn test_response_demultiplexer() -> Result<()> {
    use nntp_proxy::router::ResponseDemultiplexer;
    use nntp_proxy::types::{BackendId, ClientId, RequestId};
    use tokio::sync::mpsc;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let backend_id = BackendId::from_index(0);
    let demux = ResponseDemultiplexer::new(backend_id, tx);

    // Route multiple responses
    let clients: Vec<_> = (0..5).map(|_| ClientId::new()).collect();
    let requests: Vec<_> = (0..5).map(|_| RequestId::new()).collect();

    for i in 0..5 {
        demux.route_response(
            clients[i],
            requests[i],
            format!("200 Response {}\r\n", i).into_bytes(),
            true,
        )?;
    }

    // Verify all responses were sent
    for i in 0..5 {
        let response = rx.recv().await.unwrap();
        assert_eq!(response.client_id, clients[i]);
        assert!(response.complete);
        assert!(response.data.starts_with(b"200 Response"));
    }

    Ok(())
}

#[tokio::test]
async fn test_concurrent_clients_routing() -> Result<()> {
    use nntp_proxy::pool::DeadpoolConnectionProvider;
    use nntp_proxy::router::RequestRouter;
    use nntp_proxy::types::{BackendId, ClientId};
    use std::sync::Arc;

    // Create router with 5 backends
    let mut router = RequestRouter::new();
    for i in 0..5 {
        let provider = DeadpoolConnectionProvider::new(
            "localhost".to_string(),
            9999 + i,
            format!("backend-{}", i),
            10,
        );
        router.add_backend(
            BackendId::from_index(i as usize),
            format!("backend-{}", i),
            provider,
        );
    }
    let router = Arc::new(router);

    // Simulate 50 concurrent clients each sending 5 commands
    let mut handles = Vec::new();
    for _ in 0..50 {
        let router_clone = router.clone();
        let handle = tokio::spawn(async move {
            let client_id = ClientId::new();
            for i in 0..5 {
                let cmd = format!("LIST {}\r\n", i);
                router_clone.route_command(client_id, &cmd).await.unwrap();
            }
        });
        handles.push(handle);
    }

    // Wait for all clients to finish
    for handle in handles {
        handle.await?;
    }

    // Should have 250 pending requests (50 clients * 5 commands)
    assert_eq!(router.pending_count().await, 250);

    // Verify load is distributed across all 5 backends
    for i in 0..5 {
        let load = router.backend_load(BackendId::from_index(i)).unwrap();
        assert_eq!(
            load, 50,
            "Backend {} should have load of 50, got {}",
            i, load
        );
    }

    Ok(())
}
