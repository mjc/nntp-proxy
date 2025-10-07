//! NNTP protocol handling module
//!
//! This module contains response parsing and protocol utilities for NNTP communication.

use anyhow::Result;
use std::net::SocketAddr;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tracing::debug;

mod response;

pub use response::ResponseParser;

// Re-export for future use
#[allow(unused_imports)]
pub use response::NntpResponse;

/// Send NNTP proxy greeting to a client
///
/// Sends the standard "200 NNTP Proxy Ready" greeting message.
pub async fn send_proxy_greeting(
    client_stream: &mut TcpStream,
    client_addr: SocketAddr,
) -> Result<()> {
    let proxy_greeting = b"200 NNTP Proxy Ready\r\n";
    client_stream.write_all(proxy_greeting).await?;
    debug!("Sent proxy greeting to client {}", client_addr);
    Ok(())
}
