//! Test helpers for integration tests
//!
//! This module provides reusable test utilities to reduce duplication
//! in integration tests.

// Each integration test crate compiles `mod test_helpers;` independently, so
// some shared helpers are intentionally unused in any given test target.
#![allow(dead_code)] // Shared helpers are intentionally unused in some integration-test crates.

use anyhow::Result;
use nntp_proxy::NntpProxy;
use nntp_proxy::config::{ClientAuth, Config, HealthCheck, Proxy, Server};
use nntp_proxy::types::{MaxConnections, Port};
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
/// let (port, handle) = MockNntpServer::new()
///     .with_name("TestServer")
///     .spawn_on_random_port()
///     .await?;
/// ```
///
/// Server with authentication:
/// ```ignore
/// let (port, handle) = MockNntpServer::new()
///     .with_auth("user", "pass")
///     .spawn_on_random_port()
///     .await?;
/// ```
///
/// Server with custom command handlers:
/// ```ignore
/// let (port, handle) = MockNntpServer::new()
///     .on_command("LIST", "215 list follows\r\n.\r\n")
///     .on_command("GROUP", "211 100 1 100 alt.test\r\n")
///     .spawn_on_random_port()
///     .await?;
/// ```
pub struct MockNntpServer {
    name: String,
    require_auth: bool,
    credentials: Option<(String, String)>,
    command_handlers: HashMap<String, String>,
}

impl Default for MockNntpServer {
    fn default() -> Self {
        Self::new()
    }
}

impl MockNntpServer {
    async fn run_on_listener(
        listener: TcpListener,
        name: String,
        require_auth: bool,
        credentials: Option<(String, String)>,
        command_handlers: HashMap<String, String>,
    ) {
        while let Ok((mut stream, _)) = listener.accept().await {
            let name = name.clone();
            let credentials = credentials.clone();
            let handlers = command_handlers.clone();

            drop(tokio::spawn(async move {
                let greeting = if require_auth {
                    format!("200 {name} Ready (auth required)\r\n")
                } else {
                    format!("200 {name} Ready\r\n")
                };
                if stream.write_all(greeting.as_bytes()).await.is_err() {
                    return;
                }

                let mut authenticated = !require_auth;
                let mut pending = bytes::BytesMut::new();
                let mut buffer = [0; 1024];

                while let Ok(n) = stream.read(&mut buffer).await {
                    if n == 0 {
                        break;
                    }

                    pending.extend_from_slice(&buffer[..n]);

                    while let Some(line_end) = pending.windows(2).position(|w| w == b"\r\n") {
                        let line = pending.split_to(line_end + 2);
                        let cmd_str = String::from_utf8_lossy(&line);
                        let cmd_upper = cmd_str.trim().to_uppercase();

                        if cmd_upper.starts_with("QUIT") {
                            let _ = stream.write_all(b"205 Goodbye\r\n").await;
                            return;
                        }

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

                        let mut handled = false;
                        if let Some((_, response)) = handlers
                            .iter()
                            .filter(|(prefix, _)| cmd_upper.starts_with(prefix.as_str()))
                            .max_by_key(|(prefix, _)| prefix.len())
                        {
                            let _ = stream.write_all(response.as_bytes()).await;
                            handled = true;
                        }

                        if !handled {
                            let _ = stream.write_all(b"200 OK\r\n").await;
                        }
                    }
                }
            }));
        }
    }

    /// Create a new mock server builder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            name: "MockServer".to_string(),
            require_auth: false,
            credentials: None,
            command_handlers: HashMap::new(),
        }
    }

    /// Set the server name that appears in the greeting
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Require authentication with the given credentials
    #[must_use]
    pub fn with_auth(mut self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.require_auth = true;
        self.credentials = Some((username.into(), password.into()));
        self
    }

    /// Add a custom handler for a specific command prefix
    ///
    /// When a command starting with `cmd` is received, respond with `response`.
    #[must_use]
    pub fn on_command(mut self, cmd: impl Into<String>, response: impl Into<String>) -> Self {
        self.command_handlers
            .insert(cmd.into().to_uppercase(), response.into());
        self
    }

    fn spawn_with_listener(self, listener: TcpListener) -> AbortHandle {
        let Self {
            name,
            require_auth,
            credentials,
            command_handlers,
        } = self;

        tokio::spawn(Self::run_on_listener(
            listener,
            name,
            require_auth,
            credentials,
            command_handlers,
        ))
        .abort_handle()
    }

    /// Spawn the mock server on a pre-bound listener.
    ///
    /// The returned [`AbortHandle`] can be used to stop the server task.
    #[must_use]
    pub fn spawn_on_listener(self, listener: TcpListener) -> AbortHandle {
        self.spawn_with_listener(listener)
    }

    /// Spawn the mock server on an OS-assigned loopback port.
    ///
    /// This keeps the listener bound from port assignment through server startup,
    /// avoiding the reserve-then-bind race that makes parallel tests flaky.
    ///
    /// # Errors
    /// Returns any listener bind or local address lookup error from the OS.
    pub async fn spawn_on_random_port(self) -> Result<(u16, AbortHandle)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        Ok((port, self.spawn_on_listener(listener)))
    }
}

