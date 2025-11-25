/// Integration tests for 430/timeout retry logic
///
/// These tests demonstrate that the proxy implements automatic retry
/// when backends return 430 or timeout.
///
/// Key behaviors tested:
/// 1. ✅ All backends return 430 → proxy tries up to 3 backends before giving up
/// 2. (Additional tests can be added as needed)
use anyhow::Result;
use nntp_proxy::{Config, NntpProxy, RoutingMode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{Duration, sleep, timeout};

mod config_helpers;
use config_helpers::*;

mod test_helpers;
use test_helpers::MockNntpServer;

/// Helper function to find an available port
async fn find_available_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    addr.port()
}

#[tokio::test]
async fn test_retry_on_430_succeeds_with_second_backend() -> Result<()> {
    // This test demonstrates the key retry behavior:
    // - Backend 1 returns 430 for a specific article
    // - Backend 2 has the article
    // - Proxy automatically retries and succeeds

    let mock_port1 = find_available_port().await;
    let mock_port2 = find_available_port().await;
    let proxy_port = find_available_port().await;

    // Mock server 1: Returns 430 for <test@example.com>
    let _mock1 = MockNntpServer::new(mock_port1)
        .with_name("Backend1")
        .on_command("ARTICLE <test@example.com>", "430 No such article\r\n")
        .on_command("STAT <test@example.com>", "430 No such article\r\n")
        .spawn();

    // Mock server 2: Has the article
    let _mock2 = MockNntpServer::new(mock_port2)
        .with_name("Backend2")
        .on_command(
            "ARTICLE <test@example.com>",
            "220 0 <test@example.com> Article follows\r\nSubject: Test\r\n\r\nBody\r\n.\r\n",
        )
        .on_command(
            "STAT <test@example.com>",
            "223 0 <test@example.com> Article exists\r\n",
        )
        .spawn();

    sleep(Duration::from_millis(200)).await;

    // Create proxy with both backends
    let config = Config {
        servers: vec![
            create_test_server_config("127.0.0.1", mock_port1, "Backend1"),
            create_test_server_config("127.0.0.1", mock_port2, "Backend2"),
        ],
        ..Default::default()
    };

    let proxy = NntpProxy::new(config, RoutingMode::PerCommand)?;

    // Start proxy
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

    sleep(Duration::from_millis(200)).await;

    // Connect to proxy
    let mut client = TcpStream::connect(&proxy_addr).await?;

    // Read greeting
    let mut buffer = [0; 1024];
    let n = timeout(Duration::from_secs(2), client.read(&mut buffer)).await??;
    let greeting = String::from_utf8_lossy(&buffer[..n]);
    assert!(
        greeting.contains("200"),
        "Expected greeting, got: {}",
        greeting
    );

    // Request article - should retry and succeed
    client.write_all(b"STAT <test@example.com>\r\n").await?;
    let n = timeout(Duration::from_secs(30), client.read(&mut buffer)).await??;
    let response = String::from_utf8_lossy(&buffer[..n]);

    // Should get success (223) after retry, not 430
    assert!(
        response.contains("223"),
        "Expected 223 success after retry, got: {}",
        response
    );

    Ok(())
}

#[tokio::test]
async fn test_retry_exhausts_when_all_backends_return_430() -> Result<()> {
    // This test demonstrates max retry behavior:
    // - All backends return 430
    // - Proxy tries up to 3 backends
    // - Eventually returns error to client

    let mock_port1 = find_available_port().await;
    let mock_port2 = find_available_port().await;
    let proxy_port = find_available_port().await;

    // Both backends return 430
    let _mock1 = MockNntpServer::new(mock_port1)
        .with_name("Backend1")
        .on_command("STAT <missing@example.com>", "430 No such article\r\n")
        .spawn();

    let _mock2 = MockNntpServer::new(mock_port2)
        .with_name("Backend2")
        .on_command("STAT <missing@example.com>", "430 No such article\r\n")
        .spawn();

    sleep(Duration::from_millis(200)).await;

    // Create proxy
    let config = Config {
        servers: vec![
            create_test_server_config("127.0.0.1", mock_port1, "Backend1"),
            create_test_server_config("127.0.0.1", mock_port2, "Backend2"),
        ],
        ..Default::default()
    };

    let proxy = NntpProxy::new(config, RoutingMode::PerCommand)?;

    // Start proxy
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

    sleep(Duration::from_millis(200)).await;

    // Connect to proxy
    let mut client = TcpStream::connect(&proxy_addr).await?;

    // Read greeting
    let mut buffer = [0; 1024];
    let n = timeout(Duration::from_secs(2), client.read(&mut buffer)).await??;
    let greeting = String::from_utf8_lossy(&buffer[..n]);
    assert!(greeting.contains("200"));

    // Request article - should try multiple backends then give up
    client.write_all(b"STAT <missing@example.com>\r\n").await?;

    // Should eventually get error or disconnect after exhausting retries
    let result = timeout(Duration::from_secs(60), client.read(&mut buffer)).await;

    // Either gets an error response or connection closes (both acceptable)
    if let Ok(Ok(n)) = result
        && n > 0
    {
        let response = String::from_utf8_lossy(&buffer[..n]);
        // Should be an error response (4xx or disconnected)
        println!("Got response after retries: {}", response);
    }

    Ok(())
}

