//! Health check implementation for pooled connections
//!
//! This module provides health checking functionality for NNTP connections:
//! - TCP-level checks using non-blocking peek
//! - Application-level checks using DATE command
//! - Health check metrics tracking

use deadpool::managed;
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

use crate::constants::pool::{
    DATE_COMMAND, EXPECTED_DATE_RESPONSE_PREFIX, HEALTH_CHECK_BUFFER_SIZE, HEALTH_CHECK_TIMEOUT,
    TCP_PEEK_BUFFER_SIZE,
};
use crate::stream::ConnectionStream;

/// Errors that can occur during connection health checks
#[derive(Debug, Error)]
pub enum HealthCheckError {
    /// TCP connection is closed
    #[error("TCP connection closed")]
    TcpClosed,

    /// Unexpected data found in the buffer before health check
    #[error("Unexpected data in buffer")]
    UnexpectedData,

    /// TCP-level error occurred
    #[error("TCP error: {0}")]
    TcpError(std::io::Error),

    /// Failed to write DATE command to the connection
    #[error("Failed to write health check: {0}")]
    WriteError(std::io::Error),

    /// Failed to read response from the connection
    #[error("Failed to read health check response: {0}")]
    ReadError(std::io::Error),

    /// Health check operation timed out
    #[error("Health check timeout")]
    Timeout,

    /// Server returned unexpected response to DATE command
    #[error("Unexpected health check response: {0}")]
    UnexpectedResponse(String),

    /// Connection closed while waiting for health check response
    #[error("Connection closed during health check")]
    ConnectionClosedDuringCheck,
}

impl From<HealthCheckError> for managed::RecycleError<crate::connection_error::ConnectionError> {
    fn from(err: HealthCheckError) -> Self {
        managed::RecycleError::Message(err.to_string().into())
    }
}

/// Metrics for periodic health checks (lock-free)
#[derive(Debug, Default)]
pub struct HealthCheckMetrics {
    /// Total number of health check cycles run
    cycles_run: AtomicU64,
    /// Total number of connections checked
    connections_checked: AtomicU64,
    /// Total number of connections that failed health checks
    connections_failed: AtomicU64,
}

impl HealthCheckMetrics {
    /// Create a new metrics instance
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a health check cycle
    pub fn record_cycle(&self, checked: u64, failed: u64) {
        self.cycles_run.fetch_add(1, Ordering::Relaxed);
        self.connections_checked
            .fetch_add(checked, Ordering::Relaxed);
        self.connections_failed.fetch_add(failed, Ordering::Relaxed);
    }

    /// Get the failure rate (0.0 to 1.0)
    pub fn failure_rate(&self) -> f64 {
        let checked = self.connections_checked.load(Ordering::Relaxed);
        if checked == 0 {
            0.0
        } else {
            let failed = self.connections_failed.load(Ordering::Relaxed);
            failed as f64 / checked as f64
        }
    }

    /// Get total cycles run
    pub fn cycles_run(&self) -> u64 {
        self.cycles_run.load(Ordering::Relaxed)
    }

    /// Get total connections checked
    pub fn connections_checked(&self) -> u64 {
        self.connections_checked.load(Ordering::Relaxed)
    }

    /// Get total connections failed
    pub fn connections_failed(&self) -> u64 {
        self.connections_failed.load(Ordering::Relaxed)
    }
}

/// Fast TCP-level check for obviously dead connections
///
/// Uses non-blocking peek to detect closed connections without consuming data.
/// Only applicable to plain TCP connections; TLS connections skip this check.
///
/// # How it works
/// - `try_read()` attempts a non-blocking read of 1 byte
/// - `Ok(0)` means the connection is closed (EOF)
/// - `Ok(n)` means data is available (unexpected - should be idle)
/// - `Err(WouldBlock)` means no data available - this is **expected** for an idle,
///   healthy connection, as there should be no data to read between commands
/// - Other errors indicate TCP-level problems
pub fn check_tcp_alive(
    conn: &mut ConnectionStream,
) -> managed::RecycleResult<crate::connection_error::ConnectionError> {
    let mut peek_buf = [0u8; TCP_PEEK_BUFFER_SIZE];

    // Check the underlying TCP stream regardless of TLS/compression layers
    let tcp_stream = conn.underlying_tcp_stream();
    match tcp_stream.try_read(&mut peek_buf) {
        Ok(0) => return Err(HealthCheckError::TcpClosed.into()),
        Ok(_) => {
            // Data available on TCP socket that we haven't consumed — reject it
            return Err(HealthCheckError::UnexpectedData.into());
        }
        Err(e) if e.kind() != std::io::ErrorKind::WouldBlock => {
            return Err(
                HealthCheckError::TcpError(std::io::Error::new(e.kind(), e.to_string())).into(),
            );
        }
        // WouldBlock is the expected case - no data available on idle connection
        Err(_) => {}
    }

    Ok(())
}

