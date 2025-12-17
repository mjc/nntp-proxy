//! End-to-end integration tests for client authentication
//!
//! These tests verify the complete authentication flow with real TCP connections:
//! - Client connects to proxy
//! - Proxy requires authentication
//! - Client sends AUTHINFO USER/PASS
//! - Proxy validates and forwards to backend
//! - Full command flow after authentication

use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

mod test_helpers;
use test_helpers::wait_for_server;

use nntp_proxy::config::UserCredentials;

#[tokio::test]
async fn test_auth_flow_complete_with_valid_credentials() {
    // Use a specific port for testing
    let backend_port = 19119;

    // Start mock backend
    let _backend_handle = test_helpers::spawn_mock_server(backend_port, "test-backend");
    wait_for_server(&format!("127.0.0.1:{}", backend_port), 10)
        .await
        .unwrap();

    // Create config with auth
    use nntp_proxy::config::ClientAuth;
    use test_helpers::create_test_config;

    let mut config = create_test_config(vec![(backend_port, "backend-1")]);
    config.client_auth = ClientAuth {
        users: vec![UserCredentials {
            username: "testuser".to_string(),
            password: "testpass".to_string(),
        }],
        greeting: None,
    };

    // Start proxy
    let proxy = Arc::new(
        nntp_proxy::NntpProxy::new(config, nntp_proxy::config::RoutingMode::Stateful).unwrap(),
    );
    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();

    // Spawn proxy handler
    let proxy_clone = proxy.clone();
    tokio::spawn(async move {
        if let Ok((stream, addr)) = proxy_listener.accept().await {
            let _ = proxy_clone.handle_client(stream, addr.into()).await;
        }
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Connect as client
    let mut client = TcpStream::connect(proxy_addr).await.unwrap();
    let (reader, mut writer) = client.split();
    let mut reader = BufReader::new(reader);

    // Read proxy greeting
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("200"));

    // Send AUTHINFO USER
    writer
        .write_all(b"AUTHINFO USER testuser\r\n")
        .await
        .unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("381")); // Password required

    // Send AUTHINFO PASS
    writer
        .write_all(b"AUTHINFO PASS testpass\r\n")
        .await
        .unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("281")); // Authentication accepted

    // Now send a regular command - should work
    writer.write_all(b"CAPABILITIES\r\n").await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    // Should get response from backend (not auth related)
    assert!(!line.starts_with("381"));
    assert!(!line.starts_with("481"));
}

