//! Connection error types for the NNTP proxy
//!
//! This module provides detailed error types for connection management,
//! making it easier to diagnose and handle different failure scenarios.

use std::fmt;

/// Errors that can occur during connection management
#[derive(Debug)]
#[non_exhaustive]
pub enum ConnectionError {
    /// TCP connection failed
    TcpConnect {
        host: String,
        port: u16,
        source: std::io::Error,
    },

    /// DNS resolution failed
    DnsResolution {
        address: String,
        source: std::io::Error,
    },

    /// Socket configuration failed (buffer sizes, keepalive, etc.)
    SocketConfig {
        operation: String,
        source: std::io::Error,
    },

    /// Backend authentication failed
    AuthenticationFailed { backend: String, response: String },

    /// Invalid or unexpected greeting from backend
    InvalidGreeting { backend: String, greeting: String },

    /// Connection pool exhausted
    PoolExhausted { backend: String, max_size: usize },

    /// Connection is stale or broken
    StaleConnection { backend: String, reason: String },

    /// I/O error during communication
    IoError(std::io::Error),

    /// TLS handshake failed
    TlsHandshake {
        backend: String,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// Certificate verification failed
    CertificateVerification { backend: String, reason: String },
}

impl fmt::Display for ConnectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TcpConnect { host, port, source } => {
                write!(f, "Failed to connect to {}:{}: {}", host, port, source)
            }
            Self::DnsResolution { address, source } => {
                write!(f, "Failed to resolve DNS for {}: {}", address, source)
            }
            Self::SocketConfig { operation, source } => {
                write!(f, "Failed to configure socket ({}): {}", operation, source)
            }
            Self::AuthenticationFailed { backend, response } => {
                write!(
                    f,
                    "Authentication failed for backend '{}': {}",
                    backend, response
                )
            }
            Self::InvalidGreeting { backend, greeting } => {
                write!(
                    f,
                    "Invalid greeting from backend '{}': {}",
                    backend, greeting
                )
            }
            Self::PoolExhausted { backend, max_size } => {
                write!(
                    f,
                    "Connection pool exhausted for backend '{}' (max size: {})",
                    backend, max_size
                )
            }
            Self::StaleConnection { backend, reason } => {
                write!(f, "Stale connection to backend '{}': {}", backend, reason)
            }
            Self::IoError(e) => write!(f, "I/O error: {}", e),
            Self::TlsHandshake { backend, source } => {
                write!(
                    f,
                    "TLS handshake failed for backend '{}': {}",
                    backend, source
                )
            }
            Self::CertificateVerification { backend, reason } => {
                write!(
                    f,
                    "Certificate verification failed for backend '{}': {}",
                    backend, reason
                )
            }
        }
    }
}

