//! Connection error types for the NNTP proxy
//!
//! This module provides detailed error types for connection management,
//! making it easier to diagnose and handle different failure scenarios.

use std::fmt;

/// Errors that can occur during connection management
#[derive(Debug)]
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
    // Future: TLS/SSL errors
    // TlsHandshake { backend: String, source: ... },
    // CertificateVerification { backend: String, reason: String },
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
            _ => None,
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
}
