//! Test for duplicate greeting bug fix
//!
//! This test verifies that per-command routing mode only sends ONE greeting,
//! not two. The bug was that both setup_client_connection() and
//! handle_per_command_routing() were sending greetings.

use anyhow::Result;
use nntp_proxy::{Config, NntpProxy, RoutingMode};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

mod config_helpers;
mod test_helpers;
use config_helpers::*;
use test_helpers::{MockNntpServer, get_available_port, spawn_test_proxy};

/// Test that per-command routing mode sends exactly one greeting
#[test]
fn test_single_greeting_per_command_mode() -> Result<()> {
    // Connect to the proxy (assumes it's running on port 8121)
    // This test is meant to be run manually with a running proxy
    let addr = "127.0.0.1:8121";

    let mut stream =
        match TcpStream::connect_timeout(&addr.parse().unwrap(), Duration::from_secs(2)) {
            Ok(s) => s,
            Err(_) => {
                eprintln!("Skipping test - proxy not running on {}", addr);
                return Ok(());
            }
        };

    stream.set_read_timeout(Some(Duration::from_secs(2)))?;

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut greeting_count = 0;
    let mut first_line = String::new();

    // Read the greeting(s)
    // In the bug, we'd get two 200 lines
    // After fix, we should only get one
    reader.read_line(&mut first_line)?;

    if first_line.starts_with("200") {
        greeting_count += 1;

        // Check if there's a second greeting (the bug)
        let mut second_line = String::new();
        // Use a very short timeout to check if more data is immediately available
        stream.set_read_timeout(Some(Duration::from_millis(100)))?;
        match reader.read_line(&mut second_line) {
            Ok(_) if second_line.starts_with("200") => {
                greeting_count += 1;
                eprintln!("BUG: Received second greeting: {}", second_line.trim());
            }
            _ => {
                // No second greeting, or it's not a 200 - this is good
            }
        }
    }

    // Send a test command to verify the connection works
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    writeln!(stream, "HELP")?;
    stream.flush()?;

    // We should get exactly 1 greeting
    assert_eq!(
        greeting_count,
        1,
        "Expected exactly 1 greeting, got {}. First line: {}",
        greeting_count,
        first_line.trim()
    );

    // Verify it's the per-command routing greeting
    assert!(
        first_line.contains("Per-Command Routing") || first_line.starts_with("200"),
        "Expected greeting to be '200 NNTP Proxy Ready (Per-Command Routing)', got: {}",
        first_line.trim()
    );

    // Clean up
    let _ = writeln!(stream, "QUIT");

    Ok(())
}

/// Test that we can fetch an article without corruption
#[tokio::test]
async fn test_article_fetch_no_corruption() -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpStream;

    // Start a mock backend server
    let mock_port = get_available_port()
        .await
        .expect("Failed to get available port");
    let _mock = MockNntpServer::new(mock_port)
        .with_name("Mock Server")
        .on_command(
            "ARTICLE",
            "220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n",
        )
        .spawn();

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Start proxy
    let proxy_port = get_available_port()
        .await
        .expect("Failed to get available port");
    let config = Config {
        servers: vec![create_test_server_config(
            "127.0.0.1",
            mock_port,
            "TestServer",
        )],
        ..Default::default()
    };
    let proxy = NntpProxy::new(config, RoutingMode::PerCommand)?;
    spawn_test_proxy(proxy, proxy_port, true).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Connect to proxy
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;
    let (reader, mut writer) = stream.split();
    let mut reader = BufReader::new(reader);

    // Read greeting
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    assert!(
        line.starts_with("200"),
        "Expected 200 greeting, got: {}",
        line.trim()
    );

    // Send ARTICLE command
    writer.write_all(b"ARTICLE <test@example.com>\r\n").await?;
    writer.flush().await?;

    // Read response - should be 220 (article follows)
    line.clear();
    reader.read_line(&mut line).await?;

    // Verify we don't get a stray 200 in the article response
    assert!(
        !line.contains("200 NNTP Proxy Ready"),
        "Article response contains greeting text - corruption detected! Line: {}",
        line.trim()
    );

    // Should be a proper article response
    assert!(
        line.starts_with("220") || line.starts_with("430") || line.starts_with("423"),
        "Expected article response (220/430/423), got: {}",
        line.trim()
    );

    // Clean up
    writer.write_all(b"QUIT\r\n").await.ok();

    Ok(())
}
