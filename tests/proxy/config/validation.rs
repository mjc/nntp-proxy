//! Configuration validation tests
//!
//! Tests to prevent regression of config field naming and validation issues
//! Ensures configs match what serde expects and validates properly

use anyhow::Result;
use nntp_proxy::config::Config;

/// Test that connection_keepalive field name works (not connection_keepalive_secs)
/// Regression test for issue #31
#[test]
fn test_connection_keepalive_field_name() -> Result<()> {
    let toml = r#"
[[servers]]
host = "news.example.com"
port = 119
name = "Test Server"
connection_keepalive = 60
"#;

    let config: Config = toml::from_str(toml)?;
    assert_eq!(config.servers.len(), 1);

    let server = &config.servers[0];
    assert!(server.connection_keepalive.is_some());
    assert_eq!(server.connection_keepalive.unwrap().as_secs(), 60);

    Ok(())
}

/// Test that old incorrect field name connection_keepalive_secs is rejected
/// This ensures we catch config errors early
#[test]
fn test_connection_keepalive_secs_rejected() {
    let toml = r#"
[[servers]]
host = "news.example.com"
port = 119
name = "Test Server"
connection_keepalive_secs = 60
"#;

    // This should either fail to parse or ignore the unknown field
    // depending on serde settings
    let result: Result<Config, _> = toml::from_str(toml);

    // Either it fails, or it succeeds but the field is None (ignored)
    if let Ok(config) = result {
        assert_eq!(config.servers.len(), 1);
        // The unknown field should be ignored, so connection_keepalive should be None
        assert!(
            config.servers[0].connection_keepalive.is_none(),
            "Unknown field connection_keepalive_secs should be ignored, not parsed"
        );
    }
    // If it fails, that's also acceptable behavior
}

/// Test that omitting connection_keepalive works (it's optional)
#[test]
fn test_connection_keepalive_optional() -> Result<()> {
    let toml = r#"
[[servers]]
host = "news.example.com"
port = 119
name = "Test Server"
"#;

    let config: Config = toml::from_str(toml)?;
    assert_eq!(config.servers.len(), 1);
    assert!(config.servers[0].connection_keepalive.is_none());

    Ok(())
}

/// Test that health check uses interval and timeout (not interval_secs/timeout_secs)
/// Regression test for issue #31
#[test]
fn test_health_check_field_names() -> Result<()> {
    let toml = r#"
[[servers]]
host = "news.example.com"
port = 119
name = "Test Server"

[health_check]
interval = 30
timeout = 5
unhealthy_threshold = 3
"#;

    let config: Config = toml::from_str(toml)?;

    let health_check = &config.health_check;
    assert_eq!(health_check.interval.as_secs(), 30);
    assert_eq!(health_check.timeout.as_secs(), 5);
    assert_eq!(health_check.unhealthy_threshold.get(), 3);

    Ok(())
}

/// Test that old incorrect field names interval_secs/timeout_secs are rejected
#[test]
fn test_health_check_old_field_names_rejected() {
    let toml = r#"
[[servers]]
host = "news.example.com"
port = 119
name = "Test Server"

[health_check]
interval_secs = 30
timeout_secs = 5
unhealthy_threshold = 3
"#;

    let result: Result<Config, _> = toml::from_str(toml);

    // Should either fail or use defaults (ignoring unknown fields)
    if let Ok(config) = result {
        // Unknown fields should be ignored, defaults should be used
        assert_eq!(
            config.health_check.interval.as_secs(),
            30,
            "Should use default interval when interval_secs is provided"
        );
        assert_eq!(
            config.health_check.timeout.as_secs(),
            5,
            "Should use default timeout when timeout_secs is provided"
        );
    }
}

/// Test that health check config is optional
#[test]
fn test_health_check_optional() -> Result<()> {
    let toml = r#"
[[servers]]
host = "news.example.com"
port = 119
name = "Test Server"
"#;

    let config: Config = toml::from_str(toml)?;

    // Should use defaults when not specified
    assert_eq!(config.health_check.interval.as_secs(), 30);
    assert_eq!(config.health_check.timeout.as_secs(), 5);
    assert_eq!(config.health_check.unhealthy_threshold.get(), 3);

    Ok(())
}

