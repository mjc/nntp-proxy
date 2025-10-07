use anyhow::Result;
use async_trait::async_trait;
use deadpool::managed;
use tokio::net::TcpStream;
use tracing::info;

use crate::constants::socket::{POOL_RECV_BUFFER, POOL_SEND_BUFFER};
use crate::pool::connection_trait::{ConnectionProvider, PoolStatus};

/// TCP connection manager for deadpool
#[derive(Debug)]
pub struct TcpManager {
    host: String,
    port: u16,
    #[allow(dead_code)]
    name: String,
    username: Option<String>,
    password: Option<String>,
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
        }
    }

    /// Create an optimized TCP connection
    async fn create_optimized_tcp_stream(&self) -> Result<TcpStream, anyhow::Error> {
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

        Ok(TcpStream::from_std(std_stream)?)
    }
}

impl managed::Manager for TcpManager {
    type Type = TcpStream;
    type Error = anyhow::Error;

    async fn create(&self) -> Result<TcpStream, anyhow::Error> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut stream = self.create_optimized_tcp_stream().await?;

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
        conn: &mut TcpStream,
        _: &managed::Metrics,
    ) -> managed::RecycleResult<anyhow::Error> {
        // Fast TCP-level health check: try reading without blocking
        // Healthy idle connections return WouldBlock, dead ones return 0 or error
        let mut peek_buf = [0u8; 1];
        match conn.try_read(&mut peek_buf) {
            Ok(0) => Err(managed::RecycleError::Message("Connection closed".into())),
            Ok(_) => Err(managed::RecycleError::Message("Unexpected data".into())),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
            Err(e) => Err(managed::RecycleError::Message(format!("{}", e).into())),
        }
    }

    fn detach(&self, _conn: &mut TcpStream) {}
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

    /// Create a connection provider from a server configuration
    ///
    /// This avoids unnecessary cloning of individual fields.
    pub fn from_server_config(server: &crate::config::ServerConfig) -> Self {
        Self::new(
            server.host.clone(),
            server.port,
            server.name.clone(),
            server.max_connections as usize,
            server.username.clone(),
            server.password.clone(),
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