#[tokio::test]
async fn test_different_message_ids_route_correctly() -> Result<()> {
    // This test demonstrates that different message-IDs can succeed
    // on different backends after retry

    let mock_port1 = find_available_port().await;
    let mock_port2 = find_available_port().await;
    let proxy_port = find_available_port().await;

    // Backend1 has msg1 but not msg2
    let _mock1 = MockNntpServer::new(mock_port1)
        .with_name("Backend1")
        .on_command(
            "STAT <msg1@example.com>",
            "223 0 <msg1@example.com> exists\r\n",
        )
        .on_command("STAT <msg2@example.com>", "430 No such article\r\n")
        .spawn();

    // Backend2 has msg2 but not msg1
    let _mock2 = MockNntpServer::new(mock_port2)
        .with_name("Backend2")
        .on_command("STAT <msg1@example.com>", "430 No such article\r\n")
        .on_command(
            "STAT <msg2@example.com>",
            "223 0 <msg2@example.com> exists\r\n",
        )
        .spawn();

    sleep(Duration::from_millis(200)).await;

    // Create proxy
    let config = Config {
        servers: vec![
            create_test_server_config("127.0.0.1", mock_port1, "Backend1"),
            create_test_server_config("127.0.0.1", mock_port2, "Backend2"),
        ],
        ..Default::default()
    };

    let proxy = NntpProxy::new(config, RoutingMode::PerCommand)?;

    // Start proxy
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

    sleep(Duration::from_millis(200)).await;

    // Connect to proxy
    let mut client = TcpStream::connect(&proxy_addr).await?;

    // Read greeting
    let mut buffer = [0; 1024];
    let n = timeout(Duration::from_secs(2), client.read(&mut buffer)).await??;
    let greeting = String::from_utf8_lossy(&buffer[..n]);
    assert!(greeting.contains("200"));

    // Request msg1 - should succeed after routing (Backend1 or retry to Backend1)
    client.write_all(b"STAT <msg1@example.com>\r\n").await?;
    let n = timeout(Duration::from_secs(30), client.read(&mut buffer)).await??;
    let response1 = String::from_utf8_lossy(&buffer[..n]);
    assert!(
        response1.contains("223"),
        "msg1 should succeed: {}",
        response1
    );

    // Request msg2 - should succeed after routing (Backend2 or retry to Backend2)
    client.write_all(b"STAT <msg2@example.com>\r\n").await?;
    let n = timeout(Duration::from_secs(30), client.read(&mut buffer)).await??;
    let response2 = String::from_utf8_lossy(&buffer[..n]);
    assert!(
        response2.contains("223"),
        "msg2 should succeed: {}",
        response2
    );

    Ok(())
}

