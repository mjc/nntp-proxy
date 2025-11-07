//! Comprehensive integration tests for client authentication system
//!
//! Tests cover:
//! - Authentication enabled/disabled states
//! - Valid/invalid credentials
//! - Auth command sequence (USER -> PASS)
//! - Auth with different session modes (per-command, standard, hybrid)
//! - Edge cases and error conditions
//! - Security (credential redaction)

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

mod config_helpers;
use config_helpers::create_test_server_config;
use nntp_proxy::NntpProxy;
use nntp_proxy::auth::AuthHandler;
use nntp_proxy::config::{Config, RoutingMode};
use nntp_proxy::pool::BufferPool;
use nntp_proxy::router::BackendSelector;
use nntp_proxy::session::ClientSession;
use nntp_proxy::types::BufferSize;

/// Create test config with client auth enabled
fn create_config_with_auth(backend_ports: Vec<u16>, username: &str, password: &str) -> Config {
    use nntp_proxy::config::ClientAuthConfig;
    Config {
        servers: backend_ports
            .into_iter()
            .map(|port| create_test_server_config("127.0.0.1", port, &format!("backend-{}", port)))
            .collect(),
        health_check: Default::default(),
        cache: None,
        client_auth: ClientAuthConfig {
            username: Some(username.to_string()),
            password: Some(password.to_string()),
            greeting: None,
        },
    }
}

/// Spawn a mock NNTP backend server
async fn spawn_mock_backend() -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let handle = tokio::spawn(async move {
        loop {
            if let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    // Send greeting
                    let _ = stream.write_all(b"200 Mock NNTP Server Ready\r\n").await;

                    let (reader, mut writer) = stream.split();
                    let mut reader = BufReader::new(reader);
                    let mut line = String::new();

                    // Echo back commands as successful responses
                    while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                        if line.trim().to_uppercase().starts_with("QUIT") {
                            let _ = writer.write_all(b"205 Goodbye\r\n").await;
                            break;
                        }
                        // Simple echo for testing
                        let response = format!("250 OK {}", line);
                        let _ = writer.write_all(response.as_bytes()).await;
                        line.clear();
                    }
                });
            }
        }
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    (port, handle)
}

#[tokio::test]
async fn test_auth_handler_disabled_by_default() {
    let handler = AuthHandler::new(None, None);
    assert!(!handler.is_enabled());
}

#[tokio::test]
async fn test_auth_handler_enabled_with_credentials() {
    let handler = AuthHandler::new(Some("user".to_string()), Some("pass".to_string()));
    assert!(handler.is_enabled());
}

#[tokio::test]
async fn test_auth_handler_validates_correct_credentials() {
    let handler = AuthHandler::new(Some("alice".to_string()), Some("secret123".to_string()));

    assert!(handler.validate("alice", "secret123"));
}

#[tokio::test]
async fn test_auth_handler_rejects_wrong_password() {
    let handler = AuthHandler::new(Some("alice".to_string()), Some("secret123".to_string()));

    assert!(!handler.validate("alice", "wrongpass"));
}

#[tokio::test]
async fn test_auth_handler_rejects_wrong_username() {
    let handler = AuthHandler::new(Some("alice".to_string()), Some("secret123".to_string()));

    assert!(!handler.validate("bob", "secret123"));
}

#[tokio::test]
async fn test_auth_handler_rejects_both_wrong() {
    let handler = AuthHandler::new(Some("alice".to_string()), Some("secret123".to_string()));

    assert!(!handler.validate("bob", "wrongpass"));
}

#[tokio::test]
async fn test_auth_disabled_accepts_any_credentials() {
    let handler = AuthHandler::new(None, None);

    assert!(handler.validate("anything", "works"));
    assert!(handler.validate("", ""));
    assert!(handler.validate("random", "stuff"));
}

#[tokio::test]
async fn test_auth_handler_partial_config_disabled() {
    // Only username, no password - should be disabled
    let handler1 = AuthHandler::new(Some("user".to_string()), None);
    assert!(!handler1.is_enabled());

    // Only password, no username - should be disabled
    let handler2 = AuthHandler::new(None, Some("pass".to_string()));
    assert!(!handler2.is_enabled());
}

#[tokio::test]
async fn test_auth_handler_empty_string_credentials() {
    let handler = AuthHandler::new(Some("".to_string()), Some("".to_string()));
    assert!(handler.is_enabled());
    assert!(handler.validate("", ""));
    assert!(!handler.validate("nonempty", ""));
}

#[tokio::test]
async fn test_auth_handler_case_sensitive() {
    let handler = AuthHandler::new(Some("Alice".to_string()), Some("Secret".to_string()));

    assert!(handler.validate("Alice", "Secret"));
    assert!(!handler.validate("alice", "Secret"));
    assert!(!handler.validate("Alice", "secret"));
    assert!(!handler.validate("ALICE", "SECRET"));
}

