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
        ..Default::default()
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
    assert!(welcome.contains("200 NNTP Proxy Ready"));

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
                    // Send greeting and flush immediately
                    if stream.write_all(b"200 Server1 Ready\r\n").await.is_err() {
                        return;
                    }
                    let mut buffer = [0; 1024];
                    while let Ok(n) = stream.read(&mut buffer).await {
                        if n == 0 {
                            break;
                        }
                        // Echo back simple responses
                        if buffer.starts_with(b"QUIT") {
                            let _ = stream.write_all(b"205 Goodbye\r\n").await;
                            break;
                        }
                        // Respond to any other command
                        let _ = stream.write_all(b"200 OK\r\n").await;
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
                    // Send greeting and flush immediately
                    if stream.write_all(b"200 Server2 Ready\r\n").await.is_err() {
                        return;
                    }
                    let mut buffer = [0; 1024];
                    while let Ok(n) = stream.read(&mut buffer).await {
                        if n == 0 {
                            break;
                        }
                        // Echo back simple responses
                        if buffer.starts_with(b"QUIT") {
                            let _ = stream.write_all(b"205 Goodbye\r\n").await;
                            break;
                        }
                        // Respond to any other command
                        let _ = stream.write_all(b"200 OK\r\n").await;
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
        ..Default::default()
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

    // Test multiple connections - they should all work
    // (Round-robin is tested internally in unit tests)
    for _ in 0..6 {
        let mut client = TcpStream::connect(&proxy_addr).await?;
        let mut buffer = [0; 1024];

        let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
        let response = String::from_utf8_lossy(&buffer[..n]);

        // Should receive proxy greeting
        assert!(response.contains("200 NNTP Proxy Ready"));

        // Send QUIT to close connection
        let _ = client.write_all(b"QUIT\r\n").await;

        // Small delay between connections
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

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
        ..Default::default()
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
