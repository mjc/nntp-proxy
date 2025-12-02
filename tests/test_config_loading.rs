//! Tests for config/loading.rs module
//!
//! Tests file loading and fallback logic.

use anyhow::Result;
use nntp_proxy::config::{
    ConfigSource, create_default_config, load_config, load_config_with_fallback,
};
use std::io::Write;
use tempfile::NamedTempFile;

/// Test loading from TOML file
#[test]
fn test_load_config_from_file() -> Result<()> {
    let mut temp_file = NamedTempFile::new()?;

    let config_content = r#"
[[servers]]
host = "file.example.com"
port = 119
name = "File Server"
"#;
    temp_file.write_all(config_content.as_bytes())?;
    temp_file.flush()?;

    let path = temp_file.path().to_str().unwrap();
    let config = load_config(path)?;

    assert_eq!(config.servers.len(), 1);
    assert_eq!(config.servers[0].host.as_str(), "file.example.com");
    assert_eq!(config.servers[0].name.as_str(), "File Server");

    Ok(())
}

/// Test invalid TOML returns error
#[test]
fn test_invalid_toml_returns_error() -> Result<()> {
    let mut temp_file = NamedTempFile::new()?;

    let invalid_content = "this is not valid TOML [[[";
    temp_file.write_all(invalid_content.as_bytes())?;
    temp_file.flush()?;

    let path = temp_file.path().to_str().unwrap();
    let result = load_config(path);

    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("Failed to parse"));

    Ok(())
}

/// Test missing file returns error
#[test]
fn test_missing_file_returns_error() {
    let result = load_config("/nonexistent/path.toml");

    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("Failed to read"));
}

/// Test create_default_config generates valid config
#[test]
fn test_create_default_config() {
    let config = create_default_config();

    assert_eq!(config.servers.len(), 1);
    assert_eq!(config.servers[0].host.as_str(), "news.example.com");
    assert_eq!(config.servers[0].port.get(), 119);
    assert!(!config.servers[0].use_tls);
}

/// Test ConfigSource descriptions
#[test]
fn test_config_source_descriptions() {
    assert_eq!(ConfigSource::File.description(), "configuration file");
    assert_eq!(
        ConfigSource::Environment.description(),
        "environment variables"
    );
    assert_eq!(
        ConfigSource::DefaultCreated.description(),
        "default configuration (created)"
    );
}

/// Test load_config_with_fallback: file exists
#[test]
fn test_fallback_uses_existing_file() -> Result<()> {
    let mut temp_file = NamedTempFile::new()?;

    let config_content = r#"
[[servers]]
host = "existing.example.com"
port = 119
name = "Existing"
"#;
    temp_file.write_all(config_content.as_bytes())?;
    temp_file.flush()?;

    let path = temp_file.path().to_str().unwrap().to_string();
    let (config, source) = load_config_with_fallback(&path)?;

    assert_eq!(source, ConfigSource::File);
    assert_eq!(config.servers[0].host.as_str(), "existing.example.com");

    Ok(())
}