#[tokio::test]
async fn test_auth_handler_whitespace_in_credentials() {
    let handler = AuthHandler::new(Some("user name".to_string()), Some("pass word".to_string()));

    assert!(handler.validate("user name", "pass word"));
    assert!(!handler.validate("username", "password"));
}

#[tokio::test]
async fn test_auth_handler_special_characters() {
    let handler = AuthHandler::new(
        Some("user@example.com".to_string()),
        Some("p@$$w0rd!#%".to_string()),
    );

    assert!(handler.validate("user@example.com", "p@$$w0rd!#%"));
}

#[tokio::test]
async fn test_auth_handler_unicode_credentials() {
    let handler = AuthHandler::new(Some("用户".to_string()), Some("密码".to_string()));

    assert!(handler.validate("用户", "密码"));
    assert!(!handler.validate("user", "password"));
}

#[tokio::test]
async fn test_auth_handler_debug_redacts_credentials() {
    let handler = AuthHandler::new(
        Some("supersecret".to_string()),
        Some("topsecretpassword".to_string()),
    );

    let debug_output = format!("{:?}", handler);

    assert!(!debug_output.contains("supersecret"));
    assert!(!debug_output.contains("topsecretpassword"));
    assert!(debug_output.contains("<redacted>"));
    assert!(debug_output.contains("AuthHandler"));
}

#[tokio::test]
async fn test_auth_handler_debug_when_disabled() {
    let handler = AuthHandler::new(None, None);

    let debug_output = format!("{:?}", handler);

    assert!(debug_output.contains("AuthHandler"));
    assert!(debug_output.contains("None"));
}

#[tokio::test]
async fn test_auth_command_sequence_valid() {
    use nntp_proxy::command::{AuthAction, CommandAction, CommandHandler};

    // AUTHINFO USER should request password
    let user_action = CommandHandler::handle_command("AUTHINFO USER alice\r\n");
    assert!(matches!(
        user_action,
        CommandAction::InterceptAuth(AuthAction::RequestPassword)
    ));

    // AUTHINFO PASS should accept auth
    let pass_action = CommandHandler::handle_command("AUTHINFO PASS secret\r\n");
    assert!(matches!(
        pass_action,
        CommandAction::InterceptAuth(AuthAction::AcceptAuth)
    ));
}

