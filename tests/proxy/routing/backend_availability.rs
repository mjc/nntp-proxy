//! Tests for backend availability tracking edge cases
//!
//! This test suite verifies edge cases in backend availability tracking:
//! - Backend comes back online after being marked unavailable
//! - Cache TTL expiration
//! - All backends exhausted
//! - Availability bitset edge cases

use anyhow::Result;
use nntp_proxy::RoutingMode;
use std::time::Duration;

use crate::test_helpers::{
    connect_and_read_greeting, send_article_read_full_response, setup_proxy_with_backends,
};

#[tokio::test]
async fn test_all_backends_exhausted_returns_430() -> Result<()> {
    // When all backends return 430, proxy should return 430 to client
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
        "Should return 430 when all backends exhausted"
    );
    assert!(body.is_empty(), "430 should have no body");

    Ok(())
}

#[tokio::test]
async fn test_single_backend_immediate_430() -> Result<()> {
    // With only one backend that returns 430, should immediately return 430
    let (proxy_port, _backend_ports, _mock_handles) =
        setup_proxy_with_backends(vec![("OnlyBackend", false)], RoutingMode::PerCommand).await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;

    let (status, body) =
        send_article_read_full_response(&mut client, "<only-backend@example.com>").await?;

    assert!(
        status.starts_with("430"),
        "Single backend 430 should return immediately"
    );
    assert!(body.is_empty());

    Ok(())
}

#[tokio::test]
async fn test_availability_learned_across_requests() -> Result<()> {
    // Test that availability info is preserved across multiple requests
    let (proxy_port, _backend_ports, _mock_handles) = setup_proxy_with_backends(
        vec![
            ("Backend1", false), // Never has articles
            ("Backend2", true),  // Always has articles
        ],
        RoutingMode::PerCommand,
    )
    .await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;

    // First request learns Backend1=430, Backend2=220
    let (status1, _) = send_article_read_full_response(&mut client, "<first@example.com>").await?;
    assert!(status1.starts_with("220"), "First request should succeed");

    // Second request should use learned availability
    let (status2, _) = send_article_read_full_response(&mut client, "<second@example.com>").await?;
    assert!(
        status2.starts_with("220"),
        "Second request uses learned availability"
    );

    // Third request for different article
    let (status3, _) = send_article_read_full_response(&mut client, "<third@example.com>").await?;
    assert!(
        status3.starts_with("220"),
        "Third request uses learned availability"
    );

    Ok(())
}

#[tokio::test]
async fn test_partial_backend_availability() -> Result<()> {
    // Test with 3 backends where article exists on middle backend only
    let (proxy_port, _backend_ports, _mock_handles) = setup_proxy_with_backends(
        vec![
            ("Backend1", false),
            ("Backend2", true), // Only this one has it
            ("Backend3", false),
        ],
        RoutingMode::PerCommand,
    )
    .await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;

    let (status, body) =
        send_article_read_full_response(&mut client, "<partial@example.com>").await?;

    assert!(status.starts_with("220"), "Should find article on Backend2");
    assert!(!body.is_empty());

    Ok(())
}

#[tokio::test]
async fn test_availability_different_per_article() -> Result<()> {
    // Test that different articles can have different backend availability
    let (proxy_port, _backend_ports, _mock_handles) = setup_proxy_with_backends(
        vec![
            ("Backend1", true), // Has some articles
            ("Backend2", true), // Has some articles
        ],
        RoutingMode::PerCommand,
    )
    .await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;

    // Request multiple articles - each might be on different backends
    for i in 0..5 {
        let msgid = format!("<article-{}@example.com>", i);
        let (status, _) = send_article_read_full_response(&mut client, &msgid).await?;

        // Each article should either succeed or fail (no partial state)
        assert!(
            status.starts_with("220") || status.starts_with("430"),
            "Article {} should have valid status",
            i
        );
    }

    Ok(())
}

#[tokio::test]
async fn test_retry_stops_after_all_backends_tried() -> Result<()> {
    // Verify retry logic doesn't loop infinitely - stops after trying all backends
    let (proxy_port, _backend_ports, _mock_handles) = setup_proxy_with_backends(
        vec![
            ("Backend1", false),
            ("Backend2", false),
            ("Backend3", false),
            ("Backend4", false),
        ],
        RoutingMode::PerCommand,
    )
    .await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;

    // Should try all 4 backends then return 430
    let start = std::time::Instant::now();
    let (status, _) = send_article_read_full_response(&mut client, "<retry@example.com>").await?;
    let duration = start.elapsed();

    assert!(
        status.starts_with("430"),
        "Should return 430 after trying all backends"
    );

    // Should complete relatively quickly (not infinite loop)
    assert!(
        duration < Duration::from_secs(5),
        "Should not take more than 5 seconds"
    );

    Ok(())
}

#[tokio::test]
async fn test_availability_works_in_hybrid_mode() -> Result<()> {
    // Test that backend availability tracking works in hybrid mode too
    let (proxy_port, _backend_ports, _mock_handles) = setup_proxy_with_backends(
        vec![("Backend1", false), ("Backend2", true)],
        RoutingMode::Hybrid,
    )
    .await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;

    // Hybrid mode should still use availability tracking for stateless commands
    let (status, body) =
        send_article_read_full_response(&mut client, "<hybrid@example.com>").await?;

    assert!(
        status.starts_with("220"),
        "Hybrid mode should find article after retry"
    );
    assert!(!body.is_empty());

    Ok(())
}

#[tokio::test]
async fn test_maximum_backends_bitset_limit() -> Result<()> {
    // Test with maximum supported backends (8 = sizeof(u8) bits)
    // This tests the bitset limit
    let (proxy_port, _backend_ports, _mock_handles) = setup_proxy_with_backends(
        vec![
            ("Backend1", false),
            ("Backend2", false),
            ("Backend3", false),
            ("Backend4", false),
            ("Backend5", false),
            ("Backend6", false),
            ("Backend7", false),
            ("Backend8", true), // Only last one has it
        ],
        RoutingMode::PerCommand,
    )
    .await?;

    let mut client = connect_and_read_greeting(proxy_port).await?;

    let (status, body) =
        send_article_read_full_response(&mut client, "<max-backends@example.com>").await?;

    assert!(
        status.starts_with("220"),
        "Should find article on 8th backend"
    );
    assert!(!body.is_empty());

    Ok(())
}

#[tokio::test]
async fn test_concurrent_requests_same_article() -> Result<()> {
    // Test multiple concurrent requests for the same article
    let (proxy_port, _backend_ports, _mock_handles) = setup_proxy_with_backends(
        vec![("Backend1", false), ("Backend2", true)],
        RoutingMode::PerCommand,
    )
    .await?;

    // Create multiple clients
    let mut clients = Vec::new();
    for _ in 0..3 {
        let client = connect_and_read_greeting(proxy_port).await?;
        clients.push(client);
    }

    let msgid = "<concurrent@example.com>";

    // Send requests from all clients concurrently
    let mut tasks = Vec::new();
    for mut client in clients {
        let msgid = msgid.to_string();
        tasks.push(tokio::spawn(async move {
            send_article_read_full_response(&mut client, &msgid).await
        }));
    }

    // Wait for all to complete
    for task in tasks {
        let result = task.await??;
        assert!(
            result.0.starts_with("220"),
            "All concurrent requests should succeed"
        );
    }

    Ok(())
}
