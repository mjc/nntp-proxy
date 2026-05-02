//! Core TCP connection manager for deadpool
//!
//! This module provides the `TcpManager` struct which handles the low-level
//! creation of optimized TCP/TLS connections to NNTP servers.

use deadpool::managed;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::connection_error::ConnectionError;
use crate::constants::socket::{POOL_RECV_BUFFER, POOL_SEND_BUFFER};
use crate::protocol::{RequestContext, authinfo_pass, authinfo_user};
use crate::stream::ConnectionStream;
use crate::tls::{TlsConfig, TlsManager};

async fn write_request(
    stream: &mut ConnectionStream,
    request: &RequestContext,
) -> std::io::Result<()> {
    request.write_wire_to(stream).await
}

/// Type alias for the deadpool connection pool
pub type Pool = managed::Pool<TcpManager>;

/// Optional settings for [`TcpManager`] construction
///
/// Groups optional parameters (credentials, TLS, compression) to keep
/// the `TcpManager::new()` signature concise.
#[derive(Debug, Clone)]
pub struct TcpManagerOptions {
    pub username: Option<String>,
    pub password: Option<String>,
    pub tls_config: Option<TlsConfig>,
    /// Wire compression mode: None = auto-detect, Some(true) = require, Some(false) = disable
    pub compress: Option<bool>,
    /// Compression level (0-9). None = fast (level 1).
    pub compress_level: Option<u32>,
    /// Send MODE READER to the backend after authentication.
    ///
    /// RFC 3977 §5.3: MODE READER switches a transit server to reading mode.
    /// Defaults to `true` — required for reader-capable backends.
    pub send_mode_reader: bool,
}

impl Default for TcpManagerOptions {
    fn default() -> Self {
        Self {
            username: None,
            password: None,
            tls_config: None,
            compress: None,
            compress_level: None,
            send_mode_reader: true,
        }
    }
}

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
    /// Wire compression mode: None = auto-detect, Some(true) = require, Some(false) = disable
    pub(crate) compress: Option<bool>,
    /// Compression level (0-9). None = fast (level 1).
    pub(crate) compress_level: Option<u32>,
    /// Whether to send MODE READER after authentication (RFC 3977 §5.3)
    pub(crate) send_mode_reader: bool,
}

