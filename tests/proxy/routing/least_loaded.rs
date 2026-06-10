//! Least-loaded selection strategy tests
//!
//! Tests for the least-loaded algorithm that routes to the backend
//! with the fewest pending requests relative to capacity.

use super::*;
use nntp_proxy::config::BackendSelectionStrategy;
use std::sync::{Arc, Barrier};
use std::thread;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[test]
fn test_least_loaded_basic() {
    let mut selector = BackendSelector::with_strategy(BackendSelectionStrategy::LeastLoaded);

    // Add two backends with equal capacity
    selector.add_backend(
        ServerName::try_new("backend0".to_string()).unwrap(),
        create_backend("backend0", 10),
        0, // tier
    );
    selector.add_backend(
        ServerName::try_new("backend1".to_string()).unwrap(),
        create_backend("backend1", 10),
        0, // tier
    );

    // First request can go to either backend when both are empty.
    let backend1 = selector
        .route(nntp_proxy::router::RouteRequest::new(ClientId::new()))
        .unwrap();
    assert!(backend1.as_index() <= 1);

    // Second request should go to the other backend.
    let backend2 = selector
        .route(nntp_proxy::router::RouteRequest::new(ClientId::new()))
        .unwrap();
    assert_ne!(backend2, backend1);

    // Third request is another equal-load tie.
    let backend3 = selector
        .route(nntp_proxy::router::RouteRequest::new(ClientId::new()))
        .unwrap();
    assert!(backend3.as_index() <= 1);

    // Drain the test's pending commands and recreate a known imbalance.
    selector.complete_command(backend1);
    selector.complete_command(backend2);
    selector.complete_command(backend3);
    selector.mark_backend_pending(BackendId::from_index(0));

    // Next request should go to backend 1 (ratio 0/10 vs backend 0's 1/10).
    let backend4 = selector
        .route(nntp_proxy::router::RouteRequest::new(ClientId::new()))
        .unwrap();
    assert_eq!(backend4.as_index(), 1);
}

#[test]
fn test_least_loaded_unequal_capacity() {
    let mut selector = BackendSelector::with_strategy(BackendSelectionStrategy::LeastLoaded);

    // Backend 0: 10 connections (small)
    // Backend 1: 50 connections (large)
    selector.add_backend(
        ServerName::try_new("small".to_string()).unwrap(),
        create_backend("small", 10),
        0, // tier
    );
    selector.add_backend(
        ServerName::try_new("large".to_string()).unwrap(),
        create_backend("large", 50),
        0, // tier
    );

    // Route 15 requests
    let mut counts = [0; 2];
    for _ in 0..15 {
        let backend = selector
            .route(nntp_proxy::router::RouteRequest::new(ClientId::new()))
            .unwrap();
        counts[backend.as_index()] += 1;
    }

    // Backend 1 (larger) should get more requests
    // Expected rough distribution: backend 0 gets ~3, backend 1 gets ~12
    // (ratio should favor the larger backend significantly)
    assert!(
        counts[1] > counts[0],
        "Large backend should get more requests: {counts:?}"
    );
    assert!(
        counts[1] >= 10,
        "Large backend should get most requests: {counts:?}"
    );
}

#[test]
fn test_concurrent_least_loaded_fills_tier_capacity_without_overshoot() {
    let mut selector = BackendSelector::with_strategy(BackendSelectionStrategy::LeastLoaded);

    selector.add_backend(
        ServerName::try_new("tier0-40".to_string()).unwrap(),
        create_backend("tier0-40", 40),
        0,
    );
    selector.add_backend(
        ServerName::try_new("tier0-50".to_string()).unwrap(),
        create_backend("tier0-50", 50),
        0,
    );

    let selector = Arc::new(selector);
    let barrier = Arc::new(Barrier::new(90));
    let mut handles = Vec::with_capacity(90);

    for _ in 0..90 {
        let selector = selector.clone();
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            barrier.wait();
            selector
                .route(nntp_proxy::router::RouteRequest::new(ClientId::new()))
                .unwrap()
                .as_index()
        }));
    }

    let mut counts = [0usize; 2];
    for handle in handles {
        counts[handle.join().unwrap()] += 1;
    }

    assert_eq!(
        counts,
        [40, 50],
        "concurrent routing should fill the tier0 pools proportionally without over-queueing one backend"
    );
}

#[test]
fn test_initial_article_probe_uses_capacity_fair_distribution_despite_retry_load() {
    let mut selector = BackendSelector::with_strategy(BackendSelectionStrategy::LeastLoaded);

    selector.add_backend(
        ServerName::try_new("tier0-40".to_string()).unwrap(),
        create_backend("tier0-40", 40),
        0,
    );
    selector.add_backend(
        ServerName::try_new("tier0-50".to_string()).unwrap(),
        create_backend("tier0-50", 50),
        0,
    );

    for _ in 0..30 {
        selector.mark_backend_pending(BackendId::from_index(1));
    }

    let availability = nntp_proxy::cache::ArticleAvailability::new();
    let mut counts = [0usize; 2];
    for _ in 0..90 {
        let backend = selector
            .route(
                nntp_proxy::router::RouteRequest::new(ClientId::new())
                    .with_availability(&availability),
            )
            .unwrap();
        counts[backend.as_index()] += 1;
    }

    assert_eq!(
        counts,
        [40, 50],
        "initial article probes should use capacity-fair distribution, not retry-induced pending load"
    );
}

