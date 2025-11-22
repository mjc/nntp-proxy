//! Test helpers for integration tests
//!
//! This module provides reusable test utilities to reduce duplication
//! in integration tests.

use anyhow::Result;
use nntp_proxy::config::{Config, Server};
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::AbortHandle;

/// Builder for creating mock NNTP servers with custom behavior
///
/// This builder eliminates the need to duplicate mock server code across tests.
/// Supports authentication, custom command handlers, and various response patterns.
///
/// # Examples
///
/// Basic server:
/// ```ignore
/// let handle = MockNntpServer::new(8119)
///     .with_name("TestServer")
///     .spawn();
/// ```
///
/// Server with authentication:
/// ```ignore
/// let handle = MockNntpServer::new(8119)
///     .with_auth("user", "pass")
///     .spawn();
/// ```
///
/// Server with custom command handlers:
/// ```ignore
/// let handle = MockNntpServer::new(8119)
///     .on_command("LIST", "215 list follows\r\n.\r\n")
///     .on_command("GROUP", "211 100 1 100 alt.test\r\n")
///     .spawn();
/// ```
pub struct MockNntpServer {
    port: u16,
    name: String,
    require_auth: bool,
    credentials: Option<(String, String)>,
    command_handlers: HashMap<String, String>,
}

impl MockNntpServer {
    /// Create a new mock server builder on the specified port
    pub fn new(port: u16) -> Self {
        Self {
            port,
            name: "MockServer".to_string(),
            require_auth: false,
            credentials: None,
            command_handlers: HashMap::new(),
        }
    }

    /// Set the server name that appears in the greeting
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Require authentication with the given credentials
    pub fn with_auth(mut self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.require_auth = true;
        self.credentials = Some((username.into(), password.into()));
        self
    }

    /// Add a custom handler for a specific command prefix
    ///
    /// When a command starting with `cmd` is received, respond with `response`.
    pub fn on_command(mut self, cmd: impl Into<String>, response: impl Into<String>) -> Self {
        self.command_handlers
            .insert(cmd.into().to_uppercase(), response.into());
        self
    }

    /// Spawn the mock server and return a handle to its background task
    /// Spawn mock server and return AbortHandle for automatic cleanup
    ///
    /// When the AbortHandle is dropped, the background task is immediately cancelled.
    /// This prevents tests from hanging during shutdown waiting for mock servers to exit.
    pub fn spawn(self) -> AbortHandle {
        let Self {
            port,
            name,
            require_auth,
            credentials,
            command_handlers,
        } = self;

        tokio::spawn(async move {
            let addr = format!("127.0.0.1:{}", port);
            let listener = match TcpListener::bind(&addr).await {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("Failed to bind mock server on {}: {}", addr, e);
                    return;
                }
            };

            while let Ok((mut stream, _)) = listener.accept().await {
                let name = name.clone();
                let credentials = credentials.clone();
                let handlers = command_handlers.clone();

                // Spawn per-connection handler (explicitly drop handle)
                drop(tokio::spawn(async move {
                    // Send greeting
                    let greeting = if require_auth {
                        format!("200 {} Ready (auth required)\r\n", name)
                    } else {
                        format!("200 {} Ready\r\n", name)
                    };
                    if stream.write_all(greeting.as_bytes()).await.is_err() {
                        return;
                    }

                    let mut authenticated = !require_auth;
                    let mut buffer = [0; 1024];

                    loop {
                        let n = match stream.read(&mut buffer).await {
                            Ok(0) => break, // Connection closed
                            Ok(n) => n,
                            Err(_) => break,
                        };

                        let cmd_str = String::from_utf8_lossy(&buffer[..n]);
                        let cmd_upper = cmd_str.trim().to_uppercase();

                        // Handle QUIT
                        if cmd_upper.starts_with("QUIT") {
                            let _ = stream.write_all(b"205 Goodbye\r\n").await;
                            break;
                        }

                        // Handle authentication
                        if require_auth {
                            if cmd_upper.starts_with("AUTHINFO USER") {
                                if let Some((user, _)) = &credentials
                                    && cmd_str.contains(user.as_str())
                                {
                                    let _ = stream.write_all(b"381 Password required\r\n").await;
                                    continue;
                                }
                                let _ = stream.write_all(b"481 Authentication failed\r\n").await;
                                continue;
                            } else if cmd_upper.starts_with("AUTHINFO PASS") {
                                if let Some((_, pass)) = &credentials
                                    && cmd_str.contains(pass.as_str())
                                {
                                    authenticated = true;
                                    let _ =
                                        stream.write_all(b"281 Authentication accepted\r\n").await;
                                    continue;
                                }
                                let _ = stream.write_all(b"481 Authentication failed\r\n").await;
                                continue;
                            } else if !authenticated {
                                let _ = stream.write_all(b"480 Authentication required\r\n").await;
                                continue;
                            }
                        }

                        // Check custom command handlers
                        let mut handled = false;
                        for (prefix, response) in &handlers {
                            if cmd_upper.starts_with(prefix) {
                                let _ = stream.write_all(response.as_bytes()).await;
                                handled = true;
                                break;
                            }
                        }

                        // Default response
                        if !handled {
                            let _ = stream.write_all(b"200 OK\r\n").await;
                        }
                    }
                }));
            }
        })
        .abort_handle()
    }
}

