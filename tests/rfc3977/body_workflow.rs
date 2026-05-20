//! RFC 3977 BODY workflow tests.
//!
//! These cover end-to-end BODY semantics that differ between message-id and
//! selected-group article-number retrieval, including multiline framing for
//! large bodies and the expected 412/423 error paths.

use anyhow::Result;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::net::TcpListener;
use tokio::sync::{Notify, oneshot};
use tokio::time::{Duration, timeout};

use crate::test_helpers::{
    MockNntpServer, RfcTestClient, connect_and_read_greeting,
    create_test_server_config_with_max_connections, spawn_proxy_with_config,
};
use nntp_proxy::{Config, RoutingMode, config::Cache};

const PIPELINE_COMMON_RESPONSES: &[(&str, &str)] = &[
    ("MODE READER", "200 Posting prohibited\r\n"),
    ("DATE", "111 20260508120000\r\n"),
];

fn large_body_response(status: &str) -> String {
    let mut response = String::from(status);
    for idx in 0..2048 {
        response.push_str(&format!("line-{idx:04} {}\r\n", "x".repeat(96)));
    }
    response.push_str(".\r\n");
    response
}

fn body_backend(_port: u16) -> MockNntpServer {
    MockNntpServer::new()
        .with_name("BodyBackend")
        .on_command(
            "BODY <present@example.com>",
            "222 0 <present@example.com>\r\nBody by message-id\r\n.\r\n",
        )
        .on_command("BODY missing", "412 No newsgroup selected\r\n")
        .on_command("GROUP alt.test", "211 3 1 3 alt.test\r\n")
        .on_command("BODY 999", "423 No article with that number\r\n")
        .on_command(
            "BODY 2",
            large_body_response("222 2 <present@example.com>\r\n"),
        )
        .on_command("LAST", "223 1 <first@example.com>\r\n")
}

#[tokio::test]
async fn test_body_by_message_id_returns_multiline_in_per_command_mode() -> Result<()> {
    let mut client =
        RfcTestClient::spawn(RoutingMode::PerCommand, "body-backend", body_backend).await?;

    let lines = client
        .expect_multiline("BODY <present@example.com>", "222")
        .await?;
    assert_eq!(lines, vec!["Body by message-id\r\n".to_string()]);

    Ok(())
}

#[tokio::test]
async fn test_body_by_number_streams_large_multiline_body_after_group() -> Result<()> {
    let mut client =
        RfcTestClient::spawn(RoutingMode::Hybrid, "body-backend", body_backend).await?;

    client.expect_status("GROUP alt.test", "211").await?;

    let lines = client.expect_multiline("BODY 2", "222 2 ").await?;
    assert_eq!(lines.len(), 2048, "Expected the full large BODY payload");
    let body = lines.concat();
    assert!(
        body.contains("line-0000") && body.contains("line-2047"),
        "Expected first and last streamed body lines to arrive intact"
    );
    assert!(
        body.len() > 200_000,
        "Expected a large streamed BODY payload, got {} bytes",
        body.len()
    );

    Ok(())
}

#[tokio::test]
async fn test_body_without_group_returns_412_in_stateful_mode() -> Result<()> {
    let mut client =
        RfcTestClient::spawn(RoutingMode::Stateful, "body-backend", body_backend).await?;

    client.expect_status("BODY missing", "412").await?;
    client.expect_status("GROUP alt.test", "211").await?;

    Ok(())
}

#[tokio::test]
async fn test_body_missing_article_number_returns_423_and_session_continues() -> Result<()> {
    let mut client =
        RfcTestClient::spawn(RoutingMode::Hybrid, "body-backend", body_backend).await?;

    client.expect_status("GROUP alt.test", "211").await?;
    client.expect_status("BODY 999", "423").await?;
    client.expect_status("LAST", "223").await?;

    Ok(())
}

async fn read_line<R>(reader: &mut R, context: &str) -> Result<String>
where
    R: AsyncBufRead + Unpin,
{
    read_line_with_timeout(reader, context, Duration::from_secs(2)).await
}

async fn read_line_with_timeout<R>(
    reader: &mut R,
    context: &str,
    timeout_duration: Duration,
) -> Result<String>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = String::new();
    match timeout(timeout_duration, reader.read_line(&mut line)).await {
        Ok(Ok(0)) => anyhow::bail!("Connection closed while reading {context}"),
        Ok(Ok(_)) => Ok(line),
        Ok(Err(err)) => Err(err.into()),
        Err(_) => anyhow::bail!("Timed out while reading {context}"),
    }
}

async fn read_multiline_response<R>(reader: &mut R) -> Result<(String, Vec<String>)>
where
    R: AsyncBufRead + Unpin,
{
    let status_line = read_line(reader, "BODY status line").await?;
    let mut lines = Vec::new();

    loop {
        let line = read_line(reader, "BODY response line").await?;
        if line == ".\r\n" {
            break;
        }
        lines.push(line);
    }

    Ok((status_line, lines))
}

async fn read_multiline_responses<R>(
    reader: &mut R,
    count: usize,
) -> Result<Vec<(String, Vec<String>)>>
where
    R: AsyncBufRead + Unpin,
{
    let mut responses = Vec::with_capacity(count);
    for _ in 0..count {
        responses.push(read_multiline_response(reader).await?);
    }
    Ok(responses)
}

async fn write_commands<W>(stream: &mut W, commands: &[&str]) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    for command in commands {
        stream.write_all(command.as_bytes()).await?;
    }
    stream.flush().await?;
    Ok(())
}

fn pipeline_backend_config(backend_port: u16, name: &str) -> Config {
    pipeline_backend_config_with_max_connections(backend_port, name, 1)
}

fn pipeline_backend_config_with_max_connections(
    backend_port: u16,
    name: &str,
    max_connections: usize,
) -> Config {
    let server = create_test_server_config_with_max_connections(
        "127.0.0.1",
        backend_port,
        name,
        max_connections,
    );
    Config {
        servers: vec![server],
        ..Default::default()
    }
}

fn pipeline_backend_config_with_cache(
    backend_port: u16,
    name: &str,
    max_connections: usize,
) -> Config {
    let mut config =
        pipeline_backend_config_with_max_connections(backend_port, name, max_connections);
    config.cache = Some(Cache {
        store_article_bodies: true,
        ..Default::default()
    });
    config
}

fn find_pipeline_response<'a>(command: &str, responses: &'a [(&str, &str)]) -> Option<&'a str> {
    responses
        .iter()
        .find_map(|(expected, response)| (*expected == command).then_some(*response))
}

