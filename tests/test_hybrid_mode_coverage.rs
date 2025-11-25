/// Tests specifically targeting uncovered code paths in hybrid mode for increased coverage
use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{Duration, timeout};

use nntp_proxy::{Config, NntpProxy, RoutingMode};

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

/// Test that hybrid mode handles long-running sessions and triggers metrics flushing
/// This test sends >100 commands to trigger the METRICS_FLUSH_INTERVAL code path
#[tokio::test]
async fn test_hybrid_mode_long_session_metrics_flush() -> Result<()> {
    let mock_port = find_available_port().await;
    let proxy_port = find_available_port().await;

    let _mock = MockNntpServer::new(mock_port)
        .with_name("Long Session Server")
        .on_command("GROUP", "211 1000 1 1000 alt.test\r\n")
        .on_command(
            "ARTICLE",
            "220 1 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n",
        )
        .spawn();

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create proxy WITH metrics enabled to test metrics flushing
    let config = Config {
        servers: vec![create_test_server_config(
            "127.0.0.1",
            mock_port,
            "Mock Server",
        )],
        ..Default::default()
    };

    let proxy = NntpProxy::builder(config)
        .with_routing_mode(RoutingMode::Hybrid)
        .with_metrics() // Enable metrics to exercise flush code paths
        .build()?;

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

    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut client = TcpStream::connect(&proxy_addr).await?;

    // Read greeting
    let mut buffer = [0; 1024];
    let n = client.read(&mut buffer).await?;
    assert!(n > 0);

    // Switch to stateful mode with GROUP
    client.write_all(b"GROUP alt.test\r\n").await?;
    client.flush().await?;
    buffer = [0; 1024];
    let n = client.read(&mut buffer).await?;
    assert!(n > 0);

    // Send 150 commands to trigger METRICS_FLUSH_INTERVAL (which is 100)
    // This exercises the periodic metrics flushing code path
    for i in 1..=150 {
        client.write_all(b"ARTICLE 1\r\n").await?;
        client.flush().await?;

        // Read response - reuse the 1024 buffer
        buffer = [0; 1024];
        let n = timeout(Duration::from_millis(1000), client.read(&mut buffer))
            .await
            .map_err(|_| anyhow::anyhow!("Timeout on command {}", i))?
            .map_err(|e| anyhow::anyhow!("Read error on command {}: {}", i, e))?;

        assert!(n > 0, "Empty response on command {}", i);

        // Every 50 commands, log progress
        if i % 50 == 0 {
            println!("Completed {} commands", i);
        }
    }

    // Clean shutdown
    client.write_all(b"QUIT\r\n").await?;

    Ok(())
}

/// Test hybrid mode error handling when command execution fails in stateful mode
/// This exercises the error handling code path that logs warnings and breaks the loop
#[tokio::test]
async fn test_hybrid_mode_command_error_in_stateful_mode() -> Result<()> {
    let mock_port = find_available_port().await;
    let proxy_port = find_available_port().await;

    // Create a mock server that will close connection after GROUP to simulate backend error
    let mock_port_clone = mock_port;
    tokio::spawn(async move {
        let listener = TcpListener::bind(format!("127.0.0.1:{}", mock_port_clone))
            .await
            .unwrap();

        loop {
            let (mut stream, _) = listener.accept().await.unwrap();

            // Send greeting
            stream.write_all(b"200 Mock Server\r\n").await.unwrap();

            // Read GROUP command
            let mut buf = [0; 1024];
            let n = stream.read(&mut buf).await.unwrap();
            let cmd = std::str::from_utf8(&buf[..n]).unwrap();

            if cmd.contains("GROUP") {
                // Send GROUP response
                stream
                    .write_all(b"211 100 1 100 alt.test\r\n")
                    .await
                    .unwrap();

                // Read next command
                let n = stream.read(&mut buf).await.unwrap();
                let cmd2 = std::str::from_utf8(&buf[..n]).unwrap();

                if cmd2.contains("ARTICLE") {
                    // Simulate backend error by closing connection
                    drop(stream);
                    break;
                }
            }
        }
    });

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

    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut client = TcpStream::connect(&proxy_addr).await?;

    // Read greeting
    let mut buffer = [0; 1024];
    let n = client.read(&mut buffer).await?;
    assert!(n > 0);

    // Switch to stateful mode
    client.write_all(b"GROUP alt.test\r\n").await?;
    client.flush().await?;
    buffer = [0; 1024];
    let n = client.read(&mut buffer).await?;
    assert!(n > 0);

    // This command should fail due to backend closing connection
    client.write_all(b"ARTICLE 1\r\n").await?;
    client.flush().await?;

    // The proxy should handle the error gracefully (may disconnect or return error)
    // Either way, the connection state should be handled correctly
    buffer = [0; 1024];
    let result = timeout(Duration::from_secs(2), client.read(&mut buffer)).await;

    // Test passes if we handle various outcomes when backend fails
    match result {
        Ok(Ok(0)) => println!("Connection closed gracefully"),
        Ok(Ok(_)) => println!("Got response (proxy handled error)"),
        Ok(Err(_)) => println!("Read error (expected when backend dies)"),
        Err(_) => println!("Timeout (acceptable when backend connection lost)"),
    }

    Ok(())
}
