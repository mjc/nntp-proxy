//! Core TCP connection manager for deadpool
//!
//! This module provides the `TcpManager` struct which handles the low-level
//! creation of optimized TCP/TLS connections to NNTP servers.

use deadpool::managed;
use std::sync::Arc;
use tokio::net::TcpStream;

use crate::connection_error::ConnectionError;
use crate::constants::socket::{POOL_RECV_BUFFER, POOL_SEND_BUFFER};
use crate::protocol::{authinfo_pass, authinfo_user};
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
    /// Create a new TcpManager with optional TLS configuration
    ///
    /// If `tls_config` is `Some` with `use_tls = true`, the TLS manager is
    /// pre-initialized (certificates loaded). If `None` or `use_tls = false`,
    /// plain TCP connections are used.
    pub fn new(
        host: String,
        port: u16,
        name: String,
        username: Option<String>,
        password: Option<String>,
        tls_config: Option<TlsConfig>,
    ) -> Result<Self, ConnectionError> {
        let (tls_config, tls_manager) = match tls_config {
            Some(cfg) if cfg.use_tls => {
                let mgr = Arc::new(TlsManager::new(cfg.clone()).map_err(|e| {
                    ConnectionError::TlsHandshake {
                        backend: name.clone(),
                        source: e.into(),
                    }
                })?);
                (cfg, Some(mgr))
            }
            Some(cfg) => (cfg, None),
            None => (TlsConfig::default(), None),
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
    pub(crate) async fn create_optimized_stream(
        &self,
    ) -> Result<ConnectionStream, ConnectionError> {
        use socket2::{Domain, Protocol, Socket, Type};

        // Resolve hostname
        let addr = format!("{}:{}", self.host, self.port);
        let Some(socket_addr) = tokio::net::lookup_host(&addr).await?.next() else {
            return Err(ConnectionError::DnsNoAddresses { address: addr });
        };

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
            let Some(tls_manager) = self.tls_manager.as_ref() else {
                return Err(ConnectionError::TlsHandshake {
                    backend: self.name.clone(),
                    source: "TLS enabled but TLS manager not initialized".into(),
                });
            };

            let tls_stream = tls_manager
                .handshake(tcp_stream, &self.host, &self.name)
                .await
                .map_err(|e| ConnectionError::TlsHandshake {
                    backend: self.name.clone(),
                    source: e.into(),
                })?;
            Ok(ConnectionStream::Tls(Box::new(tls_stream)))
        } else {
            Ok(ConnectionStream::Plain(tcp_stream))
        }
    }
}

// ============================================================================
// Connection setup: greeting, auth, and future negotiation hooks
// ============================================================================

impl TcpManager {
    /// Read and validate the NNTP server greeting.
    async fn consume_greeting(
        &self,
        stream: &mut ConnectionStream,
        buffer: &mut [u8],
    ) -> Result<(), ConnectionError> {
        use tokio::io::AsyncReadExt;

        let n = stream.read(buffer).await?;
        let greeting = &buffer[..n];
        let greeting_str = String::from_utf8_lossy(greeting);

        if !matches!(
            crate::protocol::NntpResponse::parse(greeting),
            crate::protocol::NntpResponse::Greeting(_)
        ) {
            return Err(ConnectionError::InvalidGreeting {
                backend: self.name.clone(),
                greeting: greeting_str.trim().to_string(),
            });
        }

        Ok(())
    }

    /// Perform AUTHINFO USER/PASS handshake if credentials are configured.
    async fn negotiate_auth(
        &self,
        stream: &mut ConnectionStream,
        buffer: &mut [u8],
    ) -> Result<(), ConnectionError> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let Some(username) = &self.username else {
            return Ok(());
        };

        stream.write_all(authinfo_user(username).as_bytes()).await?;
        let n = stream.read(buffer).await?;
        let response = &buffer[..n];
        let response_str = String::from_utf8_lossy(response);

        if matches!(
            crate::protocol::NntpResponse::parse(response),
            crate::protocol::NntpResponse::AuthRequired(_)
        ) {
            // Password required
            let Some(password) = self.password.as_ref() else {
                return Err(ConnectionError::PasswordRequired {
                    backend: self.name.clone(),
                });
            };

            stream.write_all(authinfo_pass(password).as_bytes()).await?;
            let n = stream.read(buffer).await?;
            let response = &buffer[..n];
            let response_str = String::from_utf8_lossy(response);

            if !matches!(
                crate::protocol::NntpResponse::parse(response),
                crate::protocol::NntpResponse::AuthSuccess
            ) {
                tracing::error!(
                    "Authentication failed for {} ({}:{}) - Server response: {} - Username: {}",
                    self.name,
                    self.host,
                    self.port,
                    response_str.trim(),
                    username
                );
                return Err(ConnectionError::AuthenticationFailed {
                    backend: self.name.clone(),
                    response: response_str.trim().to_string(),
                });
            } else {
                tracing::debug!(
                    "Successfully authenticated to {} ({}:{}) as {}",
                    self.name,
                    self.host,
                    self.port,
                    username
                );
            }
        } else if !matches!(
            crate::protocol::NntpResponse::parse(response),
            crate::protocol::NntpResponse::AuthSuccess
        ) {
            return Err(ConnectionError::UnexpectedAuthResponse {
                backend: self.name.clone(),
                response: response_str.trim().to_string(),
            });
        }

        Ok(())
    }
}

// ============================================================================
// Manager trait implementation
// ============================================================================

impl managed::Manager for TcpManager {
    type Type = ConnectionStream;
    type Error = ConnectionError;