fn spawn_delayed_pipeline_backend(
    backend_listener: TcpListener,
    delayed_responses: &'static [(&'static str, &'static str)],
    immediate_responses: &'static [(&'static str, &'static str)],
) -> (Arc<AtomicUsize>, Arc<tokio::sync::Mutex<Vec<String>>>) {
    let connection_count = Arc::new(AtomicUsize::new(0));
    let seen_commands = Arc::new(tokio::sync::Mutex::new(Vec::<String>::new()));
    let connection_count_for_task = connection_count.clone();
    let seen_commands_for_task = seen_commands.clone();

    tokio::spawn(async move {
        while let Ok((mut stream, _)) = backend_listener.accept().await {
            connection_count_for_task.fetch_add(1, Ordering::SeqCst);
            if stream
                .write_all(b"200 BodyPipeline Ready\r\n")
                .await
                .is_err()
            {
                continue;
            }

            let mut pending = Vec::new();
            let mut delayed = Vec::new();
            let mut buffer = [0u8; 1024];
            while let Ok(n) = stream.read(&mut buffer).await {
                if n == 0 {
                    break;
                }
                pending.extend_from_slice(&buffer[..n]);

                while let Some(line_end) = pending.windows(2).position(|w| w == b"\r\n") {
                    let line = pending.drain(..line_end + 2).collect::<Vec<_>>();
                    let command = String::from_utf8_lossy(&line).trim().to_string();
                    seen_commands_for_task.lock().await.push(command.clone());

                    if let Some(response) = find_pipeline_response(&command, delayed_responses) {
                        delayed.push(response);
                        if delayed.len() == delayed_responses.len() {
                            for response in delayed.drain(..) {
                                if stream.write_all(response.as_bytes()).await.is_err() {
                                    break;
                                }
                            }
                        }
                        continue;
                    }

                    let response = find_pipeline_response(&command, immediate_responses)
                        .or_else(|| find_pipeline_response(&command, PIPELINE_COMMON_RESPONSES))
                        .unwrap_or("500 Unexpected command\r\n");
                    if stream.write_all(response.as_bytes()).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    (connection_count, seen_commands)
}

fn spawn_gated_multiline_backend(
    backend_listener: TcpListener,
    command: &'static str,
    initial_response: &'static str,
    trailing_response: &'static str,
) -> (Arc<Notify>, oneshot::Sender<()>) {
    let initial_written = Arc::new(Notify::new());
    let initial_written_for_task = initial_written.clone();
    let (release_tx, release_rx) = oneshot::channel::<()>();

    tokio::spawn(async move {
        let mut release_rx = Some(release_rx);
        while let Ok((mut stream, _)) = backend_listener.accept().await {
            if stream
                .write_all(b"200 GatedPipeline Ready\r\n")
                .await
                .is_err()
            {
                continue;
            }

            let mut pending = Vec::new();
            let mut buffer = [0u8; 1024];
            while let Ok(n) = stream.read(&mut buffer).await {
                if n == 0 {
                    break;
                }
                pending.extend_from_slice(&buffer[..n]);

                while let Some(line_end) = pending.windows(2).position(|w| w == b"\r\n") {
                    let line = pending.drain(..line_end + 2).collect::<Vec<_>>();
                    let received = String::from_utf8_lossy(&line).trim().to_string();

                    if let Some(response) =
                        find_pipeline_response(&received, PIPELINE_COMMON_RESPONSES)
                    {
                        if stream.write_all(response.as_bytes()).await.is_err() {
                            break;
                        }
                        continue;
                    }

                    if received == command {
                        if stream.write_all(initial_response.as_bytes()).await.is_err() {
                            break;
                        }
                        if stream.flush().await.is_err() {
                            break;
                        }
                        initial_written_for_task.notify_waiters();

                        if let Some(rx) = release_rx.take() {
                            let _ = rx.await;
                        }

                        let _ = stream.write_all(trailing_response.as_bytes()).await;
                        let _ = stream.flush().await;
                        continue;
                    }

                    let _ = stream.write_all(b"500 Unexpected command\r\n").await;
                }
            }
        }
    });

    (initial_written, release_tx)
}

fn spawn_packed_leftover_direct_backend(backend_listener: TcpListener) -> Arc<AtomicUsize> {
    let connection_count = Arc::new(AtomicUsize::new(0));
    let connection_count_for_task = connection_count.clone();

    tokio::spawn(async move {
        while let Ok((mut stream, _)) = backend_listener.accept().await {
            let connection_index = connection_count_for_task.fetch_add(1, Ordering::SeqCst) + 1;

            if stream
                .write_all(b"200 PackedLeftoverDirect Ready\r\n")
                .await
                .is_err()
            {
                continue;
            }

            let mut pending = Vec::new();
            let mut buffer = [0u8; 1024];
            while let Ok(n) = stream.read(&mut buffer).await {
                if n == 0 {
                    break;
                }
                pending.extend_from_slice(&buffer[..n]);

                while let Some(line_end) = pending.windows(2).position(|w| w == b"\r\n") {
                    let line = pending.drain(..line_end + 2).collect::<Vec<_>>();
                    let command = String::from_utf8_lossy(&line).trim().to_string();

                    if let Some(response) =
                        find_pipeline_response(&command, PIPELINE_COMMON_RESPONSES)
                    {
                        if stream.write_all(response.as_bytes()).await.is_err() {
                            break;
                        }
                        continue;
                    }

                    let response = match (connection_index, command.as_str()) {
                        (1, "BODY <packed-leftover@example>") => {
                            "222 0 <packed-leftover@example>\r\nbody-line\r\n.\r\n430 No such article\r\n"
                        }
                        (2, "STAT <fresh@example>") => "223 0 <fresh@example> status\r\n",
                        _ => "500 Unexpected command\r\n",
                    };
                    if stream.write_all(response.as_bytes()).await.is_err() {
                        break;
                    }
                    if stream.flush().await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    connection_count
}

fn spawn_reusable_direct_multiline_backend(backend_listener: TcpListener) -> Arc<AtomicUsize> {
    let connection_count = Arc::new(AtomicUsize::new(0));
    let connection_count_for_task = connection_count.clone();

    tokio::spawn(async move {
        while let Ok((mut stream, _)) = backend_listener.accept().await {
            connection_count_for_task.fetch_add(1, Ordering::SeqCst);

            if stream
                .write_all(b"200 ReusableDirectBody Ready\r\n")
                .await
                .is_err()
            {
                continue;
            }

            let mut pending = Vec::new();
            let mut buffer = [0u8; 1024];
            while let Ok(n) = stream.read(&mut buffer).await {
                if n == 0 {
                    break;
                }
                pending.extend_from_slice(&buffer[..n]);

                while let Some(line_end) = pending.windows(2).position(|w| w == b"\r\n") {
                    let line = pending.drain(..line_end + 2).collect::<Vec<_>>();
                    let command = String::from_utf8_lossy(&line).trim().to_string();

                    if let Some(response) =
                        find_pipeline_response(&command, PIPELINE_COMMON_RESPONSES)
                    {
                        if stream.write_all(response.as_bytes()).await.is_err() {
                            break;
                        }
                        continue;
                    }

                    let response = match command.as_str() {
                        "BODY <reuse-1@example>" => {
                            "222 1 <reuse-1@example>\r\nreuse-1-line\r\n.\r\n"
                        }
                        "BODY <reuse-2@example>" => {
                            "222 2 <reuse-2@example>\r\nreuse-2-line\r\n.\r\n"
                        }
                        "ARTICLE <reuse-article-1@example>" => {
                            "220 1 <reuse-article-1@example>\r\nreuse-article-1-line\r\n.\r\n"
                        }
                        "ARTICLE <reuse-article-2@example>" => {
                            "220 2 <reuse-article-2@example>\r\nreuse-article-2-line\r\n.\r\n"
                        }
                        _ => "500 Unexpected command\r\n",
                    };
                    if stream.write_all(response.as_bytes()).await.is_err() {
                        break;
                    }
                    if stream.flush().await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    connection_count
}

struct BufferedResponseExpectation<'a> {
    command: &'a str,
    status_context: &'a str,
    body_context: &'a str,
    expected_status: &'a str,
    expected_body_line: &'a str,
}

async fn expect_buffered_until_backend_finishes(
    proxy_port: u16,
    expectation: BufferedResponseExpectation<'_>,
    initial_written: Arc<Notify>,
    release_tx: oneshot::Sender<()>,
) -> Result<()> {
    let BufferedResponseExpectation {
        command,
        status_context,
        body_context,
        expected_status,
        expected_body_line,
    } = expectation;
    let client = connect_and_read_greeting(proxy_port).await?;
    let (read_half, mut write_half) = client.into_split();
    let mut reader = BufReader::new(read_half);

    write_commands(&mut write_half, &[command]).await?;
    initial_written.notified().await;

    let status_result =
        read_line_with_timeout(&mut reader, status_context, Duration::from_millis(200)).await;
    let body_line_result =
        read_line_with_timeout(&mut reader, body_context, Duration::from_millis(200)).await;

    assert!(
        status_result.is_err(),
        "{command:?} should stay buffered until the backend sends the terminator"
    );
    assert!(
        body_line_result.is_err(),
        "{command:?} body bytes should stay buffered until the backend finishes"
    );

    let _ = release_tx.send(());
    let status_line = read_line(&mut reader, status_context).await?;
    let body_line = read_line(&mut reader, body_context).await?;
    assert_eq!(status_line, expected_status);
    assert_eq!(body_line, expected_body_line);
    assert_eq!(
        read_line(&mut reader, "multiline terminator").await?,
        ".\r\n"
    );

    Ok(())
}

async fn expect_pass_through_borrows_initial_backend_bytes(
    proxy_port: u16,
    expectation: BufferedResponseExpectation<'_>,
    initial_written: Arc<Notify>,
    release_tx: oneshot::Sender<()>,
    body_line_in_initial_write: bool,
) -> Result<()> {
    let BufferedResponseExpectation {
        command,
        status_context,
        body_context,
        expected_status,
        expected_body_line,
    } = expectation;
    let client = connect_and_read_greeting(proxy_port).await?;
    let (read_half, mut write_half) = client.into_split();
    let mut reader = BufReader::new(read_half);

    write_commands(&mut write_half, &[command]).await?;
    initial_written.notified().await;

    assert_eq!(
        read_line(&mut reader, status_context).await?,
        expected_status,
        "{command:?} should forward already-framed status bytes without owning the full response"
    );

    if body_line_in_initial_write {
        assert_eq!(
            read_line(&mut reader, body_context).await?,
            expected_body_line,
            "{command:?} should forward already-framed body bytes without owning the full response"
        );
    } else {
        let body_line_result =
            read_line_with_timeout(&mut reader, body_context, Duration::from_millis(200)).await;
        assert!(
            body_line_result.is_err(),
            "{command:?} must wait for more backend bytes when the current borrowed buffer has no body line"
        );
    }

    let terminator_result = read_line_with_timeout(
        &mut reader,
        "multiline terminator before backend completion",
        Duration::from_millis(200),
    )
    .await;
    assert!(
        terminator_result.is_err(),
        "{command:?} must not synthesize completion before the backend sends the terminator"
    );

    let _ = release_tx.send(());
    if !body_line_in_initial_write {
        assert_eq!(
            read_line(&mut reader, body_context).await?,
            expected_body_line
        );
    }
    assert_eq!(
        read_line(&mut reader, "multiline terminator").await?,
        ".\r\n"
    );

    Ok(())
}

async fn expect_pass_through_borrows_initial_backend_bytes_with_auth(
    proxy_port: u16,
    expectation: BufferedResponseExpectation<'_>,
    initial_written: Arc<Notify>,
    release_tx: oneshot::Sender<()>,
    username: &str,
    password: &str,
    body_line_in_initial_write: bool,
) -> Result<()> {
    let BufferedResponseExpectation {
        command,
        status_context,
        body_context,
        expected_status,
        expected_body_line,
    } = expectation;
    let client = connect_and_read_greeting(proxy_port).await?;
    let (read_half, mut write_half) = client.into_split();
    let mut reader = BufReader::new(read_half);

    write_commands(
        &mut write_half,
        &[
            &format!("AUTHINFO USER {username}\r\n"),
            &format!("AUTHINFO PASS {password}\r\n"),
        ],
    )
    .await?;
    assert_eq!(
        read_line(&mut reader, "AUTHINFO USER status").await?,
        "381 Password required\r\n"
    );
    assert_eq!(
        read_line(&mut reader, "AUTHINFO PASS status").await?,
        "281 Authentication accepted\r\n"
    );

    write_commands(&mut write_half, &[command]).await?;
    initial_written.notified().await;

    assert_eq!(
        read_line(&mut reader, status_context).await?,
        expected_status
    );
    if body_line_in_initial_write {
        assert_eq!(
            read_line(&mut reader, body_context).await?,
            expected_body_line
        );
    } else {
        let body_line_result =
            read_line_with_timeout(&mut reader, body_context, Duration::from_millis(200)).await;
        assert!(
            body_line_result.is_err(),
            "{command:?} must wait for more backend bytes when the current borrowed buffer has no body line"
        );
    }

    let terminator_result = read_line_with_timeout(
        &mut reader,
        "multiline terminator before backend completion",
        Duration::from_millis(200),
    )
    .await;
    assert!(
        terminator_result.is_err(),
        "{command:?} must not synthesize completion before the backend sends the terminator"
    );

    let _ = release_tx.send(());
    if !body_line_in_initial_write {
        assert_eq!(
            read_line(&mut reader, body_context).await?,
            expected_body_line
        );
    }
    assert_eq!(
        read_line(&mut reader, "multiline terminator").await?,
        ".\r\n"
    );

    Ok(())
}

#[tokio::test]
async fn test_body_pipelining_pairs_four_responses_on_single_backend_connection() -> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let (connection_count, seen_commands) = spawn_delayed_pipeline_backend(
        backend_listener,
        &[],
        &[
            (
                "BODY <body-1@example>",
                "222 1 <body-1@example>\r\nbody-1-line\r\n.\r\n",
            ),
            (
                "BODY <body-2@example>",
                "222 2 <body-2@example>\r\nbody-2-line\r\n.\r\n",
            ),
            (
                "BODY <body-3@example>",
                "222 3 <body-3@example>\r\nbody-3-line\r\n.\r\n",
            ),
            (
                "BODY <body-4@example>",
                "222 4 <body-4@example>\r\nbody-4-line\r\n.\r\n",
            ),
        ],
    );

    let proxy_port = spawn_proxy_with_config(
        pipeline_backend_config(backend_port, "BodyPipeline"),
        RoutingMode::PerCommand,
    )
    .await?;
    let client = connect_and_read_greeting(proxy_port).await?;
    let (read_half, mut write_half) = client.into_split();
    let mut reader = BufReader::new(read_half);

    write_commands(
        &mut write_half,
        &[
            "BODY <body-1@example>\r\n",
            "BODY <body-2@example>\r\n",
            "BODY <body-3@example>\r\n",
            "BODY <body-4@example>\r\n",
        ],
    )
    .await?;

    let responses = read_multiline_responses(&mut reader, 4).await?;

    assert_eq!(
        responses,
        vec![
            (
                "222 1 <body-1@example>\r\n".to_string(),
                vec!["body-1-line\r\n".to_string()],
            ),
            (
                "222 2 <body-2@example>\r\n".to_string(),
                vec!["body-2-line\r\n".to_string()],
            ),
            (
                "222 3 <body-3@example>\r\n".to_string(),
                vec!["body-3-line\r\n".to_string()],
            ),
            (
                "222 4 <body-4@example>\r\n".to_string(),
                vec!["body-4-line\r\n".to_string()],
            ),
        ]
    );

    let seen_commands = seen_commands.lock().await.clone();
    let body_commands = seen_commands
        .into_iter()
        .filter(|command| command.starts_with("BODY "))
        .collect::<Vec<_>>();

    assert_eq!(
        body_commands,
        vec![
            "BODY <body-1@example>".to_string(),
            "BODY <body-2@example>".to_string(),
            "BODY <body-3@example>".to_string(),
            "BODY <body-4@example>".to_string(),
        ]
    );
    assert_eq!(
        connection_count.load(Ordering::SeqCst),
        1,
        "expected all pipelined BODY commands to share one backend connection"
    );

    Ok(())
}

#[tokio::test]
async fn test_article_pipelining_pairs_four_responses_on_single_backend_connection() -> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let (connection_count, seen_commands) = spawn_delayed_pipeline_backend(
        backend_listener,
        &[],
        &[
            (
                "ARTICLE <article-1@example>",
                "220 1 <article-1@example>\r\narticle-1-line\r\n.\r\n",
            ),
            (
                "ARTICLE <article-2@example>",
                "220 2 <article-2@example>\r\narticle-2-line\r\n.\r\n",
            ),
            (
                "ARTICLE <article-3@example>",
                "220 3 <article-3@example>\r\narticle-3-line\r\n.\r\n",
            ),
            (
                "ARTICLE <article-4@example>",
                "220 4 <article-4@example>\r\narticle-4-line\r\n.\r\n",
            ),
        ],
    );

    let proxy_port = spawn_proxy_with_config(
        pipeline_backend_config(backend_port, "ArticlePipeline"),
        RoutingMode::PerCommand,
    )
    .await?;
    let client = connect_and_read_greeting(proxy_port).await?;
    let (read_half, mut write_half) = client.into_split();
    let mut reader = BufReader::new(read_half);

    write_commands(
        &mut write_half,
        &[
            "ARTICLE <article-1@example>\r\n",
            "ARTICLE <article-2@example>\r\n",
            "ARTICLE <article-3@example>\r\n",
            "ARTICLE <article-4@example>\r\n",
        ],
    )
    .await?;

    let responses = read_multiline_responses(&mut reader, 4).await?;
    assert_eq!(
        responses,
        vec![
            (
                "220 1 <article-1@example>\r\n".to_string(),
                vec!["article-1-line\r\n".to_string()],
            ),
            (
                "220 2 <article-2@example>\r\n".to_string(),
                vec!["article-2-line\r\n".to_string()],
            ),
            (
                "220 3 <article-3@example>\r\n".to_string(),
                vec!["article-3-line\r\n".to_string()],
            ),
            (
                "220 4 <article-4@example>\r\n".to_string(),
                vec!["article-4-line\r\n".to_string()],
            ),
        ]
    );

    let seen_commands = seen_commands.lock().await.clone();
    let article_commands = seen_commands
        .into_iter()
        .filter(|command| command.starts_with("ARTICLE "))
        .collect::<Vec<_>>();
    assert_eq!(
        article_commands,
        vec![
            "ARTICLE <article-1@example>".to_string(),
            "ARTICLE <article-2@example>".to_string(),
            "ARTICLE <article-3@example>".to_string(),
            "ARTICLE <article-4@example>".to_string(),
        ]
    );
    assert_eq!(
        connection_count.load(Ordering::SeqCst),
        1,
        "expected all pipelined ARTICLE commands to share one backend connection"
    );

    Ok(())
}

#[tokio::test]
async fn test_body_pipeline_batch_drains_before_trailing_group_switch() -> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let (_connection_count, seen_commands) = spawn_delayed_pipeline_backend(
        backend_listener,
        &[],
        &[
            (
                "BODY <body-1@example>",
                "222 1 <body-1@example>\r\nbody-1-line\r\n.\r\n",
            ),
            (
                "BODY <body-2@example>",
                "222 2 <body-2@example>\r\nbody-2-line\r\n.\r\n",
            ),
            (
                "BODY <body-3@example>",
                "222 3 <body-3@example>\r\nbody-3-line\r\n.\r\n",
            ),
            (
                "BODY <body-4@example>",
                "222 4 <body-4@example>\r\nbody-4-line\r\n.\r\n",
            ),
            ("GROUP alt.test", "211 4 1 4 alt.test\r\n"),
        ],
    );

    let proxy_port = spawn_proxy_with_config(
        pipeline_backend_config(backend_port, "BodyPipelineTrailingGroup"),
        RoutingMode::Hybrid,
    )
    .await?;
    let client = connect_and_read_greeting(proxy_port).await?;
    let (read_half, mut write_half) = client.into_split();
    let mut reader = BufReader::new(read_half);

    write_commands(
        &mut write_half,
        &[
            "BODY <body-1@example>\r\n",
            "BODY <body-2@example>\r\n",
            "BODY <body-3@example>\r\n",
            "BODY <body-4@example>\r\n",
            "GROUP alt.test\r\n",
        ],
    )
    .await?;

    let responses = read_multiline_responses(&mut reader, 4).await?;
    assert_eq!(
        responses,
        vec![
            (
                "222 1 <body-1@example>\r\n".to_string(),
                vec!["body-1-line\r\n".to_string()],
            ),
            (
                "222 2 <body-2@example>\r\n".to_string(),
                vec!["body-2-line\r\n".to_string()],
            ),
            (
                "222 3 <body-3@example>\r\n".to_string(),
                vec!["body-3-line\r\n".to_string()],
            ),
            (
                "222 4 <body-4@example>\r\n".to_string(),
                vec!["body-4-line\r\n".to_string()],
            ),
        ]
    );
    assert_eq!(
        read_line(&mut reader, "GROUP status line").await?,
        "211 4 1 4 alt.test\r\n"
    );

    let seen_commands = seen_commands.lock().await.clone();
    let relevant_commands = seen_commands
        .into_iter()
        .filter(|command| command.starts_with("BODY ") || command.starts_with("GROUP "))
        .collect::<Vec<_>>();
    assert_eq!(
        relevant_commands,
        vec![
            "BODY <body-1@example>".to_string(),
            "BODY <body-2@example>".to_string(),
            "BODY <body-3@example>".to_string(),
            "BODY <body-4@example>".to_string(),
            "GROUP alt.test".to_string(),
        ],
        "expected trailing GROUP to be forwarded only after the queued BODY batch completed"
    );

    Ok(())
}

#[tokio::test]
async fn test_body_pipelining_reuses_healthy_backend_connection_across_batches() -> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let (connection_count, seen_commands) = spawn_delayed_pipeline_backend(
        backend_listener,
        &[],
        &[
            (
                "BODY <body-1@example>",
                "222 1 <body-1@example>\r\nbody-1-line\r\n.\r\n",
            ),
            (
                "BODY <body-2@example>",
                "222 2 <body-2@example>\r\nbody-2-line\r\n.\r\n",
            ),
            (
                "BODY <body-3@example>",
                "222 3 <body-3@example>\r\nbody-3-line\r\n.\r\n",
            ),
            (
                "BODY <body-4@example>",
                "222 4 <body-4@example>\r\nbody-4-line\r\n.\r\n",
            ),
        ],
    );

    let proxy_port = spawn_proxy_with_config(
        pipeline_backend_config(backend_port, "ReusableBodyPipeline"),
        RoutingMode::PerCommand,
    )
    .await?;
    let client = connect_and_read_greeting(proxy_port).await?;
    let (read_half, mut write_half) = client.into_split();
    let mut reader = BufReader::new(read_half);
    let commands = &[
        "BODY <body-1@example>\r\n",
        "BODY <body-2@example>\r\n",
        "BODY <body-3@example>\r\n",
        "BODY <body-4@example>\r\n",
    ];

    write_commands(&mut write_half, commands).await?;
    let first_batch = read_multiline_responses(&mut reader, 4).await?;

    write_commands(&mut write_half, commands).await?;
    let second_batch = match read_multiline_responses(&mut reader, 4).await {
        Ok(responses) => responses,
        Err(error) => {
            let seen_commands = seen_commands.lock().await.clone();
            anyhow::bail!(
                "second healthy BODY batch failed after backend commands {seen_commands:?} across {} connections: {error}",
                connection_count.load(Ordering::SeqCst),
            );
        }
    };

    let expected = vec![
        (
            "222 1 <body-1@example>\r\n".to_string(),
            vec!["body-1-line\r\n".to_string()],
        ),
        (
            "222 2 <body-2@example>\r\n".to_string(),
            vec!["body-2-line\r\n".to_string()],
        ),
        (
            "222 3 <body-3@example>\r\n".to_string(),
            vec!["body-3-line\r\n".to_string()],
        ),
        (
            "222 4 <body-4@example>\r\n".to_string(),
            vec!["body-4-line\r\n".to_string()],
        ),
    ];
    assert_eq!(first_batch, expected);
    assert_eq!(second_batch, expected);

    let seen_commands = seen_commands.lock().await.clone();
    let body_commands = seen_commands
        .into_iter()
        .filter(|command| command.starts_with("BODY "))
        .collect::<Vec<_>>();
    assert_eq!(
        body_commands,
        vec![
            "BODY <body-1@example>".to_string(),
            "BODY <body-2@example>".to_string(),
            "BODY <body-3@example>".to_string(),
            "BODY <body-4@example>".to_string(),
            "BODY <body-1@example>".to_string(),
            "BODY <body-2@example>".to_string(),
            "BODY <body-3@example>".to_string(),
            "BODY <body-4@example>".to_string(),
        ]
    );
    assert_eq!(
        connection_count.load(Ordering::SeqCst),
        1,
        "expected healthy queued BODY batches to keep reusing the same backend connection"
    );

    Ok(())
}

#[tokio::test]
async fn test_body_second_burst_drains_before_trailing_group_switch() -> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let (connection_count, seen_commands) = spawn_delayed_pipeline_backend(
        backend_listener,
        &[],
        &[
            (
                "BODY <body-1@example>",
                "222 1 <body-1@example>\r\nbody-1-line\r\n.\r\n",
            ),
            (
                "BODY <body-2@example>",
                "222 2 <body-2@example>\r\nbody-2-line\r\n.\r\n",
            ),
            (
                "BODY <body-3@example>",
                "222 3 <body-3@example>\r\nbody-3-line\r\n.\r\n",
            ),
            (
                "BODY <body-4@example>",
                "222 4 <body-4@example>\r\nbody-4-line\r\n.\r\n",
            ),
            ("GROUP alt.test", "211 4 1 4 alt.test\r\n"),
        ],
    );

    let proxy_port = spawn_proxy_with_config(
        pipeline_backend_config(backend_port, "BodySecondBurstTrailingGroup"),
        RoutingMode::Hybrid,
    )
    .await?;
    let client = connect_and_read_greeting(proxy_port).await?;
    let (read_half, mut write_half) = client.into_split();
    let mut reader = BufReader::new(read_half);

    let commands = &[
        "BODY <body-1@example>\r\n",
        "BODY <body-2@example>\r\n",
        "BODY <body-3@example>\r\n",
        "BODY <body-4@example>\r\n",
    ];

    write_commands(&mut write_half, commands).await?;
    let first_batch = read_multiline_responses(&mut reader, 4).await?;

    write_commands(
        &mut write_half,
        &[
            "BODY <body-1@example>\r\n",
            "BODY <body-2@example>\r\n",
            "BODY <body-3@example>\r\n",
            "BODY <body-4@example>\r\n",
            "GROUP alt.test\r\n",
        ],
    )
    .await?;
    let second_batch = read_multiline_responses(&mut reader, 4).await?;
    assert_eq!(
        read_line(&mut reader, "second-burst GROUP status line").await?,
        "211 4 1 4 alt.test\r\n"
    );

    let expected = vec![
        (
            "222 1 <body-1@example>\r\n".to_string(),
            vec!["body-1-line\r\n".to_string()],
        ),
        (
            "222 2 <body-2@example>\r\n".to_string(),
            vec!["body-2-line\r\n".to_string()],
        ),
        (
            "222 3 <body-3@example>\r\n".to_string(),
            vec!["body-3-line\r\n".to_string()],
        ),
        (
            "222 4 <body-4@example>\r\n".to_string(),
            vec!["body-4-line\r\n".to_string()],
        ),
    ];
    assert_eq!(first_batch, expected);
    assert_eq!(second_batch, expected);

    let seen_commands = seen_commands.lock().await.clone();
    let relevant_commands = seen_commands
        .into_iter()
        .filter(|command| command.starts_with("BODY ") || command.starts_with("GROUP "))
        .collect::<Vec<_>>();
    assert_eq!(
        relevant_commands,
        vec![
            "BODY <body-1@example>".to_string(),
            "BODY <body-2@example>".to_string(),
            "BODY <body-3@example>".to_string(),
            "BODY <body-4@example>".to_string(),
            "BODY <body-1@example>".to_string(),
            "BODY <body-2@example>".to_string(),
            "BODY <body-3@example>".to_string(),
            "BODY <body-4@example>".to_string(),
            "GROUP alt.test".to_string(),
        ],
        "expected trailing GROUP on the second burst only after the queued BODY batch completed"
    );
    assert_eq!(
        connection_count.load(Ordering::SeqCst),
        1,
        "expected healthy second-burst BODY traffic to keep reusing the same backend connection"
    );

    Ok(())
}

#[tokio::test]
async fn test_hybrid_partial_second_body_burst_resumes_before_trailing_group_switch() -> Result<()>
{
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let (connection_count, seen_commands) = spawn_delayed_pipeline_backend(
        backend_listener,
        &[],
        &[
            (
                "BODY <body-1@example>",
                "222 1 <body-1@example>\r\nbody-1-line\r\n.\r\n",
            ),
            (
                "BODY <body-2@example>",
                "222 2 <body-2@example>\r\nbody-2-line\r\n.\r\n",
            ),
            (
                "BODY <body-3@example>",
                "222 3 <body-3@example>\r\nbody-3-line\r\n.\r\n",
            ),
            (
                "BODY <body-4@example>",
                "222 4 <body-4@example>\r\nbody-4-line\r\n.\r\n",
            ),
            (
                "BODY <body-5@example>",
                "222 5 <body-5@example>\r\nbody-5-line\r\n.\r\n",
            ),
            ("GROUP alt.test", "211 5 1 5 alt.test\r\n"),
        ],
    );

    let proxy_port = spawn_proxy_with_config(
        pipeline_backend_config(backend_port, "HybridPartialSecondBurst"),
        RoutingMode::Hybrid,
    )
    .await?;
    let client = connect_and_read_greeting(proxy_port).await?;
    let (read_half, mut write_half) = client.into_split();
    let mut reader = BufReader::new(read_half);

    write_commands(
        &mut write_half,
        &[
            "BODY <body-1@example>\r\n",
            "BODY <body-2@example>\r\n",
            "BODY <body-3@example>\r\n",
            "BODY <body-4@example>\r\n",
            "BODY <body-5@examp",
        ],
    )
    .await?;
    let first_batch = read_multiline_responses(&mut reader, 4).await?;

    write_commands(&mut write_half, &["le>\r\n", "GROUP alt.test\r\n"]).await?;
    let second_response = read_multiline_responses(&mut reader, 1).await?;
    assert_eq!(
        read_line(&mut reader, "partial-second-burst GROUP status line").await?,
        "211 5 1 5 alt.test\r\n"
    );

    let expected_first_batch = vec![
        (
            "222 1 <body-1@example>\r\n".to_string(),
            vec!["body-1-line\r\n".to_string()],
        ),
        (
            "222 2 <body-2@example>\r\n".to_string(),
            vec!["body-2-line\r\n".to_string()],
        ),
        (
            "222 3 <body-3@example>\r\n".to_string(),
            vec!["body-3-line\r\n".to_string()],
        ),
        (
            "222 4 <body-4@example>\r\n".to_string(),
            vec!["body-4-line\r\n".to_string()],
        ),
    ];
    assert_eq!(first_batch, expected_first_batch);
    assert_eq!(
        second_response,
        vec![(
            "222 5 <body-5@example>\r\n".to_string(),
            vec!["body-5-line\r\n".to_string()],
        )]
    );

    let seen_commands = seen_commands.lock().await.clone();
    let relevant_commands = seen_commands
        .into_iter()
        .filter(|command| command.starts_with("BODY ") || command.starts_with("GROUP "))
        .collect::<Vec<_>>();
    assert_eq!(
        relevant_commands,
        vec![
            "BODY <body-1@example>".to_string(),
            "BODY <body-2@example>".to_string(),
            "BODY <body-3@example>".to_string(),
            "BODY <body-4@example>".to_string(),
            "BODY <body-5@example>".to_string(),
            "GROUP alt.test".to_string(),
        ],
        "expected the proxy to resume the partial second-burst BODY before forwarding trailing GROUP"
    );
    assert_eq!(
        connection_count.load(Ordering::SeqCst),
        1,
        "expected the resumed partial burst and trailing GROUP to stay on the same backend connection"
    );

    Ok(())
}

#[tokio::test]
async fn test_article_pipelining_reuses_healthy_backend_connection_across_batches() -> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let (connection_count, seen_commands) = spawn_delayed_pipeline_backend(
        backend_listener,
        &[],
        &[
            (
                "ARTICLE <article-1@example>",
                "220 1 <article-1@example>\r\narticle-1-line\r\n.\r\n",
            ),
            (
                "ARTICLE <article-2@example>",
                "220 2 <article-2@example>\r\narticle-2-line\r\n.\r\n",
            ),
            (
                "ARTICLE <article-3@example>",
                "220 3 <article-3@example>\r\narticle-3-line\r\n.\r\n",
            ),
            (
                "ARTICLE <article-4@example>",
                "220 4 <article-4@example>\r\narticle-4-line\r\n.\r\n",
            ),
        ],
    );

    let proxy_port = spawn_proxy_with_config(
        pipeline_backend_config(backend_port, "ReusableArticlePipeline"),
        RoutingMode::PerCommand,
    )
    .await?;
    let client = connect_and_read_greeting(proxy_port).await?;
    let (read_half, mut write_half) = client.into_split();
    let mut reader = BufReader::new(read_half);
    let commands = &[
        "ARTICLE <article-1@example>\r\n",
        "ARTICLE <article-2@example>\r\n",
        "ARTICLE <article-3@example>\r\n",
        "ARTICLE <article-4@example>\r\n",
    ];

    write_commands(&mut write_half, commands).await?;
    let first_batch = read_multiline_responses(&mut reader, 4).await?;

    write_commands(&mut write_half, commands).await?;
    let second_batch = read_multiline_responses(&mut reader, 4).await?;

    let expected = vec![
        (
            "220 1 <article-1@example>\r\n".to_string(),
            vec!["article-1-line\r\n".to_string()],
        ),
        (
            "220 2 <article-2@example>\r\n".to_string(),
            vec!["article-2-line\r\n".to_string()],
        ),
        (
            "220 3 <article-3@example>\r\n".to_string(),
            vec!["article-3-line\r\n".to_string()],
        ),
        (
            "220 4 <article-4@example>\r\n".to_string(),
            vec!["article-4-line\r\n".to_string()],
        ),
    ];
    assert_eq!(first_batch, expected);
    assert_eq!(second_batch, expected);

    let seen_commands = seen_commands.lock().await.clone();
    let article_commands = seen_commands
        .into_iter()
        .filter(|command| command.starts_with("ARTICLE "))
        .collect::<Vec<_>>();
    assert_eq!(
        article_commands,
        vec![
            "ARTICLE <article-1@example>".to_string(),
            "ARTICLE <article-2@example>".to_string(),
            "ARTICLE <article-3@example>".to_string(),
            "ARTICLE <article-4@example>".to_string(),
            "ARTICLE <article-1@example>".to_string(),
            "ARTICLE <article-2@example>".to_string(),
            "ARTICLE <article-3@example>".to_string(),
            "ARTICLE <article-4@example>".to_string(),
        ]
    );
    assert_eq!(
        connection_count.load(Ordering::SeqCst),
        1,
        "expected healthy queued ARTICLE batches to keep reusing the same backend connection"
    );

    Ok(())
}

#[tokio::test]
async fn test_article_second_burst_drains_before_trailing_group_switch() -> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let (connection_count, seen_commands) = spawn_delayed_pipeline_backend(
        backend_listener,
        &[],
        &[
            (
                "ARTICLE <article-1@example>",
                "220 1 <article-1@example>\r\narticle-1-line\r\n.\r\n",
            ),
            (
                "ARTICLE <article-2@example>",
                "220 2 <article-2@example>\r\narticle-2-line\r\n.\r\n",
            ),
            (
                "ARTICLE <article-3@example>",
                "220 3 <article-3@example>\r\narticle-3-line\r\n.\r\n",
            ),
            (
                "ARTICLE <article-4@example>",
                "220 4 <article-4@example>\r\narticle-4-line\r\n.\r\n",
            ),
            ("GROUP alt.test", "211 4 1 4 alt.test\r\n"),
        ],
    );

    let proxy_port = spawn_proxy_with_config(
        pipeline_backend_config(backend_port, "ArticleSecondBurstTrailingGroup"),
        RoutingMode::Hybrid,
    )
    .await?;
    let client = connect_and_read_greeting(proxy_port).await?;
    let (read_half, mut write_half) = client.into_split();
    let mut reader = BufReader::new(read_half);

    let commands = &[
        "ARTICLE <article-1@example>\r\n",
        "ARTICLE <article-2@example>\r\n",
        "ARTICLE <article-3@example>\r\n",
        "ARTICLE <article-4@example>\r\n",
    ];

    write_commands(&mut write_half, commands).await?;
    let first_batch = read_multiline_responses(&mut reader, 4).await?;

    write_commands(
        &mut write_half,
        &[
            "ARTICLE <article-1@example>\r\n",
            "ARTICLE <article-2@example>\r\n",
            "ARTICLE <article-3@example>\r\n",
            "ARTICLE <article-4@example>\r\n",
            "GROUP alt.test\r\n",
        ],
    )
    .await?;
    let second_batch = read_multiline_responses(&mut reader, 4).await?;
    assert_eq!(
        read_line(&mut reader, "second-burst GROUP status line").await?,
        "211 4 1 4 alt.test\r\n"
    );

    let expected = vec![
        (
            "220 1 <article-1@example>\r\n".to_string(),
            vec!["article-1-line\r\n".to_string()],
        ),
        (
            "220 2 <article-2@example>\r\n".to_string(),
            vec!["article-2-line\r\n".to_string()],
        ),
        (
            "220 3 <article-3@example>\r\n".to_string(),
            vec!["article-3-line\r\n".to_string()],
        ),
        (
            "220 4 <article-4@example>\r\n".to_string(),
            vec!["article-4-line\r\n".to_string()],
        ),
    ];
    assert_eq!(first_batch, expected);
    assert_eq!(second_batch, expected);

    let seen_commands = seen_commands.lock().await.clone();
    let relevant_commands = seen_commands
        .into_iter()
        .filter(|command| command.starts_with("ARTICLE ") || command.starts_with("GROUP "))
        .collect::<Vec<_>>();
    assert_eq!(
        relevant_commands,
        vec![
            "ARTICLE <article-1@example>".to_string(),
            "ARTICLE <article-2@example>".to_string(),
            "ARTICLE <article-3@example>".to_string(),
            "ARTICLE <article-4@example>".to_string(),
            "ARTICLE <article-1@example>".to_string(),
            "ARTICLE <article-2@example>".to_string(),
            "ARTICLE <article-3@example>".to_string(),
            "ARTICLE <article-4@example>".to_string(),
            "GROUP alt.test".to_string(),
        ],
        "expected trailing GROUP on the second ARTICLE burst only after the queued batch completed"
    );
    assert_eq!(
        connection_count.load(Ordering::SeqCst),
        1,
        "expected healthy second-burst ARTICLE traffic to keep reusing the same backend connection"
    );

    Ok(())
}

#[tokio::test]
async fn test_cached_body_pipeline_buffers_until_backend_finishes() -> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let (initial_written, release_tx) = spawn_gated_multiline_backend(
        backend_listener,
        "BODY <cached-body@example>",
        "222 0 <cached-body@example>\r\nbody-prefix\r\n",
        ".\r\n",
    );

    let proxy_port = spawn_proxy_with_config(
        pipeline_backend_config_with_cache(backend_port, "CachedBodyPipeline", 2),
        RoutingMode::PerCommand,
    )
    .await?;
    expect_buffered_until_backend_finishes(
        proxy_port,
        BufferedResponseExpectation {
            command: "BODY <cached-body@example>\r\n",
            status_context: "cached BODY status",
            body_context: "cached BODY first line",
            expected_status: "222 0 <cached-body@example>\r\n",
            expected_body_line: "body-prefix\r\n",
        },
        initial_written,
        release_tx,
    )
    .await
}

#[tokio::test]
async fn test_cached_article_pipeline_buffers_until_backend_finishes() -> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let (initial_written, release_tx) = spawn_gated_multiline_backend(
        backend_listener,
        "ARTICLE <cached-article@example>",
        "220 0 <cached-article@example>\r\narticle-prefix\r\n",
        ".\r\n",
    );

    let proxy_port = spawn_proxy_with_config(
        pipeline_backend_config_with_cache(backend_port, "CachedArticlePipeline", 2),
        RoutingMode::PerCommand,
    )
    .await?;
    expect_buffered_until_backend_finishes(
        proxy_port,
        BufferedResponseExpectation {
            command: "ARTICLE <cached-article@example>\r\n",
            status_context: "cached ARTICLE status",
            body_context: "cached ARTICLE first line",
            expected_status: "220 0 <cached-article@example>\r\n",
            expected_body_line: "article-prefix\r\n",
        },
        initial_written,
        release_tx,
    )
    .await
}

#[tokio::test]
async fn test_cached_body_pipeline_buffers_when_first_read_only_has_status_line() -> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let (initial_written, release_tx) = spawn_gated_multiline_backend(
        backend_listener,
        "BODY <cached-status-only-body@example>",
        "222 0 <cached-status-only-body@example>\r\n",
        "cached-status-only-body-prefix\r\n.\r\n",
    );

    let proxy_port = spawn_proxy_with_config(
        pipeline_backend_config_with_cache(backend_port, "CachedStatusOnlyBodyPipeline", 2),
        RoutingMode::PerCommand,
    )
    .await?;
    expect_buffered_until_backend_finishes(
        proxy_port,
        BufferedResponseExpectation {
            command: "BODY <cached-status-only-body@example>\r\n",
            status_context: "cached status-only BODY status",
            body_context: "cached status-only BODY first line",
            expected_status: "222 0 <cached-status-only-body@example>\r\n",
            expected_body_line: "cached-status-only-body-prefix\r\n",
        },
        initial_written,
        release_tx,
    )
    .await
}

#[tokio::test]
async fn test_cached_article_pipeline_buffers_when_first_read_only_has_status_line() -> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let (initial_written, release_tx) = spawn_gated_multiline_backend(
        backend_listener,
        "ARTICLE <cached-status-only-article@example>",
        "220 0 <cached-status-only-article@example>\r\n",
        "cached-status-only-article-prefix\r\n.\r\n",
    );

    let proxy_port = spawn_proxy_with_config(
        pipeline_backend_config_with_cache(backend_port, "CachedStatusOnlyArticlePipeline", 2),
        RoutingMode::PerCommand,
    )
    .await?;
    expect_buffered_until_backend_finishes(
        proxy_port,
        BufferedResponseExpectation {
            command: "ARTICLE <cached-status-only-article@example>\r\n",
            status_context: "cached status-only ARTICLE status",
            body_context: "cached status-only ARTICLE first line",
            expected_status: "220 0 <cached-status-only-article@example>\r\n",
            expected_body_line: "cached-status-only-article-prefix\r\n",
        },
        initial_written,
        release_tx,
    )
    .await
}

#[tokio::test]
async fn test_per_command_body_pipeline_borrows_initial_bytes_without_cache() -> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let (initial_written, release_tx) = spawn_gated_multiline_backend(
        backend_listener,
        "BODY <plain-body@example>",
        "222 0 <plain-body@example>\r\nplain-body-prefix\r\n",
        ".\r\n",
    );

    let proxy_port = spawn_proxy_with_config(
        pipeline_backend_config(backend_port, "PlainBodyPipeline"),
        RoutingMode::PerCommand,
    )
    .await?;

    expect_pass_through_borrows_initial_backend_bytes(
        proxy_port,
        BufferedResponseExpectation {
            command: "BODY <plain-body@example>\r\n",
            status_context: "plain BODY status",
            body_context: "plain BODY first line",
            expected_status: "222 0 <plain-body@example>\r\n",
            expected_body_line: "plain-body-prefix\r\n",
        },
        initial_written,
        release_tx,
        true,
    )
    .await
}

#[tokio::test]
async fn test_per_command_body_pipeline_borrows_status_then_waits_for_body() -> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let (initial_written, release_tx) = spawn_gated_multiline_backend(
        backend_listener,
        "BODY <status-only-body@example>",
        "222 0 <status-only-body@example>\r\n",
        "status-only-body-prefix\r\n.\r\n",
    );

    let proxy_port = spawn_proxy_with_config(
        pipeline_backend_config(backend_port, "StatusOnlyBodyPipeline"),
        RoutingMode::PerCommand,
    )
    .await?;

    expect_pass_through_borrows_initial_backend_bytes(
        proxy_port,
        BufferedResponseExpectation {
            command: "BODY <status-only-body@example>\r\n",
            status_context: "status-only BODY status",
            body_context: "status-only BODY first line",
            expected_status: "222 0 <status-only-body@example>\r\n",
            expected_body_line: "status-only-body-prefix\r\n",
        },
        initial_written,
        release_tx,
        false,
    )
    .await
}

#[tokio::test]
async fn test_per_command_article_pipeline_borrows_initial_bytes_without_cache() -> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let (initial_written, release_tx) = spawn_gated_multiline_backend(
        backend_listener,
        "ARTICLE <plain-article@example>",
        "220 0 <plain-article@example>\r\nplain-article-prefix\r\n",
        ".\r\n",
    );

    let proxy_port = spawn_proxy_with_config(
        pipeline_backend_config(backend_port, "PlainArticlePipeline"),
        RoutingMode::PerCommand,
    )
    .await?;

    expect_pass_through_borrows_initial_backend_bytes(
        proxy_port,
        BufferedResponseExpectation {
            command: "ARTICLE <plain-article@example>\r\n",
            status_context: "plain ARTICLE status",
            body_context: "plain ARTICLE first line",
            expected_status: "220 0 <plain-article@example>\r\n",
            expected_body_line: "plain-article-prefix\r\n",
        },
        initial_written,
        release_tx,
        true,
    )
    .await
}

