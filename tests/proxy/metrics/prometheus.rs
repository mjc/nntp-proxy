//! End-to-end test of the Prometheus scrape endpoint against a running proxy.

use crate::test_helpers::{
    MockNntpServer, create_test_config, send_article_read_multiline_response,
    send_command_read_line, spawn_test_proxy_on_listener,
};
use anyhow::Result;
use nntp_proxy::{NntpProxy, RoutingMode};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const ARTICLE: &str = "220 0 <present@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n";

/// HTTP responses end when the server closes; `read_to_end` is banned in
/// this repository to keep NNTP reads framed, so loop on `read` instead.
async fn read_until_closed(stream: &mut TcpStream, out: &mut Vec<u8>) -> std::io::Result<()> {
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        out.extend_from_slice(&chunk[..n]);
    }
}

async fn scrape(addr: SocketAddr) -> Result<String> {
    let mut stream = TcpStream::connect(addr).await?;
    stream
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await?;
    let mut response = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(5),
        read_until_closed(&mut stream, &mut response),
    )
    .await??;
    let response = String::from_utf8(response)?;
    let (head, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("response has no header terminator"))?;
    assert_eq!(head.lines().next(), Some("HTTP/1.1 200 OK"));
    assert!(head.contains("Content-Type: text/plain; version=0.0.4"));
    Ok(body.to_string())
}

/// Value of the first sample whose name matches and whose label block
/// contains every given `key="value"` pair.
fn sample_value(body: &str, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
    body.lines()
        .filter(|line| line.split(['{', ' ']).next() == Some(name))
        .find(|line| {
            labels
                .iter()
                .all(|(key, value)| line.contains(&format!("{key}=\"{value}\"")))
        })
        .and_then(|line| line.rsplit_once(' '))
        .and_then(|(_, value)| value.parse().ok())
}

#[tokio::test]
async fn scrape_reflects_article_and_stat_traffic() -> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let _backend = MockNntpServer::new()
        .with_name("backend-a")
        .on_command("DATE", "111 20250101000000\r\n")
        .on_command("ARTICLE", ARTICLE)
        .on_command("ARTICLE <MISSING", "430 No such article\r\n")
        .on_command("STAT", "223 0 <present@example.com>\r\n")
        .spawn_on_listener(backend_listener);

    let config = create_test_config(vec![(backend_port, "backend-a")]);
    let proxy = NntpProxy::new(config, RoutingMode::PerCommand).await?;
    let proxy_listener = TcpListener::bind("127.0.0.1:0").await?;
    let proxy_port = spawn_test_proxy_on_listener(proxy.clone(), proxy_listener, true);
    let (metrics_addr, metrics_task) =
        nntp_proxy::metrics::prometheus::spawn_endpoint("127.0.0.1:0".parse()?, Arc::new(proxy))
            .await?;

    let before = scrape(metrics_addr).await?;
    assert_eq!(
        sample_value(
            &before,
            "nntp_proxy_client_commands_total",
            &[("command", "article")]
        ),
        Some(0.0)
    );

    let mut client = TcpStream::connect(("127.0.0.1", proxy_port)).await?;
    let mut greeting = [0u8; 256];
    let _ = client.read(&mut greeting).await?;
    for _ in 0..3 {
        let (status, _) =
            send_article_read_multiline_response(&mut client, "<present@example.com>").await?;
        assert!(status.starts_with("220"), "{status}");
    }
    for _ in 0..2 {
        let status = send_command_read_line(&mut client, "STAT <present@example.com>").await?;
        assert!(status.starts_with("223"), "{status}");
    }
    let (status, _) =
        send_article_read_multiline_response(&mut client, "<missing@example.com>").await?;
    assert!(status.starts_with("430"), "{status}");

    let after = scrape(metrics_addr).await?;
    let backend = [("backend", "backend-a"), ("tier", "0")];
    let at_least = |name: &str, labels: &[(&str, &str)], min: f64| {
        let value = sample_value(&after, name, labels)
            .unwrap_or_else(|| panic!("{name} {labels:?} missing from scrape:\n{after}"));
        assert!(
            value >= min,
            "{name} {labels:?} = {value}, expected >= {min}"
        );
    };

    at_least(
        "nntp_proxy_client_commands_total",
        &[("command", "article")],
        4.0,
    );
    at_least(
        "nntp_proxy_client_commands_total",
        &[("command", "stat")],
        2.0,
    );
    at_least("nntp_proxy_client_sessions_active", &[], 1.0);
    at_least("nntp_proxy_backend_commands_total", &backend, 4.0);
    at_least("nntp_proxy_backend_articles_total", &backend, 3.0);
    at_least("nntp_proxy_backend_article_bytes_total", &backend, 1.0);
    at_least("nntp_proxy_backend_article_missing_total", &backend, 1.0);
    at_least(
        "nntp_proxy_backend_bytes_total",
        &[("backend", "backend-a"), ("direction", "received")],
        1.0,
    );
    at_least(
        "nntp_proxy_backend_connect_attempts_total",
        &[("backend", "backend-a"), ("outcome", "created")],
        1.0,
    );
    at_least("nntp_proxy_backend_pool_max_connections", &backend, 1.0);
    at_least("nntp_proxy_build_info", &[], 1.0);

    // Every non-comment line is a well-formed `name[{labels}] value` sample.
    for line in after.lines().filter(|line| !line.is_empty()) {
        if line.as_bytes().first() == Some(&b'#') {
            assert!(
                line.split(' ')
                    .nth(1)
                    .is_some_and(|word| word == "HELP" || word == "TYPE"),
                "{line}"
            );
            continue;
        }
        let (_, value) = line.rsplit_once(' ').expect("sample has a value");
        assert!(value.parse::<f64>().is_ok(), "bad value in {line:?}");
    }

    metrics_task.abort();
    Ok(())
}
