//! Constants used throughout the NNTP proxy
//!
//! This module centralizes magic numbers and configuration values
//! to improve maintainability and reduce duplication.

use std::time::Duration;

/// Buffer size constants
///
/// All buffer sizes are carefully chosen for NNTP workloads:
/// - Commands are small (< 512 bytes)
/// - Articles range from 1KB to 100KB typically
/// - Pooled buffers (256KB) handle most articles in one read
pub mod buffer {
    // Buffer pool configuration
    
    /// Size of each pooled buffer (256KB)
    /// Large enough to handle most Usenet articles in a single read
    pub const POOL: usize = 256 * 1024;
    
    /// Number of buffers in the buffer pool
    /// Sized for ~32 concurrent connections with one buffer each
    pub const POOL_COUNT: usize = 32;
    
    // Command and response limits
    
    /// Maximum command line size (512 bytes)
    /// NNTP commands are typically small: "ARTICLE <msgid@example.com>"
    pub const COMMAND: usize = 512;
    
    /// Maximum size for a single response (1MB)
    /// Prevents memory exhaustion from malicious/malformed responses
    pub const RESPONSE_MAX: usize = 1024 * 1024;
    
    /// Initial capacity for response accumulation buffers (8KB)
    /// Sized for typical status lines and small responses
    pub const RESPONSE_INITIAL: usize = 8192;
    
    // Streaming configuration
    
    /// Chunk size for streaming responses (64KB)
    /// Balance between latency and throughput
    pub const STREAM_CHUNK: usize = 65536;
}

/// Socket buffer size constants
pub mod socket {
    /// TCP socket receive buffer size for high-throughput transfers (16MB)
    pub const HIGH_THROUGHPUT_RECV_BUFFER: usize = 16 * 1024 * 1024;

    /// TCP socket send buffer size for high-throughput transfers (16MB)
    pub const HIGH_THROUGHPUT_SEND_BUFFER: usize = 16 * 1024 * 1024;

    /// TCP socket receive buffer size for connection pools (4MB)
    /// Smaller than high-throughput to avoid memory exhaustion with many connections
    pub const POOL_RECV_BUFFER: usize = 4 * 1024 * 1024;

    /// TCP socket send buffer size for connection pools (4MB)
    pub const POOL_SEND_BUFFER: usize = 4 * 1024 * 1024;
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

/// Connection pool constants
pub mod pool {
    use super::Duration;

    /// Default maximum connections per backend pool
    pub const DEFAULT_MAX_CONNECTIONS: usize = 10;

    /// Default minimum idle connections to maintain
    pub const DEFAULT_MIN_IDLE: usize = 2;

    /// Connection pool timeout for getting a connection
    pub const GET_TIMEOUT_SECS: u64 = 5;

    /// Buffer size for TCP peek during health checks
    /// Only 1 byte needed to detect if connection is readable/closed
    pub const TCP_PEEK_BUFFER_SIZE: usize = 1;

    /// Health check timeout - how long to wait for DATE command response
    pub const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(2);

    /// Buffer size for reading health check responses
    pub const HEALTH_CHECK_BUFFER_SIZE: usize = 512;

    /// DATE command bytes sent during health check
    pub const DATE_COMMAND: &[u8] = b"DATE\r\n";

    /// Expected prefix of DATE command response (NNTP 111 response code)
    pub const EXPECTED_DATE_RESPONSE_PREFIX: &str = "111 ";

    /// Minimum recommended keep-alive interval in seconds
    /// Values below this may cause excessive health check traffic
    pub const MIN_RECOMMENDED_KEEPALIVE_SECS: u64 = 30;

    /// Maximum recommended keep-alive interval in seconds (5 minutes)
    /// Values above this may not detect stale connections quickly enough
    pub const MAX_RECOMMENDED_KEEPALIVE_SECS: u64 = 300;

    /// Maximum number of idle connections to check per health check cycle
    /// Checking too many at once can temporarily starve the pool
    pub const MAX_CONNECTIONS_PER_HEALTH_CHECK_CYCLE: usize = 5;

    /// Timeout when attempting to get a connection for health checking (milliseconds)
    /// Short timeout to avoid blocking if pool is busy
    pub const HEALTH_CHECK_POOL_TIMEOUT_MS: u64 = 100;
}

/// Per-command routing constants
pub mod per_command_routing {
    /// Number of chunks to read ahead when checking for response terminator
    pub const TERMINATOR_LOOKAHEAD_CHUNKS: usize = 4;

    /// Maximum number of bytes to check for spanning terminator
    pub const MAX_TERMINATOR_SPAN_CHECK: usize = 9;
}

/// Stateless proxy protocol constants
pub mod stateless_proxy {
    pub const NNTP_BACKEND_UNAVAILABLE: &[u8] = b"400 Backend server unavailable\r\n";
    pub const NNTP_COMMAND_NOT_SUPPORTED: &[u8] =
        b"500 Command not supported by this proxy (stateless proxy mode)\r\n";

    /// Prewarming configuration constants
    pub const PREWARMING_BATCH_SIZE: usize = 5; // Create connections in batches of 5
    pub const BATCH_DELAY_MS: u64 = 100; // Wait 100ms between prewarming batches
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_buffer_sizes() {
        // Compile-time assertions for buffer size relationships
        const _: () = assert!(buffer::RESPONSE_INITIAL >= buffer::COMMAND);
        const _: () = assert!(buffer::RESPONSE_MAX > buffer::RESPONSE_INITIAL);
        const _: () = assert!(buffer::POOL >= buffer::STREAM_CHUNK);
    }

    #[test]
    fn test_timeouts() {
        assert!(timeout::BACKEND_READ.as_secs() > 0);
        assert!(timeout::COMMAND_EXECUTION >= timeout::BACKEND_READ);
    }
}
