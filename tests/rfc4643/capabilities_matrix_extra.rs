//! Additional CAPABILITIES matrix tests (RFC 4643)
//!
//! Ensure CAPABILITIES injection/stripping is correct across routing modes.

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::time::{Duration, timeout};

use crate::test_helpers::{
    MockNntpServer, RfcTestClient, connect_and_read_greeting, create_test_config_with_auth,
    send_command_read_line, spawn_proxy_with_config,
};
use nntp_proxy::RoutingMode;

const BACKEND_CAPS_VARIANT: &str =
    "101 Backend CAPABILITIES\r\nVERSION 2\r\nSTARTTLS\r\nSASL PLAIN\r\nCOMPRESS GZIP\r\n.\r\n";

async fn spawn_caps_client_with_mode_and_config(
    mode: RoutingMode,
    config_auth: bool,
) -> Result<RfcTestClient> {
    if config_auth {
        RfcTestClient::spawn_with_auth(mode, "caps-backend", "alice", "wonderland", |port| {
            MockNntpServer::new(port)
                .with_name("CapsVariantBackend")
                .on_command("CAPABILITIES", BACKEND_CAPS_VARIANT)
        })
        .await
    } else {
        RfcTestClient::spawn(mode, "caps-backend", |port| {
            MockNntpServer::new(port)
                .with_name("CapsVariantBackend")
                .on_command("CAPABILITIES", BACKEND_CAPS_VARIANT)
        })
        .await
    }
}

async fn spawn_authenticated_stateful_caps_client() -> Result<tokio::net::TcpStream> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();

    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                if stream
                    .write_all(b"200 StatefulCaps Ready\r\n")
                    .await
                    .is_err()
                {
                    return;
                }

                let mut pending = Vec::new();
                let mut buffer = [0u8; 1024];

                loop {
                    let n = match stream.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    pending.extend_from_slice(&buffer[..n]);

                    while let Some(pos) = pending.windows(2).position(|w| w == b"\r\n") {
                        let line: Vec<u8> = pending.drain(..pos + 2).collect();
                        let cmd = String::from_utf8_lossy(&line).trim().to_uppercase();

                        let response: &[u8] = if cmd.starts_with("MODE READER") {
                            b"200 Reader mode on\r\n"
                        } else if cmd.starts_with("AUTHINFO USER") {
                            b"381 Password required\r\n"
                        } else if cmd.starts_with("AUTHINFO PASS") {
                            b"281 Authentication accepted\r\n"
                        } else if cmd.starts_with("DATE") {
                            b"111 20260505155900\r\n"
                        } else if cmd.starts_with("HELP") {
                            if stream
                                .write_all(b"100 Help follows\r\nThis is help text\r\n.\r\n")
                                .await
                                .is_err()
                            {
                                return;
                            }
                            continue;
                        } else if cmd.starts_with("QUIT") {
                            b"205 Goodbye\r\n"
                        } else {
                            b"500 What?\r\n"
                        };

                        if stream.write_all(response).await.is_err() {
                            return;
                        }

                        if cmd.starts_with("QUIT") {
                            return;
                        }
                    }
                }
            });
        }
    });

    let config = create_test_config_with_auth(vec![port], "alice", "wonderland");
    let proxy_port = spawn_proxy_with_config(config, RoutingMode::Stateful).await?;
    connect_and_read_greeting(proxy_port).await
}

async fn read_line_with_timeout(reader: &mut BufReader<tokio::net::TcpStream>) -> Result<String> {
    let mut line = String::new();
    let n = timeout(Duration::from_secs(2), reader.read_line(&mut line)).await??;
    anyhow::ensure!(n > 0, "connection closed before expected line");
    Ok(line)
}

#[tokio::test]
async fn test_capabilities_in_hybrid_before_auth_injects_authinfo() -> Result<()> {
    let mut client = spawn_caps_client_with_mode_and_config(RoutingMode::Hybrid, true).await?;

    let body = client
        .expect_multiline("CAPABILITIES", "101")
        .await?
        .concat();
    assert!(
        body.contains("AUTHINFO USER PASS"),
        "Proxy should inject AUTHINFO when configured for client auth and before auth, got: {body:?}"
    );
    assert!(
        !body.contains("STARTTLS"),
        "Proxy must not leak STARTTLS, got: {body:?}"
    );
    Ok(())
}

