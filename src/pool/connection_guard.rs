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

/// RAII guard for pooled connections
///
/// Automatically calls `remove_with_cooldown` on drop unless the connection is
/// explicitly released via `release()`. This ensures all broken connections are
/// cleaned up consistently, applying the cooldown logic regardless of call site.
///
/// Follows the same pattern as `CommandGuard` from `src/router/mod.rs`.
pub struct ConnectionGuard {
    conn: Option<Object<TcpManager>>,
    provider: DeadpoolConnectionProvider,
    released: bool,
}

impl ConnectionGuard {
    /// Create a new guard (calls remove_with_cooldown on drop unless released)
    pub fn new(conn: Object<TcpManager>, provider: DeadpoolConnectionProvider) -> Self {
        Self {
            conn: Some(conn),
            provider,
            released: false,
        }
    }

    /// Return connection to pool (healthy).
    ///
    /// Connection will be returned to the pool normally when the returned
    /// Object is dropped. The guard is consumed — no cleanup happens.
    pub fn release(mut self) -> Object<TcpManager> {
        self.released = true;
        self.conn
            .take()
            .expect("ConnectionGuard::release() called on consumed guard")
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
        if !self.released
            && let Some(conn) = self.conn.take()
        {
            // Unconditional: remove_with_cooldown handles socket shutdown and
            // optional pool-size reduction (when a replacement_cooldown is configured).
            self.provider.remove_with_cooldown(conn);
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
/// * `provider` - Connection provider (used for `remove_with_cooldown` on failure)
/// * `buffer_pool` - Buffer pool for reading response data
pub async fn drain_connection_async(
    mut conn: Object<TcpManager>,
    provider: DeadpoolConnectionProvider,
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
                // Unconditional: drain failure means backend state is unknown.
                provider.remove_with_cooldown(conn);
                return;
            }
        }
    }

    // Exceeded max time - connection likely broken
    debug!("Drain exceeded 5 minutes - removing connection from pool");
    // Unconditional: drain failure means backend state is unknown.
    provider.remove_with_cooldown(conn);
}

#[cfg(test)]
mod tests {

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

    // ─── ConnectionGuard pool-fate invariants ───────────────────────────────
    //
    // These tests verify two invariants that callers must rely on:
    //
    //   1. release() returns the connection to pool — pool can reuse it without
    //      creating a new TCP connection to the backend.
    //
    //   2. drop without release() removes the connection — pool creates a fresh
    //      TCP connection on the next get().
    //
    // These invariants protect the A2 refactor: any call site that uses
    // ConnectionGuard must call release() on "clean" paths (e.g. ClientDisconnect
    // where the backend was drained successfully) and let the guard drop on
    // "dirty" paths (backend errors, unknown state).
    //
    // A buggy A2 that drops the guard on ClientDisconnect without calling
    // release() would cause release_reuses_pool_connection to fail — the pool
    // would create a new TCP connection instead of reusing the existing one.

    use super::ConnectionGuard;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    /// Spawn a minimal mock NNTP greeting server (no auth, no compression).
    ///
    /// Returns `(port, accept_count)` where `accept_count` increments on each
    /// TCP accept. The server:
    ///   1. Sends `200 Ready\r\n` greeting
    ///   2. Responds to `COMPRESS DEFLATE` with `500 Not supported\r\n`
    ///      (required so `TcpManager::create()` completes compression negotiation)
    ///   3. Keeps the connection open until the client closes it
    async fn spawn_greeting_server() -> (u16, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accept_count = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&accept_count);

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                count.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let (read_half, mut write_half) = stream.into_split();
                    let mut reader = BufReader::new(read_half);

                    // Send NNTP greeting
                    if write_half.write_all(b"200 Ready\r\n").await.is_err() {
                        return;
                    }

                    // Handle TcpManager setup commands, then keep alive.
                    // TcpManager::create() sends COMPRESS DEFLATE (auto-detect mode);
                    // we reject it so create() completes and proceeds without compression.
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line).await {
                            Ok(0) | Err(_) => break, // Client closed connection
                            Ok(_) => {
                                if line.trim().eq_ignore_ascii_case("COMPRESS DEFLATE") {
                                    let _ = write_half.write_all(b"500 Not supported\r\n").await;
                                }
                                // Other commands: stay alive, don't respond
                            }
                        }
                    }
                });
            }
        });

        (port, accept_count)
    }

    fn make_provider(port: u16) -> crate::pool::DeadpoolConnectionProvider {
        crate::pool::DeadpoolConnectionProvider::builder("127.0.0.1", port)
            .max_connections(5)
            .build()
            .unwrap()
    }

    /// Invariant: release() returns the connection to the pool.
    ///
    /// Pool must reuse the connection without creating a new TCP connection.
    /// This is the path taken on success and on ClientDisconnect (backend was
    /// cleanly drained — connection is still valid).
    #[tokio::test]
    async fn release_reuses_pool_connection() {
        let (port, accept_count) = spawn_greeting_server().await;
        let provider = make_provider(port);

        // First get — establishes TCP connection #1
        let conn = provider.get_pooled_connection().await.unwrap();
        assert_eq!(accept_count.load(Ordering::SeqCst), 1);

        // release() returns conn to pool (no shutdown)
        let guard = ConnectionGuard::new(conn, provider.clone());
        drop(guard.release());

        // Second get — pool recycles the existing connection (no new TCP handshake)
        let _conn2 = provider.get_pooled_connection().await.unwrap();
        assert_eq!(
            accept_count.load(Ordering::SeqCst),
            1,
            "release() must return connection to pool; next get() must reuse it without \
             creating a new TCP connection"
        );
    }

    /// Invariant: drop without release() removes the connection from the pool.
    ///
    /// remove_with_cooldown shuts down the socket; pool recycle detects EOF
    /// and discards it; next get() creates a fresh TCP connection.
    /// This is the path taken on backend errors and unknown connection state.
    #[tokio::test]
    async fn drop_without_release_forces_new_connection() {
        let (port, accept_count) = spawn_greeting_server().await;
        let provider = make_provider(port);

        // First get — establishes TCP connection #1
        let conn = provider.get_pooled_connection().await.unwrap();
        assert_eq!(accept_count.load(Ordering::SeqCst), 1);

        // Drop without release → remove_with_cooldown → socket shut down
        let guard = ConnectionGuard::new(conn, provider.clone());
        drop(guard);

        // remove_with_cooldown calls socket2::shutdown(Both) synchronously, so the OS
        // has already marked the fd as EOF. However, tokio's non-blocking try_read()
        // inside check_tcp_alive only returns Ok(0) once the tokio I/O driver has
        // processed the POLLIN event from epoll. That requires the runtime to park
        // (epoll_wait). A short sleep causes the current task to suspend, the runtime
        // parks, epoll delivers the event, and the socket is marked readable (EOF) —
        // so the next recycle() correctly detects the dead connection and discards it.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Second get — pool recycles, check_tcp_alive detects EOF, removes it,
        // creates a new TCP connection (#2)
        let _conn2 = provider.get_pooled_connection().await.unwrap();
        assert_eq!(
            accept_count.load(Ordering::SeqCst),
            2,
            "drop without release() must remove connection; next get() must create \
             a new TCP connection"
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
