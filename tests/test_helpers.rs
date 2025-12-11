//! Test helpers for integration tests
//!
//! This module provides reusable test utilities to reduce duplication
//! in integration tests.

use anyhow::Result;
use nntp_proxy::NntpProxy;
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

/// Spawn a basic mock NNTP server
///
/// Returns an AbortHandle that automatically cancels the server when dropped.
/// This ensures fast test cleanup without waiting for graceful shutdown.
///
/// # Arguments
/// * `port` - Port to listen on
/// * `server_name` - Name to include in greeting message
#[allow(dead_code)]
pub fn spawn_mock_server(port: u16, server_name: &str) -> AbortHandle {
    MockNntpServer::new(port).with_name(server_name).spawn()
}

/// Spawn a test proxy server in the background
///
/// # Arguments
/// * `proxy` - The NntpProxy instance to run
/// * `port` - Port to listen on
/// * `per_command_routing` - If true, use per-command routing; otherwise use stateful mode
///
/// # Example
/// ```ignore
/// let proxy = NntpProxy::new(config, RoutingMode::PerCommand)?;
/// spawn_test_proxy(proxy, 8119, true).await;
/// ```
pub async fn spawn_test_proxy(proxy: NntpProxy, port: u16, per_command_routing: bool) {
    let proxy_addr = format!("127.0.0.1:{}", port);
    let listener = TcpListener::bind(&proxy_addr).await.unwrap();

    tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = listener.accept().await {
                let proxy_clone = proxy.clone();
                tokio::spawn(async move {
                    if per_command_routing {
                        let _ = proxy_clone
                            .handle_client_per_command_routing(stream, addr.into())
                            .await;
                    } else {
                        let _ = proxy_clone.handle_client(stream, addr.into()).await;
                    }
                });
            }
        }
    });
}