#[tokio::test]
async fn test_per_command_article_pipeline_borrows_status_then_waits_for_body() -> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let (initial_written, release_tx) = spawn_gated_multiline_backend(
        backend_listener,
        "ARTICLE <status-only-article@example>",
        "220 0 <status-only-article@example>\r\n",
        "status-only-article-prefix\r\n.\r\n",
    );

    let proxy_port = spawn_proxy_with_config(
        pipeline_backend_config(backend_port, "StatusOnlyArticlePipeline"),
        RoutingMode::PerCommand,
    )
    .await?;

    expect_pass_through_borrows_initial_backend_bytes(
        proxy_port,
        BufferedResponseExpectation {
            command: "ARTICLE <status-only-article@example>\r\n",
            status_context: "status-only ARTICLE status",
            body_context: "status-only ARTICLE first line",
            expected_status: "220 0 <status-only-article@example>\r\n",
            expected_body_line: "status-only-article-prefix\r\n",
        },
        initial_written,
        release_tx,
        false,
    )
    .await
}

#[tokio::test]
async fn test_hybrid_body_message_id_borrows_initial_bytes_before_stateful_switch() -> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let (initial_written, release_tx) = spawn_gated_multiline_backend(
        backend_listener,
        "BODY <hybrid-body@example>",
        "222 0 <hybrid-body@example>\r\nhybrid-body-prefix\r\n",
        ".\r\n",
    );

    let proxy_port = spawn_proxy_with_config(
        pipeline_backend_config(backend_port, "HybridBodyPipeline"),
        RoutingMode::Hybrid,
    )
    .await?;

    expect_pass_through_borrows_initial_backend_bytes(
        proxy_port,
        BufferedResponseExpectation {
            command: "BODY <hybrid-body@example>\r\n",
            status_context: "hybrid BODY status",
            body_context: "hybrid BODY first line",
            expected_status: "222 0 <hybrid-body@example>\r\n",
            expected_body_line: "hybrid-body-prefix\r\n",
        },
        initial_written,
        release_tx,
        true,
    )
    .await
}

