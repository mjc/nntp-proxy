//! Tests for COMPRESS DEFLATE negotiation (RFC 8054)
//!
//! These tests verify the protocol-level compression negotiation behavior:
//! - Disabled: proxy never sends COMPRESS DEFLATE
//! - Auto-detect: proxy falls back gracefully when server doesn't support compression
//! - Required: proxy returns an error when server doesn't support compression

use anyhow::Result;
use nntp_proxy::config::Server;
use nntp_proxy::types::{MaxConnections, Port};
use nntp_proxy::{Config, NntpProxy, RoutingMode};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Duration;

use crate::test_helpers::{get_available_port, wait_for_server};

/// Mock NNTP server that tracks whether COMPRESS DEFLATE was received
struct CompressionMockServer {
    port: u16,
    compress_received: Arc<AtomicBool>,
    /// Response to send when COMPRESS DEFLATE is received
    compress_response: String,
}

impl CompressionMockServer {
    fn new(port: u16, compress_response: &str) -> Self {
        Self {
            port,
            compress_received: Arc::new(AtomicBool::new(false)),
            compress_response: compress_response.to_string(),
        }
    }

    fn compress_was_received(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.compress_received)
    }

    fn spawn(self) -> tokio::task::AbortHandle {
        let port = self.port;
        let compress_received = self.compress_received;
        let compress_response = self.compress_response;

        tokio::spawn(async move {
            let listener = TcpListener::bind(format!("127.0.0.1:{port}"))
                .await
                .expect("Failed to bind");

            while let Ok((mut stream, _)) = listener.accept().await {
                let compress_received = compress_received.clone();
                let compress_response = compress_response.clone();

                tokio::spawn(async move {
                    if stream
                        .write_all(b"200 CompressionTest Ready\r\n")
                        .await
                        .is_err()
                    {
                        return;
                    }

                    let mut buffer = [0u8; 4096];

                    loop {
                        let n = match stream.read(&mut buffer).await {
                            Ok(0) => break,
                            Ok(n) => n,
                            Err(_) => break,
                        };

                        let cmd = String::from_utf8_lossy(&buffer[..n]);
                        let cmd_upper = cmd.trim().to_uppercase();

                        if cmd_upper.starts_with("COMPRESS") {
                            compress_received.store(true, Ordering::SeqCst);
                            let _ = stream.write_all(compress_response.as_bytes()).await;
                        } else if cmd_upper.starts_with("QUIT") {
                            let _ = stream.write_all(b"205 Goodbye\r\n").await;
                            break;
                        } else if cmd_upper.starts_with("STAT") {
                            let _ = stream
                                .write_all(b"223 0 <test@example.com> exists\r\n")
                                .await;
                        } else {
                            let _ = stream.write_all(b"200 OK\r\n").await;
                        }
                    }
                });
            }
        })
        .abort_handle()
    }
}

fn build_server_config(port: u16, compress: Option<bool>) -> Server {
    Server::builder("127.0.0.1", Port::try_new(port).unwrap())
        .name("CompressionTest")
        .max_connections(MaxConnections::try_new(5).unwrap())
        .compress(compress)
        .build()
        .expect("Valid server config")
}

/// When compress is disabled, the proxy should never send COMPRESS DEFLATE
#[tokio::test]
async fn test_compression_disabled_skips_negotiation() -> Result<()> {
    let mock_port = get_available_port().await?;
    let proxy_port = get_available_port().await?;

    let mock = CompressionMockServer::new(mock_port, "206 Compression active\r\n");
    let compress_received = mock.compress_was_received();
    let _handle = mock.spawn();

    tokio::time::sleep(Duration::from_millis(50)).await;

    let config = Config {
        servers: vec![build_server_config(mock_port, Some(false))],
        ..Default::default()
    };

    let proxy = NntpProxy::new(config, RoutingMode::PerCommand).await?;
    let proxy_addr = format!("127.0.0.1:{proxy_port}");
    let listener = TcpListener::bind(&proxy_addr).await?;

    tokio::spawn({
        let proxy = proxy.clone();
        async move {
            while let Ok((stream, addr)) = listener.accept().await {
                let p = proxy.clone();
                tokio::spawn(async move {
                    let _ = p
                        .handle_client_per_command_routing(stream, addr.into())
                        .await;
                });
            }
        }
    });

    wait_for_server(&proxy_addr, 20).await?;

    let mut client = TcpStream::connect(&proxy_addr).await?;
    let mut reader = BufReader::new(&mut client);
    let mut line = String::new();

    // Read greeting
    reader.read_line(&mut line).await?;
    assert!(line.starts_with("200"));
    line.clear();

    // Send a command to trigger backend connection
    reader.get_mut().write_all(b"STAT <test@test>\r\n").await?;
    reader.read_line(&mut line).await?;
    assert!(line.starts_with("223"), "Expected 223, got: {line}");

    // COMPRESS DEFLATE should never have been sent
    assert!(
        !compress_received.load(Ordering::SeqCst),
        "COMPRESS DEFLATE should not be sent when compression is disabled"
    );

    reader.get_mut().write_all(b"QUIT\r\n").await?;
    Ok(())
}