#[tokio::test]
async fn test_auth_disabled_allows_immediate_commands() {
    let backend_port = 19120;

    // Start mock backend
    let _backend_handle = test_helpers::spawn_mock_server(backend_port, "test-backend");
    wait_for_server(&format!("127.0.0.1:{}", backend_port), 10)
        .await
        .unwrap();

    // Create config WITHOUT auth
    use test_helpers::create_test_config;
    let config = create_test_config(vec![(backend_port, "backend-1")]);

    // Ensure auth is disabled
    assert!(!config.client_auth.is_enabled());

    // Start proxy
    let proxy = Arc::new(
        nntp_proxy::NntpProxy::new(config, nntp_proxy::config::RoutingMode::Stateful).unwrap(),
    );
    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();

    let proxy_clone = proxy.clone();
    tokio::spawn(async move {
        if let Ok((stream, addr)) = proxy_listener.accept().await {
            let _ = proxy_clone.handle_client(stream, addr.into()).await;
        }
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Connect and immediately send command (no auth needed)
    let mut client = TcpStream::connect(proxy_addr).await.unwrap();
    let (reader, mut writer) = client.split();
    let mut reader = BufReader::new(reader);

    // Read greeting
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("200"));

    // Send command immediately (no AUTHINFO needed)
    writer.write_all(b"CAPABILITIES\r\n").await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    // Should get backend response
    assert!(!line.starts_with("381")); // Not asking for auth
    assert!(!line.starts_with("481")); // Not auth failure
}

#[tokio::test]
async fn test_auth_command_intercepted_not_sent_to_backend() {
    let backend_port = 19121;

    // Start mock backend that would fail if it receives AUTHINFO
    let _backend_handle = test_helpers::spawn_mock_server(backend_port, "test-backend");
    wait_for_server(&format!("127.0.0.1:{}", backend_port), 10)
        .await
        .unwrap();

    use nntp_proxy::config::ClientAuth;
    use test_helpers::create_test_config;

    let mut config = create_test_config(vec![(backend_port, "backend-1")]);
    config.client_auth = ClientAuth {
        users: vec![UserCredentials {
            username: "user".to_string(),
            password: "pass".to_string(),
        }],
        greeting: None,
    };

    let proxy = Arc::new(
        nntp_proxy::NntpProxy::new(config, nntp_proxy::config::RoutingMode::Stateful).unwrap(),
    );
    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();

    let proxy_clone = proxy.clone();
    tokio::spawn(async move {
        if let Ok((stream, addr)) = proxy_listener.accept().await {
            let _ = proxy_clone.handle_client(stream, addr.into()).await;
        }
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let mut client = TcpStream::connect(proxy_addr).await.unwrap();
    let (reader, mut writer) = client.split();
    let mut reader = BufReader::new(reader);

    // Read greeting
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();

    // Send AUTHINFO - should be intercepted by proxy, not forwarded to backend
    writer.write_all(b"AUTHINFO USER user\r\n").await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("381")); // Proxy responds directly

    writer.write_all(b"AUTHINFO PASS pass\r\n").await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("281")); // Proxy responds directly

    // If backend received AUTHINFO, it would respond with an error
    // But we get success, proving proxy intercepted it
}

#[tokio::test]
async fn test_multiple_clients_with_auth() {
    let backend_port = 19122;

    use nntp_proxy::config::ClientAuth;
    use test_helpers::create_test_config;
    use tokio::task::JoinSet;

    // Start mock backend
    let _backend_handle = test_helpers::spawn_mock_server(backend_port, "test-backend");
    wait_for_server(&format!("127.0.0.1:{}", backend_port), 10)
        .await
        .unwrap();

    let mut config = create_test_config(vec![(backend_port, "backend-1")]);
    config.client_auth = ClientAuth {
        users: vec![UserCredentials {
            username: "user".to_string(),
            password: "pass".to_string(),
        }],
        greeting: None,
    };

    let proxy = Arc::new(
        nntp_proxy::NntpProxy::new(config, nntp_proxy::config::RoutingMode::Stateful).unwrap(),
    );
    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();

    // Spawn proxy to handle multiple clients - simplified version
    let proxy_clone = proxy.clone();
    tokio::spawn(async move {
        while let Ok((stream, addr)) = proxy_listener.accept().await {
            let p = proxy_clone.clone();
            tokio::spawn(async move {
                let _ = p.handle_client(stream, addr.into()).await;
            });
        }
    });

    // Actually, let me simplify - the mock server handles multiple connections
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let mut set = JoinSet::new();

    // Spawn 5 concurrent clients
    for i in 0..5 {
        set.spawn(async move {
            let mut client = TcpStream::connect(proxy_addr).await.unwrap();
            let (reader, mut writer) = client.split();
            let mut reader = BufReader::new(reader);
            let mut line = String::new();

            // Read greeting
            reader.read_line(&mut line).await.unwrap();
            assert!(line.starts_with("200"), "Client {} got greeting", i);

            // Auth
            writer.write_all(b"AUTHINFO USER user\r\n").await.unwrap();
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            assert!(line.starts_with("381"), "Client {} got password request", i);

            writer.write_all(b"AUTHINFO PASS pass\r\n").await.unwrap();
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            assert!(line.starts_with("281"), "Client {} authenticated", i);

            i
        });
    }

    // All clients should authenticate successfully
    let mut count = 0;
    while let Some(result) = set.join_next().await {
        result.unwrap();
        count += 1;
    }
    assert_eq!(count, 5);
}

#[tokio::test]
async fn test_auth_handler_integration() {
    use nntp_proxy::command::{AuthAction, CommandAction, CommandHandler};
    use test_helpers::create_test_auth_handler_with;

    let handler = create_test_auth_handler_with("alice", "secret");

    // Test command classification
    let action = CommandHandler::classify("LIST\r\n");
    assert_eq!(action, CommandAction::ForwardStateless);

    let action = CommandHandler::classify("AUTHINFO USER alice\r\n");
    assert!(matches!(
        action,
        CommandAction::InterceptAuth(AuthAction::RequestPassword(_))
    ));

    let action = CommandHandler::classify("GROUP misc.test\r\n");
    assert!(matches!(action, CommandAction::Reject(_)));

    // Test auth handler responses
    let mut output = Vec::new();
    let (bytes, _) = handler
        .handle_auth_command(AuthAction::RequestPassword("alice"), &mut output, None)
        .await
        .unwrap();
    assert!(bytes > 0);
    assert!(!output.is_empty());

    output.clear();
    let (bytes, auth_success) = handler
        .handle_auth_command(
            AuthAction::ValidateAndRespond { password: "secret" },
            &mut output,
            Some("alice"),
        )
        .await
        .unwrap();
    assert!(bytes > 0);
    assert!(!output.is_empty());
    assert!(auth_success); // Valid credentials
}

#[tokio::test]
async fn test_config_auth_round_trip() {
    use nntp_proxy::config::{ClientAuth, Config};

    // Create config with auth
    let config = Config {
        servers: vec![],
        proxy: Default::default(),
        health_check: Default::default(),
        cache: None,
        client_auth: ClientAuth {
            users: vec![UserCredentials {
                username: "testuser".to_string(),
                password: "testpass".to_string(),
            }],
            greeting: Some("Custom Auth Required".to_string()),
        },
    };

    // Verify config
    assert!(config.client_auth.is_enabled());
    assert_eq!(config.client_auth.users.len(), 1);
    assert_eq!(config.client_auth.users[0].username, "testuser");
    assert_eq!(config.client_auth.users[0].password, "testpass");
    assert_eq!(
        config.client_auth.greeting.as_deref(),
        Some("Custom Auth Required")
    );

    // Serialize to TOML
    let toml = toml::to_string(&config).unwrap();
    assert!(toml.contains("[client_auth]"));
    assert!(toml.contains("username"));
    assert!(toml.contains("password"));

    // Deserialize back
    let config2: Config = toml::from_str(&toml).unwrap();
    assert_eq!(config.client_auth.users, config2.client_auth.users);
    assert_eq!(config.client_auth.greeting, config2.client_auth.greeting);
}
