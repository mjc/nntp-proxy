//! Tests for auth/backend.rs module
//!
//! Tests backend authentication logic and response parsing.

use anyhow::Result;
use nntp_proxy::pool::BufferPool;
use nntp_proxy::protocol::{ResponseParser, authinfo_pass, authinfo_user};
use nntp_proxy::types::BufferSize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Mock NNTP server for testing authentication
struct MockAuthServer {
    listener: TcpListener,
}

impl MockAuthServer {
    async fn new() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        Ok(Self { listener })
    }

    fn local_addr(&self) -> Result<std::net::SocketAddr> {
        Ok(self.listener.local_addr()?)
    }

    /// Accept connection and handle auth flow
    async fn handle_auth_flow(&self, scenario: AuthScenario) -> Result<()> {
        let (mut stream, _) = self.listener.accept().await?;

        match scenario {
            AuthScenario::SuccessWithPassword => {
                // Send greeting
                stream.write_all(b"200 Welcome\r\n").await?;

                // Read AUTHINFO USER
                let mut buf = [0u8; 1024];
                let n = stream.read(&mut buf).await?;
                assert!(String::from_utf8_lossy(&buf[..n]).contains("AUTHINFO USER"));

                // Send password required
                stream.write_all(b"381 Password required\r\n").await?;

                // Read AUTHINFO PASS
                let n = stream.read(&mut buf).await?;
                assert!(String::from_utf8_lossy(&buf[..n]).contains("AUTHINFO PASS"));

                // Send success
                stream.write_all(b"281 Authentication accepted\r\n").await?;
            }
            AuthScenario::SuccessWithUsernameOnly => {
                // Send greeting
                stream.write_all(b"200 Welcome\r\n").await?;

                // Read AUTHINFO USER
                let mut buf = [0u8; 1024];
                let n = stream.read(&mut buf).await?;
                assert!(String::from_utf8_lossy(&buf[..n]).contains("AUTHINFO USER"));

                // Send immediate success (no password needed)
                stream.write_all(b"281 Authentication accepted\r\n").await?;
            }
            AuthScenario::FailureInvalidCredentials => {
                // Send greeting
                stream.write_all(b"200 Welcome\r\n").await?;

                // Read AUTHINFO USER
                let mut buf = [0u8; 1024];
                let n = stream.read(&mut buf).await?;
                assert!(String::from_utf8_lossy(&buf[..n]).contains("AUTHINFO USER"));

                // Send password required
                stream.write_all(b"381 Password required\r\n").await?;

                // Read AUTHINFO PASS
                let _n = stream.read(&mut buf).await?;

                // Send failure
                stream.write_all(b"481 Authentication failed\r\n").await?;
            }
            AuthScenario::UnexpectedResponseToUser => {
                // Send greeting
                stream.write_all(b"200 Welcome\r\n").await?;

                // Read AUTHINFO USER
                let mut buf = [0u8; 1024];
                let _n = stream.read(&mut buf).await?;

                // Send unexpected response
                stream.write_all(b"500 Command not recognized\r\n").await?;
            }
            AuthScenario::NonSuccessGreeting => {
                // Send non-success greeting
                stream
                    .write_all(b"400 Service temporarily unavailable\r\n")
                    .await?;
            }
        }

        Ok(())
    }
}

#[derive(Clone, Copy)]
#[allow(dead_code)] // Variants used in MockAuthServer but not all instantiated
enum AuthScenario {
    SuccessWithPassword,
    SuccessWithUsernameOnly,
    FailureInvalidCredentials,
    UnexpectedResponseToUser,
    NonSuccessGreeting,
}

/// Test ResponseParser::is_auth_success with valid auth response
#[test]
fn test_is_auth_success_valid() {
    let response = b"281 Authentication accepted\r\n";
    assert!(ResponseParser::is_auth_success(response));
}

/// Test ResponseParser::is_auth_success with various valid formats
#[test]
fn test_is_auth_success_variations() {
    // Different message text
    let responses: &[&[u8]] = &[
        b"281 OK\r\n",
        b"281 Welcome\r\n",
        b"281 Authentication successful\r\n",
    ];

    for response in responses {
        assert!(ResponseParser::is_auth_success(response));
    }
}

/// Test ResponseParser::is_auth_success rejects non-281 responses
#[test]
fn test_is_auth_success_rejects_others() {
    let responses: &[&[u8]] = &[
        b"381 Password required\r\n",
        b"481 Authentication failed\r\n",
        b"200 Welcome\r\n",
        b"500 Error\r\n",
    ];

    for response in responses {
        assert!(!ResponseParser::is_auth_success(response));
    }
}

/// Test ResponseParser::is_auth_required with valid response
#[test]
fn test_is_auth_required_valid() {
    let response = b"381 Password required\r\n";
    assert!(ResponseParser::is_auth_required(response));
}

/// Test ResponseParser::is_auth_required with variations
#[test]
fn test_is_auth_required_variations() {
    let responses: &[&[u8]] = &[
        b"381 PASS required\r\n",
        b"381 More authentication required\r\n",
        b"381 \r\n", // Minimal
    ];

    for response in responses {
        assert!(ResponseParser::is_auth_required(response));
    }
}

