//! Connection error types for the NNTP proxy

use thiserror::Error;

/// Errors that can occur during connection management
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConnectionError {
    #[error("Failed to connect to {host}:{port}: {source}")]
    TcpConnect {
        host: String,
        port: u16,
        #[source]
        source: std::io::Error,
    },

    #[error("Failed to resolve DNS for {address}: {source}")]
    DnsResolution {
        address: String,
        #[source]
        source: std::io::Error,
    },

    #[error("Failed to configure socket ({operation}): {source}")]
    SocketConfig {
        operation: String,
        #[source]
        source: std::io::Error,
    },

    #[error("Authentication failed for backend '{backend}': {response}")]
    AuthenticationFailed { backend: String, response: String },

    #[error("Invalid greeting from backend '{backend}': {greeting}")]
    InvalidGreeting { backend: String, greeting: String },

    #[error("Connection pool exhausted for backend '{backend}' (max size: {max_size})")]
    PoolExhausted { backend: String, max_size: usize },

    #[error("Stale connection to backend '{backend}': {reason}")]
    StaleConnection { backend: String, reason: String },

    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("TLS handshake failed for backend '{backend}': {source}")]
    TlsHandshake {
        backend: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("Certificate verification failed for backend '{backend}': {reason}")]
    CertificateVerification { backend: String, reason: String },
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
}

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
}
