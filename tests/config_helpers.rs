//! Test configuration helpers to reduce boilerplate and improve maintainability

use nntp_proxy::config::ServerConfig;
use nntp_proxy::types::{HostName, MaxConnections, Port, ServerName};

/// Create a basic server configuration for testing (no TLS)
pub fn create_test_server_config(host: &str, port: u16, name: &str) -> ServerConfig {
    ServerConfig {
        host: HostName::new(host.to_string()).expect("Valid hostname"),
        port: Port::new(port).expect("Valid port"),
        name: ServerName::new(name.to_string()).expect("Valid server name"),
        username: None,
        password: None,
        max_connections: MaxConnections::new(5).unwrap(),
        use_tls: false,
        tls_verify_cert: true,
        tls_cert_path: None,
        connection_keepalive: None,
        health_check_max_per_cycle: nntp_proxy::config::health_check_max_per_cycle(),
        health_check_pool_timeout: nntp_proxy::config::health_check_pool_timeout(),
    }
}

/// Create a server configuration with authentication
pub fn create_test_server_config_with_auth(
    host: &str,
    port: u16,
    name: &str,
    username: &str,
    password: &str,
) -> ServerConfig {
    ServerConfig {
        host: HostName::new(host.to_string()).expect("Valid hostname"),
        port: Port::new(port).expect("Valid port"),
        name: ServerName::new(name.to_string()).expect("Valid server name"),
        username: Some(username.to_string()),
        password: Some(password.to_string()),
        max_connections: MaxConnections::new(5).unwrap(),
        use_tls: false,
        tls_verify_cert: true,
        tls_cert_path: None,
        connection_keepalive: None,
        health_check_max_per_cycle: nntp_proxy::config::health_check_max_per_cycle(),
        health_check_pool_timeout: nntp_proxy::config::health_check_pool_timeout(),
    }
}

/// Create a TLS-enabled server configuration
pub fn create_test_server_config_with_tls(
    host: &str,
    port: u16,
    name: &str,
    tls_verify_cert: bool,
    tls_cert_path: Option<String>,
) -> ServerConfig {
    ServerConfig {
        host: HostName::new(host.to_string()).expect("Valid hostname"),
        port: Port::new(port).expect("Valid port"),
        name: ServerName::new(name.to_string()).expect("Valid server name"),
        username: None,
        password: None,
        max_connections: MaxConnections::new(5).unwrap(),
        use_tls: true,
        tls_verify_cert,
        tls_cert_path,
        connection_keepalive: None,
        health_check_max_per_cycle: nntp_proxy::config::health_check_max_per_cycle(),
        health_check_pool_timeout: nntp_proxy::config::health_check_pool_timeout(),
    }
}

/// Create a server configuration with custom max_connections
pub fn create_test_server_config_with_max_connections(
    host: &str,
    port: u16,
    name: &str,
    max_connections: usize,
) -> ServerConfig {
    ServerConfig {
        host: HostName::new(host.to_string()).expect("Valid hostname"),
        port: Port::new(port).expect("Valid port"),
        name: ServerName::new(name.to_string()).expect("Valid server name"),
        username: None,
        password: None,
        max_connections: MaxConnections::new(max_connections).unwrap(),
        use_tls: false,
        tls_verify_cert: true,
        tls_cert_path: None,
        connection_keepalive: None,
        health_check_max_per_cycle: nntp_proxy::config::health_check_max_per_cycle(),
        health_check_pool_timeout: nntp_proxy::config::health_check_pool_timeout(),
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
