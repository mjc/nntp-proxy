//! Tests for handle_client() routing mode dispatch
//!
//! These tests verify that the refactored single entry point correctly
//! dispatches to the appropriate handler based on routing mode.

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{Duration, timeout};

mod config_helpers;
mod test_helpers;

use config_helpers::create_test_server_config;
use nntp_proxy::{Config, NntpProxy, RoutingMode};
use test_helpers::MockNntpServer;

async fn setup_proxy(routing_mode: RoutingMode) -> Result<(u16, u16, tokio::task::AbortHandle)> {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    drop(backend_listener);

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await?;
    let proxy_port = proxy_listener.local_addr()?.port();

    let mock = MockNntpServer::new(backend_port)
        .with_name("Test Backend")
        .on_command("HELP", "100 Help text\r\n.\r\n")
        .on_command("DATE", "111 20251120120000\r\n")
        .on_command("LIST", "215 List follows\r\n.\r\n")
        .on_command(
            "ARTICLE <test@example.com>",
            "220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n",
        )
        .on_command("GROUP alt.test", "211 100 1 100 alt.test\r\n")
        .on_command("NEXT", "223 1 <msg@example.com>\r\n")
        .on_command("LAST", "223 1 <msg@example.com>\r\n")
        .on_command("XOVER 1-10", "224 Overview follows\r\n1\tTest\r\n.\r\n")
        .on_command("QUIT", "205 Goodbye\r\n")
        .spawn();

    tokio::time::sleep(Duration::from_millis(100)).await;

    let config = Config {
        servers: vec![create_test_server_config(
            "127.0.0.1",
            backend_port,
            "backend",
        )],
        ..Default::default()
    };

    let proxy = NntpProxy::new(config, routing_mode)?;

    tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = proxy_listener.accept().await {
                let proxy_clone = proxy.clone();
                tokio::spawn(async move {
                    let _ = proxy_clone.handle_client(stream, addr).await;
                });
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    Ok((proxy_port, backend_port, mock))
}

async fn send_command(
    client: &mut TcpStream,
    command: &str,
    buffer: &mut Vec<u8>,
) -> Result<String> {
    client.write_all(command.as_bytes()).await?;
    let n = timeout(Duration::from_secs(2), client.read(buffer)).await??;
    Ok(String::from_utf8_lossy(&buffer[..n]).to_string())
}

/// Test that handle_client() correctly dispatches to Stateful handler
#[tokio::test]
async fn test_handle_client_dispatches_to_stateful() -> Result<()> {
    let (proxy_port, _, _mock) = setup_proxy(RoutingMode::Stateful).await?;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;
    let mut buffer = vec![0u8; 8192];

    // Read greeting
    let greeting = send_command(&mut client, "", &mut buffer).await?;
    assert!(greeting.contains("200"), "Should get greeting");

    // Stateful mode should handle GROUP command
    let response = send_command(&mut client, "GROUP alt.test\r\n", &mut buffer).await?;
    assert!(
        response.contains("211"),
        "Stateful mode should handle GROUP"
    );

    // Should maintain state - NEXT command requires GROUP first
    let response = send_command(&mut client, "NEXT\r\n", &mut buffer).await?;
    assert!(
        response.starts_with("223") || response.starts_with("2"),
        "Should handle NEXT after GROUP"
    );

    Ok(())
}

/// Test that handle_client() correctly dispatches to PerCommand handler
#[tokio::test]
async fn test_handle_client_dispatches_to_per_command() -> Result<()> {
    let (proxy_port, _, _mock) = setup_proxy(RoutingMode::PerCommand).await?;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;
    let mut buffer = vec![0u8; 8192];

    // Read greeting
    let _ = send_command(&mut client, "", &mut buffer).await?;

    // Per-command mode should handle stateless commands
    let response = send_command(&mut client, "DATE\r\n", &mut buffer).await?;
    assert!(response.contains("111"), "Should handle DATE");

    let response = send_command(&mut client, "HELP\r\n", &mut buffer).await?;
    assert!(response.contains("100"), "Should handle HELP");

    // Each command should route independently (round-robin)
    let response = send_command(&mut client, "LIST\r\n", &mut buffer).await?;
    assert!(response.contains("215"), "Should handle LIST");

    Ok(())
}

/// Test that handle_client() correctly dispatches to Hybrid handler
#[tokio::test]
async fn test_handle_client_dispatches_to_hybrid() -> Result<()> {
    let (proxy_port, _, _mock) = setup_proxy(RoutingMode::Hybrid).await?;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;
    let mut buffer = vec![0u8; 8192];

    // Read greeting
    let _ = send_command(&mut client, "", &mut buffer).await?;

    // Should start in per-command mode for stateless commands
    let response = send_command(&mut client, "DATE\r\n", &mut buffer).await?;
    assert!(
        response.contains("111"),
        "Should handle DATE in per-command mode"
    );

    let response = send_command(&mut client, "HELP\r\n", &mut buffer).await?;
    assert!(
        response.contains("100"),
        "Should handle HELP in per-command mode"
    );

    // Stateful command should trigger switch
    let response = send_command(&mut client, "GROUP alt.test\r\n", &mut buffer).await?;
    assert!(
        response.contains("211"),
        "Should switch to stateful and handle GROUP"
    );

    // Should remain stateful for subsequent commands
    let response = send_command(&mut client, "NEXT\r\n", &mut buffer).await?;
    assert!(
        response.starts_with("223") || response.starts_with("2"),
        "Should remain in stateful mode"
    );

    Ok(())
}

/// Test that all routing modes handle message-ID based commands
#[tokio::test]
async fn test_handle_client_message_id_commands() -> Result<()> {
    for mode in [
        RoutingMode::Stateful,
        RoutingMode::PerCommand,
        RoutingMode::Hybrid,
    ] {
        let (proxy_port, _, _mock) = setup_proxy(mode).await?;

        let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;
        let mut buffer = vec![0u8; 8192];

        // Read greeting
        let _ = send_command(&mut client, "", &mut buffer).await?;

        // All modes should handle ARTICLE with message-ID
        let response =
            send_command(&mut client, "ARTICLE <test@example.com>\r\n", &mut buffer).await?;
        assert!(
            response.contains("220"),
            "Mode {:?} should handle ARTICLE by message-ID",
            mode
        );
    }

    Ok(())
}

/// Test that per-command mode rejects stateful commands without message-ID
#[tokio::test]
async fn test_per_command_mode_rejects_stateful_commands() -> Result<()> {
    let (proxy_port, _, _mock) = setup_proxy(RoutingMode::PerCommand).await?;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;
    let mut buffer = vec![0u8; 8192];

    // Read greeting
    let _ = send_command(&mut client, "", &mut buffer).await?;

    // GROUP command should be rejected (stateful without message-ID)
    let response = send_command(&mut client, "GROUP alt.test\r\n", &mut buffer).await?;
    assert!(
        response.contains("502")
            || response.contains("503")
            || response.contains("not implemented")
            || response.contains("not supported"),
        "Should reject GROUP in per-command mode, got: {}",
        response
    );

    // NEXT should be rejected (stateful)
    let response = send_command(&mut client, "NEXT\r\n", &mut buffer).await?;
    assert!(
        response.contains("502")
            || response.contains("503")
            || response.contains("not implemented")
            || response.contains("not supported"),
        "Should reject NEXT in per-command mode"
    );

    Ok(())
}

/// Test concurrent clients with different routing modes
#[tokio::test]
async fn test_concurrent_clients_stateful_mode() -> Result<()> {
    let (proxy_port, _, _mock) = setup_proxy(RoutingMode::Stateful).await?;

    let mut clients = Vec::new();
    for _ in 0..3 {
        let client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;
        clients.push(client);
    }

    // All clients should get greetings
    for client in &mut clients {
        let mut buffer = vec![0u8; 8192];
        let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
        assert!(n > 0, "Should receive greeting");
    }

    // All clients should handle commands independently
    for (i, client) in clients.iter_mut().enumerate() {
        let mut buffer = vec![0u8; 8192];
        let response = send_command(client, "HELP\r\n", &mut buffer).await?;
        assert!(
            response.contains("100"),
            "Client {} should get HELP response",
            i
        );
    }

    Ok(())
}

/// Test concurrent clients with per-command mode (connection pooling)
#[tokio::test]
async fn test_concurrent_clients_per_command_mode() -> Result<()> {
    let (proxy_port, _, _mock) = setup_proxy(RoutingMode::PerCommand).await?;

    let mut clients = Vec::new();
    for _ in 0..5 {
        let client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;
        clients.push(client);
    }

    // All clients should get greetings
    for client in &mut clients {
        let mut buffer = vec![0u8; 8192];
        let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
        assert!(n > 0, "Should receive greeting");
    }

    // Send commands from all clients simultaneously - should share backend pool
    let handles: Vec<_> = clients
        .into_iter()
        .enumerate()
        .map(|(i, mut client)| {
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 8192];
                let cmd = if i % 2 == 0 { "DATE\r\n" } else { "HELP\r\n" };
                send_command(&mut client, cmd, &mut buffer).await
            })
        })
        .collect();

    for handle in handles {
        let response = handle.await??;
        assert!(
            response.contains("111") || response.contains("100"),
            "Should get valid response"
        );
    }

    Ok(())
}

