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

use crate::test_helpers::{MockNntpServer, wait_for_server};

use nntp_proxy::config::{RoutingMode, UserCredentials};

async fn spawn_proxy_with_auth(
    backend_port: u16,
    username: &str,
    password: &str,
    routing_mode: RoutingMode,
) -> std::net::SocketAddr {
    use crate::test_helpers::create_test_config;
    use nntp_proxy::config::ClientAuth;

    let mut config = create_test_config(vec![(backend_port, "backend-1")]);
    config.client_auth = ClientAuth {
        users: vec![UserCredentials {
            username: username.to_string(),
            password: password.to_string(),
        }],
        greeting: None,
    };

    let proxy = Arc::new(
        nntp_proxy::NntpProxy::new(config, routing_mode)
            .await
            .unwrap(),
    );
    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();

    let proxy_clone = proxy.clone();
    tokio::spawn(async move {
        if let Ok((stream, addr)) = proxy_listener.accept().await {
            let _ = match routing_mode {
                RoutingMode::PerCommand | RoutingMode::Hybrid => {
                    proxy_clone
                        .handle_client_per_command_routing(stream, addr.into())
                        .await
                }
                RoutingMode::Stateful => proxy_clone.handle_client(stream, addr.into()).await,
            };
        }
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    proxy_addr
}

async fn read_multiline_response<R>(reader: &mut BufReader<R>) -> String
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut response = String::new();

    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line).await.unwrap();
        assert!(bytes > 0, "unexpected EOF while reading multiline response");
        response.push_str(&line);
        if line == ".\r\n" {
            break;
        }
    }

    response
}

#[tokio::test]
async fn test_auth_flow_complete_with_valid_credentials() {
    // Use a specific port for testing
    let backend_port = 19119;

    // Start mock backend
    let _backend_handle = crate::test_helpers::spawn_mock_server(backend_port, "test-backend");
    wait_for_server(&format!("127.0.0.1:{}", backend_port), 10)
        .await
        .unwrap();

    // Create config with auth
    use crate::test_helpers::create_test_config;
    use nntp_proxy::config::ClientAuth;

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
        nntp_proxy::NntpProxy::new(config, nntp_proxy::config::RoutingMode::Stateful)
            .await
            .unwrap(),
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
async fn test_capabilities_allowed_before_auth_in_stateful_mode() {
    let backend_port = 19123;
    let _backend_handle = crate::test_helpers::spawn_mock_server(backend_port, "test-backend");
    wait_for_server(&format!("127.0.0.1:{backend_port}"), 10)
        .await
        .unwrap();

    let proxy_addr =
        spawn_proxy_with_auth(backend_port, "user", "pass", RoutingMode::Stateful).await;

    let mut client = TcpStream::connect(proxy_addr).await.unwrap();
    let (reader, mut writer) = client.split();
    let mut reader = BufReader::new(reader);

    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("200"));

    writer.write_all(b"CAPABILITIES\r\n").await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(
        line.starts_with("200"),
        "unexpected CAPABILITIES response: {line:?}"
    );
}

#[tokio::test]
async fn test_capabilities_allowed_before_auth_in_per_command_mode() {
    let backend_port = 19126;
    let _backend_handle = crate::test_helpers::spawn_mock_server(backend_port, "test-backend");
    wait_for_server(&format!("127.0.0.1:{backend_port}"), 10)
        .await
        .unwrap();

    let proxy_addr =
        spawn_proxy_with_auth(backend_port, "user", "pass", RoutingMode::PerCommand).await;

    let mut client = TcpStream::connect(proxy_addr).await.unwrap();
    let (reader, mut writer) = client.split();
    let mut reader = BufReader::new(reader);

    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("200"));

    writer.write_all(b"cApAbIlItIeS\r\n").await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(
        line.starts_with("200"),
        "unexpected CAPABILITIES response: {line:?}"
    );
}

#[tokio::test]
async fn test_capabilities_advertise_proxy_auth_before_authentication() {
    for (backend_port, routing_mode) in [
        (19131, RoutingMode::Stateful),
        (19132, RoutingMode::PerCommand),
    ] {
        let _backend_handle = MockNntpServer::new(backend_port)
            .with_name("test-backend")
            .on_command(
                "CAPABILITIES",
                "101 Capability list\r\nVERSION 2\r\nREADER\r\nMODE-READER\r\nAUTHINFO SASL\r\nSASL PLAIN\r\n.\r\n",
            )
            .spawn();
        wait_for_server(&format!("127.0.0.1:{backend_port}"), 10)
            .await
            .unwrap();

        let proxy_addr = spawn_proxy_with_auth(backend_port, "user", "pass", routing_mode).await;

        let mut client = TcpStream::connect(proxy_addr).await.unwrap();
        let (reader, mut writer) = client.split();
        let mut reader = BufReader::new(reader);

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert!(line.starts_with("200"));

        writer.write_all(b"CAPABILITIES\r\n").await.unwrap();
        let response = read_multiline_response(&mut reader).await;

        assert!(
            response.contains("\r\nAUTHINFO USER\r\n"),
            "routing_mode={routing_mode:?} response={response:?}"
        );
        assert!(
            !response.contains("AUTHINFO SASL"),
            "routing_mode={routing_mode:?} response={response:?}"
        );
        assert!(
            !response.contains("\r\nSASL "),
            "routing_mode={routing_mode:?} response={response:?}"
        );
        assert!(
            !response.contains("\r\nMODE-READER\r\n"),
            "routing_mode={routing_mode:?} response={response:?}"
        );
    }
}

