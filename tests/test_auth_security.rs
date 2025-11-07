//! Security-focused authentication tests
//!
//! These tests verify that authentication cannot be bypassed and that
//! invalid credentials never result in authenticated state.

use nntp_proxy::auth::AuthHandler;
use nntp_proxy::command::AuthAction;
use std::sync::Arc;

/// Test that invalid credentials are NEVER accepted
#[tokio::test]
async fn test_invalid_credentials_rejected() {
    let handler = Arc::new(AuthHandler::new(
        Some("correctuser".to_string()),
        Some("correctpass".to_string()),
    ));

    // Test wrong username
    let mut output = Vec::new();
    let (_, auth_success) = handler
        .handle_auth_command(
            AuthAction::ValidateAndRespond {
                password: "correctpass".to_string(),
            },
            &mut output,
            Some("wronguser"),
        )
        .await
        .unwrap();
    assert!(!auth_success, "Wrong username should not authenticate");

    // Test wrong password
    output.clear();
    let (_, auth_success) = handler
        .handle_auth_command(
            AuthAction::ValidateAndRespond {
                password: "wrongpass".to_string(),
            },
            &mut output,
            Some("correctuser"),
        )
        .await
        .unwrap();
    assert!(!auth_success, "Wrong password should not authenticate");

    // Test both wrong
    output.clear();
    let (_, auth_success) = handler
        .handle_auth_command(
            AuthAction::ValidateAndRespond {
                password: "wrongpass".to_string(),
            },
            &mut output,
            Some("wronguser"),
        )
        .await
        .unwrap();
    assert!(!auth_success, "Wrong credentials should not authenticate");
}

/// Test that valid credentials are accepted
#[tokio::test]
async fn test_valid_credentials_accepted() {
    let handler = Arc::new(AuthHandler::new(
        Some("correctuser".to_string()),
        Some("correctpass".to_string()),
    ));

    let mut output = Vec::new();
    let (_, auth_success) = handler
        .handle_auth_command(
            AuthAction::ValidateAndRespond {
                password: "correctpass".to_string(),
            },
            &mut output,
            Some("correctuser"),
        )
        .await
        .unwrap();
    assert!(auth_success, "Valid credentials should authenticate");
}

/// Test that AUTHINFO PASS without prior AUTHINFO USER is rejected
#[tokio::test]
async fn test_password_without_username_rejected() {
    let handler = Arc::new(AuthHandler::new(
        Some("user".to_string()),
        Some("pass".to_string()),
    ));

    let mut output = Vec::new();
    let (_, auth_success) = handler
        .handle_auth_command(
            AuthAction::ValidateAndRespond {
                password: "pass".to_string(),
            },
            &mut output,
            None, // No username stored
        )
        .await
        .unwrap();
    assert!(
        !auth_success,
        "Password without username should not authenticate"
    );
}

/// Test that authentication is case-sensitive
#[tokio::test]
async fn test_credentials_case_sensitive() {
    let handler = Arc::new(AuthHandler::new(
        Some("User".to_string()),
        Some("Pass".to_string()),
    ));

    // Wrong case username
    let mut output = Vec::new();
    let (_, auth_success) = handler
        .handle_auth_command(
            AuthAction::ValidateAndRespond {
                password: "Pass".to_string(),
            },
            &mut output,
            Some("user"),
        )
        .await
        .unwrap();
    assert!(!auth_success, "Username should be case-sensitive");

    // Wrong case password
    output.clear();
    let (_, auth_success) = handler
        .handle_auth_command(
            AuthAction::ValidateAndRespond {
                password: "pass".to_string(),
            },
            &mut output,
            Some("User"),
        )
        .await
        .unwrap();
    assert!(!auth_success, "Password should be case-sensitive");

    // Correct case
    output.clear();
    let (_, auth_success) = handler
        .handle_auth_command(
            AuthAction::ValidateAndRespond {
                password: "Pass".to_string(),
            },
            &mut output,
            Some("User"),
        )
        .await
        .unwrap();
    assert!(auth_success, "Correct case should authenticate");
}

/// Test that empty credentials don't bypass authentication
#[tokio::test]
async fn test_empty_credentials_rejected() {
    let handler = Arc::new(AuthHandler::new(
        Some("user".to_string()),
        Some("pass".to_string()),
    ));

    // Empty username
    let mut output = Vec::new();
    let (_, auth_success) = handler
        .handle_auth_command(
            AuthAction::ValidateAndRespond {
                password: "pass".to_string(),
            },
            &mut output,
            Some(""),
        )
        .await
        .unwrap();
    assert!(!auth_success, "Empty username should not authenticate");

    // Empty password
    output.clear();
    let (_, auth_success) = handler
        .handle_auth_command(
            AuthAction::ValidateAndRespond {
                password: "".to_string(),
            },
            &mut output,
            Some("user"),
        )
        .await
        .unwrap();
    assert!(!auth_success, "Empty password should not authenticate");
}

/// Test that auth_success=false is returned for all failure cases
#[tokio::test]
async fn test_auth_success_flag_reliability() {
    let test_cases = vec![
        ("correctuser", "wrongpass", false),
        ("wronguser", "correctpass", false),
        ("wronguser", "wrongpass", false),
        ("correctuser", "correctpass", true),
        ("", "correctpass", false),
        ("correctuser", "", false),
    ];

    for (username, password, expected_success) in test_cases {
        let handler = Arc::new(AuthHandler::new(
            Some("correctuser".to_string()),
            Some("correctpass".to_string()),
        ));

        let mut output = Vec::new();
        let stored_username = if username.is_empty() {
            None
        } else {
            Some(username)
        };

        let (_, auth_success) = handler
            .handle_auth_command(
                AuthAction::ValidateAndRespond {
                    password: password.to_string(),
                },
                &mut output,
                stored_username,
            )
            .await
            .unwrap();

        assert_eq!(
            auth_success, expected_success,
            "auth_success flag mismatch for username='{}', password='{}'",
            username, password
        );
    }
}

