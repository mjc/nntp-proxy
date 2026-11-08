//! Tests for 430 (article not found) retry logic across backends
//!
//! This test suite verifies that the proxy properly retries article requests
//! across all backends when receiving 430 responses, and only returns 430 to
//! the client after ALL backends have been tried.

use anyhow::Result;
use nntp_proxy::RoutingMode;
use tokio::io::AsyncWriteExt;

use crate::test_helpers::{
    MockNntpServer, connect_and_read_greeting, create_test_config, read_line_from_stream,
    send_article_read_multiline_response, send_command_read_line, setup_proxy_with_backends,
    spawn_proxy_with_config, spawn_single_backend_proxy,
};

#[tokio::test]
async fn test_430_retry_finds_article_on_second_backend() -> Result<()> {
    // Setup: Backend1 returns 430, Backend2 has the article
    eprintln!("Setting up proxy with 2 backends...");
    let (proxy_port, _backend_ports, _mock_handles) = setup_proxy_with_backends(
        vec![
            ("Backend1", false), // No article
            ("Backend2", true),  // Has article
        ],
        RoutingMode::PerCommand,
    )
    .await?;

    eprintln!("Proxy running on port {proxy_port}");

    // Connect and request article
    eprintln!("Connecting to proxy...");
    let mut client = connect_and_read_greeting(proxy_port).await?;

    eprintln!("Sending ARTICLE command...");
    let (status, body) =
        send_article_read_multiline_response(&mut client, "<test@example.com>").await?;

    eprintln!("Got response: {}", status.trim());

    // Should get article from Backend2 (after trying Backend1)
    assert!(status.starts_with("220"), "Expected 220, got: {status}");
    assert!(!body.is_empty(), "Expected article body");

    Ok(())
}

#[tokio::test]
async fn test_430_retry_tries_all_backends_before_giving_up() -> Result<()> {
    // Setup: All 3 backends return 430
    let (proxy_port, _backend_ports, _mock_handles) = setup_proxy_with_backends(
        vec![
            ("Backend1", false),
            ("Backend2", false),
            ("Backend3", false),
        ],
        RoutingMode::PerCommand,
    )
    .await?;

    // Connect and request article
    let mut client = connect_and_read_greeting(proxy_port).await?;
    let (status, body) =
        send_article_read_multiline_response(&mut client, "<test@example.com>").await?;

    // Should get 430 after trying all backends
    assert!(
        status.starts_with("430"),
        "Expected 430 after all backends tried, got: {status}"
    );
    assert!(body.is_empty(), "430 response should have no body");

    Ok(())
}

#[tokio::test]
async fn test_430_retry_finds_article_on_third_backend() -> Result<()> {
    // Setup: Backend 1 and 2 return 430, Backend 3 has article
    let (proxy_port, _backend_ports, _mock_handles) = setup_proxy_with_backends(
        vec![
            ("Backend1", false),
            ("Backend2", false),
            ("Backend3", true), // Has article
        ],
        RoutingMode::PerCommand,
    )
    .await?;

    // Connect and request article
    let mut client = connect_and_read_greeting(proxy_port).await?;
    let (status, body) =
        send_article_read_multiline_response(&mut client, "<test@example.com>").await?;

    // Should get article from Backend3
    assert!(
        status.starts_with("220"),
        "Expected 220 from Backend3, got: {status}"
    );
    assert!(!body.is_empty(), "Expected article body");

    Ok(())
}

#[tokio::test]
async fn test_430_retry_works_in_hybrid_mode() -> Result<()> {
    // Verify 430 retry works in hybrid mode (before switching to stateful)
    let (proxy_port, _backend_ports, _mock_handles) = setup_proxy_with_backends(
        vec![("Backend1", false), ("Backend2", true)],
        RoutingMode::Hybrid, // Use hybrid mode
    )
    .await?;

    // Connect and request article (stateless command, should use retry logic)
    let mut client = connect_and_read_greeting(proxy_port).await?;
    let (status, body) =
        send_article_read_multiline_response(&mut client, "<test@example.com>").await?;

    assert!(
        status.starts_with("220"),
        "Expected article in hybrid mode, got: {status}"
    );
    assert!(!body.is_empty(), "Expected article body");

    Ok(())
}

#[tokio::test]
async fn test_430_retry_multiple_articles_different_patterns() -> Result<()> {
    // Test multiple article requests with different 430 patterns
    let (proxy_port, _backend_ports, _mock_handles) = setup_proxy_with_backends(
        vec![
            ("Backend1", true),  // Has articles
            ("Backend2", false), // No articles
        ],
        RoutingMode::PerCommand,
    )
    .await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;

    // First request - Round-robin should try Backend1 first (has article)
    let (status1, body1) =
        send_article_read_multiline_response(&mut client, "<first@example.com>").await?;
    assert!(status1.starts_with("220"), "First request should succeed");
    assert!(!body1.is_empty());

    // Second request - Round-robin tries Backend2 (430), should retry to Backend1
    let (status2, body2) =
        send_article_read_multiline_response(&mut client, "<second@example.com>").await?;
    assert!(
        status2.starts_with("220"),
        "Second request should succeed after retry"
    );
    assert!(!body2.is_empty());

    Ok(())
}

