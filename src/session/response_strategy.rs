//! Response handling strategies for command execution
//!
//! This module provides different strategies for handling NNTP responses:
//! - Streaming: Direct streaming from backend to client (high performance)
//! - Caching: Buffer entire response for caching before sending to client
//!
//! Using an enum-based strategy pattern eliminates ~200 lines of code duplication between
//! execute_command_on_backend and execute_command_with_caching.

use crate::cache::{ArticleCache, CachedArticle};
use crate::pool::PooledBuffer;
use crate::protocol::{NntpResponse, Response};
use crate::session::streaming;
use crate::types::{BackendId, BytesTransferred, MessageId};
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::WriteHalf;
use tracing::{debug, warn};

/// Strategy for handling NNTP response data
///
/// Use an enum instead of trait objects to avoid dyn compatibility issues with async methods.
/// This is more efficient anyway (no vtable indirection).
pub enum ResponseStrategy {
    /// Direct streaming from backend to client
    Streaming,
    /// Buffer entire response for caching
    Caching { cache: Arc<ArticleCache> },
}

impl ResponseStrategy {
    /// Handle response data from backend
    ///
    /// # Arguments
    /// * `pooled_conn` - Backend connection to read from
    /// * `response_code` - Parsed response code
    /// * `is_multiline` - Whether this is a multiline response
    /// * `first_chunk` - First chunk of data already read
    /// * `first_n` - Size of first chunk
    /// * `client_write` - Client connection to write to
    /// * `backend_id` - Backend identifier for logging
    /// * `client_addr` - Client address for logging
    /// * `buffer_pool` - Buffer pool for allocating buffers
    /// * `command` - Original command (for message-ID extraction)
    /// * `chunk_buffer` - Reusable buffer for reading additional chunks
    ///
    /// # Returns
    /// Number of bytes written to client (type-safe)
    #[allow(clippy::too_many_arguments)]
    pub async fn handle_response(
        &self,
        pooled_conn: &mut deadpool::managed::Object<crate::pool::deadpool_connection::TcpManager>,
        response_code: Response,
        is_multiline: bool,
        first_chunk: &[u8],
        first_n: usize,
        client_write: &mut WriteHalf<'_>,
        backend_id: BackendId,
        client_addr: SocketAddr,
        buffer_pool: &crate::pool::BufferPool,
        command: &str,
        chunk_buffer: &mut PooledBuffer,
    ) -> Result<BytesTransferred> {
        let bytes_written = match self {
            Self::Streaming => {
                Self::stream_response(
                    pooled_conn,
                    is_multiline,
                    first_chunk,
                    first_n,
                    client_write,
                    backend_id,
                    client_addr,
                    buffer_pool,
                )
                .await?
            }
            Self::Caching { cache } => {
                Self::cache_response(
                    pooled_conn,
                    is_multiline,
                    first_chunk,
                    first_n,
                    client_write,
                    command,
                    chunk_buffer,
                    cache,
                )
                .await?
            }
        };

        // Log warnings for unusual responses
        Self::check_unusual_response(response_code, client_addr, command);

        Ok(bytes_written)
    }

    /// Stream response directly to client
    ///
    /// Returns total bytes written to client (type-safe)
    #[allow(clippy::too_many_arguments)]
    async fn stream_response(
        pooled_conn: &mut deadpool::managed::Object<crate::pool::deadpool_connection::TcpManager>,
        is_multiline: bool,
        first_chunk: &[u8],
        first_n: usize,
        client_write: &mut WriteHalf<'_>,
        backend_id: BackendId,
        client_addr: SocketAddr,
        buffer_pool: &crate::pool::BufferPool,
    ) -> Result<BytesTransferred> {
        if is_multiline {
            // stream_multiline_response handles the first chunk AND remaining chunks
            // It returns the total bytes written including the first chunk
            streaming::stream_multiline_response(
                &mut **pooled_conn,
                client_write,
                first_chunk,
                first_n,
                client_addr,
                backend_id,
                buffer_pool,
            )
            .await
        } else {
            // Single-line response - just write the first chunk
            client_write.write_all(&first_chunk[..first_n]).await?;
            Ok(BytesTransferred::new(first_n as u64))
        }
    }