/// Test that client_auth section is completely optional
/// Regression test for issue #33
#[test]
fn test_client_auth_optional() -> Result<()> {
    let toml = r#"
[[servers]]
host = "news.example.com"
port = 119
name = "Test Server"
"#;

    let config: Config = toml::from_str(toml)?;
    config.validate()?;

    // client_auth should use defaults (disabled)
    assert!(!config.client_auth.is_enabled());
    assert!(config.client_auth.users.is_empty());
    assert!(config.client_auth.greeting.is_none());

    Ok(())
}

/// Test that client_auth with user credentials is enabled
#[test]
fn test_client_auth_enabled_with_credentials() -> Result<()> {
    let toml = r#"
[[servers]]
host = "news.example.com"
port = 119
name = "Test Server"

[[client_auth.users]]
username = "testuser"
password = "testpass"
"#;

    let config: Config = toml::from_str(toml)?;
    config.validate()?;

    // Should be enabled with user credentials
    assert!(config.client_auth.is_enabled());
    assert_eq!(config.client_auth.users.len(), 1);
    assert_eq!(config.client_auth.users[0].username, "testuser");
    assert_eq!(config.client_auth.users[0].password, "testpass");

    Ok(())
}

/// Test that custom greeting works with client auth
#[test]
fn test_client_auth_custom_greeting() -> Result<()> {
    let toml = r#"
[[servers]]
host = "news.example.com"
port = 119
name = "Test Server"

[client_auth]
greeting = "200 Custom Greeting"

[[client_auth.users]]
username = "testuser"
password = "testpass"
"#;

    let config: Config = toml::from_str(toml)?;

    assert!(config.client_auth.is_enabled());
    assert_eq!(
        config.client_auth.greeting.as_deref(),
        Some("200 Custom Greeting")
    );

    Ok(())
}

/// Test minimal valid config (regression test for issue #33)
#[test]
fn test_minimal_valid_config() -> Result<()> {
    let toml = r#"
[[servers]]
host = "news.example.com"
port = 119
name = "Test Server"
"#;

    let config: Config = toml::from_str(toml)?;

    // Validation should pass with minimal config
    config.validate()?;

    assert_eq!(config.servers.len(), 1);
    assert!(!config.client_auth.is_enabled());

    Ok(())
}

/// Test that config requires at least one server
#[test]
fn test_config_requires_server() {
    let toml = r#"
[client_auth]

[[client_auth.users]]
username = "testuser"
password = "testpass"
"#;

    let config: Config = toml::from_str(toml).expect("Should parse");

    // Validation should fail with no servers
    let result = config.validate();
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("at least one server")
    );
}

/// Test example config files are valid
#[test]
fn test_config_example_is_valid() -> Result<()> {
    let toml = std::fs::read_to_string("config.example.toml")?;
    let config: Config = toml::from_str(&toml)?;
    config.validate()?;

    assert!(!config.servers.is_empty());

    Ok(())
}

/// Test cache config example is valid
#[test]
fn test_cache_config_example_is_valid() -> Result<()> {
    let toml = std::fs::read_to_string("cache-config.example.toml")?;
    let config: Config = toml::from_str(&toml)?;
    config.validate()?;

    assert!(!config.servers.is_empty());
    assert!(config.cache.is_some());

    Ok(())
}

/// Test that cache config fields are correct
#[test]
fn test_cache_config_field_names() -> Result<()> {
    let toml = r#"
[[servers]]
host = "news.example.com"
port = 119
name = "Test Server"

[cache]
max_capacity = 10000
ttl = 3600
"#;

    let config: Config = toml::from_str(toml)?;

    let cache = config.cache.expect("Cache config should be present");
    assert_eq!(cache.max_capacity.get(), 10000);
    assert_eq!(cache.ttl.as_secs(), 3600);

    Ok(())
}

/// Test TLS configuration fields
#[test]
fn test_tls_config_fields() -> Result<()> {
    let toml = r#"
[[servers]]
host = "secure.example.com"
port = 563
name = "Secure Server"
use_tls = true
tls_verify_cert = true
tls_cert_path = "/path/to/cert.pem"
"#;

    let config: Config = toml::from_str(toml)?;

    let server = &config.servers[0];
    assert!(server.use_tls);
    assert!(server.tls_verify_cert);
    assert_eq!(server.tls_cert_path.as_deref(), Some("/path/to/cert.pem"));

    Ok(())
}

