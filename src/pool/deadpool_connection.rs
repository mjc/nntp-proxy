use anyhow::Result;
use async_trait::async_trait;
use deadpool::managed;
use std::fmt;
use tokio::net::TcpStream;
use tokio::time::Duration;
use tracing::info;

use crate::constants::socket::{POOL_RECV_BUFFER, POOL_SEND_BUFFER};
use crate::pool::connection_trait::{ConnectionProvider, PoolStatus};
use crate::protocol::{ResponseParser, authinfo_pass, authinfo_user};
use crate::stream::ConnectionStream;
use crate::tls::{TlsConfig, TlsManager};

// Health check constants
const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(2);
const HEALTH_CHECK_BUFFER_SIZE: usize = 512;
const DATE_COMMAND: &[u8] = b"DATE\r\n";
const EXPECTED_DATE_RESPONSE_PREFIX: &str = "111 ";

/// Errors that can occur during connection health checks
#[derive(Debug)]
pub enum HealthCheckError {
    /// TCP connection is closed
    TcpClosed,
    /// Unexpected data found in the buffer before health check
    UnexpectedData,
    /// TCP-level error occurred
    TcpError(std::io::Error),
    /// Failed to write DATE command to the connection
    WriteError(std::io::Error),
    /// Failed to read response from the connection
    ReadError(std::io::Error),
    /// Health check operation timed out
    Timeout,
    /// Server returned unexpected response to DATE command
    UnexpectedResponse(String),
    /// Connection closed while waiting for health check response
    ConnectionClosedDuringCheck,
}

impl fmt::Display for HealthCheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TcpClosed => write!(f, "TCP connection closed"),
            Self::UnexpectedData => write!(f, "Unexpected data in buffer"),
            Self::TcpError(e) => write!(f, "TCP error: {}", e),
            Self::WriteError(e) => write!(f, "Failed to write health check: {}", e),
            Self::ReadError(e) => write!(f, "Failed to read health check response: {}", e),
            Self::Timeout => write!(f, "Health check timeout"),
            Self::UnexpectedResponse(response) => {
                write!(f, "Unexpected health check response: {}", response.trim())
            }
            Self::ConnectionClosedDuringCheck => {
                write!(f, "Connection closed during health check")
            }
        }
    }
}

impl std::error::Error for HealthCheckError {}

impl From<HealthCheckError> for managed::RecycleError<anyhow::Error> {
    fn from(err: HealthCheckError) -> Self {
        managed::RecycleError::Message(err.to_string().into())
    }
}

/// TCP connection manager for deadpool
#[derive(Debug)]
pub struct TcpManager {
    host: String,
    port: u16,
    name: String,
    username: Option<String>,
    password: Option<String>,
    tls_config: TlsConfig,
}

impl TcpManager {
    pub fn new(
        host: String,
        port: u16,
        name: String,
        username: Option<String>,
        password: Option<String>,
    ) -> Self {
        Self {
            host,
            port,
            name,
            username,
            password,
            tls_config: TlsConfig::default(),
        }
    }

    /// Create a new TcpManager with TLS configuration
    pub fn new_with_tls(
        host: String,
        port: u16,
        name: String,
        username: Option<String>,
        password: Option<String>,
        tls_config: TlsConfig,
    ) -> Self {
        Self {
            host,
            port,
            name,
            username,
            password,
            tls_config,
        }
    }

