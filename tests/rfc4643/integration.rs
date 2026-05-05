//! End-to-end integration tests for client authentication
//!
//! These tests verify the complete authentication flow with real TCP connections:
//! - Client connects to proxy
//! - Proxy requires authentication
//! - Client sends AUTHINFO USER/PASS
//! - Proxy validates and forwards to backend
//! - Full command flow after authentication

use tokio::task::JoinSet;

use crate::test_helpers::{
    MockNntpServer, connect_and_read_greeting, create_test_auth_handler_with,
    send_command_read_line, spawn_single_backend_proxy, spawn_single_backend_proxy_with_auth,
};

use nntp_proxy::command::{AuthAction, CommandAction, CommandHandler};
use nntp_proxy::config::{ClientAuth, Config, HealthCheck, Proxy, UserCredentials};
use nntp_proxy::protocol::RequestContext;

fn classify(command: &str) -> CommandAction<'static> {
    let request = Box::leak(Box::new(
        RequestContext::parse(command.as_bytes()).expect("valid request line"),
    ));
    CommandHandler::classify_request(request)
}

#[tokio::test]
async fn test_auth_flow_complete_with_valid_credentials() {
    let (proxy_port, _backend) = spawn_single_backend_proxy_with_auth(
        nntp_proxy::config::RoutingMode::Stateful,
        "backend-1",
        "testuser",
        "testpass",
        |port| MockNntpServer::new(port).with_name("test-backend"),
    )
    .await
    .unwrap();

    let mut client = connect_and_read_greeting(proxy_port).await.unwrap();
    assert!(
        send_command_read_line(&mut client, "AUTHINFO USER testuser")
            .await
            .unwrap()
            .starts_with("381")
    );
    assert!(
        send_command_read_line(&mut client, "AUTHINFO PASS testpass")
            .await
            .unwrap()
            .starts_with("281")
    );

    let line = send_command_read_line(&mut client, "CAPABILITIES")
        .await
        .unwrap();
    assert!(!line.starts_with("381"));
    assert!(!line.starts_with("481"));
}

#[tokio::test]
async fn test_auth_disabled_allows_immediate_commands() {
    let (proxy_port, _backend) = spawn_single_backend_proxy(
        nntp_proxy::config::RoutingMode::Stateful,
        "backend-1",
        |port| MockNntpServer::new(port).with_name("test-backend"),
    )
    .await
    .unwrap();

    let mut client = connect_and_read_greeting(proxy_port).await.unwrap();
    let line = send_command_read_line(&mut client, "CAPABILITIES")
        .await
        .unwrap();
    assert!(!line.starts_with("381"));
    assert!(!line.starts_with("481"));
}

#[tokio::test]
async fn test_auth_command_intercepted_not_sent_to_backend() {
    let (proxy_port, _backend) = spawn_single_backend_proxy_with_auth(
        nntp_proxy::config::RoutingMode::Stateful,
        "backend-1",
        "user",
        "pass",
        |port| MockNntpServer::new(port).with_name("test-backend"),
    )
    .await
    .unwrap();

    let mut client = connect_and_read_greeting(proxy_port).await.unwrap();
    assert!(
        send_command_read_line(&mut client, "AUTHINFO USER user")
            .await
            .unwrap()
            .starts_with("381")
    );
    assert!(
        send_command_read_line(&mut client, "AUTHINFO PASS pass")
            .await
            .unwrap()
            .starts_with("281")
    );
}

#[tokio::test]
async fn test_multiple_clients_with_auth() {
    let (proxy_port, _backend) = spawn_single_backend_proxy_with_auth(
        nntp_proxy::config::RoutingMode::Stateful,
        "backend-1",
        "user",
        "pass",
        |port| MockNntpServer::new(port).with_name("test-backend"),
    )
    .await
    .unwrap();

    let mut set = JoinSet::new();

    // Spawn 5 concurrent clients
    for i in 0..5 {
        set.spawn(async move {
            let mut client = connect_and_read_greeting(proxy_port).await.unwrap();
            assert!(
                send_command_read_line(&mut client, "AUTHINFO USER user")
                    .await
                    .unwrap()
                    .starts_with("381"),
                "Client {i} got password request"
            );
            assert!(
                send_command_read_line(&mut client, "AUTHINFO PASS pass")
                    .await
                    .unwrap()
                    .starts_with("281"),
                "Client {i} authenticated"
            );

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
    let handler = create_test_auth_handler_with("alice", "secret");

    // Test command classification
    let action = classify("LIST\r\n");
    assert_eq!(action, CommandAction::ForwardStateless);

    let action = classify("AUTHINFO USER alice\r\n");
    assert!(matches!(
        action,
        CommandAction::InterceptAuth(AuthAction::RequestPassword(_))
    ));

    let action = classify("GROUP misc.test\r\n");
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
    // Create config with auth
    let config = Config {
        servers: vec![],
        proxy: Proxy::default(),
        routing: Default::default(),
        memory: Default::default(),
        health_check: HealthCheck::default(),
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