/// Spawn a test proxy server on a pre-bound listener in the background.
///
/// Returns the bound listener port after the accept loop task has been spawned.
#[must_use]
pub fn spawn_test_proxy_on_listener(
    proxy: NntpProxy,
    proxy_listener: TcpListener,
    per_command_routing: bool,
) -> u16 {
    let proxy_port = proxy_listener
        .local_addr()
        .expect("test proxy listener has local address")
        .port();

    tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = proxy_listener.accept().await {
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

    proxy_port
}

/// Spawn a test proxy server on an OS-assigned loopback port in the background.
///
/// # Errors
/// Returns any listener bind error from the OS.
pub async fn spawn_test_proxy_on_random_port(
    proxy: NntpProxy,
    per_command_routing: bool,
) -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    Ok(spawn_test_proxy_on_listener(
        proxy,
        listener,
        per_command_routing,
    ))
}

/// Create a test configuration with servers on the given ports.
///
/// # Panics
/// Panics if any supplied port or generated server config is invalid.
#[must_use]
pub fn create_test_config(server_ports: Vec<(u16, &str)>) -> Config {
    use nntp_proxy::types::{MaxConnections, Port};
    Config {
        servers: server_ports
            .into_iter()
            .map(|(port, name)| {
                Server::builder("127.0.0.1", Port::try_new(port).unwrap())
                    .name(name)
                    .max_connections(MaxConnections::try_new(5).unwrap())
                    .build()
                    .unwrap()
            })
            .collect(),
        proxy: Proxy::default(),
        routing: Default::default(),
        memory: Default::default(),
        health_check: HealthCheck::default(),
        cache: None,
        client_auth: ClientAuth::default(),
    }
}

