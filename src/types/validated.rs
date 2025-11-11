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
}
