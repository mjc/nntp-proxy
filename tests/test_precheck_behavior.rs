//! Tests for precheck behavior to ensure it always checks ALL backends
//!
//! These tests verify that precheck doesn't fast-fail and actually builds
//! complete availability bitmaps for AWR to use.

use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Duration;

mod test_helpers;

async fn find_available_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

fn create_test_config_with_servers(servers: Vec<(&str, &str, u16)>) -> nntp_proxy::config::Config {
    use nntp_proxy::config::Server;

    let server_configs: Vec<Server> = servers
        .into_iter()
        .map(|(name, host, port)| {
            Server::builder(host, port)
                .name(name)
                .max_connections(5)
                .build()
                .expect("Valid server config")
        })
        .collect();

    let mut config = nntp_proxy::config::Config {
        servers: server_configs,
        ..Default::default()
    };

    // Enable adaptive precheck
    config.routing.adaptive_precheck = true;

    config
}

/// Mock server that counts how many STAT commands it receives
struct StatCountingServer {
    port: u16,
    stat_count: Arc<AtomicUsize>,
}

impl StatCountingServer {
    async fn new() -> Result<Self> {
        let port = find_available_port().await;
        Ok(Self {
            port,
            stat_count: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn spawn(self) -> (tokio::task::JoinHandle<()>, Arc<AtomicUsize>) {
        let stat_count = Arc::clone(&self.stat_count);
        let handle = tokio::spawn(async move {
            if let Err(e) = self.run().await {
                eprintln!("Mock server error: {}", e);
            }
        });
        (handle, stat_count)
    }

    async fn run(self) -> Result<()> {
        let listener = TcpListener::bind(format!("127.0.0.1:{}", self.port)).await?;

        loop {
            let (stream, _) = listener.accept().await?;
            let stat_count = Arc::clone(&self.stat_count);

            tokio::spawn(async move {
                if let Err(e) = Self::handle_client(stream, stat_count).await {
                    eprintln!("Client handler error: {}", e);
                }
            });
        }
    }

    async fn handle_client(mut stream: TcpStream, stat_count: Arc<AtomicUsize>) -> Result<()> {
        // Send greeting
        stream.write_all(b"200 Mock NNTP Server Ready\r\n").await?;
        stream.flush().await?;

        let (reader, mut writer) = stream.split();
        let mut reader = BufReader::new(reader);
        let mut line = String::new();

        loop {
            line.clear();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                break;
            }

            let cmd = line.trim();

            if cmd.starts_with("AUTHINFO USER") {
                writer.write_all(b"381 Password required\r\n").await?;
            } else if cmd.starts_with("AUTHINFO PASS") {
                writer.write_all(b"281 Authentication accepted\r\n").await?;
            } else if cmd.starts_with("STAT") {
                // COUNT this STAT command
                stat_count.fetch_add(1, Ordering::Relaxed);
                // Respond that we have the article
                writer.write_all(b"223 0 <test@example.com>\r\n").await?;
            } else if cmd.starts_with("HEAD") {
                // Support HEAD for AUTO mode testing
                writer.write_all(b"221 0 <test@example.com>\r\n").await?;
                writer.write_all(b"Subject: Test\r\n").await?;
                writer.write_all(b".\r\n").await?;
            } else if cmd.starts_with("QUIT") {
                writer.write_all(b"205 Goodbye\r\n").await?;
                break;
            } else {
                writer.write_all(b"500 Unknown command\r\n").await?;
            }
            writer.flush().await?;
        }

        Ok(())
    }
}

/// Test that precheck checks BOTH backends, not just the first one
#[tokio::test]
async fn test_precheck_checks_all_backends() -> Result<()> {
    // Create two mock servers that count STAT commands
    let server1 = StatCountingServer::new().await?;
    let port1 = server1.port;
    let (handle1, stat_count1) = server1.spawn();

    let server2 = StatCountingServer::new().await?;
    let port2 = server2.port;
    let (handle2, stat_count2) = server2.spawn();

    // Give servers time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create config with both backends
    let config = create_test_config_with_servers(vec![
        ("Backend1", "127.0.0.1", port1),
        ("Backend2", "127.0.0.1", port2),
    ]);

    let proxy = nntp_proxy::NntpProxy::new(config, nntp_proxy::config::RoutingMode::PerCommand)?;
    let proxy_port = find_available_port().await;

    // Start proxy
    let proxy_handle = tokio::spawn(async move {
        let listener = TcpListener::bind(format!("127.0.0.1:{}", proxy_port))
            .await
            .unwrap();
        let (stream, addr) = listener.accept().await.unwrap();
        proxy.handle_client(stream, addr).await.ok();
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connect to proxy and send STAT command
    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;
    let (reader, mut writer) = client.split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    // Read greeting
    reader.read_line(&mut line).await?;
    assert!(line.starts_with("200"));

    // Send STAT command - this should trigger precheck on BOTH backends
    line.clear();
    writer.write_all(b"STAT <test@example.com>\r\n").await?;
    writer.flush().await?;
    reader.read_line(&mut line).await?;

    // Give precheck time to complete
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Check that BOTH backends received STAT commands
    let count1 = stat_count1.load(Ordering::Relaxed);
    let count2 = stat_count2.load(Ordering::Relaxed);

    println!("Backend1 received {} STATs", count1);
    println!("Backend2 received {} STATs", count2);

    // CRITICAL: Both backends must be checked, not just the first one
    assert!(
        count1 > 0,
        "Backend1 should have received at least one STAT (got {})",
        count1
    );
    assert!(
        count2 > 0,
        "Backend2 should have received at least one STAT (got {})",
        count2
    );

    // Cleanup
    writer.write_all(b"QUIT\r\n").await?;
    drop(client);
    handle1.abort();
    handle2.abort();
    proxy_handle.abort();

    Ok(())
}

/// Test that when one backend is faster, precheck still waits for both
#[tokio::test]
async fn test_precheck_waits_for_slow_backend() -> Result<()> {
    // TODO: Implement test where backend1 responds in 10ms, backend2 in 500ms
    // Verify that both get their STAT counted (no fast-fail)
    Ok(())
}

/// Test that precheck results populate the location cache correctly
#[tokio::test]
async fn test_precheck_populates_cache() -> Result<()> {
    // TODO: Implement test that verifies cache has availability bitmap
    // with bits set for backends that responded with 223
    Ok(())
}