/// Wait for a server to be ready by attempting to connect
///
/// # Arguments
/// * `addr` - Address to connect to (e.g., "127.0.0.1:8080")
/// * `max_attempts` - Maximum number of connection attempts
///
/// # Errors
/// Returns an error if the server never becomes reachable within the allotted attempts.
pub async fn wait_for_server(addr: &str, max_attempts: u32) -> Result<()> {
    const WAIT_FOR_SERVER_RETRY_DELAY: Duration = Duration::from_millis(50);

    let mut retry_interval = tokio::time::interval_at(
        tokio::time::Instant::now() + WAIT_FOR_SERVER_RETRY_DELAY,
        WAIT_FOR_SERVER_RETRY_DELAY,
    );
    retry_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    for attempt in 1..=max_attempts {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return Ok(());
        }

        if attempt == max_attempts {
            return Err(anyhow::anyhow!(
                "Server at {addr} did not become ready after {max_attempts} attempts"
            ));
        }

        retry_interval.tick().await;
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
///
/// # Panics
/// Panics if the built-in test buffer size stops satisfying validation.
#[must_use]
pub fn create_test_buffer_pool() -> nntp_proxy::pool::BufferPool {
    use nntp_proxy::pool::BufferPool;
    use nntp_proxy::types::BufferSize;

    BufferPool::new(BufferSize::try_new(8192).unwrap(), 4)
}

/// Create a test auth handler with standard credentials (user/pass).
///
/// # Panics
/// Panics if constructing the test auth handler fails.
#[must_use]
pub fn create_test_auth_handler() -> std::sync::Arc<nntp_proxy::auth::AuthHandler> {
    create_test_auth_handler_with("user", "pass")
}

/// Create a test auth handler with custom credentials.
///
/// # Panics
/// Panics if constructing the test auth handler fails.
#[must_use]
pub fn create_test_auth_handler_with(
    username: &str,
    password: &str,
) -> std::sync::Arc<nntp_proxy::auth::AuthHandler> {
    std::sync::Arc::new(
        nntp_proxy::auth::AuthHandler::new(Some(username.to_string()), Some(password.to_string()))
            .unwrap(),
    )
}

/// Create a disabled (no-auth) test auth handler.
///
/// # Panics
/// Panics if constructing the disabled test auth handler fails.
#[must_use]
pub fn create_test_auth_handler_disabled() -> std::sync::Arc<nntp_proxy::auth::AuthHandler> {
    std::sync::Arc::new(nntp_proxy::auth::AuthHandler::new(None, None).unwrap())
}

/// Create a test backend selector (router)
#[must_use]
pub fn create_test_router() -> std::sync::Arc<nntp_proxy::router::BackendSelector> {
    std::sync::Arc::new(nntp_proxy::router::BackendSelector::new())
}

/// Create a test socket address (127.0.0.1:9999)
///
/// Standard test address for creating `ClientSession` instances.
///
/// # Examples
/// ```ignore
/// let addr = create_test_addr();
/// let session = ClientSession::new(addr.into(), pool, auth);
/// ```
///
/// # Panics
/// Panics if the built-in loopback socket address literal becomes invalid.
#[must_use]
pub fn create_test_addr() -> std::net::SocketAddr {
    "127.0.0.1:9999".parse().unwrap()
}

// ============================================================================
// Proxy Setup Helpers
// ============================================================================

/// Setup a proxy with mock backends and return ports + handles
///
/// This eliminates the boilerplate of:
/// 1. Binding listeners on OS-assigned ports
/// 2. Starting mock backends  
/// 3. Creating config
/// 4. Starting proxy with accept loop
/// 5. Waiting for everything to be ready
///
/// Returns: (`proxy_port`, Vec<`backend_port`>, Vec<`mock_handles`>)
///
/// # Errors
/// Returns any listener bind, address lookup, or proxy construction error.
pub async fn setup_proxy_with_backends(
    backend_configs: Vec<(&str, bool)>, // (name, has_article)
    routing_mode: nntp_proxy::RoutingMode,
) -> Result<(u16, Vec<u16>, Vec<AbortHandle>)> {
    use nntp_proxy::NntpProxy;

    // Bind backend listeners up front so the ports cannot be stolen between
    // "pick a port" and "start the mock server".
    let mut backend_listeners = Vec::new();
    let mut backend_ports = Vec::new();
    for _ in 0..backend_configs.len() {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        backend_ports.push(listener.local_addr()?.port());
        backend_listeners.push(listener);
    }

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await?;
    let proxy_port = proxy_listener.local_addr()?.port();

    // Start mock backends
    let mut mock_handles = Vec::new();
    for ((name, has_article), listener) in backend_configs.iter().zip(backend_listeners) {
        let response = if *has_article {
            "220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n"
        } else {
            "430 No such article\r\n"
        };

        let handle = MockNntpServer::new()
            .with_name(*name)
            .on_command("DATE", "111 20251203120000\r\n")
            .on_command("QUIT", "205 Goodbye\r\n")
            .on_command("ARTICLE", response)
            .spawn_on_listener(listener);
        mock_handles.push(handle);
    }

    // Create proxy config
    let config = create_test_config(
        backend_ports
            .iter()
            .zip(backend_configs.iter())
            .map(|(port, (name, _))| (*port, *name))
            .collect(),
    );

    let proxy = NntpProxy::new(config, routing_mode).await?;

    // Start proxy accept loop
    let proxy_for_spawn = proxy.clone();
    tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = proxy_listener.accept().await {
                let proxy_clone = proxy_for_spawn.clone();
                let mode = routing_mode;
                tokio::spawn(async move {
                    use nntp_proxy::RoutingMode;
                    let result = if matches!(mode, RoutingMode::PerCommand | RoutingMode::Hybrid) {
                        proxy_clone
                            .handle_client_per_command_routing(stream, addr.into())
                            .await
                    } else {
                        proxy_clone.handle_client(stream, addr.into()).await
                    };
                    if let Err(e) = result {
                        eprintln!("Proxy error handling client: {e}");
                    }
                });
            }
        }
    });

    Ok((proxy_port, backend_ports, mock_handles))
}