/// Test hybrid mode with concurrent clients
#[tokio::test]
async fn test_concurrent_clients_hybrid_mode() -> Result<()> {
    let (proxy_port, _, _mock) = setup_proxy(RoutingMode::Hybrid).await?;

    // Client 1: Stays in per-command mode
    let mut client1 = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;
    let mut buffer1 = vec![0u8; 8192];
    let _ = send_command(&mut client1, "", &mut buffer1).await?;

    // Client 2: Switches to stateful mode
    let mut client2 = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;
    let mut buffer2 = vec![0u8; 8192];
    let _ = send_command(&mut client2, "", &mut buffer2).await?;

    // Client 1 sends stateless commands
    let response1 = send_command(&mut client1, "DATE\r\n", &mut buffer1).await?;
    assert!(response1.contains("111"));

    // Client 2 sends stateful command (triggers switch)
    let response2 = send_command(&mut client2, "GROUP alt.test\r\n", &mut buffer2).await?;
    assert!(response2.contains("211"));

    // Client 1 should still be in per-command mode
    let response1 = send_command(&mut client1, "HELP\r\n", &mut buffer1).await?;
    assert!(response1.contains("100"));

    // Client 2 should be in stateful mode
    let response2 = send_command(&mut client2, "NEXT\r\n", &mut buffer2).await?;
    assert!(response2.starts_with("223") || response2.starts_with("2"));

    Ok(())
}