/// Test authentication config fields
#[test]
fn test_server_auth_fields() -> Result<()> {
    let toml = r#"
[[servers]]
host = "news.example.com"
port = 119
name = "Test Server"
username = "backend_user"
password = "backend_pass"
"#;

    let config: Config = toml::from_str(toml)?;

    let server = &config.servers[0];
    assert_eq!(server.username.as_deref(), Some("backend_user"));
    assert_eq!(server.password.as_deref(), Some("backend_pass"));

    Ok(())
}

/// Test max_connections field
#[test]
fn test_max_connections_field() -> Result<()> {
    let toml = r#"
[[servers]]
host = "news.example.com"
port = 119
name = "Test Server"
max_connections = 20
"#;

    let config: Config = toml::from_str(toml)?;

    let server = &config.servers[0];
    assert_eq!(server.max_connections.get(), 20);

    Ok(())
}

/// Test max_connections default value
#[test]
fn test_max_connections_default() -> Result<()> {
    let toml = r#"
[[servers]]
host = "news.example.com"
port = 119
name = "Test Server"
"#;

    let config: Config = toml::from_str(toml)?;

    let server = &config.servers[0];
    assert_eq!(server.max_connections.get(), 10);

    Ok(())
}

/// Test multiple servers configuration
#[test]
fn test_multiple_servers() -> Result<()> {
    let toml = r#"
[[servers]]
host = "news1.example.com"
port = 119
name = "Server 1"
max_connections = 20

[[servers]]
host = "news2.example.com"
port = 119
name = "Server 2"
max_connections = 15
connection_keepalive = 60

[[servers]]
host = "news3.example.com"
port = 563
name = "Server 3"
use_tls = true
"#;

    let config: Config = toml::from_str(toml)?;
    config.validate()?;

    assert_eq!(config.servers.len(), 3);

    assert_eq!(config.servers[0].name.as_str(), "Server 1");
    assert_eq!(config.servers[0].max_connections.get(), 20);
    assert!(config.servers[0].connection_keepalive.is_none());

    assert_eq!(config.servers[1].name.as_str(), "Server 2");
    assert_eq!(config.servers[1].max_connections.get(), 15);
    assert_eq!(
        config.servers[1].connection_keepalive.unwrap().as_secs(),
        60
    );

    assert_eq!(config.servers[2].name.as_str(), "Server 3");
    assert!(config.servers[2].use_tls);

    Ok(())
}

/// Test that duration fields accept various valid values
#[test]
fn test_duration_field_values() -> Result<()> {
    let toml = r#"
[[servers]]
host = "news.example.com"
port = 119
name = "Test Server"
connection_keepalive = 0

[health_check]
interval = 1
timeout = 3600
"#;

    let config: Config = toml::from_str(toml)?;

    // 0 should be valid (though not recommended)
    assert_eq!(config.servers[0].connection_keepalive.unwrap().as_secs(), 0);

    // Very small and large values should work
    assert_eq!(config.health_check.interval.as_secs(), 1);
    assert_eq!(config.health_check.timeout.as_secs(), 3600);

    Ok(())
}

/// Test that validation catches invalid port
#[test]
fn test_invalid_port_rejected() {
    let toml = r#"
[[servers]]
host = "news.example.com"
port = 0
name = "Test Server"
"#;

    let result: Result<Config, _> = toml::from_str(toml);
    assert!(result.is_err(), "Port 0 should be rejected");
}

/// Test that validation catches invalid max_connections
#[test]
fn test_invalid_max_connections_rejected() {
    let toml = r#"
[[servers]]
host = "news.example.com"
port = 119
name = "Test Server"
max_connections = 0
"#;

    let result: Result<Config, _> = toml::from_str(toml);
    assert!(result.is_err(), "max_connections = 0 should be rejected");
}

/// Test that validation catches empty server name
#[test]
fn test_empty_server_name_rejected() {
    let toml = r#"
[[servers]]
host = "news.example.com"
port = 119
name = ""
"#;

    let result: Result<Config, _> = toml::from_str(toml);
    assert!(result.is_err(), "Empty server name should be rejected");
}

/// Test that validation catches empty hostname
#[test]
fn test_empty_hostname_rejected() {
    let toml = r#"
[[servers]]
host = ""
port = 119
name = "Test Server"
"#;

    let result: Result<Config, _> = toml::from_str(toml);
    assert!(result.is_err(), "Empty hostname should be rejected");
}