/// Get an available port from the OS
///
/// Binds to 127.0.0.1:0 to let the OS assign a random available port,
/// then immediately releases it for use by the caller.
pub async fn get_available_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// Create a test configuration with servers on the given ports
pub fn create_test_config(server_ports: Vec<(u16, &str)>) -> Config {
    use nntp_proxy::types::{HostName, MaxConnections, Port, ServerName};
    Config {
        servers: server_ports
            .into_iter()
            .map(|(port, name)| Server {
                host: HostName::try_new("127.0.0.1".to_string()).unwrap(),
                port: Port::try_new(port).unwrap(),
                name: ServerName::try_new(name.to_string()).unwrap(),
                username: None,
                password: None,
                max_connections: MaxConnections::try_new(5).unwrap(),
                use_tls: false,
                tls_verify_cert: true,
                tls_cert_path: None,
                connection_keepalive: None,
                health_check_max_per_cycle: nntp_proxy::config::health_check_max_per_cycle(),
                health_check_pool_timeout: nntp_proxy::config::health_check_pool_timeout(),
            })
            .collect(),
        proxy: Default::default(),
        health_check: Default::default(),
        cache: Default::default(),
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

// ============================================================================
// Test Factory Functions
// ============================================================================
//
// These factory functions eliminate boilerplate when creating common test
// objects with standard/default configurations.

/// Create a standard test buffer pool (8KB, 4 buffers)
///
/// This is the most common buffer pool configuration used across tests.
/// Use this instead of repeating `BufferPool::new(BufferSize::try_new(8192).unwrap(), 4)`.
///
/// # Examples
/// ```ignore
/// let pool = create_test_buffer_pool();
/// let session = ClientSession::new(addr.into(), pool, auth_handler);
/// ```
pub fn create_test_buffer_pool() -> nntp_proxy::pool::BufferPool {
    use nntp_proxy::pool::BufferPool;
    use nntp_proxy::types::BufferSize;

    BufferPool::new(BufferSize::try_new(8192).unwrap(), 4)
}

/// Create a test auth handler with standard credentials (user/pass)
pub fn create_test_auth_handler() -> std::sync::Arc<nntp_proxy::auth::AuthHandler> {
    create_test_auth_handler_with("user", "pass")
}

/// Create a test auth handler with custom credentials
pub fn create_test_auth_handler_with(
    username: &str,
    password: &str,
) -> std::sync::Arc<nntp_proxy::auth::AuthHandler> {
    std::sync::Arc::new(
        nntp_proxy::auth::AuthHandler::new(Some(username.to_string()), Some(password.to_string()))
            .unwrap(),
    )
}

/// Create a disabled (no-auth) test auth handler
pub fn create_test_auth_handler_disabled() -> std::sync::Arc<nntp_proxy::auth::AuthHandler> {
    std::sync::Arc::new(nntp_proxy::auth::AuthHandler::new(None, None).unwrap())
}

/// Create a test backend selector (router)
pub fn create_test_router() -> std::sync::Arc<nntp_proxy::router::BackendSelector> {
    std::sync::Arc::new(nntp_proxy::router::BackendSelector::new())
}

/// Create a test socket address (127.0.0.1:9999)
///
/// Standard test address for creating ClientSession instances.
///
/// # Examples
/// ```ignore
/// let addr = create_test_addr();
/// let session = ClientSession::new(addr.into(), pool, auth);
/// ```
pub fn create_test_addr() -> std::net::SocketAddr {
    "127.0.0.1:9999".parse().unwrap()
}

// ============================================================================
// Proxy Setup Helpers
// ============================================================================

/// Setup a proxy with mock backends and return ports + handles
///
/// This eliminates the boilerplate of:
/// 1. Finding available ports
/// 2. Starting mock backends  
/// 3. Creating config
/// 4. Starting proxy with accept loop
/// 5. Waiting for everything to be ready
///
/// Returns: (proxy_port, Vec<backend_port>, Vec<mock_handles>)
#[allow(dead_code)]
pub async fn setup_proxy_with_backends(
    backend_configs: Vec<(&str, bool)>, // (name, has_article)
    routing_mode: nntp_proxy::RoutingMode,
) -> Result<(u16, Vec<u16>, Vec<AbortHandle>)> {
    use nntp_proxy::NntpProxy;

    // Allocate ports using helper
    let mut backend_ports = Vec::new();
    for _ in 0..backend_configs.len() {
        backend_ports.push(get_available_port().await?);
    }

    let proxy_port = get_available_port().await?;
    let proxy_listener = TcpListener::bind(format!("127.0.0.1:{}", proxy_port)).await?;

    // Start mock backends
    let mut mock_handles = Vec::new();
    for (i, (name, has_article)) in backend_configs.iter().enumerate() {
        let port = backend_ports[i];

        let response = if *has_article {
            "220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n"
        } else {
            "430 No such article\r\n"
        };

        let handle = MockNntpServer::new(port)
            .with_name(*name)
            .on_command("DATE", "111 20251203120000\r\n")
            .on_command("QUIT", "205 Goodbye\r\n")
            .on_command("ARTICLE", response) // ARTICLE command (any message-ID)
            .spawn();
        mock_handles.push(handle);
    }

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create proxy config
    let config = create_test_config(
        backend_ports
            .iter()
            .zip(backend_configs.iter())
            .map(|(port, (name, _))| (*port, *name))
            .collect(),
    );

    let proxy = NntpProxy::new(config, routing_mode)?;

    // Start proxy accept loop
    let proxy_for_spawn = proxy.clone();
    tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = proxy_listener.accept().await {
                let proxy_clone = proxy_for_spawn.clone();
                let mode = routing_mode;
                tokio::spawn(async move {
                    use nntp_proxy::RoutingMode;
                    let result = match mode {
                        RoutingMode::PerCommand | RoutingMode::Hybrid => {
                            proxy_clone
                                .handle_client_per_command_routing(stream, addr.into())
                                .await
                        }
                        RoutingMode::Stateful => {
                            proxy_clone.handle_client(stream, addr.into()).await
                        }
                    };
                    if let Err(e) = result {
                        eprintln!("Proxy error handling client: {}", e);
                    }
                });
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    Ok((proxy_port, backend_ports, mock_handles))
}

/// Connect to proxy and read greeting
#[allow(dead_code)] // Used by test_430_retry.rs
pub async fn connect_and_read_greeting(proxy_port: u16) -> Result<tokio::net::TcpStream> {
    use tokio::io::AsyncBufReadExt;

    let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).await?;
    let mut reader = tokio::io::BufReader::new(&mut stream);
    let mut line = String::new();

    reader.read_line(&mut line).await?;
    if !line.starts_with("200") {
        anyhow::bail!("Expected 200 greeting, got: {}", line);
    }

    // Return the raw stream (reader consumed it, need to recreate)
    drop(reader);
    Ok(stream)
}

/// Send ARTICLE command and read full multiline response
#[allow(dead_code)] // Used by test_430_retry.rs
pub async fn send_article_read_full_response(
    stream: &mut tokio::net::TcpStream,
    message_id: &str,
) -> Result<(String, Vec<String>)> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    eprintln!("Sending: ARTICLE {}", message_id);
    stream
        .write_all(format!("ARTICLE {}\r\n", message_id).as_bytes())
        .await?;

    let mut reader = tokio::io::BufReader::new(&mut *stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).await?;

    eprintln!("Received status: {}", status_line.trim());

    let mut body_lines = Vec::new();

    // If 220, read multiline body
    if status_line.starts_with("220") {
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).await?;
            if line == ".\r\n" {
                break;
            }
            body_lines.push(line);
        }
    }

    Ok((status_line, body_lines))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_get_available_port() {
        let port = get_available_port().await.unwrap();
        // Port is u16, so always valid - just verify we got one
        assert!(port > 0);
    }

    #[tokio::test]
    async fn test_mock_server_basic() {
        let port = get_available_port().await.unwrap();
        let _handle = MockNntpServer::new(port).with_name("TestServer").spawn();

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
        let port = get_available_port().await.unwrap();
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
        let port = get_available_port().await.unwrap();
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
        let port = get_available_port().await.unwrap();
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
        let port = get_available_port().await.unwrap();
        let _handle = MockNntpServer::new(port).with_name("WaitTest").spawn();

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