    /// Cache response before sending to client
    ///
    /// Returns total bytes written to client (type-safe)
    #[allow(clippy::too_many_arguments)]
    async fn cache_response(
        pooled_conn: &mut deadpool::managed::Object<crate::pool::deadpool_connection::TcpManager>,
        is_multiline: bool,
        first_chunk: &[u8],
        first_n: usize,
        client_write: &mut WriteHalf<'_>,
        command: &str,
        chunk_buffer: &mut PooledBuffer,
        cache: &Arc<ArticleCache>,
    ) -> Result<BytesTransferred> {
        use tokio::io::AsyncReadExt;

        // Buffer entire response
        let mut response_buffer = Vec::new();
        response_buffer.extend_from_slice(&first_chunk[..first_n]);

        if is_multiline {
            // Read remaining chunks until terminator
            loop {
                let n = pooled_conn.read(chunk_buffer.as_mut_slice()).await?;
                if n == 0 {
                    break;
                }
                response_buffer.extend_from_slice(&chunk_buffer[..n]);
                if NntpResponse::has_terminator_at_end(&response_buffer) {
                    break;
                }
            }
        }

        let total_bytes = BytesTransferred::new(response_buffer.len() as u64);

        // Send to client
        client_write.write_all(&response_buffer).await?;

        // Cache if this is an ARTICLE command with message-ID
        if let Some(msgid) = crate::session::common::extract_message_id(command) {
            let article = CachedArticle {
                response: Arc::new(response_buffer),
            };
            // extract_message_id returns &str, need to create MessageId
            if let Ok(msg_id) = MessageId::from_borrowed(msgid) {
                cache.insert(msg_id, article).await;
                debug!("Cached article: {}", msgid);
            }
        }

        Ok(total_bytes)
    }

    /// Check for unusual responses and log warnings
    fn check_unusual_response(response_code: Response, client_addr: SocketAddr, command: &str) {
        if let Some(code) = response_code.status_code() {
            let raw_code = code.as_u16();
            if let Some(msgid) = crate::session::common::extract_message_id(command) {
                // Warn only for truly unusual responses (not 220/222/223 or errors)
                // 220 = ARTICLE, 222 = BODY, 223 = HEAD
                if !matches!(raw_code, 220 | 222 | 223) && !code.is_error() {
                    warn!(
                        "Client {} ARTICLE {} got unusual single-line response: {}",
                        client_addr, msgid, code
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn test_streaming_strategy_creation() {
        let _strategy = ResponseStrategy::Streaming;
    }

    #[test]
    fn test_caching_strategy_creation() {
        let cache = Arc::new(ArticleCache::new(100, std::time::Duration::from_secs(3600)));
        let _strategy = ResponseStrategy::Caching { cache };
    }

    #[test]
    fn test_check_unusual_response_normal_codes() {
        use crate::protocol::StatusCode;
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);

        // These should NOT trigger warnings (normal article retrieval responses)
        ResponseStrategy::check_unusual_response(
            Response::SingleLine(StatusCode::new(220)),
            addr,
            "ARTICLE <test@example.com>",
        );
        ResponseStrategy::check_unusual_response(
            Response::SingleLine(StatusCode::new(222)),
            addr,
            "BODY <test@example.com>",
        );
        ResponseStrategy::check_unusual_response(
            Response::SingleLine(StatusCode::new(223)),
            addr,
            "STAT <test@example.com>",
        );

        // Error codes should also not trigger warnings
        ResponseStrategy::check_unusual_response(
            Response::SingleLine(StatusCode::new(430)),
            addr,
            "ARTICLE <test@example.com>",
        );
    }

    #[test]
    fn test_check_unusual_response_unusual_codes() {
        use crate::protocol::StatusCode;
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);

        // These SHOULD trigger warnings (unusual success codes for article commands)
        // Note: This test just verifies the function runs without panicking
        // Actual warning verification would require log capture
        ResponseStrategy::check_unusual_response(
            Response::SingleLine(StatusCode::new(200)),
            addr,
            "ARTICLE <test@example.com>",
        );
        ResponseStrategy::check_unusual_response(
            Response::SingleLine(StatusCode::new(211)),
            addr,
            "BODY <test@example.com>",
        );
    }

    #[test]
    fn test_check_unusual_response_no_message_id() {
        use crate::protocol::StatusCode;
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);

        // Commands without message-ID should not trigger warnings
        ResponseStrategy::check_unusual_response(
            Response::SingleLine(StatusCode::new(200)),
            addr,
            "GROUP alt.test",
        );
        ResponseStrategy::check_unusual_response(
            Response::SingleLine(StatusCode::new(211)),
            addr,
            "LIST",
        );
    }
}