#[tokio::test]
async fn test_capabilities_in_per_command_after_auth_strips_authinfo() -> Result<()> {
    let mut client = spawn_caps_client_with_mode_and_config(RoutingMode::PerCommand, true).await?;

    client.authenticate("alice", "wonderland").await?;
    let body = client
        .expect_multiline("CAPABILITIES", "101")
        .await?
        .concat();
    assert!(
        !body.contains("AUTHINFO"),
        "Proxy must not advertise AUTHINFO after client auth, got: {body:?}"
    );
    Ok(())
}

#[tokio::test]
async fn test_capabilities_no_inject_when_proxy_no_auth() -> Result<()> {
    // When the proxy is not configured for client auth, it should not inject AUTHINFO
    let mut client = spawn_caps_client_with_mode_and_config(RoutingMode::PerCommand, false).await?;

    let body = client
        .expect_multiline("CAPABILITIES", "101")
        .await?
        .concat();
    assert!(
        !body.contains("AUTHINFO"),
        "Proxy must not inject AUTHINFO when not configured for client auth"
    );
    Ok(())
}

#[tokio::test]
async fn test_capabilities_filters_backend_only_features() -> Result<()> {
    // Ensure backend-only features like SASL/COMPRESS/STARTTLS are not forwarded to clients
    let mut client = spawn_caps_client_with_mode_and_config(RoutingMode::Hybrid, true).await?;

    let body = client
        .expect_multiline("CAPABILITIES", "101")
        .await?
        .concat();
    assert!(
        !body.contains("SASL"),
        "Proxy must filter SASL from CAPABILITIES"
    );
    assert!(
        !body.contains("COMPRESS"),
        "Proxy must filter COMPRESS from CAPABILITIES"
    );
    Ok(())
}

