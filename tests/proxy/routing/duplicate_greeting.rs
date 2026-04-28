//! Test for duplicate greeting bug fix
//!
//! This test verifies that per-command routing mode only sends ONE greeting,
//! not two. The bug was that both `setup_client_connection()` and
//! `handle_per_command_routing()` were sending greetings.

use anyhow::Result;
use nntp_proxy::{Config, NntpProxy, RoutingMode};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::test_helpers::*;

/// Test that per-command routing mode sends exactly one greeting.
///
/// Spins up its own mock backend + proxy on random ports so the test is
/// self-contained and never races with other tests.
#[tokio::test]
async fn test_single_greeting_per_command_mode() -> Result<()> {
    // Start a minimal mock backend
    let mock_port = get_available_port().await?;
    let _mock = MockNntpServer::new(mock_port)
        .with_name("MockBackend")
        .spawn();

    // Start the proxy
    let proxy_port = get_available_port().await?;
    let config = Config {
        servers: vec![create_test_server_config("127.0.0.1", mock_port, "Mock")],
        ..Default::default()
    };
    let proxy = NntpProxy::new(config, RoutingMode::PerCommand).await?;
    spawn_test_proxy(proxy, proxy_port, true).await;

    // Give server+proxy time to be ready
    wait_for_server(&format!("127.0.0.1:{proxy_port}"), 20).await?;

    let stream = TcpStream::connect(format!("127.0.0.1:{proxy_port}")).await?;
    let (reader_half, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader_half);

    // Read the greeting — must arrive within 2s
    let mut first_line = String::new();
    timeout(Duration::from_secs(2), reader.read_line(&mut first_line))
        .await
        .expect("timed out waiting for greeting")?;

    assert!(
        first_line.starts_with("200") || first_line.starts_with("201"),
        "Expected 200/201 greeting, got: {first_line:?}"
    );

    // A second read immediately after the greeting must NOT produce another
    // greeting line.  We send a QUIT first so the proxy has something to
    // respond to; then we verify the *first* response line is not a greeting.
    writer.write_all(b"QUIT\r\n").await?;

    let mut response = String::new();
    timeout(Duration::from_secs(2), reader.read_line(&mut response))
        .await
        .expect("timed out waiting for QUIT response")?;

    assert!(
        !response.starts_with("200") && !response.starts_with("201"),
        "Got a duplicate greeting instead of QUIT response: {response:?}"
    );
    assert!(
        response.starts_with("205"),
        "Expected 205 goodbye after QUIT, got: {response:?}"
    );

    Ok(())
}

/// Test that we can fetch an article without corruption
#[tokio::test]
async fn test_article_fetch_no_corruption() -> Result<()> {
    // Start a mock backend server
    let mock_port = get_available_port().await?;
    let _mock = MockNntpServer::new(mock_port)
        .with_name("Mock Server")
        .on_command(
            "ARTICLE",
            "220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n",
        )
        .spawn();

    // Start proxy
    let proxy_port = get_available_port().await?;
    let config = Config {
        servers: vec![create_test_server_config(
            "127.0.0.1",
            mock_port,
            "TestServer",
        )],
        ..Default::default()
    };
    let proxy = NntpProxy::new(config, RoutingMode::PerCommand).await?;
    spawn_test_proxy(proxy, proxy_port, true).await;
    wait_for_server(&format!("127.0.0.1:{proxy_port}"), 20).await?;

    // Connect to proxy
    let stream = TcpStream::connect(format!("127.0.0.1:{proxy_port}")).await?;
    let (reader_half, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader_half);

    // Read greeting
    let mut line = String::new();
    timeout(Duration::from_secs(2), reader.read_line(&mut line))
        .await
        .expect("timed out waiting for greeting")?;
    assert!(
        line.starts_with("200") || line.starts_with("201"),
        "Expected 200/201 greeting, got: {}",
        line.trim()
    );

    // Send ARTICLE command
    writer.write_all(b"ARTICLE <test@example.com>\r\n").await?;
    writer.flush().await?;

    // Read response - should be 220 (article follows)
    line.clear();
    timeout(Duration::from_secs(2), reader.read_line(&mut line))
        .await
        .expect("timed out waiting for ARTICLE response")?;

    // Verify we don't get a stray greeting in the article response
    assert!(
        !line.contains("NNTP Proxy Ready"),
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
