//! Standalone NNTP client for fetching articles
//!
//! This module provides a zero-allocation API for fetching articles from NNTP servers,
//! independent of the proxy functionality. Useful for building downloaders,
//! indexers, or testing tools.
//!
//! # Zero-Allocation Design
//!
//! The caller provides a shared buffer pool. One pool can serve multiple clients:
//!
//! ```no_run
//! use nntp_proxy::client::NntpClient;
//! use nntp_proxy::pool::{BufferPool, DeadpoolConnectionProvider};
//! use nntp_proxy::protocol::Article;
//! use nntp_proxy::types::BufferSize;
//!
//! # async fn example() -> anyhow::Result<()> {
//! // One buffer pool shared across all clients
//! let buffer_pool = BufferPool::new(BufferSize::try_new(256 * 1024)?, 8);
//!
//! let conn_pool = DeadpoolConnectionProvider::with_tls_auth(
//!     "news.example.com", 563, "user", "pass"
//! )?;
//! let client = NntpClient::new(conn_pool, buffer_pool.clone());
//!
//! # let message_ids: Vec<&str> = vec![];
//! for msg_id in message_ids {
//!     let buffer = client.fetch_body(msg_id).await?;
//!     let article = Article::parse(&buffer, true)?;
//!     if let Some(decoded) = article.decode() {
//!         process(&decoded);
//!     }
//!     // Buffer returns to shared pool when dropped
//! }
//! # Ok(())
//! # }
//! # fn process(_: &[u8]) {}
//! ```

use anyhow::{Context, Result};

use crate::pool::{BufferPool, DeadpoolConnectionProvider, PooledBuffer};
use crate::protocol::{article_by_msgid, body_by_msgid, head_by_msgid, stat_by_msgid};
use crate::session::backend::send_command;

/// NNTP multiline terminator
const TERMINATOR: &[u8] = b"\r\n.\r\n";

/// Standalone NNTP client for fetching articles
///
/// Zero-allocation design using caller-provided buffer pool.
/// Share one pool across multiple clients for minimal allocations.
/// Returns `PooledBuffer` - caller parses with `Article::parse()`.
#[derive(Clone)]
pub struct NntpClient {
    conn_pool: DeadpoolConnectionProvider,
    buffer_pool: BufferPool,
}

impl NntpClient {
    /// Create a new client with connection pool and buffer pool
    ///
    /// The buffer pool can be shared across multiple clients via `Clone`.
    #[must_use]
    pub fn new(conn_pool: DeadpoolConnectionProvider, buffer_pool: BufferPool) -> Self {
        Self {
            conn_pool,
            buffer_pool,
        }
    }

    /// Fetch article body (BODY command)
    ///
    /// Returns `PooledBuffer` with the raw response.
    /// Parse with `Article::parse(&buffer, validate_yenc)`.
    ///
    /// # Arguments
    /// * `message_id` - Message-ID including angle brackets, e.g. `<abc@example.com>`
    #[inline]
    pub async fn fetch_body(&self, message_id: &str) -> Result<PooledBuffer> {
        let command = body_by_msgid(message_id);
        self.fetch_response(&command).await
    }

    /// Fetch article headers (HEAD command)
    ///
    /// Returns `PooledBuffer` with the raw response.
    /// Parse with `Article::parse(&buffer, false)`.
    ///
    /// # Arguments
    /// * `message_id` - Message-ID including angle brackets
    #[inline]
    pub async fn fetch_head(&self, message_id: &str) -> Result<PooledBuffer> {
        let command = head_by_msgid(message_id);
        self.fetch_response(&command).await
    }

    /// Fetch full article (ARTICLE command)
    ///
    /// Returns `PooledBuffer` with the raw response.
    /// Parse with `Article::parse(&buffer, validate_yenc)`.
    ///
    /// # Arguments
    /// * `message_id` - Message-ID including angle brackets
    #[inline]
    pub async fn fetch_article(&self, message_id: &str) -> Result<PooledBuffer> {
        let command = article_by_msgid(message_id);
        self.fetch_response(&command).await
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
        let mut conn = self.get_connection().await?;
        let mut buffer = self.buffer_pool.acquire().await;

        let response = send_command(&mut *conn, &command, &mut buffer).await?;

        Self::parse_stat_response(response.status_code())
    }

