//! RFC 3977 utility command tests (`HELP`, bare `LIST`).

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

    let config = create_test_config(vec![(backend_port, "utility-backend")]);
    let proxy_port = spawn_proxy_with_config(config, mode).await?;
    connect_and_read_greeting(proxy_port).await
}

fn utility_backend(port: u16) -> MockNntpServer {
    MockNntpServer::new(port)
        .with_name("UtilityBackend")
        .on_command(
            "HELP",
            "100 Help text follows\r\nThis server supports NNTP basics\r\n.\r\n",
        )
        .on_command(
            "LIST",
            "215 list follows\r\nalt.test 2 1 y\r\nalt.misc 5 1 y\r\n.\r\n",
        )
        .on_command("DATE", "111 20260505163500\r\n")
}

#[tokio::test]
async fn test_help_returns_multiline_and_session_continues() -> Result<()> {
    let mut client = spawn_client_with_backend(RoutingMode::PerCommand, utility_backend(0)).await?;

    let (status, lines) = send_command_read_multiline_response(&mut client, "HELP").await?;
    assert!(
        status.starts_with("100"),
        "RFC 3977 §7.5: HELP should return 100, got: {status:?}"
    );
    assert_eq!(
        lines,
        vec!["This server supports NNTP basics\r\n".to_string()]
    );

    let response = send_command_read_line(&mut client, "DATE").await?;
    assert!(
        response.starts_with("111"),
        "Session should remain usable after HELP, got: {response:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_list_returns_multiline_group_listing() -> Result<()> {
    let mut client = spawn_client_with_backend(RoutingMode::Hybrid, utility_backend(0)).await?;

    let (status, lines) = send_command_read_multiline_response(&mut client, "LIST").await?;
    assert!(
        status.starts_with("215"),
        "RFC 3977 §7.6: LIST should return 215, got: {status:?}"
    );
    assert_eq!(
        lines,
        vec![
            "alt.test 2 1 y\r\n".to_string(),
            "alt.misc 5 1 y\r\n".to_string()
        ]
    );

    let response = send_command_read_line(&mut client, "DATE").await?;
    assert!(
        response.starts_with("111"),
        "Hybrid session should remain usable after LIST, got: {response:?}"
    );

    Ok(())
}