/// Test error recovery - client disconnect mid-session
#[tokio::test]
async fn test_client_disconnect_recovery() -> Result<()> {
    let (proxy_port, _, _mock) = setup_proxy(RoutingMode::Hybrid).await?;

    // Connect and immediately disconnect
    {
        let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;
        let mut buffer = vec![0u8; 8192];
        let _ = send_command(&mut client, "", &mut buffer).await?;
        // Client dropped here
    }

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Should still accept new connections
    let mut client2 = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;
    let mut buffer = vec![0u8; 8192];
    let greeting = send_command(&mut client2, "", &mut buffer).await?;
    assert!(greeting.contains("200"));

    let response = send_command(&mut client2, "HELP\r\n", &mut buffer).await?;
    assert!(response.contains("100"));

    Ok(())
}

/// Test QUIT command handling in all modes
#[tokio::test]
async fn test_quit_command_all_modes() -> Result<()> {
    for mode in [
        RoutingMode::Stateful,
        RoutingMode::PerCommand,
        RoutingMode::Hybrid,
    ] {
        let (proxy_port, _, _mock) = setup_proxy(mode).await?;

        let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;
        let mut buffer = vec![0u8; 8192];

        // Read greeting
        let _ = send_command(&mut client, "", &mut buffer).await?;

        // Send QUIT
        let response = send_command(&mut client, "QUIT\r\n", &mut buffer).await?;
        assert!(
            response.contains("205") || response.contains("Goodbye"),
            "Mode {:?} should handle QUIT",
            mode
        );

        // Connection should close
        let result = timeout(Duration::from_millis(500), client.read(&mut buffer)).await;
        match result {
            Ok(Ok(0)) => {} // Connection closed - expected
            Ok(Ok(_)) => panic!("Mode {:?}: Should not receive data after QUIT", mode),
            Ok(Err(e)) => eprintln!("Mode {:?}: Read error after QUIT (expected): {}", mode, e),
            Err(_) => {} // Timeout is also acceptable
        }
    }

    Ok(())
}

