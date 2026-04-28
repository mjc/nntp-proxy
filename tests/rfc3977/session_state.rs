//! RFC 3977 session state compliance tests
//!
//! Tests for session state bugs:
//! - Bug 1: Proxy must send 201 (posting not permitted) since it always rejects POST
//! - Bug 3: Stateful/NonRoutable commands must return 503 (Feature not supported), not 502
//! - Bug 5: Proxy must send MODE READER to backend after connection setup

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::{Duration, timeout};

use crate::test_helpers::{MockNntpServer, create_test_server_config, get_available_port};
use nntp_proxy::NntpProxy;
use nntp_proxy::config::{Config, RoutingMode};

/// Bug 1: RFC 3977 §5.1 — proxy MUST send 201 (posting not permitted)
///
/// Since the proxy rejects ALL POST commands with 440, it must advertise
/// "posting not permitted" (201) in its initial greeting. Sending 200
/// misleads clients into believing they can post.
#[tokio::test]
async fn test_proxy_sends_201_readonly_greeting() -> Result<()> {
    let backend_port = get_available_port().await.expect("port");
    let _backend = MockNntpServer::new(backend_port)
        .with_name("TestBackend")
        .spawn();

    tokio::time::sleep(Duration::from_millis(50)).await;

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await?;
    let proxy_port = proxy_listener.local_addr()?.port();

    let config = Config {
        servers: vec![create_test_server_config("127.0.0.1", backend_port, "test")],
        ..Default::default()
    };

    let proxy = NntpProxy::new(config, RoutingMode::PerCommand).await?;

    tokio::spawn(async move {
        while let Ok((stream, addr)) = proxy_listener.accept().await {
            let proxy = proxy.clone();
            tokio::spawn(async move {
                let _ = proxy.handle_client(stream, addr.into()).await;
            });
        }
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut client = TcpStream::connect(format!("127.0.0.1:{proxy_port}")).await?;
    let mut buffer = vec![0u8; 256];
    let n = timeout(Duration::from_secs(2), client.read(&mut buffer)).await??;
    let greeting = String::from_utf8_lossy(&buffer[..n]);

    assert!(
        greeting.starts_with("201 "),
        "RFC 3977 §5.1: proxy must send 201 (posting not permitted) since it always rejects POST, \
         got: {:?}",
        greeting.trim()
    );

    Ok(())
}

/// Bug 5: RFC 3977 §5.3 — proxy must send MODE READER to backend
///
/// Without MODE READER, some NNTP servers operate in transit mode and reject
/// reader commands (ARTICLE, GROUP, etc.). RFC 3977 §5.3 defines MODE READER
/// as the mechanism to request reader mode.
#[tokio::test]
async fn test_mode_reader_sent_to_backend() -> Result<()> {
    // Inline recording backend: tracks which commands it receives
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();

    let (tx, mut rx) = mpsc::channel::<String>(64);

    tokio::spawn(async move {
        while let Ok((mut stream, _)) = backend_listener.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                stream.write_all(b"200 Ready\r\n").await.ok();
                let mut buf = [0u8; 1024];
                let mut pending: Vec<u8> = Vec::new();
                loop {
                    let n = match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    pending.extend_from_slice(&buf[..n]);
                    while let Some(pos) = pending.windows(2).position(|w| w == b"\r\n") {
                        let line: Vec<u8> = pending.drain(..pos + 2).collect();
                        let cmd = String::from_utf8_lossy(&line).trim().to_uppercase();
                        let _ = tx.send(cmd.clone()).await;
                        let response: &[u8] = if cmd.starts_with("MODE READER") {
                            b"200 Reader mode on\r\n"
                        } else if cmd.starts_with("QUIT") {
                            b"205 Goodbye\r\n"
                        } else {
                            b"200 OK\r\n"
                        };
                        stream.write_all(response).await.ok();
                        if cmd.starts_with("QUIT") {
                            return;
                        }
                    }
                }
            });
        }
    });

    let config = Config {
        servers: vec![create_test_server_config("127.0.0.1", backend_port, "test")],
        ..Default::default()
    };

    let proxy = NntpProxy::new(config, RoutingMode::PerCommand).await?;
    proxy.prewarm_connections().await.ok();

    // Allow time for the prewarm connection to complete the handshake
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut received = Vec::new();
    while let Ok(cmd) = rx.try_recv() {
        received.push(cmd);
    }

    assert!(
        received.iter().any(|c| c.starts_with("MODE READER")),
        "RFC 3977 §5.3: proxy must send MODE READER to backend during connection setup, \
         commands received by backend: {:?}",
        received
    );

    Ok(())
}
