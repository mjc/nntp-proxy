//! Stale-connection retry logic
//!
//! Provides a shared pattern for retrying operations that may fail due to stale
//! pooled connections. Both command execution and precheck use this pattern:
//! call once → on error → log → jittered sleep → retry once → return original error if retry fails.
//!
//! The jittered delay (10–59ms) prevents thundering-herd retries when multiple
//! connections go stale simultaneously (e.g. backend restart). Fail fast after
//! one retry — let the router try a different backend.
//!
//! Uses a macro rather than a generic async function because callsites capture
//! `&mut` references that cannot escape `FnMut` closure bodies.

/// Retry an async expression once on failure (stale connection recovery).
///
/// Evaluates `$expr` once. On `Err`, logs at debug level using `$label`,
/// then evaluates `$expr` a second time. If the retry also fails, returns
/// the **original** error from the first attempt.
///
/// # Usage
///
/// ```ignore
/// let result = retry_once_on_stale!("backend 3", {
///     self.execute_backend_attempt(provider, id, cmd, buf).await
/// });
/// ```
///
/// The expression is evaluated twice at most, sequentially, so mutable borrows
/// that span the `.await` work correctly — unlike an `FnMut` closure.
macro_rules! retry_once_on_stale {
    ($label:expr, $expr:expr) => {{
        match $expr {
            Ok(val) => Ok(val),
            Err(first_error) => {
                tracing::debug!(
                    "Stale connection to {}, retrying with fresh connection",
                    $label
                );

                // Jittered backoff: 10–59ms prevents thundering-herd retries
                tokio::time::sleep(std::time::Duration::from_millis(
                    10 + rand::random::<u64>() % 50,
                ))
                .await;

                match $expr {
                    Ok(val) => Ok(val),
                    Err(_retry_error) => Err(first_error),
                }
            }
        }
    }};
}

pub(crate) use retry_once_on_stale;

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    async fn fallible_op(
        counter: &AtomicU32,
        fail_first_n: u32,
    ) -> Result<&'static str, &'static str> {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        if n < fail_first_n {
            Err("stale")
        } else {
            Ok("success")
        }
    }

    #[tokio::test]
    async fn test_succeeds_on_first_try() {
        let counter = AtomicU32::new(0);
        let result = retry_once_on_stale!("test", fallible_op(&counter, 0).await);
        assert_eq!(result, Ok("success"));
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_succeeds_on_retry() {
        let counter = AtomicU32::new(0);
        let result = retry_once_on_stale!("test", fallible_op(&counter, 1).await);
        assert_eq!(result, Ok("success"));
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_returns_first_error_on_double_failure() {
        let counter = AtomicU32::new(0);

        // Both calls fail — we need different error values to prove first is returned.
        // Use a slightly different helper for this test.
        async fn numbered_fail(c: &AtomicU32) -> Result<(), String> {
            let n = c.fetch_add(1, Ordering::SeqCst);
            Err(format!("error {}", n))
        }

        let result = retry_once_on_stale!("test", numbered_fail(&counter).await);
        assert_eq!(result, Err("error 0".to_string()));
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }
}