/// Test rapid sequential commands (stress test for dispatch)
#[tokio::test]
async fn test_rapid_commands_stateful() -> Result<()> {
    let (proxy_port, _, _mock) = setup_proxy(RoutingMode::Stateful).await?;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;
    let mut buffer = vec![0u8; 8192];

    // Read greeting
    let _ = send_command(&mut client, "", &mut buffer).await?;

    // Send 10 rapid commands
    for i in 0..10 {
        let cmd = if i % 2 == 0 { "DATE\r\n" } else { "HELP\r\n" };
        let response = send_command(&mut client, cmd, &mut buffer).await?;
        assert!(
            response.contains("111") || response.contains("100"),
            "Command {} should get response",
            i
        );
    }

    Ok(())
}

/// Test rapid sequential commands in per-command mode (pool stress test)
#[tokio::test]
async fn test_rapid_commands_per_command() -> Result<()> {
    let (proxy_port, _, _mock) = setup_proxy(RoutingMode::PerCommand).await?;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;
    let mut buffer = vec![0u8; 8192];

    // Read greeting
    let _ = send_command(&mut client, "", &mut buffer).await?;

    // Send 20 rapid commands to stress connection pool
    for i in 0..20 {
        let cmd = match i % 3 {
            0 => "DATE\r\n",
            1 => "HELP\r\n",
            _ => "LIST\r\n",
        };
        let response = send_command(&mut client, cmd, &mut buffer).await?;
        assert!(response.len() > 0, "Command {} should get response", i);
    }

    Ok(())
}

/// Test hybrid mode switching behavior under rapid commands
#[tokio::test]
async fn test_rapid_commands_hybrid_with_switch() -> Result<()> {
    let (proxy_port, _, _mock) = setup_proxy(RoutingMode::Hybrid).await?;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;
    let mut buffer = vec![0u8; 8192];

    // Read greeting
    let _ = send_command(&mut client, "", &mut buffer).await?;

    // Start with stateless commands
    for _ in 0..5 {
        let _ = send_command(&mut client, "DATE\r\n", &mut buffer).await?;
    }

    // Trigger switch with stateful command
    let response = send_command(&mut client, "GROUP alt.test\r\n", &mut buffer).await?;
    assert!(response.contains("211"), "Should switch to stateful");

    // Continue with more commands in stateful mode
    for _ in 0..5 {
        let response = send_command(&mut client, "LIST\r\n", &mut buffer).await?;
        assert!(response.contains("215"), "Should remain stateful");
    }

    Ok(())
}

/// Test that dispatch happens BEFORE any command processing
#[tokio::test]
async fn test_dispatch_order() -> Result<()> {
    // This test ensures handle_client() dispatches FIRST, then processes commands
    // If dispatch happens after, we'd see errors or wrong behavior

    for mode in [
        RoutingMode::Stateful,
        RoutingMode::PerCommand,
        RoutingMode::Hybrid,
    ] {
        let (proxy_port, _, _mock) = setup_proxy(mode).await?;

        let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;
        let mut buffer = vec![0u8; 8192];

        // First thing client receives should be greeting (dispatch happened correctly)
        let n = timeout(Duration::from_secs(1), client.read(&mut buffer)).await??;
        let greeting = String::from_utf8_lossy(&buffer[..n]);
        assert!(
            greeting.starts_with("200") || greeting.starts_with("201"),
            "Mode {:?}: First message should be greeting, got: {}",
            mode,
            greeting
        );
    }

    Ok(())
}