    /// Create an optimized connection (TCP or TLS)
    async fn create_optimized_stream(&self) -> Result<ConnectionStream, anyhow::Error> {
        use socket2::{Domain, Protocol, Socket, Type};

        // Resolve hostname
        let addr = format!("{}:{}", self.host, self.port);
        let socket_addr = tokio::net::lookup_host(&addr)
            .await?
            .next()
            .ok_or_else(|| anyhow::anyhow!("No addresses found for {}", addr))?;

        // Create and configure socket
        let domain = if socket_addr.is_ipv4() {
            Domain::IPV4
        } else {
            Domain::IPV6
        };
        let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;

        // Socket buffers (4MB for pooled connections)
        socket.set_recv_buffer_size(POOL_RECV_BUFFER)?;
        socket.set_send_buffer_size(POOL_SEND_BUFFER)?;

        // TCP keepalive: probe after 60s idle, retry every 10s
        socket.set_keepalive(true)?;
        let keepalive = socket2::TcpKeepalive::new()
            .with_time(std::time::Duration::from_secs(60))
            .with_interval(std::time::Duration::from_secs(10));
        socket.set_tcp_keepalive(&keepalive)?;

        // Low latency settings
        socket.set_tcp_nodelay(true)?;
        socket.set_reuse_address(true)?;

        // Connect and convert to tokio TcpStream
        socket.connect(&socket_addr.into())?;
        let std_stream: std::net::TcpStream = socket.into();
        std_stream.set_nonblocking(true)?;
        let tcp_stream = TcpStream::from_std(std_stream)?;

        // Perform TLS handshake if enabled
        if self.tls_config.use_tls {
            let tls_manager = TlsManager::new(self.tls_config.clone());
            let tls_stream = tls_manager
                .handshake(tcp_stream, &self.host, &self.name)
                .await?;
            Ok(ConnectionStream::Tls(Box::new(tls_stream)))
        } else {
            Ok(ConnectionStream::Plain(tcp_stream))
        }
    }
}

/// Fast TCP-level check for obviously dead connections
///
/// Uses non-blocking peek to detect closed connections without consuming data.
/// Only applicable to plain TCP connections; TLS connections skip this check.
fn check_tcp_alive(conn: &mut ConnectionStream) -> managed::RecycleResult<anyhow::Error> {
    if let ConnectionStream::Plain(tcp) = conn {
        let mut peek_buf = [0u8; 1];
        match tcp.try_read(&mut peek_buf) {
            Ok(0) => return Err(HealthCheckError::TcpClosed.into()),
            Ok(_) => return Err(HealthCheckError::UnexpectedData.into()),
            Err(ref e) if e.kind() != std::io::ErrorKind::WouldBlock => {
                return Err(HealthCheckError::TcpError(std::io::Error::from(e.kind())).into());
            }
            Err(_) => {} // WouldBlock is expected, connection appears alive
        }
    }
    // TLS connections skip TCP-level check
    Ok(())
}

/// Application-level health check using DATE command
///
/// Sends DATE command and verifies response to ensure the NNTP connection
/// is still functional. This detects server-side timeouts that TCP keepalive
/// might miss.
async fn check_date_response(conn: &mut ConnectionStream) -> managed::RecycleResult<anyhow::Error> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::timeout;

    // Wrap entire health check in single timeout
    let health_check = async {
        // Send DATE command
        conn.write_all(DATE_COMMAND)
            .await
            .map_err(HealthCheckError::WriteError)?;

        // Read response
        let mut response_buf = [0u8; HEALTH_CHECK_BUFFER_SIZE];
        let n = conn
            .read(&mut response_buf)
            .await
            .map_err(HealthCheckError::ReadError)?;

        if n == 0 {
            return Err(HealthCheckError::ConnectionClosedDuringCheck);
        }

        // Validate DATE response
        let response = String::from_utf8_lossy(&response_buf[..n]);
        if response.starts_with(EXPECTED_DATE_RESPONSE_PREFIX) {
            Ok(())
        } else {
            Err(HealthCheckError::UnexpectedResponse(response.to_string()))
        }
    };

    timeout(HEALTH_CHECK_TIMEOUT, health_check)
        .await
        .unwrap_or(Err(HealthCheckError::Timeout))
        .map_err(|e| e.into())
}

impl managed::Manager for TcpManager {
    type Type = ConnectionStream;
    type Error = anyhow::Error;

    async fn create(&self) -> Result<ConnectionStream, anyhow::Error> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut stream = self.create_optimized_stream().await?;

        // Consume greeting
        let mut buffer = vec![0u8; 4096];
        let n = stream.read(&mut buffer).await?;
        let greeting = &buffer[..n];
        let greeting_str = String::from_utf8_lossy(greeting);

        if !ResponseParser::is_greeting(greeting) {
            return Err(anyhow::anyhow!("Invalid greeting: {}", greeting_str.trim()));
        }

