//! Tests for PrecheckCommand configuration

use nntp_proxy::cache::BackendAvailability;
use nntp_proxy::config::PrecheckCommand;

/// Test configuration deserialization
#[test]
fn test_precheck_command_serde_stat() {
    let stat: PrecheckCommand = serde_json::from_str(r#""stat""#).unwrap();
    assert_eq!(stat, PrecheckCommand::Stat);
}

#[test]
fn test_precheck_command_serde_head() {
    let head: PrecheckCommand = serde_json::from_str(r#""head""#).unwrap();
    assert_eq!(head, PrecheckCommand::Head);
}

#[test]
fn test_precheck_command_serde_auto() {
    let auto: PrecheckCommand = serde_json::from_str(r#""auto""#).unwrap();
    assert_eq!(auto, PrecheckCommand::Auto);
}

#[test]
fn test_precheck_command_default() {
    assert_eq!(PrecheckCommand::default(), PrecheckCommand::Auto);
}

/// Test configuration display
#[test]
fn test_precheck_command_display_stat() {
    assert_eq!(PrecheckCommand::Stat.to_string(), "STAT command");
}

#[test]
fn test_precheck_command_display_head() {
    assert_eq!(PrecheckCommand::Head.to_string(), "HEAD command");
}

#[test]
fn test_precheck_command_display_auto() {
    assert_eq!(
        PrecheckCommand::Auto.to_string(),
        "auto-detect (both commands until disagreement)"
    );
}

/// Test as_str() method
#[test]
fn test_precheck_command_as_str() {
    assert_eq!(PrecheckCommand::Stat.as_str(), "STAT command");
    assert_eq!(PrecheckCommand::Head.as_str(), "HEAD command");
    assert_eq!(
        PrecheckCommand::Auto.as_str(),
        "auto-detect (both commands until disagreement)"
    );
}

/// Test equality
#[test]
fn test_precheck_command_equality() {
    assert_eq!(PrecheckCommand::Stat, PrecheckCommand::Stat);
    assert_eq!(PrecheckCommand::Head, PrecheckCommand::Head);
    assert_eq!(PrecheckCommand::Auto, PrecheckCommand::Auto);

    assert_ne!(PrecheckCommand::Stat, PrecheckCommand::Head);
    assert_ne!(PrecheckCommand::Stat, PrecheckCommand::Auto);
    assert_ne!(PrecheckCommand::Head, PrecheckCommand::Auto);
}

/// Test clone
#[test]
fn test_precheck_command_clone() {
    let stat = PrecheckCommand::Stat;
    let stat2 = stat;
    assert_eq!(stat, stat2);
}

/// Test copy
#[test]
fn test_precheck_command_copy() {
    let stat = PrecheckCommand::Stat;
    #[allow(clippy::clone_on_copy)]
    let stat2 = stat.clone();
    assert_eq!(stat, stat2);
}

/// Unit test: BackendAvailability bitmap
#[test]
fn test_backend_availability_with_precheck() {
    let mut availability = BackendAvailability::new();

    // Mark backend 0 as having article (from STAT precheck)
    availability.mark_available(nntp_proxy::types::BackendId::from_index(0));
    assert!(availability.has_article(nntp_proxy::types::BackendId::from_index(0)));
    assert!(!availability.has_article(nntp_proxy::types::BackendId::from_index(1)));

    // Mark backend 1 as having article (from HEAD precheck)
    availability.mark_available(nntp_proxy::types::BackendId::from_index(1));
    assert!(availability.has_article(nntp_proxy::types::BackendId::from_index(1)));

    assert!(availability.has_any());
}

/// Test TOML deserialization
#[test]
fn test_precheck_command_toml_stat() {
    let toml = r#"
        [[servers]]
        host = "news.example.com"
        port = 119
        name = "Test Server"
        precheck_command = "stat"
    "#;

    let config: nntp_proxy::config::Config = toml::from_str(toml).unwrap();
    assert_eq!(config.servers[0].precheck_command, PrecheckCommand::Stat);
}

#[test]
fn test_precheck_command_toml_head() {
    let toml = r#"
        [[servers]]
        host = "news.example.com"
        port = 119
        name = "Test Server"
        precheck_command = "head"
    "#;

    let config: nntp_proxy::config::Config = toml::from_str(toml).unwrap();
    assert_eq!(config.servers[0].precheck_command, PrecheckCommand::Head);
}

#[test]
fn test_precheck_command_toml_auto() {
    let toml = r#"
        [[servers]]
        host = "news.example.com"
        port = 119
        name = "Test Server"
        precheck_command = "auto"
    "#;

    let config: nntp_proxy::config::Config = toml::from_str(toml).unwrap();
    assert_eq!(config.servers[0].precheck_command, PrecheckCommand::Auto);
}

#[test]
fn test_precheck_command_toml_default() {
    let toml = r#"
        [[servers]]
        host = "news.example.com"
        port = 119
        name = "Test Server"
    "#;

    let config: nntp_proxy::config::Config = toml::from_str(toml).unwrap();
    // Should default to Auto
    assert_eq!(config.servers[0].precheck_command, PrecheckCommand::Auto);
}

/// Test that precheck_command is passed through ServerBuilder
#[test]
fn test_server_builder_precheck_command() {
    use nntp_proxy::config::Server;

    let server = Server::builder("news.example.com", 119)
        .name("Test")
        .precheck_command(PrecheckCommand::Head)
        .build()
        .unwrap();

    assert_eq!(server.precheck_command, PrecheckCommand::Head);
}

/// Test debug formatting
#[test]
fn test_precheck_command_debug() {
    let stat = PrecheckCommand::Stat;
    let debug_str = format!("{:?}", stat);
    assert_eq!(debug_str, "Stat");

    let head = PrecheckCommand::Head;
    let debug_str = format!("{:?}", head);
    assert_eq!(debug_str, "Head");

    let auto = PrecheckCommand::Auto;
    let debug_str = format!("{:?}", auto);
    assert_eq!(debug_str, "Auto");
}
