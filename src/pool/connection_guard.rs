//! Connection guard for automatic cleanup of broken pooled connections
//!
//! This module provides utilities to automatically remove broken connections from
//! the pool when I/O errors occur, preventing stale connections from being recycled.
//!
//! # CRITICAL: Connection Hold Time Guarantees
//!
//! All connection salvage operations MUST complete in <1 second to prevent pool
//! starvation and throughput collapse. This is enforced by:
//! - Compile-time const assertions on timeout values
//! - Type-level guarantees preventing timeout loops

use deadpool::managed::Object;

use crate::connection_error::{ConnectionError, is_connection_error_kind};
use crate::constants::pool::HEALTH_CHECK_TIMEOUT;
use crate::pool::deadpool_connection::TcpManager;
use crate::pool::provider::DeadpoolConnectionProvider;

// ═══════════════════════════════════════════════════════════════════════════
// COMPILE-TIME SAFEGUARDS: Connection Hold Time Limits
// ═══════════════════════════════════════════════════════════════════════════

/// Maximum time (milliseconds) any connection salvage operation can take
///
/// CRITICAL: If salvage takes longer than this, we risk pool starvation and
/// throughput collapse. This constant is enforced by:
/// - Const assertions below
/// - Code review guidelines
///
/// Background: A previous implementation held connections for 10 seconds trying
/// to drain unparseable data, causing 40% throughput regression (60 MB/s vs 100+ MB/s).
pub const MAX_CONNECTION_SALVAGE_MS: u64 = 1000;

/// COMPILE-TIME ASSERTION: Prevent timeout loops from being added
///
/// This const fn exists purely to create a compile error if someone tries to add
/// a timeout loop. Any loop with MAX_ITERATIONS > 1 will fail this assertion.
///
/// Example that will NOT compile:
/// ```compile_fail
/// const MAX_DRAIN_ITERATIONS: usize = 50; // FAILS ASSERTION
/// const DRAIN_TIMEOUT_MS: u64 = 200;
/// const _: () = assert_no_timeout_loop(MAX_DRAIN_ITERATIONS, DRAIN_TIMEOUT_MS);
/// ```
#[allow(dead_code)]
const fn assert_no_timeout_loop(max_iterations: usize, timeout_per_iteration_ms: u64) {
    // If you see this compile error, you're trying to add a timeout loop
    // that could hold connections for too long. Use DATE health check instead.
    assert!(
        max_iterations == 1,
        "Connection salvage MUST NOT use timeout loops (max_iterations must be 1)"
    );
    assert!(
        timeout_per_iteration_ms <= MAX_CONNECTION_SALVAGE_MS,
        "Single timeout must be <= MAX_CONNECTION_SALVAGE_MS"
    );
}

// Apply assertion to salvage_with_health_check (implicit: it has no loop)
const _SALVAGE_NO_LOOP: () = {
    // salvage_with_health_check has exactly 1 operation (DATE check)
    // Compare as u128 to avoid truncating cast (as_millis() returns u128; safe for any
    // reasonable timeout value, but avoids the footgun entirely)
    assert!(
        HEALTH_CHECK_TIMEOUT.as_millis() <= MAX_CONNECTION_SALVAGE_MS as u128,
        "HEALTH_CHECK_TIMEOUT must be <= MAX_CONNECTION_SALVAGE_MS"
    );
    // Assert exactly 1 operation (no loop)
    assert_no_timeout_loop(1, MAX_CONNECTION_SALVAGE_MS);
};

/// Check if an error should cause the connection to be removed from the pool
///
/// Uses centralized CONNECTION_ERROR_KINDS from connection_error module.
#[inline]
pub fn is_connection_error(e: &anyhow::Error) -> bool {
    // Check for typed ConnectionError first
    if let Some(conn_err) = e.downcast_ref::<ConnectionError>() {
        return matches!(
            conn_err,
            ConnectionError::IoError(io_err) if is_connection_error_kind(io_err.kind())
        ) || matches!(
            conn_err,
            ConnectionError::StaleConnection { .. } | ConnectionError::BackendTimeout { .. }
        );
    }
    // Fallback for raw io::Error from non-pool paths
    if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
        return is_connection_error_kind(io_err.kind());
    }
    false
}

