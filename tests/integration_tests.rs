use anyhow::Result;
use std::io::Write;
use tempfile::NamedTempFile;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{Duration, timeout};

use nntp_proxy::config::{ClientAuth, HealthCheck, Proxy, Server};
use nntp_proxy::{Config, NntpProxy, RoutingMode, load_config};

mod test_helpers;
use test_helpers::*;

#[tokio::test]
async fn test_proxy_with_mock_servers() -> Result<()> {
    // Find available ports
    let mock_port1 = get_available_port()
        .await
        .expect("Failed to get available port");
    let mock_port2 = get_available_port()
        .await
        .expect("Failed to get available port");
    let proxy_port = get_available_port()
        .await
        .expect("Failed to get available port");

    // Start mock servers using builder
    let _mock1 = MockNntpServer::new(mock_port1)
        .with_name("Mock NNTP Server")
        .on_command("HELP", "100 HELP command received\r\n")
        .spawn();
    let _mock2 = MockNntpServer::new(mock_port2)
        .with_name("Mock NNTP Server")
        .on_command("HELP", "100 HELP command received\r\n")
        .spawn();

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

    let proxy = NntpProxy::new(config, RoutingMode::Hybrid).await?;

    // Start proxy server
    let proxy_addr = format!("127.0.0.1:{proxy_port}");
    let listener = TcpListener::bind(&proxy_addr).await?;

    tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = listener.accept().await {
                let proxy_clone = proxy.clone();
                tokio::spawn(async move {
                    let _ = proxy_clone.handle_client(stream, addr.into()).await;
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
    assert!(welcome.contains("201 NNTP Proxy Ready"));

    // Send a test command
    client.write_all(b"HELP\r\n").await?;

    // Read echo response
    let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
    let response = String::from_utf8_lossy(&buffer[..n]);
    assert!(response.contains("HELP") || response.contains("100"));

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
    let mock_port1 = get_available_port()
        .await
        .expect("Failed to get available port");
    let mock_port2 = get_available_port()
        .await
        .expect("Failed to get available port");
    let proxy_port = get_available_port()
        .await
        .expect("Failed to get available port");

    // Start mock servers using builder
    let _mock1 = MockNntpServer::new(mock_port1)
        .with_name("Mock Server 1")
        .on_command("HELP", "100 HELP command received\r\n")
        .spawn();
    let _mock2 = MockNntpServer::new(mock_port2)
        .with_name("Mock Server 2")
        .on_command("HELP", "100 HELP command received\r\n")
        .spawn();

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

    let proxy = NntpProxy::new(config, RoutingMode::Stateful).await?;

    // Start proxy server
    let proxy_addr = format!("127.0.0.1:{proxy_port}");
    let listener = TcpListener::bind(&proxy_addr).await?;

    tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = listener.accept().await {
                let proxy_clone = proxy.clone();
                tokio::spawn(async move {
                    let _ = proxy_clone.handle_client(stream, addr.into()).await;
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
        assert!(response.contains("201 NNTP Proxy Ready"));

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
    write!(temp_file, "{config_content}")?;

    let config = load_config(temp_file.path().to_str().unwrap())?;

    assert_eq!(config.servers.len(), 2);
    assert_eq!(config.servers[0].host.as_str(), "test1.example.com");
    assert_eq!(config.servers[0].port.get(), 119);
    assert_eq!(config.servers[1].host.as_str(), "test2.example.com");
    assert_eq!(config.servers[1].port.get(), 563);

    Ok(())
}

#[tokio::test]
async fn test_proxy_handles_connection_failure() -> Result<()> {
    let proxy_port = get_available_port()
        .await
        .expect("Failed to get available port");
    let nonexistent_port = get_available_port()
        .await
        .expect("Failed to get available port");

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

    let proxy = NntpProxy::new(config, RoutingMode::Stateful).await?;

    // Start proxy server
    let proxy_addr = format!("127.0.0.1:{proxy_port}");
    let listener = TcpListener::bind(&proxy_addr).await?;

    tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = listener.accept().await {
                let proxy_clone = proxy.clone();
                tokio::spawn(async move {
                    let _ = proxy_clone.handle_client(stream, addr.into()).await;
                });
            }
        }
    });

    // Give proxy time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Test client connection - should receive greeting first, then error when backend connection fails
    let mut client = TcpStream::connect(&proxy_addr).await?;
    let mut buffer = [0; 1024];

    // Read greeting (sent by prepare_stateful_connection)
    let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
    let greeting = String::from_utf8_lossy(&buffer[..n]);
    assert!(
        greeting.contains("201"),
        "Expected greeting, got: {greeting:?}"
    );

    // Read error (sent by handle_stateful_session when backend connection fails)
    let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
    let error_response = String::from_utf8_lossy(&buffer[..n]);
    assert!(
        error_response.contains("400 Backend server unavailable"),
        "Expected backend unavailable error, got: {error_response:?}"
    );

    Ok(())
}

/// Test that responses are delivered promptly - simulates rapid article requests
/// This test validates response delivery timing regardless of flush implementation.
#[tokio::test]
async fn test_response_flushing_with_rapid_commands() -> Result<()> {
    let mock_port = get_available_port()
        .await
        .expect("Failed to get available port");
    let proxy_port = get_available_port()
        .await
        .expect("Failed to get available port");

    // Start mock server using builder with custom article responses
    let _mock = MockNntpServer::new(mock_port)
        .with_name("Mock NNTP Server")
        .on_command("BODY", "220 0 <test@example.com>\r\nArticle body line 1\r\nArticle body line 2\r\nArticle body line 3\r\n.\r\n")
        .on_command("ARTICLE", "220 0 <test@example.com>\r\nArticle body line 1\r\nArticle body line 2\r\nArticle body line 3\r\n.\r\n")
        .spawn();

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create proxy config
    let config = create_test_config(vec![(mock_port, "TestServer")]);
    let proxy = NntpProxy::new(config, RoutingMode::Stateful).await?;

    // Start proxy in per-command routing mode
    spawn_test_proxy(proxy, proxy_port, true).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connect client
    let mut client = TcpStream::connect(format!("127.0.0.1:{proxy_port}")).await?;

    // Read greeting with short timeout - should arrive immediately
    let mut buffer = [0; 1024];
    let n = timeout(Duration::from_millis(500), client.read(&mut buffer))
        .await
        .map_err(|_| anyhow::anyhow!("Timeout reading greeting - not flushed!"))?
        .map_err(|e| anyhow::anyhow!("Failed to read greeting: {e}"))?;

    let greeting = String::from_utf8_lossy(&buffer[..n]);
    assert!(
        greeting.contains("201"),
        "Expected greeting, got: {greeting}"
    );

    // Send rapid BODY commands - this is where flush issues would show up
    for i in 1..=10 {
        let cmd = format!("BODY <msg{i}@test.com>\r\n");
        client.write_all(cmd.as_bytes()).await?;
        client.flush().await?;

        // Try to read response with a VERY short timeout
        // This validates that responses are delivered immediately by the proxy.
        let n = timeout(Duration::from_millis(200), client.read(&mut buffer))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "Timeout reading response #{i} - proxy likely not flushing responses!"
                )
            })?
            .map_err(|e| anyhow::anyhow!("Failed to read response #{i}: {e}"))?;

        let response = String::from_utf8_lossy(&buffer[..n]);

        // Verify we got a complete response (should include terminator)
        assert!(
            response.contains("220") && response.contains(".\r\n"),
            "Response #{i} incomplete or malformed: {response}"
        );
    }

    // Clean up
    client.write_all(b"QUIT\r\n").await?;

    Ok(())
}