#[tokio::test]
async fn test_auth_responses_are_valid_nntp() {
    let handler = AuthHandler::new(Some("user".to_string()), Some("pass".to_string()));

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
async fn test_forwarding_handles_auth_commands() {
    use nntp_proxy::command::{AuthAction, CommandAction};
    use nntp_proxy::session::forwarding::handle_intercepted_command;

    let handler = AuthHandler::new(Some("user".to_string()), Some("pass".to_string()));
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let mut output = Vec::new();

    // Test USER command
    let user_action = CommandAction::InterceptAuth(AuthAction::RequestPassword);
    let result = handle_intercepted_command(
        user_action,
        "AUTHINFO USER test\r\n",
        &mut output,
        &handler,
        &addr,
    )
    .await
    .unwrap();

    assert!(result.is_some());
    assert!(result.unwrap() > 0);
    assert!(!output.is_empty());

    // Test PASS command
    output.clear();
    let pass_action = CommandAction::InterceptAuth(AuthAction::AcceptAuth);
    let result = handle_intercepted_command(
        pass_action,
        "AUTHINFO PASS secret\r\n",
        &mut output,
        &handler,
        &addr,
    )
    .await
    .unwrap();

    assert!(result.is_some());
    assert!(result.unwrap() > 0);
    assert!(!output.is_empty());
}

#[tokio::test]
async fn test_forwarding_handles_rejected_commands() {
    use nntp_proxy::command::CommandAction;
    use nntp_proxy::session::forwarding::handle_intercepted_command;

    let handler = AuthHandler::new(None, None);
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let mut output = Vec::new();

    let reject_action = CommandAction::Reject("stateful command");
    let result = handle_intercepted_command(
        reject_action,
        "GROUP misc.test\r\n",
        &mut output,
        &handler,
        &addr,
    )
    .await
    .unwrap();

    assert!(result.is_some());
    assert!(result.unwrap() > 0);

    let response = String::from_utf8_lossy(&output);
    assert!(response.starts_with("500") || response.starts_with("503"));
}

#[tokio::test]
async fn test_forwarding_returns_none_for_forward_commands() {
    use nntp_proxy::command::CommandAction;
    use nntp_proxy::session::forwarding::handle_intercepted_command;

    let handler = AuthHandler::new(None, None);
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let mut output = Vec::new();

    let forward_action = CommandAction::ForwardStateless;
    let result = handle_intercepted_command(
        forward_action,
        "ARTICLE <msgid@example.com>\r\n",
        &mut output,
        &handler,
        &addr,
    )
    .await
    .unwrap();

    assert!(result.is_none());
    assert!(output.is_empty());
}

#[tokio::test]
async fn test_session_with_auth_handler() {
    let (_backend_port, _handle) = spawn_mock_backend().await;
    let buffer_pool = BufferPool::new(BufferSize::new(8192).unwrap(), 4);
    let auth_handler = Arc::new(AuthHandler::new(
        Some("testuser".to_string()),
        Some("testpass".to_string()),
    ));

    let addr: SocketAddr = "127.0.0.1:9999".parse().unwrap();
    let _session = ClientSession::new(addr, buffer_pool, auth_handler);

    // Session should be created successfully with auth handler
}

#[tokio::test]
async fn test_session_with_disabled_auth() {
    let (_backend_port, _handle) = spawn_mock_backend().await;
    let buffer_pool = BufferPool::new(BufferSize::new(8192).unwrap(), 4);
    let auth_handler = Arc::new(AuthHandler::new(None, None));

    let addr: SocketAddr = "127.0.0.1:9999".parse().unwrap();
    let _session = ClientSession::new(addr, buffer_pool, auth_handler.clone());

    assert!(!auth_handler.is_enabled());
}

#[tokio::test]
async fn test_config_client_auth_is_enabled() {
    use nntp_proxy::config::ClientAuthConfig;

    let config = ClientAuthConfig {
        username: Some("user".to_string()),
        password: Some("pass".to_string()),
        greeting: None,
    };

    assert!(config.is_enabled());
}

#[tokio::test]
async fn test_config_client_auth_disabled_missing_username() {
    use nntp_proxy::config::ClientAuthConfig;

    let config = ClientAuthConfig {
        username: None,
        password: Some("pass".to_string()),
        greeting: None,
    };

    assert!(!config.is_enabled());
}

#[tokio::test]
async fn test_config_client_auth_disabled_missing_password() {
    use nntp_proxy::config::ClientAuthConfig;

    let config = ClientAuthConfig {
        username: Some("user".to_string()),
        password: None,
        greeting: None,
    };

    assert!(!config.is_enabled());
}

#[tokio::test]
async fn test_config_client_auth_disabled_both_missing() {
    use nntp_proxy::config::ClientAuthConfig;

    let config = ClientAuthConfig {
        username: None,
        password: None,
        greeting: None,
    };

    assert!(!config.is_enabled());
}

#[tokio::test]
async fn test_config_default_client_auth_is_disabled() {
    use nntp_proxy::config::ClientAuthConfig;

    let config: ClientAuthConfig = Default::default();
    assert!(!config.is_enabled());
}

#[tokio::test]
async fn test_auth_handler_with_very_long_credentials() {
    let long_user = "a".repeat(1000);
    let long_pass = "b".repeat(1000);

    let handler = AuthHandler::new(Some(long_user.clone()), Some(long_pass.clone()));

    assert!(handler.validate(&long_user, &long_pass));
    assert!(!handler.validate(&long_user, "short"));
    assert!(!handler.validate("short", &long_pass));
}

#[tokio::test]
async fn test_multiple_auth_handlers_independent() {
    let handler1 = AuthHandler::new(Some("user1".to_string()), Some("pass1".to_string()));
    let handler2 = AuthHandler::new(Some("user2".to_string()), Some("pass2".to_string()));

    assert!(handler1.validate("user1", "pass1"));
    assert!(!handler1.validate("user2", "pass2"));

    assert!(handler2.validate("user2", "pass2"));
    assert!(!handler2.validate("user1", "pass1"));
}

#[tokio::test]
async fn test_auth_handler_clone_via_arc() {
    let handler = Arc::new(AuthHandler::new(
        Some("user".to_string()),
        Some("pass".to_string()),
    ));
    let handler_clone = handler.clone();

    assert!(handler.validate("user", "pass"));
    assert!(handler_clone.validate("user", "pass"));
    assert_eq!(Arc::strong_count(&handler), 2);
}

#[tokio::test]
async fn test_session_builder_with_auth_handler() {
    let buffer_pool = BufferPool::new(BufferSize::new(8192).unwrap(), 4);
    let auth_handler = Arc::new(AuthHandler::new(
        Some("user".to_string()),
        Some("pass".to_string()),
    ));
    let addr: SocketAddr = "127.0.0.1:9999".parse().unwrap();

    let session = ClientSession::builder(addr, buffer_pool.clone(), auth_handler.clone()).build();

    assert!(!session.is_per_command_routing());
}

#[tokio::test]
async fn test_session_builder_with_router_and_auth() {
    let buffer_pool = BufferPool::new(BufferSize::new(8192).unwrap(), 4);
    let auth_handler = Arc::new(AuthHandler::new(
        Some("user".to_string()),
        Some("pass".to_string()),
    ));
    let router = Arc::new(BackendSelector::new());
    let addr: SocketAddr = "127.0.0.1:9999".parse().unwrap();

    let session = ClientSession::builder(addr, buffer_pool.clone(), auth_handler)
        .with_router(router)
        .with_routing_mode(RoutingMode::PerCommand)
        .build();

    assert!(session.is_per_command_routing());
}

#[tokio::test]
async fn test_proxy_creates_auth_handler_from_config() {
    let (backend_port, _handle) = spawn_mock_backend().await;

    let config = create_config_with_auth(vec![backend_port], "proxyuser", "proxypass");

    let proxy = NntpProxy::new(config, RoutingMode::Standard).unwrap();

    // Proxy should be created successfully with auth config
    assert!(!proxy.servers().is_empty());
}

#[tokio::test]
async fn test_auth_with_empty_command() {
    use nntp_proxy::command::CommandHandler;

    let _action = CommandHandler::handle_command("");
    // Should be rejected or handled gracefully, not crash
}

#[tokio::test]
async fn test_auth_with_whitespace_only_command() {
    use nntp_proxy::command::CommandHandler;

    let _action = CommandHandler::handle_command("   \r\n");
    // Should be handled gracefully
}

#[tokio::test]
async fn test_auth_case_variations() {
    use nntp_proxy::command::{AuthAction, CommandAction, CommandHandler};

    // Test uppercase (most common)
    let upper = CommandHandler::handle_command("AUTHINFO USER test\r\n");
    assert!(
        matches!(
            upper,
            CommandAction::InterceptAuth(AuthAction::RequestPassword)
        ),
        "Upper case AUTHINFO USER should be intercepted"
    );

    // Test lowercase
    let lower = CommandHandler::handle_command("authinfo user test\r\n");
    assert!(
        matches!(
            lower,
            CommandAction::InterceptAuth(AuthAction::RequestPassword)
        ),
        "Lower case authinfo user should be intercepted"
    );

    // Test Titlecase (Authinfo with lowercase 'user')
    let title = CommandHandler::handle_command("Authinfo user test\r\n");
    assert!(
        matches!(
            title,
            CommandAction::InterceptAuth(AuthAction::RequestPassword)
        ),
        "Titlecase Authinfo user should be intercepted"
    );

    // AUTHINFO PASS variations
    let upper_pass = CommandHandler::handle_command("AUTHINFO PASS secret\r\n");
    assert!(matches!(
        upper_pass,
        CommandAction::InterceptAuth(AuthAction::AcceptAuth)
    ));

    let lower_pass = CommandHandler::handle_command("authinfo pass secret\r\n");
    assert!(matches!(
        lower_pass,
        CommandAction::InterceptAuth(AuthAction::AcceptAuth)
    ));
}

#[tokio::test]
async fn test_concurrent_auth_handlers() {
    use std::sync::Arc;
    use tokio::task::JoinSet;

    let handler = Arc::new(AuthHandler::new(
        Some("shared".to_string()),
        Some("password".to_string()),
    ));

    let mut set = JoinSet::new();

    for i in 0..100 {
        let h = handler.clone();
        set.spawn(async move {
            if i % 2 == 0 {
                h.validate("shared", "password")
            } else {
                h.validate("wrong", "credentials")
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
    let handler = AuthHandler::new(Some("user".to_string()), Some("pass".to_string()));

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
    );

    assert!(handler.validate("user\nname", "pass\nword"));
    assert!(!handler.validate("username", "password"));
}

#[tokio::test]
async fn test_auth_with_null_bytes_in_credentials() {
    // Null bytes in credentials (edge case)
    let handler = AuthHandler::new(
        Some("user\0name".to_string()),
        Some("pass\0word".to_string()),
    );

    assert!(handler.validate("user\0name", "pass\0word"));
}

#[tokio::test]
async fn test_forwarding_with_concurrent_requests() {
    use nntp_proxy::command::CommandAction;
    use nntp_proxy::session::forwarding::handle_intercepted_command;
    use tokio::task::JoinSet;

    let handler = Arc::new(AuthHandler::new(
        Some("user".to_string()),
        Some("pass".to_string()),
    ));
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();

    let mut set = JoinSet::new();

    for _ in 0..50 {
        let h = handler.clone();
        set.spawn(async move {
            let mut output = Vec::new();
            handle_intercepted_command(
                CommandAction::ForwardStateless,
                "LIST\r\n",
                &mut output,
                &h,
                &addr,
            )
            .await
        });
    }

    while let Some(result) = set.join_next().await {
        let res = result.unwrap().unwrap();
        assert!(res.is_none());
    }
}
