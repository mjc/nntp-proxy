//! Comprehensive health check tests
//!
//! Tests for connection health checking functionality including:
//! - TCP alive checks (plain and TLS)
//! - DATE command health checks
//! - Timeout scenarios
//! - Health check metrics
//! - Error handling

use anyhow::Result;
use nntp_proxy::pool::health_check::{HealthCheckMetrics, check_date_response, check_tcp_alive};
use nntp_proxy::stream::ConnectionStream;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

mod test_helpers;

/// Test that a healthy connection passes TCP alive check
#[tokio::test]
async fn test_tcp_alive_check_healthy_connection() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;

    // Spawn a server that accepts but doesn't send data
    let server_handle = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.unwrap();
        // Keep connection open but idle
        tokio::time::sleep(Duration::from_secs(1)).await;
    });

    // Connect and check health
    let stream = TcpStream::connect(addr).await?;
    let mut conn_stream = ConnectionStream::Plain(stream);

    // Should pass - connection is alive and idle
    let result = check_tcp_alive(&mut conn_stream);
    assert!(
        result.is_ok(),
        "Healthy connection should pass TCP alive check"
    );

    drop(conn_stream);
    server_handle.abort();
    Ok(())
}

/// Test that a closed connection fails TCP alive check
#[tokio::test]
async fn test_tcp_alive_check_closed_connection() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;

    // Server that immediately closes connection
    let server_handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        drop(stream); // Close immediately
    });

    let stream = TcpStream::connect(addr).await?;

    // Wait for server to close
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut conn_stream = ConnectionStream::Plain(stream);

    // Should fail - connection is closed
    let result = check_tcp_alive(&mut conn_stream);
    assert!(
        result.is_err(),
        "Closed connection should fail TCP alive check"
    );

    server_handle.abort();
    Ok(())
}

/// Test that a connection with unexpected data fails TCP alive check
#[tokio::test]
async fn test_tcp_alive_check_unexpected_data() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;

    // Server that sends unexpected data
    let server_handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        // Send data immediately - unexpected for idle connection
        stream.write_all(b"UNEXPECTED DATA\r\n").await.unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
    });

    let stream = TcpStream::connect(addr).await?;

    // Give server time to send data
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut conn_stream = ConnectionStream::Plain(stream);

    // Should fail - unexpected data in buffer
    let result = check_tcp_alive(&mut conn_stream);
    assert!(
        result.is_err(),
        "Connection with unexpected data should fail"
    );

    server_handle.abort();
    Ok(())
}

/// Test DATE command health check with valid response
#[tokio::test]
async fn test_date_health_check_success() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;

    // Server that responds to DATE command
    let server_handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();

        // Read DATE command
        let mut buf = [0u8; 64];
        let n = stream.read(&mut buf).await.unwrap();
        let cmd = String::from_utf8_lossy(&buf[..n]);

        assert!(cmd.contains("DATE"), "Expected DATE command");

        // Send valid DATE response
        stream.write_all(b"111 20251112120000\r\n").await.unwrap();
    });

    let stream = TcpStream::connect(addr).await?;
    let mut conn_stream = ConnectionStream::Plain(stream);

    // Should succeed
    let result = check_date_response(&mut conn_stream).await;
    assert!(
        result.is_ok(),
        "Valid DATE response should pass health check"
    );

    server_handle.abort();
    Ok(())
}

/// Test DATE command health check with invalid response
#[tokio::test]
async fn test_date_health_check_invalid_response() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;

    // Server that sends wrong response code
    let server_handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();

        let mut buf = [0u8; 64];
        let _ = stream.read(&mut buf).await.unwrap();

        // Send invalid response
        stream
            .write_all(b"500 Command not recognized\r\n")
            .await
            .unwrap();
    });

    let stream = TcpStream::connect(addr).await?;
    let mut conn_stream = ConnectionStream::Plain(stream);

    // Should fail - wrong response code
    let result = check_date_response(&mut conn_stream).await;
    assert!(
        result.is_err(),
        "Invalid DATE response should fail health check"
    );

    server_handle.abort();
    Ok(())
}

