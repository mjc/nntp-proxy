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
use crate::types::{BackendId, MessageId};
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
    /// Number of bytes written to client
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
    ) -> Result<u64> {
        match self {
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

        Ok(first_n as u64)
    }

    /// Stream response directly to client
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
    ) -> Result<()> {
        // Write first chunk
        client_write.write_all(&first_chunk[..first_n]).await?;

        if is_multiline {
            // Continue streaming remaining chunks
            streaming::stream_multiline_response(
                &mut **pooled_conn,
                client_write,
                first_chunk,
                first_n,
                client_addr,
                backend_id,
                buffer_pool,
            )
            .await?;
        }

        Ok(())
    }

    /// Cache response before sending to client
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
    ) -> Result<()> {
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

        Ok(())
    }

    /// Check for unusual responses and log warnings
    fn check_unusual_response(response_code: Response, client_addr: SocketAddr, command: &str) {
        if let Some(code) = response_code.status_code() {
            let raw_code = code.as_u16();
            if let Some(msgid) = crate::session::common::extract_message_id(command) {
                // Warn only for truly unusual responses (not 223 or errors)
                if raw_code != 223 && !code.is_error() {
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

    #[test]
    fn test_streaming_strategy_creation() {
        let _strategy = ResponseStrategy::Streaming;
    }

    #[test]
    fn test_caching_strategy_creation() {
        let cache = Arc::new(ArticleCache::new(100, std::time::Duration::from_secs(3600)));
        let _strategy = ResponseStrategy::Caching { cache };
    }
}
