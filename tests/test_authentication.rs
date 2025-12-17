//! Comprehensive integration tests for client authentication system
//!
//! Tests cover:
//! - Authentication enabled/disabled states
//! - Valid/invalid credentials
//! - Auth command sequence (USER -> PASS)
//! - Auth with different session modes (per-command, standard, hybrid)
//! - Edge cases and error conditions
//! - Security (credential redaction)

use std::sync::Arc;
use tokio::net::TcpListener;

mod config_helpers;
mod test_helpers;
use config_helpers::create_test_server_config;
use nntp_proxy::NntpProxy;
use nntp_proxy::auth::AuthHandler;
use nntp_proxy::config::{Config, RoutingMode};
use nntp_proxy::session::ClientSession;
use test_helpers::MockNntpServer;

/// Create test config with client auth enabled
fn create_config_with_auth(backend_ports: Vec<u16>, username: &str, password: &str) -> Config {
    use nntp_proxy::config::{ClientAuth, UserCredentials};
    Config {
        servers: backend_ports
            .into_iter()
            .map(|port| create_test_server_config("127.0.0.1", port, &format!("backend-{}", port)))
            .collect(),
        proxy: Default::default(),
        health_check: Default::default(),
        cache: None,
        client_auth: ClientAuth {
            users: vec![UserCredentials {
                username: username.to_string(),
                password: password.to_string(),
            }],
            greeting: None,
        },
    }
}

/// Spawn a mock NNTP backend server
async fn spawn_mock_backend() -> (u16, tokio::task::AbortHandle) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let handle = MockNntpServer::new(port)
        .with_name("Mock NNTP Server")
        .spawn();

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    (port, handle)
}

#[tokio::test]
async fn test_auth_handler_disabled_by_default() {
    let handler = AuthHandler::new(None, None).unwrap();
    assert!(!handler.is_enabled());
}

#[tokio::test]
async fn test_auth_handler_enabled_with_credentials() {
    let handler = AuthHandler::new(Some("user".to_string()), Some("pass".to_string())).unwrap();
    assert!(handler.is_enabled());
}

#[tokio::test]
async fn test_auth_handler_validates_correct_credentials() {
    let handler =
        AuthHandler::new(Some("alice".to_string()), Some("secret123".to_string())).unwrap();

    assert!(handler.validate_credentials("alice", "secret123"));
}

#[tokio::test]
async fn test_auth_handler_rejects_wrong_password() {
    let handler =
        AuthHandler::new(Some("alice".to_string()), Some("secret123".to_string())).unwrap();

    assert!(!handler.validate_credentials("alice", "wrongpass"));
}

#[tokio::test]
async fn test_auth_handler_rejects_wrong_username() {
    let handler =
        AuthHandler::new(Some("alice".to_string()), Some("secret123".to_string())).unwrap();

    assert!(!handler.validate_credentials("bob", "secret123"));
}

#[tokio::test]
async fn test_auth_handler_rejects_both_wrong() {
    let handler =
        AuthHandler::new(Some("alice".to_string()), Some("secret123".to_string())).unwrap();

    assert!(!handler.validate_credentials("bob", "wrongpass"));
}

#[tokio::test]
async fn test_auth_disabled_accepts_any_credentials() {
    let handler = AuthHandler::new(None, None).unwrap();

    assert!(handler.validate_credentials("anything", "works"));
    assert!(handler.validate_credentials("", ""));
    assert!(handler.validate_credentials("random", "stuff"));
}

#[tokio::test]
async fn test_auth_handler_partial_config_disabled() {
    // Only username, no password - should be disabled
    let handler1 = AuthHandler::new(Some("user".to_string()), None).unwrap();
    assert!(!handler1.is_enabled());

    // Only password, no username - should be disabled
    let handler2 = AuthHandler::new(None, Some("pass".to_string())).unwrap();
    assert!(!handler2.is_enabled());
}