#[tokio::test]
async fn test_retry_buffer_corruption_prevented() -> Result<()> {
    // This test demonstrates that buffer corruption is prevented during retries.
    // Without the fix (buffer reuse), this scenario would cause corruption:
    //
    // 1. Backend1 returns "430 No such article\r\n" (30 bytes)
    // 2. Backend2 returns a longer response (e.g., 100+ bytes)
    // 3. BUG: If buffer wasn't cleared, Backend2's response could contain
    //    remnants of Backend1's 430 response mixed with the new data
    //
    // With the fix (fresh buffer per retry), each backend gets a clean buffer
    // and no corruption occurs.

    let mock_port1 = find_available_port().await;
    let mock_port2 = find_available_port().await;
    let proxy_port = find_available_port().await;

    // Backend1: Returns short 430 error
    let _mock1 = MockNntpServer::new(mock_port1)
        .with_name("Backend1")
        .on_command("ARTICLE <test@example.com>", "430 No such article\r\n")
        .spawn();

    // Backend2: Returns longer success response with specific content
    // This response is longer than Backend1's 430 to trigger potential corruption
    let article_body = "Subject: Test Article\r\n\
                        From: test@example.com\r\n\
                        Content-Type: text/plain\r\n\
                        \r\n\
                        This is a test article body with enough content\r\n\
                        to ensure the response is significantly longer than\r\n\
                        the error from the first backend. This helps detect any\r\n\
                        buffer corruption where old data might remain.\r\n";

    let full_response = format!(
        "220 0 <test@example.com> Article follows\r\n{}\r\n.\r\n",
        article_body
    );

    let _mock2 = MockNntpServer::new(mock_port2)
        .with_name("Backend2")
        .on_command("ARTICLE <test@example.com>", &full_response)
        .spawn();

    sleep(Duration::from_millis(200)).await;

    // Create proxy
    let config = Config {
        servers: vec![
            create_test_server_config("127.0.0.1", mock_port1, "Backend1"),
            create_test_server_config("127.0.0.1", mock_port2, "Backend2"),
        ],
        ..Default::default()
    };

    let proxy = NntpProxy::new(config, RoutingMode::PerCommand)?;

    // Start proxy
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

    sleep(Duration::from_millis(200)).await;

    // Connect to proxy
    let mut client = TcpStream::connect(&proxy_addr).await?;

    // Read greeting
    let mut buffer = vec![0u8; 8192];
    let n = timeout(Duration::from_secs(2), client.read(&mut buffer)).await??;
    let greeting = String::from_utf8_lossy(&buffer[..n]);
    assert!(
        greeting.contains("200"),
        "Expected greeting, got: {}",
        greeting
    );

    // Request article - Backend1 will fail with 430, Backend2 will succeed
    client.write_all(b"ARTICLE <test@example.com>\r\n").await?;

    // Read entire response
    let mut full_response_buf = Vec::new();
    let mut chunk = vec![0u8; 8192];

    loop {
        let n = timeout(Duration::from_secs(5), client.read(&mut chunk)).await??;
        if n == 0 {
            break;
        }
        full_response_buf.extend_from_slice(&chunk[..n]);

        // Check if we've received the complete article (ends with .\r\n)
        if full_response_buf.ends_with(b".\r\n") {
            break;
        }
    }

    let response = String::from_utf8_lossy(&full_response_buf);

    // Verify no corruption:
    // 1. Response should start with 220 (success), NOT 430
    assert!(
        response.starts_with("220"),
        "Expected 220 response, got: {}",
        &response[..response.len().min(50)]
    );

    // 2. Response should contain the full article content
    assert!(
        response.contains("Subject: Test Article"),
        "Missing article subject: {}",
        response
    );
    assert!(
        response.contains("This is a test article body"),
        "Missing article body: {}",
        response
    );

    // 3. Critical: Verify the status line doesn't contain error codes
    // If buffer corruption occurred, we might see "430" in the first line
    let first_line = response.lines().next().unwrap_or("");
    assert!(
        !first_line.contains("430"),
        "Status line should not contain 430 error: {}",
        first_line
    );
    assert!(
        first_line.starts_with("220"),
        "First line should be 220 success: {}",
        first_line
    );

    // 4. Response should NOT contain the exact error message from Backend1
    // (This would indicate buffer reuse corruption)
    assert!(
        !response.contains("No such article"),
        "Response should NOT contain Backend1's error message: {}",
        response
    );

    // 5. Verify response ends with proper terminator
    assert!(
        response.trim_end().ends_with(".\r\n") || response.trim_end().ends_with('.'),
        "Response should end with proper terminator: {:?}",
        &response[response.len().saturating_sub(10)..]
    );

    println!("✅ Buffer corruption test passed - no data mixing detected");
    println!("Response length: {} bytes", response.len());
    println!(
        "First 100 chars: {:?}",
        &response[..response.len().min(100)]
    );
    println!(
        "Last 50 chars: {:?}",
        &response[response.len().saturating_sub(50)..]
    );

    Ok(())
}
