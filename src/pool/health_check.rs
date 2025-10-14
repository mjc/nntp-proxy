//! Health check implementation for pooled connections
//!
//! This module provides health checking functionality for NNTP connections:
//! - TCP-level checks using non-blocking peek
//! - Application-level checks using DATE command
//! - Health check metrics tracking

use deadpool::managed;
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

impl From<HealthCheckError> for managed::RecycleError<anyhow::Error> {
    fn from(err: HealthCheckError) -> Self {
        managed::RecycleError::Message(err.to_string().into())
    }
}

/// Metrics for periodic health checks
#[derive(Debug, Default, Clone)]
pub struct HealthCheckMetrics {
    /// Total number of health check cycles run
    pub cycles_run: u64,
    /// Total number of connections checked
    pub connections_checked: u64,
    /// Total number of connections that failed health checks
    pub connections_failed: u64,
    /// Last check timestamp
    pub last_check_time: Option<std::time::Instant>,
}

impl HealthCheckMetrics {
    /// Create a new metrics instance
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a health check cycle
    pub fn record_cycle(&mut self, checked: u64, failed: u64) {
        self.cycles_run += 1;
        self.connections_checked += checked;
        self.connections_failed += failed;
        self.last_check_time = Some(std::time::Instant::now());
    }

    /// Get the failure rate (0.0 to 1.0)
    pub fn failure_rate(&self) -> f64 {
        if self.connections_checked == 0 {
            0.0
        } else {
            self.connections_failed as f64 / self.connections_checked as f64
        }
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
pub fn check_tcp_alive(conn: &mut ConnectionStream) -> managed::RecycleResult<anyhow::Error> {
    let mut peek_buf = [0u8; TCP_PEEK_BUFFER_SIZE];

    match conn {
        ConnectionStream::Plain(tcp) => {
            match tcp.try_read(&mut peek_buf) {
                Ok(0) => return Err(HealthCheckError::TcpClosed.into()),
                Ok(_) => return Err(HealthCheckError::UnexpectedData.into()),
                Err(e) if e.kind() != std::io::ErrorKind::WouldBlock => {
                    // Clone to preserve original error details and message
                    return Err(HealthCheckError::TcpError(std::io::Error::new(
                        e.kind(),
                        e.to_string(),
                    ))
                    .into());
                }
                // WouldBlock is the expected case - no data available on idle connection
                Err(_) => {}
            }
        }
        ConnectionStream::Tls(tls) => {
            // For TLS connections, check the underlying TCP socket for readable data
            // We can't use try_read on TlsStream directly, so we check the inner TCP stream
            let (tcp_stream, _) = tls.get_ref();
            match tcp_stream.try_read(&mut peek_buf) {
                Ok(0) => return Err(HealthCheckError::TcpClosed.into()),
                Ok(_) => {
                    // Data available on TCP socket - could be TLS records or buffered application data
                    // Either way, this connection has data we haven't consumed, so reject it
                    return Err(HealthCheckError::UnexpectedData.into());
                }
                Err(e) if e.kind() != std::io::ErrorKind::WouldBlock => {
                    return Err(HealthCheckError::TcpError(std::io::Error::new(
                        e.kind(),
                        e.to_string(),
                    ))
                    .into());
                }
                // WouldBlock is the expected case - no data available on idle connection
                Err(_) => {}
            }
        }
    }

    Ok(())
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
        if response.starts_with(EXPECTED_DATE_RESPONSE_PREFIX) {
            Ok(())
        } else {
            Err(HealthCheckError::UnexpectedResponse(response.to_string()))
        }
    };

    // Apply timeout and convert errors
    timeout(HEALTH_CHECK_TIMEOUT, health_check)
        .await
        .map_err(|_| HealthCheckError::Timeout)?
}