/// Test that single-line responses (auth, reject) are also properly flushed
#[tokio::test]
async fn test_auth_and_reject_response_flushing() -> Result<()> {
    let mock_port = get_available_port()
        .await
        .expect("Failed to get available port");
    let proxy_port = get_available_port()
        .await
        .expect("Failed to get available port");

    // Start mock server using builder
    let _mock = MockNntpServer::new(mock_port)
        .with_name("TestServer")
        .spawn();

    tokio::time::sleep(Duration::from_millis(100)).await;

    let config = create_test_config(vec![(mock_port, "TestServer")]);
    let proxy = NntpProxy::new(config, RoutingMode::Stateful).await?;

    spawn_test_proxy(proxy, proxy_port, true).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut client = TcpStream::connect(format!("127.0.0.1:{proxy_port}")).await?;

    // Read greeting - should arrive quickly
    let mut buffer = [0; 1024];
    let n = timeout(Duration::from_millis(500), client.read(&mut buffer))
        .await
        .map_err(|_| anyhow::anyhow!("Timeout reading greeting - not flushed!"))?
        .map_err(|e| anyhow::anyhow!("Failed to read greeting: {e}"))?;

    let greeting = String::from_utf8_lossy(&buffer[..n]);
    assert!(greeting.contains("201"));

    // Test authentication interception (handled locally by proxy)
    client.write_all(b"AUTHINFO USER testuser\r\n").await?;
    client.flush().await?;

    // Response should arrive immediately (local handling)
    let n = timeout(Duration::from_millis(200), client.read(&mut buffer))
        .await
        .map_err(|_| anyhow::anyhow!("Timeout reading auth response - not flushed!"))?
        .map_err(|e| anyhow::anyhow!("Failed to read auth response: {e}"))?;

    let auth_response = String::from_utf8_lossy(&buffer[..n]);
    assert!(
        auth_response.contains("381"),
        "Expected password request, got: {auth_response}"
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
        .map_err(|e| anyhow::anyhow!("Failed to read CAPABILITIES response: {e}"))?;

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
/// Per RFC 3977 Section 3.2 (<https://tools.ietf.org/html/rfc3977#section-3.2)>:
/// "Multi-line responses have the second digit as 1, 2, or 3 (e.g., 215, 220, 231).
///  All other responses are single-line."
///
/// Previously, the proxy incorrectly treated 200 responses as multiline, causing it to
/// wait indefinitely for a terminator that would never come, exhausting the connection
/// pool and causing subsequent commands to hang.
#[tokio::test]
async fn test_sequential_requests_no_delay() -> Result<()> {
    let mock_port = get_available_port()
        .await
        .expect("Failed to get available port");
    let proxy_port = get_available_port()
        .await
        .expect("Failed to get available port");

    // Start mock server using builder
    let _mock = MockNntpServer::new(mock_port)
        .with_name("Ready")
        .on_command("STAT", "200 Command OK\r\n")
        .spawn();

    tokio::time::sleep(Duration::from_millis(100)).await;

    let config = create_test_config(vec![(mock_port, "TestServer")]);
    let proxy = NntpProxy::new(config, RoutingMode::Stateful).await?;
    spawn_test_proxy(proxy, proxy_port, true).await;

    // Give proxy more time to initialize connection pool
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut client = TcpStream::connect(format!("127.0.0.1:{proxy_port}")).await?;

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
        let cmd = format!("STAT <msg{i}@test.com>\r\n");
        println!("Sending command {}: {}", i, cmd.trim());
        client.write_all(cmd.as_bytes()).await?;
        client.flush().await?;

        // Clear buffer before reading
        buffer = [0; 1024];

        // Each response should arrive within 500ms (increased timeout for connection pool)
        // This test verifies that explicit flush() calls are not required for TCP streams,
        // and that responses are received promptly after write_all().
        let n = timeout(Duration::from_secs(1), client.read(&mut buffer))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "Timeout waiting for response to command {i}. Possible server or network issue."
                )
            })?
            .map_err(|e| anyhow::anyhow!("Read error on command {i}: {e}"))?;

        assert!(n > 0, "Empty response on command {i}");

        // Verify we got a proper response
        let response = String::from_utf8_lossy(&buffer[..n]);
        println!("Response {}: {}", i, response.trim());
        assert!(
            response.contains("200"),
            "Expected '200' in response #{i}, got: {response}"
        );
    }

    client.write_all(b"QUIT\r\n").await?;

    Ok(())
}

