//! Integration tests to prevent authentication bypass bugs
//!
//! These tests verify that session handlers properly validate credentials
//! before marking a session as authenticated.

mod test_helpers;

use nntp_proxy::command::{AuthAction, CommandAction, CommandHandler};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Test that StandardHandler requires valid credentials
#[tokio::test]
async fn test_standard_handler_validates_credentials() {
    use test_helpers::create_test_auth_handler_with;

    let auth_handler = create_test_auth_handler_with("testuser", "testpass");

    // Simulate the authentication flow
    let authenticated = Arc::new(AtomicBool::new(false));

    // Step 1: AUTHINFO USER
    let action = CommandHandler::classify("AUTHINFO USER testuser\r\n");
    let username = match action {
        CommandAction::InterceptAuth(AuthAction::RequestPassword(ref u)) => u.clone(),
        _ => panic!("Expected RequestPassword action"),
    };

    let mut output = Vec::new();
    let (_, auth_success) = auth_handler
        .handle_auth_command(
            AuthAction::RequestPassword(username.clone()),
            &mut output,
            None,
        )
        .await
        .unwrap();

    // RequestPassword should not authenticate
    assert!(!auth_success);

    // Step 2: AUTHINFO PASS with WRONG password
    let action = CommandHandler::classify("AUTHINFO PASS wrongpass\r\n");
    let password = match action {
        CommandAction::InterceptAuth(AuthAction::ValidateAndRespond { ref password }) => {
            password.clone()
        }
        _ => panic!("Expected ValidateAndRespond action"),
    };

    output.clear();
    let (_, auth_success) = auth_handler
        .handle_auth_command(
            AuthAction::ValidateAndRespond { password },
            &mut output,
            Some(&username),
        )
        .await
        .unwrap();

    // Wrong password should NOT authenticate
    assert!(!auth_success);
    // Verify session would NOT be marked as authenticated
    if auth_success {
        authenticated.store(true, Ordering::Release);
    }
    assert!(!authenticated.load(Ordering::Acquire));

    // Step 3: AUTHINFO PASS with CORRECT password
    let action = CommandHandler::classify("AUTHINFO PASS testpass\r\n");
    let password = match action {
        CommandAction::InterceptAuth(AuthAction::ValidateAndRespond { ref password }) => {
            password.clone()
        }
        _ => panic!("Expected ValidateAndRespond action"),
    };

    output.clear();
    let (_, auth_success) = auth_handler
        .handle_auth_command(
            AuthAction::ValidateAndRespond { password },
            &mut output,
            Some(&username),
        )
        .await
        .unwrap();

    // Correct password SHOULD authenticate
    assert!(auth_success);
    // Verify session WOULD be marked as authenticated
    if auth_success {
        authenticated.store(true, Ordering::Release);
    }
    assert!(authenticated.load(Ordering::Acquire));
}

/// Test that authentication cannot be bypassed with PASS before USER
#[tokio::test]
async fn test_pass_before_user_rejected() {
    use test_helpers::create_test_auth_handler_with;

    let auth_handler = create_test_auth_handler_with("testuser", "testpass");

    // Try to send AUTHINFO PASS without first sending USER
    let action = CommandHandler::classify("AUTHINFO PASS testpass\r\n");
    let password = match action {
        CommandAction::InterceptAuth(AuthAction::ValidateAndRespond { ref password }) => {
            password.clone()
        }
        _ => panic!("Expected ValidateAndRespond action"),
    };

    let mut output = Vec::new();
    let (_, auth_success) = auth_handler
        .handle_auth_command(
            AuthAction::ValidateAndRespond { password },
            &mut output,
            None, // No username stored!
        )
        .await
        .unwrap();

    // Should not authenticate without username
    assert!(
        !auth_success,
        "AUTHINFO PASS without prior AUTHINFO USER should not authenticate"
    );
}

/// Test that authentication state is properly isolated
#[tokio::test]
async fn test_auth_state_isolation() {
    use test_helpers::create_test_auth_handler_with;

    let auth_handler = create_test_auth_handler_with("user1", "pass1");

    // Session 1 authenticates with correct credentials
    let session1_authenticated = Arc::new(AtomicBool::new(false));
    let mut output = Vec::new();
    let (_, auth_success) = auth_handler
        .handle_auth_command(
            AuthAction::ValidateAndRespond {
                password: "pass1".to_string(),
            },
            &mut output,
            Some("user1"),
        )
        .await
        .unwrap();

    if auth_success {
        session1_authenticated.store(true, Ordering::Release);
    }
    assert!(session1_authenticated.load(Ordering::Acquire));

    // Session 2 with WRONG credentials should NOT be authenticated
    let session2_authenticated = Arc::new(AtomicBool::new(false));
    output.clear();
    let (_, auth_success) = auth_handler
        .handle_auth_command(
            AuthAction::ValidateAndRespond {
                password: "wrongpass".to_string(),
            },
            &mut output,
            Some("user1"),
        )
        .await
        .unwrap();

    if auth_success {
        session2_authenticated.store(true, Ordering::Release);
    }
    assert!(!session2_authenticated.load(Ordering::Acquire));

    // Session 1 should still be authenticated
    assert!(session1_authenticated.load(Ordering::Acquire));
}

