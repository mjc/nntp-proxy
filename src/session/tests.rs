use super::*;
use crate::protocol::QUIT;
use std::net::{IpAddr, Ipv4Addr};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn test_client_session_creation() {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
    let buffer_pool = BufferPool::new(1024, 4);
    let session = ClientSession::new(addr, buffer_pool.clone());

    assert_eq!(session.client_addr.port(), 8080);
    assert_eq!(
        session.client_addr.ip(),
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))
    );
}

#[test]
fn test_client_session_with_different_ports() {
    let buffer_pool = BufferPool::new(1024, 4);

    let addr1 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
    let session1 = ClientSession::new(addr1, buffer_pool.clone());

    let addr2 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 9090);
    let session2 = ClientSession::new(addr2, buffer_pool.clone());

    assert_ne!(session1.client_addr.port(), session2.client_addr.port());
    assert_eq!(session1.client_addr.port(), 8080);
    assert_eq!(session2.client_addr.port(), 9090);
}

#[test]
fn test_client_session_with_ipv6() {
    let buffer_pool = BufferPool::new(1024, 4);
    let addr = SocketAddr::new(IpAddr::V6("::1".parse().unwrap()), 8119);
    let session = ClientSession::new(addr, buffer_pool);

    assert_eq!(session.client_addr.port(), 8119);
    assert!(session.client_addr.is_ipv6());
}

#[test]
fn test_buffer_pool_cloning() {
    let buffer_pool = BufferPool::new(8192, 10);
    let buffer_pool_clone = buffer_pool.clone();

    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 1234);
    let _session1 = ClientSession::new(addr, buffer_pool);
    let _session2 = ClientSession::new(addr, buffer_pool_clone);

    // Both sessions should work with the same underlying pool
}

#[test]
fn test_session_addr_formatting() {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 5555);
    let buffer_pool = BufferPool::new(1024, 4);
    let session = ClientSession::new(addr, buffer_pool);

    let addr_str = format!("{}", session.client_addr);
    assert!(addr_str.contains("10.0.0.1"));
    assert!(addr_str.contains("5555"));
}

#[test]
fn test_multiple_sessions_same_buffer_pool() {
    let buffer_pool = BufferPool::new(4096, 8);
    let sessions: Vec<_> = (0..5)
        .map(|i| {
            let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8000 + i);
            ClientSession::new(addr, buffer_pool.clone())
        })
        .collect();

    assert_eq!(sessions.len(), 5);
    for (i, session) in sessions.iter().enumerate() {
        assert_eq!(session.client_addr.port(), 8000 + i as u16);
    }
}

#[test]
fn test_loopback_address() {
    let buffer_pool = BufferPool::new(1024, 4);
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8119);
    let session = ClientSession::new(addr, buffer_pool);

    assert!(session.client_addr.ip().is_loopback());
}

#[test]
fn test_unspecified_address() {
    let buffer_pool = BufferPool::new(1024, 4);
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
    let session = ClientSession::new(addr, buffer_pool);

    assert!(session.client_addr.ip().is_unspecified());
    assert_eq!(session.client_addr.port(), 0);
}

#[test]
fn test_session_without_router() {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
    let buffer_pool = BufferPool::new(1024, 4);
    let session = ClientSession::new(addr, buffer_pool);

    assert!(!session.is_per_command_routing());
    assert_eq!(session.client_addr.port(), 8080);
}

#[test]
fn test_session_with_router() {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
    let buffer_pool = BufferPool::new(1024, 4);
    let router = Arc::new(BackendSelector::new());
    let session =
        ClientSession::new_with_router(addr, buffer_pool, router, RoutingMode::PerCommand);

    assert!(session.is_per_command_routing());
    assert_eq!(session.client_addr.port(), 8080);
}

#[test]
fn test_client_id_uniqueness() {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
    let buffer_pool = BufferPool::new(1024, 4);

    let session1 = ClientSession::new(addr, buffer_pool.clone());
    let session2 = ClientSession::new(addr, buffer_pool);

    // Each session should have a unique client ID
    assert_ne!(session1.client_id(), session2.client_id());
}

