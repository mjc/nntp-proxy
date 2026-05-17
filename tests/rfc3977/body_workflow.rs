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
use nntp_proxy::{
    Config, RoutingMode,
    config::{BackendSelectionStrategy, Cache},
};

type RecordedCommandBatches = Arc<tokio::sync::Mutex<Vec<Vec<String>>>>;

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

fn body_backend(port: u16) -> MockNntpServer {
    MockNntpServer::new(port)
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
    let mut server = create_test_server_config_with_max_connections(
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

fn multi_backend_pipeline_config(servers: Vec<nntp_proxy::config::Server>) -> Config {
    let mut config = Config {
        servers,
        ..Default::default()
    };
    config.routing.backend_selection = BackendSelectionStrategy::WeightedRoundRobin;
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

fn spawn_concurrent_pipeline_backend(
    backend_listener: TcpListener,
    delayed_responses: &'static [(&'static str, &'static str)],
    expected_connections: usize,
) -> (Arc<AtomicUsize>, RecordedCommandBatches) {
    let body_connection_count = Arc::new(AtomicUsize::new(0));
    let batches = Arc::new(tokio::sync::Mutex::new(Vec::<Vec<String>>::new()));
    let ready_connections = Arc::new(AtomicUsize::new(0));
    let release_responses = Arc::new(Notify::new());
    let body_connection_count_for_task = body_connection_count.clone();
    let batches_for_task = batches.clone();
    let ready_connections_for_task = ready_connections.clone();
    let release_responses_for_task = release_responses.clone();

    tokio::spawn(async move {
        while let Ok((mut stream, _)) = backend_listener.accept().await {
            let batches_for_connection = batches_for_task.clone();
            let ready_connections_for_connection = ready_connections_for_task.clone();
            let release_responses_for_connection = release_responses_for_task.clone();
            let body_connection_count_for_connection = body_connection_count_for_task.clone();

            tokio::spawn(async move {
                if stream
                    .write_all(b"200 ConcurrentBodyPipeline Ready\r\n")
                    .await
                    .is_err()
                {
                    return;
                }

                let mut pending = Vec::new();
                let mut delayed = Vec::new();
                let mut batch_commands = Vec::new();
                let mut buffer = [0u8; 1024];

                while let Ok(n) = stream.read(&mut buffer).await {
                    if n == 0 {
                        break;
                    }
                    pending.extend_from_slice(&buffer[..n]);

                    while let Some(line_end) = pending.windows(2).position(|w| w == b"\r\n") {
                        let line = pending.drain(..line_end + 2).collect::<Vec<_>>();
                        let command = String::from_utf8_lossy(&line).trim().to_string();

                        if let Some(response) = find_pipeline_response(&command, delayed_responses)
                        {
                            batch_commands.push(command);
                            delayed.push(response);

                            if delayed.len() == delayed_responses.len() / expected_connections {
                                batches_for_connection
                                    .lock()
                                    .await
                                    .push(batch_commands.clone());
                                body_connection_count_for_connection.fetch_add(1, Ordering::SeqCst);

                                let ready = ready_connections_for_connection
                                    .fetch_add(1, Ordering::SeqCst)
                                    + 1;
                                if ready.is_multiple_of(expected_connections) {
                                    release_responses_for_connection.notify_waiters();
                                } else {
                                    release_responses_for_connection.notified().await;
                                }

                                for response in delayed.drain(..) {
                                    if stream.write_all(response.as_bytes()).await.is_err() {
                                        break;
                                    }
                                }
                                batch_commands.clear();
                            }
                            continue;
                        }

                        let response = find_pipeline_response(&command, PIPELINE_COMMON_RESPONSES)
                            .unwrap_or("500 Unexpected command\r\n");
                        if stream.write_all(response.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                }
            });
        }
    });

    (body_connection_count, batches)
}

async fn run_multiline_pipeline_batch(
    proxy_port: u16,
    commands: &[&str],
) -> Result<Vec<(String, Vec<String>)>> {
    let client = connect_and_read_greeting(proxy_port).await?;
    let (read_half, mut write_half) = client.into_split();
    let mut reader = BufReader::new(read_half);

    write_commands(&mut write_half, commands).await?;
    read_multiline_responses(&mut reader, commands.len()).await
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

async fn expect_buffered_until_backend_finishes_with_auth(
    proxy_port: u16,
    expectation: BufferedResponseExpectation<'_>,
    initial_written: Arc<Notify>,
    release_tx: oneshot::Sender<()>,
    username: &str,
    password: &str,
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
        &[],
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
        &[("GROUP alt.test", "211 4 1 4 alt.test\r\n")],
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
async fn test_body_second_burst_batches_four_commands_on_reused_backend_connection() -> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let (body_connection_count, batches) = spawn_concurrent_pipeline_backend(
        backend_listener,
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
        1,
    );

    let proxy_port = spawn_proxy_with_config(
        pipeline_backend_config_with_max_connections(backend_port, "SecondBurstBodyBatching", 1),
        RoutingMode::PerCommand,
    )
    .await?;
    let commands = &[
        "BODY <body-1@example>\r\n",
        "BODY <body-2@example>\r\n",
        "BODY <body-3@example>\r\n",
        "BODY <body-4@example>\r\n",
    ];

    let first_batch = run_multiline_pipeline_batch(proxy_port, commands).await?;
    let second_batch = run_multiline_pipeline_batch(proxy_port, commands).await?;

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

    let recorded_batches = batches.lock().await.clone();
    assert_eq!(
        recorded_batches,
        vec![
            vec![
                "BODY <body-1@example>".to_string(),
                "BODY <body-2@example>".to_string(),
                "BODY <body-3@example>".to_string(),
                "BODY <body-4@example>".to_string(),
            ],
            vec![
                "BODY <body-1@example>".to_string(),
                "BODY <body-2@example>".to_string(),
                "BODY <body-3@example>".to_string(),
                "BODY <body-4@example>".to_string(),
            ],
        ],
        "expected the reused backend connection to receive a full four-command BODY batch on the second burst"
    );
    assert_eq!(
        body_connection_count.load(Ordering::SeqCst),
        2,
        "expected both bursts to reach the backend as full BODY pipeline batches"
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
async fn test_hybrid_body_second_burst_batches_four_commands_before_stateful_switch() -> Result<()>
{
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let (body_connection_count, batches) = spawn_concurrent_pipeline_backend(
        backend_listener,
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
        1,
    );

    let proxy_port = spawn_proxy_with_config(
        pipeline_backend_config_with_max_connections(
            backend_port,
            "HybridSecondBurstBodyBatching",
            1,
        ),
        RoutingMode::Hybrid,
    )
    .await?;
    let commands = &[
        "BODY <body-1@example>\r\n",
        "BODY <body-2@example>\r\n",
        "BODY <body-3@example>\r\n",
        "BODY <body-4@example>\r\n",
    ];

    let first_batch = run_multiline_pipeline_batch(proxy_port, commands).await?;
    let second_batch = run_multiline_pipeline_batch(proxy_port, commands).await?;

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

    let recorded_batches = batches.lock().await.clone();
    assert_eq!(
        recorded_batches,
        vec![
            vec![
                "BODY <body-1@example>".to_string(),
                "BODY <body-2@example>".to_string(),
                "BODY <body-3@example>".to_string(),
                "BODY <body-4@example>".to_string(),
            ],
            vec![
                "BODY <body-1@example>".to_string(),
                "BODY <body-2@example>".to_string(),
                "BODY <body-3@example>".to_string(),
                "BODY <body-4@example>".to_string(),
            ],
        ],
        "expected hybrid mode to keep batching message-ID BODY second bursts before any stateful switch"
    );
    assert_eq!(
        body_connection_count.load(Ordering::SeqCst),
        2,
        "expected both hybrid message-ID bursts to reach the backend as full BODY pipeline batches"
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
async fn test_per_command_body_pipeline_buffers_until_backend_finishes_without_cache() -> Result<()>
{
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

    expect_buffered_until_backend_finishes(
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
    )
    .await
}

#[tokio::test]
async fn test_per_command_body_pipeline_buffers_when_first_read_only_has_status_line() -> Result<()>
{
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

    expect_buffered_until_backend_finishes(
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
    )
    .await
}

#[tokio::test]
async fn test_per_command_article_pipeline_buffers_until_backend_finishes_without_cache()
-> Result<()> {
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

    expect_buffered_until_backend_finishes(
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
    )
    .await
}

#[tokio::test]
async fn test_per_command_article_pipeline_buffers_when_first_read_only_has_status_line()
-> Result<()> {
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

    expect_buffered_until_backend_finishes(
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
    )
    .await
}

#[tokio::test]
async fn test_hybrid_body_message_id_buffers_until_backend_finishes_before_stateful_switch()
-> Result<()> {
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

    expect_buffered_until_backend_finishes(
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
    )
    .await
}

#[tokio::test]
async fn test_hybrid_body_message_id_buffers_when_first_read_only_has_status_line() -> Result<()> {
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

    expect_buffered_until_backend_finishes(
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
    )
    .await
}

#[tokio::test]
async fn test_hybrid_article_message_id_buffers_until_backend_finishes_before_stateful_switch()
-> Result<()> {
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

    expect_buffered_until_backend_finishes(
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
    )
    .await
}

#[tokio::test]
async fn test_hybrid_article_message_id_buffers_when_first_read_only_has_status_line() -> Result<()>
{
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

    expect_buffered_until_backend_finishes(
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
async fn test_per_command_body_pipeline_buffers_when_first_read_only_has_status_line_with_client_auth()
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
    expect_buffered_until_backend_finishes_with_auth(
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
    )
    .await
}

#[tokio::test]
async fn test_per_command_article_pipeline_buffers_when_first_read_only_has_status_line_with_client_auth()
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
    expect_buffered_until_backend_finishes_with_auth(
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

#[tokio::test]
async fn test_body_pipelining_uses_multiple_backend_connections_for_concurrent_clients()
-> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let (body_connection_count, batches) = spawn_concurrent_pipeline_backend(
        backend_listener,
        &[
            (
                "BODY <client-a-1@example>",
                "222 1 <client-a-1@example>\r\nclient-a-1-line\r\n.\r\n",
            ),
            (
                "BODY <client-a-2@example>",
                "222 2 <client-a-2@example>\r\nclient-a-2-line\r\n.\r\n",
            ),
            (
                "BODY <client-a-3@example>",
                "222 3 <client-a-3@example>\r\nclient-a-3-line\r\n.\r\n",
            ),
            (
                "BODY <client-a-4@example>",
                "222 4 <client-a-4@example>\r\nclient-a-4-line\r\n.\r\n",
            ),
            (
                "BODY <client-b-1@example>",
                "222 1 <client-b-1@example>\r\nclient-b-1-line\r\n.\r\n",
            ),
            (
                "BODY <client-b-2@example>",
                "222 2 <client-b-2@example>\r\nclient-b-2-line\r\n.\r\n",
            ),
            (
                "BODY <client-b-3@example>",
                "222 3 <client-b-3@example>\r\nclient-b-3-line\r\n.\r\n",
            ),
            (
                "BODY <client-b-4@example>",
                "222 4 <client-b-4@example>\r\nclient-b-4-line\r\n.\r\n",
            ),
        ],
        2,
    );

    let proxy_port = spawn_proxy_with_config(
        pipeline_backend_config_with_max_connections(backend_port, "ConcurrentBodyPipeline", 4),
        RoutingMode::PerCommand,
    )
    .await?;

    let client_a_commands = &[
        "BODY <client-a-1@example>\r\n",
        "BODY <client-a-2@example>\r\n",
        "BODY <client-a-3@example>\r\n",
        "BODY <client-a-4@example>\r\n",
    ];
    let client_b_commands = &[
        "BODY <client-b-1@example>\r\n",
        "BODY <client-b-2@example>\r\n",
        "BODY <client-b-3@example>\r\n",
        "BODY <client-b-4@example>\r\n",
    ];

    let (client_a_responses, client_b_responses) = timeout(Duration::from_secs(5), async {
        tokio::try_join!(
            run_multiline_pipeline_batch(proxy_port, client_a_commands),
            run_multiline_pipeline_batch(proxy_port, client_b_commands),
        )
    })
    .await??;

    assert_eq!(
        client_a_responses,
        vec![
            (
                "222 1 <client-a-1@example>\r\n".to_string(),
                vec!["client-a-1-line\r\n".to_string()],
            ),
            (
                "222 2 <client-a-2@example>\r\n".to_string(),
                vec!["client-a-2-line\r\n".to_string()],
            ),
            (
                "222 3 <client-a-3@example>\r\n".to_string(),
                vec!["client-a-3-line\r\n".to_string()],
            ),
            (
                "222 4 <client-a-4@example>\r\n".to_string(),
                vec!["client-a-4-line\r\n".to_string()],
            ),
        ]
    );
    assert_eq!(
        client_b_responses,
        vec![
            (
                "222 1 <client-b-1@example>\r\n".to_string(),
                vec!["client-b-1-line\r\n".to_string()],
            ),
            (
                "222 2 <client-b-2@example>\r\n".to_string(),
                vec!["client-b-2-line\r\n".to_string()],
            ),
            (
                "222 3 <client-b-3@example>\r\n".to_string(),
                vec!["client-b-3-line\r\n".to_string()],
            ),
            (
                "222 4 <client-b-4@example>\r\n".to_string(),
                vec!["client-b-4-line\r\n".to_string()],
            ),
        ]
    );

    let mut seen_batches = batches.lock().await.clone();
    seen_batches.sort();
    assert_eq!(
        seen_batches,
        vec![
            vec![
                "BODY <client-a-1@example>".to_string(),
                "BODY <client-a-2@example>".to_string(),
                "BODY <client-a-3@example>".to_string(),
                "BODY <client-a-4@example>".to_string(),
            ],
            vec![
                "BODY <client-b-1@example>".to_string(),
                "BODY <client-b-2@example>".to_string(),
                "BODY <client-b-3@example>".to_string(),
                "BODY <client-b-4@example>".to_string(),
            ],
        ]
    );
    assert_eq!(
        body_connection_count.load(Ordering::SeqCst),
        2,
        "expected concurrent healthy BODY clients to reach two backend pipeline connections"
    );

    Ok(())
}

#[tokio::test]
async fn test_body_pipelining_keeps_two_clients_ordered_on_one_backend_connection() -> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let (body_connection_count, batches) = spawn_concurrent_pipeline_backend(
        backend_listener,
        &[
            (
                "BODY <client-a@example>",
                "222 1 <client-a@example>\r\nclient-a-line\r\n.\r\n",
            ),
            (
                "BODY <client-b@example>",
                "222 2 <client-b@example>\r\nclient-b-line\r\n.\r\n",
            ),
        ],
        1,
    );

    let proxy_port = spawn_proxy_with_config(
        pipeline_backend_config_with_max_connections(backend_port, "SingleConnConcurrentBody", 1),
        RoutingMode::PerCommand,
    )
    .await?;

    let client_a = connect_and_read_greeting(proxy_port).await?;
    let (read_half_a, mut write_half_a) = client_a.into_split();
    let mut reader_a = BufReader::new(read_half_a);

    let client_b = connect_and_read_greeting(proxy_port).await?;
    let (read_half_b, mut write_half_b) = client_b.into_split();
    let mut reader_b = BufReader::new(read_half_b);

    tokio::try_join!(
        write_commands(&mut write_half_a, &["BODY <client-a@example>\r\n"]),
        write_commands(&mut write_half_b, &["BODY <client-b@example>\r\n"]),
    )?;

    let ((status_a, lines_a), (status_b, lines_b)) = tokio::try_join!(
        read_multiline_response(&mut reader_a),
        read_multiline_response(&mut reader_b),
    )?;

    assert_eq!(status_a, "222 1 <client-a@example>\r\n");
    assert_eq!(lines_a, vec!["client-a-line\r\n".to_string()]);
    assert_eq!(status_b, "222 2 <client-b@example>\r\n");
    assert_eq!(lines_b, vec!["client-b-line\r\n".to_string()]);

    let recorded_batches = batches.lock().await.clone();
    assert_eq!(
        recorded_batches,
        vec![vec![
            "BODY <client-a@example>".to_string(),
            "BODY <client-b@example>".to_string(),
        ]],
        "expected both clients' BODY requests to share one backend pipeline batch"
    );
    assert_eq!(
        body_connection_count.load(Ordering::SeqCst),
        1,
        "expected both clients to share one backend connection"
    );

    Ok(())
}

#[tokio::test]
async fn test_body_client_disconnect_does_not_poison_other_client_on_shared_backend_connection()
-> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let (body_connection_count, batches) = spawn_concurrent_pipeline_backend(
        backend_listener,
        &[
            (
                "BODY <client-a@example>",
                "222 1 <client-a@example>\r\nclient-a-line\r\n.\r\n",
            ),
            (
                "BODY <client-b@example>",
                "222 2 <client-b@example>\r\nclient-b-line\r\n.\r\n",
            ),
        ],
        1,
    );

    let proxy_port = spawn_proxy_with_config(
        pipeline_backend_config_with_max_connections(
            backend_port,
            "DisconnectSharedBodyPipeline",
            1,
        ),
        RoutingMode::PerCommand,
    )
    .await?;

    let client_a = connect_and_read_greeting(proxy_port).await?;
    let (read_half_a, mut write_half_a) = client_a.into_split();
    let _reader_a = BufReader::new(read_half_a);

    let client_b = connect_and_read_greeting(proxy_port).await?;
    let (read_half_b, mut write_half_b) = client_b.into_split();
    let mut reader_b = BufReader::new(read_half_b);

    tokio::try_join!(
        write_commands(&mut write_half_a, &["BODY <client-a@example>\r\n"]),
        write_commands(&mut write_half_b, &["BODY <client-b@example>\r\n"]),
    )?;

    drop(write_half_a);

    let (status_b, lines_b) = read_multiline_response(&mut reader_b).await?;

    assert_eq!(status_b, "222 2 <client-b@example>\r\n");
    assert_eq!(lines_b, vec!["client-b-line\r\n".to_string()]);

    let recorded_batches = batches.lock().await.clone();
    assert_eq!(
        recorded_batches,
        vec![vec![
            "BODY <client-a@example>".to_string(),
            "BODY <client-b@example>".to_string(),
        ]],
        "expected the disconnecting client and surviving client to share one backend pipeline batch"
    );
    assert_eq!(
        body_connection_count.load(Ordering::SeqCst),
        1,
        "expected the surviving client to complete on the shared backend connection"
    );

    Ok(())
}

#[tokio::test]
async fn test_article_client_disconnect_does_not_poison_other_client_on_shared_backend_connection()
-> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let (article_connection_count, batches) = spawn_concurrent_pipeline_backend(
        backend_listener,
        &[
            (
                "ARTICLE <client-a@example>",
                "220 1 <client-a@example>\r\nclient-a-line\r\n.\r\n",
            ),
            (
                "ARTICLE <client-b@example>",
                "220 2 <client-b@example>\r\nclient-b-line\r\n.\r\n",
            ),
        ],
        1,
    );

    let proxy_port = spawn_proxy_with_config(
        pipeline_backend_config_with_max_connections(
            backend_port,
            "DisconnectSharedArticlePipeline",
            1,
        ),
        RoutingMode::PerCommand,
    )
    .await?;

    let client_a = connect_and_read_greeting(proxy_port).await?;
    let (read_half_a, mut write_half_a) = client_a.into_split();
    let _reader_a = BufReader::new(read_half_a);

    let client_b = connect_and_read_greeting(proxy_port).await?;
    let (read_half_b, mut write_half_b) = client_b.into_split();
    let mut reader_b = BufReader::new(read_half_b);

    tokio::try_join!(
        write_commands(&mut write_half_a, &["ARTICLE <client-a@example>\r\n"]),
        write_commands(&mut write_half_b, &["ARTICLE <client-b@example>\r\n"]),
    )?;

    drop(write_half_a);

    let (status_b, lines_b) = read_multiline_response(&mut reader_b).await?;

    assert_eq!(status_b, "220 2 <client-b@example>\r\n");
    assert_eq!(lines_b, vec!["client-b-line\r\n".to_string()]);

    let recorded_batches = batches.lock().await.clone();
    assert_eq!(
        recorded_batches,
        vec![vec![
            "ARTICLE <client-a@example>".to_string(),
            "ARTICLE <client-b@example>".to_string(),
        ]],
        "expected the disconnecting ARTICLE client and surviving client to share one backend pipeline batch"
    );
    assert_eq!(
        article_connection_count.load(Ordering::SeqCst),
        1,
        "expected the surviving ARTICLE client to complete on the shared backend connection"
    );

    Ok(())
}

#[tokio::test]
async fn test_body_430_retry_keeps_later_buffered_response_in_order() -> Result<()> {
    let backend_a = TcpListener::bind("127.0.0.1:0").await?;
    let backend_a_port = backend_a.local_addr()?.port();
    let backend_b = TcpListener::bind("127.0.0.1:0").await?;
    let backend_b_port = backend_b.local_addr()?.port();

    tokio::spawn(async move {
        while let Ok((mut stream, _)) = backend_a.accept().await {
            if stream
                .write_all(b"200 RetryBackendA Ready\r\n")
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
                        "BODY <retry@example>" => "430 No such article\r\n",
                        _ => "500 Unexpected command\r\n",
                    };
                    if stream.write_all(response.as_bytes()).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    tokio::spawn(async move {
        while let Ok((mut stream, _)) = backend_b.accept().await {
            if stream
                .write_all(b"200 RetryBackendB Ready\r\n")
                .await
                .is_err()
            {
                continue;
            }

            let mut pending = Vec::new();
            let mut buffer = [0u8; 1024];
            let mut retry_seen = false;
            let mut later_seen = false;

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

                    match command.as_str() {
                        "BODY <later@example>" => {
                            later_seen = true;
                            if stream
                                .write_all(b"222 0 <later@example>\r\nlater-line\r\n.\r\n")
                                .await
                                .is_err()
                            {
                                break;
                            }
                            if retry_seen
                                && stream
                                    .write_all(b"222 0 <retry@example>\r\nretry-line\r\n.\r\n")
                                    .await
                                    .is_err()
                            {
                                break;
                            }
                        }
                        "BODY <retry@example>" => {
                            retry_seen = true;
                            if later_seen
                                && stream
                                    .write_all(b"222 0 <retry@example>\r\nretry-line\r\n.\r\n")
                                    .await
                                    .is_err()
                            {
                                break;
                            }
                        }
                        _ => {
                            if stream
                                .write_all(b"500 Unexpected command\r\n")
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                }
            }
        }
    });

    let server_a =
        create_test_server_config_with_max_connections("127.0.0.1", backend_a_port, "RetryA", 1);
    let server_b =
        create_test_server_config_with_max_connections("127.0.0.1", backend_b_port, "RetryB", 1);
    let proxy_port = spawn_proxy_with_config(
        multi_backend_pipeline_config(vec![server_a, server_b]),
        RoutingMode::PerCommand,
    )
    .await?;

    let responses = run_multiline_pipeline_batch(
        proxy_port,
        &["BODY <retry@example>\r\n", "BODY <later@example>\r\n"],
    )
    .await?;

    assert_eq!(
        responses,
        vec![
            (
                "222 0 <retry@example>\r\n".to_string(),
                vec!["retry-line\r\n".to_string()],
            ),
            (
                "222 0 <later@example>\r\n".to_string(),
                vec!["later-line\r\n".to_string()],
            ),
        ],
        "later queued BODY response must not overtake the first request after a 430 retry"
    );

    Ok(())
}
