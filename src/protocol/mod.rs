//! NNTP protocol handling module
//!
//! This module contains response parsing and protocol utilities for NNTP communication.

use anyhow::Result;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tracing::debug;

mod article;
pub mod codes;
mod commands;
mod request;
mod response;
mod responses;

// Re-export article parsing types
pub use article::{Article, HeaderIter, Headers, ParseError, yenc};

// Re-export response types and utilities
pub use request::{
    RequestCacheArticleNumber, RequestCacheAvailability, RequestCacheEntryMetadata,
    RequestCachePayloadKind, RequestCacheStatus, RequestCacheTier, RequestCacheTimestampMillis,
    RequestContext, RequestKind, RequestResponseMetadata, RequestRouteClass, RequestWireLen,
    ResponseShape, ResponseWireLen,
};
pub use response::{NntpResponse, StatusCode};

// Re-export command construction helpers
pub use commands::{
    COMPRESS_DEFLATE, QUIT, article_request, authinfo_pass, authinfo_user, body_request,
    date_request, head_request, stat_request,
};

// Re-export response constants and helpers
pub use responses::{
    AUTH_ACCEPTED, AUTH_ALREADY_AUTHENTICATED, AUTH_FAILED, AUTH_OUT_OF_SEQUENCE, AUTH_REQUIRED,
    AUTH_REQUIRED_FOR_COMMAND, AUTH_UNKNOWN_SUBCOMMAND, BACKEND_ERROR, BACKEND_UNAVAILABLE,
    CAPABILITIES_WITH_AUTHINFO, CAPABILITIES_WITHOUT_AUTHINFO, COMMAND_TOO_LONG,
    CONNECTION_CLOSING, CRLF, GOODBYE, MIN_RESPONSE_LENGTH, NO_SUCH_ARTICLE, POSTING_NOT_PERMITTED,
    PROXY_GREETING_PCR, TERMINATOR_TAIL_SIZE, error_response, greeting, greeting_readonly,
    ok_response, response,
};

/// Send NNTP proxy greeting to a client
///
/// Sends the "201 NNTP Proxy Ready" greeting message.
/// The greeting is flushed immediately to ensure the client receives it
/// before we start processing commands.
pub async fn send_proxy_greeting(
    client_stream: &mut TcpStream,
    client_addr: impl std::fmt::Display,
) -> Result<()> {
    let proxy_greeting = b"201 NNTP Proxy Ready\r\n";
    client_stream.write_all(proxy_greeting).await?;
    client_stream.flush().await?;
    debug!("Sent and flushed proxy greeting to client {}", client_addr);
    Ok(())
}
