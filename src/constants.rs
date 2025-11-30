//! Constants used throughout the NNTP proxy
//!
//! This module centralizes magic numbers and configuration values
//! to improve maintainability and reduce duplication.

use std::time::Duration;

/// Buffer size constants
///
/// All buffer sizes are carefully chosen for NNTP workloads:
/// - Commands are small (< 512 bytes)
/// - Articles range from 1KB to 768KB typically (99th percentile)
/// - Pooled buffers use aligned sizes for optimal memory access
pub mod buffer {
    // Buffer pool configuration

    /// Page size for memory alignment (4KB = standard OS page)
    #[allow(dead_code)]
    const PAGE_SIZE: usize = 4096;

    /// Size of each pooled buffer (724KB, page-aligned)
    /// Tuned for typical Usenet article sizes (average ~725KB)
    /// Aligned to 4KB page boundaries for optimal memory access
    pub const POOL: usize = 724 * 1024;

    /// Number of buffers in the buffer pool
    /// Sized for ~25 concurrent connections with one buffer each
    /// Total memory: 25 × 724KB ≈ 18MB
    pub const POOL_COUNT: usize = 25;

    /// BufReader capacity for client command parsing (64KB)
    /// Large enough to handle any NNTP command line without multiple reads
    /// Sized for efficient line-based reading with minimal syscalls
    pub const READER_CAPACITY: usize = 64 * 1024;

    // Command and response limits

    /// Maximum command line size (512 bytes)
    /// NNTP commands are typically small: "ARTICLE <msgid@example.com>"
    pub const COMMAND: usize = 512;

    /// Maximum size for a single response (2MB)
    /// Increased to handle larger articles, prevents memory exhaustion
    pub const RESPONSE_MAX: usize = 2 * 1024 * 1024;

    /// Initial capacity for response accumulation buffers (8KB, page-aligned)
    /// Sized for typical status lines and small responses
    pub const RESPONSE_INITIAL: usize = 8192;

    // Streaming configuration

    /// Chunk size for streaming responses (724KB, page-aligned)
    /// Matches POOL size to handle most articles in a single read
    /// Aligned to 4KB page boundaries for optimal zero-copy I/O
    pub const STREAM_CHUNK: usize = 724 * 1024;

    // Compile-time validation

    /// Verify pool buffer is page-aligned at compile time
    const _POOL_ALIGNED: () = assert!(POOL.is_multiple_of(PAGE_SIZE), "POOL must be page-aligned");

    /// Verify stream chunk is page-aligned at compile time
    const _CHUNK_ALIGNED: () = assert!(
        STREAM_CHUNK.is_multiple_of(PAGE_SIZE),
        "STREAM_CHUNK must be page-aligned"
    );

    /// Verify pool and chunk sizes match
    const _SIZES_MATCH: () = assert!(
        POOL == STREAM_CHUNK,
        "POOL and STREAM_CHUNK should match for single-read optimization"
    );
}

/// Socket buffer size constants
pub mod socket {
    /// TCP socket receive buffer size for high-throughput transfers (16MB)
    pub const HIGH_THROUGHPUT_RECV_BUFFER: usize = 16 * 1024 * 1024;

    /// TCP socket send buffer size for high-throughput transfers (16MB)
    pub const HIGH_THROUGHPUT_SEND_BUFFER: usize = 16 * 1024 * 1024;

    /// TCP socket receive buffer size for connection pools (7.25MB)
    /// Sized to handle 724KB streaming chunks efficiently (10x chunk size)
    pub const POOL_RECV_BUFFER: usize = 7 * 1024 * 1024 + 256 * 1024;

    /// TCP socket send buffer size for connection pools (7.25MB)
    /// Sized to handle 724KB streaming chunks efficiently (10x chunk size)
    pub const POOL_SEND_BUFFER: usize = 7 * 1024 * 1024 + 256 * 1024;
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

/// Display strings for user metrics and logging
pub mod user {
    /// Display name for anonymous/unauthenticated users
    ///
    /// Used as HashMap key and display value for users who haven't authenticated.
    /// The `<anonymous>` format is chosen to:
    /// - Sort first in alphabetical listings (< comes before letters)
    /// - Be clearly distinguished from actual usernames
    /// - Be consistent across all metrics and logging
    pub const ANONYMOUS: &str = "<anonymous>";
}

#[cfg(test)]
#[allow(clippy::assertions_on_constants)]
mod tests {
    use super::*;

