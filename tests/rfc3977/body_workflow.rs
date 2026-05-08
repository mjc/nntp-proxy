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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{Duration, timeout};

use crate::test_helpers::{
    MockNntpServer, RfcTestClient, connect_and_read_greeting,
    create_test_server_config_with_max_connections, spawn_proxy_with_config,
};
use nntp_proxy::{Config, RoutingMode};

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

async fn read_line(stream: &mut TcpStream, context: &str) -> Result<String> {
    let mut bytes = Vec::with_capacity(128);
    let mut byte = [0u8; 1];

    loop {
        let n = match timeout(Duration::from_secs(2), stream.read(&mut byte)).await {
            Ok(result) => result?,
            Err(_) => anyhow::bail!("Timed out while reading {context}"),
        };
        if n == 0 {
            if bytes.is_empty() {
                anyhow::bail!("Connection closed while reading {context}");
            }
            anyhow::bail!("Connection closed before line terminator while reading {context}");
        }

        bytes.push(byte[0]);
        if byte[0] == b'\n' {
            return String::from_utf8(bytes).map_err(Into::into);
        }
    }
}

async fn read_multiline_response(stream: &mut TcpStream) -> Result<(String, Vec<String>)> {
    let status_line = read_line(stream, "BODY status line").await?;
    let mut lines = Vec::new();

    loop {
        let line = read_line(stream, "BODY response line").await?;
        if line == ".\r\n" {
            break;
        }
        lines.push(line);
    }

    Ok((status_line, lines))
}

#[tokio::test]
async fn test_body_pipelining_pairs_four_responses_on_single_backend_connection() -> Result<()> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
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

                    let response = match command.as_str() {
                        "MODE READER" => Some("200 Posting prohibited\r\n"),
                        "DATE" => Some("111 20260508120000\r\n"),
                        "BODY <body-1@example>" => {
                            Some("222 1 <body-1@example>\r\nbody-1-line\r\n.\r\n")
                        }
                        "BODY <body-2@example>" => {
                            Some("222 2 <body-2@example>\r\nbody-2-line\r\n.\r\n")
                        }
                        "BODY <body-3@example>" => {
                            Some("222 3 <body-3@example>\r\nbody-3-line\r\n.\r\n")
                        }
                        "BODY <body-4@example>" => {
                            Some("222 4 <body-4@example>\r\nbody-4-line\r\n.\r\n")
                        }
                        _ => Some("500 Unexpected command\r\n"),
                    };

                    if let Some(response) = response
                        && stream.write_all(response.as_bytes()).await.is_err()
                    {
                        break;
                    }
                }
            }
        }
    });

    let mut server = create_test_server_config_with_max_connections(
        "127.0.0.1",
        backend_port,
        "BodyPipeline",
        1,
    );
    server.pipeline_batch_size = 4;
    let config = Config {
        servers: vec![server],
        ..Default::default()
    };

    let proxy_port = spawn_proxy_with_config(config, RoutingMode::PerCommand).await?;
    let mut client = connect_and_read_greeting(proxy_port).await?;

    client
        .write_all(
            concat!(
                "BODY <body-1@example>\r\n",
                "BODY <body-2@example>\r\n",
                "BODY <body-3@example>\r\n",
                "BODY <body-4@example>\r\n"
            )
            .as_bytes(),
        )
        .await?;
    client.flush().await?;

    let responses = [
        read_multiline_response(&mut client).await?,
        read_multiline_response(&mut client).await?,
        read_multiline_response(&mut client).await?,
        read_multiline_response(&mut client).await?,
    ];

    assert_eq!(
        responses,
        [
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
