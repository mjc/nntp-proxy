//! RFC 3977 stateful reader workflow tests.
//!
//! These tests cover state-dependent reader semantics that parser-only checks
//! cannot validate: commands that require a selected group, and hybrid-mode
//! switching into a backend session that preserves current-group context.

use anyhow::Result;
use tokio::net::{TcpListener, TcpStream};

use crate::test_helpers::{
    MockNntpServer, connect_and_read_greeting, create_test_config,
    send_article_read_multiline_response, send_command_read_line, spawn_proxy_with_config,
};
use nntp_proxy::RoutingMode;

async fn spawn_client_with_backend(
    mode: RoutingMode,
    backend: MockNntpServer,
) -> Result<TcpStream> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let _backend = backend.spawn_on_listener(backend_listener);

    let config = create_test_config(vec![(backend_port, "workflow-backend")]);
    let proxy_port = spawn_proxy_with_config(config, mode).await?;
    connect_and_read_greeting(proxy_port).await
}

fn workflow_backend(port: u16) -> MockNntpServer {
    MockNntpServer::new(port)
        .with_name("WorkflowBackend")
        .on_command("GROUP", "211 3 1 3 alt.test\r\n")
        .on_command("NEXT", "223 2 <second@example.com>\r\n")
        .on_command("LAST", "223 1 <first@example.com>\r\n")
        .on_command(
            "ARTICLE 2",
            "220 2 <second@example.com>\r\nSubject: Second\r\n\r\nBody 2\r\n.\r\n",
        )
}

fn no_such_group_backend(port: u16) -> MockNntpServer {
    MockNntpServer::new(port)
        .with_name("MissingGroupBackend")
        .on_command("GROUP missing.group", "411 No such newsgroup\r\n")
        .on_command("DATE", "111 20260504112233\r\n")
}

fn missing_article_number_backend(port: u16) -> MockNntpServer {
    MockNntpServer::new(port)
        .with_name("MissingArticleBackend")
        .on_command("GROUP", "211 3 1 3 alt.test\r\n")
        .on_command("ARTICLE 999", "423 No article with that number\r\n")
        .on_command("LAST", "223 1 <first@example.com>\r\n")
}

#[tokio::test]
async fn test_next_without_group_returns_412_and_session_continues() -> Result<()> {
    let backend = MockNntpServer::new(0)
        .with_name("NoGroupBackend")
        .on_command("NEXT", "412 No newsgroup selected\r\n")
        .on_command("GROUP", "211 3 1 3 alt.test\r\n");
    let mut client = spawn_client_with_backend(RoutingMode::Stateful, backend).await?;

    let response = send_command_read_line(&mut client, "NEXT").await?;
    assert!(
        response.starts_with("412"),
        "RFC 3977 §6.2.4: NEXT without a selected group should return 412, got: {response:?}"
    );

    let response = send_command_read_line(&mut client, "GROUP alt.test").await?;
    assert!(
        response.starts_with("211"),
        "Session should remain usable after 412, got: {response:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_hybrid_group_switch_preserves_next_last_and_article_number_workflow() -> Result<()> {
    let mut client = spawn_client_with_backend(RoutingMode::Hybrid, workflow_backend(0)).await?;

    let response = send_command_read_line(&mut client, "GROUP alt.test").await?;
    assert!(
        response.starts_with("211"),
        "RFC 3977 §6.1.1: GROUP should select the group in hybrid mode, got: {response:?}"
    );

    let response = send_command_read_line(&mut client, "NEXT").await?;
    assert!(
        response.starts_with("223"),
        "RFC 3977 §6.2.4: NEXT should work after GROUP, got: {response:?}"
    );

    let response = send_command_read_line(&mut client, "LAST").await?;
    assert!(
        response.starts_with("223"),
        "RFC 3977 §6.2.5: LAST should work after GROUP, got: {response:?}"
    );

    let (status, body) = send_article_read_multiline_response(&mut client, "2").await?;
    assert!(
        status.starts_with("220 2 "),
        "RFC 3977 §6.2.1: ARTICLE by number should work in the selected group, got: {status:?}"
    );
    assert!(
        body.concat().contains("Body 2\r\n"),
        "Expected ARTICLE body from selected-group context, got: {body:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_missing_group_returns_411_and_session_continues() -> Result<()> {
    let mut client =
        spawn_client_with_backend(RoutingMode::Stateful, no_such_group_backend(0)).await?;

    let response = send_command_read_line(&mut client, "GROUP missing.group").await?;
    assert!(
        response.starts_with("411"),
        "RFC 3977 §6.1.1: missing groups should return 411, got: {response:?}"
    );

    let response = send_command_read_line(&mut client, "DATE").await?;
    assert!(
        response.starts_with("111"),
        "A 411 response should not terminate the session, got: {response:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_missing_article_number_returns_423_after_group_selection() -> Result<()> {
    let mut client =
        spawn_client_with_backend(RoutingMode::Hybrid, missing_article_number_backend(0)).await?;

    let response = send_command_read_line(&mut client, "GROUP alt.test").await?;
    assert!(
        response.starts_with("211"),
        "Expected selected group before ARTICLE by number, got: {response:?}"
    );

    let response = send_command_read_line(&mut client, "ARTICLE 999").await?;
    assert!(
        response.starts_with("423"),
        "RFC 3977 §6.2.1: missing article numbers should return 423, got: {response:?}"
    );

    let response = send_command_read_line(&mut client, "LAST").await?;
    assert!(
        response.starts_with("223"),
        "A 423 response should not break the selected-group session, got: {response:?}"
    );

    Ok(())
}
