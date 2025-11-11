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

/// Macro to generate validated string newtypes.
///
/// This macro eliminates boilerplate by generating all the standard implementations
/// for validated string types. Each type gets:
/// - A `new()` constructor that validates
/// - `as_str()` getter
/// - `AsRef<str>`, `Deref`, `Display`, `TryFrom<String>` impls
/// - Serde `Serialize` and `Deserialize` with validation
///
/// # Example
///
/// ```ignore
/// validated_string! {
///     /// A validated username
///     pub struct UserName(String) {
///         validation: |s| {
///             if s.trim().is_empty() {
///                 Err(ValidationError::EmptyUserName)
///             } else {
///                 Ok(())
///             }
///         },
///         error_variant: EmptyUserName,
///         error_message: "username cannot be empty",
///     }
/// }
/// ```
macro_rules! validated_string {
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident(String) {
            validation: |$s_param:ident| $validation:expr,
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
        #[serde(transparent)]
        $vis struct $name(String);

        impl $name {
            #[doc = concat!("Create a new ", stringify!($name), " after validation")]
            pub fn new($s_param: String) -> Result<Self, ValidationError> {
                $validation?;
                Ok(Self($s_param))
            }

            #[doc = concat!("Get the ", stringify!($name), " as a string slice")]
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
                write!(f, "{}", self.0)
            }
        }

        impl TryFrom<String> for $name {
            type Error = ValidationError;

            fn try_from($s_param: String) -> Result<Self, Self::Error> {
                Self::new($s_param)
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

// Now use the macro to generate the types

validated_string! {
    /// A validated hostname that cannot be empty or whitespace-only
    ///
    /// This type enforces at compile time that a hostname is always valid,
    /// eliminating the need for runtime validation checks.
    ///
    /// # Examples
    /// ```
    /// use nntp_proxy::types::HostName;
    ///
    /// let host = HostName::new("news.example.com".to_string()).unwrap();
    /// assert_eq!(host.as_str(), "news.example.com");
    ///
    /// // Empty strings are rejected
    /// assert!(HostName::new("".to_string()).is_err());
    /// assert!(HostName::new("   ".to_string()).is_err());
    /// ```
    #[doc(alias = "host")]
    #[doc(alias = "domain")]
    pub struct HostName(String) {
        validation: |s| {
            if s.trim().is_empty() {
                Err(ValidationError::EmptyHostName)
            } else {
                Ok(())
            }
        },
    }
}

validated_string! {
    /// A validated server name that cannot be empty or whitespace-only
    pub struct ServerName(String) {
        validation: |s| {
            if s.trim().is_empty() {
                Err(ValidationError::EmptyServerName)
            } else {
                Ok(())
            }
        },
    }
}

validated_string! {
    /// A validated username for authentication that cannot be empty or whitespace-only
    ///
    /// This type enforces that usernames are always non-empty, preventing
    /// authentication bypass vulnerabilities from empty credentials.
    ///
    /// # Examples
    /// ```
    /// use nntp_proxy::types::Username;
    ///
    /// let user = Username::new("alice".to_string()).unwrap();
    /// assert_eq!(user.as_str(), "alice");
    ///
    /// // Empty strings are rejected
    /// assert!(Username::new("".to_string()).is_err());
    /// assert!(Username::new("   ".to_string()).is_err());
    /// ```
    pub struct Username(String) {
        validation: |s| {
            if s.trim().is_empty() {
                Err(ValidationError::EmptyUsername)
            } else {
                Ok(())
            }
        },
    }
}

validated_string! {
    /// A validated password for authentication that cannot be empty or whitespace-only
    ///
    /// This type enforces that passwords are always non-empty, preventing
    /// authentication bypass vulnerabilities from empty credentials.
    ///
    /// # Examples
    /// ```
    /// use nntp_proxy::types::Password;
    ///
    /// let pass = Password::new("secret123".to_string()).unwrap();
    /// assert_eq!(pass.as_str(), "secret123");
    ///
    /// // Empty strings are rejected
    /// assert!(Password::new("".to_string()).is_err());
    /// assert!(Password::new("   ".to_string()).is_err());
    /// ```
    pub struct Password(String) {
        validation: |s| {
            if s.trim().is_empty() {
                Err(ValidationError::EmptyPassword)
            } else {
                Ok(())
            }
        },
    }
}

/// A validated configuration file path
///
/// This type ensures that the path is not empty or whitespace-only.
/// It does not validate that the file exists, as that is a runtime concern.
///
/// # Examples
/// ```
/// use nntp_proxy::types::ConfigPath;
///
/// let path = ConfigPath::new("config.toml").unwrap();
/// assert_eq!(path.as_str(), "config.toml");
///
/// // Empty paths are rejected
/// assert!(ConfigPath::new("").is_err());
/// assert!(ConfigPath::new("   ").is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConfigPath(PathBuf);

impl ConfigPath {
    /// Create a new ConfigPath from a string-like value
    pub fn new(path: impl AsRef<Path>) -> Result<Self, ValidationError> {
        let path_ref = path.as_ref();
        let path_str = path_ref.to_str().ok_or(ValidationError::EmptyConfigPath)?;

        if path_str.trim().is_empty() {
            return Err(ValidationError::EmptyConfigPath);
        }

        Ok(Self(path_ref.to_path_buf()))
    }

    /// Get the path as a &Path
    #[must_use]
    #[inline]
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// Get the path as a string slice
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

    // Test the macro-generated validation - only need one test per type for the core logic
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
