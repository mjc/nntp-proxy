//! Property-based integration tests for NNTP proxy
//!
//! Uses proptest to test the proxy with randomized inputs, ensuring
//! robust handling of edge cases and malformed inputs across the protocol.

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

/// Strategy for generating valid NNTP commands (limited to what mock supports)
fn nntp_command_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("HELP".to_string()),
        Just("DATE".to_string()),
        Just("LIST".to_string()),
    ]
}

/// Strategy for generating potentially invalid commands (protocol fuzzing)
fn fuzzy_command_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        // Valid commands
        nntp_command_strategy(),
        // Empty/whitespace
        Just("".to_string()),
        Just("   ".to_string()),
        // Invalid commands
        prop::string::string_regex("[A-Z]{3,10}").unwrap(),
        // Malformed message IDs
        Just("ARTICLE <>".to_string()),
        Just("ARTICLE <".to_string()),
        Just("ARTICLE >".to_string()),
        Just("ARTICLE no-brackets".to_string()),
        // Case variations
        Just("help".to_string()),
        Just("HeLp".to_string()),
        Just("article <test@example>".to_string()),
    ]
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

    /// Property: All valid NNTP commands should receive responses
    #[test]
    fn prop_valid_commands_get_responses(command in nntp_command_strategy()) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (proxy_port, _, _mock) = setup_test_proxy().await.unwrap();

            let mut client = timeout(
                Duration::from_secs(2),
                TcpStream::connect(format!("127.0.0.1:{}", proxy_port))
            ).await.unwrap().unwrap();

            // Read greeting
            let mut buffer = vec![0u8; 4096];
            let n = timeout(Duration::from_secs(1), client.read(&mut buffer))
                .await.unwrap().unwrap();
            assert!(n > 0, "Should receive greeting");

            // Send command
            let cmd = format!("{}\r\n", command);
            timeout(Duration::from_secs(1), client.write_all(cmd.as_bytes()))
                .await.unwrap().unwrap();

            // Should receive response (even if error)
            let n = timeout(Duration::from_secs(1), client.read(&mut buffer))
                .await.unwrap().unwrap();
            assert!(n > 0, "Should receive response for command: {}", command);

            // Response should be valid NNTP format (starts with 3-digit code)
            let response = String::from_utf8_lossy(&buffer[..n]);
            let first_line = response.lines().next().unwrap_or("");
            assert!(
                first_line.len() >= 3 && first_line[0..3].chars().all(|c| c.is_ascii_digit()),
                "Invalid response format for '{}': {}",
                command,
                first_line
            );
        });
    }

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

    /// Property: Proxy should gracefully handle malformed commands
    #[test]
    fn prop_malformed_commands_handled(command in fuzzy_command_strategy()) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (proxy_port, _, _mock) = setup_test_proxy().await.unwrap();

            let mut client = timeout(
                Duration::from_secs(2),
                TcpStream::connect(format!("127.0.0.1:{}", proxy_port))
            ).await.unwrap().unwrap();

            // Read greeting
            let mut buffer = vec![0u8; 4096];
            let _ = timeout(Duration::from_secs(1), client.read(&mut buffer))
                .await.unwrap().unwrap();

            // Send potentially malformed command
            let cmd = format!("{}\r\n", command);
            let write_result = timeout(
                Duration::from_secs(1),
                client.write_all(cmd.as_bytes())
            ).await;

            // If write succeeds, should get a response (even if error)
            if write_result.is_ok() {
                let read_result = timeout(
                    Duration::from_secs(1),
                    client.read(&mut buffer)
                ).await;

                // Either we get a response, or the connection closes
                // (both are acceptable for malformed input)
                if let Ok(Ok(n)) = read_result
                    && n > 0
                {
                    let response = String::from_utf8_lossy(&buffer[..n]);
                    // Should be valid NNTP response format
                    let first_line = response.lines().next().unwrap_or("");
                    if first_line.len() >= 3 {
                        assert!(
                            first_line[0..3].chars().all(|c| c.is_ascii_digit()),
                            "Invalid response format: {}",
                            first_line
                        );
                    }
                }
            }
        });
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

    /// Property: Connection should survive multiple sequential commands
    #[test]
    fn prop_sequential_commands(commands in prop::collection::vec(nntp_command_strategy(), 3..8)) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (proxy_port, _, _mock) = setup_test_proxy().await.unwrap();

            let mut client = timeout(
                Duration::from_secs(2),
                TcpStream::connect(format!("127.0.0.1:{}", proxy_port))
            ).await.unwrap().unwrap();

            // Read greeting
            let mut buffer = vec![0u8; 4096];
            let _ = timeout(Duration::from_secs(1), client.read(&mut buffer))
                .await.unwrap().unwrap();

            // Send commands one at a time and read responses
            for cmd in &commands {
                let cmd_line = format!("{}\r\n", cmd);
                timeout(Duration::from_secs(1), client.write_all(cmd_line.as_bytes()))
                    .await.unwrap().unwrap();

                // Read response before sending next command
                let n = timeout(Duration::from_secs(2), client.read(&mut buffer))
                    .await.unwrap().unwrap();
                assert!(n > 0, "Should receive response for command: {}", cmd);
            }
        });
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
            let n = timeout(Duration::from_secs(1), client.read(&mut buffer))
                .await
                .unwrap()
                .unwrap();
            assert!(n > 0, "Client {} should receive greeting", i);

            // Send command
            client.write_all(b"HELP\r\n").await.unwrap();

            // Read response
            let n = timeout(Duration::from_secs(1), client.read(&mut buffer))
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