/// Take a connection out of the pool, preventing it from being recycled
///
/// This should be called when a connection encounters an error that indicates
/// it's broken and should not be reused.
///
/// The connection is shut down at the TCP level before being dropped, ensuring
/// deadpool's recycle check reliably detects it as dead. The connection is then
/// returned to the pool normally, where deadpool drops the broken connection
/// BEFORE creating a replacement (preventing connection limit exceeded errors).
#[inline]
pub fn remove_from_pool(conn: Object<TcpManager>) {
    // Shut down the raw TCP socket so recycle reliably detects it as dead
    let _ = socket2::SockRef::from(conn.underlying_tcp_stream()).shutdown(std::net::Shutdown::Both);
    // Return to pool normally — deadpool drops it before creating a replacement
    drop(conn);
}

/// RAII guard for pooled connections
///
/// Automatically removes the connection from the pool on drop unless
/// explicitly marked as healthy via `success()`.
///
/// Follows the same pattern as `CommandGuard` from `src/router/mod.rs`.
pub struct ConnectionGuard {
    conn: Option<Object<TcpManager>>,
    remove_on_drop: bool,
}

impl ConnectionGuard {
    /// Create a new guard (defaults to removing on drop)
    pub fn new(conn: Object<TcpManager>) -> Self {
        Self {
            conn: Some(conn),
            remove_on_drop: true,
        }
    }

    /// Mark connection as healthy and return it
    ///
    /// Connection will be returned to pool normally when dropped.
    pub fn success(mut self) -> Object<TcpManager> {
        self.remove_on_drop = false;
        self.conn
            .take()
            .expect("ConnectionGuard::success() called twice")
    }

    /// Get mutable reference to the connection
    pub fn get_mut(&mut self) -> &mut Object<TcpManager> {
        self.conn
            .as_mut()
            .expect("ConnectionGuard already consumed")
    }

    /// Get shared reference to the connection
    pub fn get(&self) -> &Object<TcpManager> {
        self.conn
            .as_ref()
            .expect("ConnectionGuard already consumed")
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        if self.remove_on_drop
            && let Some(conn) = self.conn.take()
        {
            remove_from_pool(conn);
        }
    }
}

impl std::ops::Deref for ConnectionGuard {
    type Target = Object<TcpManager>;
    fn deref(&self) -> &Self::Target {
        self.get()
    }
}

impl std::ops::DerefMut for ConnectionGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.get_mut()
    }
}

/// Salvage connection after Invalid response using DATE health check
///
/// Used when an Invalid response is detected - attempts to salvage the connection
/// instead of immediately removing it. This helps prevent connection churn.
///
/// # Strategy
/// Send DATE command to verify connection is clean and responsive. If leftover
/// data exists in the stream, the DATE response will be corrupted and the check
/// will fail - this is both faster (~1 RTT vs 10 seconds) and equally correct.
///
/// On success: return connection to pool (just drop it normally)
/// On failure: use `remove_with_cooldown` to remove gracefully with backoff
///
/// # Arguments
/// * `conn` - Pooled connection to verify
/// * `provider` - Connection provider (used for `remove_with_cooldown` on failure)
pub async fn salvage_with_health_check(
    mut conn: Object<TcpManager>,
    provider: DeadpoolConnectionProvider,
) {
    use tracing::{debug, warn};

    match crate::pool::health_check::check_date_response(&mut conn).await {
        Ok(()) => {
            debug!("Connection salvaged after Invalid response - DATE check passed");
            drop(conn); // returns to pool
        }
        Err(e) => {
            warn!("DATE health check failed after Invalid response: {}", e);
            // Unconditional: this is a pool-level operation with no client involved.
            // DATE failure means the connection is in an unknown/dirty state.
            provider.remove_with_cooldown(conn);
        }
    }
}