/// Spawn a mock NNTP server that responds with a greeting
///
/// **Deprecated:** Use `MockNntpServer::new(port).with_name(name).spawn()` instead.
///
/// # Arguments
/// * `port` - Port to listen on
/// * `server_name` - Name to include in greeting message
///
/// # Returns
/// Handle to the background task running the mock server
/// Spawn a basic mock NNTP server
///
/// Returns an AbortHandle that automatically cancels the server when dropped.
/// This ensures fast test cleanup without waiting for graceful shutdown.
pub fn spawn_mock_server(port: u16, server_name: &str) -> AbortHandle {
    MockNntpServer::new(port).with_name(server_name).spawn()
}

/// Spawn a mock NNTP server that requires authentication
///
/// **Deprecated:** Use `MockNntpServer::new(port).with_auth(user, pass).spawn()` instead.
///
/// # Arguments
/// * `port` - Port to listen on
/// * `expected_user` - Expected username
/// * `expected_pass` - Expected password
///
/// # Returns
/// Handle to the background task running the mock server
#[allow(dead_code)]
/// Spawn a mock NNTP server that requires authentication
///
/// Returns an AbortHandle that automatically cancels the server when dropped.
/// This ensures fast test cleanup without waiting for graceful shutdown.
pub fn spawn_mock_server_with_auth(
    port: u16,
    server_name: &str,
    username: &str,
    password: &str,
) -> AbortHandle {
    MockNntpServer::new(port)
        .with_name(server_name)
        .with_auth(username, password)
        .spawn()
}

/// Create a test configuration with servers on the given ports
pub fn create_test_config(server_ports: Vec<(u16, &str)>) -> Config {
    use nntp_proxy::types::{HostName, MaxConnections, Port, ServerName};
    Config {
        servers: server_ports
            .into_iter()
            .map(|(port, name)| Server {
                host: HostName::new("127.0.0.1".to_string()).unwrap(),
                port: Port::new(port).unwrap(),
                name: ServerName::new(name.to_string()).unwrap(),
                username: None,
                password: None,
                max_connections: MaxConnections::new(5).unwrap(),
                use_tls: false,
                tls_verify_cert: true,
                tls_cert_path: None,
                connection_keepalive: None,
                health_check_max_per_cycle: nntp_proxy::config::health_check_max_per_cycle(),
                health_check_pool_timeout: nntp_proxy::config::health_check_pool_timeout(),
            })
            .collect(),
        routing_strategy: Default::default(),
        precheck_enabled: false,
        proxy: Default::default(),
        health_check: Default::default(),
        cache: Default::default(),
        client_auth: Default::default(),
    }
}

/// Create a test configuration with authentication
///
/// # Arguments
/// * `server_ports` - List of (port, name, user, pass) tuples for backend servers
///
/// # Returns
/// Configuration object ready for use in tests
#[allow(dead_code)]
pub fn create_test_config_with_auth(server_ports: Vec<(u16, &str, &str, &str)>) -> Config {
    use nntp_proxy::types::{HostName, MaxConnections, Port, ServerName};
    Config {
        servers: server_ports
            .into_iter()
            .map(|(port, name, user, pass)| Server {
                host: HostName::new("127.0.0.1".to_string()).unwrap(),
                port: Port::new(port).unwrap(),
                name: ServerName::new(name.to_string()).unwrap(),
                username: Some(user.to_string()),
                password: Some(pass.to_string()),
                max_connections: MaxConnections::new(5).unwrap(),
                use_tls: false,
                tls_verify_cert: true,
                tls_cert_path: None,
                connection_keepalive: None,
                health_check_max_per_cycle: nntp_proxy::config::health_check_max_per_cycle(),
                health_check_pool_timeout: nntp_proxy::config::health_check_pool_timeout(),
            })
            .collect(),
        routing_strategy: Default::default(),
        precheck_enabled: false,
        proxy: Default::default(),
        health_check: Default::default(),
        cache: None,
        client_auth: Default::default(),
    }
}

