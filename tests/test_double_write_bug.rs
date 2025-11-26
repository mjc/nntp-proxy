//! Test for double-write bug in response streaming
//!
//! Regression test for bug introduced in commit 8e57227 where the first chunk
//! was written twice to the client:
//! 1. Once in stream_response() before calling stream_multiline_response()
//! 2. Again inside stream_multiline_response() which processes the first chunk
//!
//! This caused corrupted responses where the first chunk appeared twice,
//! breaking article downloads.

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Duration;

mod config_helpers;
mod test_helpers;

use config_helpers::create_test_server_config;
use nntp_proxy::{Config, NntpProxy, RoutingMode};
use test_helpers::MockNntpServer;

#[tokio::test]
async fn test_first_chunk_not_written_twice() -> Result<()> {
    // Setup backend server
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    drop(backend_listener);

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await?;
    let proxy_port = proxy_listener.local_addr()?.port();

    // Start mock backend with a very recognizable first line
    let mock = MockNntpServer::new(backend_port)
        .with_name("Test Backend")
        .on_command(
            "ARTICLE <test@example.com>",
            "220 0 <test@example.com> Article follows\r\nFIRST_LINE_OF_ARTICLE\r\nSecond line\r\nThird line\r\n.\r\n"
        )
        .on_command("QUIT", "205 Goodbye\r\n")
        .spawn();

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create and start proxy
    let config = Config {
        servers: vec![create_test_server_config(
            "127.0.0.1",
            backend_port,
            "backend",
        )],
        ..Default::default()
    };

    let proxy = NntpProxy::new(config, RoutingMode::PerCommand)?;
    let proxy_handle = tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = proxy_listener.accept().await {
                let proxy_clone = proxy.clone();
                tokio::spawn(async move {
                    let _ = proxy_clone.handle_client(stream, addr).await;
                });
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connect to proxy as a client
    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;
    let (reader, mut writer) = client.split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    // Read greeting
    reader.read_line(&mut line).await?;
    assert!(line.starts_with("200"));

    // Send ARTICLE command
    writer.write_all(b"ARTICLE <test@example.com>\r\n").await?;

    // Read response
    line.clear();
    reader.read_line(&mut line).await?;
    assert!(
        line.starts_with("220"),
        "Expected 220 response, got: {}",
        line
    );

    // Read article lines
    let mut article_lines = Vec::new();
    loop {
        line.clear();
        reader.read_line(&mut line).await?;
        if line == ".\r\n" {
            break;
        }
        article_lines.push(line.clone());
    }

    // CRITICAL TEST: The first line should appear EXACTLY ONCE
    // Bug: Would appear twice due to double-write
    let first_line_count = article_lines
        .iter()
        .filter(|l| l.contains("FIRST_LINE_OF_ARTICLE"))
        .count();

    assert_eq!(
        first_line_count, 1,
        "FIRST_LINE_OF_ARTICLE should appear exactly once, but appeared {} times.\nArticle lines: {:?}",
        first_line_count, article_lines
    );

    // Verify we got all expected lines in correct order
    assert_eq!(
        article_lines.len(),
        3,
        "Should have exactly 3 lines before terminator"
    );
    assert!(article_lines[0].contains("FIRST_LINE_OF_ARTICLE"));
    assert!(article_lines[1].contains("Second line"));
    assert!(article_lines[2].contains("Third line"));

    // Send QUIT
    writer.write_all(b"QUIT\r\n").await?;
    line.clear();
    reader.read_line(&mut line).await?;
    assert!(line.starts_with("205"));

    proxy_handle.abort();
    drop(mock);

    Ok(())
}

#[tokio::test]
async fn test_multiline_response_integrity() -> Result<()> {
    // Setup backend server
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    drop(backend_listener);

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await?;
    let proxy_port = proxy_listener.local_addr()?.port();

    // Start mock backend
    let mock = MockNntpServer::new(backend_port)
        .with_name("Test Backend")
        .on_command(
            "ARTICLE <test@example.com>",
            "220 0 <test@example.com> Article follows\r\nFIRST_LINE_OF_ARTICLE\r\nSecond line\r\nThird line\r\n.\r\n"
        )
        .on_command("QUIT", "205 Goodbye\r\n")
        .spawn();

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create and start proxy
    let config = Config {
        servers: vec![create_test_server_config(
            "127.0.0.1",
            backend_port,
            "backend",
        )],
        ..Default::default()
    };

    let proxy = NntpProxy::new(config, RoutingMode::PerCommand)?;
    let proxy_handle = tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = proxy_listener.accept().await {
                let proxy_clone = proxy.clone();
                tokio::spawn(async move {
                    let _ = proxy_clone.handle_client(stream, addr).await;
                });
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connect to proxy
    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;
    let (reader, mut writer) = client.split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    // Read greeting
    reader.read_line(&mut line).await?;

    // Send ARTICLE command
    writer.write_all(b"ARTICLE <test@example.com>\r\n").await?;

    // Collect entire response
    let mut all_lines = Vec::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }
        all_lines.push(line.clone());
        if line == ".\r\n" {
            break;
        }
    }

    // Verify response structure:
    // 1. Status line (220)
    // 2. Article content lines (3 lines)
    // 3. Terminator (.)
    assert_eq!(
        all_lines.len(),
        5,
        "Should have 5 lines total (status + 3 content + terminator), got: {:?}",
        all_lines
    );

    // Check for any duplicates (would indicate double-write bug)
    for (i, line_i) in all_lines.iter().enumerate() {
        for (j, line_j) in all_lines.iter().enumerate() {
            if i != j && line_i == line_j && line_i != ".\r\n" {
                panic!(
                    "Found duplicate line at positions {} and {}: {}",
                    i, j, line_i
                );
            }
        }
    }

    proxy_handle.abort();
    drop(mock);

    Ok(())
}