/// Test DATE command health check timeout
#[tokio::test]
async fn test_date_health_check_timeout() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;

    // Server that never responds
    let server_handle = tokio::spawn(async move {
        let (mut _stream, _) = listener.accept().await.unwrap();
        // Read command but never respond
        let mut buf = [0u8; 64];
        let _ = _stream.read(&mut buf).await;
        // Sleep longer than health check timeout
        tokio::time::sleep(Duration::from_secs(10)).await;
    });

    let stream = TcpStream::connect(addr).await?;
    let mut conn_stream = ConnectionStream::Plain(stream);

    // Should timeout
    let result = timeout(
        Duration::from_secs(2),
        check_date_response(&mut conn_stream),
    )
    .await;
    assert!(
        result.is_err() || result.unwrap().is_err(),
        "Health check should timeout when server doesn't respond"
    );

    server_handle.abort();
    Ok(())
}

/// Test DATE command health check with connection closed during check
#[tokio::test]
async fn test_date_health_check_connection_closed() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;

    // Server that closes connection after receiving command
    let server_handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 64];
        let _ = stream.read(&mut buf).await;
        // Close without responding
        drop(stream);
    });

    let stream = TcpStream::connect(addr).await?;
    let mut conn_stream = ConnectionStream::Plain(stream);

    // Should fail - connection closed
    let result = check_date_response(&mut conn_stream).await;
    assert!(
        result.is_err(),
        "Closed connection should fail health check"
    );

    server_handle.abort();
    Ok(())
}

/// Test health check metrics recording
#[test]
fn test_health_check_metrics_initialization() {
    let metrics = HealthCheckMetrics::new();

    assert_eq!(metrics.cycles_run, 0);
    assert_eq!(metrics.connections_checked, 0);
    assert_eq!(metrics.connections_failed, 0);
    assert!(metrics.last_check_time.is_none());
    assert_eq!(metrics.failure_rate(), 0.0);
}

/// Test health check metrics recording a cycle
#[test]
fn test_health_check_metrics_record_cycle() {
    let mut metrics = HealthCheckMetrics::new();

    metrics.record_cycle(10, 2);

    assert_eq!(metrics.cycles_run, 1);
    assert_eq!(metrics.connections_checked, 10);
    assert_eq!(metrics.connections_failed, 2);
    assert!(metrics.last_check_time.is_some());
    assert_eq!(metrics.failure_rate(), 0.2);
}

/// Test health check metrics multiple cycles
#[test]
fn test_health_check_metrics_multiple_cycles() {
    let mut metrics = HealthCheckMetrics::new();

    metrics.record_cycle(10, 1);
    metrics.record_cycle(5, 0);
    metrics.record_cycle(8, 2);

    assert_eq!(metrics.cycles_run, 3);
    assert_eq!(metrics.connections_checked, 23);
    assert_eq!(metrics.connections_failed, 3);

    // 3 failures out of 23 checks
    let expected_rate = 3.0 / 23.0;
    assert!((metrics.failure_rate() - expected_rate).abs() < 0.001);
}

/// Test health check metrics failure rate edge cases
#[test]
fn test_health_check_metrics_failure_rate_edge_cases() {
    let mut metrics = HealthCheckMetrics::new();

    // No checks yet
    assert_eq!(metrics.failure_rate(), 0.0);

    // All pass
    metrics.record_cycle(10, 0);
    assert_eq!(metrics.failure_rate(), 0.0);

    // All fail
    metrics = HealthCheckMetrics::new();
    metrics.record_cycle(5, 5);
    assert_eq!(metrics.failure_rate(), 1.0);
}

/// Test health check metrics with only failures
#[test]
fn test_health_check_metrics_all_failures() {
    let mut metrics = HealthCheckMetrics::new();

    metrics.record_cycle(10, 10);
    metrics.record_cycle(5, 5);

    assert_eq!(metrics.cycles_run, 2);
    assert_eq!(metrics.connections_checked, 15);
    assert_eq!(metrics.connections_failed, 15);
    assert_eq!(metrics.failure_rate(), 1.0);
}

/// Test health check metrics clone
#[test]
fn test_health_check_metrics_clone() {
    let mut metrics = HealthCheckMetrics::new();
    metrics.record_cycle(10, 3);

    let cloned = metrics.clone();

    assert_eq!(cloned.cycles_run, metrics.cycles_run);
    assert_eq!(cloned.connections_checked, metrics.connections_checked);
    assert_eq!(cloned.connections_failed, metrics.connections_failed);
    assert_eq!(cloned.failure_rate(), metrics.failure_rate());
}

