//! Network socket optimization utilities
//!
//! This module provides utilities for optimizing TCP socket performance
//! for high-throughput NNTP transfers.
//!
use anyhow::{Context, Result};
use socket2::SockRef;
use std::time::Duration;
use tokio::net::TcpStream;
use tracing::debug;

/// `SO_LINGER` timeout - prevents indefinite blocking on socket close
const LINGER_TIMEOUT: Duration = Duration::from_secs(5);

/// `TCP_USER_TIMEOUT` - faster dead connection detection on Linux
#[cfg(target_os = "linux")]
const TCP_USER_TIMEOUT: Duration = Duration::from_secs(30);

/// `IP_TOS` value for throughput optimization
#[cfg(target_os = "linux")]
const TOS_THROUGHPUT: u32 = 0x08;

/// Apply core TCP optimizations to a socket reference
fn apply_core_optimizations(
    sock_ref: &SockRef,
    recv_buffer_size: usize,
    send_buffer_size: usize,
) -> Result<()> {
    if recv_buffer_size > 0 {
        sock_ref
            .set_recv_buffer_size(recv_buffer_size)
            .context("Failed to set TCP receive buffer size")?;
    }

    if send_buffer_size > 0 {
        sock_ref
            .set_send_buffer_size(send_buffer_size)
            .context("Failed to set TCP send buffer size")?;
    }

    sock_ref
        .set_linger(Some(LINGER_TIMEOUT))
        .context("Failed to set SO_LINGER timeout")?;

    sock_ref
        .set_tcp_nodelay(true)
        .context("Failed to set TCP_NODELAY")?;

    Ok(())
}

/// Apply Linux-specific TCP optimizations (best-effort)
#[cfg(target_os = "linux")]
fn apply_linux_optimizations(sock_ref: &SockRef, context: &str) {
    [
        (
            "TCP_USER_TIMEOUT",
            sock_ref.set_tcp_user_timeout(Some(TCP_USER_TIMEOUT)),
        ),
        ("IP_TOS", sock_ref.set_tos_v4(TOS_THROUGHPUT)),
    ]
    .into_iter()
    .filter_map(|(name, result)| result.err().map(|e| (name, e)))
    .for_each(|(name, err)| {
        debug!("Failed to set {} on {}: {}", name, context, err);
    });
}

/// Get platform-specific optimization description
const fn platform_optimization_desc() -> &'static str {
    match () {
        #[cfg(target_os = "linux")]
        () => ", tcp_user_timeout=30s, tos=0x08",
        #[cfg(target_os = "windows")]
        () => " (Windows)",
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        () => "",
    }
}

/// Apply TCP optimizations for high-throughput client sockets.
pub(crate) fn optimize_tcp_stream(
    stream: &TcpStream,
    recv_buffer_size: usize,
    send_buffer_size: usize,
) -> Result<()> {
    let sock_ref = SockRef::from(stream);

    apply_core_optimizations(&sock_ref, recv_buffer_size, send_buffer_size)
        .context("Failed to apply core TCP optimizations")?;

    #[cfg(target_os = "linux")]
    apply_linux_optimizations(&sock_ref, "TCP stream");

    debug!(
        "Applied TCP optimizations: recv_buffer={}, send_buffer={}, linger={}s, nodelay=true{}",
        recv_buffer_size,
        send_buffer_size,
        LINGER_TIMEOUT.as_secs(),
        platform_optimization_desc()
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::socket::{HIGH_THROUGHPUT_RECV_BUFFER, HIGH_THROUGHPUT_SEND_BUFFER};
    use tokio::net::TcpListener;

    #[test]
    fn test_buffer_sizes_are_equal() {
        assert_eq!(HIGH_THROUGHPUT_RECV_BUFFER, HIGH_THROUGHPUT_SEND_BUFFER);
    }

    #[test]
    fn test_platform_optimization_desc() {
        let desc = platform_optimization_desc();
        #[cfg(target_os = "linux")]
        assert!(desc.contains("tcp_user_timeout"));
        #[cfg(target_os = "windows")]
        assert!(desc.contains("Windows"));
        // Just verify it doesn't panic on other platforms
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        let _ = desc;
    }

    #[tokio::test]
    async fn test_optimize_tcp_stream_accepts_zero_buffers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();

        optimize_tcp_stream(&stream, 0, 0).unwrap();
    }
}
