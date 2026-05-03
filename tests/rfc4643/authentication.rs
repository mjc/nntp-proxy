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

use crate::test_helpers::{create_test_config_with_auth, get_available_port, spawn_mock_server};
use nntp_proxy::NntpProxy;
use nntp_proxy::auth::AuthHandler;
use nntp_proxy::command::{AuthAction, CommandAction, CommandHandler};
use nntp_proxy::config::RoutingMode;
use nntp_proxy::protocol::RequestContext;
use nntp_proxy::session::ClientSession;

fn classify(command: &str) -> CommandAction<'static> {
    let request = Box::leak(Box::new(RequestContext::parse(command.as_bytes())));
    CommandHandler::classify_request(request)
}

fn auth_handler(username: &str, password: &str) -> Arc<AuthHandler> {
    Arc::new(AuthHandler::new(Some(username.to_string()), Some(password.to_string())).unwrap())
}

#[tokio::test]
async fn test_auth_handler_enabled_state() {
    [
        (None, None, false),
        (Some("user"), Some("pass"), true),
        (Some("user"), None, false),
        (None, Some("pass"), false),
    ]
    .into_iter()
    .for_each(|(username, password, enabled)| {
        let handler = AuthHandler::new(
            username.map(ToOwned::to_owned),
            password.map(ToOwned::to_owned),
        )
        .unwrap();
        assert_eq!(handler.is_enabled(), enabled);
    });
}

#[tokio::test]
async fn test_auth_handler_validates_credentials() {
    let handler = auth_handler("alice", "secret123");
    [
        ("alice", "secret123", true),
        ("alice", "wrongpass", false),
        ("bob", "secret123", false),
        ("bob", "wrongpass", false),
    ]
    .into_iter()
    .for_each(|(username, password, valid)| {
        assert_eq!(handler.validate_credentials(username, password), valid);
    });
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
    let result = AuthHandler::new(Some(String::new()), Some(String::new()));
    assert!(
        result.is_err(),
        "Empty credentials should be rejected to prevent silent auth bypass"
    );
}

#[tokio::test]
async fn test_auth_handler_case_sensitive() {
    let handler = auth_handler("Alice", "Secret");
    [
        ("Alice", "Secret", true),
        ("alice", "Secret", false),
        ("Alice", "secret", false),
        ("ALICE", "SECRET", false),
    ]
    .into_iter()
    .for_each(|(username, password, valid)| {
        assert_eq!(handler.validate_credentials(username, password), valid);
    });
}

#[tokio::test]
async fn test_auth_handler_whitespace_in_credentials() {
    let handler = auth_handler("user name", "pass word");

    assert!(handler.validate_credentials("user name", "pass word"));
    assert!(!handler.validate_credentials("username", "password"));
}

#[tokio::test]
async fn test_auth_handler_special_characters() {
    let handler = auth_handler("user@example.com", "p@$$w0rd!#%");

    assert!(handler.validate_credentials("user@example.com", "p@$$w0rd!#%"));
}

#[tokio::test]
async fn test_auth_handler_unicode_credentials() {
    let handler = auth_handler("用户", "密码");

    assert!(handler.validate_credentials("用户", "密码"));
    assert!(!handler.validate_credentials("user", "password"));
}

#[tokio::test]
async fn test_auth_handler_debug_output() {
    [
        (auth_handler("supersecret", "topsecretpassword"), true),
        (Arc::new(AuthHandler::new(None, None).unwrap()), false),
    ]
    .into_iter()
    .for_each(|(handler, enabled)| {
        let debug_output = format!("{handler:?}");
        assert!(!debug_output.contains("supersecret"));
        assert!(!debug_output.contains("topsecretpassword"));
        assert!(debug_output.contains("AuthHandler"));
        assert!(debug_output.contains(&format!("enabled: {enabled}")));
    });
}