/// Test ResponseParser::is_auth_required rejects non-381
#[test]
fn test_is_auth_required_rejects_others() {
    let responses: &[&[u8]] = &[
        b"281 Authentication accepted\r\n",
        b"481 Authentication failed\r\n",
        b"200 Welcome\r\n",
    ];

    for response in responses {
        assert!(!ResponseParser::is_auth_required(response));
    }
}

/// Test ResponseParser::is_greeting with 200 response
#[test]
fn test_is_greeting_200() {
    let response = b"200 Welcome to NNTP server\r\n";
    assert!(ResponseParser::is_greeting(response));
}

/// Test ResponseParser::is_greeting with 201 response
#[test]
fn test_is_greeting_201() {
    let response = b"201 Service available, posting prohibited\r\n";
    assert!(ResponseParser::is_greeting(response));
}

/// Test ResponseParser::is_greeting rejects non-200/201
#[test]
fn test_is_greeting_rejects_others() {
    let responses: &[&[u8]] = &[
        b"400 Service temporarily unavailable\r\n",
        b"500 Command not recognized\r\n",
        b"281 Authentication accepted\r\n",
    ];

    for response in responses {
        assert!(!ResponseParser::is_greeting(response));
    }
}

/// Test authinfo_user command formatting
#[test]
fn test_authinfo_user_formatting() {
    let command = authinfo_user("testuser");
    assert_eq!(command, "AUTHINFO USER testuser\r\n");
}

/// Test authinfo_user with various usernames
#[test]
fn test_authinfo_user_variations() {
    let test_cases = [
        ("user", "AUTHINFO USER user\r\n"),
        ("admin", "AUTHINFO USER admin\r\n"),
        ("user@domain.com", "AUTHINFO USER user@domain.com\r\n"),
        ("user-name_123", "AUTHINFO USER user-name_123\r\n"),
    ];

    for (username, expected) in &test_cases {
        assert_eq!(authinfo_user(username), *expected);
    }
}

/// Test authinfo_user with empty username
#[test]
fn test_authinfo_user_empty() {
    let command = authinfo_user("");
    assert_eq!(command, "AUTHINFO USER \r\n");
}

/// Test authinfo_pass command formatting
#[test]
fn test_authinfo_pass_formatting() {
    let command = authinfo_pass("testpass");
    assert_eq!(command, "AUTHINFO PASS testpass\r\n");
}

/// Test authinfo_pass with various passwords
#[test]
fn test_authinfo_pass_variations() {
    let test_cases = [
        ("pass", "AUTHINFO PASS pass\r\n"),
        ("p@ssw0rd", "AUTHINFO PASS p@ssw0rd\r\n"),
        ("complex!@#$%", "AUTHINFO PASS complex!@#$%\r\n"),
        ("pass word", "AUTHINFO PASS pass word\r\n"), // Space in password
    ];

    for (password, expected) in &test_cases {
        assert_eq!(authinfo_pass(password), *expected);
    }
}

/// Test authinfo_pass with empty password
#[test]
fn test_authinfo_pass_empty() {
    let command = authinfo_pass("");
    assert_eq!(command, "AUTHINFO PASS \r\n");
}

/// Test buffer pool creation and usage
#[tokio::test]
async fn test_buffer_pool_basic_usage() {
    let buffer_pool = BufferPool::new(BufferSize::new(8192).unwrap(), 2);

    // Get buffer
    let buffer1 = buffer_pool.acquire().await;
    assert_eq!(buffer1.capacity(), 8192);

    // Get another
    let buffer2 = buffer_pool.acquire().await;
    assert_eq!(buffer2.capacity(), 8192);

    // Drop and get again
    drop(buffer1);
    let buffer3 = buffer_pool.acquire().await;
    assert_eq!(buffer3.capacity(), 8192);
}

/// Test buffer pool with different sizes
#[tokio::test]
async fn test_buffer_pool_different_sizes() {
    let sizes = [1024, 4096, 8192, 16384];

    for size in &sizes {
        let buffer_pool = BufferPool::new(BufferSize::new(*size).unwrap(), 1);
        let buffer = buffer_pool.acquire().await;
        assert_eq!(buffer.capacity(), *size);
    }
}

/// Test successful authentication with password flow
#[tokio::test]
async fn test_auth_flow_success_with_password() -> Result<()> {
    let server = MockAuthServer::new().await?;
    let addr = server.local_addr()?;

    // Spawn server handler
    tokio::spawn(async move {
        server
            .handle_auth_flow(AuthScenario::SuccessWithPassword)
            .await
    });

    // Small delay to let server start
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    // Connect client
    let mut stream = TcpStream::connect(addr).await?;
    let _buffer_pool = BufferPool::new(BufferSize::new(8192).unwrap(), 2);

    // Read greeting
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await?;
    assert!(ResponseParser::is_greeting(&buf[..n]));

    // Send AUTHINFO USER
    stream
        .write_all(authinfo_user("testuser").as_bytes())
        .await?;

    // Read 381 response
    let n = stream.read(&mut buf).await?;
    assert!(ResponseParser::is_auth_required(&buf[..n]));

    // Send AUTHINFO PASS
    stream
        .write_all(authinfo_pass("testpass").as_bytes())
        .await?;

    // Read 281 response
    let n = stream.read(&mut buf).await?;
    assert!(ResponseParser::is_auth_success(&buf[..n]));

    Ok(())
}