#[tokio::test]
async fn test_hybrid_body_message_id_borrows_status_then_waits_for_body() -> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let (initial_written, release_tx) = spawn_gated_multiline_backend(
        backend_listener,
        "BODY <hybrid-status-only-body@example>",
        "222 0 <hybrid-status-only-body@example>\r\n",
        "hybrid-status-only-body-prefix\r\n.\r\n",
    );

    let proxy_port = spawn_proxy_with_config(
        pipeline_backend_config(backend_port, "HybridStatusOnlyBodyPipeline"),
        RoutingMode::Hybrid,
    )
    .await?;

    expect_pass_through_borrows_initial_backend_bytes(
        proxy_port,
        BufferedResponseExpectation {
            command: "BODY <hybrid-status-only-body@example>\r\n",
            status_context: "hybrid status-only BODY status",
            body_context: "hybrid status-only BODY first line",
            expected_status: "222 0 <hybrid-status-only-body@example>\r\n",
            expected_body_line: "hybrid-status-only-body-prefix\r\n",
        },
        initial_written,
        release_tx,
        false,
    )
    .await
}

#[tokio::test]
async fn test_hybrid_article_message_id_borrows_initial_bytes_before_stateful_switch() -> Result<()>
{
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let (initial_written, release_tx) = spawn_gated_multiline_backend(
        backend_listener,
        "ARTICLE <hybrid-article@example>",
        "220 0 <hybrid-article@example>\r\nhybrid-article-prefix\r\n",
        ".\r\n",
    );

    let proxy_port = spawn_proxy_with_config(
        pipeline_backend_config(backend_port, "HybridArticlePipeline"),
        RoutingMode::Hybrid,
    )
    .await?;

    expect_pass_through_borrows_initial_backend_bytes(
        proxy_port,
        BufferedResponseExpectation {
            command: "ARTICLE <hybrid-article@example>\r\n",
            status_context: "hybrid ARTICLE status",
            body_context: "hybrid ARTICLE first line",
            expected_status: "220 0 <hybrid-article@example>\r\n",
            expected_body_line: "hybrid-article-prefix\r\n",
        },
        initial_written,
        release_tx,
        true,
    )
    .await
}