        // Authenticate if needed
        if let Some(username) = &self.username {
            stream.write_all(authinfo_user(username).as_bytes()).await?;
            let n = stream.read(&mut buffer).await?;
            let response = &buffer[..n];
            let response_str = String::from_utf8_lossy(response);

            if ResponseParser::is_auth_required(response) {
                // Password required
                let password = self
                    .password
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Password required but not provided"))?;

                stream.write_all(authinfo_pass(password).as_bytes()).await?;
                let n = stream.read(&mut buffer).await?;
                let response = &buffer[..n];
                let response_str = String::from_utf8_lossy(response);

                if !ResponseParser::is_auth_success(response) {
                    return Err(anyhow::anyhow!("Auth failed: {}", response_str.trim()));
                }
            } else if !ResponseParser::is_auth_success(response) {
                return Err(anyhow::anyhow!(
                    "Unexpected auth response: {}",
                    response_str.trim()
                ));
            }
        }

        Ok(stream)
    }

    async fn recycle(
        &self,
        conn: &mut ConnectionStream,
        _metrics: &managed::Metrics,
    ) -> managed::RecycleResult<anyhow::Error> {
        // Fast TCP-level check for plain connections
        check_tcp_alive(conn)?;

        // Application-level health check using DATE command
        check_date_response(conn).await
    }

    fn detach(&self, _conn: &mut ConnectionStream) {}
}

type Pool = managed::Pool<TcpManager>;

/// Deadpool-based connection provider for high-performance connection pooling
#[derive(Debug, Clone)]
pub struct DeadpoolConnectionProvider {
    pool: Pool,
    name: String,
}

impl DeadpoolConnectionProvider {
    pub fn new(
        host: String,
        port: u16,
        name: String,
        max_size: usize,
        username: Option<String>,
        password: Option<String>,
    ) -> Self {
        let manager = TcpManager::new(host, port, name.clone(), username, password);
        let pool = Pool::builder(manager)
            .max_size(max_size)
            .build()
            .expect("Failed to create connection pool");

        Self { pool, name }
    }

    /// Create a new connection provider with TLS support
    pub fn new_with_tls(
        host: String,
        port: u16,
        name: String,
        max_size: usize,
        username: Option<String>,
        password: Option<String>,
        tls_config: TlsConfig,
    ) -> Self {
        let manager =
            TcpManager::new_with_tls(host, port, name.clone(), username, password, tls_config);
        let pool = Pool::builder(manager)
            .max_size(max_size)
            .build()
            .expect("Failed to create connection pool");

        Self { pool, name }
    }

    /// Create a connection provider from a server configuration
    ///
    /// This avoids unnecessary cloning of individual fields.
    pub fn from_server_config(server: &crate::config::ServerConfig) -> Self {
        let tls_config = TlsConfig {
            use_tls: server.use_tls,
            tls_verify_cert: server.tls_verify_cert,
            tls_cert_path: server.tls_cert_path.clone(),
        };

        Self::new_with_tls(
            server.host.clone(),
            server.port,
            server.name.clone(),
            server.max_connections as usize,
            server.username.clone(),
            server.password.clone(),
            tls_config,
        )
    }

    /// Get a connection from the pool (automatically returned when dropped)
    pub async fn get_pooled_connection(&self) -> Result<managed::Object<TcpManager>> {
        self.pool
            .get()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get connection from {}: {}", self.name, e))
    }

    /// Get the maximum pool size
    #[must_use]
    #[inline]
    pub fn max_size(&self) -> usize {
        self.pool.status().max_size
    }

    /// Gracefully shutdown the pool
    pub async fn graceful_shutdown(&self) {
        use deadpool::managed::Object;
        use tokio::io::AsyncWriteExt;

        let status = self.pool.status();
        info!(
            "Shutting down pool '{}' ({} idle connections)",
            self.name, status.available
        );

        // Send QUIT to idle connections with minimal timeout
        let mut timeouts = managed::Timeouts::new();
        timeouts.wait = Some(std::time::Duration::from_millis(1));

        for _ in 0..status.available {
            if let Ok(conn_obj) = self.pool.timeout_get(&timeouts).await {
                let mut conn = Object::take(conn_obj);
                let _ = conn.write_all(b"QUIT\r\n").await;
            } else {
                break;
            }
        }

        self.pool.close();
    }
}