#[tokio::test]
async fn test_quit_command_per_command_routing() {
    use tokio::net::TcpListener;

    // Start a mock server for the backend
    let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = backend_listener.local_addr().unwrap();

    // Spawn mock backend server
    tokio::spawn(async move {
        if let Ok((mut stream, _)) = backend_listener.accept().await {
            // Send greeting
            let _ = stream.write_all(b"200 Mock Server Ready\r\n").await;

            // Read and discard any commands, keep connection alive briefly
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
        }
    });

    // Give server time to start
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Create a router with the mock backend
    let mut router = BackendSelector::new();
    let provider = crate::pool::DeadpoolConnectionProvider::new(
        "127.0.0.1".to_string(),
        backend_addr.port(),
        "test-backend".to_string(),
        2,
        None,
        None,
    );
    router.add_backend(
        crate::types::BackendId::from_index(0),
        "test-backend".to_string(),
        provider,
    );

    // Create client connection
    let client_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client_addr = client_listener.local_addr().unwrap();

    // Create session
    let buffer_pool = BufferPool::new(1024, 4);
    let session = ClientSession::new_with_router(
        client_addr,
        buffer_pool,
        Arc::new(router),
        RoutingMode::PerCommand,
    );

    // Spawn client that sends QUIT and immediately closes
    let client_handle = tokio::spawn(async move {
        let mut client = tokio::net::TcpStream::connect(client_addr).await.unwrap();

        // Read greeting
        let mut greeting = [0u8; 256];
        let n = client.read(&mut greeting).await.unwrap();
        assert!(n > 0);

        // Send QUIT command
        client.write_all(QUIT).await.unwrap();

        // Try to read response (might fail if we close too fast, which is fine)
        let mut response = [0u8; 256];
        let _ = client.read(&mut response).await;

        // Close connection immediately (simulating SABnzbd behavior)
        drop(client);
    });

    // Accept client connection
    let (client_stream, _) = client_listener.accept().await.unwrap();

    // Handle the session - should not return an error despite client closing
    let result = session.handle_per_command_routing(client_stream).await;

    // Should succeed (not propagate broken pipe error)
    assert!(
        result.is_ok(),
        "QUIT handling should not return error: {:?}",
        result
    );

    if let Ok((sent, received)) = result {
        // Should have sent QUIT command
        assert!(sent > 0, "Should have sent bytes (QUIT command)");
        // Should have received greeting and possibly closing message
        assert!(received > 0, "Should have received bytes (greeting)");
    }

    // Wait for client to finish
    let _ = client_handle.await;
}