#[tokio::test]
async fn test_capabilities_hide_auth_after_successful_authentication() {
    for (backend_port, routing_mode) in [
        (19133, RoutingMode::Stateful),
        (19134, RoutingMode::PerCommand),
    ] {
        let _backend_handle = MockNntpServer::new(backend_port)
            .with_name("test-backend")
            .on_command(
                "CAPABILITIES",
                "101 Capability list\r\nVERSION 2\r\nREADER\r\nMODE-READER\r\nAUTHINFO USER SASL\r\nSASL PLAIN\r\n.\r\n",
            )
            .spawn();
        wait_for_server(&format!("127.0.0.1:{backend_port}"), 10)
            .await
            .unwrap();

        let proxy_addr = spawn_proxy_with_auth(backend_port, "user", "pass", routing_mode).await;

        let mut client = TcpStream::connect(proxy_addr).await.unwrap();
        let (reader, mut writer) = client.split();
        let mut reader = BufReader::new(reader);

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert!(line.starts_with("200"));

        writer.write_all(b"AUTHINFO USER user\r\n").await.unwrap();
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        assert!(line.starts_with("381"));

        writer.write_all(b"AUTHINFO PASS pass\r\n").await.unwrap();
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        assert!(line.starts_with("281"));

        writer.write_all(b"CAPABILITIES\r\n").await.unwrap();
        let response = read_multiline_response(&mut reader).await;

        assert!(
            !response.contains("\r\nAUTHINFO"),
            "routing_mode={routing_mode:?} response={response:?}"
        );
        assert!(
            !response.contains("\r\nSASL "),
            "routing_mode={routing_mode:?} response={response:?}"
        );
        assert!(
            !response.contains("\r\nMODE-READER\r\n"),
            "routing_mode={routing_mode:?} response={response:?}"
        );
        assert!(
            response.contains("\r\nREADER\r\n"),
            "routing_mode={routing_mode:?} response={response:?}"
        );
    }
}

#[tokio::test]
async fn test_mode_reader_rejected_after_successful_authentication_in_stateful_mode() {
    let backend_port = 19135;
    let _backend_handle = MockNntpServer::new(backend_port)
        .with_name("test-backend")
        .on_command("MODE", "201 Reader mode acknowledged\r\n")
        .spawn();
    wait_for_server(&format!("127.0.0.1:{backend_port}"), 10)
        .await
        .unwrap();

    let proxy_addr =
        spawn_proxy_with_auth(backend_port, "user", "pass", RoutingMode::Stateful).await;

    let mut client = TcpStream::connect(proxy_addr).await.unwrap();
    let (reader, mut writer) = client.split();
    let mut reader = BufReader::new(reader);

    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("200"));

    writer.write_all(b"AUTHINFO USER user\r\n").await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("381"));

    writer.write_all(b"AUTHINFO PASS pass\r\n").await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("281"));

    writer.write_all(b"MODE READER\r\n").await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert_eq!(line, "502 Command unavailable after authentication\r\n");
}

#[tokio::test]
async fn test_mode_reader_rejected_after_successful_authentication_in_per_command_mode() {
    let backend_port = 19136;
    let _backend_handle = MockNntpServer::new(backend_port)
        .with_name("test-backend")
        .on_command("MODE", "201 Reader mode acknowledged\r\n")
        .spawn();
    wait_for_server(&format!("127.0.0.1:{backend_port}"), 10)
        .await
        .unwrap();

    let proxy_addr =
        spawn_proxy_with_auth(backend_port, "user", "pass", RoutingMode::PerCommand).await;

    let mut client = TcpStream::connect(proxy_addr).await.unwrap();
    let (reader, mut writer) = client.split();
    let mut reader = BufReader::new(reader);

    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("200"));

    writer.write_all(b"AUTHINFO USER user\r\n").await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("381"));

    writer.write_all(b"AUTHINFO PASS pass\r\n").await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("281"));

    writer.write_all(b"MODE READER\r\n").await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert_eq!(line, "502 Command unavailable after authentication\r\n");
}