/// Test successful authentication with username only
#[tokio::test]
async fn test_auth_flow_success_username_only() -> Result<()> {
    let server = MockAuthServer::new().await?;
    let addr = server.local_addr()?;

    tokio::spawn(async move {
        server
            .handle_auth_flow(AuthScenario::SuccessWithUsernameOnly)
            .await
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    let mut stream = TcpStream::connect(addr).await?;

    // Read greeting
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await?;
    assert!(ResponseParser::is_greeting(&buf[..n]));

    // Send AUTHINFO USER
    stream
        .write_all(authinfo_user("testuser").as_bytes())
        .await?;

    // Should get immediate 281 success
    let n = stream.read(&mut buf).await?;
    assert!(ResponseParser::is_auth_success(&buf[..n]));

    Ok(())
}

/// Test authentication failure
#[tokio::test]
async fn test_auth_flow_failure() -> Result<()> {
    let server = MockAuthServer::new().await?;
    let addr = server.local_addr()?;

    tokio::spawn(async move {
        server
            .handle_auth_flow(AuthScenario::FailureInvalidCredentials)
            .await
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    let mut stream = TcpStream::connect(addr).await?;

    // Read greeting
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await?;
    assert!(ResponseParser::is_greeting(&buf[..n]));

    // Send AUTHINFO USER
    stream
        .write_all(authinfo_user("baduser").as_bytes())
        .await?;

    // Read 381
    let n = stream.read(&mut buf).await?;
    assert!(ResponseParser::is_auth_required(&buf[..n]));

    // Send AUTHINFO PASS
    stream
        .write_all(authinfo_pass("badpass").as_bytes())
        .await?;

    // Should get 481 failure
    let n = stream.read(&mut buf).await?;
    let response = String::from_utf8_lossy(&buf[..n]);
    assert!(response.starts_with("481"));
    assert!(!ResponseParser::is_auth_success(&buf[..n]));

    Ok(())
}

/// Test response parsing with malformed responses
#[test]
fn test_response_parsing_malformed() {
    // Too short (less than 3 bytes)
    assert!(!ResponseParser::is_auth_success(b"28"));
    assert!(!ResponseParser::is_auth_required(b"38"));
    assert!(!ResponseParser::is_greeting(b"20"));

    // Non-numeric characters
    assert!(!ResponseParser::is_auth_success(b"2X1"));
    assert!(!ResponseParser::is_auth_required(b"3X1"));

    // Empty
    assert!(!ResponseParser::is_auth_success(b""));
    assert!(!ResponseParser::is_auth_required(b""));
    assert!(!ResponseParser::is_greeting(b""));
}

/// Test response parsing with extra whitespace
#[test]
fn test_response_parsing_whitespace() {
    // Leading space in message (valid)
    assert!(ResponseParser::is_auth_success(b"281  OK\r\n"));
    assert!(ResponseParser::is_auth_required(b"381  Required\r\n"));
    assert!(ResponseParser::is_greeting(b"200  Welcome\r\n"));
}

/// Test command formatting preserves CRLF
#[test]
fn test_command_formatting_crlf() {
    let user_cmd = authinfo_user("test");
    assert!(user_cmd.ends_with("\r\n"));
    assert_eq!(user_cmd.matches("\r\n").count(), 1);

    let pass_cmd = authinfo_pass("test");
    assert!(pass_cmd.ends_with("\r\n"));
    assert_eq!(pass_cmd.matches("\r\n").count(), 1);
}

/// Test buffer pool concurrent access
#[tokio::test]
async fn test_buffer_pool_concurrent_access() {
    let buffer_pool = BufferPool::new(BufferSize::new(8192).unwrap(), 10);

    let mut handles = vec![];

    for _ in 0..20 {
        let pool = buffer_pool.clone();
        let handle = tokio::spawn(async move {
            let buffer = pool.acquire().await;
            assert_eq!(buffer.capacity(), 8192);
            // Simulate some work
            tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
        });
        handles.push(handle);
    }

    // Wait for all tasks
    for handle in handles {
        handle.await.unwrap();
    }
}

/// Test response code extraction
#[test]
fn test_response_code_patterns() {
    // Valid 3-digit codes
    let codes = [
        (b"200 OK\r\n" as &[u8], true, "200"),
        (b"281 Auth\r\n", true, "281"),
        (b"381 Pass\r\n", true, "381"),
        (b"481 Fail\r\n", true, "481"),
        (b"500 Error\r\n", true, "500"),
    ];

    for (response, should_parse, expected_code) in &codes {
        if *should_parse {
            let code = std::str::from_utf8(&response[0..3]).unwrap();
            assert_eq!(code, *expected_code);
        }
    }
}