/// Test DATE command is correctly formatted
#[tokio::test]
async fn test_date_command_format() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;

    // Server that captures the command
    let server_handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();

        let mut buf = [0u8; 64];
        let n = stream.read(&mut buf).await.unwrap();
        let cmd = String::from_utf8_lossy(&buf[..n]);

        // Verify command format
        assert_eq!(
            cmd.trim(),
            "DATE",
            "DATE command should be formatted correctly"
        );

        stream.write_all(b"111 20251112120000\r\n").await.unwrap();
    });

    let stream = TcpStream::connect(addr).await?;
    let mut conn_stream = ConnectionStream::Plain(stream);

    let _ = check_date_response(&mut conn_stream).await;

    server_handle.await?;
    Ok(())
}

/// Test multiple sequential health checks on same connection
#[tokio::test]
async fn test_multiple_sequential_health_checks() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;

    // Server that responds to multiple DATE commands
    let server_handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();

        for _ in 0..3 {
            let mut buf = [0u8; 64];
            let n = stream.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }

            stream.write_all(b"111 20251112120000\r\n").await.unwrap();
        }
    });

    let stream = TcpStream::connect(addr).await?;
    let mut conn_stream = ConnectionStream::Plain(stream);

    // Multiple checks should all pass
    for i in 0..3 {
        let result = check_date_response(&mut conn_stream).await;
        assert!(result.is_ok(), "Health check {} should pass", i + 1);
    }

    server_handle.abort();
    Ok(())
}

/// Test health check with partial response
#[tokio::test]
async fn test_date_health_check_partial_response() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;

    // Server that sends incomplete response
    let server_handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();

        let mut buf = [0u8; 64];
        let _ = stream.read(&mut buf).await.unwrap();

        // Send incomplete response (no CRLF)
        stream.write_all(b"111").await.unwrap();
        tokio::time::sleep(Duration::from_secs(10)).await;
    });

    let stream = TcpStream::connect(addr).await?;
    let mut conn_stream = ConnectionStream::Plain(stream);

    // Should eventually timeout or fail
    let result = timeout(
        Duration::from_secs(2),
        check_date_response(&mut conn_stream),
    )
    .await;
    assert!(
        result.is_err() || result.unwrap().is_err(),
        "Partial response should fail or timeout"
    );

    server_handle.abort();
    Ok(())
}

/// Test DATE command health check with malformed DATE response
#[tokio::test]
async fn test_date_health_check_malformed_response() -> Result<()> {
    let test_cases = &[
        b"INVALID\r\n".as_slice(),
        b"200 OK\r\n".as_slice(),
        b"111\r\n".as_slice(), // Missing timestamp
        b"\r\n".as_slice(),    // Empty
    ];

    for (i, response) in test_cases.iter().enumerate() {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let response = response.to_vec();

        let server_handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 64];
            let _ = stream.read(&mut buf).await.unwrap();
            stream.write_all(&response).await.unwrap();
        });

        let stream = TcpStream::connect(addr).await?;
        let mut conn_stream = ConnectionStream::Plain(stream);

        let result = check_date_response(&mut conn_stream).await;
        assert!(result.is_err(), "Malformed response {} should fail", i);

        server_handle.abort();
    }

    Ok(())
}

/// Test TCP alive check doesn't consume data
#[tokio::test]
async fn test_tcp_alive_check_non_destructive() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;

    // Spawn a simple server
    let server_handle = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
    });

    let stream = TcpStream::connect(addr).await?;
    let mut conn_stream = ConnectionStream::Plain(stream);

    // Check health
    let result = check_tcp_alive(&mut conn_stream);
    assert!(result.is_ok());

    // Try to write data - connection should still work
    if let ConnectionStream::Plain(ref mut tcp) = conn_stream {
        let write_result = tcp.write_all(b"TEST\r\n").await;
        assert!(
            write_result.is_ok(),
            "Connection should still be writable after health check"
        );
    }

    server_handle.abort();
    Ok(())
}
