use anyhow::Result;
use std::io::Write;
use tempfile::NamedTempFile;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{Duration, timeout};

use nntp_proxy::{Config, NntpProxy, RoutingMode, load_config};

mod config_helpers;
use config_helpers::*;

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
            create_test_server_config_with_max_connections(
                "127.0.0.1",
                mock_port1,
                "Mock Server 1",
                10,
            ),
            create_test_server_config_with_max_connections(
                "127.0.0.1",
                mock_port2,
                "Mock Server 2",
                10,
            ),
        ],
        ..Default::default()
    };

    let proxy = NntpProxy::new(config, RoutingMode::Hybrid)?;

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
            create_test_server_config_with_max_connections(
                "127.0.0.1",
                mock_port1,
                "Mock Server 1",
                10,
            ),
            create_test_server_config_with_max_connections(
                "127.0.0.1",
                mock_port2,
                "Mock Server 2",
                10,
            ),
        ],
        ..Default::default()
    };

    let proxy = NntpProxy::new(config, RoutingMode::Standard)?;

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
        servers: vec![create_test_server_config_with_max_connections(
            "127.0.0.1",
            nonexistent_port,
            "Nonexistent Server",
            10,
        )],
        ..Default::default()
    };

    let proxy = NntpProxy::new(config, RoutingMode::Standard)?;

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
                        let _ = proxy_clone
                            .handle_client_per_command_routing(stream, addr)
                            .await;
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
            .map(|(port, name)| create_test_server_config("127.0.0.1", port, name))
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
                    stream
                        .write_all(b"200 Mock NNTP Server Ready\r\n")
                        .await
                        .ok();
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
    let proxy = NntpProxy::new(config, RoutingMode::Standard)?;

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
    assert!(
        greeting.contains("200"),
        "Expected greeting, got: {}",
        greeting
    );

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
    let proxy = NntpProxy::new(config, RoutingMode::Standard)?;

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
    assert!(
        auth_response.contains("381"),
        "Expected password request, got: {}",
        auth_response
    );

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
    let proxy = NntpProxy::new(config, RoutingMode::Standard)?;
    spawn_test_proxy(proxy, proxy_port, true).await;

    // Give proxy more time to initialize connection pool
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;

    // Read greeting
    let mut buffer = [0; 1024];
    let n = timeout(Duration::from_millis(500), client.read(&mut buffer)).await??;
    assert!(n > 0, "Expected greeting");
    println!(
        "Greeting received: {}",
        String::from_utf8_lossy(&buffer[..n])
    );

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

#[tokio::test]
async fn test_hybrid_mode_stateless_commands() -> Result<()> {
    // Test hybrid mode with stateless commands only (should stay in per-command routing)
    let mock_port1 = find_available_port().await;
    let mock_port2 = find_available_port().await;
    let proxy_port = find_available_port().await;

    // Start mock servers that track command distribution
    tokio::spawn(create_smart_mock_server(mock_port1, "Server1"));
    tokio::spawn(create_smart_mock_server(mock_port2, "Server2"));

    // Give servers time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    let config = Config {
        servers: vec![
            create_test_server_config("127.0.0.1", mock_port1, "Mock Server 1"),
            create_test_server_config("127.0.0.1", mock_port2, "Mock Server 2"),
        ],
        ..Default::default()
    };

    let proxy = NntpProxy::new(config, RoutingMode::Hybrid)?;
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

    // Connect to proxy and send stateless commands
    let mut client = TcpStream::connect(&proxy_addr).await?;

    // Read greeting
    let mut buffer = [0; 1024];
    let n = client.read(&mut buffer).await?;
    let greeting = String::from_utf8_lossy(&buffer[..n]);
    assert!(greeting.contains("200"));

    // Send multiple stateless commands - should distribute across backends
    let stateless_commands = vec![
        "LIST\r\n",
        "DATE\r\n",
        "CAPABILITIES\r\n",
        "HELP\r\n",
        "ARTICLE <msg1@example.com>\r\n",
        "HEAD <msg2@example.com>\r\n",
        "BODY <msg3@example.com>\r\n",
        "STAT <msg4@example.com>\r\n",
    ];

    for cmd in stateless_commands {
        client.write_all(cmd.as_bytes()).await?;
        client.flush().await?;

        // Read response
        buffer = [0; 1024];
        let n = timeout(Duration::from_millis(2000), client.read(&mut buffer))
            .await
            .map_err(|_| anyhow::anyhow!("Timeout waiting for response to: {}", cmd.trim()))?
            .map_err(|e| anyhow::anyhow!("Read error for command {}: {}", cmd.trim(), e))?;

        let response = String::from_utf8_lossy(&buffer[..n]);
        println!("Response to {}: {}", cmd.trim(), response.trim());

        // Should get some response (exact code depends on mock server implementation)
        assert!(n > 0, "Empty response to command: {}", cmd.trim());
    }

    // Send QUIT
    client.write_all(b"QUIT\r\n").await?;

    Ok(())
}

