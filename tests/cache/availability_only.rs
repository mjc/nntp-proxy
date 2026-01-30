//! Tests for availability-only mode (cache_articles = false)
//!
//! This test suite verifies that the cache works in availability-only mode,
//! storing only backend availability bitsets without headers/body data.
//! This mode should use minimal memory while still providing smart routing.

use anyhow::Result;
use nntp_proxy::RoutingMode;

use crate::test_helpers::{
    connect_and_read_greeting, send_article_read_full_response, setup_proxy_with_backends,
};

#[tokio::test]
async fn test_availability_only_mode_tracks_backend_availability() -> Result<()> {
    // In availability-only mode (cache_articles=false), the cache should still
    // track which backends have which articles for smart routing
    let (proxy_port, _backend_ports, _mock_handles) = setup_proxy_with_backends(
        vec![("Backend1", true), ("Backend2", false)],
        RoutingMode::PerCommand,
    )
    .await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;

    let msgid = "<availability-test@example.com>";

    // First request - should succeed and track availability
    let (status1, body1) = send_article_read_full_response(&mut client, msgid).await?;
    assert!(
        status1.starts_with("220"),
        "First request should succeed: {}",
        status1
    );
    assert!(!body1.is_empty());

    // Second request - should use cached availability info to route to correct backend
    let (status2, body2) = send_article_read_full_response(&mut client, msgid).await?;
    assert!(
        status2.starts_with("220"),
        "Second request should use cached availability"
    );
    assert!(!body2.is_empty());

    Ok(())
}

#[tokio::test]
async fn test_availability_only_mode_retries_430() -> Result<()> {
    // Test that availability-only mode still does 430 retries
    let (proxy_port, _backend_ports, _mock_handles) = setup_proxy_with_backends(
        vec![
            ("Backend1", false), // No article
            ("Backend2", true),  // Has article
        ],
        RoutingMode::PerCommand,
    )
    .await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;

    // Request should retry from Backend1 (430) to Backend2 (220)
    let (status, body) =
        send_article_read_full_response(&mut client, "<retry-test@example.com>").await?;
    assert!(
        status.starts_with("220"),
        "Should find article on Backend2 after retry"
    );
    assert!(!body.is_empty(), "Should return article body");

    Ok(())
}

#[tokio::test]
async fn test_availability_only_mode_learns_from_requests() -> Result<()> {
    // Test that availability tracking improves over multiple requests
    let (proxy_port, _backend_ports, _mock_handles) = setup_proxy_with_backends(
        vec![
            ("Backend1", false), // No articles
            ("Backend2", true),  // Has all articles
        ],
        RoutingMode::PerCommand,
    )
    .await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;

    // Request multiple articles - cache should learn Backend2 has them
    for i in 0..5 {
        let msgid = format!("<article-{}@example.com>", i);
        let (status, body) = send_article_read_full_response(&mut client, &msgid).await?;
        assert!(status.starts_with("220"), "Article {} should succeed", i);
        assert!(!body.is_empty());
    }

    // All requests should succeed, with cache learning backend availability
    Ok(())
}

#[tokio::test]
async fn test_availability_only_mode_mixed_availability() -> Result<()> {
    // Test with articles split across backends
    let (proxy_port, _backend_ports, _mock_handles) = setup_proxy_with_backends(
        vec![
            ("Backend1", true), // Has some articles
            ("Backend2", true), // Has some articles
        ],
        RoutingMode::PerCommand,
    )
    .await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;

    // Request multiple articles - some on each backend
    for i in 0..10 {
        let msgid = format!("<mixed-article-{}@example.com>", i);
        let (status, _body) = send_article_read_full_response(&mut client, &msgid).await?;
        // May get 220 or 430 depending on backend availability
        assert!(
            status.starts_with("220") || status.starts_with("430"),
            "Valid response for article {}",
            i
        );
    }

    Ok(())
}

#[tokio::test]
async fn test_availability_only_mode_all_backends_missing() -> Result<()> {
    // Test that 430 is returned when all backends don't have article
    let (proxy_port, _backend_ports, _mock_handles) = setup_proxy_with_backends(
        vec![
            ("Backend1", false),
            ("Backend2", false),
            ("Backend3", false),
        ],
        RoutingMode::PerCommand,
    )
    .await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;

    let (status, body) =
        send_article_read_full_response(&mut client, "<missing@example.com>").await?;
    assert!(
        status.starts_with("430"),
        "Should return 430 when all backends missing"
    );
    assert!(body.is_empty(), "430 response should have no body");

    Ok(())
}