#[tokio::test]
async fn test_auth_command_sequence_valid() {
    [
        ("AUTHINFO USER alice\r\n", "alice", true),
        ("AUTHINFO PASS secret\r\n", "secret", false),
    ]
    .into_iter()
    .for_each(|(command, expected, is_user)| match classify(command) {
        CommandAction::InterceptAuth(AuthAction::RequestPassword(username)) if is_user => {
            assert_eq!(username, expected);
        }
        CommandAction::InterceptAuth(AuthAction::ValidateAndRespond { password }) if !is_user => {
            assert_eq!(password, expected);
        }
        action => panic!("unexpected auth action: {action:?}"),
    });
}

#[tokio::test]
async fn test_auth_responses_are_valid_nntp() {
    let handler = auth_handler("user", "pass");

    [
        (handler.user_response(), "381"),
        (handler.pass_response(), "281"),
    ]
    .into_iter()
    .for_each(|(response, prefix)| {
        let response = String::from_utf8_lossy(response);
        assert!(response.starts_with(prefix));
        assert!(response.ends_with("\r\n"));
    });
}

#[tokio::test]
async fn test_auth_handler_processes_auth_commands() {
    let handler = auth_handler("user", "pass");
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
    ["ARTICLE <msgid@example.com>\r\n", "LIST\r\n"]
        .into_iter()
        .for_each(|command| assert_eq!(classify(command), CommandAction::ForwardStateless));
}

#[tokio::test]
async fn test_session_with_auth_handler() {
    use crate::test_helpers::{
        create_test_addr, create_test_auth_handler_with, create_test_buffer_pool,
    };
    use nntp_proxy::metrics::MetricsCollector;

    let backend_port = get_available_port().await.unwrap();
    let _handle = spawn_mock_server(backend_port, "Mock Backend");
    let buffer_pool = create_test_buffer_pool();
    let auth_handler = create_test_auth_handler_with("testuser", "testpass");
    let metrics = MetricsCollector::new(1);

    let addr = create_test_addr();
    let _session = ClientSession::new(addr.into(), buffer_pool, auth_handler, metrics);

    // Session should be created successfully with auth handler
}

#[tokio::test]
async fn test_session_with_disabled_auth() {
    use crate::test_helpers::{
        create_test_addr, create_test_auth_handler_disabled, create_test_buffer_pool,
    };
    use nntp_proxy::metrics::MetricsCollector;

    let backend_port = get_available_port().await.unwrap();
    let _handle = spawn_mock_server(backend_port, "Mock Backend");
    let buffer_pool = create_test_buffer_pool();
    let auth_handler = create_test_auth_handler_disabled();
    let metrics = MetricsCollector::new(1);

    let addr = create_test_addr();
    let _session = ClientSession::new(addr.into(), buffer_pool, auth_handler.clone(), metrics);

    assert!(!auth_handler.is_enabled());
}

#[tokio::test]
async fn test_config_client_auth_enabled_state() {
    use nntp_proxy::config::{ClientAuth, UserCredentials};

    [
        (
            ClientAuth {
                users: vec![UserCredentials {
                    username: "user".to_string(),
                    password: "pass".to_string(),
                }],
                greeting: None,
            },
            true,
        ),
        (
            ClientAuth {
                users: vec![],
                greeting: None,
            },
            false,
        ),
        (ClientAuth::default(), false),
    ]
    .into_iter()
    .for_each(|(config, enabled)| assert_eq!(config.is_enabled(), enabled));
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
    let handler1 = auth_handler("user1", "pass1");
    let handler2 = auth_handler("user2", "pass2");

    assert!(handler1.validate_credentials("user1", "pass1"));
    assert!(!handler1.validate_credentials("user2", "pass2"));

    assert!(handler2.validate_credentials("user2", "pass2"));
    assert!(!handler2.validate_credentials("user1", "pass1"));
}

#[tokio::test]
async fn test_auth_handler_clone_via_arc() {
    use crate::test_helpers::create_test_auth_handler;

    let handler = create_test_auth_handler();
    let handler_clone = handler.clone();

    assert!(handler.validate_credentials("user", "pass"));
    assert!(handler_clone.validate_credentials("user", "pass"));
    assert_eq!(Arc::strong_count(&handler), 2);
}

