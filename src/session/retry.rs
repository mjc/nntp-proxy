//! Backend error retry logic
//!
//! Provides a shared pattern for retrying operations that fail on first attempt.
//! Both command execution and precheck use this pattern:
//! call once → on error → log → jittered sleep → retry once → return original error if retry fails.
//!
//! The jittered delay (1–9ms) prevents thundering-herd retries when multiple
//! connections fail simultaneously (e.g. backend restart). Fail fast after
//! one retry — let the router try a different backend.
//!
//! Uses a macro rather than a generic async function because callsites capture
//! `&mut` references that cannot escape `FnMut` closure bodies.

/// Retry an async expression once on failure.
///
/// Evaluates `$expr` once. On `Err`, logs at warn level with optional context,
/// then evaluates `$expr` a second time. If the retry also fails, returns
/// the **original** error from the first attempt.
///
/// # Usage
///
/// ```ignore
/// // With context fields (lazy evaluation, zero allocation on success path)
/// let result = retry_once!(
///     self.execute_backend_attempt(provider, id, cmd, buf).await,
///     client = self.client_addr,
///     backend = backend_id.as_index()
/// );
///
/// // Without context (tests)
/// let result = retry_once!(fallible_op(&counter).await);
/// ```
///
/// The expression is evaluated twice at most, sequentially, so mutable borrows
/// that span the `.await` work correctly — unlike an `FnMut` closure.
macro_rules! retry_once {
    ($expr:expr $(, $key:ident = $val:expr)* $(,)?) => {{
        match $expr {
            Ok(val) => Ok(val),
            Err(first_error) => {
                tracing::warn!($($key = %$val,)* error = ?first_error, "Backend error, retrying once");

                // Jittered backoff: 1–9ms prevents thundering-herd retries
                let jitter_ms = rand::Rng::random_range(&mut rand::rng(), 1..10);
                tokio::time::sleep(std::time::Duration::from_millis(jitter_ms)).await;

                match $expr {
                    Ok(val) => Ok(val),
                    Err(_retry_error) => {
                        tracing::warn!($($key = %$val,)* retry_error = ?_retry_error, "Retry also failed");
                        Err(first_error)
                    }
                }
            }
        }
    }};
}

pub(crate) use retry_once;

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    fn numbered_fail(c: &AtomicU32) -> Result<(), String> {
        let n = c.fetch_add(1, Ordering::SeqCst);
        Err(format!("error {n}"))
    }

    fn fallible_op(counter: &AtomicU32, fail_first_n: u32) -> Result<&'static str, &'static str> {
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
        let result = retry_once!(fallible_op(&counter, 0));
        assert_eq!(result, Ok("success"));
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn test_succeeds_on_retry() {
        let counter = AtomicU32::new(0);
        let result = retry_once!(fallible_op(&counter, 1));
        assert_eq!(result, Ok("success"));
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn test_returns_first_error_on_double_failure() {
        let counter = AtomicU32::new(0);
        let result = retry_once!(numbered_fail(&counter));
        assert_eq!(result, Err("error 0".to_string()));
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_jitter_range() {
        // Verify jitter is in expected range (1-9ms)
        for _ in 0..100 {
            let jitter_ms = rand::Rng::random_range(&mut rand::rng(), 1..10);
            assert!(
                (1..10).contains(&jitter_ms),
                "Jitter {jitter_ms} out of range [1, 10)",
            );
        }
    }
}
