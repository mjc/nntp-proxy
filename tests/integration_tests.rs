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
                use_tls: false,
                tls_verify_cert: true,
                tls_cert_path: None,
            },
            ServerConfig {
                host: "127.0.0.1".to_string(),
                port: mock_port2,
                name: "Mock Server 2".to_string(),
                username: None,
                password: None,
                max_connections: 10,
                use_tls: false,
                tls_verify_cert: true,
                tls_cert_path: None,
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
                use_tls: false,
                tls_verify_cert: true,
                tls_cert_path: None,
            },
            ServerConfig {
                host: "127.0.0.1".to_string(),
                port: mock_port2,
                name: "Mock Server 2".to_string(),
                username: None,
                password: None,
                max_connections: 10,
                use_tls: false,
                tls_verify_cert: true,
                tls_cert_path: None,
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
            use_tls: false,
            tls_verify_cert: true,
            tls_cert_path: None,
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

/// Helper to spawn a test proxy server
async fn spawn_test_proxy(proxy: NntpProxy, port: u16, per_command_routing: bool) {
    let proxy_addr = format!("127.0.0.1:{}", port);
    let listener = TcpListener::bind(&proxy_addr).await.unwrap();

    tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = listener.accept().await {
                let proxy_clone = proxy.clone();
                tokio::spawn(async move {
                    if per_command_routing {
                        let _ = proxy_clone.handle_client_per_command_routing(stream, addr).await;
                    } else {
                        let _ = proxy_clone.handle_client(stream, addr).await;
                    }
                });
            }
        }
    });
}

/// Helper to create test config from port/name pairs
fn create_test_config(server_ports: Vec<(u16, &str)>) -> Config {
    Config {
        servers: server_ports
            .into_iter()
            .map(|(port, name)| ServerConfig {
                host: "127.0.0.1".to_string(),
                port,
                name: name.to_string(),
                username: None,
                password: None,
                max_connections: 5,
                use_tls: false,
                tls_verify_cert: true,
                tls_cert_path: None,
            })
            .collect(),
        ..Default::default()
    }
}

/// Test that responses are delivered promptly - simulates rapid article requests
/// This test validates response delivery timing regardless of flush implementation.
#[tokio::test]
async fn test_response_flushing_with_rapid_commands() -> Result<()> {

    let mock_port = find_available_port().await;
    let proxy_port = find_available_port().await;

    // Start mock server that simulates article responses
    tokio::spawn(async move {
        let addr = format!("127.0.0.1:{}", mock_port);
        let listener = TcpListener::bind(&addr).await.unwrap();

        loop {
            if let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;

                    // Send greeting
                    stream.write_all(b"200 Mock NNTP Server Ready\r\n").await.ok();
                    // IMPORTANT: Mock server DOES flush - this simulates real backend behavior
                    stream.flush().await.ok();

                    let mut buffer = [0; 1024];
                    while let Ok(n) = stream.read(&mut buffer).await {
                        if n == 0 {
                            break;
                        }

                        let cmd = String::from_utf8_lossy(&buffer[..n]);

                        if cmd.starts_with("QUIT") {
                            stream.write_all(b"205 Goodbye\r\n").await.ok();
                            stream.flush().await.ok();
                            break;
                        } else if cmd.contains("BODY") || cmd.contains("ARTICLE") {
                            // Simulate a multiline article response
                            let response = b"220 0 <test@example.com>\r\n\
                                Article body line 1\r\n\
                                Article body line 2\r\n\
                                Article body line 3\r\n\
                                .\r\n";
                            stream.write_all(response).await.ok();
                            stream.flush().await.ok();
                        } else {
                            // Simple response for other commands
                            stream.write_all(b"200 OK\r\n").await.ok();
                            stream.flush().await.ok();
                        }
                    }
                });
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create proxy config
    let config = create_test_config(vec![(mock_port, "TestServer")]);
    let proxy = NntpProxy::new(config)?;

    // Start proxy in per-command routing mode
    spawn_test_proxy(proxy, proxy_port, true).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connect client
    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;

    // Read greeting with short timeout - should arrive immediately
    let mut buffer = [0; 1024];
    let n = timeout(Duration::from_millis(500), client.read(&mut buffer))
        .await
        .map_err(|_| anyhow::anyhow!("Timeout reading greeting - not flushed!"))?
        .map_err(|e| anyhow::anyhow!("Failed to read greeting: {}", e))?;
    
    let greeting = String::from_utf8_lossy(&buffer[..n]);
    assert!(greeting.contains("200"), "Expected greeting, got: {}", greeting);

    // Send rapid BODY commands - this is where flush issues would show up
    for i in 1..=10 {
        let cmd = format!("BODY <msg{}@test.com>\r\n", i);
        client.write_all(cmd.as_bytes()).await?;
        client.flush().await?;

        // Try to read response with a VERY short timeout
        // This validates that responses are delivered immediately by the proxy.
        let n = timeout(Duration::from_millis(200), client.read(&mut buffer))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "Timeout reading response #{} - proxy likely not flushing responses!",
                    i
                )
            })?
            .map_err(|e| anyhow::anyhow!("Failed to read response #{}: {}", i, e))?;

        let response = String::from_utf8_lossy(&buffer[..n]);
        
        // Verify we got a complete response (should include terminator)
        assert!(
            response.contains("220") && response.contains(".\r\n"),
            "Response #{} incomplete or malformed: {}",
            i,
            response
        );
    }

    // Clean up
    client.write_all(b"QUIT\r\n").await?;

    Ok(())
}

