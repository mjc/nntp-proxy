//! Core TCP connection manager for deadpool
//!
//! This module provides the `TcpManager` struct which handles the low-level
//! creation of optimized TCP/TLS connections to NNTP servers.

use deadpool::managed;
use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::sync::Notify;

use super::dns::DnsCache;
use crate::connection_error::ConnectionError;
use crate::protocol::{RequestContext, authinfo_pass, authinfo_user};
use crate::stream::ConnectionStream;
use crate::tls::{TlsConfig, TlsManager};

/// Type alias for the deadpool connection pool
pub type Pool = managed::Pool<TcpManager>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompressionSupport {
    Supported,
    Unsupported,
}

#[derive(Debug, Default)]
enum CompressionSupportState {
    #[default]
    Unknown,
    Probing(Arc<Notify>),
    Supported,
    Unsupported,
}

/// Optional settings for [`TcpManager`] construction
///
/// Groups optional parameters (credentials, TLS, compression) to keep
/// the `TcpManager::new()` signature concise.
#[derive(Debug, Clone)]
pub struct TcpManagerOptions {
    pub username: Option<String>,
    pub password: Option<String>,
    pub tls_config: Option<TlsConfig>,
    /// TCP receive buffer size for this connection.
    pub recv_buffer_size: usize,
    /// TCP send buffer size for this connection.
    pub send_buffer_size: usize,
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
            recv_buffer_size: crate::constants::socket::HIGH_THROUGHPUT_RECV_BUFFER,
            send_buffer_size: crate::constants::socket::HIGH_THROUGHPUT_SEND_BUFFER,
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
    dns_cache: DnsCache,
    next_resolved_socket_addr: Arc<AtomicUsize>,
    pub(crate) recv_buffer_size: usize,
    pub(crate) send_buffer_size: usize,
    /// Wire compression mode: None = auto-detect, Some(true) = require, Some(false) = disable
    pub(crate) compress: Option<bool>,
    /// Compression level (0-9). None = fast (level 1).
    pub(crate) compress_level: Option<u32>,
    compression_support: Arc<Mutex<CompressionSupportState>>,
    /// Whether to send MODE READER after authentication (RFC 3977 §5.3)
    pub(crate) send_mode_reader: bool,
}