#[tokio::test]
async fn test_session_builder_with_auth_handler() {
    use crate::test_helpers::{
        create_test_addr, create_test_auth_handler, create_test_buffer_pool,
    };
    use nntp_proxy::metrics::MetricsCollector;

    let buffer_pool = create_test_buffer_pool();
    let auth_handler = create_test_auth_handler();
    let addr = create_test_addr();
    let metrics = MetricsCollector::new(1);

    let session = ClientSession::builder(addr.into(), buffer_pool, auth_handler, metrics).build();

    assert!(!session.is_per_command_routing());
}

#[tokio::test]
async fn test_session_builder_with_router_and_auth() {
    use crate::test_helpers::{
        create_test_addr, create_test_auth_handler, create_test_buffer_pool, create_test_router,
    };
    use nntp_proxy::metrics::MetricsCollector;

    let buffer_pool = create_test_buffer_pool();
    let auth_handler = create_test_auth_handler();
    let router = create_test_router();
    let addr = create_test_addr();
    let metrics = MetricsCollector::new(1);

    let session = ClientSession::builder(addr.into(), buffer_pool, auth_handler, metrics)
        .with_router(router)
        .with_routing_mode(RoutingMode::PerCommand)
        .build();

    assert!(session.is_per_command_routing());
}

#[tokio::test]
async fn test_proxy_creates_auth_handler_from_config() {
    let backend_port = get_available_port().await.unwrap();
    let _handle = spawn_mock_server(backend_port, "Mock Backend");

    let config = create_test_config_with_auth(vec![backend_port], "proxyuser", "proxypass");

    let proxy = NntpProxy::new(config, RoutingMode::Stateful).await.unwrap();

    // Proxy should be created successfully with auth config
    assert!(!proxy.servers().is_empty());
}

#[tokio::test]
async fn test_auth_with_empty_command() {
    let _action = classify("");
    // Should be rejected or handled gracefully, not crash
}

#[tokio::test]
async fn test_auth_with_whitespace_only_command() {
    let _action = classify("   \r\n");
    // Should be handled gracefully
}

#[tokio::test]
async fn test_auth_case_variations() {
    [
        ("AUTHINFO USER test\r\n", true),
        ("authinfo user test\r\n", true),
        ("Authinfo user test\r\n", true),
        ("AUTHINFO PASS secret\r\n", false),
        ("authinfo pass secret\r\n", false),
    ]
    .into_iter()
    .for_each(|(command, is_user)| match classify(command) {
        CommandAction::InterceptAuth(AuthAction::RequestPassword(_)) if is_user => {}
        CommandAction::InterceptAuth(AuthAction::ValidateAndRespond { .. }) if !is_user => {}
        action => panic!("unexpected auth action for {command:?}: {action:?}"),
    });
}

#[tokio::test]
async fn test_concurrent_auth_handlers() {
    use crate::test_helpers::create_test_auth_handler_with;
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
    let handler = auth_handler("user", "pass");

    // Call multiple times, should always return same static responses
    for _ in 0..10 {
        assert_eq!(handler.user_response(), handler.user_response());
        assert_eq!(handler.pass_response(), handler.pass_response());
    }
}

#[tokio::test]
async fn test_auth_with_newlines_in_credentials() {
    // Newlines in credentials should work (unlikely but valid)
    let handler = auth_handler("user\nname", "pass\nword");

    assert!(handler.validate_credentials("user\nname", "pass\nword"));
    assert!(!handler.validate_credentials("username", "password"));
}

#[tokio::test]
async fn test_auth_with_null_bytes_in_credentials() {
    // Null bytes in credentials (edge case)
    let handler = auth_handler("user\0name", "pass\0word");

    assert!(handler.validate_credentials("user\0name", "pass\0word"));
}

#[tokio::test]
async fn test_auth_handler_with_concurrent_requests() {
    use crate::test_helpers::create_test_auth_handler;
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