impl TcpManager {
    /// Create a new `TcpManager` with optional TLS configuration
    ///
    /// If `options.tls_config` is `Some` with `use_tls = true`, the TLS manager is
    /// pre-initialized (certificates loaded). If `None` or `use_tls = false`,
    /// plain TCP connections are used.
    pub fn new(
        host: String,
        port: u16,
        name: String,
        options: TcpManagerOptions,
    ) -> Result<Self, ConnectionError> {
        let (tls_config, tls_manager) = match options.tls_config {
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
            username: options.username,
            password: options.password,
            tls_config,
            tls_manager,
            compress: options.compress,
            compress_level: options.compress_level,
            send_mode_reader: options.send_mode_reader,
        })
    }

    /// Create an optimized connection (TCP or TLS)
    pub(crate) async fn create_optimized_stream(
        &self,
    ) -> Result<ConnectionStream, ConnectionError> {
        // Resolve hostname
        let addr = format!("{}:{}", self.host, self.port);
        let Some(socket_addr) = tokio::net::lookup_host(&addr).await?.next() else {
            return Err(ConnectionError::DnsNoAddresses { address: addr });
        };

        // Create tokio TcpSocket (non-blocking from the start)
        let socket = if socket_addr.is_ipv4() {
            tokio::net::TcpSocket::new_v4()?
        } else {
            tokio::net::TcpSocket::new_v6()?
        };

        // Pre-connect options (buffer sizes, reuse)
        socket.set_recv_buffer_size(POOL_RECV_BUFFER as u32)?;
        socket.set_send_buffer_size(POOL_SEND_BUFFER as u32)?;
        socket.set_reuseaddr(true)?;

        // Async connect — does NOT block the tokio worker thread
        let tcp_stream = socket.connect(socket_addr).await?;

        // Post-connect options via socket2::SockRef (keepalive with params, nodelay)
        let sock_ref = socket2::SockRef::from(&tcp_stream);
        sock_ref.set_keepalive(true)?;
        let keepalive = socket2::TcpKeepalive::new()
            .with_time(std::time::Duration::from_secs(60))
            .with_interval(std::time::Duration::from_secs(10));
        sock_ref.set_tcp_keepalive(&keepalive)?;
        sock_ref.set_tcp_nodelay(true)?;

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
            Ok(ConnectionStream::tls(tls_stream))
        } else {
            Ok(ConnectionStream::plain(tcp_stream))
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

    /// Send MODE READER to the backend after authentication.
    ///
    /// RFC 3977 §5.3: MODE READER switches the server into reader mode.
    /// Valid responses are 200 (posting allowed) or 201 (posting not permitted).
    /// Any other response indicates the service is unavailable for reading.
    async fn negotiate_mode_reader(
        &self,
        stream: &mut ConnectionStream,
        buffer: &mut [u8],
    ) -> Result<(), ConnectionError> {
        stream.write_all(b"MODE READER\r\n").await?;
        stream.flush().await?;

        let n = stream.read(buffer).await?;
        let response = &buffer[..n];

        // RFC 3977 §5.3: 200 = reader mode + posting allowed, 201 = reader mode + posting not permitted
        if response.len() >= 3 && (response.starts_with(b"200") || response.starts_with(b"201")) {
            tracing::debug!(
                backend = %self.name,
                response = %String::from_utf8_lossy(&response[..n]).trim(),
                "MODE READER accepted"
            );
            return Ok(());
        }

        Err(ConnectionError::InvalidGreeting {
            backend: self.name.clone(),
            greeting: String::from_utf8_lossy(response).trim().to_string(),
        })
    }

    /// Negotiate COMPRESS DEFLATE (RFC 8054) with the backend server.
    ///
    /// Returns `Ok(true)` if compression was successfully negotiated,
    /// `Ok(false)` if compression was skipped or not supported.
    async fn negotiate_compression(
        &self,
        stream: &mut ConnectionStream,
        buffer: &mut [u8],
    ) -> Result<bool, ConnectionError> {
        if self.compress == Some(false) {
            return Ok(false);
        }

        stream.write_all(crate::protocol::COMPRESS_DEFLATE).await?;
        stream.flush().await?;

        let n = stream.read(buffer).await?;
        let response = &buffer[..n];
        let response_str = String::from_utf8_lossy(response);

        // 206 = Compression active
        if response.len() >= 3 && &response[..3] == b"206" {
            tracing::debug!(
                backend = %self.name,
                "COMPRESS DEFLATE negotiated successfully"
            );
            return Ok(true);
        }

        if self.compress == Some(true) {
            return Err(ConnectionError::CompressionRequired {
                backend: self.name.clone(),
                response: response_str.trim().to_string(),
            });
        }

        // Auto mode: compression not supported, continue without it
        tracing::debug!(
            backend = %self.name,
            response = %response_str.trim(),
            "COMPRESS DEFLATE not supported, continuing without compression"
        );
        Ok(false)
    }

    /// Perform AUTHINFO USER/PASS handshake if credentials are configured.
    async fn negotiate_auth(
        &self,
        stream: &mut ConnectionStream,
        buffer: &mut [u8],
    ) -> Result<(), ConnectionError> {
        let Some(username) = &self.username else {
            return Ok(());
        };

        write_request(stream, &authinfo_user(username)).await?;
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

            write_request(stream, &authinfo_pass(password)).await?;
            let n = stream.read(buffer).await?;
            let response = &buffer[..n];
            let response_str = String::from_utf8_lossy(response);

            if !matches!(
                crate::protocol::NntpResponse::parse(response),
                crate::protocol::NntpResponse::AuthSuccess
            ) {
                // Check for 482 (connection limit exceeded) before generic auth failure
                if crate::protocol::StatusCode::parse(response).is_some_and(|c| c.as_u16() == 482) {
                    tracing::error!(
                        backend = %self.name,
                        host = %self.host,
                        port = self.port,
                        response = %response_str.trim(),
                        "Backend connection limit exceeded"
                    );
                    return Err(ConnectionError::ConnectionLimitExceeded {
                        backend: self.name.clone(),
                        response: response_str.trim().to_string(),
                    });
                }

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
            }
            tracing::debug!(
                "Successfully authenticated to {} ({}:{}) as {}",
                self.name,
                self.host,
                self.port,
                username
            );
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

        if self.send_mode_reader {
            self.negotiate_mode_reader(&mut stream, &mut buffer).await?;
        }

        if self.negotiate_compression(&mut stream, &mut buffer).await? {
            let level = self.compress_level.unwrap_or(1);
            stream = stream.into_compressed(level)?;
        }

        Ok(stream)
    }

    async fn recycle(
        &self,
        conn: &mut ConnectionStream,
        _metrics: &managed::Metrics,
    ) -> managed::RecycleResult<ConnectionError> {
        use super::health_check::check_tcp_alive;
        match check_tcp_alive(conn) {
            Ok(()) => Ok(()),
            Err(e) => {
                // Shut down TCP immediately so backend releases the slot
                // before deadpool drops this and creates a replacement.
                let _ = socket2::SockRef::from(conn.underlying_tcp_stream())
                    .shutdown(std::net::Shutdown::Both);
                Err(e)
            }
        }
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
            TcpManagerOptions {
                username: Some("user".to_string()),
                password: Some("pass".to_string()),
                ..TcpManagerOptions::default()
            },
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
            TcpManagerOptions::default(),
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
            TcpManagerOptions {
                username: Some("user".to_string()),
                password: Some("pass".to_string()),
                tls_config: Some(tls_config),
                ..TcpManagerOptions::default()
            },
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
            TcpManagerOptions {
                username: Some("user".to_string()),
                password: Some("pass".to_string()),
                tls_config: Some(tls_config),
                ..TcpManagerOptions::default()
            },
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
            TcpManagerOptions {
                username: Some("user".to_string()),
                password: Some("pass".to_string()),
                ..TcpManagerOptions::default()
            },
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
            TcpManagerOptions {
                username: Some("user".to_string()),
                password: Some("pass".to_string()),
                ..TcpManagerOptions::default()
            },
        )
        .unwrap();

        let debug_str = format!("{manager:?}");
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
            TcpManagerOptions {
                tls_config: Some(tls_config),
                ..TcpManagerOptions::default()
            },
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
            TcpManagerOptions {
                tls_config: Some(tls_config),
                ..TcpManagerOptions::default()
            },
        );

        // Should fail because cert file doesn't exist
        assert!(result.is_err());
    }
}