/// Test multiple failed auth attempts don't eventually succeed
#[tokio::test]
async fn test_repeated_failures_dont_succeed() {
    use test_helpers::create_test_auth_handler;

    let auth_handler = create_test_auth_handler();

    let authenticated = Arc::new(AtomicBool::new(false));

    // Try 100 times with wrong password
    for i in 0..100 {
        let mut output = Vec::new();
        let (_, auth_success) = auth_handler
            .handle_auth_command(
                AuthAction::ValidateAndRespond {
                    password: format!("wrongpass{}", i),
                },
                &mut output,
                Some("user"),
            )
            .await
            .unwrap();

        if auth_success {
            authenticated.store(true, Ordering::Release);
        }
    }

    // Should never authenticate with wrong passwords
    assert!(
        !authenticated.load(Ordering::Acquire),
        "Repeated failed attempts should never authenticate"
    );
}

/// Test that auth_success flag is the ONLY way to authenticate
#[tokio::test]
async fn test_auth_success_is_only_path_to_authentication() {
    use test_helpers::create_test_auth_handler;

    let auth_handler = create_test_auth_handler();

    // This simulates what session handlers do
    let authenticated = Arc::new(AtomicBool::new(false));

    // Try various auth actions
    let test_cases = vec![
        (AuthAction::RequestPassword("user".to_string()), None, false),
        (
            AuthAction::ValidateAndRespond {
                password: "wrong".to_string(),
            },
            Some("user"),
            false,
        ),
        (
            AuthAction::ValidateAndRespond {
                password: "pass".to_string(),
            },
            Some("wrong"),
            false,
        ),
        (
            AuthAction::ValidateAndRespond {
                password: "pass".to_string(),
            },
            Some("user"),
            true,
        ),
    ];

    for (action, stored_username, should_auth) in test_cases {
        authenticated.store(false, Ordering::Release);
        let mut output = Vec::new();
        let (_, auth_success) = auth_handler
            .handle_auth_command(action, &mut output, stored_username)
            .await
            .unwrap();

        // This is what session handlers do - ONLY set authenticated if auth_success is true
        if auth_success {
            authenticated.store(true, Ordering::Release);
        }

        assert_eq!(
            authenticated.load(Ordering::Acquire),
            should_auth,
            "Authentication state mismatch"
        );
    }
}

/// Test concurrent authentication attempts
#[tokio::test]
async fn test_concurrent_auth_attempts() {
    use test_helpers::create_test_auth_handler;
    use tokio::task::JoinSet;

    let auth_handler = create_test_auth_handler();

    let mut set = JoinSet::new();

    // Spawn 50 concurrent auth attempts with wrong credentials
    for i in 0..50 {
        let handler = auth_handler.clone();
        set.spawn(async move {
            let authenticated = Arc::new(AtomicBool::new(false));
            let mut output = Vec::new();
            let (_, auth_success) = handler
                .handle_auth_command(
                    AuthAction::ValidateAndRespond {
                        password: format!("wrongpass{}", i),
                    },
                    &mut output,
                    Some("user"),
                )
                .await
                .unwrap();

            if auth_success {
                authenticated.store(true, Ordering::Release);
            }
            authenticated.load(Ordering::Acquire)
        });
    }

    // Spawn 50 concurrent auth attempts with correct credentials
    for _ in 0..50 {
        let handler = auth_handler.clone();
        set.spawn(async move {
            let authenticated = Arc::new(AtomicBool::new(false));
            let mut output = Vec::new();
            let (_, auth_success) = handler
                .handle_auth_command(
                    AuthAction::ValidateAndRespond {
                        password: "pass".to_string(),
                    },
                    &mut output,
                    Some("user"),
                )
                .await
                .unwrap();

            if auth_success {
                authenticated.store(true, Ordering::Release);
            }
            authenticated.load(Ordering::Acquire)
        });
    }

    let mut wrong_count = 0;
    let mut correct_count = 0;

    while let Some(result) = set.join_next().await {
        if result.unwrap() {
            correct_count += 1;
        } else {
            wrong_count += 1;
        }
    }

    assert_eq!(wrong_count, 50, "All wrong attempts should fail");
    assert_eq!(correct_count, 50, "All correct attempts should succeed");
}

/// Test that session handlers respect auth_success flag
#[tokio::test]
async fn test_session_handler_respects_auth_success() {
    use test_helpers::create_test_auth_handler;

    // This test documents the expected behavior of session handlers

    let auth_handler = create_test_auth_handler();

    // Simulate session handler behavior
    let mut auth_username: Option<String> = None;
    let authenticated = Arc::new(AtomicBool::new(false));

    // Step 1: AUTHINFO USER
    let action = CommandHandler::classify("AUTHINFO USER user\r\n");
    if let CommandAction::InterceptAuth(AuthAction::RequestPassword(username)) = action {
        auth_username = Some(username.clone());

        let mut output = Vec::new();
        let (_, auth_success) = auth_handler
            .handle_auth_command(
                AuthAction::RequestPassword(username),
                &mut output,
                auth_username.as_deref(),
            )
            .await
            .unwrap();

        // CRITICAL: Only set authenticated if auth_success is true
        if auth_success {
            authenticated.store(true, Ordering::Release);
        }
    }

    assert!(
        !authenticated.load(Ordering::Acquire),
        "USER should not authenticate"
    );

    // Step 2: AUTHINFO PASS with correct password
    let action = CommandHandler::classify("AUTHINFO PASS pass\r\n");
    if let CommandAction::InterceptAuth(AuthAction::ValidateAndRespond { password }) = action {
        let mut output = Vec::new();
        let (_, auth_success) = auth_handler
            .handle_auth_command(
                AuthAction::ValidateAndRespond { password },
                &mut output,
                auth_username.as_deref(),
            )
            .await
            .unwrap();

        // CRITICAL: Only set authenticated if auth_success is true
        if auth_success {
            authenticated.store(true, Ordering::Release);
        }
    }

    assert!(
        authenticated.load(Ordering::Acquire),
        "Valid PASS should authenticate"
    );
}