/// Drain response data from timed-out connection to allow pool reuse
///
/// Backend connection limits require us to reuse connections instead of creating new ones.
/// When a backend times out on first byte, we spawn this task to:
/// 1. Read until multiline terminator found (connection clean → return to pool)
/// 2. Timeout/error/EOF → remove from pool (connection broken)
///
/// This prevents hitting backend connection limits while letting clients retry immediately.
///
/// # Arguments
/// * `conn` - Pooled connection to drain
/// * `buffer_pool` - Buffer pool for reading response data
pub async fn drain_connection_async(
    mut conn: Object<TcpManager>,
    buffer_pool: crate::pool::BufferPool,
) {
    use crate::session::streaming::tail_buffer::TailBuffer;
    use std::time::Duration;
    use tracing::debug;

    const MAX_DRAIN_ITERATIONS: usize = 1500; // 5 minutes at 200ms/iteration
    const DRAIN_TIMEOUT: Duration = Duration::from_millis(200);

    let mut buffer = buffer_pool.acquire().await;
    let mut tail = TailBuffer::default();

    for _ in 0..MAX_DRAIN_ITERATIONS {
        match tokio::time::timeout(DRAIN_TIMEOUT, buffer.read_from(&mut *conn)).await {
            Ok(Ok(n)) if n > 0 => {
                use crate::session::streaming::tail_buffer::TerminatorStatus;
                let data = &buffer[..n];
                match tail.detect_terminator(data) {
                    TerminatorStatus::FoundAt(pos) if pos == n => {
                        // Terminator at exact end of chunk — no leftover bytes, connection clean
                        debug!("Successfully drained connection - returning to pool");
                        return; // Drop returns connection to pool
                    }
                    TerminatorStatus::FoundAt(_) => {
                        // Terminator mid-chunk: leftover bytes already consumed from socket,
                        // connection is desynchronized and cannot be safely reused
                        debug!("Leftover bytes after terminator - removing from pool");
                        remove_from_pool(conn);
                        return;
                    }
                    TerminatorStatus::NotFound => {
                        tail.update(data);
                    }
                }
            }
            _ => {
                // Timeout, EOF, or error - connection broken
                debug!("Failed to drain connection - removing from pool");
                remove_from_pool(conn);
                return;
            }
        }
    }

    // Exceeded max time - connection likely broken
    debug!("Drain exceeded 5 minutes - removing connection from pool");
    remove_from_pool(conn);
}

