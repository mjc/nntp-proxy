use anyhow::Result;
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::io::AsyncWriteExt;
use tracing::{error, info, warn};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Port to listen on
    #[arg(short, long, default_value = "8119")]
    port: u16,

    /// Configuration file path
    #[arg(short, long, default_value = "config.toml")]
    config: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Config {
    /// List of backend NNTP servers
    pub servers: Vec<ServerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub name: String,
}

#[derive(Clone)]
struct NntpProxy {
    servers: Vec<ServerConfig>,
    current_index: Arc<AtomicUsize>,
}

impl NntpProxy {
    fn new(config: Config) -> Self {
        Self {
            servers: config.servers,
            current_index: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Get the next server using round-robin
    fn next_server(&self) -> &ServerConfig {
        let index = self.current_index.fetch_add(1, Ordering::Relaxed);
        &self.servers[index % self.servers.len()]
    }

    async fn handle_client(&self, mut client_stream: TcpStream, client_addr: SocketAddr) -> Result<()> {
        info!("New client connection from {}", client_addr);

        // Get the next backend server
        let server = self.next_server();
        info!("Routing client {} to server {}:{}", client_addr, server.host, server.port);

        // Connect to backend server
        let backend_addr = format!("{}:{}", server.host, server.port);
        let mut backend_stream = match TcpStream::connect(&backend_addr).await {
            Ok(stream) => stream,
            Err(e) => {
                error!("Failed to connect to backend {}: {}", backend_addr, e);
                let _ = client_stream.write_all(b"400 Backend server unavailable\r\n").await;
                return Err(e.into());
            }
        };

        info!("Connected to backend server {}", backend_addr);

        // Split streams for bidirectional proxying
        let (client_read, client_write) = client_stream.split();
        let (backend_read, backend_write) = backend_stream.split();

        // Proxy data in both directions
        let client_to_backend = Self::proxy_data(client_read, backend_write, "client->backend");
        let backend_to_client = Self::proxy_data(backend_read, client_write, "backend->client");

        // Wait for either direction to close
        tokio::select! {
            result = client_to_backend => {
                if let Err(e) = result {
                    warn!("Client to backend proxy error: {}", e);
                }
            }
            result = backend_to_client => {
                if let Err(e) = result {
                    warn!("Backend to client proxy error: {}", e);
                }
            }
        }

        info!("Connection closed for client {}", client_addr);
        Ok(())
    }

    async fn proxy_data<R, W>(
        mut reader: R,
        mut writer: W,
        direction: &str,
    ) -> Result<()>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let mut buf = vec![0u8; 8192];
        loop {
            match tokio::io::AsyncReadExt::read(&mut reader, &mut buf).await {
                Ok(0) => break, // EOF
                Ok(n) => {
                    if let Err(e) = tokio::io::AsyncWriteExt::write_all(&mut writer, &buf[..n]).await {
                        error!("Write error in {}: {}", direction, e);
                        return Err(e.into());
                    }
                    if let Err(e) = tokio::io::AsyncWriteExt::flush(&mut writer).await {
                        error!("Flush error in {}: {}", direction, e);
                        return Err(e.into());
                    }
                }
                Err(e) => {
                    error!("Read error in {}: {}", direction, e);
                    return Err(e.into());
                }
            }
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    // Load configuration
    let config_content = tokio::fs::read_to_string(&args.config).await.unwrap_or_else(|_| {
        warn!("Config file not found, creating default config");
        let default_config = Config {
            servers: vec![
                ServerConfig {
                    host: "news.example.com".to_string(),
                    port: 119,
                    name: "Example News Server".to_string(),
                },
            ],
        };
        let config_toml = toml::to_string_pretty(&default_config).unwrap();
        std::fs::write(&args.config, &config_toml).expect("Failed to write default config");
        info!("Created default config file: {}", args.config);
        config_toml
    });

    let config: Config = toml::from_str(&config_content)?;
    
    if config.servers.is_empty() {
        anyhow::bail!("No servers configured");
    }

    info!("Loaded {} backend servers:", config.servers.len());
    for server in &config.servers {
        info!("  - {} ({}:{})", server.name, server.host, server.port);
    }

    // Create proxy
    let proxy = NntpProxy::new(config);

    // Start listening
    let listen_addr = format!("0.0.0.0:{}", args.port);
    let listener = TcpListener::bind(&listen_addr).await?;
    info!("NNTP proxy listening on {}", listen_addr);

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                let proxy_clone = proxy.clone();
                tokio::spawn(async move {
                    if let Err(e) = proxy_clone.handle_client(stream, addr).await {
                        error!("Error handling client {}: {}", addr, e);
                    }
                });
            }
            Err(e) => {
                error!("Failed to accept connection: {}", e);
            }
        }
    }
}