#[tokio::test]
async fn test_auth_handler_empty_string_credentials() {
    // SECURITY: Empty credentials must be rejected, not silently disable auth
    let result = AuthHandler::new(Some("".to_string()), Some("".to_string()));
    assert!(
        result.is_err(),
        "Empty credentials should be rejected to prevent silent auth bypass"
    );
}

#[tokio::test]
async fn test_auth_handler_case_sensitive() {
    let handler = AuthHandler::new(Some("Alice".to_string()), Some("Secret".to_string())).unwrap();

    assert!(handler.validate_credentials("Alice", "Secret"));
    assert!(!handler.validate_credentials("alice", "Secret"));
    assert!(!handler.validate_credentials("Alice", "secret"));
    assert!(!handler.validate_credentials("ALICE", "SECRET"));
}

#[tokio::test]
async fn test_auth_handler_whitespace_in_credentials() {
    let handler =
        AuthHandler::new(Some("user name".to_string()), Some("pass word".to_string())).unwrap();

    assert!(handler.validate_credentials("user name", "pass word"));
    assert!(!handler.validate_credentials("username", "password"));
}

#[tokio::test]
async fn test_auth_handler_special_characters() {
    let handler = AuthHandler::new(
        Some("user@example.com".to_string()),
        Some("p@$$w0rd!#%".to_string()),
    )
    .unwrap();

    assert!(handler.validate_credentials("user@example.com", "p@$$w0rd!#%"));
}

#[tokio::test]
async fn test_auth_handler_unicode_credentials() {
    let handler = AuthHandler::new(Some("用户".to_string()), Some("密码".to_string())).unwrap();

    assert!(handler.validate_credentials("用户", "密码"));
    assert!(!handler.validate_credentials("user", "password"));
}

#[tokio::test]
async fn test_auth_handler_debug_redacts_credentials() {
    let handler = AuthHandler::new(
        Some("supersecret".to_string()),
        Some("topsecretpassword".to_string()),
    )
    .unwrap();

    let debug_output = format!("{:?}", handler);

    // Credentials should not appear in debug output
    assert!(!debug_output.contains("supersecret"));
    assert!(!debug_output.contains("topsecretpassword"));
    assert!(debug_output.contains("AuthHandler"));
    assert!(debug_output.contains("enabled: true"));
}

#[tokio::test]
async fn test_auth_handler_debug_when_disabled() {
    let handler = AuthHandler::new(None, None).unwrap();

    let debug_output = format!("{:?}", handler);
    assert!(debug_output.contains("AuthHandler"));
    assert!(debug_output.contains("enabled: false"));
}

#[tokio::test]
async fn test_auth_command_sequence_valid() {
    use nntp_proxy::command::{AuthAction, CommandAction, CommandHandler};

    // AUTHINFO USER should request password
    let user_action = CommandHandler::classify("AUTHINFO USER alice\r\n");
    assert!(matches!(
        user_action,
        CommandAction::InterceptAuth(AuthAction::RequestPassword(ref u)) if *u == "alice"
    ));

    // AUTHINFO PASS should validate and respond
    let pass_action = CommandHandler::classify("AUTHINFO PASS secret\r\n");
    assert!(matches!(
        pass_action,
        CommandAction::InterceptAuth(AuthAction::ValidateAndRespond { ref password }) if *password == "secret"
    ));
}

#[tokio::test]
async fn test_auth_responses_are_valid_nntp() {
    let handler = AuthHandler::new(Some("user".to_string()), Some("pass".to_string())).unwrap();

    let user_resp = handler.user_response();
    let user_str = String::from_utf8_lossy(user_resp);
    assert!(user_str.starts_with("381"));
    assert!(user_str.ends_with("\r\n"));

    let pass_resp = handler.pass_response();
    let pass_str = String::from_utf8_lossy(pass_resp);
    assert!(pass_str.starts_with("281"));
    assert!(pass_str.ends_with("\r\n"));
}