#[tokio::test]
async fn test_hybrid_article_message_id_borrows_status_then_waits_for_body() -> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let (initial_written, release_tx) = spawn_gated_multiline_backend(
        backend_listener,
        "ARTICLE <hybrid-status-only-article@example>",
        "220 0 <hybrid-status-only-article@example>\r\n",
        "hybrid-status-only-article-prefix\r\n.\r\n",
    );

    let proxy_port = spawn_proxy_with_config(
        pipeline_backend_config(backend_port, "HybridStatusOnlyArticlePipeline"),
        RoutingMode::Hybrid,
    )
    .await?;

    expect_pass_through_borrows_initial_backend_bytes(
        proxy_port,
        BufferedResponseExpectation {
            command: "ARTICLE <hybrid-status-only-article@example>\r\n",
            status_context: "hybrid status-only ARTICLE status",
            body_context: "hybrid status-only ARTICLE first line",
            expected_status: "220 0 <hybrid-status-only-article@example>\r\n",
            expected_body_line: "hybrid-status-only-article-prefix\r\n",
        },
        initial_written,
        release_tx,
        false,
    )
    .await
}

#[tokio::test]
async fn test_per_command_direct_body_retires_connection_after_packed_stale_response() -> Result<()>
{
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let connection_count = spawn_packed_leftover_direct_backend(backend_listener);

    let proxy_port = spawn_proxy_with_config(
        pipeline_backend_config_with_max_connections(backend_port, "DirectPackedLeftover", 1),
        RoutingMode::PerCommand,
    )
    .await?;

    let client = connect_and_read_greeting(proxy_port).await?;
    let (read_half, mut write_half) = client.into_split();
    let mut reader = BufReader::new(read_half);

    write_commands(&mut write_half, &["BODY <packed-leftover@example>\r\n"]).await?;
    let (status_line, body_lines) = read_multiline_response(&mut reader).await?;
    assert_eq!(status_line, "222 0 <packed-leftover@example>\r\n");
    assert_eq!(body_lines, vec!["body-line\r\n".to_string()]);

    write_commands(&mut write_half, &["STAT <fresh@example>\r\n"]).await?;
    assert_eq!(
        read_line(&mut reader, "fresh STAT status").await?,
        "223 0 <fresh@example> status\r\n"
    );
    assert_eq!(
        connection_count.load(Ordering::SeqCst),
        2,
        "stale bytes packed after a direct BODY response must retire the backend connection before the next borrow"
    );

    Ok(())
}

