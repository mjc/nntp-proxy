use anyhow::Result;
use async_trait::async_trait;
use deadpool::managed;
use rustls::{ClientConfig, RootCertStore};
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio_rustls::{TlsConnector, client::TlsStream};
use tracing::{info, warn};

use crate::connection_error::ConnectionError;
use crate::constants::socket::{POOL_RECV_BUFFER, POOL_SEND_BUFFER};
use crate::pool::connection_trait::{ConnectionProvider, PoolStatus};
use crate::stream::ConnectionStream;

/// Configuration for TLS connections
#[derive(Debug, Clone)]
pub struct TlsConfig {
    pub use_tls: bool,
    pub tls_verify_cert: bool,
    pub tls_cert_path: Option<String>,
}

/// TCP connection manager for deadpool
#[derive(Debug)]
pub struct TcpManager {
    host: String,
    port: u16,
    #[allow(dead_code)]
    name: String,
    username: Option<String>,
    password: Option<String>,
    use_tls: bool,
    tls_verify_cert: bool,
    tls_cert_path: Option<String>,
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
            use_tls: false,
            tls_verify_cert: true,
            tls_cert_path: None,
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
            use_tls: tls_config.use_tls,
            tls_verify_cert: tls_config.tls_verify_cert,
            tls_cert_path: tls_config.tls_cert_path,
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
        if self.use_tls {
            let tls_stream = self.tls_handshake(tcp_stream).await?;
            Ok(ConnectionStream::Tls(tls_stream))
        } else {
            Ok(ConnectionStream::Plain(tcp_stream))
        }
    }

    /// Performs TLS handshake with the NNTP server using rustls optimized for maximum performance.
    /// 
    /// This method uses rustls with performance optimizations:
    /// - Ring crypto provider for fastest cryptographic operations
    /// - TLS 1.3 early data (0-RTT) enabled for faster reconnections
    /// - Session resumption enabled to avoid full handshakes
    /// - Pure Rust implementation (memory safe, no C dependencies)
    /// - Support for loading system certificates with rustls-native-certs
    /// - Fallback to Mozilla's webpki-roots CA bundle if system certs unavailable
    /// 
    /// Certificate loading priority:
    /// 1. Custom certificate from tls_cert_path (if provided)
    /// 2. System certificate store (via rustls-native-certs)
    /// 3. Mozilla CA bundle (webpki-roots) as fallback
    async fn tls_handshake(
        &self,
        stream: TcpStream,
    ) -> Result<TlsStream<TcpStream>, anyhow::Error> {
        use tracing::debug;

        // Create root certificate store
        let mut root_store = RootCertStore::empty();
        let mut cert_sources = Vec::new();

        // 1. Load custom CA certificate if provided
        if let Some(cert_path) = &self.tls_cert_path {
            debug!("TLS: Loading custom CA certificate from: {}", cert_path);
            let cert_data = std::fs::read(cert_path).map_err(|e| {
                anyhow::anyhow!("Failed to read TLS certificate from {}: {}", cert_path, e)
            })?;
            
            let certs = rustls_pemfile::certs(&mut cert_data.as_slice())
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| anyhow::anyhow!("Failed to parse TLS certificate: {}", e))?;
            
            for cert in certs {
                root_store.add(cert).map_err(|e| {
                    anyhow::anyhow!("Failed to add custom certificate to store: {}", e)
                })?;
            }
            cert_sources.push("custom certificate");
        }

        // 2. Try to load system certificates
        let cert_result = rustls_native_certs::load_native_certs();
        let mut added_count = 0;
        for cert in cert_result.certs {
            if root_store.add(cert).is_ok() {
                added_count += 1;
            }
        }
        if added_count > 0 {
            debug!("TLS: Loaded {} certificates from system store", added_count);
            cert_sources.push("system certificates");
        }
        
        // Log any errors but don't fail - we have fallback
        for error in cert_result.errors {
            warn!("TLS: Certificate loading error: {}", error);
        }

        // 3. Fallback to Mozilla CA bundle if no certificates loaded
        if root_store.is_empty() {
            debug!("TLS: No system certificates available, using Mozilla CA bundle fallback");
            root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            cert_sources.push("Mozilla CA bundle");
        }

        debug!("TLS: Certificate sources: {}", cert_sources.join(", "));

        // Create client config optimized for performance using ring crypto provider
        let config_builder = ClientConfig::builder_with_provider(
            Arc::new(rustls::crypto::ring::default_provider())
        )
        .with_safe_default_protocol_versions()
        .map_err(|e| anyhow::anyhow!("Failed to create TLS config with ring provider: {}", e))?
        .with_root_certificates(root_store);

        let mut config = if self.tls_verify_cert {
            debug!("TLS: Certificate verification enabled with ring crypto provider");
            config_builder.with_no_client_auth()
        } else {
            debug!("TLS: WARNING - Certificate verification disabled (insecure!)");
            // For rustls, disabling cert verification requires a custom verifier
            // This is intentionally more difficult than native-tls for security
            return Err(anyhow::anyhow!(
                "Certificate verification cannot be disabled with rustls. \
                This is intentional for security. If you need to connect to \
                servers with invalid certificates, consider using a custom CA certificate."
            ));
        };

        // Performance optimizations
        config.enable_early_data = true;  // Enable TLS 1.3 0-RTT for faster reconnections
        config.resumption = rustls::client::Resumption::default();  // Enable session resumption

        let connector = TlsConnector::from(Arc::new(config));
        let domain = rustls_pki_types::ServerName::try_from(self.host.as_str())
            .map_err(|e| anyhow::anyhow!("Invalid hostname for TLS: {}", e))?
            .to_owned();

        debug!("TLS: Connecting to {} with rustls", self.host);
        connector.connect(domain, stream).await.map_err(|e| {
            ConnectionError::TlsHandshake {
                backend: self.name.clone(),
                source: Box::new(e),
            }
            .into()
        })
    }
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
        let greeting = String::from_utf8_lossy(&buffer[..n]);

        if !greeting.starts_with("200") && !greeting.starts_with("201") {
            return Err(anyhow::anyhow!("Invalid greeting: {}", greeting.trim()));
        }

        // Authenticate if needed
        if let Some(username) = &self.username {
            stream
                .write_all(format!("AUTHINFO USER {}\r\n", username).as_bytes())
                .await?;
            let n = stream.read(&mut buffer).await?;
            let response = String::from_utf8_lossy(&buffer[..n]);

            if response.starts_with("381") {
                // Password required
                let password = self
                    .password
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Password required but not provided"))?;

                stream
                    .write_all(format!("AUTHINFO PASS {}\r\n", password).as_bytes())
                    .await?;
                let n = stream.read(&mut buffer).await?;
                let response = String::from_utf8_lossy(&buffer[..n]);

                if !response.starts_with("281") {
                    return Err(anyhow::anyhow!("Auth failed: {}", response.trim()));
                }
            } else if !response.starts_with("281") {
                return Err(anyhow::anyhow!(
                    "Unexpected auth response: {}",
                    response.trim()
                ));
            }
        }

        Ok(stream)
    }

    async fn recycle(
        &self,
        conn: &mut ConnectionStream,
        _: &managed::Metrics,
    ) -> managed::RecycleResult<anyhow::Error> {
        // Fast TCP-level health check: try reading without blocking
        // Healthy idle connections return WouldBlock, dead ones return 0 or error
        let mut peek_buf = [0u8; 1];

        // Only plain TCP supports try_read directly, for TLS we just return Ok
        match conn {
            ConnectionStream::Plain(tcp) => match tcp.try_read(&mut peek_buf) {
                Ok(0) => Err(managed::RecycleError::Message("Connection closed".into())),
                Ok(_) => Err(managed::RecycleError::Message("Unexpected data".into())),
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
                Err(e) => Err(managed::RecycleError::Message(format!("{}", e).into())),
            },
            ConnectionStream::Tls(_) => {
                // For TLS connections, we can't easily peek without blocking
                // Just assume the connection is healthy
                Ok(())
            }
        }
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