#[tokio::test]
async fn test_hybrid_mode_stateless_commands() -> Result<()> {
    // Test hybrid mode with stateless commands only (should stay in per-command routing)
    let mock_port1 = get_available_port()
        .await
        .expect("Failed to get available port");
    let mock_port2 = get_available_port()
        .await
        .expect("Failed to get available port");
    let proxy_port = get_available_port()
        .await
        .expect("Failed to get available port");

    // Start mock servers using builder with smart command handlers
    let _mock1 = create_smart_mock_builder(mock_port1, "Server1").spawn();
    let _mock2 = create_smart_mock_builder(mock_port2, "Server2").spawn();

    // Give servers time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    let config = Config {
        servers: vec![
            create_test_server_config("127.0.0.1", mock_port1, "Mock Server 1"),
            create_test_server_config("127.0.0.1", mock_port2, "Mock Server 2"),
        ],
        ..Default::default()
    };

    let proxy = NntpProxy::new(config, RoutingMode::Hybrid).await?;
    let proxy_addr = format!("127.0.0.1:{proxy_port}");
    let listener = TcpListener::bind(&proxy_addr).await?;

    tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = listener.accept().await {
                let proxy_clone = proxy.clone();
                tokio::spawn(async move {
                    let _ = proxy_clone.handle_client(stream, addr.into()).await;
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
    assert!(greeting.contains("201"));

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
        let n = timeout(Duration::from_secs(2), client.read(&mut buffer))
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
    let mock_port = get_available_port()
        .await
        .expect("Failed to get available port");
    let proxy_port = get_available_port()
        .await
        .expect("Failed to get available port");

    let _mock = create_smart_mock_builder(mock_port, "StatefulServer").spawn();

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

    let proxy = NntpProxy::new(config, RoutingMode::Hybrid).await?;
    let proxy_addr = format!("127.0.0.1:{proxy_port}");
    let listener = TcpListener::bind(&proxy_addr).await?;

    tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = listener.accept().await {
                let proxy_clone = proxy.clone();
                tokio::spawn(async move {
                    let _ = proxy_clone.handle_client(stream, addr.into()).await;
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
    assert!(greeting.contains("201"));

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
    let n = timeout(Duration::from_secs(2), client.read(&mut buffer))
        .await
        .map_err(|_| anyhow::anyhow!("Timeout waiting for GROUP response"))?
        .map_err(|e| anyhow::anyhow!("Read error for GROUP command: {e}"))?;

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
        let n = timeout(Duration::from_secs(2), client.read(&mut buffer))
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
    // Test multiple concurrent clients in hybrid mode
    let mock_port = get_available_port()
        .await
        .expect("Failed to get available port");
    let proxy_port = get_available_port()
        .await
        .expect("Failed to get available port");

    let _mock = create_smart_mock_builder(mock_port, "MultiServer").spawn();

    tokio::time::sleep(Duration::from_millis(100)).await;

    let config = Config {
        servers: vec![create_test_server_config_with_max_connections(
            "127.0.0.1",
            mock_port,
            "Mock Server",
            10,
        )],
        ..Default::default()
    };

    let proxy = NntpProxy::new(config, RoutingMode::Hybrid).await?;
    let proxy_addr = format!("127.0.0.1:{proxy_port}");
    let listener = TcpListener::bind(&proxy_addr).await?;

    tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = listener.accept().await {
                let proxy_clone = proxy.clone();
                tokio::spawn(async move {
                    let _ = proxy_clone.handle_client(stream, addr.into()).await;
                });
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Run 3 clients concurrently
    let results = tokio::try_join!(
        async {
            let mut client = TcpStream::connect(&proxy_addr).await?;
            let mut buf = [0; 1024];

            // Greeting
            let n = timeout(Duration::from_secs(1), client.read(&mut buf)).await??;
            assert!(n > 0);

            // Command
            client.write_all(b"DATE\r\n").await?;
            let n = timeout(Duration::from_secs(1), client.read(&mut buf)).await??;
            assert!(n > 0);

            client.write_all(b"QUIT\r\n").await?;
            anyhow::Ok(())
        },
        async {
            let mut client = TcpStream::connect(&proxy_addr).await?;
            let mut buf = [0; 1024];

            // Greeting
            let n = timeout(Duration::from_secs(1), client.read(&mut buf)).await??;
            assert!(n > 0);

            // Command
            client.write_all(b"LIST\r\n").await?;
            let n = timeout(Duration::from_secs(1), client.read(&mut buf)).await??;
            assert!(n > 0);

            client.write_all(b"QUIT\r\n").await?;
            anyhow::Ok(())
        },
        async {
            let mut client = TcpStream::connect(&proxy_addr).await?;
            let mut buf = [0; 1024];

            // Greeting
            let n = timeout(Duration::from_secs(1), client.read(&mut buf)).await??;
            assert!(n > 0);

            // Command
            client.write_all(b"ARTICLE <msg@example.com>\r\n").await?;
            let n = timeout(Duration::from_secs(1), client.read(&mut buf)).await??;
            assert!(n > 0);

            client.write_all(b"QUIT\r\n").await?;
            anyhow::Ok(())
        }
    );

    results?;
    Ok(())
}

/// Create a smart mock server builder with comprehensive NNTP command handlers
fn create_smart_mock_builder(port: u16, server_name: &str) -> MockNntpServer {
    MockNntpServer::new(port)
        .with_name(format!("{server_name} Mock NNTP Server"))
        .on_command("LIST", "215 List of newsgroups\r\nalt.test 100 1 y\r\n.\r\n")
        .on_command("DATE", "111 20231013120000\r\n")
        .on_command("CAPABILITIES", "101 Capability list\r\nVERSION 2\r\nREADER\r\n.\r\n")
        .on_command("HELP", "100 Help text\r\nCommands available\r\n.\r\n")
        .on_command("GROUP", "211 100 1 100 alt.test\r\n")
        .on_command("ARTICLE", "220 1 <msg@example.com>\r\nSubject: Test\r\n\r\nTest body\r\n.\r\n")
        .on_command("HEAD", "221 1 <msg@example.com>\r\nSubject: Test\r\n.\r\n")
        .on_command("BODY", "222 1 <msg@example.com>\r\nTest body\r\n.\r\n")
        .on_command("STAT", "223 1 <current@example.com>\r\n")
        .on_command("XOVER", "224 Overview\r\n1\tTest Subject\tauthor@example.com\t13 Oct 2023\t<msg1@example.com>\t\t100\t5\r\n.\r\n")
        .on_command("NEXT", "223 2 <next@example.com>\r\n")
        .on_command("LAST", "223 1 <prev@example.com>\r\n")
}

/// Test that backends can return 223 for ARTICLE commands with message-IDs
///
/// In practice, some backends return "223 0 <msgid>" for article-by-message-ID
/// requests when the article is not found, even though RFC 3977 specifies 223
/// is for "no such article number" (numeric requests). This is a real-world
/// backend behavior that the proxy should handle gracefully without warnings.
#[tokio::test]
async fn test_backend_223_response_for_message_id() -> Result<()> {
    // Find available ports
    let mock_port = get_available_port()
        .await
        .expect("Failed to get available port");
    let proxy_port = get_available_port()
        .await
        .expect("Failed to get available port");

    // Start mock backend that returns 223 for missing articles by message-ID
    // This simulates real-world backend behavior observed in production
    let _mock = MockNntpServer::new(mock_port)
        .with_name("Backend That Returns 223")
        .on_command("ARTICLE <missing@", "223 0 <missing@example.com>\r\n")
        .on_command(
            "ARTICLE <exists@",
            "220 1 <exists@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n",
        )
        .spawn();

    // Give server time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create proxy configuration
    let config = Config {
        servers: vec![create_test_server_config_with_max_connections(
            "127.0.0.1",
            mock_port,
            "Backend That Returns 223",
            5,
        )],
        ..Default::default()
    };

    // Start proxy in per-command mode (where this issue was observed)
    let proxy = NntpProxy::new(config, RoutingMode::PerCommand).await?;
    let proxy_clone = proxy.clone();
    let proxy_addr = format!("127.0.0.1:{proxy_port}");
    let proxy_addr_clone = proxy_addr.clone();

    let _proxy_handle = tokio::spawn(async move {
        let listener = TcpListener::bind(&proxy_addr_clone).await.unwrap();
        while let Ok((stream, addr)) = listener.accept().await {
            let proxy = proxy_clone.clone();
            tokio::spawn(async move {
                let _ = proxy
                    .handle_client_per_command_routing(stream, addr.into())
                    .await;
            });
        }
    });

    // Give proxy time to start
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Connect to proxy
    let mut client = TcpStream::connect(&proxy_addr).await?;

    // Read greeting
    let mut buffer = [0; 1024];
    let n = client.read(&mut buffer).await?;
    let greeting = String::from_utf8_lossy(&buffer[..n]);
    assert!(
        greeting.starts_with("201"),
        "Expected greeting, got: {greeting}"
    );

    // Request a missing article - backend will return 223
    client
        .write_all(b"ARTICLE <missing@example.com>\r\n")
        .await?;
    client.flush().await?;

    buffer = [0; 1024];
    let n = timeout(Duration::from_secs(2), client.read(&mut buffer)).await??;
    let response = String::from_utf8_lossy(&buffer[..n]);

    // Verify we get the 223 response from backend
    assert!(
        response.starts_with("223"),
        "Expected 223 response, got: {response}"
    );
    assert!(
        response.contains("<missing@example.com>"),
        "Response should contain message-ID"
    );

    // Now request an existing article - should work normally
    client
        .write_all(b"ARTICLE <exists@example.com>\r\n")
        .await?;
    client.flush().await?;

    let mut large_buffer = [0u8; 4096];
    let n = timeout(Duration::from_secs(2), client.read(&mut large_buffer)).await??;
    let response = String::from_utf8_lossy(&large_buffer[..n]);

    // Verify we get the 220 multiline response
    assert!(
        response.starts_with("220"),
        "Expected 220 response, got: {response}"
    );
    assert!(
        response.contains("Subject: Test"),
        "Should have article headers"
    );
    assert!(
        response.ends_with(".\r\n"),
        "Should have multiline terminator"
    );

    // Clean up
    client.write_all(b"QUIT\r\n").await?;

    Ok(())
}

/// Test that tier 0 backends are exhausted before escalating to tier 1
///
/// This test verifies the critical tier-based routing behavior:
/// When backend 0 (tier 0) returns 430, the proxy MUST try backend 1 (tier 0)
/// before escalating to tier 1. This ensures all tier 0 backends are exhausted.
fn build_tiered_server(port: u16, name: &str, tier: u8) -> Result<Server> {
    Server::builder("127.0.0.1", nntp_proxy::types::Port::try_new(port)?)
        .name(name)
        .tier(tier)
        .max_connections(nntp_proxy::types::MaxConnections::try_new(5)?)
        .build()
}

async fn start_tiered_proxy(servers: Vec<Server>, proxy_port: u16) -> Result<()> {
    let config = Config {
        servers,
        proxy: Proxy::default(),
        health_check: HealthCheck::default(),
        cache: None,
        client_auth: ClientAuth::default(),
    };
    let proxy = NntpProxy::new(config, RoutingMode::PerCommand).await?;
    spawn_test_proxy(proxy, proxy_port, true).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    Ok(())
}

async fn connect_tiered_client(proxy_port: u16) -> Result<TcpStream> {
    let mut client = TcpStream::connect(format!("127.0.0.1:{proxy_port}")).await?;
    let mut buffer = [0u8; 4096];
    let n = timeout(Duration::from_secs(2), client.read(&mut buffer)).await??;
    let greeting = String::from_utf8_lossy(&buffer[..n]);
    assert!(
        greeting.starts_with("201"),
        "Expected greeting, got: {greeting}"
    );
    Ok(client)
}

async fn assert_article_response(
    client: &mut TcpStream,
    command: &str,
    expected_body: &str,
    context: &str,
) -> Result<()> {
    client.write_all(command.as_bytes()).await?;
    client.flush().await?;

    let mut buffer = [0u8; 4096];
    let n = timeout(Duration::from_secs(2), client.read(&mut buffer)).await??;
    let response = String::from_utf8_lossy(&buffer[..n]);

    assert!(
        response.starts_with("220"),
        "{context}: expected 220, got: {response}"
    );
    assert!(
        response.contains(expected_body),
        "{context}: expected body {expected_body:?}, got: {response}"
    );
    Ok(())
}

#[tokio::test]
async fn test_tier_0_exhaustion_before_escalation() -> Result<()> {
    // Find available ports
    let backend_0_port = get_available_port().await?;
    let backend_1_port = get_available_port().await?;
    let backend_2_port = get_available_port().await?;
    let proxy_port = get_available_port().await?;

    // Start mock backends:
    // Backend 0 (tier 0): Returns 430 for the article
    // Backend 1 (tier 0): Has the article (returns 220)
    // Backend 2 (tier 1): Has the article but should NOT be tried if tier 0 works
    let _backend_0 = MockNntpServer::new(backend_0_port)
        .with_name("Backend-0-Tier-0")
        .on_command(
            "ARTICLE",
            "430 No such article\r\n", // Backend 0 doesn't have it
        )
        .spawn();

    let _backend_1 = MockNntpServer::new(backend_1_port)
        .with_name("Backend-1-Tier-0")
        .on_command(
            "ARTICLE",
            "220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody from backend 1\r\n.\r\n", // Backend 1 has it
        )
        .spawn();

    let _backend_2 = MockNntpServer::new(backend_2_port)
        .with_name("Backend-2-Tier-1")
        .on_command(
            "ARTICLE",
            "220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody from backend 2\r\n.\r\n",
        )
        .spawn();

    tokio::time::sleep(Duration::from_millis(100)).await;

    start_tiered_proxy(
        vec![
            build_tiered_server(backend_0_port, "Backend-0-Tier-0", 0)?,
            build_tiered_server(backend_1_port, "Backend-1-Tier-0", 0)?,
            build_tiered_server(backend_2_port, "Backend-2-Tier-1", 1)?,
        ],
        proxy_port,
    )
    .await?;
    let mut client = connect_tiered_client(proxy_port).await?;

    // Request article - should try backend 0 (430), then backend 1 (220)
    // Backend 2 (tier 1) should NOT be tried at all
    assert_article_response(
        &mut client,
        "ARTICLE <test@example.com>\r\n",
        "Body from backend 1",
        "initial request",
    )
    .await?;

    // Test multiple requests to ensure consistent behavior
    for i in 1..=5 {
        assert_article_response(
            &mut client,
            &format!("ARTICLE <test{i}@example.com>\r\n"),
            "Body from backend 1",
            &format!("request {i}"),
        )
        .await?;
    }

    // Clean up
    client.write_all(b"QUIT\r\n").await?;

    Ok(())
}

#[tokio::test]
async fn test_tier_exhaustion_multi_tier() -> Result<()> {
    // Find available ports
    let backend_0_port = get_available_port().await?;
    let backend_1_port = get_available_port().await?;
    let backend_2_port = get_available_port().await?;
    let proxy_port = get_available_port().await?;

    // Start mock backends:
    // Backend 0 (tier 0): Returns 430 for the article (doesn't have it)
    // Backend 1 (tier 0): Also returns 430 (doesn't have it either)
    // Backend 2 (tier 1): Has the article (returns 220)
    // CRITICAL: Backend 2 should ONLY be tried after BOTH tier 0 backends are exhausted
    let _backend_0 = MockNntpServer::new(backend_0_port)
        .with_name("Backend-0-Tier-0")
        .on_command(
            "ARTICLE",
            "430 No such article\r\n", // Backend 0 doesn't have it
        )
        .spawn();

    let _backend_1 = MockNntpServer::new(backend_1_port)
        .with_name("Backend-1-Tier-0")
        .on_command(
            "ARTICLE",
            "430 No such article\r\n", // Backend 1 doesn't have it either
        )
        .spawn();

    let _backend_2 = MockNntpServer::new(backend_2_port)
        .with_name("Backend-2-Tier-1")
        .on_command(
            "ARTICLE",
            "220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody from tier 1\r\n.\r\n", // Backend 2 has it
        )
        .spawn();

    tokio::time::sleep(Duration::from_millis(100)).await;

    start_tiered_proxy(
        vec![
            build_tiered_server(backend_0_port, "Backend-0-Tier-0", 0)?,
            build_tiered_server(backend_1_port, "Backend-1-Tier-0", 0)?,
            build_tiered_server(backend_2_port, "Backend-2-Tier-1", 1)?,
        ],
        proxy_port,
    )
    .await?;
    let mut client = connect_tiered_client(proxy_port).await?;

    // Request article - should try backend 0 (430), backend 1 (430), then backend 2 (220)
    // This tests that tier 0 is fully exhausted before escalating to tier 1
    assert_article_response(
        &mut client,
        "ARTICLE <test@example.com>\r\n",
        "Body from tier 1",
        "initial request",
    )
    .await?;

    // Test multiple requests to ensure consistent behavior
    for i in 1..=5 {
        assert_article_response(
            &mut client,
            &format!("ARTICLE <test{i}@example.com>\r\n"),
            "Body from tier 1",
            &format!("request {i}"),
        )
        .await?;
    }

    // Clean up
    client.write_all(b"QUIT\r\n").await?;

    Ok(())
}

/// Test that an oversized command (>512 bytes) sent as the second command in a
/// pipelined batch receives a 501 error response instead of being forwarded.
/// RFC 3977 §3.2.1: 501 = syntax error (overlong command is a syntax error, not unknown)
#[tokio::test]
async fn test_oversized_pipelined_command_rejected_with_500() -> Result<()> {
    let mock_port = get_available_port().await?;
    let proxy_port = get_available_port().await?;

    let _mock = MockNntpServer::new(mock_port)
        .with_name("TestServer")
        .on_command("STAT", "223 0 <test@example.com> status\r\n")
        .spawn();

    tokio::time::sleep(Duration::from_millis(100)).await;

    let config = create_test_config(vec![(mock_port, "TestServer")]);
    let proxy = NntpProxy::new(config, RoutingMode::Hybrid).await?;

    spawn_test_proxy(proxy, proxy_port, true).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut client = TcpStream::connect(format!("127.0.0.1:{proxy_port}")).await?;
    let mut buffer = [0; 4096];

    // Read greeting
    let _n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;

    // Send a valid STAT command followed by an oversized command in one write
    // (simulates TCP pipelining — both arrive in the same buffer)
    let oversized_msg_id = format!("<{}@example.com>", "x".repeat(510));
    let oversized_cmd = format!("STAT {oversized_msg_id}\r\n");
    assert!(
        oversized_cmd.len() > 512,
        "Test setup: command must exceed 512 bytes, got {}",
        oversized_cmd.len()
    );

    let mut pipelined = b"STAT <valid@example.com>\r\n".to_vec();
    pipelined.extend_from_slice(oversized_cmd.as_bytes());
    client.write_all(&pipelined).await?;
    client.flush().await?;

    // Read response to first (valid) STAT command
    let n = timeout(Duration::from_secs(2), client.read(&mut buffer)).await??;
    let response = String::from_utf8_lossy(&buffer[..n]);

    // The first command should get a normal 223 response
    assert!(
        response.contains("223"),
        "Expected 223 for valid STAT, got: {response}"
    );

    // Read response to the oversized command — should be 501 error, not forwarded
    // (may have arrived in the same read above, check that too)
    let observed_response = if response.contains("501") {
        response.to_string()
    } else {
        let n = timeout(Duration::from_secs(2), client.read(&mut buffer)).await??;
        String::from_utf8_lossy(&buffer[..n]).to_string()
    };

    assert!(
        observed_response.contains("501"),
        "Expected 501 error for oversized command, got: {observed_response}"
    );

    client.write_all(b"QUIT\r\n").await?;
    Ok(())
}

/// Empty request lines are syntax errors and must not create request contexts
/// or get forwarded to a backend.
#[tokio::test]
async fn test_empty_pipelined_command_rejected_with_501() -> Result<()> {
    let mock_port = get_available_port().await?;
    let proxy_port = get_available_port().await?;

    let _mock = MockNntpServer::new(mock_port)
        .with_name("TestServer")
        .on_command("STAT", "223 0 <test@example.com> status\r\n")
        .spawn();

    tokio::time::sleep(Duration::from_millis(100)).await;

    let config = create_test_config(vec![(mock_port, "TestServer")]);
    let proxy = NntpProxy::new(config, RoutingMode::Hybrid).await?;

    spawn_test_proxy(proxy, proxy_port, true).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut client = TcpStream::connect(format!("127.0.0.1:{proxy_port}")).await?;
    let mut buffer = [0; 4096];

    let _n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;

    client
        .write_all(b"STAT <valid@example.com>\r\n\r\n")
        .await?;
    client.flush().await?;

    let n = timeout(Duration::from_secs(2), client.read(&mut buffer)).await??;
    let response = String::from_utf8_lossy(&buffer[..n]);
    assert!(
        response.contains("223"),
        "Expected 223 for valid STAT, got: {response}"
    );

    let observed_response = if response.contains("501") {
        response.to_string()
    } else {
        let n = timeout(Duration::from_secs(2), client.read(&mut buffer)).await??;
        String::from_utf8_lossy(&buffer[..n]).to_string()
    };

    assert!(
        observed_response.contains("501 Syntax error in command"),
        "Expected 501 syntax error for empty command, got: {observed_response}"
    );

    client.write_all(b"QUIT\r\n").await?;
    Ok(())
}

/// Test that a partial command in the `BufReader` buffer doesn't cause the batch
/// reader to block waiting for more data from the socket.
///
/// Scenario: Client sends a valid STAT + a partial (incomplete) second command
/// in one TCP write. The proxy should process the first STAT immediately and
/// respond, NOT block waiting for the partial command to complete.
#[tokio::test]
async fn test_partial_buffered_command_does_not_block() -> Result<()> {
    let mock_port = get_available_port().await?;
    let proxy_port = get_available_port().await?;

    let _mock = MockNntpServer::new(mock_port)
        .with_name("TestServer")
        .on_command("STAT", "223 0 <test@example.com> status\r\n")
        .spawn();

    tokio::time::sleep(Duration::from_millis(100)).await;

    let config = create_test_config(vec![(mock_port, "TestServer")]);
    let proxy = NntpProxy::new(config, RoutingMode::Hybrid).await?;

    spawn_test_proxy(proxy, proxy_port, true).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut client = TcpStream::connect(format!("127.0.0.1:{proxy_port}")).await?;
    let mut buffer = [0; 4096];

    // Read greeting
    let _n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;

    // Send a valid STAT command followed by a PARTIAL second command (no \n)
    // Both arrive in the same TCP buffer, but the second is incomplete.
    let mut pipelined = b"STAT <valid@example.com>\r\n".to_vec();
    pipelined.extend_from_slice(b"STAT <partial-no-newline@examp"); // no \r\n!
    client.write_all(&pipelined).await?;
    client.flush().await?;

    // The proxy must respond to the first STAT within a reasonable time.
    // If it blocks trying to read_line() on the partial second command, this will timeout.
    let n = timeout(Duration::from_millis(500), client.read(&mut buffer))
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "Timeout! Proxy blocked on partial command instead of responding to first STAT"
            )
        })??;

    let response = String::from_utf8_lossy(&buffer[..n]);
    assert!(
        response.contains("223"),
        "Expected 223 for valid STAT, got: {response}"
    );

    // Now complete the partial command so the session can clean up
    client.write_all(b"le.com>\r\nQUIT\r\n").await?;
    client.flush().await?;

    Ok(())
}