#[tokio::test]
async fn test_auth_handler_processes_auth_commands() {
    use nntp_proxy::command::AuthAction;

    let handler = AuthHandler::new(Some("user".to_string()), Some("pass".to_string())).unwrap();
    let mut output = Vec::new();

    // Test USER command
    let user_action = AuthAction::RequestPassword("testuser");
    let (bytes, auth_success) = handler
        .handle_auth_command(user_action, &mut output, None)
        .await
        .unwrap();

    assert!(bytes > 0);
    assert!(!output.is_empty());
    assert!(!auth_success); // USER doesn't authenticate

    // Test PASS command with validation
    output.clear();
    let pass_action = AuthAction::ValidateAndRespond { password: "pass" };
    let (bytes, auth_success) = handler
        .handle_auth_command(pass_action, &mut output, Some("user"))
        .await
        .unwrap();

    assert!(bytes > 0);
    assert!(!output.is_empty());
    assert!(auth_success); // Correct credentials
}

#[tokio::test]
async fn test_reject_response_formatting() {
    use tokio::io::AsyncWriteExt;

    let mut output = Vec::new();
    let reject_message = "500 Command not supported\r\n";

    output.write_all(reject_message.as_bytes()).await.unwrap();

    let response = String::from_utf8_lossy(&output);
    assert!(response.starts_with("500"));
    assert!(response.ends_with("\r\n"));
}

#[tokio::test]
async fn test_command_classification_for_stateless() {
    use nntp_proxy::command::{CommandAction, CommandHandler};

    // Test that stateless commands are classified correctly
    let action = CommandHandler::classify("ARTICLE <msgid@example.com>\r\n");
    assert_eq!(action, CommandAction::ForwardStateless);

    let action = CommandHandler::classify("LIST\r\n");
    assert_eq!(action, CommandAction::ForwardStateless);
}

#[tokio::test]
async fn test_session_with_auth_handler() {
    use test_helpers::{create_test_addr, create_test_auth_handler_with, create_test_buffer_pool};

    let (_backend_port, _handle) = spawn_mock_backend().await;
    let buffer_pool = create_test_buffer_pool();
    let auth_handler = create_test_auth_handler_with("testuser", "testpass");

    let addr = create_test_addr();
    let _session = ClientSession::new(addr.into(), buffer_pool, auth_handler);

    // Session should be created successfully with auth handler
}

#[tokio::test]
async fn test_session_with_disabled_auth() {
    use test_helpers::{
        create_test_addr, create_test_auth_handler_disabled, create_test_buffer_pool,
    };

    let (_backend_port, _handle) = spawn_mock_backend().await;
    let buffer_pool = create_test_buffer_pool();
    let auth_handler = create_test_auth_handler_disabled();

    let addr = create_test_addr();
    let _session = ClientSession::new(addr.into(), buffer_pool, auth_handler.clone());

    assert!(!auth_handler.is_enabled());
}

#[tokio::test]
async fn test_config_client_auth_is_enabled() {
    use nntp_proxy::config::{ClientAuth, UserCredentials};

    let config = ClientAuth {
        users: vec![UserCredentials {
            username: "user".to_string(),
            password: "pass".to_string(),
        }],
        greeting: None,
    };

    assert!(config.is_enabled());
}

#[tokio::test]
async fn test_config_client_auth_disabled_missing_username() {
    use nntp_proxy::config::ClientAuth;

    let config = ClientAuth {
        users: vec![],
        greeting: None,
    };

    assert!(!config.is_enabled());
}

#[tokio::test]
async fn test_config_client_auth_disabled_missing_password() {
    use nntp_proxy::config::ClientAuth;

    let config = ClientAuth {
        users: vec![],
        greeting: None,
    };

    assert!(!config.is_enabled());
}

#[tokio::test]
async fn test_config_client_auth_disabled_both_missing() {
    use nntp_proxy::config::ClientAuth;

    let config = ClientAuth {
        users: vec![],
        greeting: None,
    };

    assert!(!config.is_enabled());
}

