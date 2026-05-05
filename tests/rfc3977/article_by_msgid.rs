//! ARTICLE by message-id tests (RFC 3977)
//!
//! Verify ARTICLE <message-id> behavior in per-command routing.

use anyhow::Result;
use tokio::net::TcpListener;

use crate::test_helpers::{
    MockNntpServer, connect_and_read_greeting, create_test_config,
    send_article_read_multiline_response, spawn_proxy_with_config,
};
use nntp_proxy::RoutingMode;

async fn spawn_client_with_backend(
    mode: RoutingMode,
    backend: MockNntpServer,
) -> Result<tokio::net::TcpStream> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let _backend = backend.spawn_on_listener(backend_listener);

    let config = create_test_config(vec![(backend_port, "article-backend")]);
    let proxy_port = spawn_proxy_with_config(config, mode).await?;
    connect_and_read_greeting(proxy_port).await
}

fn article_backend(port: u16, message_id: &str) -> MockNntpServer {
    MockNntpServer::new(port)
        .with_name("ArticleBackend")
        .on_command(
            &format!("ARTICLE {message_id}"),
            &format!("220 0 <{message_id}>\r\nSubject: Test\r\n\r\nBody for {message_id}\r\n.\r\n"),
        )
}

#[tokio::test]
async fn test_article_by_message_id_in_per_command_mode() -> Result<()> {
    let msgid = "msgid-123@example.com";
    let mut client =
        spawn_client_with_backend(RoutingMode::PerCommand, article_backend(0, msgid)).await?;

    let (status, body) = send_article_read_multiline_response(&mut client, msgid).await?;
    assert!(
        status.starts_with("220"),
        "Expected 220 ARTICLE response, got: {status:?}"
    );
    let concatenated = body.concat();
    assert!(
        concatenated.contains("Body for"),
        "ARTICLE body should be returned in per-command mode, got: {body:?}"
    );
    Ok(())
}