/// Wait for a server to be ready by attempting to connect
///
/// # Arguments
/// * `addr` - Address to connect to (e.g., "127.0.0.1:8080")
/// * `max_attempts` - Maximum number of connection attempts
///
/// # Returns
/// Ok(()) if connection succeeds, Err otherwise
pub async fn wait_for_server(addr: &str, max_attempts: u32) -> Result<()> {
    for attempt in 1..=max_attempts {
        tokio::time::sleep(Duration::from_millis(50)).await;

        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return Ok(());
        }

        if attempt == max_attempts {
            return Err(anyhow::anyhow!(
                "Server at {} did not become ready after {} attempts",
                addr,
                max_attempts
            ));
        }
    }

    Ok(())
}

/// Read a complete NNTP response from a stream
///
/// # Arguments
/// * `stream` - TCP stream to read from
/// * `buffer` - Buffer to read into
/// * `timeout_ms` - Timeout in milliseconds
///
/// # Returns
/// Number of bytes read
#[allow(dead_code)]
pub async fn read_response(
    stream: &mut tokio::net::TcpStream,
    buffer: &mut [u8],
    timeout_ms: u64,
) -> Result<usize> {
    tokio::time::timeout(Duration::from_millis(timeout_ms), stream.read(buffer))
        .await?
        .map_err(Into::into)
}

// ============================================================================
// Test Factory Functions
// ============================================================================
//
// These factory functions eliminate boilerplate when creating common test
// objects with standard/default configurations.

/// Create a standard test buffer pool (8KB, 4 buffers)
///
/// This is the most common buffer pool configuration used across tests.
/// Use this instead of repeating `BufferPool::new(BufferSize::new(8192).unwrap(), 4)`.
///
/// # Examples
/// ```ignore
/// let pool = create_test_buffer_pool();
/// let session = ClientSession::new(addr, pool, auth_handler);
/// ```
pub fn create_test_buffer_pool() -> nntp_proxy::pool::BufferPool {
    use nntp_proxy::pool::BufferPool;
    use nntp_proxy::types::BufferSize;

    BufferPool::new(BufferSize::new(8192).unwrap(), 4)
}

/// Create a test auth handler with standard credentials (user/pass)
///
/// Use this instead of repeating `Arc::new(AuthHandler::new(Some("user".to_string()), Some("pass".to_string())).unwrap())`.
///
/// # Examples
/// ```ignore
/// let auth = create_test_auth_handler();
/// let session = ClientSession::new(addr, pool, auth);
/// ```
pub fn create_test_auth_handler() -> std::sync::Arc<nntp_proxy::auth::AuthHandler> {
    use nntp_proxy::auth::AuthHandler;
    use std::sync::Arc;

    Arc::new(AuthHandler::new(Some("user".to_string()), Some("pass".to_string())).unwrap())
}

/// Create a test auth handler with custom credentials
///
/// # Examples
/// ```ignore
/// let auth = create_test_auth_handler_with("alice", "secret123");
/// ```
pub fn create_test_auth_handler_with(
    username: &str,
    password: &str,
) -> std::sync::Arc<nntp_proxy::auth::AuthHandler> {
    use nntp_proxy::auth::AuthHandler;
    use std::sync::Arc;

    Arc::new(AuthHandler::new(Some(username.to_string()), Some(password.to_string())).unwrap())
}

/// Create a disabled (no-auth) test auth handler
///
/// Use this instead of repeating `Arc::new(AuthHandler::new(None, None).unwrap())`.
///
/// # Examples
/// ```ignore
/// let auth = create_test_auth_handler_disabled();
/// let session = ClientSession::new(addr, pool, auth);
/// ```
pub fn create_test_auth_handler_disabled() -> std::sync::Arc<nntp_proxy::auth::AuthHandler> {
    use nntp_proxy::auth::AuthHandler;
    use std::sync::Arc;

    Arc::new(AuthHandler::new(None, None).unwrap())
}

/// Create a test backend selector (router)
///
/// Use this instead of repeating `Arc::new(BackendSelector::default())`.
///
/// # Examples
/// ```ignore
/// let router = create_test_router();
/// router.add_backend(...);
/// ```
pub fn create_test_router() -> std::sync::Arc<nntp_proxy::router::BackendSelector> {
    use nntp_proxy::router::BackendSelector;
    use std::sync::Arc;

    Arc::new(BackendSelector::default())
}

