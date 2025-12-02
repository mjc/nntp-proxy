//! Validated string types that enforce invariants at construction time

use nutype::nutype;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Validation errors for string types
#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum ValidationError {
    #[error("hostname cannot be empty or whitespace")]
    EmptyHostName,
    #[error("server name cannot be empty or whitespace")]
    EmptyServerName,
    #[error("invalid hostname: {0}")]
    InvalidHostName(String),
    #[error("port cannot be 0")]
    InvalidPort,
    #[error("invalid message ID: {0}")]
    InvalidMessageId(String),
    #[error("config path cannot be empty")]
    EmptyConfigPath,
    #[error("username cannot be empty or whitespace")]
    EmptyUsername,
    #[error("password cannot be empty or whitespace")]
    EmptyPassword,
}

/// Validated hostname (non-empty, non-whitespace)
#[nutype(
    sanitize(trim),
    validate(not_empty),
    derive(
        Debug,
        Clone,
        PartialEq,
        Eq,
        Hash,
        Display,
        AsRef,
        Deref,
        TryFrom,
        Serialize,
        Deserialize
    )
)]
pub struct HostName(String);

/// Validated server name (non-empty, non-whitespace)
#[nutype(
    sanitize(trim),
    validate(not_empty),
    derive(
        Debug,
        Clone,
        PartialEq,
        Eq,
        Hash,
        Display,
        AsRef,
        Deref,
        TryFrom,
        Serialize,
        Deserialize
    )
)]
pub struct ServerName(String);

/// Validated username (non-empty, non-whitespace)
#[nutype(
    sanitize(trim),
    validate(not_empty),
    derive(
        Debug,
        Clone,
        PartialEq,
        Eq,
        Hash,
        Display,
        AsRef,
        Deref,
        TryFrom,
        Serialize,
        Deserialize
    )
)]
pub struct Username(String);

/// Validated password (non-empty, non-whitespace)
#[nutype(
    sanitize(trim),
    validate(not_empty),
    derive(
        Debug,
        Clone,
        PartialEq,
        Eq,
        Hash,
        Display,
        AsRef,
        Deref,
        TryFrom,
        Serialize,
        Deserialize
    )
)]
pub struct Password(String);

// Convert nutype errors to our ValidationError
impl From<HostNameError> for ValidationError {
    fn from(_: HostNameError) -> Self {
        ValidationError::EmptyHostName
    }
}

impl From<ServerNameError> for ValidationError {
    fn from(_: ServerNameError) -> Self {
        ValidationError::EmptyServerName
    }
}

impl From<UsernameError> for ValidationError {
    fn from(_: UsernameError) -> Self {
        ValidationError::EmptyUsername
    }
}

impl From<PasswordError> for ValidationError {
    fn from(_: PasswordError) -> Self {
        ValidationError::EmptyPassword
    }
}

/// Validated configuration file path (non-empty, non-whitespace)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConfigPath(PathBuf);

impl ConfigPath {
    pub fn new(path: impl AsRef<Path>) -> Result<Self, ValidationError> {
        let path_ref = path.as_ref();
        let path_str = path_ref.to_str().ok_or(ValidationError::EmptyConfigPath)?;
        if path_str.trim().is_empty() {
            return Err(ValidationError::EmptyConfigPath);
        }
        Ok(Self(path_ref.to_path_buf()))
    }

    #[must_use]
    #[inline]
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    #[must_use]
    #[inline]
    pub fn as_str(&self) -> &str {
        self.0.to_str().unwrap_or("")
    }
}