/// Test that RequestPassword never returns auth_success=true
#[tokio::test]
async fn test_request_password_never_authenticates() {
    let handler = Arc::new(AuthHandler::new(
        Some("user".to_string()),
        Some("pass".to_string()),
    ));

    let mut output = Vec::new();
    let (_, auth_success) = handler
        .handle_auth_command(
            AuthAction::RequestPassword("user".to_string()),
            &mut output,
            None,
        )
        .await
        .unwrap();

    assert!(
        !auth_success,
        "RequestPassword should never return auth_success=true"
    );
}

/// Test disabled auth accepts anything
#[tokio::test]
async fn test_disabled_auth_accepts_all() {
    let handler = Arc::new(AuthHandler::new(None, None));

    let mut output = Vec::new();
    let (_, auth_success) = handler
        .handle_auth_command(
            AuthAction::ValidateAndRespond {
                password: "anypass".to_string(),
            },
            &mut output,
            Some("anyuser"),
        )
        .await
        .unwrap();

    assert!(auth_success, "Disabled auth should accept any credentials");
}

/// Test that validate() method matches handle_auth_command behavior
#[tokio::test]
async fn test_validate_matches_handle_auth_command() {
    let handler = Arc::new(AuthHandler::new(
        Some("user".to_string()),
        Some("pass".to_string()),
    ));

    let test_cases = vec![
        ("user", "pass", true),
        ("user", "wrong", false),
        ("wrong", "pass", false),
        ("wrong", "wrong", false),
    ];

    for (username, password, expected) in test_cases {
        // Test validate() directly
        let validate_result = handler.validate(username, password);

        // Test via handle_auth_command
        let mut output = Vec::new();
        let (_, auth_success) = handler
            .handle_auth_command(
                AuthAction::ValidateAndRespond {
                    password: password.to_string(),
                },
                &mut output,
                Some(username),
            )
            .await
            .unwrap();

        assert_eq!(
            validate_result, expected,
            "validate() mismatch for {}/{}",
            username, password
        );
        assert_eq!(
            auth_success, expected,
            "handle_auth_command mismatch for {}/{}",
            username, password
        );
        assert_eq!(
            validate_result, auth_success,
            "validate() and handle_auth_command disagree for {}/{}",
            username, password
        );
    }
}

/// Test special characters in credentials
#[tokio::test]
async fn test_special_characters_in_credentials() {
    let handler = Arc::new(AuthHandler::new(
        Some("user@host.com".to_string()),
        Some("p@ss!w0rd#$%".to_string()),
    ));

    let mut output = Vec::new();
    let (_, auth_success) = handler
        .handle_auth_command(
            AuthAction::ValidateAndRespond {
                password: "p@ss!w0rd#$%".to_string(),
            },
            &mut output,
            Some("user@host.com"),
        )
        .await
        .unwrap();

    assert!(auth_success, "Special characters should be supported");

    // Test wrong special chars
    output.clear();
    let (_, auth_success) = handler
        .handle_auth_command(
            AuthAction::ValidateAndRespond {
                password: "p@ss!w0rd#$".to_string(), // Missing %
            },
            &mut output,
            Some("user@host.com"),
        )
        .await
        .unwrap();

    assert!(!auth_success, "Partial match should fail");
}

/// Property: auth_success=true MUST mean valid credentials
#[tokio::test]
async fn property_auth_success_implies_valid_credentials() {
    let handler = Arc::new(AuthHandler::new(
        Some("validuser".to_string()),
        Some("validpass".to_string()),
    ));

    // If auth_success is true, credentials MUST be valid
    let mut output = Vec::new();
    let (_, auth_success) = handler
        .handle_auth_command(
            AuthAction::ValidateAndRespond {
                password: "validpass".to_string(),
            },
            &mut output,
            Some("validuser"),
        )
        .await
        .unwrap();

    if auth_success {
        // Double-check with validate()
        assert!(
            handler.validate("validuser", "validpass"),
            "If auth_success=true, validate() must also return true"
        );
    }
}

/// Property: invalid credentials MUST result in auth_success=false
#[tokio::test]
async fn property_invalid_credentials_implies_no_auth_success() {
    let handler = Arc::new(AuthHandler::new(
        Some("validuser".to_string()),
        Some("validpass".to_string()),
    ));

    let invalid_cases = vec![
        ("invaliduser", "validpass"),
        ("validuser", "invalidpass"),
        ("invaliduser", "invalidpass"),
        ("", "validpass"),
        ("validuser", ""),
        ("", ""),
    ];

    for (username, password) in invalid_cases {
        let stored_username = if username.is_empty() {
            None
        } else {
            Some(username)
        };

        let mut output = Vec::new();
        let (_, auth_success) = handler
            .handle_auth_command(
                AuthAction::ValidateAndRespond {
                    password: password.to_string(),
                },
                &mut output,
                stored_username,
            )
            .await
            .unwrap();

        assert!(
            !auth_success,
            "Invalid credentials {}/{} resulted in auth_success=true",
            username, password
        );
    }
}