impl TcpManager {
    fn socket_buffer_size_u32(size: usize, label: &str) -> Result<u32, ConnectionError> {
        u32::try_from(size).map_err(|_| {
            ConnectionError::IoError(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{label} socket buffer size {size} exceeds u32::MAX"),
            ))
        })
    }

    async fn resolve_socket_addrs(&self) -> Result<Arc<[SocketAddr]>, ConnectionError> {
        self.dns_cache.resolve_socket_addrs().await
    }

    async fn refresh_socket_addrs(&self) -> Result<Arc<[SocketAddr]>, ConnectionError> {
        self.dns_cache.refresh_socket_addrs().await
    }

    fn is_ipv6_network_unreachable(socket_addr: SocketAddr, error: &ConnectionError) -> bool {
        socket_addr.is_ipv6()
            && matches!(
                error,
                ConnectionError::IoError(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NetworkUnreachable | io::ErrorKind::HostUnreachable
                    )
            )
    }

    /// Create a new `TcpManager` with optional TLS configuration
    ///
    /// If `options.tls_config` is `Some` with `use_tls = true`, the TLS manager is
    /// pre-initialized (certificates loaded). If `None` or `use_tls = false`,
    /// plain TCP connections are used.
    ///
    /// # Errors
    /// Returns any TLS initialization error when TLS is enabled for the manager.
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

        let dns_cache = DnsCache::new(host.clone(), port, name.clone())?;

        Ok(Self {
            host,
            port,
            name,
            username: options.username,
            password: options.password,
            tls_config,
            tls_manager,
            dns_cache,
            next_resolved_socket_addr: Arc::new(AtomicUsize::new(0)),
            recv_buffer_size: options.recv_buffer_size,
            send_buffer_size: options.send_buffer_size,
            compress: options.compress,
            compress_level: options.compress_level,
            compression_support: Arc::new(Mutex::new(CompressionSupportState::Unknown)),
            send_mode_reader: options.send_mode_reader,
        })
    }

    async fn connect_socket_addr(
        &self,
        socket_addr: SocketAddr,
    ) -> Result<TcpStream, ConnectionError> {
        // Create tokio TcpSocket (non-blocking from the start)
        let socket = if socket_addr.is_ipv4() {
            tokio::net::TcpSocket::new_v4()?
        } else {
            tokio::net::TcpSocket::new_v6()?
        };

        // Pre-connect options (buffer sizes, reuse)
        if self.recv_buffer_size > 0 {
            socket.set_recv_buffer_size(Self::socket_buffer_size_u32(
                self.recv_buffer_size,
                "receive",
            )?)?;
        }
        if self.send_buffer_size > 0 {
            socket.set_send_buffer_size(Self::socket_buffer_size_u32(
                self.send_buffer_size,
                "send",
            )?)?;
        }
        socket.set_reuseaddr(true)?;

        // Async connect — does NOT block the tokio worker thread
        let tcp_stream = socket.connect(socket_addr).await?;

        // Post-connect options via socket2::SockRef (keepalive with params, nodelay)
        let sock_ref = socket2::SockRef::from(&tcp_stream);
        sock_ref.set_keepalive(true)?;
        let keepalive = socket2::TcpKeepalive::new()
            .with_time(crate::constants::duration_polyfill::from_minutes(1))
            .with_interval(std::time::Duration::from_secs(10));
        sock_ref.set_tcp_keepalive(&keepalive)?;
        sock_ref.set_tcp_nodelay(true)?;

        Ok(tcp_stream)
    }

    async fn create_connected_tcp_stream(&self) -> Result<TcpStream, ConnectionError> {
        let addrs = self.resolve_socket_addrs().await?;
        let last_error = match self.try_resolved_socket_addrs(&addrs).await {
            Ok(tcp_stream) => return Ok(tcp_stream),
            Err(last_error) => last_error,
        };

        if self.dns_cache.is_ip_literal() {
            return Err(
                last_error.unwrap_or_else(|| ConnectionError::DnsNoAddresses {
                    address: format!("{}:{}", self.host, self.port),
                }),
            );
        }

        tracing::debug!(
            backend = %self.name,
            host = %self.host,
            "All cached backend socket addresses failed; refreshing DNS before final connect pass"
        );

        let addrs = self.refresh_socket_addrs().await?;
        self.try_resolved_socket_addrs(&addrs)
            .await
            .map_err(|last_error| {
                last_error.unwrap_or_else(|| ConnectionError::DnsNoAddresses {
                    address: format!("{}:{}", self.host, self.port),
                })
            })
    }

    async fn try_resolved_socket_addrs(
        &self,
        addrs: &[SocketAddr],
    ) -> Result<TcpStream, Option<ConnectionError>> {
        let start = self
            .next_resolved_socket_addr
            .fetch_add(1, Ordering::Relaxed)
            % addrs.len();
        let mut last_error = None;

        let mut remaining_addrs = (0..addrs.len())
            .map(|offset| addrs[(start + offset) % addrs.len()])
            .collect::<VecDeque<_>>();

        while let Some(socket_addr) = remaining_addrs.pop_front() {
            match self.connect_socket_addr(socket_addr).await {
                Ok(tcp_stream) => return Ok(tcp_stream),
                Err(error) => {
                    if Self::is_ipv6_network_unreachable(socket_addr, &error) {
                        self.dns_cache.remove_cached_ipv6_socket_addrs();
                        remaining_addrs.retain(SocketAddr::is_ipv4);
                    }

                    tracing::debug!(
                        backend = %self.name,
                        host = %self.host,
                        socket_addr = %socket_addr,
                        error = %error,
                        "Backend socket address connect failed; trying next resolved address"
                    );
                    last_error = Some(error);
                }
            }
        }

        Err(last_error)
    }

    /// Create an optimized connection (TCP or TLS)
    pub(crate) async fn create_optimized_stream(
        &self,
    ) -> Result<ConnectionStream, ConnectionError> {
        let tcp_stream = self.create_connected_tcp_stream().await?;

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
    /// Read one single-line setup reply using the same backend reply framing
    /// facade as normal commands.
    ///
    /// Connection setup commands are not on the hot article path, but they must
    /// still tolerate split TCP reads and must not open another place that
    /// reasons about response line boundaries.
    async fn read_backend_setup_reply(
        stream: &mut ConnectionStream,
        request: &RequestContext,
        buffer: &mut [u8],
    ) -> Result<String, ConnectionError> {
        match crate::session::backend::read_single_line_reply(stream, request, buffer).await {
            Ok(reply) => Ok(reply),
            Err(
                crate::session::backend::SingleLineReplyReadError::Full { bytes_read }
                | crate::session::backend::SingleLineReplyReadError::Invalid { bytes_read },
            ) => Err(ConnectionError::IoError(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "invalid or truncated backend setup reply: {}",
                    String::from_utf8_lossy(&buffer[..bytes_read]).trim_end()
                ),
            ))),
            Err(crate::session::backend::SingleLineReplyReadError::Io(err)) => Err(err.into()),
            Err(crate::session::backend::SingleLineReplyReadError::Closed) => {
                Err(ConnectionError::IoError(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "backend closed while reading setup reply",
                )))
            }
        }
    }

    /// Read and validate the NNTP server greeting.
    async fn consume_greeting(
        &self,
        stream: &mut ConnectionStream,
        buffer: &mut [u8],
    ) -> Result<(), ConnectionError> {
        let request = RequestContext::from_verb_args(b"MODE", b"READER");
        let greeting = Self::read_backend_setup_reply(stream, &request, buffer).await?;

        if !crate::protocol::StatusCode::parse(greeting.as_bytes())
            .is_some_and(|code| code.is_greeting())
        {
            return Err(ConnectionError::InvalidGreeting {
                backend: self.name.clone(),
                greeting: greeting.trim().to_string(),
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

        let request = RequestContext::from_verb_args(b"MODE", b"READER");
        let response = Self::read_backend_setup_reply(stream, &request, buffer).await?;

        // RFC 3977 §5.3: 200 = reader mode + posting allowed, 201 = reader mode + posting not permitted
        if crate::protocol::StatusCode::parse(response.as_bytes())
            .is_some_and(|code| matches!(code.as_u16(), 200 | 201))
        {
            tracing::debug!(
                backend = %self.name,
                response = %response.trim(),
                "MODE READER accepted"
            );
            return Ok(());
        }

        Err(ConnectionError::InvalidGreeting {
            backend: self.name.clone(),
            greeting: response.trim().to_string(),
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

        if self.compress == Some(true) {
            return self
                .probe_compression_with_timeout(stream, buffer)
                .await
                .map(|support| matches!(support, CompressionSupport::Supported));
        }

        loop {
            let probe_waiter = {
                let mut cached_support = self.compression_support.lock().await;
                match &*cached_support {
                    CompressionSupportState::Unsupported => {
                        tracing::debug!(
                            backend = %self.name,
                            "Skipping COMPRESS DEFLATE; backend previously reported it unsupported"
                        );
                        return Ok(false);
                    }
                    CompressionSupportState::Supported => {
                        drop(cached_support);
                        return self
                            .probe_compression_with_timeout(stream, buffer)
                            .await
                            .map(|support| matches!(support, CompressionSupport::Supported));
                    }
                    CompressionSupportState::Probing(notify) => {
                        Some(notify.clone().notified_owned())
                    }
                    CompressionSupportState::Unknown => {
                        let notify = Arc::new(Notify::new());
                        *cached_support = CompressionSupportState::Probing(notify);
                        None
                    }
                }
            };

            if let Some(waiter) = probe_waiter {
                waiter.await;
                continue;
            }

            let support = self.probe_compression_with_timeout(stream, buffer).await;
            let mut cached_support = self.compression_support.lock().await;
            let notify = match std::mem::take(&mut *cached_support) {
                CompressionSupportState::Probing(notify) => notify,
                state => {
                    *cached_support = state;
                    return support.map(|support| matches!(support, CompressionSupport::Supported));
                }
            };

            match support {
                Ok(CompressionSupport::Supported) => {
                    *cached_support = CompressionSupportState::Supported;
                    notify.notify_waiters();
                    return Ok(true);
                }
                Ok(CompressionSupport::Unsupported) => {
                    *cached_support = CompressionSupportState::Unsupported;
                    notify.notify_waiters();
                    return Ok(false);
                }
                Err(err) => {
                    *cached_support = CompressionSupportState::Unknown;
                    notify.notify_waiters();
                    return Err(err);
                }
            }
        }
    }

    async fn probe_compression_with_timeout(
        &self,
        stream: &mut ConnectionStream,
        buffer: &mut [u8],
    ) -> Result<CompressionSupport, ConnectionError> {
        tokio::time::timeout(
            crate::constants::timeout::CONNECTION,
            self.probe_compression(stream, buffer),
        )
        .await
        .map_err(|_| {
            ConnectionError::IoError(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out negotiating COMPRESS DEFLATE",
            ))
        })?
    }

    async fn probe_compression(
        &self,
        stream: &mut ConnectionStream,
        buffer: &mut [u8],
    ) -> Result<CompressionSupport, ConnectionError> {
        stream.write_all(crate::protocol::COMPRESS_DEFLATE).await?;
        stream.flush().await?;

        let request = RequestContext::from_verb_args(b"COMPRESS", b"DEFLATE");
        let response = Self::read_backend_setup_reply(stream, &request, buffer).await?;

        // 206 = Compression active
        if crate::protocol::StatusCode::parse(response.as_bytes())
            .is_some_and(|code| code.as_u16() == 206)
        {
            tracing::debug!(
                backend = %self.name,
                "COMPRESS DEFLATE negotiated successfully"
            );
            return Ok(CompressionSupport::Supported);
        }

        if self.compress == Some(true) {
            return Err(ConnectionError::CompressionRequired {
                backend: self.name.clone(),
                response: response.trim().to_string(),
            });
        }

        // Auto mode: compression not supported, continue without it
        tracing::debug!(
            backend = %self.name,
            response = %response.trim(),
            "COMPRESS DEFLATE not supported, continuing without compression"
        );
        Ok(CompressionSupport::Unsupported)
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

        authinfo_user(username).write_wire_to(stream).await?;
        let user_request = authinfo_user(username);
        let response = Self::read_backend_setup_reply(stream, &user_request, buffer).await?;

        if crate::protocol::StatusCode::parse(response.as_bytes())
            .is_some_and(|code| code.requires_auth_credentials())
        {
            // Password required
            let Some(password) = self.password.as_ref() else {
                return Err(ConnectionError::PasswordRequired {
                    backend: self.name.clone(),
                });
            };

            authinfo_pass(password).write_wire_to(stream).await?;
            let pass_request = authinfo_pass(password);
            let response = Self::read_backend_setup_reply(stream, &pass_request, buffer).await?;

            if !crate::protocol::StatusCode::parse(response.as_bytes())
                .is_some_and(|code| code.is_auth_accepted())
            {
                // Check for 482 (connection limit exceeded) before generic auth failure
                if crate::protocol::StatusCode::parse(response.as_bytes())
                    .is_some_and(|c| c.as_u16() == 482)
                {
                    tracing::error!(
                        backend = %self.name,
                        host = %self.host,
                        port = self.port,
                        response = %response.trim(),
                        "Backend connection limit exceeded"
                    );
                    return Err(ConnectionError::ConnectionLimitExceeded {
                        backend: self.name.clone(),
                        response: response.trim().to_string(),
                    });
                }

                tracing::error!(
                    "Authentication failed for {} ({}:{}) - Server response: {} - Username: {}",
                    self.name,
                    self.host,
                    self.port,
                    response.trim(),
                    username
                );
                return Err(ConnectionError::AuthenticationFailed {
                    backend: self.name.clone(),
                    response: response.trim().to_string(),
                });
            }
            tracing::debug!(
                "Successfully authenticated to {} ({}:{}) as {}",
                self.name,
                self.host,
                self.port,
                username
            );
        } else if !crate::protocol::StatusCode::parse(response.as_bytes())
            .is_some_and(|code| code.is_auth_accepted())
        {
            return Err(ConnectionError::UnexpectedAuthResponse {
                backend: self.name.clone(),
                response: response.trim().to_string(),
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
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    #[test]
    fn test_socket_buffer_size_u32_rejects_oversized_values() {
        let result = TcpManager::socket_buffer_size_u32(u32::MAX as usize + 1, "receive");

        assert!(matches!(
            result,
            Err(ConnectionError::IoError(ref error))
                if error.kind() == io::ErrorKind::InvalidInput
                    && error.to_string().contains("receive socket buffer size")
        ));
    }

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
    fn ip_literal_socket_addr_parses_ipv4_without_dns() {
        let manager = TcpManager::new(
            "127.0.0.1".to_string(),
            119,
            "IpBackend".to_string(),
            TcpManagerOptions::default(),
        )
        .unwrap();

        assert!(manager.dns_cache.is_ip_literal());
    }

    #[test]
    fn ip_literal_socket_addr_parses_ipv6_without_dns() {
        let manager = TcpManager::new(
            "::1".to_string(),
            563,
            "IpBackend".to_string(),
            TcpManagerOptions::default(),
        )
        .unwrap();

        assert!(manager.dns_cache.is_ip_literal());
    }

    #[test]
    fn ip_literal_socket_addr_leaves_hostnames_for_dns() {
        let manager = TcpManager::new(
            "news.example.com".to_string(),
            119,
            "DnsBackend".to_string(),
            TcpManagerOptions::default(),
        )
        .unwrap();

        assert!(!manager.dns_cache.is_ip_literal());
    }

    #[test]
    fn ipv6_network_unreachable_matches_error_kind_only_for_ipv6() {
        let ipv6_addr = SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], 563));
        let ipv4_addr = SocketAddr::from(([127, 0, 0, 1], 563));
        let error = ConnectionError::IoError(io::Error::new(
            io::ErrorKind::NetworkUnreachable,
            "network unreachable",
        ));

        assert!(TcpManager::is_ipv6_network_unreachable(ipv6_addr, &error));
        assert!(!TcpManager::is_ipv6_network_unreachable(ipv4_addr, &error));
    }

    #[tokio::test]
    async fn create_connected_tcp_stream_resolves_hostname() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live_addr = listener.local_addr().unwrap();

        let manager = TcpManager::new(
            "localhost".to_string(),
            live_addr.port(),
            "DnsBackend".to_string(),
            TcpManagerOptions::default(),
        )
        .unwrap();

        let accept = tokio::spawn(async move { listener.accept().await.unwrap() });
        let stream = manager.create_connected_tcp_stream().await.unwrap();
        let _accepted = accept.await.unwrap();

        assert_eq!(stream.peer_addr().unwrap(), live_addr);
    }

    #[tokio::test]
    async fn create_connected_tcp_stream_ip_literal_skips_dns() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live_addr = listener.local_addr().unwrap();

        let manager = TcpManager::new(
            live_addr.ip().to_string(),
            live_addr.port(),
            "IpBackend".to_string(),
            TcpManagerOptions::default(),
        )
        .unwrap();

        let accept = tokio::spawn(async move { listener.accept().await.unwrap() });
        let stream = manager.create_connected_tcp_stream().await.unwrap();
        let _accepted = accept.await.unwrap();

        assert_eq!(stream.peer_addr().unwrap(), live_addr);
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
        assert!(cloned.dns_cache.shares_resolver_with(&manager.dns_cache));
        assert!(Arc::ptr_eq(
            &cloned.next_resolved_socket_addr,
            &manager.next_resolved_socket_addr
        ));
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

    #[tokio::test]
    async fn try_resolved_socket_addrs_tries_next_address_after_connect_error() {
        let unavailable_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let unavailable_addr = unavailable_listener.local_addr().unwrap();
        drop(unavailable_listener);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let available_addr = listener.local_addr().unwrap();
        let accept_task = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
        });

        let manager = TcpManager::new(
            "test.example.com".to_string(),
            available_addr.port(),
            "FallbackBackend".to_string(),
            TcpManagerOptions::default(),
        )
        .unwrap();

        let stream = manager
            .try_resolved_socket_addrs(&[unavailable_addr, available_addr])
            .await
            .expect("second resolved address should be tried after first connect error");

        assert_eq!(stream.peer_addr().unwrap(), available_addr);
        accept_task.await.unwrap();
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

    #[tokio::test]
    async fn mode_reader_negotiation_reads_split_setup_reply() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut command = [0u8; 13];
            stream.read_exact(&mut command).await.unwrap();
            assert_eq!(&command, b"MODE READER\r\n");
            stream.write_all(b"20").await.unwrap();
            stream.write_all(b"0 Posting allowed\r\n").await.unwrap();
        });

        let manager = TcpManager::new(
            addr.ip().to_string(),
            addr.port(),
            "SplitSetup".to_string(),
            TcpManagerOptions::default(),
        )
        .unwrap();
        let tcp_stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut stream = ConnectionStream::plain(tcp_stream);
        let mut buffer = [0u8; 64];

        manager
            .negotiate_mode_reader(&mut stream, &mut buffer)
            .await
            .expect("split MODE READER setup reply should be accepted");
    }

    #[tokio::test]
    async fn compression_negotiation_reads_split_unsupported_reply() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut command = [0u8; 18];
            stream.read_exact(&mut command).await.unwrap();
            assert_eq!(&command, crate::protocol::COMPRESS_DEFLATE);
            stream.write_all(b"50").await.unwrap();
            stream.write_all(b"0 Not supported\r\n").await.unwrap();
        });

        let manager = TcpManager::new(
            addr.ip().to_string(),
            addr.port(),
            "SplitCompression".to_string(),
            TcpManagerOptions {
                compress: None,
                ..TcpManagerOptions::default()
            },
        )
        .unwrap();
        let tcp_stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut stream = ConnectionStream::plain(tcp_stream);
        let mut buffer = [0u8; 64];

        let enabled = manager
            .negotiate_compression(&mut stream, &mut buffer)
            .await
            .expect("split unsupported compression reply should be accepted");

        assert!(!enabled);
    }

    #[tokio::test]
    async fn auto_compression_serializes_and_remembers_unsupported_backend() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let compress_commands = Arc::new(AtomicUsize::new(0));
        let server_commands = compress_commands.clone();

        tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let server_commands = server_commands.clone();
                tokio::spawn(async move {
                    let mut command = [0u8; 18];
                    let read = tokio::time::timeout(
                        std::time::Duration::from_millis(100),
                        stream.read_exact(&mut command),
                    )
                    .await;
                    if read.is_err() {
                        return;
                    }
                    read.unwrap().unwrap();
                    if command == crate::protocol::COMPRESS_DEFLATE {
                        server_commands.fetch_add(1, Ordering::SeqCst);
                        stream.write_all(b"500 Not supported\r\n").await.unwrap();
                    }
                });
            }
        });

        let manager = TcpManager::new(
            addr.ip().to_string(),
            addr.port(),
            "CachedUnsupportedCompression".to_string(),
            TcpManagerOptions {
                compress: None,
                ..TcpManagerOptions::default()
            },
        )
        .unwrap();

        let mut tasks = Vec::new();
        for _ in 0..2 {
            let manager = manager.clone();
            tasks.push(tokio::spawn(async move {
                let tcp_stream = tokio::net::TcpStream::connect(addr).await.unwrap();
                let mut stream = ConnectionStream::plain(tcp_stream);
                let mut buffer = [0u8; 64];

                let enabled = manager
                    .negotiate_compression(&mut stream, &mut buffer)
                    .await
                    .expect("unsupported compression should fall back in auto mode");

                assert!(!enabled);
            }));
        }

        for task in tasks {
            task.await.unwrap();
        }

        assert_eq!(
            compress_commands.load(Ordering::SeqCst),
            1,
            "auto mode should remember unsupported COMPRESS DEFLATE"
        );
    }
}