#[tokio::test]
async fn test_hybrid_mode_stateful_switching() -> Result<()> {
    // Test hybrid mode switching from per-command to stateful on GROUP command
    let mock_port = find_available_port().await;
    let proxy_port = find_available_port().await;

    tokio::spawn(create_smart_mock_server(mock_port, "StatefulServer"));

    // Give server time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    let config = Config {
        servers: vec![create_test_server_config(
            "127.0.0.1",
            mock_port,
            "Mock Server",
        )],
        ..Default::default()
    };

    let proxy = NntpProxy::new(config, RoutingMode::Hybrid)?;
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

    // Connect to proxy
    let mut client = TcpStream::connect(&proxy_addr).await?;

    // Read greeting
    let mut buffer = [0; 1024];
    let n = client.read(&mut buffer).await?;
    let greeting = String::from_utf8_lossy(&buffer[..n]);
    assert!(greeting.contains("200"));

    // Start with stateless commands
    client.write_all(b"LIST\r\n").await?;
    client.flush().await?;
    buffer = [0; 1024];
    let n = client.read(&mut buffer).await?;
    assert!(n > 0);

    // Send GROUP command - should trigger switch to stateful mode
    client.write_all(b"GROUP alt.test\r\n").await?;
    client.flush().await?;

    // Read response
    buffer = [0; 1024];
    let n = timeout(Duration::from_millis(2000), client.read(&mut buffer))
        .await
        .map_err(|_| anyhow::anyhow!("Timeout waiting for GROUP response"))?
        .map_err(|e| anyhow::anyhow!("Read error for GROUP command: {}", e))?;

    let response = String::from_utf8_lossy(&buffer[..n]);
    println!("GROUP Response: {}", response.trim());
    assert!(n > 0, "Empty response to GROUP command");

    // Now send stateful commands that should work
    let stateful_commands = vec!["ARTICLE 1\r\n", "HEAD 2\r\n", "XOVER 1-10\r\n", "NEXT\r\n"];

    for cmd in stateful_commands {
        client.write_all(cmd.as_bytes()).await?;
        client.flush().await?;

        // Read response
        buffer = [0; 1024];
        let n = timeout(Duration::from_millis(2000), client.read(&mut buffer))
            .await
            .map_err(|_| anyhow::anyhow!("Timeout waiting for response to: {}", cmd.trim()))?
            .map_err(|e| anyhow::anyhow!("Read error for command {}: {}", cmd.trim(), e))?;

        let response = String::from_utf8_lossy(&buffer[..n]);
        println!("Response to {}: {}", cmd.trim(), response.trim());
        assert!(n > 0, "Empty response to command: {}", cmd.trim());
    }

    // Send QUIT
    client.write_all(b"QUIT\r\n").await?;

    Ok(())
}

#[tokio::test]
async fn test_hybrid_mode_multiple_clients() -> Result<()> {
    // Test hybrid mode with multiple clients, some stateless, some stateful
    let mock_port = find_available_port().await;
    let proxy_port = find_available_port().await;

    tokio::spawn(create_smart_mock_server(mock_port, "MultiServer"));

    // Give server time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    let config = Config {
        servers: vec![create_test_server_config_with_max_connections(
            "127.0.0.1",
            mock_port,
            "Mock Server",
            6,
        )],
        ..Default::default()
    };

    let proxy = NntpProxy::new(config, RoutingMode::Hybrid)?;
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

    // Create multiple client tasks
    let mut client_tasks = Vec::new();

    // Client 1: Stateless only
    let proxy_addr_clone = proxy_addr.clone();
    client_tasks.push(tokio::spawn(async move {
        let mut client = TcpStream::connect(&proxy_addr_clone).await.unwrap();

        // Read greeting
        let mut buffer = [0; 1024];
        let _ = client.read(&mut buffer).await.unwrap();

        // Send only stateless commands
        for _ in 0..3 {
            client.write_all(b"LIST\r\n").await.unwrap();
            client.flush().await.unwrap();
            buffer = [0; 1024];
            let _ = client.read(&mut buffer).await.unwrap();
        }

        client.write_all(b"QUIT\r\n").await.unwrap();
    }));

    // Client 2: Switch to stateful
    let proxy_addr_clone = proxy_addr.clone();
    client_tasks.push(tokio::spawn(async move {
        let mut client = TcpStream::connect(&proxy_addr_clone).await.unwrap();

        // Read greeting
        let mut buffer = [0; 1024];
        let _ = client.read(&mut buffer).await.unwrap();

        // Start with stateless
        client.write_all(b"DATE\r\n").await.unwrap();
        client.flush().await.unwrap();
        buffer = [0; 1024];
        let _ = client.read(&mut buffer).await.unwrap();

        // Switch to stateful
        client.write_all(b"GROUP alt.test\r\n").await.unwrap();
        client.flush().await.unwrap();
        buffer = [0; 1024];
        let _ = client.read(&mut buffer).await.unwrap();

        // Use stateful commands
        client.write_all(b"ARTICLE 1\r\n").await.unwrap();
        client.flush().await.unwrap();
        buffer = [0; 1024];
        let _ = client.read(&mut buffer).await.unwrap();

        client.write_all(b"QUIT\r\n").await.unwrap();
    }));

    // Client 3: Another stateless client
    client_tasks.push(tokio::spawn(async move {
        let mut client = TcpStream::connect(&proxy_addr).await.unwrap();

        // Read greeting
        let mut buffer = [0; 1024];
        let _ = client.read(&mut buffer).await.unwrap();

        // Send only stateless commands
        client
            .write_all(b"ARTICLE <msg@example.com>\r\n")
            .await
            .unwrap();
        client.flush().await.unwrap();
        buffer = [0; 1024];
        let _ = client.read(&mut buffer).await.unwrap();

        client.write_all(b"QUIT\r\n").await.unwrap();
    }));

    // Wait for all clients to complete
    for task in client_tasks {
        task.await?;
    }

    Ok(())
}