/// Spawn a proxy with the supplied config on a random loopback port.
///
/// Returns the chosen proxy port once the listener has been bound and the
/// accept loop task has been spawned.
///
/// # Errors
/// Returns any bind, address lookup, or proxy construction error.
pub async fn spawn_proxy_with_config(
    config: Config,
    routing_mode: nntp_proxy::RoutingMode,
) -> Result<u16> {
    use nntp_proxy::NntpProxy;

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await?;
    let proxy_port = proxy_listener.local_addr()?.port();
    let proxy = NntpProxy::new(config, routing_mode).await?;

    tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = proxy_listener.accept().await {
                let proxy_clone = proxy.clone();
                let mode = routing_mode;
                tokio::spawn(async move {
                    use nntp_proxy::RoutingMode;
                    let result = if matches!(mode, RoutingMode::PerCommand | RoutingMode::Hybrid) {
                        proxy_clone
                            .handle_client_per_command_routing(stream, addr.into())
                            .await
                    } else {
                        proxy_clone.handle_client(stream, addr.into()).await
                    };
                    if let Err(error) = result {
                        eprintln!("Proxy error handling client: {error}");
                    }
                });
            }
        }
    });

    Ok(proxy_port)
}

/// Spawn a proxy backed by a single mock backend and return the proxy port.
///
/// This is the shared setup used by tests that need to connect one or more raw
/// clients instead of using [`RfcTestClient`].
pub async fn spawn_single_backend_proxy<F>(
    routing_mode: nntp_proxy::RoutingMode,
    backend_name: &str,
    build_backend: F,
) -> Result<(u16, AbortHandle)>
where
    F: FnOnce(u16) -> MockNntpServer,
{
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let backend = build_backend(backend_port).spawn_on_listener(backend_listener);
    let config = create_test_config(vec![(backend_port, backend_name)]);
    let proxy_port = spawn_proxy_with_config(config, routing_mode).await?;
    Ok((proxy_port, backend))
}

/// Spawn an auth-enabled proxy backed by a single mock backend and return the proxy port.
pub async fn spawn_single_backend_proxy_with_auth<F>(
    routing_mode: nntp_proxy::RoutingMode,
    _backend_name: &str,
    username: &str,
    password: &str,
    build_backend: F,
) -> Result<(u16, AbortHandle)>
where
    F: FnOnce(u16) -> MockNntpServer,
{
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_port = backend_listener.local_addr()?.port();
    let backend = build_backend(backend_port).spawn_on_listener(backend_listener);
    let config = create_test_config_with_auth(vec![backend_port], username, password);
    let proxy_port = spawn_proxy_with_config(config, routing_mode).await?;
    Ok((proxy_port, backend))
}

/// Connect to proxy and read greeting.
///
/// # Errors
/// Returns any connection or read error, or an error if the greeting is unexpected.
pub async fn connect_and_read_greeting(proxy_port: u16) -> Result<tokio::net::TcpStream> {
    let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{proxy_port}")).await?;
    let line = read_line_from_stream(&mut stream, "proxy greeting").await?;
    if !line.starts_with("201") {
        anyhow::bail!("Expected 201 greeting, got: {line}");
    }

    Ok(stream)
}

/// Connected RFC test client backed by a single spawned mock backend.
///
/// This fixture collapses the repetitive integration-test setup of:
/// bind backend listener → spawn backend → create config → spawn proxy → connect client.
pub struct RfcTestClient {
    stream: tokio::net::TcpStream,
    #[allow(dead_code)]
    backend: AbortHandle,
}

impl RfcTestClient {
    /// Spawn a client/proxy pair backed by one mock backend using default config.
    pub async fn spawn<F>(
        routing_mode: nntp_proxy::RoutingMode,
        backend_name: &str,
        build_backend: F,
    ) -> Result<Self>
    where
        F: FnOnce(u16) -> MockNntpServer,
    {
        Self::spawn_with_config(
            routing_mode,
            backend_name,
            create_test_config,
            build_backend,
        )
        .await
    }

    /// Spawn a client/proxy pair backed by one mock backend using auth-enabled config.
    pub async fn spawn_with_auth<F>(
        routing_mode: nntp_proxy::RoutingMode,
        backend_name: &str,
        username: &str,
        password: &str,
        build_backend: F,
    ) -> Result<Self>
    where
        F: FnOnce(u16) -> MockNntpServer,
    {
        Self::spawn_with_config(
            routing_mode,
            backend_name,
            |servers| {
                let ports = servers.into_iter().map(|(port, _)| port).collect();
                create_test_config_with_auth(ports, username, password)
            },
            build_backend,
        )
        .await
    }

