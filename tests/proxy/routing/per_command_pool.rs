//! Regression tests for per-command backend pool ownership.

use anyhow::Result;
use nntp_proxy::config::{Cache, Config};
use nntp_proxy::{NntpProxy, RoutingMode};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

use crate::test_helpers::{
    connect_and_read_greeting, create_test_server_config_with_max_connections,
    send_command_read_line, spawn_test_proxy_on_random_port, wait_for_server,
};

struct CountingBackend {
    port: u16,
    accepts: Arc<AtomicUsize>,
    stat_requests: Arc<AtomicUsize>,
    _handle: tokio::task::JoinHandle<()>,
}

async fn spawn_counting_stat_backend() -> Result<CountingBackend> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let accepts = Arc::new(AtomicUsize::new(0));
    let stat_requests = Arc::new(AtomicUsize::new(0));
    let accepts_for_task = Arc::clone(&accepts);
    let requests_for_task = Arc::clone(&stat_requests);

    let handle = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            accepts_for_task.fetch_add(1, Ordering::SeqCst);
            let requests = Arc::clone(&requests_for_task);

            tokio::spawn(async move {
                if stream
                    .write_all(b"200 CountingBackend Ready\r\n")
                    .await
                    .is_err()
                {
                    return;
                }

                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                    let cmd = line.trim().to_uppercase();
                    let writer = reader.get_mut();

                    if cmd.starts_with("STAT") {
                        requests.fetch_add(1, Ordering::SeqCst);
                        if writer
                            .write_all(b"223 0 <pool-release@example.com> status\r\n")
                            .await
                            .is_err()
                        {
                            break;
                        }
                    } else if cmd.starts_with("DATE") {
                        let _ = writer.write_all(b"111 20260520000000\r\n").await;
                    } else if cmd.starts_with("QUIT") {
                        let _ = writer.write_all(b"205 Goodbye\r\n").await;
                        break;
                    } else {
                        let _ = writer.write_all(b"200 OK\r\n").await;
                    }
                    line.clear();
                }
            });
        }
    });

    Ok(CountingBackend {
        port,
        accepts,
        stat_requests,
        _handle: handle,
    })
}

async fn spawn_single_slot_per_command_proxy(backend_port: u16) -> Result<u16> {
    spawn_per_command_proxy(backend_port, 1).await
}

async fn spawn_per_command_proxy(backend_port: u16, max_connections: usize) -> Result<u16> {
    let config = Config {
        servers: vec![create_test_server_config_with_max_connections(
            "127.0.0.1",
            backend_port,
            "CountingBackend",
            max_connections,
        )],
        cache: Some(Cache {
            adaptive_precheck: false,
            ..Default::default()
        }),
        ..Default::default()
    };
    let proxy = NntpProxy::new(config, RoutingMode::PerCommand).await?;
    spawn_test_proxy_on_random_port(proxy, true).await
}

#[tokio::test]
async fn per_command_releases_backend_connection_between_client_batches() -> Result<()> {
    let backend = spawn_counting_stat_backend().await?;
    wait_for_server(&format!("127.0.0.1:{}", backend.port), 20).await?;
    let proxy_port = spawn_single_slot_per_command_proxy(backend.port).await?;
    wait_for_server(&format!("127.0.0.1:{proxy_port}"), 20).await?;

    let mut idle_client = connect_and_read_greeting(proxy_port).await?;
    let first = send_command_read_line(&mut idle_client, "STAT <first@example.com>").await?;
    assert!(
        first.starts_with("223"),
        "first STAT should succeed, got {first:?}"
    );

    let mut second_client = connect_and_read_greeting(proxy_port).await?;
    let second = tokio::time::timeout(
        Duration::from_millis(500),
        send_command_read_line(&mut second_client, "STAT <second@example.com>"),
    )
    .await
    .map_err(|_| anyhow::anyhow!("second client blocked waiting for the only backend slot"))??;

    assert!(
        second.starts_with("223"),
        "second STAT should succeed, got {second:?}"
    );
    assert_eq!(backend.stat_requests.load(Ordering::SeqCst), 2);

    Ok(())
}