#[tokio::test]
async fn test_config_default_client_auth_is_disabled() {
    use nntp_proxy::config::ClientAuth;

    let config: ClientAuth = Default::default();
    assert!(!config.is_enabled());
}

#[tokio::test]
async fn test_auth_handler_with_very_long_credentials() {
    let long_user = "a".repeat(1000);
    let long_pass = "b".repeat(1000);

    let handler = AuthHandler::new(Some(long_user.clone()), Some(long_pass.clone())).unwrap();

    assert!(handler.validate_credentials(&long_user, &long_pass));
    assert!(!handler.validate_credentials(&long_user, "short"));
    assert!(!handler.validate_credentials("short", &long_pass));
}

#[tokio::test]
async fn test_multiple_auth_handlers_independent() {
    let handler1 = AuthHandler::new(Some("user1".to_string()), Some("pass1".to_string())).unwrap();
    let handler2 = AuthHandler::new(Some("user2".to_string()), Some("pass2".to_string())).unwrap();

    assert!(handler1.validate_credentials("user1", "pass1"));
    assert!(!handler1.validate_credentials("user2", "pass2"));

    assert!(handler2.validate_credentials("user2", "pass2"));
    assert!(!handler2.validate_credentials("user1", "pass1"));
}

#[tokio::test]
async fn test_auth_handler_clone_via_arc() {
    use test_helpers::create_test_auth_handler;

    let handler = create_test_auth_handler();
    let handler_clone = handler.clone();

    assert!(handler.validate_credentials("user", "pass"));
    assert!(handler_clone.validate_credentials("user", "pass"));
    assert_eq!(Arc::strong_count(&handler), 2);
}

#[tokio::test]
async fn test_session_builder_with_auth_handler() {
    use test_helpers::{create_test_addr, create_test_auth_handler, create_test_buffer_pool};

    let buffer_pool = create_test_buffer_pool();
    let auth_handler = create_test_auth_handler();
    let addr = create_test_addr();

    let session =
        ClientSession::builder(addr.into(), buffer_pool.clone(), auth_handler.clone()).build();

    assert!(!session.is_per_command_routing());
}

#[tokio::test]
async fn test_session_builder_with_router_and_auth() {
    use test_helpers::{
        create_test_addr, create_test_auth_handler, create_test_buffer_pool, create_test_router,
    };

    let buffer_pool = create_test_buffer_pool();
    let auth_handler = create_test_auth_handler();
    let router = create_test_router();
    let addr = create_test_addr();

    let session = ClientSession::builder(addr.into(), buffer_pool.clone(), auth_handler)
        .with_router(router)
        .with_routing_mode(RoutingMode::PerCommand)
        .build();

    assert!(session.is_per_command_routing());
}

#[tokio::test]
async fn test_proxy_creates_auth_handler_from_config() {
    let (backend_port, _handle) = spawn_mock_backend().await;

    let config = create_config_with_auth(vec![backend_port], "proxyuser", "proxypass");

    let proxy = NntpProxy::new(config, RoutingMode::Stateful).unwrap();

    // Proxy should be created successfully with auth config
    assert!(!proxy.servers().is_empty());
}

#[tokio::test]
async fn test_auth_with_empty_command() {
    use nntp_proxy::command::CommandHandler;

    let _action = CommandHandler::classify("");
    // Should be rejected or handled gracefully, not crash
}

#[tokio::test]
async fn test_auth_with_whitespace_only_command() {
    use nntp_proxy::command::CommandHandler;

    let _action = CommandHandler::classify("   \r\n");
    // Should be handled gracefully
}