    async fn spawn_with_config<F, C>(
        routing_mode: nntp_proxy::RoutingMode,
        backend_name: &str,
        make_config: C,
        build_backend: F,
    ) -> Result<Self>
    where
        F: FnOnce(u16) -> MockNntpServer,
        C: FnOnce(Vec<(u16, &str)>) -> Config,
    {
        let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
        let backend_port = backend_listener.local_addr()?.port();
        let backend = build_backend(backend_port).spawn_on_listener(backend_listener);
        let config = make_config(vec![(backend_port, backend_name)]);
        let proxy_port = spawn_proxy_with_config(config, routing_mode).await?;
        let stream = connect_and_read_greeting(proxy_port).await?;

        Ok(Self { stream, backend })
    }

    /// Borrow the underlying stream for bespoke pipelining/reader assertions.
    pub fn stream_mut(&mut self) -> &mut tokio::net::TcpStream {
        &mut self.stream
    }

    /// Consume the fixture and return the connected stream for custom readers.
    pub fn into_stream(self) -> tokio::net::TcpStream {
        self.stream
    }

    /// Send a command and return the single-line response.
    pub async fn send_line(&mut self, command: &str) -> Result<String> {
        send_command_read_line(&mut self.stream, command).await
    }

    /// Send a command and return the multiline response.
    pub async fn send_multiline(&mut self, command: &str) -> Result<(String, Vec<String>)> {
        send_command_read_multiline_response(&mut self.stream, command).await
    }

    /// Send ARTICLE and return the multiline response body.
    pub async fn send_article(&mut self, selector: &str) -> Result<(String, Vec<String>)> {
        send_article_read_multiline_response(&mut self.stream, selector).await
    }

    /// Send a command and assert the returned status prefix.
    pub async fn expect_status(&mut self, command: &str, expected_prefix: &str) -> Result<String> {
        let response = self.send_line(command).await?;
        anyhow::ensure!(
            response.starts_with(expected_prefix),
            "Expected {expected_prefix} for command {command:?}, got: {response:?}"
        );
        Ok(response)
    }

    /// Send a command and assert the multiline response status prefix.
    pub async fn expect_multiline(
        &mut self,
        command: &str,
        expected_prefix: &str,
    ) -> Result<Vec<String>> {
        let (status, lines) = self.send_multiline(command).await?;
        anyhow::ensure!(
            status.starts_with(expected_prefix),
            "Expected multiline status {expected_prefix} for command {command:?}, got: {status:?}"
        );
        Ok(lines)
    }

    /// Send ARTICLE and assert the returned status prefix.
    pub async fn expect_article(
        &mut self,
        selector: &str,
        expected_prefix: &str,
    ) -> Result<Vec<String>> {
        let (status, lines) = self.send_article(selector).await?;
        anyhow::ensure!(
            status.starts_with(expected_prefix),
            "Expected ARTICLE status {expected_prefix} for selector {selector:?}, got: {status:?}"
        );
        Ok(lines)
    }

    /// Run a standard AUTHINFO USER/PASS flow and assert the expected status codes.
    pub async fn authenticate(&mut self, username: &str, password: &str) -> Result<()> {
        self.expect_status(&format!("AUTHINFO USER {username}"), "381")
            .await?;
        self.expect_status(&format!("AUTHINFO PASS {password}"), "281")
            .await?;
        Ok(())
    }
}

async fn write_command_line(stream: &mut tokio::net::TcpStream, command: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    if command.ends_with("\r\n") {
        stream.write_all(command.as_bytes()).await?;
    } else {
        stream
            .write_all(format!("{command}\r\n").as_bytes())
            .await?;
    }

    Ok(())
}

pub async fn read_line_from_stream(
    stream: &mut tokio::net::TcpStream,
    context: &str,
) -> Result<String> {
    use tokio::io::AsyncReadExt;

    const READ_LINE_TIMEOUT: Duration = Duration::from_secs(2);

    let mut bytes = Vec::with_capacity(128);
    let mut byte = [0u8; 1];

    loop {
        let n = match tokio::time::timeout(READ_LINE_TIMEOUT, stream.read(&mut byte)).await {
            Ok(result) => result?,
            Err(_) => anyhow::bail!("Timed out while reading {context}"),
        };
        if n == 0 {
            if bytes.is_empty() {
                anyhow::bail!("Connection closed while reading {context}");
            }
            anyhow::bail!("Connection closed before line terminator while reading {context}");
        }

        bytes.push(byte[0]);
        if byte[0] == b'\n' {
            return String::from_utf8(bytes).map_err(Into::into);
        }
    }
}

