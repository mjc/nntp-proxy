//! Standalone NNTP client for fetching articles
//!
//! This module provides a zero-allocation API for fetching articles from NNTP servers,
//! independent of the proxy functionality. Useful for building downloaders,
//! indexers, or testing tools.
//!
//! # Zero-Allocation Design
//!
//! Uses buffer pool for all allocations. Returns `PooledBuffer` - caller parses:
//!
//! ```no_run
//! use nntp_proxy::client::NntpClient;
//! use nntp_proxy::pool::{BufferPool, DeadpoolConnectionProvider};
//! use nntp_proxy::protocol::Article;
//! use nntp_proxy::types::BufferSize;
//!
//! # async fn example() -> anyhow::Result<()> {
//! // Create connection pool
//! let pool = DeadpoolConnectionProvider::with_tls_auth(
//!     "news.example.com", 563, "user", "pass"
//! )?;
//! let client = NntpClient::new(pool);
//!
//! // Create buffer pool once
//! let buffer_pool = BufferPool::new(BufferSize::try_new(256 * 1024)?, 8);
//!
//! // Fetch returns buffer, caller parses
//! for msg_id in message_ids {
//!     let buffer = client.fetch_body(&msg_id, &buffer_pool).await?;
//!     let article = Article::parse(&buffer, true)?;
//!     if let Some(decoded) = article.decode() {
//!         process(&decoded);
//!     }
//!     // Buffer returns to pool when dropped
//! }
//! # Ok(())
//! # }
//! # fn process(_: &[u8]) {}
//! # let message_ids: Vec<String> = vec![];
//! ```

use anyhow::{Context, Result};

use crate::pool::{BufferPool, DeadpoolConnectionProvider, PooledBuffer};
use crate::protocol::{article_by_msgid, body_by_msgid, head_by_msgid, stat_by_msgid};
use crate::session::backend::send_command;

/// NNTP multiline terminator
const TERMINATOR: &[u8] = b"\r\n.\r\n";

/// Standalone NNTP client for fetching articles
///
/// Zero-allocation design using buffer pool.
/// Returns `PooledBuffer` - caller parses with `Article::parse()`.
#[derive(Clone)]
pub struct NntpClient {
    pool: DeadpoolConnectionProvider,
}

impl NntpClient {
    /// Create a new client with the given connection pool
    #[must_use]
    pub fn new(pool: DeadpoolConnectionProvider) -> Self {
        Self { pool }
    }

    /// Fetch article body (BODY command)
    ///
    /// Returns `PooledBuffer` with the raw response.
    /// Parse with `Article::parse(&buffer, validate_yenc)`.
    ///
    /// # Arguments
    /// * `message_id` - Message-ID including angle brackets, e.g. `<abc@example.com>`
    /// * `buffer_pool` - Buffer pool for I/O and output
    #[inline]
    pub async fn fetch_body(
        &self,
        message_id: &str,
        buffer_pool: &BufferPool,
    ) -> Result<PooledBuffer> {
        let command = body_by_msgid(message_id);
        self.fetch_response(&command, buffer_pool).await
    }

    /// Fetch article headers (HEAD command)
    ///
    /// Returns `PooledBuffer` with the raw response.
    /// Parse with `Article::parse(&buffer, false)`.
    ///
    /// # Arguments
    /// * `message_id` - Message-ID including angle brackets
    /// * `buffer_pool` - Buffer pool for I/O and output
    #[inline]
    pub async fn fetch_head(
        &self,
        message_id: &str,
        buffer_pool: &BufferPool,
    ) -> Result<PooledBuffer> {
        let command = head_by_msgid(message_id);
        self.fetch_response(&command, buffer_pool).await
    }

