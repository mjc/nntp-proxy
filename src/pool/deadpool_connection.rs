use anyhow::Result;
use async_trait::async_trait;
use deadpool::managed;
use tokio::net::TcpStream;
use tracing::info;

use crate::constants::socket::{POOL_RECV_BUFFER, POOL_SEND_BUFFER};
use crate::pool::connection_trait::{ConnectionProvider, PoolStatus};
use crate::stream::ConnectionStream;
use crate::tls::{TlsConfig, TlsManager};

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
            let tls_stream = tls_manager.handshake(tcp_stream, &self.host, &self.name).await?;
            Ok(ConnectionStream::Tls(tls_stream))
        } else {
            Ok(ConnectionStream::Plain(tcp_stream))
        }
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