#[tokio::test]
async fn test_per_command_body_pipeline_borrows_status_then_waits_for_body_with_client_auth()
-> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let (initial_written, release_tx) = spawn_gated_multiline_backend(
        backend_listener,
        "BODY <auth-status-only-body@example>",
        "222 0 <auth-status-only-body@example>\r\n",
        "auth-status-only-body-prefix\r\n.\r\n",
    );

    let mut config = pipeline_backend_config(backend_port, "AuthStatusOnlyBodyPipeline");
    config.client_auth = nntp_proxy::config::ClientAuth {
        users: vec![nntp_proxy::config::UserCredentials {
            username: "test-user".to_string(),
            password: "test-pass".to_string(),
        }],
        ..Default::default()
    };

    let proxy_port = spawn_proxy_with_config(config, RoutingMode::PerCommand).await?;
    expect_pass_through_borrows_initial_backend_bytes_with_auth(
        proxy_port,
        BufferedResponseExpectation {
            command: "BODY <auth-status-only-body@example>\r\n",
            status_context: "auth status-only BODY status",
            body_context: "auth status-only BODY first line",
            expected_status: "222 0 <auth-status-only-body@example>\r\n",
            expected_body_line: "auth-status-only-body-prefix\r\n",
        },
        initial_written,
        release_tx,
        "test-user",
        "test-pass",
        false,
    )
    .await
}

