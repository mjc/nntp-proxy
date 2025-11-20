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

    #[test]
    fn test_hostname_validation() {
        assert!(HostName::new("example.com".to_string()).is_ok());
        assert!(matches!(
            HostName::new("".to_string()),
            Err(ValidationError::EmptyHostName)
        ));
        assert!(matches!(
            HostName::new("   ".to_string()),
            Err(ValidationError::EmptyHostName)
        ));
    }

    #[test]
    fn test_server_name_validation() {
        assert!(ServerName::new("backend-1".to_string()).is_ok());
        assert!(matches!(
            ServerName::new("".to_string()),
            Err(ValidationError::EmptyServerName)
        ));
        assert!(matches!(
            ServerName::new("\t".to_string()),
            Err(ValidationError::EmptyServerName)
        ));
    }

    #[test]
    fn test_username_validation() {
        assert!(Username::new("alice".to_string()).is_ok());
        assert!(matches!(
            Username::new("".to_string()),
            Err(ValidationError::EmptyUsername)
        ));
    }

    #[test]
    fn test_password_validation() {
        assert!(Password::new("secret".to_string()).is_ok());
        assert!(matches!(
            Password::new("".to_string()),
            Err(ValidationError::EmptyPassword)
        ));
    }

    #[test]
    fn test_config_path_validation() {
        assert!(ConfigPath::try_from("config.toml").is_ok());
        assert!(matches!(
            ConfigPath::try_from(""),
            Err(ValidationError::EmptyConfigPath)
        ));
    }

    // Test actual business logic - custom methods

    #[test]
    fn test_hostname_as_str() {
        let hostname = HostName::new("example.com".to_string()).unwrap();
        assert_eq!(hostname.as_str(), "example.com");
    }

    #[test]
    fn test_config_path_as_path() {
        let config = ConfigPath::try_from("/etc/config.toml").unwrap();
        assert_eq!(config.as_path(), Path::new("/etc/config.toml"));
    }

    #[test]
    fn test_config_path_as_str() {
        let config = ConfigPath::try_from("config.toml").unwrap();
        assert_eq!(config.as_str(), "config.toml");
    }

    // Test edge cases in validation logic

    #[test]
    fn test_username_whitespace_edge_cases() {
        // Only whitespace should be rejected
        assert!(Username::new("   ".to_string()).is_err());
        assert!(Username::new("\t\n".to_string()).is_err());
        // But username with spaces in content is valid (trim checks if empty)
        assert!(Username::new("  user  ".to_string()).is_ok());
        assert!(Username::new("user name".to_string()).is_ok());
    }

    #[test]
    fn test_username_special_characters() {
        assert!(Username::new("user@domain.com".to_string()).is_ok());
        assert!(Username::new("user-123".to_string()).is_ok());
        assert!(Username::new("user_name".to_string()).is_ok());
    }

    #[test]
    fn test_password_edge_cases() {
        assert!(Password::new("P@ssw0rd!".to_string()).is_ok());
        // Whitespace only should be rejected
        assert!(Password::new("   ".to_string()).is_err());
        // Password with content and spaces is valid
        assert!(Password::new("   pass   ".to_string()).is_ok());
        assert!(Password::new("密码123".to_string()).is_ok());
    }

    #[test]
    fn test_config_path_edge_cases() {
        assert!(ConfigPath::try_from("my config.toml").is_ok());
        assert!(ConfigPath::try_from("   ").is_err());
        assert!(ConfigPath::try_from("/absolute/path/config.toml").is_ok());
        assert!(ConfigPath::try_from("./relative/config.toml").is_ok());
        assert!(ConfigPath::try_from("../parent/config.toml").is_ok());
    }

    // Test Display trait implementations
    #[test]
    fn test_hostname_display() {
        let hostname = HostName::new("example.com".to_string()).unwrap();
        assert_eq!(format!("{}", hostname), "example.com");
        assert_eq!(hostname.to_string(), "example.com");
    }

    #[test]
    fn test_server_name_display() {
        let server = ServerName::new("backend-1".to_string()).unwrap();
        assert_eq!(format!("{}", server), "backend-1");
    }

    #[test]
    fn test_username_display() {
        let username = Username::new("alice".to_string()).unwrap();
        assert_eq!(format!("{}", username), "alice");
    }

    #[test]
    fn test_password_display() {
        let password = Password::new("secret123".to_string()).unwrap();
        assert_eq!(format!("{}", password), "secret123");
    }

    #[test]
    fn test_config_path_display() {
        let config = ConfigPath::try_from("config.toml").unwrap();
        assert_eq!(format!("{}", config), "config.toml");
    }

    // Test TryFrom implementations
    #[test]
    fn test_hostname_try_from_string() {
        let result: Result<HostName, _> = "example.com".to_string().try_into();
        assert!(result.is_ok());
        assert_eq!(result.unwrap().as_str(), "example.com");
    }

    #[test]
    fn test_server_name_try_from_string() {
        let result: Result<ServerName, _> = "server1".to_string().try_into();
        assert!(result.is_ok());
    }

    #[test]
    fn test_username_try_from_string() {
        let result: Result<Username, _> = "bob".to_string().try_into();
        assert!(result.is_ok());
    }

    #[test]
    fn test_password_try_from_string() {
        let result: Result<Password, _> = "password".to_string().try_into();
        assert!(result.is_ok());
    }

    #[test]
    fn test_config_path_try_from_str() {
        let result: Result<ConfigPath, _> = "config.toml".try_into();
        assert!(result.is_ok());
    }

    #[test]
    fn test_config_path_try_from_string() {
        let result: Result<ConfigPath, _> = "config.toml".to_string().try_into();
        assert!(result.is_ok());
    }

    // Test FromStr implementation
    #[test]
    fn test_config_path_from_str() {
        use std::str::FromStr;
        let result = ConfigPath::from_str("config.toml");
        assert!(result.is_ok());
        assert_eq!(result.unwrap().as_str(), "config.toml");
    }

    #[test]
    fn test_config_path_from_str_empty() {
        use std::str::FromStr;
        let result = ConfigPath::from_str("");
        assert!(result.is_err());
    }

    // Test AsRef<str> implementation
    #[test]
    fn test_hostname_as_ref_str() {
        let hostname = HostName::new("example.com".to_string()).unwrap();
        let s: &str = hostname.as_ref();
        assert_eq!(s, "example.com");
    }

    #[test]
    fn test_server_name_as_ref() {
        let server = ServerName::new("server1".to_string()).unwrap();
        let s: &str = server.as_ref();
        assert_eq!(s, "server1");
    }

    #[test]
    fn test_username_as_ref() {
        let username = Username::new("alice".to_string()).unwrap();
        let s: &str = username.as_ref();
        assert_eq!(s, "alice");
    }

    #[test]
    fn test_password_as_ref() {
        let password = Password::new("secret".to_string()).unwrap();
        let s: &str = password.as_ref();
        assert_eq!(s, "secret");
    }

    // Test AsRef<Path> implementation
    #[test]
    fn test_config_path_as_ref_path() {
        let config = ConfigPath::try_from("config.toml").unwrap();
        let path: &Path = config.as_ref();
        assert_eq!(path, Path::new("config.toml"));
    }

    // Test Deref implementation
    #[test]
    fn test_hostname_deref() {
        let hostname = HostName::new("example.com".to_string()).unwrap();
        assert_eq!(&*hostname, "example.com");
        assert_eq!(hostname.len(), 11); // Deref allows calling str methods
    }

    #[test]
    fn test_server_name_deref() {
        let server = ServerName::new("backend-1".to_string()).unwrap();
        assert_eq!(&*server, "backend-1");
        assert!(server.contains("backend")); // Deref to str
    }

    #[test]
    fn test_username_deref() {
        let username = Username::new("alice".to_string()).unwrap();
        assert_eq!(&*username, "alice");
    }

    #[test]
    fn test_password_deref() {
        let password = Password::new("secret".to_string()).unwrap();
        assert_eq!(&*password, "secret");
    }

    // Test Clone and PartialEq
    #[test]
    fn test_hostname_clone() {
        let hostname = HostName::new("example.com".to_string()).unwrap();
        let cloned = hostname.clone();
        assert_eq!(hostname, cloned);
    }

    #[test]
    fn test_server_name_clone() {
        let server = ServerName::new("server1".to_string()).unwrap();
        let cloned = server.clone();
        assert_eq!(server, cloned);
    }

    #[test]
    fn test_username_clone() {
        let username = Username::new("alice".to_string()).unwrap();
        let cloned = username.clone();
        assert_eq!(username, cloned);
    }

    #[test]
    fn test_password_clone() {
        let password = Password::new("secret".to_string()).unwrap();
        let cloned = password.clone();
        assert_eq!(password, cloned);
    }

    #[test]
    fn test_config_path_clone() {
        let config = ConfigPath::try_from("config.toml").unwrap();
        let cloned = config.clone();
        assert_eq!(config, cloned);
    }

    // Test Hash implementation (via HashSet usage)
    #[test]
    fn test_hostname_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(HostName::new("example.com".to_string()).unwrap());
        assert!(set.contains(&HostName::new("example.com".to_string()).unwrap()));
    }

    #[test]
    fn test_server_name_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(ServerName::new("server1".to_string()).unwrap());
        assert!(set.contains(&ServerName::new("server1".to_string()).unwrap()));
    }

    // Test Serialize/Deserialize
    #[test]
    fn test_hostname_serialize() {
        let hostname = HostName::new("example.com".to_string()).unwrap();
        let json = serde_json::to_string(&hostname).unwrap();
        assert_eq!(json, "\"example.com\"");
    }

    #[test]
    fn test_hostname_deserialize() {
        let json = "\"example.com\"";
        let hostname: HostName = serde_json::from_str(json).unwrap();
        assert_eq!(hostname.as_str(), "example.com");
    }

    #[test]
    fn test_hostname_deserialize_empty_fails() {
        let json = "\"\"";
        let result: Result<HostName, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_server_name_serialize() {
        let server = ServerName::new("backend-1".to_string()).unwrap();
        let json = serde_json::to_string(&server).unwrap();
        assert_eq!(json, "\"backend-1\"");
    }

    #[test]
    fn test_server_name_deserialize() {
        let json = "\"backend-1\"";
        let server: ServerName = serde_json::from_str(json).unwrap();
        assert_eq!(server.as_str(), "backend-1");
    }

    #[test]
    fn test_username_serialize() {
        let username = Username::new("alice".to_string()).unwrap();
        let json = serde_json::to_string(&username).unwrap();
        assert_eq!(json, "\"alice\"");
    }

    #[test]
    fn test_username_deserialize() {
        let json = "\"alice\"";
        let username: Username = serde_json::from_str(json).unwrap();
        assert_eq!(username.as_str(), "alice");
    }

    #[test]
    fn test_password_serialize() {
        let password = Password::new("secret".to_string()).unwrap();
        let json = serde_json::to_string(&password).unwrap();
        assert_eq!(json, "\"secret\"");
    }

    #[test]
    fn test_password_deserialize() {
        let json = "\"secret\"";
        let password: Password = serde_json::from_str(json).unwrap();
        assert_eq!(password.as_str(), "secret");
    }

    #[test]
    fn test_config_path_serialize() {
        let config = ConfigPath::try_from("config.toml").unwrap();
        let json = serde_json::to_string(&config).unwrap();
        assert_eq!(json, "\"config.toml\"");
    }

    #[test]
    fn test_config_path_deserialize() {
        let json = "\"config.toml\"";
        let config: ConfigPath = serde_json::from_str(json).unwrap();
        assert_eq!(config.as_str(), "config.toml");
    }

    #[test]
    fn test_config_path_deserialize_empty_fails() {
        let json = "\"\"";
        let result: Result<ConfigPath, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    // Test ValidationError types
    #[test]
    fn test_validation_error_display() {
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
    fn test_validation_error_clone() {
        let err = ValidationError::EmptyHostName;
        let cloned = err.clone();
        assert_eq!(err, cloned);
    }
}