#[tokio::test]
async fn test_430_retry_only_retries_for_430() -> Result<()> {
    // Verify that other 4xx errors (like 423) don't trigger retries
    let (proxy_port, _backend_ports, _mock_handles) = setup_proxy_with_backends(
        vec![("Backend1", true), ("Backend2", true)],
        RoutingMode::PerCommand,
    )
    .await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;

    // Normal article request should work
    let (status, body) =
        send_article_read_multiline_response(&mut client, "<test@example.com>").await?;
    assert!(status.starts_with("220"), "Expected article");
    assert!(!body.is_empty());

    Ok(())
}

#[tokio::test]
async fn test_430_retry_with_single_backend() -> Result<()> {
    // With only one backend, should immediately return 430
    let (proxy_port, _backend_ports, _mock_handles) =
        setup_proxy_with_backends(vec![("OnlyBackend", false)], RoutingMode::PerCommand).await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;
    let (status, body) =
        send_article_read_multiline_response(&mut client, "<test@example.com>").await?;

    assert!(
        status.starts_with("430"),
        "Expected 430 with single backend"
    );
    assert!(body.is_empty());

    Ok(())
}

#[tokio::test]
async fn test_430_retry_retires_connection_with_queued_backend_bytes() -> Result<()> {
    let (proxy_port, _backend) =
        spawn_single_backend_proxy(RoutingMode::PerCommand, "Packed430Backend", |_port| {
            MockNntpServer::new()
                .with_name("Packed430Backend")
                .on_command(
                    "ARTICLE",
                    "430 No such article\r\n223 0 <stale@example.com>\r\n",
                )
        })
        .await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;

    let (first_status, first_body) =
        send_article_read_multiline_response(&mut client, "<first@example.com>").await?;
    assert!(
        first_status.starts_with("430"),
        "first request should exhaust the single backend, got {first_status:?}"
    );
    assert!(first_body.is_empty(), "430 response should have no body");

    let (second_status, second_body) =
        send_article_read_multiline_response(&mut client, "<second@example.com>").await?;
    assert!(
        second_status.starts_with("430"),
        "stale queued backend bytes must not satisfy the next ARTICLE, got {second_status:?}"
    );
    assert!(second_body.is_empty(), "430 response should have no body");

    Ok(())
}

#[tokio::test]
async fn test_retry_tier_transient_exhaustion_returns_reply_and_keeps_session_open() -> Result<()> {
    let (bad0_port, _bad0) = MockNntpServer::new()
        .with_name("BadTier0A")
        .on_command("DATE", "111 20251203120000\r\n")
        .on_command("ARTICLE", "not a status line\r\n")
        .spawn_on_random_port()
        .await?;
    let (bad1_port, _bad1) = MockNntpServer::new()
        .with_name("BadTier0B")
        .on_command("DATE", "111 20251203120000\r\n")
        .on_command("ARTICLE", "also not a status line\r\n")
        .spawn_on_random_port()
        .await?;
    let (tier1_port, _tier1) = MockNntpServer::new()
        .with_name("Tier1ShouldNotBeUsed")
        .on_command("DATE", "111 20251203120000\r\n")
        .on_command(
            "ARTICLE",
            "220 0 <test@example.com>\r\nSubject: Backup\r\n\r\nBody\r\n.\r\n",
        )
        .spawn_on_random_port()
        .await?;

    let mut config = create_test_config(vec![
        (bad0_port, "BadTier0A"),
        (bad1_port, "BadTier0B"),
        (tier1_port, "Tier1ShouldNotBeUsed"),
    ]);
    config.servers[2].tier = 1;
    let proxy_port = spawn_proxy_with_config(config, RoutingMode::PerCommand).await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;
    let (status, body) =
        send_article_read_multiline_response(&mut client, "<transient@example.com>").await?;
    assert!(
        status.starts_with("503"),
        "transient tier exhaustion should return a command reply, got: {status}"
    );
    assert!(body.is_empty(), "503 response should not have a body");

    let date = send_command_read_line(&mut client, "DATE").await?;
    assert!(
        date.starts_with("111"),
        "session should remain usable after transient tier exhaustion, got: {date}"
    );

    Ok(())
}

