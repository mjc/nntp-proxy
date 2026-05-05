//! LIST variants tests (NEWSGROUPS/LISTGROUP)

use anyhow::Result;
use nntp_proxy::RoutingMode;
use tokio::net::TcpListener;

use crate::test_helpers::{
    MockNntpServer, connect_and_read_greeting, create_test_config,
    send_command_read_multiline_response, spawn_proxy_with_config,
};

#[tokio::test]
async fn test_list_newsgroups_multiline() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();

    let backend = MockNntpServer::new(port)
        .with_name("ListBackend")
        .on_command(
            "LIST NEWSGROUPS",
            "215 list follows\r\nalt.test 100 1 y\r\n.\r\n",
        )
        .spawn_on_listener(listener);

    let config = create_test_config(vec![(port, "list-backend")]);
    let proxy_port = spawn_proxy_with_config(config, RoutingMode::PerCommand).await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;

    let (status, lines) =
        send_command_read_multiline_response(&mut client, "LIST NEWSGROUPS").await?;
    assert!(
        status.starts_with("215"),
        "Expected 215 LIST response, got: {status:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("alt.test")),
        "Expected alt.test in LIST output"
    );

    drop(backend);
    Ok(())
}

#[tokio::test]
async fn test_listgroup_multiline_uses_211_and_preserves_body() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();

    let backend = MockNntpServer::new(port)
        .with_name("ListgroupBackend")
        .on_command("LISTGROUP", "211 2 1 2 alt.test\r\n1\r\n2\r\n.\r\n")
        .spawn_on_listener(listener);

    let config = create_test_config(vec![(port, "listgroup-backend")]);
    let proxy_port = spawn_proxy_with_config(config, RoutingMode::Stateful).await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;

    let (status, lines) = send_command_read_multiline_response(&mut client, "LISTGROUP").await?;
    assert!(
        status.starts_with("211"),
        "Expected 211 LISTGROUP response, got: {status:?}"
    );
    assert_eq!(lines, vec!["1\r\n".to_string(), "2\r\n".to_string()]);

    drop(backend);
    Ok(())
}
