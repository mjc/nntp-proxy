//! Test helpers for integration tests
//!
//! This module provides reusable test utilities to reduce duplication
//! in integration tests.

use anyhow::Result;
use nntp_proxy::config::{Config, ServerConfig};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// Spawn a mock NNTP server that responds with a greeting
///
/// # Arguments
/// * `port` - Port to listen on
/// * `server_name` - Name to include in greeting message
///
/// # Returns
/// Handle to the background task running the mock server
pub fn spawn_mock_server(port: u16, server_name: &str) -> JoinHandle<()> {
    let name = server_name.to_string();
    tokio::spawn(async move {
        let addr = format!("127.0.0.1:{}", port);
        let listener = match TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("Failed to bind mock server on {}: {}", addr, e);
                return;
            }
        };

        while let Ok((mut stream, _)) = listener.accept().await {
            let name_clone = name.clone();
            tokio::spawn(async move {
                // Send greeting
                let greeting = format!("200 {} Ready\r\n", name_clone);
                let _ = stream.write_all(greeting.as_bytes()).await;

                // Echo commands until QUIT
                let mut buffer = [0; 1024];
                loop {
                    match stream.read(&mut buffer).await {
                        Ok(0) => break, // Connection closed
                        Ok(n) => {
                            // Check for QUIT
                            if buffer[..n].starts_with(b"QUIT") {
                                let _ = stream.write_all(b"205 Goodbye\r\n").await;
                                break;
                            }
                            // Echo back a simple response
                            let _ = stream.write_all(b"200 OK\r\n").await;
                        }
                        Err(_) => break,
                    }
                }
            });
        }
    })
}

/// Spawn a mock NNTP server that requires authentication
///
/// # Arguments
/// * `port` - Port to listen on
/// * `expected_user` - Expected username
/// * `expected_pass` - Expected password
///
/// # Returns
/// Handle to the background task running the mock server
pub fn spawn_mock_server_with_auth(
    port: u16,
    expected_user: &str,
    expected_pass: &str,
) -> JoinHandle<()> {
    let user = expected_user.to_string();
    let pass = expected_pass.to_string();

    tokio::spawn(async move {
        let addr = format!("127.0.0.1:{}", port);
        let listener = match TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(_) => return,
        };

        loop {
            if let Ok((mut stream, _)) = listener.accept().await {
                let user_clone = user.clone();
                let pass_clone = pass.clone();

                tokio::spawn(async move {
                    // Send greeting requesting auth
                    let _ = stream
                        .write_all(b"200 Server ready (auth required)\r\n")
                        .await;

                    let mut authenticated = false;
                    let mut buffer = [0; 1024];

                    while let Ok(n) = stream.read(&mut buffer).await {
                        if n == 0 {
                            break;
                        }

                        let cmd = String::from_utf8_lossy(&buffer[..n]);

                        if cmd.starts_with("AUTHINFO USER") {
                            if cmd.contains(&user_clone) {
                                let _ = stream.write_all(b"381 Password required\r\n").await;
                            } else {
                                let _ = stream.write_all(b"481 Authentication failed\r\n").await;
                            }
                        } else if cmd.starts_with("AUTHINFO PASS") {
                            if cmd.contains(&pass_clone) {
                                authenticated = true;
                                let _ = stream.write_all(b"281 Authentication accepted\r\n").await;
                            } else {
                                let _ = stream.write_all(b"481 Authentication failed\r\n").await;
                            }
                        } else if cmd.starts_with("QUIT") {
                            let _ = stream.write_all(b"205 Goodbye\r\n").await;
                            break;
                        } else if authenticated {
                            let _ = stream.write_all(b"200 OK\r\n").await;
                        } else {
                            let _ = stream.write_all(b"480 Authentication required\r\n").await;
                        }
                    }
                });
            }
        }
    })
}

/// Create a test configuration with the specified backend servers
///
/// # Arguments
/// * `server_ports` - List of (port, name) tuples for backend servers
///
/// # Returns
/// Configuration object ready for use in tests
pub fn create_test_config(server_ports: Vec<(u16, &str)>) -> Config {
    Config {
        servers: server_ports
            .into_iter()
            .map(|(port, name)| ServerConfig {
                host: "127.0.0.1".to_string(),
                port,
                name: name.to_string(),
                username: None,
                password: None,
                max_connections: 5,
            })
            .collect(),
        health_check: Default::default(),
    }
}

/// Create a test configuration with authentication
///
/// # Arguments
/// * `server_ports` - List of (port, name, user, pass) tuples for backend servers
///
/// # Returns
/// Configuration object ready for use in tests
pub fn create_test_config_with_auth(
    server_ports: Vec<(u16, &str, &str, &str)>,
    Config {
        servers: server_ports
            .into_iter()
            .map(|(port, name, user, pass)| ServerConfig {
                host: "127.0.0.1".to_string(),
                port,
                name: name.to_string(),
                username: Some(user.to_string()),
                password: Some(pass.to_string()),
                max_connections: 5,
            })
            .collect(),
        health_check: Default::default(),
    }
}

/// Wait for a server to be ready by attempting to connect
///
/// # Arguments
/// * `addr` - Address to connect to (e.g., "127.0.0.1:8080")
/// * `max_attempts` - Maximum number of connection attempts
///
/// # Returns
/// Ok(()) if connection succeeds, Err otherwise
pub async fn wait_for_server(addr: &str, max_attempts: u32) -> Result<()> {
    for attempt in 1..=max_attempts {
        tokio::time::sleep(Duration::from_millis(50)).await;

        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return Ok(());
        }

        if attempt == max_attempts {
            return Err(anyhow::anyhow!(
                "Server at {} did not become ready after {} attempts",
                addr,
                max_attempts
            ));
        }
    }

    Ok(())
}

/// Read a complete NNTP response from a stream
///
/// # Arguments
/// * `stream` - TCP stream to read from
/// * `buffer` - Buffer to read into
/// * `timeout_ms` - Timeout in milliseconds
///
/// # Returns
/// Number of bytes read
pub async fn read_response(
    stream: &mut tokio::net::TcpStream,
    buffer: &mut [u8],
    timeout_ms: u64,
) -> Result<usize> {
    tokio::time::timeout(Duration::from_millis(timeout_ms), stream.read(buffer))
        .await?
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_spawn_mock_server() {
        let port = 19001;
        let _handle = spawn_mock_server(port, "TestServer");

        // Give server time to start
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Connect and verify greeting
        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();

        let mut buffer = [0; 1024];
        let n = stream.read(&mut buffer).await.unwrap();
        let response = String::from_utf8_lossy(&buffer[..n]);

        assert!(response.contains("200"));
        assert!(response.contains("TestServer"));
    }

    #[tokio::test]
    async fn test_create_test_config() {
        let config = create_test_config(vec![(19002, "server1"), (19003, "server2")]);

        assert_eq!(config.servers.len(), 2);
        assert_eq!(config.servers[0].port, 19002);
        assert_eq!(config.servers[1].port, 19003);
        assert_eq!(config.servers[0].name, "server1");
    }

    #[tokio::test]
    async fn test_wait_for_server() {
        let port = 19004;
        let _handle = spawn_mock_server(port, "WaitTest");

        let result = wait_for_server(&format!("127.0.0.1:{}", port), 20).await;
        assert!(result.is_ok());
    }
}