#[tokio::test]
async fn test_quit_command_closes_connection_cleanly() {
    use tokio::net::TcpListener;

    // Start a mock backend
    let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = backend_listener.local_addr().unwrap();

    tokio::spawn(async move {
        if let Ok((mut stream, _)) = backend_listener.accept().await {
            let _ = stream.write_all(b"200 Ready\r\n").await;
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
        }
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Create router
    let mut router = BackendSelector::new();
    let provider = crate::pool::DeadpoolConnectionProvider::new(
        "127.0.0.1".to_string(),
        backend_addr.port(),
        "test".to_string(),
        1,
        None,
        None,
    );
    router.add_backend(
        crate::types::BackendId::from_index(0),
        "test".to_string(),
        provider,
    );

    // Create client
    let client_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client_addr = client_listener.local_addr().unwrap();

    let buffer_pool = BufferPool::new(1024, 4);
    let session = ClientSession::new_with_router(
        client_addr,
        buffer_pool,
        Arc::new(router),
        RoutingMode::PerCommand,
    );

    // Client that sends QUIT and waits for response
    let client_handle = tokio::spawn(async move {
        let mut client = tokio::net::TcpStream::connect(client_addr).await.unwrap();

        // Read greeting
        let mut buf = [0u8; 256];
        let n = client.read(&mut buf).await.unwrap();
        assert!(n > 0, "Should receive greeting");

        // Send QUIT
        client.write_all(QUIT).await.unwrap();

        // Read closing response
        let n = client.read(&mut buf).await.unwrap();

        // Should receive "205 Connection closing"
        let response = String::from_utf8_lossy(&buf[..n]);
        assert!(
            response.contains("205"),
            "Should receive 205 closing response"
        );
    });

    let (client_stream, _) = client_listener.accept().await.unwrap();
    let result = session.handle_per_command_routing(client_stream).await;

    assert!(result.is_ok(), "Session should handle QUIT cleanly");

    client_handle.await.unwrap();
}

#[test]
fn test_session_mode_enum() {
    // Test SessionMode enum behavior
    assert_eq!(SessionMode::PerCommand, SessionMode::PerCommand);
    assert_eq!(SessionMode::Stateful, SessionMode::Stateful);
    assert_ne!(SessionMode::PerCommand, SessionMode::Stateful);

    // Test Debug formatting
    let per_command = format!("{:?}", SessionMode::PerCommand);
    let stateful = format!("{:?}", SessionMode::Stateful);
    assert!(per_command.contains("PerCommand"));
    assert!(stateful.contains("Stateful"));
}

#[test]
fn test_hybrid_session_creation() {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
    let buffer_pool = BufferPool::new(1024, 4);
    let router = Arc::new(BackendSelector::new());

    // Test hybrid mode session creation
    let session = ClientSession::new_with_router(
        addr,
        buffer_pool.clone(),
        router.clone(),
        RoutingMode::Hybrid,
    );

    assert!(session.is_per_command_routing());
    assert_eq!(session.routing_mode, RoutingMode::Hybrid);
    assert_eq!(session.mode, SessionMode::PerCommand);
}

#[test]
fn test_routing_mode_configurations() {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
    let buffer_pool = BufferPool::new(1024, 4);
    let router = Arc::new(BackendSelector::new());

    // Test Standard mode
    let session = ClientSession::new_with_router(
        addr,
        buffer_pool.clone(),
        router.clone(),
        RoutingMode::Standard,
    );
    // Standard mode still has per-command routing capability (router is present)
    assert!(session.is_per_command_routing());
    assert_eq!(session.routing_mode, RoutingMode::Standard);

    // Test PerCommand mode
    let session = ClientSession::new_with_router(
        addr,
        buffer_pool.clone(),
        router.clone(),
        RoutingMode::PerCommand,
    );
    assert!(session.is_per_command_routing());
    assert_eq!(session.routing_mode, RoutingMode::PerCommand);
    assert_eq!(session.mode, SessionMode::PerCommand);

    // Test Hybrid mode
    let session = ClientSession::new_with_router(
        addr,
        buffer_pool.clone(),
        router.clone(),
        RoutingMode::Hybrid,
    );
    assert!(session.is_per_command_routing());
    assert_eq!(session.routing_mode, RoutingMode::Hybrid);
    assert_eq!(session.mode, SessionMode::PerCommand);
}

#[test]
fn test_hybrid_mode_initial_state() {
    // Test that hybrid mode starts in per-command mode
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
    let buffer_pool = BufferPool::new(1024, 4);
    let router = Arc::new(BackendSelector::new());

    let session = ClientSession::new_with_router(addr, buffer_pool, router, RoutingMode::Hybrid);

    // Initially should be in per-command mode
    assert_eq!(session.mode, SessionMode::PerCommand);
    assert_eq!(session.routing_mode, RoutingMode::Hybrid);
    assert!(session.is_per_command_routing());
}

#[test]
fn test_is_per_command_routing_logic() {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
    let buffer_pool = BufferPool::new(1024, 4);
    let router = Arc::new(BackendSelector::new());

    // Standard mode has router capability (can do per-command routing)
    let session = ClientSession::new_with_router(
        addr,
        buffer_pool.clone(),
        router.clone(),
        RoutingMode::Standard,
    );
    assert!(session.is_per_command_routing());

    // PerCommand mode should be per-command routing
    let session = ClientSession::new_with_router(
        addr,
        buffer_pool.clone(),
        router.clone(),
        RoutingMode::PerCommand,
    );
    assert!(session.is_per_command_routing());

    // Hybrid mode should be per-command routing (initially)
    let session = ClientSession::new_with_router(
        addr,
        buffer_pool.clone(),
        router.clone(),
        RoutingMode::Hybrid,
    );
    assert!(session.is_per_command_routing());

    // Session without router should not be per-command routing
    let session = ClientSession::new(addr, buffer_pool);
    assert!(!session.is_per_command_routing());
}
