//! RFC 4643 compliance bug tests
//!
//! Tests for the three AUTHINFO compliance bugs identified in the audit:
//! - Bug 1 (§2.3.2): AUTHINFO PASS without prior USER must return 482
//! - Bug 2 (§2.2): AUTHINFO after successful auth must return 502
//! - Bug 3 (§2.3.1): Unknown AUTHINFO subcommand must return 501

use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::test_helpers::{MockNntpServer, create_test_config_with_auth};
use nntp_proxy::NntpProxy;
use nntp_proxy::config::RoutingMode;

/// Spawn a proxy that accepts a single connection.
///
/// Routes per the `mode` argument. Returns the address the proxy is listening on.
async fn spawn_single_connection_proxy(
    config: nntp_proxy::config::Config,
    mode: RoutingMode,
) -> std::net::SocketAddr {
    let proxy = Arc::new(NntpProxy::new(config, mode).await.unwrap());
    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();

    tokio::spawn(async move {
        if let Ok((stream, addr)) = proxy_listener.accept().await {
            match mode {
                RoutingMode::PerCommand | RoutingMode::Hybrid => {
                    let _ = proxy
                        .handle_client_per_command_routing(stream, addr.into())
                        .await;
                }
                RoutingMode::Stateful => {
                    let _ = proxy.handle_client(stream, addr.into()).await;
                }
            }
        }
    });

    // Small delay to ensure the proxy loop is ready to accept
    tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    proxy_addr
}

// ============================================================================
// Bug 1: AUTHINFO PASS without prior USER → 482 (§2.3.2)
// ============================================================================

/// RFC 4643 §2.3.2: "AUTHINFO PASS command MUST NOT be issued before a
/// successful AUTHINFO USER command … the server MUST respond with response
/// code 482."
#[tokio::test]
async fn test_authinfo_pass_without_user_returns_482() {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_port = backend_listener.local_addr().unwrap().port();
    let _backend = MockNntpServer::new(backend_port).spawn_on_listener(backend_listener);

    let config = create_test_config_with_auth(vec![backend_port], "testuser", "testpass");
    let proxy_addr = spawn_single_connection_proxy(config, RoutingMode::PerCommand).await;

    let client = TcpStream::connect(proxy_addr).await.unwrap();
    let (reader, mut writer) = client.into_split();
    let mut reader = BufReader::new(reader);

    // Read greeting
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert!(
        line.starts_with("200"),
        "Expected 200 greeting, got: {line}"
    );

    // Send AUTHINFO PASS without a preceding AUTHINFO USER
    writer
        .write_all(b"AUTHINFO PASS testpass\r\n")
        .await
        .unwrap();

    line.clear();
    reader.read_line(&mut line).await.unwrap();

    // Bug 1: currently returns 481 (auth failed). Must return 482 (out of sequence).
    assert!(
        line.starts_with("482"),
        "RFC 4643 §2.3.2: AUTHINFO PASS without prior USER must return 482, got: {line}"
    );
}

/// Same as above but in Stateful routing mode to confirm the fix applies there too.
#[tokio::test]
async fn test_authinfo_pass_without_user_returns_482_stateful() {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_port = backend_listener.local_addr().unwrap().port();
    let _backend = MockNntpServer::new(backend_port).spawn_on_listener(backend_listener);

    let config = create_test_config_with_auth(vec![backend_port], "testuser", "testpass");
    let proxy_addr = spawn_single_connection_proxy(config, RoutingMode::Stateful).await;

    let client = TcpStream::connect(proxy_addr).await.unwrap();
    let (reader, mut writer) = client.into_split();
    let mut reader = BufReader::new(reader);

    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert!(
        line.starts_with("200"),
        "Expected 200 greeting, got: {line}"
    );

    writer
        .write_all(b"AUTHINFO PASS testpass\r\n")
        .await
        .unwrap();

    line.clear();
    reader.read_line(&mut line).await.unwrap();

    assert!(
        line.starts_with("482"),
        "RFC 4643 §2.3.2: AUTHINFO PASS without prior USER must return 482 (stateful mode), got: {line}"
    );
}

// ============================================================================
// Bug 2: AUTHINFO after successful authentication → 502 (§2.2)
// ============================================================================

/// RFC 4643 §2.2: "Once a client has successfully authenticated … any
/// subsequent AUTHINFO commands … MUST be rejected with a 502 response."
#[tokio::test]
async fn test_authinfo_after_auth_returns_502_per_command() {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_port = backend_listener.local_addr().unwrap().port();
    let _backend = MockNntpServer::new(backend_port).spawn_on_listener(backend_listener);

    let config = create_test_config_with_auth(vec![backend_port], "alice", "wonderland");
    let proxy_addr = spawn_single_connection_proxy(config, RoutingMode::PerCommand).await;

    let client = TcpStream::connect(proxy_addr).await.unwrap();
    let (reader, mut writer) = client.into_split();
    let mut reader = BufReader::new(reader);

    let mut line = String::new();

    // Greeting
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("200"), "Expected 200, got: {line}");

    // Authenticate successfully
    writer.write_all(b"AUTHINFO USER alice\r\n").await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("381"), "Expected 381, got: {line}");

    writer
        .write_all(b"AUTHINFO PASS wonderland\r\n")
        .await
        .unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(
        line.starts_with("281"),
        "Expected 281 after valid credentials, got: {line}"
    );

    // Now send AUTHINFO USER again — already authenticated
    writer.write_all(b"AUTHINFO USER alice\r\n").await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();

    // Bug 2: currently returns 381 (starts a fresh auth). Must return 502.
    assert!(
        line.starts_with("502"),
        "RFC 4643 §2.2: AUTHINFO after successful auth must return 502 (per-command mode), got: {line}"
    );
}

