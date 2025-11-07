//! Validated string types that enforce invariants at construction time

use serde::{Deserialize, Serialize};
use std::fmt;
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
            error_variant: $error_variant:ident,
            error_message: $error_msg:literal,
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
        #[serde(transparent)]
        $vis struct $name(String);

        impl $name {
            #[doc = concat!("Create a new ", stringify!($name), " after validation")]
            pub fn new($s_param: String) -> Result<Self, ValidationError> {
                let validate = || $validation;
                validate()?;
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
        error_variant: EmptyHostName,
        error_message: "hostname cannot be empty or whitespace",
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
        error_variant: EmptyServerName,
        error_message: "server name cannot be empty or whitespace",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // HostName tests
    #[test]
    fn test_hostname_valid() {
        let host = HostName::new("example.com".to_string()).unwrap();
        assert_eq!(host.as_str(), "example.com");
    }

    #[test]
    fn test_hostname_valid_ip() {
        let host = HostName::new("192.168.1.1".to_string()).unwrap();
        assert_eq!(host.as_str(), "192.168.1.1");
    }

    #[test]
    fn test_hostname_valid_localhost() {
        let host = HostName::new("localhost".to_string()).unwrap();
        assert_eq!(host.as_str(), "localhost");
    }

    #[test]
    fn test_hostname_valid_with_subdomain() {
        let host = HostName::new("news.example.com".to_string()).unwrap();
        assert_eq!(host.as_str(), "news.example.com");
    }

    #[test]
    fn test_hostname_valid_with_port_notation() {
        // HostName only validates non-empty, does not parse or validate port notation.
        // In production, host and port are stored separately (HostName + Port types).
        // This test verifies the type doesn't reject strings with colons.
        let host = HostName::new("example.com:119".to_string()).unwrap();
        assert_eq!(host.as_str(), "example.com:119");
    }

    #[test]
    fn test_hostname_empty_rejected() {
        let result = HostName::new("".to_string());
        assert!(matches!(result, Err(ValidationError::EmptyHostName)));
    }

    #[test]
    fn test_hostname_whitespace_rejected() {
        let result = HostName::new("   ".to_string());
        assert!(matches!(result, Err(ValidationError::EmptyHostName)));
    }

    #[test]
    fn test_hostname_tabs_rejected() {
        let result = HostName::new("\t\t".to_string());
        assert!(matches!(result, Err(ValidationError::EmptyHostName)));
    }

    #[test]
    fn test_hostname_newlines_rejected() {
        let result = HostName::new("\n\n".to_string());
        assert!(matches!(result, Err(ValidationError::EmptyHostName)));
    }

    #[test]
    fn test_hostname_mixed_whitespace_rejected() {
        let result = HostName::new(" \t\n ".to_string());
        assert!(matches!(result, Err(ValidationError::EmptyHostName)));
    }

    #[test]
    fn test_hostname_display() {
        let host = HostName::new("example.com".to_string()).unwrap();
        assert_eq!(format!("{}", host), "example.com");
    }

    #[test]
    fn test_hostname_as_ref() {
        let host = HostName::new("example.com".to_string()).unwrap();
        let s: &str = host.as_ref();
        assert_eq!(s, "example.com");
    }

    #[test]
    fn test_hostname_try_from() {
        let result: Result<HostName, _> = "example.com".to_string().try_into();
        assert!(result.is_ok());
        assert_eq!(result.unwrap().as_str(), "example.com");
    }

    #[test]
    fn test_hostname_try_from_empty() {
        let result: Result<HostName, _> = "".to_string().try_into();
        assert!(result.is_err());
    }

    #[test]
    fn test_hostname_clone() {
        let host1 = HostName::new("example.com".to_string()).unwrap();
        let host2 = host1.clone();
        assert_eq!(host1, host2);
    }

    #[test]
    fn test_hostname_equality() {
        let host1 = HostName::new("example.com".to_string()).unwrap();
        let host2 = HostName::new("example.com".to_string()).unwrap();
        let host3 = HostName::new("other.com".to_string()).unwrap();
        assert_eq!(host1, host2);
        assert_ne!(host1, host3);
    }

    #[test]
    fn test_hostname_serde() {
        let host = HostName::new("test.com".to_string()).unwrap();
        let json = serde_json::to_string(&host).unwrap();
        assert_eq!(json, "\"test.com\"");

        let deserialized: HostName = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, host);
    }

    #[test]
    fn test_hostname_serde_invalid() {
        let json = "\"\"";
        let result: Result<HostName, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_hostname_serde_whitespace_rejected() {
        let json = "\"   \"";
        let result: Result<HostName, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    // ServerName tests
    #[test]
    fn test_server_name_valid() {
        let name = ServerName::new("backend-1".to_string()).unwrap();
        assert_eq!(name.as_str(), "backend-1");
    }

    #[test]
    fn test_server_name_valid_simple() {
        let name = ServerName::new("server1".to_string()).unwrap();
        assert_eq!(name.as_str(), "server1");
    }

    #[test]
    fn test_server_name_valid_descriptive() {
        let name = ServerName::new("Primary News Server".to_string()).unwrap();
        assert_eq!(name.as_str(), "Primary News Server");
    }

    #[test]
    fn test_server_name_valid_with_symbols() {
        let name = ServerName::new("server_1-prod".to_string()).unwrap();
        assert_eq!(name.as_str(), "server_1-prod");
    }

    #[test]
    fn test_server_name_empty_rejected() {
        let result = ServerName::new("".to_string());
        assert!(matches!(result, Err(ValidationError::EmptyServerName)));
    }

    #[test]
    fn test_server_name_whitespace_rejected() {
        let result = ServerName::new("   ".to_string());
        assert!(matches!(result, Err(ValidationError::EmptyServerName)));
    }

    #[test]
    fn test_server_name_tabs_rejected() {
        let result = ServerName::new("\t".to_string());
        assert!(matches!(result, Err(ValidationError::EmptyServerName)));
    }

    #[test]
    fn test_server_name_display() {
        let name = ServerName::new("backend-1".to_string()).unwrap();
        assert_eq!(format!("{}", name), "backend-1");
    }

    #[test]
    fn test_server_name_as_ref() {
        let name = ServerName::new("backend-1".to_string()).unwrap();
        let s: &str = name.as_ref();
        assert_eq!(s, "backend-1");
    }

    #[test]
    fn test_server_name_try_from() {
        let result: Result<ServerName, _> = "backend-1".to_string().try_into();
        assert!(result.is_ok());
        assert_eq!(result.unwrap().as_str(), "backend-1");
    }

    #[test]
    fn test_server_name_try_from_empty() {
        let result: Result<ServerName, _> = "".to_string().try_into();
        assert!(result.is_err());
    }

    #[test]
    fn test_server_name_clone() {
        let name1 = ServerName::new("backend-1".to_string()).unwrap();
        let name2 = name1.clone();
        assert_eq!(name1, name2);
    }

    #[test]
    fn test_server_name_equality() {
        let name1 = ServerName::new("backend-1".to_string()).unwrap();
        let name2 = ServerName::new("backend-1".to_string()).unwrap();
        let name3 = ServerName::new("backend-2".to_string()).unwrap();
        assert_eq!(name1, name2);
        assert_ne!(name1, name3);
    }

    #[test]
    fn test_server_name_serde() {
        let name = ServerName::new("backend-1".to_string()).unwrap();
        let json = serde_json::to_string(&name).unwrap();
        assert_eq!(json, "\"backend-1\"");

        let deserialized: ServerName = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, name);
    }

    #[test]
    fn test_server_name_serde_invalid() {
        let json = "\"\"";
        let result: Result<ServerName, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    // ValidationError tests
    #[test]
    fn test_validation_error_display_hostname() {
        let error = ValidationError::EmptyHostName;
        assert_eq!(
            format!("{}", error),
            "hostname cannot be empty or whitespace"
        );
    }

    #[test]
    fn test_validation_error_display_servername() {
        let error = ValidationError::EmptyServerName;
        assert_eq!(
            format!("{}", error),
            "server name cannot be empty or whitespace"
        );
    }

    #[test]
    fn test_validation_error_display_invalid_hostname() {
        let error = ValidationError::InvalidHostName("bad-host".to_string());
        assert!(format!("{}", error).contains("bad-host"));
    }

    #[test]
    fn test_validation_error_equality() {
        let error1 = ValidationError::EmptyHostName;
        let error2 = ValidationError::EmptyHostName;
        let error3 = ValidationError::EmptyServerName;
        assert_eq!(error1, error2);
        assert_ne!(error1, error3);
    }

    #[test]
    fn test_validation_error_clone() {
        let error1 = ValidationError::EmptyHostName;
        let error2 = error1.clone();
        assert_eq!(error1, error2);
    }

    // Integration tests
    #[test]
    fn test_hostname_and_servername_different_types() {
        let host = HostName::new("example.com".to_string()).unwrap();
        let name = ServerName::new("example.com".to_string()).unwrap();
        // They have the same string value but are different types
        assert_eq!(host.as_str(), name.as_str());
    }

    #[test]
    fn test_multiple_validations() {
        // Ensure multiple validations work independently
        let host1 = HostName::new("host1.com".to_string()).unwrap();
        let host2 = HostName::new("host2.com".to_string()).unwrap();
        let name1 = ServerName::new("server1".to_string()).unwrap();
        let name2 = ServerName::new("server2".to_string()).unwrap();

        assert_ne!(host1, host2);
        assert_ne!(name1, name2);
    }

    #[test]
    fn test_serde_roundtrip_hostname() {
        let original = HostName::new("test.example.com".to_string()).unwrap();
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: HostName = serde_json::from_str(&json).unwrap();
        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_serde_roundtrip_servername() {
        let original = ServerName::new("production-server-01".to_string()).unwrap();
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: ServerName = serde_json::from_str(&json).unwrap();
        assert_eq!(original, deserialized);
    }
}