#[tokio::test]
async fn test_auth_case_variations() {
    use nntp_proxy::command::{AuthAction, CommandAction, CommandHandler};

    // Test uppercase (most common)
    let upper = CommandHandler::classify("AUTHINFO USER test\r\n");
    assert!(
        matches!(
            upper,
            CommandAction::InterceptAuth(AuthAction::RequestPassword(_))
        ),
        "Upper case AUTHINFO USER should be intercepted"
    );

    // Test lowercase
    let lower = CommandHandler::classify("authinfo user test\r\n");
    assert!(
        matches!(
            lower,
            CommandAction::InterceptAuth(AuthAction::RequestPassword(_))
        ),
        "Lower case authinfo user should be intercepted"
    );

    // Test Titlecase (Authinfo with lowercase 'user')
    let title = CommandHandler::classify("Authinfo user test\r\n");
    assert!(
        matches!(
            title,
            CommandAction::InterceptAuth(AuthAction::RequestPassword(_))
        ),
        "Titlecase Authinfo user should be intercepted"
    );

    // AUTHINFO PASS variations
    let upper_pass = CommandHandler::classify("AUTHINFO PASS secret\r\n");
    assert!(matches!(
        upper_pass,
        CommandAction::InterceptAuth(AuthAction::ValidateAndRespond { .. })
    ));

    let lower_pass = CommandHandler::classify("authinfo pass secret\r\n");
    assert!(matches!(
        lower_pass,
        CommandAction::InterceptAuth(AuthAction::ValidateAndRespond { .. })
    ));
}

#[tokio::test]
async fn test_concurrent_auth_handlers() {
    use test_helpers::create_test_auth_handler_with;
    use tokio::task::JoinSet;

    let handler = create_test_auth_handler_with("shared", "password");

    let mut set = JoinSet::new();

    for i in 0..100 {
        let h = handler.clone();
        set.spawn(async move {
            if i % 2 == 0 {
                h.validate_credentials("shared", "password")
            } else {
                h.validate_credentials("wrong", "credentials")
            }
        });
    }

    let mut correct = 0;
    let mut incorrect = 0;

    while let Some(result) = set.join_next().await {
        if result.unwrap() {
            correct += 1;
        } else {
            incorrect += 1;
        }
    }

    assert_eq!(correct, 50);
    assert_eq!(incorrect, 50);
}

#[tokio::test]
async fn test_auth_handler_response_consistency() {
    let handler = AuthHandler::new(Some("user".to_string()), Some("pass".to_string())).unwrap();

    // Call multiple times, should always return same static responses
    for _ in 0..10 {
        assert_eq!(handler.user_response(), handler.user_response());
        assert_eq!(handler.pass_response(), handler.pass_response());
    }
}

#[tokio::test]
async fn test_auth_with_newlines_in_credentials() {
    // Newlines in credentials should work (unlikely but valid)
    let handler = AuthHandler::new(
        Some("user\nname".to_string()),
        Some("pass\nword".to_string()),
    )
    .unwrap();

    assert!(handler.validate_credentials("user\nname", "pass\nword"));
    assert!(!handler.validate_credentials("username", "password"));
}

#[tokio::test]
async fn test_auth_with_null_bytes_in_credentials() {
    // Null bytes in credentials (edge case)
    let handler = AuthHandler::new(
        Some("user\0name".to_string()),
        Some("pass\0word".to_string()),
    )
    .unwrap();

    assert!(handler.validate_credentials("user\0name", "pass\0word"));
}

#[tokio::test]
async fn test_auth_handler_with_concurrent_requests() {
    use nntp_proxy::command::AuthAction;
    use test_helpers::create_test_auth_handler;
    use tokio::task::JoinSet;

    let handler = create_test_auth_handler();

    let mut set = JoinSet::new();

    for _ in 0..50 {
        let h = handler.clone();
        set.spawn(async move {
            let mut output = Vec::new();
            h.handle_auth_command(AuthAction::RequestPassword("test"), &mut output, None)
                .await
        });
    }

    while let Some(result) = set.join_next().await {
        let (bytes, _) = result.unwrap().unwrap();
        assert!(bytes > 0);
    }
}
