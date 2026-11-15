//! Byte and command accounting for per-command requests whose first backend
//! exchange fails part-way.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

use nntp_proxy::config::{Routing, Server};
use nntp_proxy::types::{MaxConnections, Port};
use nntp_proxy::{Config, NntpProxy, RoutingMode};

use crate::test_helpers::{connect_and_read_greeting, spawn_test_proxy_on_random_port};

const IDS: [&str; 2] = ["<window-a@example.com>", "<window-b@example.com>"];

fn body_reply(id: &str) -> String {
    format!("222 0 {id}\r\nbody of {id}\r\n.\r\n")
}

/// A backend whose first connection to receive a `BODY` reads the next
/// command too and then closes without answering either. Every later lookup
/// is answered normally.
async fn spawn_backend_that_drops_the_first_window() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let dropped = Arc::new(AtomicBool::new(false));
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let dropped = Arc::clone(&dropped);
            tokio::spawn(async move {
                let mut stream = BufReader::new(stream);
                if stream.get_mut().write_all(b"200 ready\r\n").await.is_err() {
                    return;
                }
                let mut line = String::new();
                loop {
                    line.clear();
                    if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let reply = match line.trim_end().split_once(' ') {
                        Some(("BODY", id)) => {
                            if !dropped.swap(true, Ordering::SeqCst) {
                                line.clear();
                                let _ = stream.read_line(&mut line).await;
                                return;
                            }
                            body_reply(id)
                        }
                        _ => "200 ok\r\n".to_string(),
                    };
                    if stream.get_mut().write_all(reply.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    Ok(port)
}

/// A pipelined window whose backend connection dies before answering is
/// retried one request at a time. The window's commands reached the
/// provider, so they count as backend bytes, but the user sent each command
/// once and is charged for it once.
#[tokio::test]
async fn a_retried_article_window_charges_every_send_and_each_user_command_once() -> Result<()> {
    let port = spawn_backend_that_drops_the_first_window().await?;
    let config = Config {
        servers: vec![
            Server::builder("127.0.0.1", Port::try_new(port)?)
                .name("only")
                .max_connections(MaxConnections::try_new(2)?)
                .compress(Some(false))
                .build()?,
        ],
        routing: Routing::default(),
        ..Config::default()
    };
    let proxy = NntpProxy::builder(config)
        .with_routing_mode(RoutingMode::PerCommand)
        .build()
        .await?;
    let proxy_port = spawn_test_proxy_on_random_port(proxy.clone(), true).await?;
    let mut client = connect_and_read_greeting(proxy_port).await?;
    let before = proxy.metrics().snapshot(None);

    let commands: Vec<String> = IDS.iter().map(|id| format!("BODY {id}\r\n")).collect();
    // One write, so both requests form one upstream window.
    client.write_all(commands.concat().as_bytes()).await?;
    let expected: String = IDS.iter().map(|id| body_reply(id)).collect();
    let mut actual = vec![0; expected.len()];
    tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut actual)).await??;
    assert_eq!(String::from_utf8_lossy(&actual), expected);
    let command_bytes: u64 = commands.iter().map(|command| command.len() as u64).sum();
    // User bytes for a forwarded reply land just after the client has it.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let after = loop {
        let after = proxy.metrics().snapshot(None);
        let received = |stats: &nntp_proxy::metrics::MetricsSnapshot| {
            stats
                .user_stats
                .first()
                .map_or(0, |user| user.bytes_received.as_u64())
        };
        if received(&after) - received(&before) >= expected.len() as u64
            || tokio::time::Instant::now() >= deadline
        {
            break after;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };

    let backend = |stats: &nntp_proxy::metrics::MetricsSnapshot| {
        let backend = &stats.backend_stats[0];
        (
            backend.total_commands.get(),
            backend.bytes_sent.as_u64(),
            backend.bytes_received.as_u64(),
        )
    };
    let user = |stats: &nntp_proxy::metrics::MetricsSnapshot| {
        stats.user_stats.first().map_or((0, 0, 0), |user| {
            (
                user.total_commands.get(),
                user.bytes_sent.as_u64(),
                user.bytes_received.as_u64(),
            )
        })
    };
    let (commands_after, sent_after, received_after) = backend(&after);
    let (commands_before, sent_before, received_before) = backend(&before);
    let (user_commands_after, user_sent_after, user_received_after) = user(&after);
    let (user_commands_before, user_sent_before, user_received_before) = user(&before);
    assert_eq!(
        [
            (
                commands_after - commands_before,
                sent_after - sent_before,
                received_after - received_before
            ),
            (
                user_commands_after - user_commands_before,
                user_sent_after - user_sent_before,
                user_received_after - user_received_before
            ),
        ],
        [
            (4, 2 * command_bytes, expected.len() as u64),
            (2, command_bytes, expected.len() as u64),
        ],
        "(commands, bytes sent, bytes received) for the backend (window and retry), then the user"
    );
    Ok(())
}

/// A backend that answers every `BODY` with a body far larger than socket
/// buffers, so a client that stops reading makes the proxy's write fail.
async fn spawn_backend_with_large_bodies() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut stream = BufReader::new(stream);
                if stream.get_mut().write_all(b"200 ready\r\n").await.is_err() {
                    return;
                }
                let mut line = String::new();
                loop {
                    line.clear();
                    if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let reply = match line.trim_end().split_once(' ') {
                        Some(("BODY", id)) => {
                            format!("222 0 {id}\r\n{}\r\n.\r\n", "x".repeat(16 * 1024 * 1024))
                        }
                        _ => "200 ok\r\n".to_string(),
                    };
                    if stream.get_mut().write_all(reply.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    Ok(port)
}

/// A window whose client disconnects while the first reply is being written
/// never reaches its later replies, but the user sent every command in it.
#[tokio::test]
async fn a_window_cut_short_by_the_client_counts_every_user_command() -> Result<()> {
    let port = spawn_backend_with_large_bodies().await?;
    let config = Config {
        servers: vec![
            Server::builder("127.0.0.1", Port::try_new(port)?)
                .name("only")
                .max_connections(MaxConnections::try_new(2)?)
                .compress(Some(false))
                .build()?,
        ],
        routing: Routing::default(),
        ..Config::default()
    };
    let proxy = NntpProxy::builder(config)
        .with_routing_mode(RoutingMode::PerCommand)
        .build()
        .await?;
    let proxy_port = spawn_test_proxy_on_random_port(proxy.clone(), true).await?;
    let mut client = connect_and_read_greeting(proxy_port).await?;
    let user_commands = |proxy: &NntpProxy| {
        proxy
            .metrics()
            .snapshot(None)
            .user_stats
            .first()
            .map_or(0, |user| user.total_commands.get())
    };
    let before = user_commands(&proxy);

    // One write, so both requests form one upstream window.
    let commands: Vec<String> = IDS.iter().map(|id| format!("BODY {id}\r\n")).collect();
    client.write_all(commands.concat().as_bytes()).await?;
    let mut status = vec![0; format!("222 0 {}\r\n", IDS[0]).len()];
    tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut status)).await??;
    // Close with the first body unread, so its write fails.
    drop(client);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while user_commands(&proxy) - before < 2 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        user_commands(&proxy) - before,
        2,
        "user commands for a two-request window"
    );
    Ok(())
}