#[tokio::test]
async fn test_stateful_capabilities_reply_waits_for_prior_backend_multiline() -> Result<()> {
    let mut client = spawn_authenticated_stateful_caps_client().await?;

    let resp = send_command_read_line(&mut client, "AUTHINFO USER alice").await?;
    assert!(
        resp.starts_with("381"),
        "Expected 381 password required, got: {resp:?}"
    );
    let resp = send_command_read_line(&mut client, "AUTHINFO PASS wonderland").await?;
    assert!(
        resp.starts_with("281"),
        "Expected 281 auth accepted, got: {resp:?}"
    );

    client
        .write_all(b"DATE\r\nHELP\r\nCAPABILITIES\r\n")
        .await?;

    let mut reader = BufReader::new(client);
    let date = read_line_with_timeout(&mut reader).await?;
    assert!(
        date.starts_with("111"),
        "Expected DATE response first, got: {date:?}"
    );

    let help_status = read_line_with_timeout(&mut reader).await?;
    assert!(
        help_status.starts_with("100"),
        "Expected HELP status before local CAPABILITIES, got: {help_status:?}"
    );

    let help_body = read_line_with_timeout(&mut reader).await?;
    assert_eq!(
        help_body, "This is help text\r\n",
        "Expected HELP body before local CAPABILITIES"
    );

    let help_terminator = read_line_with_timeout(&mut reader).await?;
    assert_eq!(
        help_terminator, ".\r\n",
        "Expected HELP terminator before local CAPABILITIES"
    );

    let caps_status = read_line_with_timeout(&mut reader).await?;
    assert!(
        caps_status.starts_with("101"),
        "Expected deferred CAPABILITIES only after HELP completed, got: {caps_status:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_stateful_capabilities_stays_ordered_before_later_backend_reply() -> Result<()> {
    let mut client = spawn_authenticated_stateful_caps_client().await?;

    let resp = send_command_read_line(&mut client, "AUTHINFO USER alice").await?;
    assert!(
        resp.starts_with("381"),
        "Expected 381 password required, got: {resp:?}"
    );
    let resp = send_command_read_line(&mut client, "AUTHINFO PASS wonderland").await?;
    assert!(
        resp.starts_with("281"),
        "Expected 281 auth accepted, got: {resp:?}"
    );

    client
        .write_all(b"HELP\r\nCAPABILITIES\r\nDATE\r\n")
        .await?;

    let mut reader = BufReader::new(client);
    let help_status = read_line_with_timeout(&mut reader).await?;
    assert!(
        help_status.starts_with("100"),
        "Expected HELP status first, got: {help_status:?}"
    );

    let help_body = read_line_with_timeout(&mut reader).await?;
    assert_eq!(
        help_body, "This is help text\r\n",
        "Expected HELP body before local CAPABILITIES"
    );

    let help_terminator = read_line_with_timeout(&mut reader).await?;
    assert_eq!(
        help_terminator, ".\r\n",
        "Expected HELP terminator before local CAPABILITIES"
    );

    let caps_status = read_line_with_timeout(&mut reader).await?;
    assert!(
        caps_status.starts_with("101"),
        "Expected deferred CAPABILITIES before later DATE, got: {caps_status:?}"
    );

    let caps_version = read_line_with_timeout(&mut reader).await?;
    assert_eq!(caps_version, "VERSION 2\r\n");

    let caps_reader = read_line_with_timeout(&mut reader).await?;
    assert_eq!(caps_reader, "READER\r\n");

    let caps_over = read_line_with_timeout(&mut reader).await?;
    assert_eq!(caps_over, "OVER\r\n");

    let caps_hdr = read_line_with_timeout(&mut reader).await?;
    assert_eq!(caps_hdr, "HDR\r\n");

    let caps_terminator = read_line_with_timeout(&mut reader).await?;
    assert_eq!(
        caps_terminator, ".\r\n",
        "Expected full CAPABILITIES response before DATE"
    );

    let date = read_line_with_timeout(&mut reader).await?;
    assert!(
        date.starts_with("111"),
        "Expected DATE only after deferred CAPABILITIES, got: {date:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_stateful_capabilities_stays_ordered_between_backend_replies() -> Result<()> {
    let mut client = spawn_authenticated_stateful_caps_client().await?;

    let resp = send_command_read_line(&mut client, "AUTHINFO USER alice").await?;
    assert!(
        resp.starts_with("381"),
        "Expected 381 password required, got: {resp:?}"
    );
    let resp = send_command_read_line(&mut client, "AUTHINFO PASS wonderland").await?;
    assert!(
        resp.starts_with("281"),
        "Expected 281 auth accepted, got: {resp:?}"
    );

    client
        .write_all(b"DATE\r\nCAPABILITIES\r\nDATE\r\n")
        .await?;

    let mut reader = BufReader::new(client);
    let first_date = read_line_with_timeout(&mut reader).await?;
    assert!(
        first_date.starts_with("111"),
        "Expected first DATE response before CAPABILITIES, got: {first_date:?}"
    );

    let caps_status = read_line_with_timeout(&mut reader).await?;
    assert!(
        caps_status.starts_with("101"),
        "Expected deferred CAPABILITIES between DATE responses, got: {caps_status:?}"
    );

    let caps_version = read_line_with_timeout(&mut reader).await?;
    assert_eq!(caps_version, "VERSION 2\r\n");

    let caps_reader = read_line_with_timeout(&mut reader).await?;
    assert_eq!(caps_reader, "READER\r\n");

    let caps_over = read_line_with_timeout(&mut reader).await?;
    assert_eq!(caps_over, "OVER\r\n");

    let caps_hdr = read_line_with_timeout(&mut reader).await?;
    assert_eq!(caps_hdr, "HDR\r\n");

    let caps_terminator = read_line_with_timeout(&mut reader).await?;
    assert_eq!(
        caps_terminator, ".\r\n",
        "Expected full CAPABILITIES response before the second DATE"
    );

    let second_date = read_line_with_timeout(&mut reader).await?;
    assert!(
        second_date.starts_with("111"),
        "Expected second DATE only after deferred CAPABILITIES, got: {second_date:?}"
    );

    Ok(())
}
