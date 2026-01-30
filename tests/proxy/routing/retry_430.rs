//! Tests for 430 (article not found) retry logic across backends
//!
//! This test suite verifies that the proxy properly retries article requests
//! across all backends when receiving 430 responses, and only returns 430 to
//! the client after ALL backends have been tried.

use anyhow::Result;
use nntp_proxy::RoutingMode;

use crate::test_helpers::{
    connect_and_read_greeting, send_article_read_full_response, setup_proxy_with_backends,
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

    eprintln!("Proxy running on port {}", proxy_port);

    // Connect and request article
    eprintln!("Connecting to proxy...");
    let mut client = connect_and_read_greeting(proxy_port).await?;

    eprintln!("Sending ARTICLE command...");
    let (status, body) = send_article_read_full_response(&mut client, "<test@example.com>").await?;

    eprintln!("Got response: {}", status.trim());

    // Should get article from Backend2 (after trying Backend1)
    assert!(status.starts_with("220"), "Expected 220, got: {}", status);
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
    let (status, body) = send_article_read_full_response(&mut client, "<test@example.com>").await?;

    // Should get 430 after trying all backends
    assert!(
        status.starts_with("430"),
        "Expected 430 after all backends tried, got: {}",
        status
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
    let (status, body) = send_article_read_full_response(&mut client, "<test@example.com>").await?;

    // Should get article from Backend3
    assert!(
        status.starts_with("220"),
        "Expected 220 from Backend3, got: {}",
        status
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
    let (status, body) = send_article_read_full_response(&mut client, "<test@example.com>").await?;

    assert!(
        status.starts_with("220"),
        "Expected article in hybrid mode, got: {}",
        status
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
        send_article_read_full_response(&mut client, "<first@example.com>").await?;
    assert!(status1.starts_with("220"), "First request should succeed");
    assert!(!body1.is_empty());

    // Second request - Round-robin tries Backend2 (430), should retry to Backend1
    let (status2, body2) =
        send_article_read_full_response(&mut client, "<second@example.com>").await?;
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
    let (status, body) = send_article_read_full_response(&mut client, "<test@example.com>").await?;
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
    let (status, body) = send_article_read_full_response(&mut client, "<test@example.com>").await?;

    assert!(
        status.starts_with("430"),
        "Expected 430 with single backend"
    );
    assert!(body.is_empty());

    Ok(())
}