#[tokio::test]
async fn per_command_reuses_backend_connection_inside_one_pipeline_batch() -> Result<()> {
    let backend = spawn_counting_stat_backend().await?;
    wait_for_server(&format!("127.0.0.1:{}", backend.port), 20).await?;
    let proxy_port = spawn_single_slot_per_command_proxy(backend.port).await?;
    wait_for_server(&format!("127.0.0.1:{proxy_port}"), 20).await?;
    let accepts_before_commands = backend.accepts.load(Ordering::SeqCst);

    let mut client = connect_and_read_greeting(proxy_port).await?;
    client
        .write_all(b"STAT <first@example.com>\r\nSTAT <second@example.com>\r\n")
        .await?;

    let first = crate::test_helpers::read_line_from_stream(&mut client, "first STAT").await?;
    let second = crate::test_helpers::read_line_from_stream(&mut client, "second STAT").await?;

    assert!(first.starts_with("223"), "first STAT got {first:?}");
    assert!(second.starts_with("223"), "second STAT got {second:?}");
    assert_eq!(backend.stat_requests.load(Ordering::SeqCst), 2);
    assert_eq!(
        backend.accepts.load(Ordering::SeqCst),
        accepts_before_commands + 1,
        "pipelined commands in one batch should reuse one backend connection"
    );

    Ok(())
}

#[tokio::test]
async fn per_command_pipeline_uses_idle_pool_capacity_before_reusing_connection() -> Result<()> {
    let backend = spawn_counting_stat_backend().await?;
    wait_for_server(&format!("127.0.0.1:{}", backend.port), 20).await?;
    let proxy_port = spawn_per_command_proxy(backend.port, 2).await?;
    wait_for_server(&format!("127.0.0.1:{proxy_port}"), 20).await?;
    let accepts_before_commands = backend.accepts.load(Ordering::SeqCst);

    let mut client = connect_and_read_greeting(proxy_port).await?;
    client
        .write_all(b"STAT <first@example.com>\r\nSTAT <second@example.com>\r\n")
        .await?;

    let first = crate::test_helpers::read_line_from_stream(&mut client, "first STAT").await?;
    let second = crate::test_helpers::read_line_from_stream(&mut client, "second STAT").await?;

    assert!(first.starts_with("223"), "first STAT got {first:?}");
    assert!(second.starts_with("223"), "second STAT got {second:?}");
    assert_eq!(backend.stat_requests.load(Ordering::SeqCst), 2);
    assert_eq!(
        backend.accepts.load(Ordering::SeqCst),
        accepts_before_commands + 2,
        "pipelined commands should use another backend connection before reusing a checked-out one"
    );

    Ok(())
}

#[tokio::test]
async fn per_command_releases_backend_connection_after_trailing_non_pipelineable_command()
-> Result<()> {
    let backend = spawn_counting_stat_backend().await?;
    wait_for_server(&format!("127.0.0.1:{}", backend.port), 20).await?;
    let proxy_port = spawn_single_slot_per_command_proxy(backend.port).await?;
    wait_for_server(&format!("127.0.0.1:{proxy_port}"), 20).await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;
    client
        .write_all(b"STAT <first@example.com>\r\nDATE\r\n")
        .await?;

    let stat = crate::test_helpers::read_line_from_stream(&mut client, "STAT").await?;
    let date = crate::test_helpers::read_line_from_stream(&mut client, "DATE").await?;
    assert!(stat.starts_with("223"), "STAT got {stat:?}");
    assert!(date.starts_with("111"), "DATE got {date:?}");

    let mut second_client = connect_and_read_greeting(proxy_port).await?;
    let second = tokio::time::timeout(
        Duration::from_millis(500),
        send_command_read_line(&mut second_client, "STAT <second@example.com>"),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!("second client blocked after a batch with a trailing command")
    })??;

    assert!(second.starts_with("223"), "second STAT got {second:?}");

    Ok(())
}

#[tokio::test]
async fn per_command_releases_backend_connection_after_single_non_pipelineable_command()
-> Result<()> {
    let backend = spawn_counting_stat_backend().await?;
    wait_for_server(&format!("127.0.0.1:{}", backend.port), 20).await?;
    let proxy_port = spawn_single_slot_per_command_proxy(backend.port).await?;
    wait_for_server(&format!("127.0.0.1:{proxy_port}"), 20).await?;

    let mut idle_client = connect_and_read_greeting(proxy_port).await?;
    let date = send_command_read_line(&mut idle_client, "DATE").await?;
    assert!(date.starts_with("111"), "DATE got {date:?}");

    let mut second_client = connect_and_read_greeting(proxy_port).await?;
    let second = tokio::time::timeout(
        Duration::from_millis(500),
        send_command_read_line(&mut second_client, "STAT <after-date@example.com>"),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!("second client blocked after a single non-pipelineable command")
    })??;

    assert!(second.starts_with("223"), "second STAT got {second:?}");

    Ok(())
}
