//! Core TCP connection manager for deadpool
//!
//! This module provides the `TcpManager` struct which handles the low-level
//! creation of optimized TCP/TLS connections to NNTP servers.

use anyhow::Result;
use deadpool::managed;
use std::sync::Arc;
use tokio::net::TcpStream;

use crate::constants::socket::{POOL_RECV_BUFFER, POOL_SEND_BUFFER};
use crate::stream::ConnectionStream;
use crate::tls::{TlsConfig, TlsManager};

/// Type alias for the deadpool connection pool
pub type Pool = managed::Pool<TcpManager>;

/// TCP connection manager for deadpool with cached TLS config
#[derive(Debug, Clone)]
pub struct TcpManager {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) name: String,
    pub(crate) username: Option<String>,
    pub(crate) password: Option<String>,
    pub(crate) tls_config: TlsConfig,
    /// Cached TLS manager with pre-loaded certificates (avoids base64 decode overhead)
    pub(crate) tls_manager: Option<Arc<TlsManager>>,
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
            tls_manager: None,
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
    ) -> Result<Self> {
        // Pre-initialize TLS manager if TLS is enabled
        let tls_manager = if tls_config.use_tls {
            Some(Arc::new(TlsManager::new(tls_config.clone())?))
        } else {
            None
        };

        Ok(Self {
            host,
            port,
            name,
            username,
            password,
            tls_config,
            tls_manager,
        })
    }

    /// Create an optimized connection (TCP or TLS)
    pub(crate) async fn create_optimized_stream(&self) -> Result<ConnectionStream, anyhow::Error> {
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
            // Use cached TLS manager to avoid re-parsing certificates
            let tls_manager = self
                .tls_manager
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("TLS enabled but TLS manager not initialized"))?;

            let tls_stream = tls_manager
                .handshake(tcp_stream, &self.host, &self.name)
                .await?;
            Ok(ConnectionStream::Tls(Box::new(tls_stream)))
        } else {
            Ok(ConnectionStream::Plain(tcp_stream))
        }
    }
}

// ============================================================================
// Manager trait implementation
// ============================================================================

/// Helper function to create an AUTHINFO USER command
#[inline]
fn authinfo_user(username: &str) -> String {
    format!("AUTHINFO USER {}\r\n", username)
}

/// Helper function to create an AUTHINFO PASS command
#[inline]
fn authinfo_pass(password: &str) -> String {
    format!("AUTHINFO PASS {}\r\n", password)
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

        if !crate::protocol::ResponseParser::is_greeting(greeting) {
            return Err(anyhow::anyhow!("Invalid greeting: {}", greeting_str.trim()));
        }

        // Authenticate if needed
        if let Some(username) = &self.username {
            stream.write_all(authinfo_user(username).as_bytes()).await?;
            let n = stream.read(&mut buffer).await?;
            let response = &buffer[..n];
            let response_str = String::from_utf8_lossy(response);

            if crate::protocol::ResponseParser::is_auth_required(response) {
                // Password required
                let password = self
                    .password
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Password required but not provided"))?;

                stream.write_all(authinfo_pass(password).as_bytes()).await?;
                let n = stream.read(&mut buffer).await?;
                let response = &buffer[..n];
                let response_str = String::from_utf8_lossy(response);

                if !crate::protocol::ResponseParser::is_auth_success(response) {
                    return Err(anyhow::anyhow!("Auth failed: {}", response_str.trim()));
                }
            } else if !crate::protocol::ResponseParser::is_auth_success(response) {
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
        use super::health_check::check_tcp_alive;
        // Only do fast TCP-level check on recycle
        // Full health checks happen periodically via the periodic health checks
        check_tcp_alive(conn)?;
        Ok(())
    }

    fn detach(&self, _conn: &mut ConnectionStream) {}
}