    /// Fetch full article (ARTICLE command)
    ///
    /// Returns `PooledBuffer` with the raw response.
    /// Parse with `Article::parse(&buffer, validate_yenc)`.
    ///
    /// # Arguments
    /// * `message_id` - Message-ID including angle brackets
    /// * `buffer_pool` - Buffer pool for I/O and output
    #[inline]
    pub async fn fetch_article(
        &self,
        message_id: &str,
        buffer_pool: &BufferPool,
    ) -> Result<PooledBuffer> {
        let command = article_by_msgid(message_id);
        self.fetch_response(&command, buffer_pool).await
    }

    /// Check if article exists (STAT command)
    ///
    /// # Arguments
    /// * `message_id` - Message-ID including angle brackets
    /// * `buffer_pool` - Caller-owned buffer pool for I/O operations
    ///
    /// # Returns
    /// `true` if article exists, `false` if 430 (not found)
    pub async fn stat(&self, message_id: &str, buffer_pool: &BufferPool) -> Result<bool> {
        let command = stat_by_msgid(message_id);
        let mut conn = self
            .pool
            .get_pooled_connection()
            .await
            .context("Failed to get connection from pool")?;
        let mut buffer = buffer_pool.acquire().await;

        let response = send_command(&mut *conn, &command, &mut buffer).await?;

        response
            .status_code()
            .ok_or_else(|| anyhow::anyhow!("Invalid STAT response"))
            .and_then(|code| match code {
                223 => Ok(true),  // Article exists
                430 => Ok(false), // No such article
                _ => anyhow::bail!("Unexpected STAT response: {}", code),
            })
    }

    /// Internal: fetch response into PooledBuffer
    async fn fetch_response(
        &self,
        command: &str,
        buffer_pool: &BufferPool,
    ) -> Result<PooledBuffer> {
        let mut conn = self
            .pool
            .get_pooled_connection()
            .await
            .context("Failed to get connection from pool")?;
        let mut io_buffer = buffer_pool.acquire().await;

        let response = send_command(&mut *conn, command, &mut io_buffer).await?;

        // Validate response - early return on errors
        self.validate_response(&response)?;

        // Single-line response - copy directly
        if !response.is_multiline {
            let mut output = buffer_pool.acquire().await;
            output.copy_from_slice(&io_buffer[..response.bytes_read]);
            return Ok(output);
        }

        // Multiline response - stream until terminator
        let accumulated = self
            .stream_until_terminator(&mut conn, &mut io_buffer, response.bytes_read)
            .await?;

        // Copy complete response to output buffer
        let mut output = buffer_pool.acquire().await;
        output.copy_from_slice(&accumulated);
        Ok(output)
    }

    /// Validate NNTP response status code
    #[inline]
    fn validate_response(&self, response: &crate::session::backend::CommandResponse) -> Result<()> {
        match response.response.status_code() {
            Some(code) if code.as_u16() == 430 => anyhow::bail!("Article not found (430)"),
            Some(code) if code.as_u16() >= 400 => anyhow::bail!("Server error: {}", code),
            None => anyhow::bail!("Invalid response from server"),
            Some(_) => Ok(()),
        }
    }

    /// Stream multiline response until terminator
    async fn stream_until_terminator(
        &self,
        conn: &mut crate::stream::ConnectionStream,
        io_buffer: &mut PooledBuffer,
        first_chunk_size: usize,
    ) -> Result<Vec<u8>> {
        let mut accumulated = Vec::with_capacity(first_chunk_size * 2);
        accumulated.extend_from_slice(&io_buffer[..first_chunk_size]);

        // Early return if first chunk contains terminator
        if accumulated.ends_with(TERMINATOR) {
            return Ok(accumulated);
        }

        // Stream remaining chunks
        loop {
            let n = io_buffer.read_from(conn).await?;
            if n == 0 {
                break; // EOF
            }

            accumulated.extend_from_slice(&io_buffer[..n]);

            if accumulated.ends_with(TERMINATOR) {
                break;
            }
        }

        Ok(accumulated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_terminator_detection() {
        let with_term = b"content\r\n.\r\n";
        let without_term = b"content\r\n";

        assert!(with_term.ends_with(TERMINATOR));
        assert!(!without_term.ends_with(TERMINATOR));
    }
}
