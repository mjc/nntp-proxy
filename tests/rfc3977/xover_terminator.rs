//! XOVER terminator and pipelining edge-case tests
//!
//! Ensure multiline XOVER responses are handled correctly and terminators respected.

use anyhow::Result;
use tokio::net::TcpListener;
use nntp_proxy::RoutingMode;

use crate::test_helpers::{MockNntpServer, connect_and_read_greeting, send_command_read_multiline_response, spawn_proxy_with_config, create_test_config};

#[tokio::test]
async fn test_xover_multiline_terminator() -> Result<()> {
    // Start backend that returns GROUP and a multiline XOVER
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();

    let backend = MockNntpServer::new(port)
        .with_name("XoverBackend")
        .on_command("GROUP", "211 100 1 100 alt.test\r\n")
        .on_command("XOVER", "224 1 Overview follows\r\n1\t<msg1@example.com>\tSubject1\r\n.\r\n")
        .spawn_on_listener(listener);

    let config = create_test_config(vec![(port, "xover-backend")]);
    let proxy_port = spawn_proxy_with_config(config, RoutingMode::Stateful).await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;

    // Select group
    let _ = crate::test_helpers::send_command_read_line(&mut client, "GROUP alt.test").await?;

    // Request XOVER and ensure we get the multiline body and terminator respected
    let (status, lines) = send_command_read_multiline_response(&mut client, "XOVER").await?;
    assert!(status.starts_with("224"), "Expected 224 XOVER response, got: {status:?}");
    assert!(lines.iter().any(|l| l.contains("<msg1@example.com>")), "Expected overview line with msgid");

    drop(backend);
    Ok(())
}