    async fn create(&self) -> Result<ConnectionStream, ConnectionError> {
        let mut stream = self.create_optimized_stream().await?;
        let mut buffer = [0u8; 4096];

        self.consume_greeting(&mut stream, &mut buffer).await?;
        self.negotiate_auth(&mut stream, &mut buffer).await?;
        // Future: self.negotiate_compression(&mut stream, &mut buffer).await?
        // See feature/wire-compression-rfc8054

        Ok(stream)
    }

    async fn recycle(
        &self,
        conn: &mut ConnectionStream,
        _metrics: &managed::Metrics,
    ) -> managed::RecycleResult<ConnectionError> {
        use super::health_check::check_tcp_alive;
        // Only do fast TCP-level check on recycle
        // Full health checks happen periodically via the periodic health checks
        check_tcp_alive(conn)?;
        Ok(())
    }

    fn detach(&self, _conn: &mut ConnectionStream) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tcp_manager_new_plain() {
        let manager = TcpManager::new(
            "news.example.com".to_string(),
            119,
            "TestServer".to_string(),
            Some("user".to_string()),
            Some("pass".to_string()),
            None,
        )
        .unwrap();

        assert_eq!(manager.host, "news.example.com");
        assert_eq!(manager.port, 119);
        assert_eq!(manager.name, "TestServer");
        assert_eq!(manager.username, Some("user".to_string()));
        assert_eq!(manager.password, Some("pass".to_string()));
        assert!(!manager.tls_config.use_tls);
        assert!(manager.tls_manager.is_none());
    }

    #[test]
    fn test_tcp_manager_new_without_auth() {
        let manager = TcpManager::new(
            "news.example.com".to_string(),
            563,
            "SecureServer".to_string(),
            None,
            None,
            None,
        )
        .unwrap();

        assert_eq!(manager.host, "news.example.com");
        assert_eq!(manager.port, 563);
        assert_eq!(manager.name, "SecureServer");
        assert!(manager.username.is_none());
        assert!(manager.password.is_none());
    }

    #[test]
    fn test_tcp_manager_new_with_tls_disabled() {
        let tls_config = TlsConfig::default(); // use_tls = false
        let manager = TcpManager::new(
            "news.example.com".to_string(),
            119,
            "PlainServer".to_string(),
            Some("user".to_string()),
            Some("pass".to_string()),
            Some(tls_config),
        )
        .unwrap();

        assert_eq!(manager.host, "news.example.com");
        assert_eq!(manager.port, 119);
        assert!(!manager.tls_config.use_tls);
        assert!(manager.tls_manager.is_none());
    }

    #[test]
    fn test_tcp_manager_new_with_tls_enabled() {
        let tls_config = TlsConfig {
            use_tls: true,
            tls_verify_cert: true,
            tls_cert_path: None,
        };
        let manager = TcpManager::new(
            "secure.example.com".to_string(),
            563,
            "SecureServer".to_string(),
            Some("user".to_string()),
            Some("pass".to_string()),
            Some(tls_config),
        )
        .unwrap();

        assert_eq!(manager.host, "secure.example.com");
        assert_eq!(manager.port, 563);
        assert!(manager.tls_config.use_tls);
        assert!(manager.tls_manager.is_some());
    }

    #[test]
    fn test_tcp_manager_clone() {
        let manager = TcpManager::new(
            "news.example.com".to_string(),
            119,
            "TestServer".to_string(),
            Some("user".to_string()),
            Some("pass".to_string()),
            None,
        )
        .unwrap();

        let cloned = manager.clone();
        assert_eq!(cloned.host, manager.host);
        assert_eq!(cloned.port, manager.port);
        assert_eq!(cloned.name, manager.name);
        assert_eq!(cloned.username, manager.username);
        assert_eq!(cloned.password, manager.password);
    }

    #[test]
    fn test_tcp_manager_debug_format() {
        let manager = TcpManager::new(
            "news.example.com".to_string(),
            119,
            "TestServer".to_string(),
            Some("user".to_string()),
            Some("pass".to_string()),
            None,
        )
        .unwrap();

        let debug_str = format!("{:?}", manager);
        assert!(debug_str.contains("TcpManager"));
        assert!(debug_str.contains("news.example.com"));
        assert!(debug_str.contains("119"));
    }

    #[test]
    fn test_tcp_manager_with_tls_manager_is_some() {
        let tls_config = TlsConfig {
            use_tls: true,
            tls_verify_cert: false,
            tls_cert_path: None,
        };
        let manager = TcpManager::new(
            "secure.example.com".to_string(),
            563,
            "SecureServer".to_string(),
            None,
            None,
            Some(tls_config),
        )
        .unwrap();

        assert!(manager.tls_manager.is_some());

        // Verify TLS manager is an Arc (cheap clone)
        let arc_clone = manager.tls_manager.as_ref().unwrap().clone();
        assert!(Arc::ptr_eq(
            manager.tls_manager.as_ref().unwrap(),
            &arc_clone
        ));
    }

    #[test]
    fn test_tcp_manager_with_tls_cert_path() {
        let tls_config = TlsConfig {
            use_tls: true,
            tls_verify_cert: true,
            tls_cert_path: Some("/path/to/ca.pem".to_string()),
        };

        // This will fail due to missing file, but tests the construction path
        let result = TcpManager::new(
            "secure.example.com".to_string(),
            563,
            "SecureServer".to_string(),
            None,
            None,
            Some(tls_config),
        );

        // Should fail because cert file doesn't exist
        assert!(result.is_err());
    }
}