#[tokio::test]
async fn test_authinfo_user_without_username_rejected_as_syntax_error() {
    for (backend_port, routing_mode) in [
        (19137, RoutingMode::Stateful),
        (19138, RoutingMode::PerCommand),
    ] {
        let _backend_handle = crate::test_helpers::spawn_mock_server(backend_port, "test-backend");
        wait_for_server(&format!("127.0.0.1:{backend_port}"), 10)
            .await
            .unwrap();

        let proxy_addr = spawn_proxy_with_auth(backend_port, "user", "pass", routing_mode).await;

        let mut client = TcpStream::connect(proxy_addr).await.unwrap();
        let (reader, mut writer) = client.split();
        let mut reader = BufReader::new(reader);

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert!(line.starts_with("200"));

        writer.write_all(b"AUTHINFO USER\r\n").await.unwrap();
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(
            line, "501 Command syntax error\r\n",
            "routing_mode={routing_mode:?}"
        );
    }
}

#[tokio::test]
async fn test_authinfo_pass_without_password_rejected_as_syntax_error() {
    for (backend_port, routing_mode) in [
        (19139, RoutingMode::Stateful),
        (19140, RoutingMode::PerCommand),
    ] {
        let _backend_handle = crate::test_helpers::spawn_mock_server(backend_port, "test-backend");
        wait_for_server(&format!("127.0.0.1:{backend_port}"), 10)
            .await
            .unwrap();

        let proxy_addr = spawn_proxy_with_auth(backend_port, "user", "pass", routing_mode).await;

        let mut client = TcpStream::connect(proxy_addr).await.unwrap();
        let (reader, mut writer) = client.split();
        let mut reader = BufReader::new(reader);

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert!(line.starts_with("200"));

        writer.write_all(b"AUTHINFO USER user\r\n").await.unwrap();
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        assert!(line.starts_with("381"));

        writer.write_all(b"AUTHINFO PASS\r\n").await.unwrap();
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(
            line, "501 Command syntax error\r\n",
            "routing_mode={routing_mode:?}"
        );
    }
}

#[tokio::test]
async fn test_authinfo_sasl_without_mechanism_rejected_as_syntax_error() {
    for (backend_port, routing_mode) in [
        (19141, RoutingMode::Stateful),
        (19142, RoutingMode::PerCommand),
    ] {
        let _backend_handle = crate::test_helpers::spawn_mock_server(backend_port, "test-backend");
        wait_for_server(&format!("127.0.0.1:{backend_port}"), 10)
            .await
            .unwrap();

        let proxy_addr = spawn_proxy_with_auth(backend_port, "user", "pass", routing_mode).await;

        let mut client = TcpStream::connect(proxy_addr).await.unwrap();
        let (reader, mut writer) = client.split();
        let mut reader = BufReader::new(reader);

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert!(line.starts_with("200"));

        writer.write_all(b"AUTHINFO SASL\r\n").await.unwrap();
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(
            line, "501 Command syntax error\r\n",
            "routing_mode={routing_mode:?}"
        );
    }
}

#[tokio::test]
async fn test_authinfo_sasl_with_unsupported_mechanism_rejected_with_503() {
    for (backend_port, routing_mode) in [
        (19143, RoutingMode::Stateful),
        (19144, RoutingMode::PerCommand),
    ] {
        let _backend_handle = crate::test_helpers::spawn_mock_server(backend_port, "test-backend");
        wait_for_server(&format!("127.0.0.1:{backend_port}"), 10)
            .await
            .unwrap();

        let proxy_addr = spawn_proxy_with_auth(backend_port, "user", "pass", routing_mode).await;

        let mut client = TcpStream::connect(proxy_addr).await.unwrap();
        let (reader, mut writer) = client.split();
        let mut reader = BufReader::new(reader);

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert!(line.starts_with("200"));

        writer
            .write_all(b"AUTHINFO SASL EXAMPLE\r\n")
            .await
            .unwrap();
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(
            line, "503 Mechanism not recognized\r\n",
            "routing_mode={routing_mode:?}"
        );
    }
}

#[tokio::test]
async fn test_quit_allowed_before_auth_in_stateful_mode() {
    let backend_port = 19124;
    let _backend_handle = crate::test_helpers::spawn_mock_server(backend_port, "test-backend");
    wait_for_server(&format!("127.0.0.1:{backend_port}"), 10)
        .await
        .unwrap();

    let proxy_addr =
        spawn_proxy_with_auth(backend_port, "user", "pass", RoutingMode::Stateful).await;

    let mut client = TcpStream::connect(proxy_addr).await.unwrap();
    let (reader, mut writer) = client.split();
    let mut reader = BufReader::new(reader);

    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("200"));

    writer.write_all(b"QUIT\r\n").await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("205"));
}

