//! RFC 3977 discovery command tests (`NEWGROUPS`, `NEWNEWS`).
//!
//! These commands are stateless and return multiline responses that should work
//! without switching the client into stateful reader mode.

use anyhow::Result;
use tokio::net::{TcpListener, TcpStream};

use crate::test_helpers::{
    MockNntpServer, connect_and_read_greeting, create_test_config, send_command_read_line,
    send_command_read_multiline_response, spawn_proxy_with_config,
};
use nntp_proxy::RoutingMode;

async fn spawn_client_with_backend(
    mode: RoutingMode,
    backend: MockNntpServer,
) -> Result<TcpStream> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let _backend = backend.spawn_on_listener(backend_listener);

    let config = create_test_config(vec![(backend_port, "discovery-backend")]);
    let proxy_port = spawn_proxy_with_config(config, mode).await?;
    connect_and_read_greeting(proxy_port).await
}

fn discovery_backend(port: u16) -> MockNntpServer {
    MockNntpServer::new(port)
        .with_name("DiscoveryBackend")
        .on_command("MODE READER", "200 Reader mode acknowledged\r\n")
        .on_command(
            "NEWGROUPS",
            "231 List of new newsgroups follows\r\nalt.new 0000000002 0000000001 y\r\n.\r\n",
        )
        .on_command(
            "NEWNEWS",
            "230 List of new articles follows\r\n<new-1@example.com>\r\n.\r\n",
        )
        .on_command("DATE", "111 20260505161700\r\n")
}

#[tokio::test]
async fn test_newgroups_returns_multiline_and_session_continues_in_per_command() -> Result<()> {
    let mut client =
        spawn_client_with_backend(RoutingMode::PerCommand, discovery_backend(0)).await?;

    let (status, lines) =
        send_command_read_multiline_response(&mut client, "NEWGROUPS 20260505 000000 GMT").await?;
    assert!(
        status.starts_with("231"),
        "RFC 3977 §7.3: NEWGROUPS should return 231 on success, got: {status:?}"
    );
    assert_eq!(
        lines,
        vec!["alt.new 0000000002 0000000001 y\r\n".to_string()]
    );

    let response = send_command_read_line(&mut client, "DATE").await?;
    assert!(
        response.starts_with("111"),
        "The session should remain usable after NEWGROUPS, got: {response:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_newnews_returns_multiline_in_hybrid_without_stateful_switch() -> Result<()> {
    let mut client = spawn_client_with_backend(RoutingMode::Hybrid, discovery_backend(0)).await?;

    let (status, lines) =
        send_command_read_multiline_response(&mut client, "NEWNEWS * 20260505 000000 GMT").await?;
    assert!(
        status.starts_with("230"),
        "RFC 3977 §7.4: NEWNEWS should return 230 on success, got: {status:?}"
    );
    assert_eq!(lines, vec!["<new-1@example.com>\r\n".to_string()]);

    let response = send_command_read_line(&mut client, "DATE").await?;
    assert!(
        response.starts_with("111"),
        "Hybrid mode should continue serving stateless commands after NEWNEWS, got: {response:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_mode_reader_is_forwarded_statelessly_and_session_continues() -> Result<()> {
    let mut client =
        spawn_client_with_backend(RoutingMode::PerCommand, discovery_backend(0)).await?;

    let response = send_command_read_line(&mut client, "MODE READER").await?;
    assert!(
        response.starts_with("200"),
        "RFC 3977 §5.3: client-issued MODE READER should be forwarded successfully, got: {response:?}"
    );

    let response = send_command_read_line(&mut client, "DATE").await?;
    assert!(
        response.starts_with("111"),
        "The session should remain usable after MODE READER, got: {response:?}"
    );

    Ok(())
}
