//! Test configuration helpers to reduce boilerplate and improve maintainability

use nntp_proxy::config::ServerConfig;

/// Create a basic server configuration for testing (no TLS)
pub fn create_test_server_config(host: &str, port: u16, name: &str) -> ServerConfig {
    ServerConfig {
        host: host.to_string(),
        port,
        name: name.to_string(),
        username: None,
        password: None,
        max_connections: 5,
        use_tls: false,
        tls_verify_cert: true,
        tls_cert_path: None,
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
        host: host.to_string(),
        port,
        name: name.to_string(),
        username: Some(username.to_string()),
        password: Some(password.to_string()),
        max_connections: 5,
        use_tls: false,
        tls_verify_cert: true,
        tls_cert_path: None,
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
        host: host.to_string(),
        port,
        name: name.to_string(),
        username: None,
        password: None,
        max_connections: 5,
        use_tls: true,
        tls_verify_cert,
        tls_cert_path,
    }
}

/// Create a server configuration with custom max_connections
pub fn create_test_server_config_with_max_connections(
    host: &str,
    port: u16,
    name: &str,
    max_connections: u32,
) -> ServerConfig {
    ServerConfig {
        host: host.to_string(),
        port,
        name: name.to_string(),
        username: None,
        password: None,
        max_connections,
        use_tls: false,
        tls_verify_cert: true,
        tls_cert_path: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_basic_server_config() {
        let config = create_test_server_config("localhost", 119, "test-server");
        assert_eq!(config.host, "localhost");
        assert_eq!(config.port, 119);
        assert_eq!(config.name, "test-server");
        assert!(!config.use_tls);
        assert!(config.tls_verify_cert);
        assert!(config.username.is_none());
        assert!(config.password.is_none());
        assert_eq!(config.max_connections, 5);
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
        assert_eq!(config.max_connections, 20);
    }
}
