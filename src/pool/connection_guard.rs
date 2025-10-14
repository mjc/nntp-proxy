//! Connection guard for automatic cleanup of broken pooled connections
//!
//! This module provides utilities to automatically remove broken connections from
//! the pool when I/O errors occur, preventing stale connections from being recycled.

use deadpool::managed::Object;
use std::io::ErrorKind;

use crate::pool::deadpool_connection::TcpManager;

/// Check if an error should cause the connection to be removed from the pool
pub fn is_connection_error(e: &anyhow::Error) -> bool {
    if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
        matches!(
            io_err.kind(),
            ErrorKind::BrokenPipe
                | ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted
                | ErrorKind::UnexpectedEof
        )
    } else {
        false
    }
}

/// Take a connection out of the pool, preventing it from being recycled
///
/// This should be called when a connection encounters an error that indicates
/// it's broken and should not be reused.
pub fn remove_from_pool(conn: Object<TcpManager>) {
    drop(Object::take(conn));
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
    use std::io::Error as IoError;

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
}