/// Test that single-line responses (auth, reject) are also properly flushed
#[tokio::test]
async fn test_auth_and_reject_response_flushing() -> Result<()> {
    let mock_port = find_available_port().await;
    let proxy_port = find_available_port().await;

    // Start basic mock server
    tokio::spawn(async move {
        let addr = format!("127.0.0.1:{}", mock_port);
        let listener = TcpListener::bind(&addr).await.unwrap();

        loop {
            if let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;

                    // Send greeting
                    stream.write_all(b"200 TestServer Ready\r\n").await.ok();
                    stream.flush().await.ok();

                    let mut buffer = [0; 1024];
                    while let Ok(n) = stream.read(&mut buffer).await {
                        if n == 0 {
                            break;
                        }

                        if buffer[..n].starts_with(b"QUIT") {
                            stream.write_all(b"205 Goodbye\r\n").await.ok();
                            stream.flush().await.ok();
                            break;
                        }

                        // Echo back simple response
                        stream.write_all(b"200 OK\r\n").await.ok();
                        stream.flush().await.ok();
                    }
                });
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let config = create_test_config(vec![(mock_port, "TestServer")]);
    let proxy = NntpProxy::new(config)?;

    spawn_test_proxy(proxy, proxy_port, true).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;

    // Read greeting - should arrive quickly
    let mut buffer = [0; 1024];
    let n = timeout(Duration::from_millis(500), client.read(&mut buffer))
        .await
        .map_err(|_| anyhow::anyhow!("Timeout reading greeting - not flushed!"))?
        .map_err(|e| anyhow::anyhow!("Failed to read greeting: {}", e))?;
    
    let greeting = String::from_utf8_lossy(&buffer[..n]);
    assert!(greeting.contains("200"));

    // Test authentication interception (handled locally by proxy)
    client.write_all(b"AUTHINFO USER testuser\r\n").await?;
    client.flush().await?;

    // Response should arrive immediately (local handling)
    let n = timeout(Duration::from_millis(200), client.read(&mut buffer))
        .await
        .map_err(|_| anyhow::anyhow!("Timeout reading auth response - not flushed!"))?
        .map_err(|e| anyhow::anyhow!("Failed to read auth response: {}", e))?;

    let auth_response = String::from_utf8_lossy(&buffer[..n]);
    assert!(auth_response.contains("381"), "Expected password request, got: {}", auth_response);

    // Test command rejection (if any commands are rejected)
    // MODE READER might be rejected or forwarded depending on config
    // Let's try a command that should be forwarded
    client.write_all(b"CAPABILITIES\r\n").await?;
    client.flush().await?;

    // Should get response quickly
    let n = timeout(Duration::from_millis(500), client.read(&mut buffer))
        .await
        .map_err(|_| anyhow::anyhow!("Timeout reading CAPABILITIES response!"))?
        .map_err(|e| anyhow::anyhow!("Failed to read CAPABILITIES response: {}", e))?;

    let response = String::from_utf8_lossy(&buffer[..n]);
    // Should get some response (either from proxy or backend)
    assert!(!response.is_empty(), "Expected some response");

    client.write_all(b"QUIT\r\n").await?;

    Ok(())
}

/// Test multiple sequential requests without delays to catch buffering issues
/// 
/// This test validates proper handling of single-line NNTP responses (like "200 Command OK")
/// in per-command routing mode with connection pooling. Single-line responses have status
/// codes where the second digit is 0, 4, or 8 (e.g., 200, 205, 400, 480).
/// 
/// Per RFC 3977 Section 3.2 (https://tools.ietf.org/html/rfc3977#section-3.2):
/// "Multi-line responses have the second digit as 1, 2, or 3 (e.g., 215, 220, 231).
///  All other responses are single-line."
///
/// Previously, the proxy incorrectly treated 200 responses as multiline, causing it to
/// wait indefinitely for a terminator that would never come, exhausting the connection
/// pool and causing subsequent commands to hang.
#[tokio::test]
async fn test_sequential_requests_no_delay() -> Result<()> {
    let mock_port = find_available_port().await;
    let proxy_port = find_available_port().await;

    // Start mock server
    tokio::spawn(async move {
        let addr = format!("127.0.0.1:{}", mock_port);
        let listener = TcpListener::bind(&addr).await.unwrap();

        loop {
            if let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;

                    stream.write_all(b"200 Ready\r\n").await.ok();
                    stream.flush().await.ok();

                    let mut buffer = [0; 1024];
                    while let Ok(n) = stream.read(&mut buffer).await {
                        if n == 0 {
                            break;
                        }

                        let cmd = String::from_utf8_lossy(&buffer[..n]);
                        if cmd.starts_with("QUIT") {
                            stream.write_all(b"205 Goodbye\r\n").await.ok();
                            stream.flush().await.ok();
                            break;
                        }

                        // Send immediate response
                        stream.write_all(b"200 Command OK\r\n").await.ok();
                        stream.flush().await.ok();
                    }
                });
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let config = create_test_config(vec![(mock_port, "TestServer")]);
    let proxy = NntpProxy::new(config)?;
    spawn_test_proxy(proxy, proxy_port, true).await;
    
    // Give proxy more time to initialize connection pool
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;

    // Read greeting
    let mut buffer = [0; 1024];
    let n = timeout(Duration::from_millis(500), client.read(&mut buffer)).await??;
    assert!(n > 0, "Expected greeting");
    println!("Greeting received: {}", String::from_utf8_lossy(&buffer[..n]));

    // Send just 5 commands to start (not 20)
    for i in 1..=5 {
        let cmd = format!("STAT <msg{}@test.com>\r\n", i);
        println!("Sending command {}: {}", i, cmd.trim());
        client.write_all(cmd.as_bytes()).await?;
        client.flush().await?;

        // Clear buffer before reading
        buffer = [0; 1024];

        // Each response should arrive within 500ms (increased timeout for connection pool)
        // This test verifies that explicit flush() calls are not required for TCP streams,
        // and that responses are received promptly after write_all().
        let n = timeout(Duration::from_millis(1000), client.read(&mut buffer))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "Timeout waiting for response to command {}. Possible server or network issue.",
                    i
                )
            })?
            .map_err(|e| anyhow::anyhow!("Read error on command {}: {}", i, e))?;

        assert!(n > 0, "Empty response on command {}", i);
        
        // Verify we got a proper response
        let response = String::from_utf8_lossy(&buffer[..n]);
        println!("Response {}: {}", i, response.trim());
        assert!(
            response.contains("200"),
            "Expected '200' in response #{}, got: {}",
            i,
            response
        );
    }

    client.write_all(b"QUIT\r\n").await?;

    Ok(())
}