/// Create a test socket address (127.0.0.1:9999)
///
/// Standard test address for creating ClientSession instances.
///
/// # Examples
/// ```ignore
/// let addr = create_test_addr();
/// let session = ClientSession::new(addr, pool, auth);
/// ```
pub fn create_test_addr() -> std::net::SocketAddr {
    "127.0.0.1:9999".parse().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_spawn_mock_server() {
        // Use port 0 to let OS assign a random available port
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener); // Release port for mock server

        let _handle = spawn_mock_server(port, "TestServer");

        // Give server time to start
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Connect and verify greeting
        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();

        let mut buffer = [0; 1024];
        let n = stream.read(&mut buffer).await.unwrap();
        let response = String::from_utf8_lossy(&buffer[..n]);

        assert!(response.contains("200"));
        assert!(response.contains("TestServer"));
    }

    #[tokio::test]
    async fn test_mock_server_builder_basic() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let _handle = MockNntpServer::new(port).with_name("BuilderTest").spawn();

        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();

        let mut buffer = [0; 1024];
        let n = stream.read(&mut buffer).await.unwrap();
        let response = String::from_utf8_lossy(&buffer[..n]);

        assert!(response.contains("200"));
        assert!(response.contains("BuilderTest"));
    }

    #[tokio::test]
    async fn test_mock_server_builder_with_auth() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let _handle = MockNntpServer::new(port)
            .with_auth("testuser", "testpass")
            .spawn();

        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();

        let mut buffer = [0; 1024];

        // Read greeting
        let n = stream.read(&mut buffer).await.unwrap();
        let response = String::from_utf8_lossy(&buffer[..n]);
        assert!(response.contains("200"));
        assert!(response.contains("auth required"));

        // Send command without auth
        stream.write_all(b"LIST\r\n").await.unwrap();
        let n = stream.read(&mut buffer).await.unwrap();
        let response = String::from_utf8_lossy(&buffer[..n]);
        assert!(response.contains("480")); // Auth required

        // Authenticate
        stream
            .write_all(b"AUTHINFO USER testuser\r\n")
            .await
            .unwrap();
        let n = stream.read(&mut buffer).await.unwrap();
        let response = String::from_utf8_lossy(&buffer[..n]);
        assert!(response.contains("381")); // Password required

        stream
            .write_all(b"AUTHINFO PASS testpass\r\n")
            .await
            .unwrap();
        let n = stream.read(&mut buffer).await.unwrap();
        let response = String::from_utf8_lossy(&buffer[..n]);
        assert!(response.contains("281")); // Auth accepted
    }

    #[tokio::test]
    async fn test_mock_server_builder_custom_commands() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let _handle = MockNntpServer::new(port)
            .on_command("LIST", "215 list follows\r\n.\r\n")
            .on_command("GROUP", "211 100 1 100 alt.test\r\n")
            .spawn();

        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();

        let mut buffer = [0; 1024];

        // Read greeting
        let _ = stream.read(&mut buffer).await.unwrap();

        // Test custom LIST handler
        stream.write_all(b"LIST\r\n").await.unwrap();
        let n = stream.read(&mut buffer).await.unwrap();
        let response = String::from_utf8_lossy(&buffer[..n]);
        assert!(response.contains("215"));

        // Test custom GROUP handler
        stream.write_all(b"GROUP alt.test\r\n").await.unwrap();
        let n = stream.read(&mut buffer).await.unwrap();
        let response = String::from_utf8_lossy(&buffer[..n]);
        assert!(response.contains("211"));
        assert!(response.contains("alt.test"));
    }

    #[tokio::test]
    async fn test_create_test_config() {
        let config = create_test_config(vec![(19002, "server1"), (19003, "server2")]);

        assert_eq!(config.servers.len(), 2);
        assert_eq!(config.servers[0].port.get(), 19002);
        assert_eq!(config.servers[1].port.get(), 19003);
        assert_eq!(config.servers[0].name.as_str(), "server1");
    }

    #[tokio::test]
    async fn test_wait_for_server() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let _handle = spawn_mock_server(port, "WaitTest");

        let result = wait_for_server(&format!("127.0.0.1:{}", port), 20).await;
        assert!(result.is_ok());
    }

    // Factory function tests
    #[test]
    fn test_create_test_buffer_pool() {
        // Just verify it was created successfully
        let _pool = create_test_buffer_pool();
        // BufferPool is opaque, no public inspection API
    }

    #[test]
    fn test_create_test_auth_handler() {
        let auth = create_test_auth_handler();
        assert!(auth.is_enabled());
        assert!(auth.validate_credentials("user", "pass"));
        assert!(!auth.validate_credentials("wrong", "credentials"));
    }

    #[test]
    fn test_create_test_auth_handler_with() {
        let auth = create_test_auth_handler_with("alice", "secret123");
        assert!(auth.is_enabled());
        assert!(auth.validate_credentials("alice", "secret123"));
        assert!(!auth.validate_credentials("alice", "wrong"));
    }

    #[test]
    fn test_create_test_auth_handler_disabled() {
        let auth = create_test_auth_handler_disabled();
        assert!(!auth.is_enabled());
        // Disabled auth accepts anything
        assert!(auth.validate_credentials("any", "thing"));
    }

    #[test]
    fn test_create_test_router() {
        let router = create_test_router();
        assert_eq!(router.backend_count(), 0);
    }

    #[test]
    fn test_create_test_addr() {
        let addr = create_test_addr();
        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        assert_eq!(addr.port(), 9999);
    }
}
