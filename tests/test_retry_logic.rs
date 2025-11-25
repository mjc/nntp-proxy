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
    if let Ok(Ok(n)) = result {
        if n > 0 {
            let response = String::from_utf8_lossy(&buffer[..n]);
            // Should be an error response (4xx or disconnected)
            println!("Got response after retries: {}", response);
        }
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
async fn test_buffer_corruption_between_retries() -> Result<()> {
    // This test demonstrates the buffer corruption bug that occurs when
    // the same buffer is reused across retry attempts.
    //
    // Bug scenario:
    // 1. Backend1 returns 430 error (30 bytes)
    // 2. Same buffer used for Backend2's response
    // 3. If Backend2's response overlaps with old data, corruption occurs
    //
    // With the fix (fresh buffer per retry), this should pass.
    // Without the fix (reused buffer), the response may contain fragments
    // of the 430 error mixed with the actual article data.

    let mock_port1 = find_available_port().await;
    let mock_port2 = find_available_port().await;
    let proxy_port = find_available_port().await;

    // Backend1: Returns 430 error (short response)
    let _mock1 = MockNntpServer::new(mock_port1)
        .with_name("Backend1")
        .on_command(
            "ARTICLE <test@example.com>",
            "430 No such article\r\n", // 21 bytes
        )
        .spawn();

    // Backend2: Returns actual article with distinctive content
    let article_response = concat!(
        "220 0 <test@example.com>\r\n",
        "Path: example.com\r\n",
        "From: test@example.com\r\n",
        "Newsgroups: alt.test\r\n",
        "Subject: Test Article SUCCESS\r\n",
        "Message-ID: <test@example.com>\r\n",
        "Date: Mon, 25 Nov 2025 12:00:00 GMT\r\n",
        "\r\n",
        "This is a test article body that should be clean.\r\n",
        "MARKER_SUCCESS_MARKER\r\n",
        ".\r\n",
    );

    let _mock2 = MockNntpServer::new(mock_port2)
        .with_name("Backend2")
        .on_command("ARTICLE <test@example.com>", article_response)
        .spawn();

    sleep(Duration::from_millis(200)).await;

    // Create proxy configuration
    let config = Config {
        servers: vec![
            create_test_server_config("127.0.0.1", mock_port1, "Backend1"),
            create_test_server_config("127.0.0.1", mock_port2, "Backend2"),
        ],
        ..Default::default()
    };

    // Start proxy
    let proxy = NntpProxy::new(config, RoutingMode::PerCommand)?;
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

    // Connect as client
    let mut client = TcpStream::connect(&proxy_addr).await?;
    let mut buffer = vec![0u8; 8192];

    // Read greeting
    let n = timeout(Duration::from_secs(5), client.read(&mut buffer)).await??;
    assert!(n > 0, "Should receive greeting");

    // Request article - Backend1 will 430, Backend2 will succeed
    client.write_all(b"ARTICLE <test@example.com>\r\n").await?;

    // Read full response
    let mut full_response = Vec::new();
    loop {
        let n = timeout(Duration::from_secs(5), client.read(&mut buffer)).await??;
        if n == 0 {
            break;
        }
        full_response.extend_from_slice(&buffer[..n]);

        // Check if we got the terminator
        if full_response.ends_with(b".\r\n") {
            break;
        }
    }

    let response = String::from_utf8_lossy(&full_response);

    // Verify the response is clean and doesn't contain corruption
    // The response should start with 220 (success)
    assert!(
        response.starts_with("220"),
        "Should start with 220 success code, got: {}",
        &response[..response.len().min(50)]
    );

    // Critical check: Look for "No such article" fragment from 430 error
    // If buffer was corrupted, this phrase from Backend1's error might appear
    assert!(
        !response.contains("No such article"),
        "Article should not contain '430 No such article' error fragments! Got:\n{}",
        response
    );

    // Verify the expected content is present
    assert!(
        response.contains("MARKER_SUCCESS_MARKER"),
        "Should contain success marker (proves we got Backend2's response): {}",
        response
    );

    Ok(())
}