#[async_trait]
impl ConnectionProvider for DeadpoolConnectionProvider {
    fn status(&self) -> PoolStatus {
        let status = self.pool.status();
        PoolStatus {
            available: status.available,
            max_size: status.max_size,
            created: status.size,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deadpool::managed::Manager;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::{Duration, sleep};

    /// Behavior modes for mock NNTP server
    #[derive(Clone, Copy, Debug)]
    enum MockServerBehavior {
        /// Responds normally to DATE commands with "111 ..."
        Healthy,
        /// Responds with error to DATE commands
        WrongResponse,
        /// Receives DATE but never responds (causes timeout)
        Timeout,
        /// Closes connection immediately after greeting
        CloseImmediately,
        /// Closes connection when DATE command is received
        CloseOnHealthCheck,
    }

    /// Mock NNTP server for testing health checks
    ///
    /// Automatically manages server lifecycle and provides clean teardown.
    /// Uses a builder pattern for configuration.
    struct MockNntpServer {
        port: u16,
        behavior: MockServerBehavior,
    }

    impl MockNntpServer {
        /// Create a new mock server with the specified behavior
        async fn new(behavior: MockServerBehavior) -> Self {
            let port = Self::find_available_port().await;
            let server = Self { port, behavior };
            server.start().await;
            // Give server time to start accepting connections
            sleep(Duration::from_millis(50)).await;
            server
        }

        /// Find an available port for the mock server
        async fn find_available_port() -> u16 {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            addr.port()
        }

        /// Start the mock server in the background
        async fn start(&self) {
            let port = self.port;
            let behavior = self.behavior;

            tokio::spawn(async move {
                let listener = TcpListener::bind(format!("127.0.0.1:{}", port))
                    .await
                    .unwrap();

                while let Ok((mut stream, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        let mut buffer = vec![0u8; 1024];

                        // Send greeting
                        let _ = stream.write_all(b"200 Mock NNTP Server Ready\r\n").await;

                        loop {
                            match stream.read(&mut buffer).await {
                                Ok(0) => break, // Connection closed
                                Ok(n) => {
                                    let command = String::from_utf8_lossy(&buffer[..n]);

                                    match behavior {
                                        MockServerBehavior::Healthy => {
                                            if command.starts_with("DATE") {
                                                let _ = stream
                                                    .write_all(b"111 20251013120000\r\n")
                                                    .await;
                                            }
                                        }
                                        MockServerBehavior::WrongResponse => {
                                            if command.starts_with("DATE") {
                                                let _ = stream
                                                    .write_all(b"500 Command not recognized\r\n")
                                                    .await;
                                            }
                                        }
                                        MockServerBehavior::Timeout => {
                                            if command.starts_with("DATE") {
                                                // Don't respond, causing timeout
                                                sleep(Duration::from_secs(5)).await;
                                            }
                                        }
                                        MockServerBehavior::CloseImmediately => {
                                            // Close connection immediately after greeting
                                            break;
                                        }
                                        MockServerBehavior::CloseOnHealthCheck => {
                                            if command.starts_with("DATE") {
                                                // Close without responding
                                                break;
                                            }
                                        }
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                    });
                }
            });
        }

        /// Create a TcpManager configured to connect to this mock server
        fn create_manager(&self) -> TcpManager {
            TcpManager::new(
                "127.0.0.1".to_string(),
                self.port,
                "test".to_string(),
                None,
                None,
            )
        }

        /// Create a connection to this mock server
        async fn create_connection(&self) -> ConnectionStream {
            self.create_manager().create().await.unwrap()
        }
    }

    /// Test helper to run a health check and return the result
    async fn run_health_check(
        manager: &TcpManager,
        conn: &mut ConnectionStream,
    ) -> managed::RecycleResult<anyhow::Error> {
        manager.recycle(conn, &managed::Metrics::default()).await
    }

    /// Assert that an error message contains one of the expected substrings
    fn assert_error_contains(result: managed::RecycleResult<anyhow::Error>, expected: &[&str]) {
        assert!(result.is_err(), "Expected error but got Ok");
        if let Err(e) = result {
            let msg = format!("{:?}", e);
            let found = expected.iter().any(|s| msg.contains(s));
            assert!(
                found,
                "Error should contain one of {:?}, but was: {}",
                expected, msg
            );
        }
    }

    #[tokio::test]
    async fn test_health_check_passes_for_healthy_connection() {
        let server = MockNntpServer::new(MockServerBehavior::Healthy).await;
        let manager = server.create_manager();
        let mut conn = server.create_connection().await;

        let result = run_health_check(&manager, &mut conn).await;

        assert!(
            result.is_ok(),
            "Health check should pass for healthy connection"
        );
    }

    #[tokio::test]
    async fn test_health_check_fails_for_closed_connection() {
        let server = MockNntpServer::new(MockServerBehavior::CloseImmediately).await;
        let manager = server.create_manager();
        let mut conn = server.create_connection().await;

        // Wait for server to close
        sleep(Duration::from_millis(200)).await;

        let result = run_health_check(&manager, &mut conn).await;

        assert_error_contains(result, &["closed", "Connection"]);
    }

    #[tokio::test]
    async fn test_health_check_fails_for_wrong_response() {
        let server = MockNntpServer::new(MockServerBehavior::WrongResponse).await;
        let manager = server.create_manager();
        let mut conn = server.create_connection().await;

        let result = run_health_check(&manager, &mut conn).await;

        assert_error_contains(result, &["Unexpected", "500"]);
    }

    #[tokio::test]
    async fn test_health_check_timeout() {
        let server = MockNntpServer::new(MockServerBehavior::Timeout).await;
        let manager = server.create_manager();
        let mut conn = server.create_connection().await;

        let start = std::time::Instant::now();
        let result = run_health_check(&manager, &mut conn).await;
        let elapsed = start.elapsed();

        assert!(
            elapsed.as_secs() <= 3,
            "Health check should timeout within ~2 seconds, took {:?}",
            elapsed
        );
        assert_error_contains(result, &["timeout", "Timeout"]);
    }

    #[tokio::test]
    async fn test_health_check_connection_closed_during_check() {
        let server = MockNntpServer::new(MockServerBehavior::CloseOnHealthCheck).await;
        let manager = server.create_manager();
        let mut conn = server.create_connection().await;

        let result = run_health_check(&manager, &mut conn).await;

        assert_error_contains(result, &["closed", "Connection"]);
    }

    #[tokio::test]
    async fn test_tcp_level_check_detects_closed_plain_connection() {
        // This test needs manual setup since we need the server to close immediately
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Accept and immediately close
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let _ = stream.try_write(b"200 Mock NNTP Server\r\n");
                drop(stream); // Close immediately
            }
        });

        sleep(Duration::from_millis(50)).await;

        // Connect and get a stream
        let tcp = TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();

        let mut conn = ConnectionStream::Plain(tcp);

        // Consume greeting
        let mut buf = vec![0u8; 1024];
        let _ = conn.read(&mut buf).await;

        // Give the server time to close
        sleep(Duration::from_millis(100)).await;

        let manager = TcpManager::new(
            "127.0.0.1".to_string(),
            port,
            "test".to_string(),
            None,
            None,
        );

        let result = run_health_check(&manager, &mut conn).await;

        assert!(
            result.is_err(),
            "TCP-level check should detect closed connection"
        );
    }

    #[tokio::test]
    async fn test_health_check_preserves_connection_on_success() {
        let server = MockNntpServer::new(MockServerBehavior::Healthy).await;
        let manager = server.create_manager();
        let mut conn = server.create_connection().await;

        // Run health check multiple times - should keep succeeding
        for i in 0..3 {
            let result = run_health_check(&manager, &mut conn).await;
            assert!(result.is_ok(), "Health check #{} should pass", i + 1);
        }

        // Connection should still be usable
        conn.write_all(b"DATE\r\n").await.unwrap();

        let mut response = vec![0u8; 512];
        let n = conn.read(&mut response).await.unwrap();
        let response_str = String::from_utf8_lossy(&response[..n]);

        assert!(
            response_str.starts_with("111"),
            "Connection should still work after health checks"
        );
    }
}
