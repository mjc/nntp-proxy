//! Standalone NNTP client for fetching articles
//!
//! This module provides a simple API for fetching articles from NNTP servers,
//! independent of the proxy functionality. Useful for building downloaders,
//! indexers, or testing tools.
//!
//! # Example
//!
//! ```no_run
//! use nntp_proxy::client::NntpClient;
//! use nntp_proxy::pool::DeadpoolConnectionProvider;
//!
//! # async fn example() -> anyhow::Result<()> {
//! // Create a TLS connection pool with authentication (typical Usenet provider)
//! let pool = DeadpoolConnectionProvider::with_tls_auth(
//!     "news.example.com",
//!     563,
//!     "username",
//!     "password",
//! )?;
//!
//! // Create client
//! let client = NntpClient::new(pool);
//!
//! // Fetch article body
//! let body = client.fetch_body("<message-id@example.com>").await?;
//!
//! // Check if article exists
//! let exists = client.stat("<message-id@example.com>").await?;
//! # Ok(())
//! # }
//! ```

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;

use crate::constants::buffer::{POOL, POOL_COUNT};
use crate::pool::{BufferPool, DeadpoolConnectionProvider};
use crate::protocol::{article_by_msgid, body_by_msgid, head_by_msgid, stat_by_msgid};
use crate::session::backend::send_command;
use crate::types::BufferSize;

/// NNTP multiline terminator
const TERMINATOR: &[u8] = b"\r\n.\r\n";

/// Standalone NNTP client for fetching articles
///
/// Wraps a connection pool and provides high-level methods for common operations.
#[derive(Clone)]
pub struct NntpClient {
    pool: DeadpoolConnectionProvider,
    buffer_pool: BufferPool,
}

impl NntpClient {
    /// Create a new client with the given connection pool
    ///
    /// Uses default buffer pool settings.
    #[must_use]
    pub fn new(pool: DeadpoolConnectionProvider) -> Self {
        let buffer_size = BufferSize::try_new(POOL).expect("valid buffer size");
        Self {
            pool,
            buffer_pool: BufferPool::new(buffer_size, POOL_COUNT),
        }
    }

    /// Create a new client with custom buffer pool
    #[must_use]
    pub fn with_buffer_pool(pool: DeadpoolConnectionProvider, buffer_pool: BufferPool) -> Self {
        Self { pool, buffer_pool }
    }

    /// Fetch article body by message-ID (BODY command)
    ///
    /// Returns the raw body bytes. For yEnc-encoded articles, you'll need to
    /// decode with `yenc::decode_buffer()`.
    ///
    /// # Arguments
    /// * `message_id` - Message-ID including angle brackets, e.g. `<abc@example.com>`
    ///
    /// # Returns
    /// Raw body bytes (may be yEnc encoded)
    pub async fn fetch_body(&self, message_id: &str) -> Result<Vec<u8>> {
        let command = body_by_msgid(message_id);
        self.fetch_multiline(&command).await
    }

    /// Fetch article headers by message-ID (HEAD command)
    ///
    /// # Arguments
    /// * `message_id` - Message-ID including angle brackets
    ///
    /// # Returns
    /// Raw header bytes
    pub async fn fetch_head(&self, message_id: &str) -> Result<Vec<u8>> {
        let command = head_by_msgid(message_id);
        self.fetch_multiline(&command).await
    }

    /// Fetch full article by message-ID (ARTICLE command)
    ///
    /// # Arguments
    /// * `message_id` - Message-ID including angle brackets
    ///
    /// # Returns
    /// Raw article bytes (headers + body)
    pub async fn fetch_article(&self, message_id: &str) -> Result<Vec<u8>> {
        let command = article_by_msgid(message_id);
        self.fetch_multiline(&command).await
    }

    /// Check if article exists (STAT command)
    ///
    /// # Arguments
    /// * `message_id` - Message-ID including angle brackets
    ///
    /// # Returns
    /// `true` if article exists, `false` if 430 (not found)
    pub async fn stat(&self, message_id: &str) -> Result<bool> {
        let command = stat_by_msgid(message_id);
        let mut conn = self
            .pool
            .get_pooled_connection()
            .await
            .context("Failed to get connection from pool")?;
        let mut buffer = self.buffer_pool.acquire().await;

        let response = send_command(&mut *conn, &command, &mut buffer).await?;

        match response.status_code() {
            Some(223) => Ok(true),  // Article exists
            Some(430) => Ok(false), // No such article
            Some(code) => anyhow::bail!("Unexpected STAT response: {}", code),
            None => anyhow::bail!("Invalid STAT response"),
        }
    }

    /// Internal: fetch a multiline response and return all bytes
    async fn fetch_multiline(&self, command: &str) -> Result<Vec<u8>> {
        let mut conn = self
            .pool
            .get_pooled_connection()
            .await
            .context("Failed to get connection from pool")?;
        let mut buffer = self.buffer_pool.acquire().await;

        let response = send_command(&mut *conn, command, &mut buffer).await?;

        // Check for errors
        match response.status_code() {
            Some(430) => anyhow::bail!("Article not found (430)"),
            Some(code) if code >= 400 => {
                anyhow::bail!("Server error: {}", code)
            }
            None => anyhow::bail!("Invalid response from server"),
            _ => {}
        }

        if !response.is_multiline {
            // Single-line response, just return first line
            return Ok(buffer[..response.bytes_read].to_vec());
        }

        // Read multiline response into buffer
        self.read_multiline_to_vec(&mut *conn, &buffer[..response.bytes_read])
            .await
    }

    /// Read remaining multiline response into a Vec
    async fn read_multiline_to_vec<C>(&self, conn: &mut C, first_chunk: &[u8]) -> Result<Vec<u8>>
    where
        C: AsyncReadExt + Unpin,
    {
        // Pre-allocate with estimate
        let mut result = Vec::with_capacity(first_chunk.len() * 4);
        result.extend_from_slice(first_chunk);

        // Check if first chunk already has terminator
        if has_terminator(&result) {
            strip_terminator(&mut result);
            return Ok(result);
        }

        // Get a buffer for reading
        let mut read_buffer = self.buffer_pool.acquire().await;

        // Read remaining chunks
        loop {
            let n = read_buffer.read_from(conn).await?;
            if n == 0 {
                break; // EOF
            }

            result.extend_from_slice(&read_buffer[..n]);

            // Check for terminator at end
            if has_terminator(&result) {
                strip_terminator(&mut result);
                break;
            }
        }

        Ok(result)
    }
}

/// Check if buffer ends with NNTP terminator
#[inline]
fn has_terminator(data: &[u8]) -> bool {
    data.ends_with(TERMINATOR)
}

/// Strip NNTP terminator from end of buffer
#[inline]
fn strip_terminator(data: &mut Vec<u8>) {
    if data.ends_with(TERMINATOR) {
        data.truncate(data.len() - TERMINATOR.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_has_terminator() {
        assert!(has_terminator(b"data\r\n.\r\n"));
        assert!(!has_terminator(b"data\r\n"));
        assert!(!has_terminator(b"data"));
    }

    #[test]
    fn test_strip_terminator() {
        let mut data = b"content\r\n.\r\n".to_vec();
        strip_terminator(&mut data);
        assert_eq!(data, b"content");
    }
}
