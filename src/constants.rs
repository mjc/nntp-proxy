//! Constants used throughout the NNTP proxy
//!
//! This module centralizes magic numbers and configuration values
//! to improve maintainability and reduce duplication.

use std::time::Duration;

/// Polyfill for `Duration` constructors unavailable on the project MSRV.
pub mod duration_polyfill {
    use super::Duration;

    #[inline]
    #[must_use]
    pub const fn from_minutes(value: u64) -> Duration {
        Duration::from_secs(value * 60)
    }

    #[inline]
    #[must_use]
    pub const fn from_hours(value: u64) -> Duration {
        Duration::from_secs(value * 60 * 60)
    }
}

/// Buffer size constants
///
/// All buffer sizes are carefully chosen for NNTP workloads:
/// - Commands are small (< 512 bytes)
/// - Articles range from 1KB to 768KB typically (99th percentile)
/// - Pooled buffers use aligned sizes for optimal memory access
pub mod buffer {
    // Buffer pool configuration

    /// Page size for memory alignment (4KB = standard OS page)
    /// Used in compile-time assertions below to verify alignment.
    #[allow(dead_code)] // Used in const assertions which the compiler doesn't track
    const PAGE_SIZE: usize = 4096;

    /// Size of each pooled buffer (1 MiB, page-aligned)
    /// Large enough to keep typical article reads in a single chunk while
    /// leaving headroom for bigger first reads and direct streaming paths.
    pub const POOL: usize = 1024 * 1024;

    /// Default number of buffers in the buffer pool
    /// Sized for ~50 concurrent streaming connections
    /// Total memory: 50 × 1MiB = 50MiB
    /// This is a conservative default; increase via config for higher concurrency
    pub const POOL_COUNT: usize = 50;

    /// Size of each capture buffer (1 MiB, page-aligned).
    ///
    /// Sized to capture typical articles and direct pending-byte tracking without
    /// immediate growth or fallback allocations.
    pub const CAPTURE: usize = 1024 * 1024;

    /// Default number of capture buffers in the pool.
    ///
    /// Sized for 16 concurrent caching operations.
    /// Total memory: 16 × 1MiB = 16MiB.
    /// This is a conservative default; increase via config for higher caching throughput.
    pub const CAPTURE_COUNT: usize = 16;

    // Command and response limits

    /// Maximum command line size (512 bytes)
    /// NNTP commands are typically small: "ARTICLE <msgid@example.com>"
    pub const COMMAND: usize = 512;

    /// `BufReader` capacity for client command parsing.
    ///
    /// NNTP command lines are capped at 512 bytes. This holds a full 16-command
    /// pipeline batch without giving every client a 64KB read buffer.
    pub const READER_CAPACITY: usize = COMMAND * 16;

    /// Maximum size for a single response (2MB)
    /// Increased to handle larger articles, prevents memory exhaustion
    pub const RESPONSE_MAX: usize = 2 * 1024 * 1024;

    /// Maximum pending bytes kept on a pooled backend connection.
    ///
    /// Larger queues indicate protocol desynchronization or unsolicited extra
    /// responses, so the connection should be retired instead of buffering
    /// unbounded data.
    pub const MAX_PENDING_BACKEND_BYTES: usize = 128 * 1024;

    /// Initial capacity for response accumulation buffers (8KB, page-aligned)
    /// Sized for typical status lines and small responses
    pub const RESPONSE_INITIAL: usize = 8192;

    // Streaming configuration

    /// Chunk size for streaming responses (1 MiB, page-aligned)
    /// Matches POOL size so the first direct backend read and follow-on reads
    /// use the same pooled allocation size.
    pub const STREAM_CHUNK: usize = 1024 * 1024;

    // Compile-time validation

    /// Verify pool buffer is page-aligned at compile time
    const _POOL_ALIGNED: () = assert!(POOL.is_multiple_of(PAGE_SIZE), "POOL must be page-aligned");

    /// Verify capture buffer is page-aligned at compile time
    const _CAPTURE_ALIGNED: () = assert!(
        CAPTURE.is_multiple_of(PAGE_SIZE),
        "CAPTURE must be page-aligned"
    );

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

    /// TCP socket receive buffer size for connection pools (10MiB)
    /// Sized to handle 1 MiB streaming chunks efficiently (10x chunk size)
    pub const POOL_RECV_BUFFER: usize = 10 * 1024 * 1024;

    /// TCP socket send buffer size for connection pools (10MiB)
    /// Sized to handle 1 MiB streaming chunks efficiently (10x chunk size)
    pub const POOL_SEND_BUFFER: usize = 10 * 1024 * 1024;
}

/// Timeout constants
pub mod timeout {
    use super::Duration;

    /// Timeout for reading responses from backend servers
    pub const BACKEND_READ: Duration = Duration::from_secs(30);

    /// Timeout for executing a command on backend
    pub const COMMAND_EXECUTION: Duration = crate::constants::duration_polyfill::from_minutes(1);

    /// Connection timeout for backend connections
    pub const CONNECTION: Duration = Duration::from_secs(10);

    /// Timeout for adaptive precheck queries (STAT/HEAD)
    /// If a backend doesn't respond within this time, treat as 430 (missing)
    /// This prevents slow backends from blocking all client connections
    pub const PRECHECK_QUERY: Duration = Duration::from_secs(2);