    /// Parse STAT response code into existence check
    #[inline]
    fn parse_stat_response(status_code: Option<u16>) -> Result<bool> {
        status_code
            .ok_or_else(|| anyhow::anyhow!("Invalid STAT response"))
            .and_then(|code| match code {
                223 => Ok(true),  // Article exists
                430 => Ok(false), // No such article
                _ => anyhow::bail!("Unexpected STAT response: {}", code),
            })
    }

    /// Get a connection from the pool
    #[inline]
    async fn get_connection(
        &self,
    ) -> Result<impl std::ops::DerefMut<Target = crate::stream::ConnectionStream>> {
        self.conn_pool
            .get_pooled_connection()
            .await
            .context("Failed to get connection from pool")
    }

    /// Internal: fetch response into PooledBuffer
    async fn fetch_response(&self, command: &str) -> Result<PooledBuffer> {
        let mut conn = self.get_connection().await?;
        let mut io_buffer = self.buffer_pool.acquire().await;

        let response = send_command(&mut *conn, command, &mut io_buffer).await?;

        // Validate response - early return on errors
        Self::validate_response(&response)?;

        // Handle multiline responses - need to stream remaining data
        // For single-line responses, io_buffer already has correct initialized count
        if response.is_multiline {
            self.handle_multiline_response(&mut conn, &mut io_buffer, response.bytes_read)
                .await?;
        }

        Ok(io_buffer)
    }

    /// Handle multiline response - stream until terminator, extending io_buffer in-place
    async fn handle_multiline_response(
        &self,
        conn: &mut crate::stream::ConnectionStream,
        io_buffer: &mut PooledBuffer,
        first_chunk_size: usize,
    ) -> Result<()> {
        // Early return if first chunk contains terminator
        let first_chunk = &io_buffer.as_mut_slice()[..first_chunk_size];
        if first_chunk.ends_with(TERMINATOR) {
            return Ok(());
        }

        // Stream remaining chunks until terminator
        // Note: We need to accumulate because io_buffer has fixed capacity
        // and articles can exceed it. Using Vec for dynamic growth.
        let mut accumulated = Vec::with_capacity(first_chunk_size * 2);
        accumulated.extend_from_slice(first_chunk);

        while let Ok(n) = io_buffer.read_from(conn).await {
            if n == 0 {
                break; // EOF
            }

            accumulated.extend_from_slice(&io_buffer.as_mut_slice()[..n]);

            if accumulated.ends_with(TERMINATOR) {
                break;
            }
        }

        // Copy accumulated data back to io_buffer (sets initialized count)
        io_buffer.copy_from_slice(&accumulated);
        Ok(())
    }

    /// Validate NNTP response status code
    #[inline]
    fn validate_response(response: &crate::session::backend::CommandResponse) -> Result<()> {
        response
            .response
            .status_code()
            .ok_or_else(|| anyhow::anyhow!("Invalid response from server"))
            .and_then(|code| match code.as_u16() {
                430 => anyhow::bail!("Article not found (430)"),
                code if code >= 400 => anyhow::bail!("Server error: {}", code),
                _ => Ok(()),
            })
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

    #[test]
    fn test_parse_stat_response_success() {
        // Article exists (223)
        assert!(NntpClient::parse_stat_response(Some(223)).unwrap());

        // Article not found (430)
        assert!(!NntpClient::parse_stat_response(Some(430)).unwrap());
    }

    #[test]
    fn test_parse_stat_response_errors() {
        // No status code
        assert!(NntpClient::parse_stat_response(None).is_err());

        // Unexpected codes
        assert!(NntpClient::parse_stat_response(Some(500)).is_err());
        assert!(NntpClient::parse_stat_response(Some(200)).is_err());
        assert!(NntpClient::parse_stat_response(Some(400)).is_err());
    }

    #[test]
    fn test_terminator_constant() {
        assert_eq!(TERMINATOR, b"\r\n.\r\n");
        assert_eq!(TERMINATOR.len(), 5);
    }
}
