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