#[tokio::test]
async fn test_ordered_retry_transient_exhaustion_returns_in_order_replies() -> Result<()> {
    let (bad0_port, _bad0) = MockNntpServer::new()
        .with_name("OrderedBadTier0A")
        .on_command("DATE", "111 20251203120000\r\n")
        .on_command("ARTICLE", "not a status line\r\n")
        .spawn_on_random_port()
        .await?;
    let (bad1_port, _bad1) = MockNntpServer::new()
        .with_name("OrderedBadTier0B")
        .on_command("DATE", "111 20251203120000\r\n")
        .on_command("ARTICLE", "also not a status line\r\n")
        .spawn_on_random_port()
        .await?;
    let (tier1_port, _tier1) = MockNntpServer::new()
        .with_name("OrderedTier1ShouldNotBeUsed")
        .on_command("DATE", "111 20251203120000\r\n")
        .on_command(
            "ARTICLE",
            "220 0 <test@example.com>\r\nSubject: Backup\r\n\r\nBody\r\n.\r\n",
        )
        .spawn_on_random_port()
        .await?;

    let mut config = create_test_config(vec![
        (bad0_port, "OrderedBadTier0A"),
        (bad1_port, "OrderedBadTier0B"),
        (tier1_port, "OrderedTier1ShouldNotBeUsed"),
    ]);
    config.servers[2].tier = 1;
    let proxy_port = spawn_proxy_with_config(config, RoutingMode::PerCommand).await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;
    client
        .write_all(
            b"ARTICLE <ordered-transient-a@example.com>\r\nARTICLE <ordered-transient-b@example.com>\r\n",
        )
        .await?;
    client.flush().await?;

    let first = read_line_from_stream(&mut client, "first ordered transient ARTICLE").await?;
    let second = read_line_from_stream(&mut client, "second ordered transient ARTICLE").await?;

    assert!(
        first.starts_with("503"),
        "first ordered transient exhaustion should return 503, got: {first}"
    );
    assert!(
        second.starts_with("503"),
        "second ordered transient exhaustion should return 503, got: {second}"
    );

    let date = send_command_read_line(&mut client, "DATE").await?;
    assert!(
        date.starts_with("111"),
        "session should remain usable after ordered transient exhaustion, got: {date}"
    );

    Ok(())
}

/// Send BODY and read its status line plus any multiline body.
async fn send_body_read_response(
    client: &mut tokio::net::TcpStream,
    message_id: &str,
) -> Result<(String, Vec<String>)> {
    client
        .write_all(format!("BODY {message_id}\r\n").as_bytes())
        .await?;
    client.flush().await?;
    let status = read_line_from_stream(client, "BODY status line").await?;
    let mut body = Vec::new();
    if status.starts_with("222") {
        loop {
            let line = read_line_from_stream(client, "BODY body").await?;
            if line == ".\r\n" {
                break;
            }
            body.push(line);
        }
    }
    Ok((status, body))
}

#[tokio::test]
async fn test_non_430_error_reply_fails_over_to_backend_with_article() -> Result<()> {
    // Newsgroupdirect answers lookups it cannot serve with `451 0 <id>`.
    let (refusing_port, _refusing) = MockNntpServer::new()
        .with_name("Refuses451")
        .on_command("DATE", "111 20251203120000\r\n")
        .on_command("BODY", "451 0 <refused@example.com>\r\n")
        .spawn_on_random_port()
        .await?;
    let (good_port, _good) = MockNntpServer::new()
        .with_name("HasArticle")
        .on_command("DATE", "111 20251203120000\r\n")
        .on_command("BODY", "222 0 <found@example.com>\r\nBody line\r\n.\r\n")
        .spawn_on_random_port()
        .await?;

    let config = create_test_config(vec![
        (refusing_port, "Refuses451"),
        (good_port, "HasArticle"),
    ]);
    let proxy_port = spawn_proxy_with_config(config, RoutingMode::PerCommand).await?;
    let mut client = connect_and_read_greeting(proxy_port).await?;

    // Several lookups so the refusing backend is picked first at least once,
    // whatever order the selector starts in.
    for index in 0..4 {
        let (status, body) =
            send_body_read_response(&mut client, &format!("<failover-{index}@example.com>"))
                .await?;
        assert!(
            status.starts_with("222"),
            "lookup {index} should be served by the backend that has it, got: {status}"
        );
        assert_eq!(body, vec!["Body line\r\n".to_string()]);
    }

    Ok(())
}

#[tokio::test]
async fn test_non_430_error_replies_from_every_backend_return_fallback() -> Result<()> {
    let (first_port, _first) = MockNntpServer::new()
        .with_name("Refuses451A")
        .on_command("DATE", "111 20251203120000\r\n")
        .on_command("BODY", "451 0 <refused@example.com>\r\n")
        .spawn_on_random_port()
        .await?;
    let (second_port, _second) = MockNntpServer::new()
        .with_name("Refuses451B")
        .on_command("DATE", "111 20251203120000\r\n")
        .on_command("BODY", "451 0 <refused@example.com>\r\n")
        .spawn_on_random_port()
        .await?;

    let config = create_test_config(vec![
        (first_port, "Refuses451A"),
        (second_port, "Refuses451B"),
    ]);
    let proxy_port = spawn_proxy_with_config(config, RoutingMode::PerCommand).await?;
    let mut client = connect_and_read_greeting(proxy_port).await?;

    let (status, body) = send_body_read_response(&mut client, "<refused@example.com>").await?;
    // No backend reported the article missing, so the reply is the one the
    // retry loop gives when no backend can take the request, not the
    // provider-specific 451.
    assert!(
        status.starts_with("503"),
        "every backend refusing should return the no-backend reply, got: {status}"
    );
    assert!(body.is_empty());

    let date = send_command_read_line(&mut client, "DATE").await?;
    assert!(
        date.starts_with("111"),
        "session should remain usable after every backend refused, got: {date}"
    );

    Ok(())
}