impl std::error::Error for ConnectionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TcpConnect { source, .. } => Some(source),
            Self::DnsResolution { source, .. } => Some(source),
            Self::SocketConfig { source, .. } => Some(source),
            Self::IoError(e) => Some(e),
            Self::TlsHandshake { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

impl ConnectionError {
    /// Check if this is a client disconnection (broken pipe)
    #[must_use]
    pub fn is_client_disconnect(&self) -> bool {
        matches!(self, Self::IoError(e) if e.kind() == std::io::ErrorKind::BrokenPipe)
    }

    /// Check if this is an authentication error
    #[must_use]
    pub const fn is_authentication_error(&self) -> bool {
        matches!(self, Self::AuthenticationFailed { .. })
    }

    /// Check if this is a network connectivity error
    #[must_use]
    pub const fn is_network_error(&self) -> bool {
        matches!(self, Self::TcpConnect { .. } | Self::DnsResolution { .. })
    }

    /// Get the appropriate log level for this error
    #[must_use]
    pub fn log_level(&self) -> tracing::Level {
        match self {
            // Client disconnects (broken pipe) are normal
            Self::IoError(e) if e.kind() == std::io::ErrorKind::BrokenPipe => tracing::Level::DEBUG,
            // Other IO errors are warnings
            Self::IoError(_) => tracing::Level::WARN,
            // Authentication and configuration errors need attention
            Self::AuthenticationFailed { .. }
            | Self::InvalidGreeting { .. }
            | Self::CertificateVerification { .. } => tracing::Level::ERROR,
            // Network errors might be transient
            Self::TcpConnect { .. } | Self::DnsResolution { .. } => tracing::Level::WARN,
            // Everything else is a warning
            _ => tracing::Level::WARN,
        }
    }
}

impl From<std::io::Error> for ConnectionError {
    fn from(err: std::io::Error) -> Self {
        Self::IoError(err)
    }
}

// Note: No need for From<ConnectionError> for anyhow::Error
// anyhow has a blanket impl for all types implementing std::error::Error

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn test_tcp_connect_error() {
        let err = ConnectionError::TcpConnect {
            host: "example.com".to_string(),
            port: 119,
            source: std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"),
        };

        let msg = err.to_string();
        assert!(msg.contains("example.com"));
        assert!(msg.contains("119"));
        assert!(msg.contains("refused"));
    }

    #[test]
    fn test_authentication_failed_error() {
        let err = ConnectionError::AuthenticationFailed {
            backend: "news.example.com".to_string(),
            response: "502 Authentication failed".to_string(),
        };

        let msg = err.to_string();
        assert!(msg.contains("news.example.com"));
        assert!(msg.contains("502"));
    }

    #[test]
    fn test_pool_exhausted_error() {
        let err = ConnectionError::PoolExhausted {
            backend: "backend1".to_string(),
            max_size: 20,
        };

        let msg = err.to_string();
        assert!(msg.contains("backend1"));
        assert!(msg.contains("20"));
    }

    #[test]
    fn test_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout");
        let conn_err: ConnectionError = io_err.into();

        assert!(matches!(conn_err, ConnectionError::IoError(_)));
    }

    #[test]
    fn test_error_source() {
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        let err = ConnectionError::TcpConnect {
            host: "test.com".to_string(),
            port: 119,
            source: io_err,
        };

        assert!(err.source().is_some());
    }

    #[test]
    fn test_invalid_greeting_error() {
        let err = ConnectionError::InvalidGreeting {
            backend: "news.server.com".to_string(),
            greeting: "500 Server error".to_string(),
        };

        let msg = err.to_string();
        assert!(msg.contains("Invalid greeting"));
        assert!(msg.contains("news.server.com"));
    }

    #[test]
    fn test_stale_connection_error() {
        let err = ConnectionError::StaleConnection {
            backend: "backend2".to_string(),
            reason: "Connection closed by peer".to_string(),
        };

        let msg = err.to_string();
        assert!(msg.contains("Stale"));
        assert!(msg.contains("backend2"));
    }

    #[test]
    fn test_is_client_disconnect() {
        let io_err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broken pipe");
        let err = ConnectionError::IoError(io_err);
        assert!(err.is_client_disconnect());

        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        let err = ConnectionError::IoError(io_err);
        assert!(!err.is_client_disconnect());
    }

    #[test]
    fn test_is_authentication_error() {
        let err = ConnectionError::AuthenticationFailed {
            backend: "test".to_string(),
            response: "failed".to_string(),
        };
        assert!(err.is_authentication_error());

        let err = ConnectionError::IoError(std::io::Error::other("test"));
        assert!(!err.is_authentication_error());
    }

    #[test]
    fn test_is_network_error() {
        let err = ConnectionError::TcpConnect {
            host: "test.com".to_string(),
            port: 119,
            source: std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"),
        };
        assert!(err.is_network_error());

        let err = ConnectionError::DnsResolution {
            address: "test.com".to_string(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "not found"),
        };
        assert!(err.is_network_error());

        let err = ConnectionError::AuthenticationFailed {
            backend: "test".to_string(),
            response: "failed".to_string(),
        };
        assert!(!err.is_network_error());
    }

    #[test]
    fn test_log_level() {
        let io_err = ConnectionError::IoError(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "broken",
        ));
        assert_eq!(io_err.log_level(), tracing::Level::DEBUG);

        let auth_err = ConnectionError::AuthenticationFailed {
            backend: "test".to_string(),
            response: "failed".to_string(),
        };
        assert_eq!(auth_err.log_level(), tracing::Level::ERROR);

        let tcp_err = ConnectionError::TcpConnect {
            host: "test.com".to_string(),
            port: 119,
            source: std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"),
        };
        assert_eq!(tcp_err.log_level(), tracing::Level::WARN);
    }
}
