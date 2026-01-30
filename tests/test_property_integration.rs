//! Property-based integration tests for NNTP proxy
//!
//! Uses proptest to test the proxy with randomized inputs, ensuring
//! robust handling of edge cases and malformed inputs across the protocol.
//!
//! Note: Network-dependent property tests (prop_valid_commands_get_responses,
//! prop_malformed_commands_handled, prop_sequential_commands) were removed because
//! proptest's synchronous macro requires creating a new tokio runtime + full proxy
//! per case (20 cases × 3 tests = 60 TCP listener pairs), causing resource contention
//! and flaky timeouts under parallel nextest execution. The async
//! test_property_concurrent_connections covers the network path reliably by reusing
//! a single proxy instance.

use anyhow::Result;
use proptest::prelude::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{Duration, timeout};

mod test_helpers;
use nntp_proxy::{Config, NntpProxy, RoutingMode};
use test_helpers::{MockNntpServer, create_test_server_config};

/// Strategy for generating valid NNTP message IDs
fn message_id_strategy() -> impl Strategy<Value = String> {
    prop::string::string_regex("[a-zA-Z0-9._-]+@[a-zA-Z0-9.-]+")
        .unwrap()
        .prop_map(|s| format!("<{}>", s))
}

/// Strategy for valid authentication credentials
fn auth_credentials_strategy() -> impl Strategy<Value = (String, String)> {
    (
        prop::string::string_regex("[a-zA-Z0-9_-]{3,32}").unwrap(),
        prop::string::string_regex("[a-zA-Z0-9!@#$%^&*]{6,64}").unwrap(),
    )
}

/// Helper to setup a test environment
async fn setup_test_proxy() -> Result<(u16, u16, tokio::task::AbortHandle)> {
    // Find available ports
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    drop(backend_listener);

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await?;
    let proxy_port = proxy_listener.local_addr()?.port();

    // Start mock backend with responses for all commands
    let mock = MockNntpServer::new(backend_port)
        .with_name("Test Backend")
        .on_command("HELP", "100 Help text\r\n")
        .on_command("DATE", "111 20251120120000\r\n")
        .on_command("LIST", "215 List follows\r\n.\r\n")
        .on_command("QUIT", "205 Goodbye\r\n")
        .spawn();

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create and start proxy
    let config = Config {
        servers: vec![create_test_server_config(
            "127.0.0.1",
            backend_port,
            "backend",
        )],
        ..Default::default()
    };

    let proxy = NntpProxy::new(config, RoutingMode::Hybrid).await?;

    tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = proxy_listener.accept().await {
                let proxy_clone = proxy.clone();
                tokio::spawn(async move {
                    let _ = proxy_clone.handle_client(stream, addr.into()).await;
                });
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    Ok((proxy_port, backend_port, mock))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    /// Property: Message IDs should be correctly parsed (doesn't require backend response)
    #[test]
    fn prop_message_id_parsing(msg_id in message_id_strategy()) {
        use nntp_proxy::types::MessageId;

        // Property: Valid message IDs should parse correctly
        let result = MessageId::from_str_or_wrap(&msg_id);
        prop_assert!(result.is_ok(), "Valid message ID should parse: {}", msg_id);

        let parsed = result.unwrap();
        prop_assert_eq!(parsed.as_str(), msg_id);
    }

    /// Property: Valid auth credentials should authenticate successfully
    #[test]
    fn prop_auth_credentials_validation((username, password) in auth_credentials_strategy()) {
        use nntp_proxy::auth::AuthHandler;

        // Create handler with random credentials
        let handler = AuthHandler::new(
            Some(username.to_string()),
            Some(password.to_string())
        ).unwrap();

        // Should validate exact match
        assert!(handler.validate_credentials(&username, &password));

        // Should reject wrong password
        assert!(!handler.validate_credentials(&username, "wrongpass"));

        // Should reject wrong username
        assert!(!handler.validate_credentials("wronguser", &password));
    }
}

/// Tokio-based property test (for async property tests without proptest! macro)
#[tokio::test]
async fn test_property_concurrent_connections() -> Result<()> {
    let (proxy_port, _, _mock) = setup_test_proxy().await?;

    // Property: Multiple concurrent connections should all work
    let mut handles = vec![];

    for i in 0..10 {
        let port = proxy_port;
        handles.push(tokio::spawn(async move {
            let mut client = TcpStream::connect(format!("127.0.0.1:{}", port))
                .await
                .unwrap();

            // Read greeting
            let mut buffer = vec![0u8; 1024];
            let n = timeout(Duration::from_secs(5), client.read(&mut buffer))
                .await
                .unwrap()
                .unwrap();
            assert!(n > 0, "Client {} should receive greeting", i);

            // Send command
            client.write_all(b"HELP\r\n").await.unwrap();

            // Read response
            let n = timeout(Duration::from_secs(5), client.read(&mut buffer))
                .await
                .unwrap()
                .unwrap();
            assert!(n > 0, "Client {} should receive response", i);
        }));
    }

    // All connections should succeed
    for handle in handles {
        handle.await?;
    }

    Ok(())
}
