//! Connection guard for automatic cleanup of broken pooled connections
//!
//! This module provides utilities to automatically remove broken connections from
//! the pool when I/O errors occur, preventing stale connections from being recycled.

use deadpool::managed::Object;

use crate::connection_error::{ConnectionError, is_connection_error_kind};
use crate::pool::deadpool_connection::TcpManager;

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
                let data = &buffer[..n];
                // Check for terminator (handles mid-chunk and cross-boundary spanning)
                if tail.detect_terminator(data).is_found() {
                    debug!("Successfully drained connection - returning to pool");
                    return; // Drop returns connection to pool
                }
                tail.update(data);
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
}