/// When compress is auto (None) and server doesn't support it, proxy falls back gracefully
#[tokio::test]
async fn test_compression_auto_fallback_on_unsupported() -> Result<()> {
    let mock_port = get_available_port().await?;
    let proxy_port = get_available_port().await?;

    // Server responds 500 to COMPRESS DEFLATE
    let mock = CompressionMockServer::new(mock_port, "500 Command not recognized\r\n");
    let compress_received = mock.compress_was_received();
    let _handle = mock.spawn();

    tokio::time::sleep(Duration::from_millis(50)).await;

    let config = Config {
        servers: vec![build_server_config(mock_port, None)],
        ..Default::default()
    };

    let proxy = NntpProxy::new(config, RoutingMode::PerCommand).await?;
    let proxy_addr = format!("127.0.0.1:{proxy_port}");
    let listener = TcpListener::bind(&proxy_addr).await?;

    tokio::spawn({
        let proxy = proxy.clone();
        async move {
            while let Ok((stream, addr)) = listener.accept().await {
                let p = proxy.clone();
                tokio::spawn(async move {
                    let _ = p
                        .handle_client_per_command_routing(stream, addr.into())
                        .await;
                });
            }
        }
    });

    wait_for_server(&proxy_addr, 20).await?;

    let mut client = TcpStream::connect(&proxy_addr).await?;
    let mut reader = BufReader::new(&mut client);
    let mut line = String::new();

    reader.read_line(&mut line).await?;
    assert!(line.starts_with("200"));
    line.clear();

    // Command should still work even though compression was rejected
    reader.get_mut().write_all(b"STAT <test@test>\r\n").await?;
    reader.read_line(&mut line).await?;
    assert!(line.starts_with("223"), "Expected 223, got: {line}");

    // COMPRESS DEFLATE was sent (auto mode tries it)
    assert!(
        compress_received.load(Ordering::SeqCst),
        "COMPRESS DEFLATE should be sent in auto mode"
    );

    reader.get_mut().write_all(b"QUIT\r\n").await?;
    Ok(())
}

/// When compress is required and server doesn't support it, proxy should fail the connection
#[tokio::test]
async fn test_compression_required_fails_on_unsupported() -> Result<()> {
    let mock_port = get_available_port().await?;
    let proxy_port = get_available_port().await?;

    // Server rejects COMPRESS DEFLATE
    let mock = CompressionMockServer::new(mock_port, "500 Command not recognized\r\n");
    let _handle = mock.spawn();

    tokio::time::sleep(Duration::from_millis(50)).await;

    let config = Config {
        servers: vec![build_server_config(mock_port, Some(true))],
        ..Default::default()
    };

    let proxy = NntpProxy::new(config, RoutingMode::PerCommand).await?;
    let proxy_addr = format!("127.0.0.1:{proxy_port}");
    let listener = TcpListener::bind(&proxy_addr).await?;

    tokio::spawn({
        let proxy = proxy.clone();
        async move {
            while let Ok((stream, addr)) = listener.accept().await {
                let p = proxy.clone();
                tokio::spawn(async move {
                    let _ = p
                        .handle_client_per_command_routing(stream, addr.into())
                        .await;
                });
            }
        }
    });

    wait_for_server(&proxy_addr, 20).await?;

    let mut client = TcpStream::connect(&proxy_addr).await?;
    let mut reader = BufReader::new(&mut client);
    let mut line = String::new();

    reader.read_line(&mut line).await?;
    assert!(line.starts_with("200"));
    line.clear();

    // Command should fail because backend connection can't be established without compression
    reader.get_mut().write_all(b"STAT <test@test>\r\n").await?;
    let result = tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line)).await;

    match result {
        Ok(Ok(0)) => {} // Connection closed — expected
        Ok(Ok(_)) => {
            // Got a response — it should not be a successful article response
            assert!(
                !line.starts_with("223"),
                "Should not get successful response when compression is required but unsupported, got: {line}"
            );
        }
        Ok(Err(_)) => {} // Read error — expected when backend unavailable
        Err(_) => {}     // Timeout — acceptable, proxy couldn't get a backend
    }

    Ok(())
}