impl AsRef<Path> for ConfigPath {
    #[inline]
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl fmt::Display for ConfigPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

impl std::str::FromStr for ConfigPath {
    type Err = ValidationError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl TryFrom<String> for ConfigPath {
    type Error = ValidationError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}

impl TryFrom<&str> for ConfigPath {
    type Error = ValidationError;
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}

impl Serialize for ConfigPath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ConfigPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::new(s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // Property test strategies for non-empty strings
    fn non_empty_string() -> impl Strategy<Value = String> {
        "[a-zA-Z0-9._@-]{1,100}"
    }

    fn path_string() -> impl Strategy<Value = String> {
        "[a-zA-Z0-9._/-]{1,100}"
    }

    // Property tests for HostName
    proptest! {
        #[test]
        fn hostname_serde_roundtrip(s in non_empty_string()) {
            let hostname = HostName::try_new(s).unwrap();
            let json = serde_json::to_string(&hostname).unwrap();
            let deserialized: HostName = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(hostname, deserialized);
        }
    }

    // Property tests for ServerName
    proptest! {
        #[test]
        fn server_name_serde_roundtrip(s in non_empty_string()) {
            let server = ServerName::try_new(s).unwrap();
            let json = serde_json::to_string(&server).unwrap();
            let deserialized: ServerName = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(server, deserialized);
        }
    }

    // Property tests for Username
    proptest! {
        #[test]
        fn username_serde_roundtrip(s in non_empty_string()) {
            let username = Username::try_new(s).unwrap();
            let json = serde_json::to_string(&username).unwrap();
            let deserialized: Username = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(username, deserialized);
        }
    }

    // Property tests for Password
    proptest! {
        #[test]
        fn password_serde_roundtrip(s in non_empty_string()) {
            let password = Password::try_new(s).unwrap();
            let json = serde_json::to_string(&password).unwrap();
            let deserialized: Password = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(password, deserialized);
        }
    }

    // Property tests for ConfigPath
    proptest! {
        #[test]
        fn config_path_non_empty_accepts(s in path_string()) {
            let config = ConfigPath::try_from(s.clone()).unwrap();
            prop_assert_eq!(config.as_str(), &s);
        }

        #[test]
        fn config_path_as_path_roundtrip(s in path_string()) {
            let config = ConfigPath::try_from(s.clone()).unwrap();
            let path: &Path = config.as_ref();
            prop_assert_eq!(path, Path::new(&s));
        }

        #[test]
        fn config_path_serde_roundtrip(s in path_string()) {
            let config = ConfigPath::try_from(s).unwrap();
            let json = serde_json::to_string(&config).unwrap();
            let deserialized: ConfigPath = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(config, deserialized);
        }
    }

    // Edge case tests - verify sanitization behavior
    #[test]
    fn username_with_spaces_accepted() {
        // Username with spaces in content is valid (trim only removes leading/trailing)
        assert!(Username::try_new("  user  ".to_string()).is_ok());
        assert!(Username::try_new("user name".to_string()).is_ok());
    }

    #[test]
    fn password_with_spaces_accepted() {
        assert!(Password::try_new("   pass   ".to_string()).is_ok());
        assert!(Password::try_new("P@ssw0rd!".to_string()).is_ok());
        assert!(Password::try_new("密码123".to_string()).is_ok());
    }

    #[test]
    fn config_path_with_spaces_accepted() {
        assert!(ConfigPath::try_from("my config.toml").is_ok());
        assert!(ConfigPath::try_from("/absolute/path/config.toml").is_ok());
        assert!(ConfigPath::try_from("./relative/config.toml").is_ok());
        assert!(ConfigPath::try_from("../parent/config.toml").is_ok());
    }

    // Deserialization failure tests
    #[test]
    fn hostname_deserialize_empty_fails() {
        let json = "\"\"";
        let result: Result<HostName, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn config_path_deserialize_empty_fails() {
        let json = "\"\"";
        let result: Result<ConfigPath, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    // ValidationError tests
    #[test]
    fn validation_error_messages() {
        assert_eq!(
            ValidationError::EmptyHostName.to_string(),
            "hostname cannot be empty or whitespace"
        );
        assert_eq!(
            ValidationError::EmptyServerName.to_string(),
            "server name cannot be empty or whitespace"
        );
        assert_eq!(
            ValidationError::EmptyUsername.to_string(),
            "username cannot be empty or whitespace"
        );
        assert_eq!(
            ValidationError::EmptyPassword.to_string(),
            "password cannot be empty or whitespace"
        );
        assert_eq!(
            ValidationError::EmptyConfigPath.to_string(),
            "config path cannot be empty"
        );
    }

    #[test]
    fn validation_error_clone() {
        let err = ValidationError::EmptyHostName;
        let cloned = err.clone();
        assert_eq!(err, cloned);
    }
}
