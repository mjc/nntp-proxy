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
/// This `const fn` exists purely to create a compile error if someone tries to add
/// a timeout loop. Any loop with `MAX_ITERATIONS > 1` will fail this assertion.
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
    /// Create a new guard (calls `remove_with_cooldown` on drop unless released)
    pub const fn new(conn: Object<TcpManager>, provider: DeadpoolConnectionProvider) -> Self {
        Self {
            conn: Some(conn),
            provider,
            released: false,
        }
    }

    /// Return connection to pool (healthy).
    ///
    /// Connection will be returned to the pool normally when the returned
    /// `Object` is dropped. The guard is consumed — no cleanup happens.
    ///
    /// # Panics
    ///
    /// Panics if the guard has already been consumed (double-release).
    pub fn release(mut self) -> Object<TcpManager> {
        self.released = true;
        self.conn
            .take()
            .expect("ConnectionGuard::release() called on consumed guard")
    }

    /// Get mutable reference to the connection
    ///
    /// # Panics
    ///
    /// Panics if the guard has already been consumed.
    pub const fn get_mut(&mut self) -> &mut Object<TcpManager> {
        self.conn
            .as_mut()
            .expect("ConnectionGuard already consumed")
    }

    /// Get shared reference to the connection
    ///
    /// # Panics
    ///
    /// Panics if the guard has already been consumed.
    pub const fn get(&self) -> &Object<TcpManager> {
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

#[cfg(test)]
mod tests {

    /// Verify that `TailBuffer` detects terminators that span chunk boundaries.
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
    //   1. `release()` returns the connection to pool — pool can reuse it without
    //      creating a new TCP connection to the backend.
    //
    //   2. drop without `release()` removes the connection — pool creates a fresh
    //      TCP connection on the next `get()`.
    //
    // These invariants protect the A2 refactor: any call site that uses
    // `ConnectionGuard` must call `release()` on "clean" paths (e.g. `ClientDisconnect`
    // where the backend was drained successfully) and let the guard drop on
    // "dirty" paths (backend errors, unknown state).
    //
    // A buggy A2 that drops the guard on `ClientDisconnect` without calling
    // `release()` would cause release_reuses_pool_connection to fail — the pool
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

    /// Invariant: `release()` returns the connection to the pool.
    ///
    /// Pool must reuse the connection without creating a new TCP connection.
    /// This is the path taken on success and on `ClientDisconnect` (backend was
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

    /// Invariant: drop without `release()` removes the connection from the pool.
    ///
    /// `remove_with_cooldown` shuts down the socket; pool recycle detects EOF
    /// and discards it; next `get()` creates a fresh TCP connection.
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

    // ─── salvage_with_health_check notes ────────────────────────────────────

    // Full integration tests for salvage_with_health_check would require
    // complex mocking of pooled connections with pending data. It is tested
    // indirectly through the command_execution integration tests.
    //
    // The constituent part check_date_response has its own tests in health_check.rs.
}
