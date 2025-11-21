//! Validated string types that enforce invariants at construction time

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

/// Generate validated non-empty string newtypes
macro_rules! validated_string {
    ($(#[$meta:meta])* $vis:vis struct $name:ident($err:path);) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
        #[serde(transparent)]
        $vis struct $name(String);

        impl $name {
            pub fn new(s: String) -> Result<Self, ValidationError> {
                if s.trim().is_empty() {
                    Err($err)
                } else {
                    Ok(Self(s))
                }
            }

            #[must_use]
            #[inline]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl AsRef<str> for $name {
            #[inline]
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl std::ops::Deref for $name {
            type Target = str;
            #[inline]
            fn deref(&self) -> &Self::Target {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl TryFrom<String> for $name {
            type Error = ValidationError;
            fn try_from(s: String) -> Result<Self, Self::Error> {
                Self::new(s)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let s = String::deserialize(deserializer)?;
                Self::new(s).map_err(serde::de::Error::custom)
            }
        }
    };
}

validated_string! {
    /// Validated hostname (non-empty, non-whitespace)
    pub struct HostName(ValidationError::EmptyHostName);
}

validated_string! {
    /// Validated server name (non-empty, non-whitespace)
    pub struct ServerName(ValidationError::EmptyServerName);
}

validated_string! {
    /// Validated username (non-empty, non-whitespace)
    pub struct Username(ValidationError::EmptyUsername);
}

validated_string! {
    /// Validated password (non-empty, non-whitespace)
    pub struct Password(ValidationError::EmptyPassword);
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
    use std::collections::HashSet;

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
        fn hostname_non_empty_accepts(s in non_empty_string()) {
            let hostname = HostName::new(s.clone()).unwrap();
            prop_assert_eq!(hostname.as_str(), &s);
        }

        #[test]
        fn hostname_roundtrip_string(s in non_empty_string()) {
            let hostname = HostName::new(s.clone()).unwrap();
            let as_str: &str = hostname.as_ref();
            prop_assert_eq!(as_str, &s);
        }

        #[test]
        fn hostname_deref_works(s in non_empty_string()) {
            let hostname = HostName::new(s.clone()).unwrap();
            prop_assert_eq!(&*hostname, &s);
            prop_assert_eq!(hostname.len(), s.len());
        }

        #[test]
        fn hostname_display(s in non_empty_string()) {
            let hostname = HostName::new(s.clone()).unwrap();
            prop_assert_eq!(format!("{}", hostname), s);
        }

        #[test]
        fn hostname_try_from(s in non_empty_string()) {
            let result: Result<HostName, _> = s.clone().try_into();
            prop_assert!(result.is_ok());
            let hostname = result.unwrap();
            prop_assert_eq!(hostname.as_str(), &s);
        }

        #[test]
        fn hostname_serde_roundtrip(s in non_empty_string()) {
            let hostname = HostName::new(s).unwrap();
            let json = serde_json::to_string(&hostname).unwrap();
            let deserialized: HostName = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(hostname, deserialized);
        }

        #[test]
        fn hostname_clone_equality(s in non_empty_string()) {
            let hostname = HostName::new(s).unwrap();
            let cloned = hostname.clone();
            prop_assert_eq!(hostname, cloned);
        }
    }

    // Property tests for ServerName
    proptest! {
        #[test]
        fn server_name_non_empty_accepts(s in non_empty_string()) {
            let server = ServerName::new(s.clone()).unwrap();
            prop_assert_eq!(server.as_str(), &s);
        }

        #[test]
        fn server_name_deref_works(s in non_empty_string()) {
            let server = ServerName::new(s.clone()).unwrap();
            prop_assert_eq!(&*server, &s);
        }

        #[test]
        fn server_name_serde_roundtrip(s in non_empty_string()) {
            let server = ServerName::new(s).unwrap();
            let json = serde_json::to_string(&server).unwrap();
            let deserialized: ServerName = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(server, deserialized);
        }
    }

    // Property tests for Username
    proptest! {
        #[test]
        fn username_non_empty_accepts(s in non_empty_string()) {
            let username = Username::new(s.clone()).unwrap();
            prop_assert_eq!(username.as_str(), &s);
        }

        #[test]
        fn username_serde_roundtrip(s in non_empty_string()) {
            let username = Username::new(s).unwrap();
            let json = serde_json::to_string(&username).unwrap();
            let deserialized: Username = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(username, deserialized);
        }
    }

    // Property tests for Password
    proptest! {
        #[test]
        fn password_non_empty_accepts(s in non_empty_string()) {
            let password = Password::new(s.clone()).unwrap();
            prop_assert_eq!(password.as_str(), &s);
        }

        #[test]
        fn password_serde_roundtrip(s in non_empty_string()) {
            let password = Password::new(s).unwrap();
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

    // Edge case tests - empty validation
    #[test]
    fn hostname_empty_rejected() {
        assert!(matches!(
            HostName::new("".to_string()),
            Err(ValidationError::EmptyHostName)
        ));
    }

    #[test]
    fn hostname_whitespace_rejected() {
        assert!(matches!(
            HostName::new("   ".to_string()),
            Err(ValidationError::EmptyHostName)
        ));
    }

    #[test]
    fn server_name_empty_rejected() {
        assert!(matches!(
            ServerName::new("".to_string()),
            Err(ValidationError::EmptyServerName)
        ));
    }

    #[test]
    fn server_name_whitespace_rejected() {
        assert!(matches!(
            ServerName::new("\t".to_string()),
            Err(ValidationError::EmptyServerName)
        ));
    }

    #[test]
    fn username_empty_rejected() {
        assert!(matches!(
            Username::new("".to_string()),
            Err(ValidationError::EmptyUsername)
        ));
    }

    #[test]
    fn username_whitespace_only_rejected() {
        assert!(Username::new("   ".to_string()).is_err());
        assert!(Username::new("\t\n".to_string()).is_err());
    }

    #[test]
    fn username_with_spaces_accepted() {
        // Username with spaces in content is valid (trim checks if empty)
        assert!(Username::new("  user  ".to_string()).is_ok());
        assert!(Username::new("user name".to_string()).is_ok());
    }

    #[test]
    fn username_special_characters_accepted() {
        assert!(Username::new("user@domain.com".to_string()).is_ok());
        assert!(Username::new("user-123".to_string()).is_ok());
        assert!(Username::new("user_name".to_string()).is_ok());
    }

    #[test]
    fn password_empty_rejected() {
        assert!(matches!(
            Password::new("".to_string()),
            Err(ValidationError::EmptyPassword)
        ));
    }

    #[test]
    fn password_whitespace_only_rejected() {
        assert!(Password::new("   ".to_string()).is_err());
    }

    #[test]
    fn password_with_spaces_accepted() {
        assert!(Password::new("   pass   ".to_string()).is_ok());
        assert!(Password::new("P@ssw0rd!".to_string()).is_ok());
        assert!(Password::new("密码123".to_string()).is_ok());
    }

    #[test]
    fn config_path_empty_rejected() {
        assert!(matches!(
            ConfigPath::try_from(""),
            Err(ValidationError::EmptyConfigPath)
        ));
    }

    #[test]
    fn config_path_whitespace_rejected() {
        assert!(ConfigPath::try_from("   ").is_err());
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

    // Hash implementation tests
    #[test]
    fn hostname_hash_works() {
        let mut set = HashSet::new();
        set.insert(HostName::new("example.com".to_string()).unwrap());
        assert!(set.contains(&HostName::new("example.com".to_string()).unwrap()));
    }

    #[test]
    fn server_name_hash_works() {
        let mut set = HashSet::new();
        set.insert(ServerName::new("server1".to_string()).unwrap());
        assert!(set.contains(&ServerName::new("server1".to_string()).unwrap()));
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