/// Validate DATE command response
///
/// Returns Ok(()) if the response starts with "111 " (EXPECTED_DATE_RESPONSE_PREFIX),
/// otherwise returns an error with the actual response.
///
/// This is a pure function extracted for testability.
#[inline]
pub(crate) fn validate_date_response(response: &str) -> Result<(), HealthCheckError> {
    if response.starts_with(EXPECTED_DATE_RESPONSE_PREFIX) {
        Ok(())
    } else {
        Err(HealthCheckError::UnexpectedResponse(response.to_string()))
    }
}

/// Application-level health check using DATE command
///
/// Sends DATE command and verifies response to ensure the NNTP connection
/// is still functional. This detects server-side timeouts that TCP keepalive
/// might miss.
pub async fn check_date_response(conn: &mut ConnectionStream) -> Result<(), HealthCheckError> {
    // Wrap entire health check in single timeout
    let health_check = async {
        // Send DATE command
        conn.write_all(DATE_COMMAND)
            .await
            .map_err(HealthCheckError::WriteError)?;

        // Read response
        let mut response_buf = [0u8; HEALTH_CHECK_BUFFER_SIZE];
        let n = conn
            .read(&mut response_buf)
            .await
            .map_err(HealthCheckError::ReadError)?;

        if n == 0 {
            return Err(HealthCheckError::ConnectionClosedDuringCheck);
        }

        // Validate DATE response
        let response = String::from_utf8_lossy(&response_buf[..n]);
        validate_date_response(&response)
    };

    // Apply timeout and convert errors
    timeout(HEALTH_CHECK_TIMEOUT, health_check)
        .await
        .map_err(|_| HealthCheckError::Timeout)?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_health_check_metrics_new() {
        let metrics = HealthCheckMetrics::new();
        assert_eq!(metrics.cycles_run(), 0);
        assert_eq!(metrics.connections_checked(), 0);
        assert_eq!(metrics.connections_failed(), 0);
        assert_eq!(metrics.failure_rate(), 0.0);
    }

    #[test]
    fn test_health_check_metrics_record_cycle() {
        let metrics = HealthCheckMetrics::new();

        metrics.record_cycle(10, 2);
        assert_eq!(metrics.cycles_run(), 1);
        assert_eq!(metrics.connections_checked(), 10);
        assert_eq!(metrics.connections_failed(), 2);
        assert_eq!(metrics.failure_rate(), 0.2);

        metrics.record_cycle(5, 1);
        assert_eq!(metrics.cycles_run(), 2);
        assert_eq!(metrics.connections_checked(), 15);
        assert_eq!(metrics.connections_failed(), 3);
        assert_eq!(metrics.failure_rate(), 0.2);
    }

    #[test]
    fn test_health_check_metrics_failure_rate() {
        let metrics = HealthCheckMetrics::new();

        // No failures
        metrics.record_cycle(10, 0);
        assert_eq!(metrics.failure_rate(), 0.0);

        // 50% failure rate
        metrics.record_cycle(10, 5);
        assert!((metrics.failure_rate() - 0.25).abs() < 0.01);

        // 100% failure rate cycle
        metrics.record_cycle(10, 10);
        assert!((metrics.failure_rate() - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_health_check_metrics_zero_checked() {
        let metrics = HealthCheckMetrics::new();
        assert_eq!(metrics.failure_rate(), 0.0);
    }

    #[test]
    fn test_health_check_metrics_multiple_cycles() {
        let metrics = HealthCheckMetrics::new();

        for i in 1..=5 {
            metrics.record_cycle(10, 1);
            assert_eq!(metrics.cycles_run(), i);
        }

        assert_eq!(metrics.connections_checked(), 50);
        assert_eq!(metrics.connections_failed(), 5);
        assert_eq!(metrics.failure_rate(), 0.1);
    }

    #[test]
    fn test_health_check_error_display() {
        assert_eq!(
            HealthCheckError::TcpClosed.to_string(),
            "TCP connection closed"
        );
        assert_eq!(
            HealthCheckError::UnexpectedData.to_string(),
            "Unexpected data in buffer"
        );
        assert_eq!(
            HealthCheckError::Timeout.to_string(),
            "Health check timeout"
        );
        assert_eq!(
            HealthCheckError::ConnectionClosedDuringCheck.to_string(),
            "Connection closed during health check"
        );
    }

    #[test]
    fn test_health_check_error_unexpected_response() {
        let err = HealthCheckError::UnexpectedResponse("500 Error".to_string());
        assert_eq!(
            err.to_string(),
            "Unexpected health check response: 500 Error"
        );
    }

    #[test]
    fn test_health_check_error_tcp_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        let err = HealthCheckError::TcpError(io_err);
        assert!(err.to_string().contains("TCP error"));
        assert!(err.to_string().contains("reset"));
    }

    #[test]
    fn test_health_check_error_write_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe");
        let err = HealthCheckError::WriteError(io_err);
        assert!(err.to_string().contains("Failed to write health check"));
    }

    #[test]
    fn test_health_check_error_read_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout");
        let err = HealthCheckError::ReadError(io_err);
        assert!(
            err.to_string()
                .contains("Failed to read health check response")
        );
    }

    // DATE response validation tests

    #[test]
    fn test_validate_date_response_success() {
        // Standard DATE response format
        assert!(validate_date_response("111 20231215120000\r\n").is_ok());
    }

    #[test]
    fn test_validate_date_response_success_minimal() {
        // Minimal valid response (just "111 " prefix)
        assert!(validate_date_response("111 \r\n").is_ok());
    }

    #[test]
    fn test_validate_date_response_success_with_extra() {
        // Extra data after timestamp is OK
        assert!(validate_date_response("111 20231215120000 extra info\r\n").is_ok());
    }

    #[test]
    fn test_validate_date_response_wrong_code() {
        // Wrong status code
        let result = validate_date_response("200 OK\r\n");
        assert!(result.is_err());
        match result {
            Err(HealthCheckError::UnexpectedResponse(msg)) => {
                assert_eq!(msg, "200 OK\r\n");
            }
            _ => panic!("Expected UnexpectedResponse error"),
        }
    }

    #[test]
    fn test_validate_date_response_error_code() {
        // Error codes (4xx, 5xx) should fail
        assert!(validate_date_response("400 Bad Request\r\n").is_err());
        assert!(validate_date_response("500 Server Error\r\n").is_err());
    }

    #[test]
    fn test_validate_date_response_empty() {
        // Empty response
        let result = validate_date_response("");
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_date_response_malformed() {
        // Malformed responses
        assert!(validate_date_response("not a valid response").is_err());
        assert!(validate_date_response("1\r\n").is_err());
        assert!(validate_date_response("11 \r\n").is_err()); // Too short prefix
    }

    #[test]
    fn test_validate_date_response_partial_match() {
        // Starts with "11" but not "111 "
        assert!(validate_date_response("110 Info\r\n").is_err());
        assert!(validate_date_response("112 Other\r\n").is_err());
    }

    #[test]
    fn test_validate_date_response_no_space() {
        // Missing space after "111"
        assert!(validate_date_response("11120231215120000\r\n").is_err());
    }

    #[test]
    fn test_validate_date_response_whitespace_prefix() {
        // Leading whitespace should fail
        assert!(validate_date_response(" 111 20231215120000\r\n").is_err());
        assert!(validate_date_response("\r\n111 20231215120000\r\n").is_err());
    }

    #[test]
    fn test_validate_date_response_case_sensitivity() {
        // Status codes are numeric, so no case issues, but test weird inputs
        assert!(validate_date_response("111 lowercase\r\n").is_ok());
        assert!(validate_date_response("111 UPPERCASE\r\n").is_ok());
    }

    #[test]
    fn test_validate_date_response_unicode() {
        // Unicode in timestamp portion (unusual but should work if starts with "111 ")
        assert!(validate_date_response("111 日本語\r\n").is_ok());
    }

    #[test]
    fn test_validate_date_response_realistic_examples() {
        // Real-world examples from different NNTP servers
        assert!(validate_date_response("111 20231215120530\r\n").is_ok());
        assert!(validate_date_response("111 19700101000000\r\n").is_ok());
        assert!(validate_date_response("111 20991231235959\r\n").is_ok());
    }
}