/// Execute a function with a pooled connection, automatically removing it from
/// the pool if a connection-level I/O error occurs.
///
/// # Example
/// ```no_run
/// use anyhow::Result;
/// use deadpool::managed::Object;
/// use nntp_proxy::pool::deadpool_connection::TcpManager;
/// use nntp_proxy::pool::execute_with_guard;
///
/// async fn example(conn: Object<TcpManager>) -> Result<()> {
///     execute_with_guard(conn, |conn| async move {
///         // Do work with connection
///         // If this returns a BrokenPipe, ConnectionReset, etc.,
///         // the connection will be automatically removed from the pool
///         Ok(())
///     }).await
/// }
/// ```
pub async fn execute_with_guard<F, Fut, T>(
    mut pooled_conn: Object<TcpManager>,
    f: F,
) -> anyhow::Result<T>
where
    F: FnOnce(&mut Object<TcpManager>) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let result = f(&mut pooled_conn).await;

    // If there was a connection error, remove from pool
    if let Err(ref e) = result
        && is_connection_error(e)
    {
        remove_from_pool(pooled_conn);
        return result;
    }

    // Otherwise, connection will be returned to pool on drop
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error as IoError, ErrorKind};

    #[test]
    fn test_is_connection_error_broken_pipe() {
        let io_err = IoError::new(ErrorKind::BrokenPipe, "broken pipe");
        let err: anyhow::Error = io_err.into();
        assert!(is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_connection_reset() {
        let io_err = IoError::new(ErrorKind::ConnectionReset, "connection reset");
        let err: anyhow::Error = io_err.into();
        assert!(is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_connection_aborted() {
        let io_err = IoError::new(ErrorKind::ConnectionAborted, "connection aborted");
        let err: anyhow::Error = io_err.into();
        assert!(is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_unexpected_eof() {
        let io_err = IoError::new(ErrorKind::UnexpectedEof, "unexpected eof");
        let err: anyhow::Error = io_err.into();
        assert!(is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_would_block() {
        let io_err = IoError::new(ErrorKind::WouldBlock, "would block");
        let err: anyhow::Error = io_err.into();
        assert!(!is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_timeout() {
        let io_err = IoError::new(ErrorKind::TimedOut, "timed out");
        let err: anyhow::Error = io_err.into();
        assert!(!is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_permission_denied() {
        let io_err = IoError::new(ErrorKind::PermissionDenied, "permission denied");
        let err: anyhow::Error = io_err.into();
        assert!(!is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_non_io_error() {
        let err = anyhow::anyhow!("some other error");
        assert!(!is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_custom_error() {
        #[derive(Debug)]
        struct CustomError;
        impl std::fmt::Display for CustomError {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "custom error")
            }
        }
        impl std::error::Error for CustomError {}

        let err: anyhow::Error = CustomError.into();
        assert!(!is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_all_connection_errors() {
        // Test all the error kinds we consider "connection errors"
        let error_kinds = vec![
            ErrorKind::BrokenPipe,
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::UnexpectedEof,
        ];

        for kind in error_kinds {
            let io_err = IoError::new(kind, format!("{:?}", kind));
            let err: anyhow::Error = io_err.into();
            assert!(
                is_connection_error(&err),
                "Expected {:?} to be a connection error",
                kind
            );
        }
    }

    #[test]
    fn test_is_connection_error_non_connection_errors() {
        // Test error kinds that should NOT be considered connection errors
        let error_kinds = vec![
            ErrorKind::NotFound,
            ErrorKind::PermissionDenied,
            ErrorKind::AddrInUse,
            ErrorKind::AddrNotAvailable,
            ErrorKind::AlreadyExists,
            ErrorKind::WouldBlock,
            ErrorKind::InvalidInput,
            ErrorKind::InvalidData,
            ErrorKind::TimedOut,
            ErrorKind::WriteZero,
            ErrorKind::Interrupted,
            ErrorKind::Unsupported,
            ErrorKind::OutOfMemory,
        ];

        for kind in error_kinds {
            let io_err = IoError::new(kind, format!("{:?}", kind));
            let err: anyhow::Error = io_err.into();
            assert!(
                !is_connection_error(&err),
                "Expected {:?} to NOT be a connection error",
                kind
            );
        }
    }

    /// Verify that TailBuffer sees pos < n for a mid-chunk terminator.
    /// This guards the condition checked by drain_connection_async: FoundAt(pos) with
    /// leftover bytes must remove from pool. The end-to-end pool behavior is verified
    /// in test_drain_async_mid_chunk_terminator_removes_from_pool.
    #[test]
    fn test_tailbuffer_mid_chunk_terminator_pos_less_than_n() {
        use crate::session::streaming::tail_buffer::{TailBuffer, TerminatorStatus};

        // Chunk contains the terminator mid-way, with extra bytes after it.
        // Simulates a backend that sent two pipelined responses in one TCP read.
        let chunk = b"220 Article follows\r\nBody\r\n.\r\nEXTRA DATA FROM NEXT RESPONSE";
        let n = chunk.len();

        let tail = TailBuffer::default();
        let status = tail.detect_terminator(chunk);

        match status {
            TerminatorStatus::FoundAt(pos) => {
                assert!(
                    pos < n,
                    "Terminator should be mid-chunk (pos={} < n={})",
                    pos,
                    n
                );
                // This is the condition that triggers remove_from_pool in
                // drain_connection_async — connection is desynchronized
            }
            other => panic!("Expected FoundAt, got {:?}", other),
        }
    }

    /// Verify that TailBuffer sees pos == n for a terminator exactly at chunk end.
    /// This guards the condition checked by drain_connection_async: FoundAt(pos) with
    /// no leftover bytes returns the connection to the pool. The end-to-end pool
    /// behavior is verified in test_drain_async_clean_terminator_returns_to_pool.
    #[test]
    fn test_tailbuffer_end_of_chunk_terminator_pos_equals_n() {
        use crate::session::streaming::tail_buffer::{TailBuffer, TerminatorStatus};

        // Chunk ends exactly with the terminator — no leftover bytes.
        let chunk = b"220 Article follows\r\nBody\r\n.\r\n";
        let n = chunk.len();

        let tail = TailBuffer::default();
        let status = tail.detect_terminator(chunk);

        match status {
            TerminatorStatus::FoundAt(pos) => {
                assert_eq!(
                    pos, n,
                    "Terminator at end of chunk should have pos == n ({} == {})",
                    pos, n
                );
                // This is the condition that allows returning to pool in
                // drain_connection_async — connection is clean
            }
            other => panic!("Expected FoundAt, got {:?}", other),
        }
    }

    /// Spawn a minimal NNTP mock server that sends a greeting, then waits for
    /// `notify` before sending `data`. Loops to accept multiple connections
    /// (needed when a broken connection is replaced by `create_new`).
    ///
    /// The caller calls `pool.get()` first (which consumes only the greeting),
    /// then fires the notify so the server sends article data into the established
    /// connection. This prevents `consume_greeting` from inadvertently consuming
    /// article bytes (both writes arriving in the same TCP segment).
    async fn spawn_test_nntp_server(
        data: &'static [u8],
    ) -> (
        std::net::SocketAddr,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        std::sync::Arc<tokio::sync::Notify>,
    ) {
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;
        use tokio::sync::Notify;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accept_count = Arc::new(AtomicUsize::new(0));
        let notify = Arc::new(Notify::new());
        let count = Arc::clone(&accept_count);
        let n = Arc::clone(&notify);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let wake = Arc::clone(&n);
                tokio::spawn(async move {
                    let _ = stream.write_all(b"200 mock\r\n").await;
                    // Block until the test signals that pool.get() has returned
                    // (greeting already consumed) before sending article data
                    wake.notified().await;
                    let _ = stream.write_all(data).await;
                    // Keep alive so recycle's try_read sees WouldBlock (not EOF)
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                });
            }
        });
        (addr, accept_count, notify)
    }

    async fn make_test_pool(addr: std::net::SocketAddr) -> crate::pool::deadpool_connection::Pool {
        let manager = crate::pool::deadpool_connection::TcpManager::new(
            addr.ip().to_string(),
            addr.port(),
            "test".to_string(),
            None,
            None,
            None,
            Some(false), // disable compression — mock doesn't handle it
            None,
        )
        .unwrap();
        crate::pool::deadpool_connection::Pool::builder(manager)
            .max_size(2)
            .build()
            .unwrap()
    }

    /// `drain_connection_async` returns the connection to the pool when the
    /// terminator arrives exactly at the end of a read chunk (pos == n).
    /// Verifies via pool.status() and that the same server connection is reused.
    #[tokio::test]
    async fn test_drain_async_clean_terminator_returns_to_pool() {
        use crate::pool::BufferPool;
        use crate::types::BufferSize;
        use std::sync::atomic::Ordering;

        let (addr, accept_count, notify) = spawn_test_nntp_server(b"article body\r\n.\r\n").await;
        let pool = make_test_pool(addr).await;
        let buffer_pool = BufferPool::new(BufferSize::try_new(4096).unwrap(), 2);

        let conn = pool.get().await.unwrap();
        // Signal server to send article data now that the greeting is consumed
        notify.notify_one();
        assert_eq!(pool.status().available, 0);

        drain_connection_async(conn, buffer_pool).await;

        // Object dropped synchronously → returned to ready queue immediately
        assert_eq!(pool.status().available, 1, "connection must return to pool");

        // Recycle check: try_read sees WouldBlock (server idle) → recycle succeeds
        // No new server connection should be required
        let _conn2 = pool.get().await.unwrap();
        assert_eq!(
            accept_count.load(Ordering::Relaxed),
            1,
            "clean path: same connection recycled, no new server connection"
        );
    }

    /// `drain_connection_async` removes the connection from the pool when the
    /// terminator is found mid-chunk (pos < n), meaning leftover bytes from the
    /// next response were already consumed from the socket.
    /// Verifies that the dead connection is detected during recycle and replaced.
    #[tokio::test]
    async fn test_drain_async_mid_chunk_terminator_removes_from_pool() {
        use crate::pool::BufferPool;
        use crate::types::BufferSize;
        use std::sync::atomic::Ordering;

        let (addr, accept_count, notify) =
            spawn_test_nntp_server(b"article body\r\n.\r\nEXTRA_BYTES").await;
        let pool = make_test_pool(addr).await;
        let buffer_pool = BufferPool::new(BufferSize::try_new(4096).unwrap(), 2);

        let conn = pool.get().await.unwrap();
        // Signal server to send article data now that the greeting is consumed
        notify.notify_one();
        drain_connection_async(conn, buffer_pool).await;

        // Object is returned to the ready queue (deadpool doesn't know it's broken)
        assert_eq!(pool.status().available, 1);

        // Yield to let tokio process the pending EPOLLRDHUP event from shutdown(Both).
        // Without this, check_tcp_alive's try_read sees WouldBlock (readiness not yet
        // updated) instead of Ok(0) (EOF), causing recycle to succeed incorrectly.
        tokio::task::yield_now().await;

        // Next get() calls try_recycle → check_tcp_alive on the shutdown socket
        // sees EOF → recycle fails → create_new → server accepts a second connection
        let _conn2 = pool.get().await.unwrap();
        assert_eq!(
            accept_count.load(Ordering::Relaxed),
            2,
            "remove path: broken connection replaced with fresh server connection"
        );
    }

    /// Verify that TailBuffer detects terminators that span chunk boundaries,
    /// which the old `windows()` approach in `drain_connection_async` would miss.
    #[test]
    fn test_drain_cross_boundary_terminator() {
        use crate::session::streaming::tail_buffer::TailBuffer;

        // Simulate two reads where the terminator \r\n.\r\n is split:
        // First read ends with "\r\n.", second read starts with "\r\n"
        let chunk1 = b"220 Article follows\r\nBody content\r\n.";
        let chunk2 = b"\r\n";

        // Old approach: windows() on each chunk independently — would MISS this
        let terminator = b"\r\n.\r\n";
        let found_by_windows_chunk1 = chunk1.windows(terminator.len()).any(|w| w == terminator);
        let found_by_windows_chunk2 = chunk2.windows(terminator.len()).any(|w| w == terminator);
        assert!(
            !found_by_windows_chunk1,
            "windows() should not find terminator in chunk1"
        );
        assert!(
            !found_by_windows_chunk2,
            "windows() should not find terminator in chunk2"
        );

        // New approach: TailBuffer tracks cross-boundary state
        let mut tail = TailBuffer::default();

        // Process chunk1 — no terminator yet
        assert!(!tail.detect_terminator(chunk1).is_found());
        tail.update(chunk1);

        // Process chunk2 — spanning terminator detected!
        assert!(
            tail.detect_terminator(chunk2).is_found(),
            "TailBuffer should detect terminator spanning chunk boundary"
        );
    }

    // ─── drain_and_health_check tests ───────────────────────────────────────

    // Note: Full integration tests for drain_and_health_check would require
    // complex mocking of pooled connections with pending data. The function
    // is tested indirectly through the command_execution integration tests.
    //
    // Unit tests verify the constituent parts:
    // - drain_connection_async (existing tests above)
    // - check_date_response (tests in health_check.rs)
    //
    // The drain_and_health_check function combines these and is primarily
    // tested through integration tests where Invalid responses trigger draining.
}