#[tokio::test]
async fn test_per_command_article_pipeline_borrows_status_then_waits_for_body_with_client_auth()
-> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let (initial_written, release_tx) = spawn_gated_multiline_backend(
        backend_listener,
        "ARTICLE <auth-status-only-article@example>",
        "220 0 <auth-status-only-article@example>\r\n",
        "auth-status-only-article-prefix\r\n.\r\n",
    );

    let mut config = pipeline_backend_config(backend_port, "AuthStatusOnlyArticlePipeline");
    config.client_auth = nntp_proxy::config::ClientAuth {
        users: vec![nntp_proxy::config::UserCredentials {
            username: "test-user".to_string(),
            password: "test-pass".to_string(),
        }],
        ..Default::default()
    };

    let proxy_port = spawn_proxy_with_config(config, RoutingMode::PerCommand).await?;
    expect_pass_through_borrows_initial_backend_bytes_with_auth(
        proxy_port,
        BufferedResponseExpectation {
            command: "ARTICLE <auth-status-only-article@example>\r\n",
            status_context: "auth status-only ARTICLE status",
            body_context: "auth status-only ARTICLE first line",
            expected_status: "220 0 <auth-status-only-article@example>\r\n",
            expected_body_line: "auth-status-only-article-prefix\r\n",
        },
        initial_written,
        release_tx,
        "test-user",
        "test-pass",
        false,
    )
    .await
}