/// Same check in Stateful routing mode — the hot-path forwarding must also be intercepted.
#[tokio::test]
async fn test_authinfo_after_auth_returns_502_stateful() {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_port = backend_listener.local_addr().unwrap().port();
    let _backend = MockNntpServer::new(backend_port).spawn_on_listener(backend_listener);

    let config = create_test_config_with_auth(vec![backend_port], "alice", "wonderland");
    let proxy_addr = spawn_single_connection_proxy(config, RoutingMode::Stateful).await;

    let client = TcpStream::connect(proxy_addr).await.unwrap();
    let (reader, mut writer) = client.into_split();
    let mut reader = BufReader::new(reader);

    let mut line = String::new();

    // Greeting
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("200"), "Expected 200, got: {line}");

    // Authenticate successfully
    writer.write_all(b"AUTHINFO USER alice\r\n").await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("381"), "Expected 381, got: {line}");

    writer
        .write_all(b"AUTHINFO PASS wonderland\r\n")
        .await
        .unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(
        line.starts_with("281"),
        "Expected 281 after valid credentials, got: {line}"
    );

    // Now send AUTHINFO USER again — already authenticated
    writer.write_all(b"AUTHINFO USER alice\r\n").await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();

    // Bug 2 (stateful): currently gets forwarded to backend which returns "200 OK".
    // Must return 502.
    assert!(
        line.starts_with("502"),
        "RFC 4643 §2.2: AUTHINFO after successful auth must return 502 (stateful mode), got: {line}"
    );
}

// ============================================================================
// Bug 3: Unknown AUTHINFO subcommand → 501 (§2.3.1)
// ============================================================================

/// RFC 4643 §2.2 + §2.3.1: After authentication, even an unknown AUTHINFO subcommand
/// must return 502 (already authenticated) rather than 501 (syntax error). The §2.2
/// rule that all AUTHINFO commands must be rejected post-auth takes precedence.
#[tokio::test]
async fn test_authinfo_unknown_subcommand_returns_502_when_authenticated() {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_port = backend_listener.local_addr().unwrap().port();
    let _backend = MockNntpServer::new(backend_port).spawn_on_listener(backend_listener);

    let config = create_test_config_with_auth(vec![backend_port], "alice", "wonderland");
    let proxy_addr = spawn_single_connection_proxy(config, RoutingMode::PerCommand).await;

    let client = TcpStream::connect(proxy_addr).await.unwrap();
    let (reader, mut writer) = client.into_split();
    let mut reader = BufReader::new(reader);

    let mut line = String::new();

    // Authenticate first
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("200"));

    writer.write_all(b"AUTHINFO USER alice\r\n").await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("381"));

    writer
        .write_all(b"AUTHINFO PASS wonderland\r\n")
        .await
        .unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("281"), "Expected 281, got: {line}");

    // Send AUTHINFO with an unknown subcommand after authentication.
    // RFC 4643 §2.2 takes precedence: 502 (already authenticated) rather than 501 (unknown cmd).
    writer
        .write_all(b"AUTHINFO GENERIC some-mechanism\r\n")
        .await
        .unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();

    assert!(
        line.starts_with("502"),
        "RFC 4643 §2.2: Any AUTHINFO after successful auth must return 502, got: {line}"
    );
}

/// Unknown AUTHINFO subcommand before authentication should also return 501,
/// not 480 (auth-required).
#[tokio::test]
async fn test_authinfo_unknown_subcommand_returns_501_unauthenticated() {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_port = backend_listener.local_addr().unwrap().port();
    let _backend = MockNntpServer::new(backend_port).spawn_on_listener(backend_listener);

    let config = create_test_config_with_auth(vec![backend_port], "alice", "wonderland");
    let proxy_addr = spawn_single_connection_proxy(config, RoutingMode::PerCommand).await;

    let client = TcpStream::connect(proxy_addr).await.unwrap();
    let (reader, mut writer) = client.into_split();
    let mut reader = BufReader::new(reader);

    let mut line = String::new();

    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("200"));

    // Send AUTHINFO with an unknown subcommand before authenticating
    writer
        .write_all(b"AUTHINFO GENERIC some-mechanism\r\n")
        .await
        .unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();

    // Should return 501 (syntax error), not 480 (auth required)
    assert!(
        line.starts_with("501"),
        "RFC 4643 §2.3.1: Unknown AUTHINFO subcommand must return 501 even unauthenticated, got: {line}"
    );
}