#[test]
fn test_least_loaded_respects_pending_counts() {
    let mut selector = BackendSelector::with_strategy(BackendSelectionStrategy::LeastLoaded);

    selector.add_backend(
        ServerName::try_new("backend0".to_string()).unwrap(),
        create_backend("backend0", 10),
        0, // tier
    );
    selector.add_backend(
        ServerName::try_new("backend1".to_string()).unwrap(),
        create_backend("backend1", 10),
        0, // tier
    );

    // Route 10 requests - they should distribute evenly (both start at 0)
    for _ in 0..10 {
        selector
            .route(nntp_proxy::router::RouteRequest::new(ClientId::new()))
            .unwrap();
    }

    // Both backends now have 5 pending each (even distribution)
    assert_eq!(
        selector
            .backend_load(BackendId::from_index(0))
            .map(|c| c.get()),
        Some(5)
    );
    assert_eq!(
        selector
            .backend_load(BackendId::from_index(1))
            .map(|c| c.get()),
        Some(5)
    );

    // Complete 3 commands from backend 0
    for _ in 0..3 {
        selector.complete_command(BackendId::from_index(0));
    }

    // Now backend 0 has 2 pending, backend 1 has 5 pending
    // Next request should go to backend 0 (ratio 2/10 = 0.2 vs 5/10 = 0.5)
    let backend = selector
        .route(nntp_proxy::router::RouteRequest::new(ClientId::new()))
        .unwrap();
    assert_eq!(
        backend.as_index(),
        0,
        "Should route to less loaded backend (backend 0 with 2 pending vs backend 1 with 5 pending)"
    );
}

async fn spawn_setup_backend() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };

            tokio::spawn(async move {
                let mut stream = tokio::io::BufReader::new(stream);
                let _ = stream.get_mut().write_all(b"200 test ready\r\n").await;

                let mut line = Vec::with_capacity(64);
                for _ in 0..2 {
                    line.clear();
                    let Ok(n) = stream.read_until(b'\n', &mut line).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    if line.starts_with(b"COMPRESS") {
                        let _ = stream
                            .get_mut()
                            .write_all(b"501 compression unsupported\r\n")
                            .await;
                        break;
                    }
                    let _ = stream.get_mut().write_all(b"200 reader mode\r\n").await;
                }

                let mut hold = [0u8; 1];
                let _ = stream.get_mut().read(&mut hold).await;
            });
        }
    });

    port
}

fn live_backend(name: &str, port: u16, max_connections: usize) -> DeadpoolConnectionProvider {
    DeadpoolConnectionProvider::builder("127.0.0.1", port)
        .name(name)
        .max_connections(max_connections)
        .build()
        .unwrap()
}

#[tokio::test]
async fn test_least_loaded_counts_checked_out_pool_connections() {
    let backend0_port = spawn_setup_backend().await;
    let backend1_port = spawn_setup_backend().await;

    let mut selector = BackendSelector::with_strategy(BackendSelectionStrategy::LeastLoaded);
    let backend0 = live_backend("backend0", backend0_port, 2);
    let backend1 = live_backend("backend1", backend1_port, 2);

    let held_backend1 = backend1.checkout_connection_guard().await.unwrap();

    selector.add_backend(
        ServerName::try_new("backend0".to_string()).unwrap(),
        backend0,
        0,
    );
    selector.add_backend(
        ServerName::try_new("backend1".to_string()).unwrap(),
        backend1,
        0,
    );

    assert_eq!(
        selector
            .route(nntp_proxy::router::RouteRequest::new(ClientId::new()))
            .unwrap()
            .as_index(),
        0,
        "least-loaded must count checked-out pool connections, not only router pending counts"
    );

    drop(held_backend1.complete_success());
}

#[test]
fn test_least_loaded_single_backend() {
    let mut selector = BackendSelector::with_strategy(BackendSelectionStrategy::LeastLoaded);

    selector.add_backend(
        ServerName::try_new("only".to_string()).unwrap(),
        create_backend("only", 10),
        0, // tier
    );

    // All requests should go to the only backend
    for _ in 0..20 {
        let backend = selector
            .route(nntp_proxy::router::RouteRequest::new(ClientId::new()))
            .unwrap();
        assert_eq!(backend.as_index(), 0);
    }
}

#[test]
fn test_least_loaded_load_balancing_fairness() {
    let mut selector = BackendSelector::with_strategy(BackendSelectionStrategy::LeastLoaded);

    // Three backends with equal capacity
    for i in 0..3 {
        selector.add_backend(
            ServerName::try_new(format!("backend-{i}")).unwrap(),
            create_backend(&format!("backend-{i}"), 10),
            0, // tier
        );
    }

    // Route 30 requests
    let mut counts = [0; 3];
    for _ in 0..30 {
        let backend = selector
            .route(nntp_proxy::router::RouteRequest::new(ClientId::new()))
            .unwrap();
        counts[backend.as_index()] += 1;
    }

    // With equal capacity and no completions, should distribute evenly
    // Each backend should get exactly 10 requests (30 / 3 = 10)
    assert_eq!(counts, [10, 10, 10], "Should distribute evenly: {counts:?}");
}