#[tokio::test]
async fn test_authinfo_rejected_after_successful_authentication() {
    let backend_port = 19125;
    let _backend_handle = crate::test_helpers::spawn_mock_server(backend_port, "test-backend");
    wait_for_server(&format!("127.0.0.1:{backend_port}"), 10)
        .await
        .unwrap();

    let proxy_addr =
        spawn_proxy_with_auth(backend_port, "user", "pass", RoutingMode::Stateful).await;

    let mut client = TcpStream::connect(proxy_addr).await.unwrap();
    let (reader, mut writer) = client.split();
    let mut reader = BufReader::new(reader);

    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("200"));

    writer.write_all(b"AUTHINFO USER user\r\n").await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("381"));

    writer.write_all(b"AUTHINFO PASS pass\r\n").await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("281"));

    writer.write_all(b"AuthInfo\tUser user\r\n").await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("502"));
}

#[tokio::test]
async fn test_pass_before_user_returns_sequence_error_in_stateful_mode() {
    let backend_port = 19127;
    let _backend_handle = crate::test_helpers::spawn_mock_server(backend_port, "test-backend");
    wait_for_server(&format!("127.0.0.1:{backend_port}"), 10)
        .await
        .unwrap();

    let proxy_addr =
        spawn_proxy_with_auth(backend_port, "user", "pass", RoutingMode::Stateful).await;

    let mut client = TcpStream::connect(proxy_addr).await.unwrap();
    let (reader, mut writer) = client.split();
    let mut reader = BufReader::new(reader);

    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("200"));

    writer.write_all(b"AUTHINFO PASS pass\r\n").await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert_eq!(
        line,
        "482 Authentication commands issued out of sequence\r\n"
    );
}

#[tokio::test]
async fn test_authinfo_rejected_after_successful_authentication_in_per_command_mode() {
    let backend_port = 19128;
    let _backend_handle = crate::test_helpers::spawn_mock_server(backend_port, "test-backend");
    wait_for_server(&format!("127.0.0.1:{backend_port}"), 10)
        .await
        .unwrap();

    let proxy_addr =
        spawn_proxy_with_auth(backend_port, "user", "pass", RoutingMode::PerCommand).await;

    let mut client = TcpStream::connect(proxy_addr).await.unwrap();
    let (reader, mut writer) = client.split();
    let mut reader = BufReader::new(reader);

    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("200"));

    writer.write_all(b"AUTHINFO USER user\r\n").await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("381"));

    writer.write_all(b"AUTHINFO PASS pass\r\n").await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("281"));

    writer.write_all(b"AUTHINFO PASS pass\r\n").await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert_eq!(
        line,
        "502 Authentication commands invalid after authentication\r\n"
    );
}

#[tokio::test]
async fn test_auth_disabled_allows_immediate_commands() {
    let backend_port = 19120;

    // Start mock backend
    let _backend_handle = crate::test_helpers::spawn_mock_server(backend_port, "test-backend");
    wait_for_server(&format!("127.0.0.1:{}", backend_port), 10)
        .await
        .unwrap();

    // Create config WITHOUT auth
    use crate::test_helpers::create_test_config;
    let config = create_test_config(vec![(backend_port, "backend-1")]);

    // Ensure auth is disabled
    assert!(!config.client_auth.is_enabled());

    // Start proxy
    let proxy = Arc::new(
        nntp_proxy::NntpProxy::new(config, nntp_proxy::config::RoutingMode::Stateful)
            .await
            .unwrap(),
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
    let _backend_handle = crate::test_helpers::spawn_mock_server(backend_port, "test-backend");
    wait_for_server(&format!("127.0.0.1:{}", backend_port), 10)
        .await
        .unwrap();

    use crate::test_helpers::create_test_config;
    use nntp_proxy::config::ClientAuth;

    let mut config = create_test_config(vec![(backend_port, "backend-1")]);
    config.client_auth = ClientAuth {
        users: vec![UserCredentials {
            username: "user".to_string(),
            password: "pass".to_string(),
        }],
        greeting: None,
    };

    let proxy = Arc::new(
        nntp_proxy::NntpProxy::new(config, nntp_proxy::config::RoutingMode::Stateful)
            .await
            .unwrap(),
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

    use crate::test_helpers::create_test_config;
    use nntp_proxy::config::ClientAuth;
    use tokio::task::JoinSet;

    // Start mock backend
    let _backend_handle = crate::test_helpers::spawn_mock_server(backend_port, "test-backend");
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
        nntp_proxy::NntpProxy::new(config, nntp_proxy::config::RoutingMode::Stateful)
            .await
            .unwrap(),
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
    use crate::test_helpers::create_test_auth_handler_with;
    use nntp_proxy::command::{AuthAction, CommandAction, CommandHandler};

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