    /// Timeout for closing the cache during graceful shutdown
    /// foyer's `close()` can hang indefinitely if the runtime is winding down
    pub const CACHE_CLOSE: Duration = Duration::from_secs(3);

    /// Timeout for sending QUIT to an idle backend connection during shutdown
    /// Half-closed connections can block `write_all` indefinitely without this
    pub const SHUTDOWN_QUIT_WRITE: Duration = Duration::from_millis(500);

    /// Timeout for acquiring an idle connection from the pool during shutdown
    /// Used to drain connections one at a time; should be near-instant if
    /// the connection is truly idle
    pub const SHUTDOWN_POOL_GET: Duration = Duration::from_millis(1);
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
    /// CRITICAL: Must be < `MAX_CONNECTION_SALVAGE_MS` (1000ms) to prevent pool starvation
    pub const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_millis(500);

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

/// Session and metrics constants
pub mod session {
    /// Flush incremental metrics every N commands for long-running sessions
    ///
    /// Prevents metrics from accumulating indefinitely without being recorded.
    /// Value of 100 balances between:
    /// - Frequent enough to avoid significant data loss on crashes
    /// - Infrequent enough to avoid performance overhead
    pub const METRICS_FLUSH_INTERVAL: u32 = 100;
}

/// Display strings for user metrics and logging
pub mod user {
    /// Display name for anonymous/unauthenticated users
    ///
    /// Used as `HashMap` key and display value for users who haven't authenticated.
    /// The `<anonymous>` format is chosen to:
    /// - Sort first in alphabetical listings (< comes before letters)
    /// - Be clearly distinguished from actual usernames
    /// - Be consistent across all metrics and logging
    pub const ANONYMOUS: &str = "<anonymous>";
}

// Compile-time invariant checks — these fail the build if constants are inconsistent
const _: () = {
    // Buffer alignment (4KB page boundaries)
    assert!(
        buffer::POOL.is_multiple_of(4096),
        "POOL must be page-aligned"
    );
    assert!(
        buffer::STREAM_CHUNK.is_multiple_of(4096),
        "STREAM_CHUNK must be page-aligned"
    );
    assert!(
        buffer::POOL == buffer::STREAM_CHUNK,
        "POOL and STREAM_CHUNK should match"
    );

    // Buffer size relationships
    assert!(buffer::POOL == 1024 * 1024);
    assert!(buffer::READER_CAPACITY >= buffer::COMMAND);
    assert!(buffer::READER_CAPACITY == buffer::COMMAND * 16);
    assert!(buffer::RESPONSE_MAX > buffer::POOL);
    assert!(buffer::RESPONSE_INITIAL >= buffer::COMMAND);
    assert!(buffer::RESPONSE_MAX > buffer::RESPONSE_INITIAL);
    assert!(buffer::POOL >= buffer::STREAM_CHUNK);

    // Socket buffer ratios (~10x streaming chunk)
    assert!(socket::POOL_RECV_BUFFER >= buffer::STREAM_CHUNK * 10);
    assert!(socket::POOL_SEND_BUFFER >= buffer::STREAM_CHUNK * 10);
    assert!(socket::HIGH_THROUGHPUT_RECV_BUFFER > socket::POOL_RECV_BUFFER);
    assert!(socket::HIGH_THROUGHPUT_SEND_BUFFER > socket::POOL_SEND_BUFFER);
    assert!(socket::HIGH_THROUGHPUT_RECV_BUFFER == socket::HIGH_THROUGHPUT_SEND_BUFFER);

    // Health check constraints
    assert!(pool::MIN_RECOMMENDED_KEEPALIVE_SECS > 0);
    assert!(pool::MAX_RECOMMENDED_KEEPALIVE_SECS > pool::MIN_RECOMMENDED_KEEPALIVE_SECS);
    assert!(
        pool::HEALTH_CHECK_POOL_TIMEOUT_MS < 1000,
        "Health check timeout should be < 1s"
    );
    assert!(pool::MAX_CONNECTIONS_PER_HEALTH_CHECK_CYCLE > 0);
    assert!(
        pool::MAX_CONNECTIONS_PER_HEALTH_CHECK_CYCLE <= 10,
        "Should not check too many at once"
    );
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pool_memory_footprint() {
        // Calculate total pool memory
        let total_memory = buffer::POOL * buffer::POOL_COUNT;

        // Should be exactly 50MiB (50 buffers × 1MiB)
        let expected_mb = 50;
        let actual_mb = total_memory / (1024 * 1024);

        assert_eq!(
            actual_mb, expected_mb,
            "Pool memory should be {expected_mb}MiB, got {actual_mb}MiB"
        );
    }

    #[test]
    fn test_timeouts() {
        // Command execution timeout should be at least as long as backend read timeout
        assert!(timeout::COMMAND_EXECUTION >= timeout::BACKEND_READ);

        // Backend read timeout should be at least as long as connection timeout
        assert!(timeout::BACKEND_READ >= timeout::CONNECTION);

        // All timeouts should be non-zero
        assert!(timeout::BACKEND_READ.as_secs() > 0);
    }
}