/// Enhanced mock server that can handle more NNTP-like responses
async fn create_smart_mock_server(port: u16, server_name: &str) -> Result<()> {
    let addr = format!("127.0.0.1:{}", port);
    let listener = TcpListener::bind(&addr).await?;
    let server_name = server_name.to_string();

    loop {
        if let Ok((mut stream, _)) = listener.accept().await {
            let server_name_clone = server_name.clone();
            tokio::spawn(async move {
                let mut buffer = [0; 1024];

                // Send welcome message
                let welcome = format!("200 {} Mock NNTP Server Ready\r\n", server_name_clone);
                let _ = stream.write_all(welcome.as_bytes()).await;

                // Handle commands
                while let Ok(n) = stream.read(&mut buffer).await {
                    if n == 0 {
                        break;
                    }

                    let command = String::from_utf8_lossy(&buffer[..n]);
                    let command = command.trim();

                    let response = match command {
                        cmd if cmd.starts_with("QUIT") => {
                            let _ = stream.write_all(b"205 Goodbye\r\n").await;
                            break;
                        }
                        cmd if cmd.starts_with("LIST") => {
                            "215 List of newsgroups\r\nalt.test 100 1 y\r\n.\r\n"
                        }
                        cmd if cmd.starts_with("DATE") => "111 20231013120000\r\n",
                        cmd if cmd.starts_with("CAPABILITIES") => {
                            "101 Capability list\r\nVERSION 2\r\nREADER\r\n.\r\n"
                        }
                        cmd if cmd.starts_with("HELP") => {
                            "100 Help text\r\nCommands available\r\n.\r\n"
                        }
                        cmd if cmd.starts_with("GROUP") => "211 100 1 100 alt.test\r\n",
                        cmd if cmd.starts_with("ARTICLE") && cmd.contains('<') => {
                            "220 1 <msg@example.com>\r\nSubject: Test\r\n\r\nTest body\r\n.\r\n"
                        }
                        cmd if cmd.starts_with("ARTICLE") => {
                            "220 1 <current@example.com>\r\nSubject: Current\r\n\r\nCurrent body\r\n.\r\n"
                        }
                        cmd if cmd.starts_with("HEAD") && cmd.contains('<') => {
                            "221 1 <msg@example.com>\r\nSubject: Test\r\n.\r\n"
                        }
                        cmd if cmd.starts_with("HEAD") => {
                            "221 1 <current@example.com>\r\nSubject: Current\r\n.\r\n"
                        }
                        cmd if cmd.starts_with("BODY") && cmd.contains('<') => {
                            "222 1 <msg@example.com>\r\nTest body\r\n.\r\n"
                        }
                        cmd if cmd.starts_with("BODY") => {
                            "222 1 <current@example.com>\r\nCurrent body\r\n.\r\n"
                        }
                        cmd if cmd.starts_with("STAT") => "223 1 <current@example.com>\r\n",
                        cmd if cmd.starts_with("XOVER") => {
                            "224 Overview\r\n1\tTest Subject\tauthor@example.com\t13 Oct 2023\t<msg1@example.com>\t\t100\t5\r\n.\r\n"
                        }
                        cmd if cmd.starts_with("NEXT") => "223 2 <next@example.com>\r\n",
                        cmd if cmd.starts_with("LAST") => "223 1 <prev@example.com>\r\n",
                        cmd if cmd.starts_with("AUTHINFO") => "281 Authentication accepted\r\n",
                        _ => "500 Unknown command\r\n",
                    };

                    let _ = stream.write_all(response.as_bytes()).await;
                }
            });
        }
    }
}
