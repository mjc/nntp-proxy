//! Test configuration helpers to reduce boilerplate and improve maintainability
//!
//! These helpers use the Server builder pattern for cleaner, more maintainable test code.

use nntp_proxy::config::{Cache, Config, Server};
use nntp_proxy::types::{CacheCapacity, MaxConnections, Port};

/// Create a basic server configuration for testing (no TLS)
pub fn create_test_server_config(host: &str, port: u16, name: &str) -> Server {
    Server::builder(host, Port::try_new(port).unwrap())
        .name(name)
        .max_connections(MaxConnections::try_new(5).unwrap())
        .build()
        .expect("Valid server config")
}

/// Create a server configuration with authentication
pub fn create_test_server_config_with_auth(
    host: &str,
    port: u16,
    name: &str,
    username: &str,
    password: &str,
) -> Server {
    Server::builder(host, Port::try_new(port).unwrap())
        .name(name)
        .username(username)
        .password(password)
        .max_connections(MaxConnections::try_new(5).unwrap())
        .build()
        .expect("Valid server config")
}

/// Create a TLS-enabled server configuration
pub fn create_test_server_config_with_tls(
    host: &str,
    port: u16,
    name: &str,
    tls_verify_cert: bool,
    tls_cert_path: Option<String>,
) -> Server {
    let mut builder = Server::builder(host, Port::try_new(port).unwrap())
        .name(name)
        .max_connections(MaxConnections::try_new(5).unwrap())
        .use_tls(true)
        .tls_verify_cert(tls_verify_cert);

    if let Some(path) = tls_cert_path {
        builder = builder.tls_cert_path(path);
    }

    builder.build().expect("Valid server config")
}

/// Create a server configuration with custom max_connections
pub fn create_test_server_config_with_max_connections(
    host: &str,
    port: u16,
    name: &str,
    max_connections: usize,
) -> Server {
    Server::builder(host, Port::try_new(port).unwrap())
        .name(name)
        .max_connections(MaxConnections::try_new(max_connections).unwrap())
        .build()
        .expect("Valid server config")
}

/// Create a full proxy configuration with cache enabled for testing
pub fn create_test_config_with_cache(backend_port: u16, cache_capacity: usize) -> Config {
    Config {
        servers: vec![create_test_server_config(
            "localhost",
            backend_port,
            "test-backend",
        )],
        cache: Some(Cache {
            max_capacity: CacheCapacity::try_new(cache_capacity).unwrap(),
            ttl: std::time::Duration::from_secs(3600),
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_basic_server_config() {
        let config = create_test_server_config("localhost", 119, "test-server");
        assert_eq!(config.host.as_str(), "localhost");
        assert_eq!(config.port.get(), 119);
        assert_eq!(config.name.as_str(), "test-server");
        assert!(!config.use_tls);
        assert!(config.tls_verify_cert);
        assert!(config.username.is_none());
        assert!(config.password.is_none());
        assert_eq!(config.max_connections.get(), 5);
    }

    #[test]
    fn test_create_server_config_with_auth() {
        let config = create_test_server_config_with_auth(
            "server.example.com",
            563,
            "secure-server",
            "testuser",
            "testpass",
        );
        assert_eq!(config.username, Some("testuser".to_string()));
        assert_eq!(config.password, Some("testpass".to_string()));
    }

    #[test]
    fn test_create_server_config_with_tls() {
        let config = create_test_server_config_with_tls(
            "tls.example.com",
            563,
            "tls-server",
            false,
            Some("/path/to/cert.pem".to_string()),
        );
        assert!(config.use_tls);
        assert!(!config.tls_verify_cert);
        assert_eq!(config.tls_cert_path, Some("/path/to/cert.pem".to_string()));
    }

    #[test]
    fn test_create_server_config_with_max_connections() {
        let config = create_test_server_config_with_max_connections(
            "busy.example.com",
            119,
            "busy-server",
            20,
        );
        assert_eq!(config.max_connections.get(), 20);
    }
}