/// Send a command and read its single-line response.
///
/// The command is terminated with CRLF if needed.
///
/// # Errors
/// Returns any socket read/write error while exchanging the command.
pub async fn send_command_read_line(
    stream: &mut tokio::net::TcpStream,
    command: &str,
) -> Result<String> {
    write_command_line(stream, command).await?;
    read_line_from_stream(stream, "response line").await
}

/// Send ARTICLE command and read its multiline response.
///
/// # Errors
/// Returns any socket read/write error while exchanging the ARTICLE command.
pub async fn send_article_read_multiline_response(
    stream: &mut tokio::net::TcpStream,
    message_id: &str,
) -> Result<(String, Vec<String>)> {
    eprintln!("Sending: ARTICLE {message_id}");
    write_command_line(stream, &format!("ARTICLE {message_id}")).await?;

    let status_line = read_line_from_stream(stream, "ARTICLE status line").await?;

    eprintln!("Received status: {}", status_line.trim());

    let mut body_lines = Vec::new();

    // If 220, read multiline body
    if status_line.starts_with("220") {
        loop {
            let line = read_line_from_stream(stream, "ARTICLE body").await?;
            if line == ".\r\n" {
                break;
            }
            body_lines.push(line);
        }
    }

    Ok((status_line, body_lines))
}

/// Send a command and read a multiline NNTP response until the terminator.
///
/// Returns the status line and each subsequent response line, excluding the
/// terminating `.\r\n` line.
///
/// # Errors
/// Returns any socket read/write error while exchanging the command.
pub async fn send_command_read_multiline_response(
    stream: &mut tokio::net::TcpStream,
    command: &str,
) -> Result<(String, Vec<String>)> {
    write_command_line(stream, command).await?;
    let status_line = read_line_from_stream(stream, "response status line").await?;

    // Only attempt multiline reads for command/response pairs that are defined
    // as multiline by NNTP semantics. Status 211 is ambiguous: GROUP is
    // single-line, while LISTGROUP is multiline.
    let command_upper = command.trim().to_ascii_uppercase();
    let is_multiline = status_line.starts_with("100")
        || status_line.starts_with("101")
        || status_line.starts_with("215")
        || status_line.starts_with("220")
        || status_line.starts_with("221")
        || status_line.starts_with("222")
        || status_line.starts_with("224")
        || status_line.starts_with("225")
        || status_line.starts_with("230")
        || status_line.starts_with("231")
        || (status_line.starts_with("211") && command_upper.starts_with("LISTGROUP"));

    let mut lines = Vec::new();
    if is_multiline {
        loop {
            let line = read_line_from_stream(stream, "multiline response").await?;
            if line == ".\r\n" {
                break;
            }
            lines.push(line);
        }
    }

    Ok((status_line, lines))
}

// =============================================================================
// Server Configuration Helpers
// =============================================================================

// These functions are used across different test files. Since each test file
// compiles test_helpers.rs as a module, they appear "unused" in some compilations.
// This is expected behavior for the `mod test_helpers;` pattern.

/// Create a basic server configuration for testing (no TLS).
///
/// # Panics
/// Panics if the supplied port or generated server config is invalid.
#[must_use]
pub fn create_test_server_config(host: &str, port: u16, name: &str) -> Server {
    Server::builder(host, Port::try_new(port).unwrap())
        .name(name)
        .max_connections(MaxConnections::try_new(5).unwrap())
        .build()
        .expect("Valid server config")
}

/// Create a server configuration with authentication.
///
/// # Panics
/// Panics if the supplied port or generated server config is invalid.
#[must_use]
pub fn create_test_server_config_with_auth(
    host: &str,
    port: u16,
    name: &str,
    username: &str,
    password: &str,
) -> Server {
    Server::builder(host, Port::try_new(port).unwrap())
        .name(name)
        .username(username)
        .password(password)
        .max_connections(MaxConnections::try_new(5).unwrap())
        .build()
        .expect("Valid server config")
}