    #[test]
    fn test_buffer_alignment() {
        // Pool buffer should be page-aligned (4KB boundaries)
        assert_eq!(buffer::POOL % 4096, 0, "POOL must be page-aligned");

        // Stream chunk should be page-aligned
        assert_eq!(
            buffer::STREAM_CHUNK % 4096,
            0,
            "STREAM_CHUNK must be page-aligned"
        );

        // Pool and chunk should match for single-read optimization
        assert_eq!(
            buffer::POOL,
            buffer::STREAM_CHUNK,
            "POOL and STREAM_CHUNK should match"
        );
    }

    #[test]
    fn test_buffer_sizes() {
        // Pool buffer should be 724KB (optimal for 725KB average articles)
        assert_eq!(buffer::POOL, 724 * 1024);

        // Reader capacity should be large enough for any command
        assert!(buffer::READER_CAPACITY >= buffer::COMMAND);
        assert_eq!(buffer::READER_CAPACITY, 64 * 1024);

        // Response max should be larger than pool for safety margin
        assert!(buffer::RESPONSE_MAX > buffer::POOL);

        // Basic size relationships
        assert!(buffer::RESPONSE_INITIAL >= buffer::COMMAND);
        assert!(buffer::RESPONSE_MAX > buffer::RESPONSE_INITIAL);
        assert!(buffer::POOL >= buffer::STREAM_CHUNK);
    }

    #[test]
    fn test_socket_buffer_ratios() {
        // Socket buffers should be ~10x the streaming chunk size for optimal throughput
        let expected_min_buffer = buffer::STREAM_CHUNK * 10;
        assert!(socket::POOL_RECV_BUFFER >= expected_min_buffer);
        assert!(socket::POOL_SEND_BUFFER >= expected_min_buffer);

        // High-throughput buffers should be larger than pool buffers
        assert!(socket::HIGH_THROUGHPUT_RECV_BUFFER > socket::POOL_RECV_BUFFER);
        assert!(socket::HIGH_THROUGHPUT_SEND_BUFFER > socket::POOL_SEND_BUFFER);

        // Buffers should be symmetric (send == receive)
        assert_eq!(
            socket::HIGH_THROUGHPUT_RECV_BUFFER,
            socket::HIGH_THROUGHPUT_SEND_BUFFER
        );
    }

    #[test]
    fn test_pool_memory_footprint() {
        // Calculate total pool memory
        let total_memory = buffer::POOL * buffer::POOL_COUNT;

        // Should be approximately 18MB
        let expected_mb = 18;
        let actual_mb = total_memory / (1024 * 1024);

        assert!(
            actual_mb >= expected_mb - 1 && actual_mb <= expected_mb + 1,
            "Pool memory should be ~{}MB, got {}MB",
            expected_mb,
            actual_mb
        );
    }

    #[test]
    fn test_timeouts() {
        // Command execution timeout should be longer than backend read timeout
        assert!(timeout::COMMAND_EXECUTION > timeout::BACKEND_READ);

        // Backend read timeout should be longer than connection timeout
        assert!(timeout::BACKEND_READ > timeout::CONNECTION);

        // All timeouts should be non-zero
        assert!(timeout::BACKEND_READ.as_secs() > 0);
    }

    #[test]
    fn test_health_check_constraints() {
        // Keepalive interval should be within recommended bounds
        assert!(pool::MIN_RECOMMENDED_KEEPALIVE_SECS > 0);
        assert!(pool::MAX_RECOMMENDED_KEEPALIVE_SECS > pool::MIN_RECOMMENDED_KEEPALIVE_SECS);

        // Health check pool timeout should be short
        assert!(
            pool::HEALTH_CHECK_POOL_TIMEOUT_MS < 1000,
            "Health check timeout should be < 1s"
        );

        // Connection health check cycle limit should be reasonable
        assert!(pool::MAX_CONNECTIONS_PER_HEALTH_CHECK_CYCLE > 0);
        assert!(
            pool::MAX_CONNECTIONS_PER_HEALTH_CHECK_CYCLE <= 10,
            "Should not check too many at once"
        );
    }
}