#[tokio::test]
async fn test_per_command_direct_body_reuses_backend_connection_after_buffered_completion()
-> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let connection_count = spawn_reusable_direct_multiline_backend(backend_listener);

    let proxy_port = spawn_proxy_with_config(
        pipeline_backend_config_with_max_connections(backend_port, "ReusableDirectBody", 1),
        RoutingMode::PerCommand,
    )
    .await?;

    let client = connect_and_read_greeting(proxy_port).await?;
    let (read_half, mut write_half) = client.into_split();
    let mut reader = BufReader::new(read_half);

    write_commands(&mut write_half, &["BODY <reuse-1@example>\r\n"]).await?;
    let (status_line, body_lines) = read_multiline_response(&mut reader).await?;
    assert_eq!(status_line, "222 1 <reuse-1@example>\r\n");
    assert_eq!(body_lines, vec!["reuse-1-line\r\n".to_string()]);

    write_commands(&mut write_half, &["BODY <reuse-2@example>\r\n"]).await?;
    let (status_line, body_lines) = read_multiline_response(&mut reader).await?;
    assert_eq!(status_line, "222 2 <reuse-2@example>\r\n");
    assert_eq!(body_lines, vec!["reuse-2-line\r\n".to_string()]);

    assert_eq!(
        connection_count.load(Ordering::SeqCst),
        1,
        "buffered direct BODY completions should release a clean backend connection back to the pool"
    );

    Ok(())
}

#[tokio::test]
async fn test_per_command_direct_article_reuses_backend_connection_after_buffered_completion()
-> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let connection_count = spawn_reusable_direct_multiline_backend(backend_listener);

    let proxy_port = spawn_proxy_with_config(
        pipeline_backend_config_with_max_connections(backend_port, "ReusableDirectArticle", 1),
        RoutingMode::PerCommand,
    )
    .await?;

    let client = connect_and_read_greeting(proxy_port).await?;
    let (read_half, mut write_half) = client.into_split();
    let mut reader = BufReader::new(read_half);

    write_commands(&mut write_half, &["ARTICLE <reuse-article-1@example>\r\n"]).await?;
    let (status_line, body_lines) = read_multiline_response(&mut reader).await?;
    assert_eq!(status_line, "220 1 <reuse-article-1@example>\r\n");
    assert_eq!(body_lines, vec!["reuse-article-1-line\r\n".to_string()]);

    write_commands(&mut write_half, &["ARTICLE <reuse-article-2@example>\r\n"]).await?;
    let (status_line, body_lines) = read_multiline_response(&mut reader).await?;
    assert_eq!(status_line, "220 2 <reuse-article-2@example>\r\n");
    assert_eq!(body_lines, vec!["reuse-article-2-line\r\n".to_string()]);

    assert_eq!(
        connection_count.load(Ordering::SeqCst),
        1,
        "buffered direct ARTICLE completions should release a clean backend connection back to the pool"
    );

    Ok(())
}

#[tokio::test]
async fn test_per_command_direct_body_reuses_backend_connection_after_buffered_completion_with_client_auth()
-> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let connection_count = spawn_reusable_direct_multiline_backend(backend_listener);

    let mut config =
        pipeline_backend_config_with_max_connections(backend_port, "ReusableAuthDirectBody", 1);
    config.client_auth = nntp_proxy::config::ClientAuth {
        users: vec![nntp_proxy::config::UserCredentials {
            username: "test-user".to_string(),
            password: "test-pass".to_string(),
        }],
        ..Default::default()
    };

    let proxy_port = spawn_proxy_with_config(config, RoutingMode::PerCommand).await?;
    let client = connect_and_read_greeting(proxy_port).await?;
    let (read_half, mut write_half) = client.into_split();
    let mut reader = BufReader::new(read_half);

    write_commands(
        &mut write_half,
        &[
            "AUTHINFO USER test-user\r\n",
            "AUTHINFO PASS test-pass\r\n",
            "BODY <reuse-1@example>\r\n",
        ],
    )
    .await?;
    assert_eq!(
        read_line(&mut reader, "AUTHINFO USER status").await?,
        "381 Password required\r\n"
    );
    assert_eq!(
        read_line(&mut reader, "AUTHINFO PASS status").await?,
        "281 Authentication accepted\r\n"
    );
    let (status_line, body_lines) = read_multiline_response(&mut reader).await?;
    assert_eq!(status_line, "222 1 <reuse-1@example>\r\n");
    assert_eq!(body_lines, vec!["reuse-1-line\r\n".to_string()]);

    write_commands(&mut write_half, &["BODY <reuse-2@example>\r\n"]).await?;
    let (status_line, body_lines) = read_multiline_response(&mut reader).await?;
    assert_eq!(status_line, "222 2 <reuse-2@example>\r\n");
    assert_eq!(body_lines, vec!["reuse-2-line\r\n".to_string()]);

    assert_eq!(
        connection_count.load(Ordering::SeqCst),
        1,
        "authenticated buffered direct BODY completions should still reuse the same clean backend connection"
    );

    Ok(())
}