/// Create a TLS-enabled server configuration.
///
/// # Panics
/// Panics if the supplied port or generated server config is invalid.
#[must_use]
pub fn create_test_server_config_with_tls(
    host: &str,
    port: u16,
    name: &str,
    tls_verify_cert: bool,
    tls_cert_path: Option<String>,
) -> Server {
    let mut builder = Server::builder(host, Port::try_new(port).unwrap())
        .name(name)
        .max_connections(MaxConnections::try_new(5).unwrap())
        .use_tls(true)
        .tls_verify_cert(tls_verify_cert);

    if let Some(path) = tls_cert_path {
        builder = builder.tls_cert_path(path);
    }

    builder.build().expect("Valid server config")
}

/// Create a server configuration with custom `max_connections`.
///
/// # Panics
/// Panics if the supplied port, max-connections value, or server config is invalid.
#[must_use]
pub fn create_test_server_config_with_max_connections(
    host: &str,
    port: u16,
    name: &str,
    max_connections: usize,
) -> Server {
    Server::builder(host, Port::try_new(port).unwrap())
        .name(name)
        .max_connections(MaxConnections::try_new(max_connections).unwrap())
        .build()
        .expect("Valid server config")
}

/// Create a full Config with client authentication enabled.
///
/// # Panics
/// Panics if any generated backend server config is invalid.
#[must_use]
pub fn create_test_config_with_auth(
    backend_ports: Vec<u16>,
    username: &str,
    password: &str,
) -> Config {
    use nntp_proxy::config::{ClientAuth, UserCredentials};

    Config {
        servers: backend_ports
            .into_iter()
            .map(|port| create_test_server_config("127.0.0.1", port, &format!("backend-{port}")))
            .collect(),
        proxy: Proxy::default(),
        routing: Default::default(),
        memory: Default::default(),
        health_check: HealthCheck::default(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_server_basic() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let _handle = MockNntpServer::new()
            .with_name("TestServer")
            .spawn_on_listener(listener);

        // Connect and verify greeting
        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
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
        let _handle = MockNntpServer::new()
            .with_name("BuilderTest")
            .spawn_on_listener(listener);

        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
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
        let _handle = MockNntpServer::new()
            .with_auth("testuser", "testpass")
            .spawn_on_listener(listener);

        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
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
        let _handle = MockNntpServer::new()
            .on_command("LIST", "215 list follows\r\n.\r\n")
            .on_command("GROUP", "211 100 1 100 alt.test\r\n")
            .spawn_on_listener(listener);

        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
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
    async fn test_mock_server_handles_multiple_commands_in_one_read() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let _handle = MockNntpServer::new()
            .on_command("DATE", "111 20260410235959\r\n")
            .on_command("HELP", "100 help follows\r\n.\r\n")
            .spawn_on_listener(listener);

        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();

        let mut buffer = [0; 1024];
        let _ = stream.read(&mut buffer).await.unwrap();

        stream.write_all(b"DATE\r\nHELP\r\n").await.unwrap();

        let mut response_bytes = Vec::new();
        loop {
            let n = tokio::time::timeout(Duration::from_secs(1), stream.read(&mut buffer))
                .await
                .expect("timed out waiting for mock server responses")
                .unwrap();

            assert!(
                n > 0,
                "mock server closed connection before sending both responses"
            );

            response_bytes.extend_from_slice(&buffer[..n]);
            let response = String::from_utf8_lossy(&response_bytes);
            if response.contains("111 20260410235959") && response.contains("100 help follows") {
                break;
            }
        }

        let response = String::from_utf8_lossy(&response_bytes);
        assert!(response.contains("111 20260410235959"));
        assert!(response.contains("100 help follows"));
    }

    #[test]
    fn test_create_test_config() {
        let config = create_test_config(vec![(19002, "server1"), (19003, "server2")]);

        assert_eq!(config.servers.len(), 2);
        assert_eq!(config.servers[0].port.get(), 19002);
        assert_eq!(config.servers[1].port.get(), 19003);
        assert_eq!(config.servers[0].name.as_str(), "server1");
    }

    #[tokio::test]
    async fn test_wait_for_server() {
        let (port, _handle) = MockNntpServer::new()
            .with_name("WaitTest")
            .spawn_on_random_port()
            .await
            .unwrap();

        let result = wait_for_server(&format!("127.0.0.1:{port}"), 20).await;
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
