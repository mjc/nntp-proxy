//! Constants used throughout the NNTP proxy
//!
//! This module centralizes magic numbers and configuration values
//! to improve maintainability and reduce duplication.

use std::time::Duration;

/// Buffer size constants
pub mod buffer {
    /// Default buffer size for general I/O operations
    pub const DEFAULT_SIZE: usize = 8192;

    /// Buffer size for reading commands from clients
    pub const COMMAND_SIZE: usize = 512;

    /// Maximum size for a single response (prevents memory exhaustion)
    pub const MAX_RESPONSE_SIZE: usize = 1024 * 1024; // 1MB

    /// Initial capacity for response accumulation buffers
    pub const RESPONSE_INITIAL_CAPACITY: usize = 8192;
}

/// Timeout constants
pub mod timeout {
    use super::Duration;

    /// Timeout for reading responses from backend servers
    pub const BACKEND_READ: Duration = Duration::from_secs(30);

    /// Timeout for executing a command on backend
    pub const COMMAND_EXECUTION: Duration = Duration::from_secs(60);

    /// Connection timeout for backend connections
    pub const CONNECTION: Duration = Duration::from_secs(10);
}

/// NNTP protocol constants
pub mod protocol {
    /// Multiline response terminator: "\r\n.\r\n"
    pub const MULTILINE_TERMINATOR: &[u8] = b"\r\n.\r\n";

    /// Line ending: "\r\n"
    pub const CRLF: &[u8] = b"\r\n";

    /// Authentication required response
    pub const AUTH_REQUIRED: &[u8] = b"381 Password required\r\n";

    /// Authentication accepted response
    pub const AUTH_ACCEPTED: &[u8] = b"281 Authentication accepted\r\n";

    /// Command not supported response
    pub const COMMAND_NOT_SUPPORTED: &[u8] = b"500 Command not supported by this proxy\r\n";

    /// Minimum response length (3-digit code + CRLF)
    pub const MIN_RESPONSE_LENGTH: usize = 5;
}

/// Connection pool constants
pub mod pool {
    /// Default maximum connections per backend pool
    pub const DEFAULT_MAX_CONNECTIONS: usize = 10;

    /// Default minimum idle connections to maintain
    pub const DEFAULT_MIN_IDLE: usize = 2;

    /// Connection pool timeout for getting a connection
    pub const GET_TIMEOUT_SECS: u64 = 5;
}

/// Per-command routing constants
pub mod per_command_routing {
    /// Number of chunks to read ahead when checking for response terminator
    pub const TERMINATOR_LOOKAHEAD_CHUNKS: usize = 4;

    /// Maximum number of bytes to check for spanning terminator
    pub const MAX_TERMINATOR_SPAN_CHECK: usize = 9;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_protocol_constants() {
        assert_eq!(protocol::CRLF, b"\r\n");
        assert_eq!(protocol::MULTILINE_TERMINATOR, b"\r\n.\r\n");
        assert_eq!(protocol::MULTILINE_TERMINATOR.len(), 5);
    }

    #[test]
    fn test_buffer_sizes() {
        // Compile-time assertions
        const _: () = assert!(buffer::DEFAULT_SIZE >= buffer::COMMAND_SIZE);
        const _: () = assert!(buffer::MAX_RESPONSE_SIZE > buffer::DEFAULT_SIZE);
    }

    #[test]
    fn test_timeouts() {
        assert!(timeout::BACKEND_READ.as_secs() > 0);
        assert!(timeout::COMMAND_EXECUTION >= timeout::BACKEND_READ);
    }
}
