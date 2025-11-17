//! NNTP protocol handling module
//!
//! This module contains response parsing and protocol utilities for NNTP communication.

use anyhow::Result;
use std::net::SocketAddr;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tracing::debug;

mod commands;
mod response;
mod responses;

pub use response::ResponseParser;

// Re-export for future use
#[allow(unused_imports)]
pub use response::{NntpResponse, Response, StatusCode};

// Re-export command construction helpers
pub use commands::{
    DATE, QUIT, article_by_msgid, authinfo_pass, authinfo_user, body_by_msgid, head_by_msgid,
    stat_by_msgid,
};

// Re-export response constants and helpers
pub use responses::{
    AUTH_ACCEPTED, AUTH_FAILED, AUTH_REQUIRED, AUTH_REQUIRED_FOR_COMMAND, BACKEND_ERROR,
    BACKEND_UNAVAILABLE, COMMAND_NOT_SUPPORTED, COMMAND_NOT_SUPPORTED_STATELESS,
    CONNECTION_CLOSING, CRLF, GOODBYE, MIN_RESPONSE_LENGTH, MULTILINE_TERMINATOR,
    PROXY_GREETING_PCR, TERMINATOR_TAIL_SIZE, error_response, greeting, greeting_readonly,
    ok_response, response,
};

/// Send NNTP proxy greeting to a client
///
/// Sends the standard "200 NNTP Proxy Ready" greeting message.
/// The greeting is flushed immediately to ensure the client receives it
/// before we start processing commands.
pub async fn send_proxy_greeting(
    client_stream: &mut TcpStream,
    client_addr: SocketAddr,
) -> Result<()> {
    let proxy_greeting = b"200 NNTP Proxy Ready\r\n";
    client_stream.write_all(proxy_greeting).await?;
    client_stream.flush().await?;
    debug!("Sent and flushed proxy greeting to client {}", client_addr);
    Ok(())
}
