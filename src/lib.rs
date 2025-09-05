use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::io::AsyncWriteExt;
use tracing::{error, info, warn};

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct Config {
    /// List of backend NNTP servers
    pub servers: Vec<ServerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct NntpProxy {
    servers: Vec<ServerConfig>,
    current_index: Arc<AtomicUsize>,
}

impl NntpProxy {
    pub fn new(config: Config) -> Result<Self> {
        if config.servers.is_empty() {
            anyhow::bail!("No servers configured");
        }
        
        Ok(Self {
            servers: config.servers,
            current_index: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Get the next server using round-robin
    pub fn next_server(&self) -> &ServerConfig {
        let index = self.current_index.fetch_add(1, Ordering::Relaxed);
        &self.servers[index % self.servers.len()]
    }

    /// Get the current server index (for testing)
    #[cfg(test)]
    pub fn current_index(&self) -> usize {
        self.current_index.load(Ordering::Relaxed) % self.servers.len()
    }

    /// Reset the server index (for testing)
    #[cfg(test)]
    pub fn reset_index(&self) {
        self.current_index.store(0, Ordering::Relaxed);
    }

    /// Get the list of servers
    pub fn servers(&self) -> &[ServerConfig] {
        &self.servers
    }

    pub async fn handle_client(&self, mut client_stream: TcpStream, client_addr: SocketAddr) -> Result<()> {
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

    pub async fn proxy_data<R, W>(
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

pub fn load_config(config_path: &str) -> Result<Config> {
    let config_content = std::fs::read_to_string(config_path)
        .map_err(|e| anyhow::anyhow!("Failed to read config file '{}': {}", config_path, e))?;
    
    let config: Config = toml::from_str(&config_content)
        .map_err(|e| anyhow::anyhow!("Failed to parse config file '{}': {}", config_path, e))?;
    
    Ok(config)
}

pub fn create_default_config() -> Config {
    Config {
        servers: vec![
            ServerConfig {
                host: "news.example.com".to_string(),
                port: 119,
                name: "Example News Server".to_string(),
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Arc;
    use tempfile::NamedTempFile;

    fn create_test_config() -> Config {
        Config {
            servers: vec![
                ServerConfig {
                    host: "server1.example.com".to_string(),
                    port: 119,
                    name: "Test Server 1".to_string(),
                },
                ServerConfig {
                    host: "server2.example.com".to_string(),
                    port: 119,
                    name: "Test Server 2".to_string(),
                },
                ServerConfig {
                    host: "server3.example.com".to_string(),
                    port: 119,
                    name: "Test Server 3".to_string(),
                },
            ],
        }
    }

    #[test]
    fn test_server_config_creation() {
        let config = ServerConfig {
            host: "news.example.com".to_string(),
            port: 119,
            name: "Example Server".to_string(),
        };

        assert_eq!(config.host, "news.example.com");
        assert_eq!(config.port, 119);
        assert_eq!(config.name, "Example Server");
    }

    #[test]
    fn test_config_creation() {
        let config = create_test_config();
        assert_eq!(config.servers.len(), 3);
        assert_eq!(config.servers[0].name, "Test Server 1");
        assert_eq!(config.servers[1].name, "Test Server 2");
        assert_eq!(config.servers[2].name, "Test Server 3");
    }

    #[test]
    fn test_proxy_creation_with_servers() {
        let config = create_test_config();
        let proxy = NntpProxy::new(config).expect("Failed to create proxy");
        
        assert_eq!(proxy.servers().len(), 3);
        assert_eq!(proxy.servers()[0].name, "Test Server 1");
    }

    #[test]
    fn test_proxy_creation_with_empty_servers() {
        let config = Config { servers: vec![] };
        let result = NntpProxy::new(config);
        
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No servers configured"));
    }

    #[test]
    fn test_round_robin_server_selection() {
        let config = create_test_config();
        let proxy = NntpProxy::new(config).expect("Failed to create proxy");
        
        proxy.reset_index();
        
        // Test first round
        assert_eq!(proxy.next_server().name, "Test Server 1");
        assert_eq!(proxy.next_server().name, "Test Server 2");
        assert_eq!(proxy.next_server().name, "Test Server 3");
        
        // Test wraparound
        assert_eq!(proxy.next_server().name, "Test Server 1");
        assert_eq!(proxy.next_server().name, "Test Server 2");
    }

    #[test]
    fn test_round_robin_with_single_server() {
        let config = Config {
            servers: vec![
                ServerConfig {
                    host: "single.example.com".to_string(),
                    port: 119,
                    name: "Single Server".to_string(),
                },
            ],
        };
        
        let proxy = NntpProxy::new(config).expect("Failed to create proxy");
        proxy.reset_index();
        
        // All requests should go to the same server
        assert_eq!(proxy.next_server().name, "Single Server");
        assert_eq!(proxy.next_server().name, "Single Server");
        assert_eq!(proxy.next_server().name, "Single Server");
    }

    #[test]
    fn test_concurrent_round_robin() {
        let config = create_test_config();
        let proxy = Arc::new(NntpProxy::new(config).expect("Failed to create proxy"));
        proxy.reset_index();
        
        let mut handles = vec![];
        let servers_selected = Arc::new(std::sync::Mutex::new(Vec::new()));
        
        // Spawn multiple tasks to test concurrent access
        for _ in 0..9 {
            let proxy_clone = Arc::clone(&proxy);
            let servers_clone = Arc::clone(&servers_selected);
            
            let handle = std::thread::spawn(move || {
                let server = proxy_clone.next_server();
                servers_clone.lock().unwrap().push(server.name.clone());
            });
            handles.push(handle);
        }
        
        // Wait for all tasks to complete
        for handle in handles {
            handle.join().unwrap();
        }
        
        let servers = servers_selected.lock().unwrap();
        assert_eq!(servers.len(), 9);
        
        // Count occurrences of each server (should be balanced)
        let server1_count = servers.iter().filter(|&s| s == "Test Server 1").count();
        let server2_count = servers.iter().filter(|&s| s == "Test Server 2").count();
        let server3_count = servers.iter().filter(|&s| s == "Test Server 3").count();
        
        // Each server should be selected 3 times
        assert_eq!(server1_count, 3);
        assert_eq!(server2_count, 3);
        assert_eq!(server3_count, 3);
    }

    #[test]
    fn test_load_config_from_file() -> Result<()> {
        let config = create_test_config();
        let config_toml = toml::to_string_pretty(&config)?;
        
        // Create a temporary file
        let mut temp_file = NamedTempFile::new()?;
        write!(temp_file, "{}", config_toml)?;
        
        // Load config from file
        let loaded_config = load_config(temp_file.path().to_str().unwrap())?;
        
        assert_eq!(loaded_config.servers.len(), 3);
        assert_eq!(loaded_config.servers[0].name, "Test Server 1");
        assert_eq!(loaded_config.servers[0].host, "server1.example.com");
        assert_eq!(loaded_config.servers[0].port, 119);
        
        Ok(())
    }

    #[test]
    fn test_load_config_nonexistent_file() {
        let result = load_config("/nonexistent/path/config.toml");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Failed to read config file"));
    }

    #[test]
    fn test_load_config_invalid_toml() -> Result<()> {
        let invalid_toml = "invalid toml content [[[";
        
        // Create a temporary file with invalid TOML
        let mut temp_file = NamedTempFile::new()?;
        write!(temp_file, "{}", invalid_toml)?;
        
        let result = load_config(temp_file.path().to_str().unwrap());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Failed to parse config file"));
        
        Ok(())
    }

    #[test]
    fn test_create_default_config() {
        let config = create_default_config();
        
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].host, "news.example.com");
        assert_eq!(config.servers[0].port, 119);
        assert_eq!(config.servers[0].name, "Example News Server");
    }

    #[test]
    fn test_config_serialization() -> Result<()> {
        let config = create_test_config();
        
        // Serialize to TOML
        let toml_string = toml::to_string_pretty(&config)?;
        assert!(toml_string.contains("server1.example.com"));
        assert!(toml_string.contains("Test Server 1"));
        
        // Deserialize back
        let deserialized: Config = toml::from_str(&toml_string)?;
        assert_eq!(deserialized, config);
        
        Ok(())
    }

    #[tokio::test]
    async fn test_proxy_data_empty_stream() -> Result<()> {
        use tokio::io::{empty, sink};
        
        let result = NntpProxy::proxy_data(empty(), sink(), "test").await;
        assert!(result.is_ok());
        
        Ok(())
    }

    #[tokio::test]
    async fn test_proxy_data_with_data() -> Result<()> {
        use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};
        
        let (mut client, server) = duplex(64);
        let (server_read, server_write) = tokio::io::split(server);
        
        // Send data in the background
        tokio::spawn(async move {
            let _ = client.write_all(b"Hello, world!").await;
            let _ = client.shutdown().await;
        });
        
        // Capture the proxied data
        let (mut output_reader, output_writer) = duplex(64);
        
        // Start proxying
        let proxy_task = tokio::spawn(async move {
            NntpProxy::proxy_data(server_read, output_writer, "test").await
        });
        
        // Close the write side so the proxy task can complete
        drop(server_write);
        
        // Read the proxied data
        let mut buffer = Vec::new();
        let _ = output_reader.read_to_end(&mut buffer).await;
        
        // Wait for proxy to finish
        let result = proxy_task.await.unwrap();
        assert!(result.is_ok());
        
        // Verify data was proxied correctly
        assert_eq!(buffer, b"Hello, world!");
        
        Ok(())
    }
}
