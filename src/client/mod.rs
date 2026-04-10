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
use deadpool::managed::Object;

use crate::pool::deadpool_connection::TcpManager;
use crate::pool::{BufferPool, DeadpoolConnectionProvider, PooledBuffer};
use crate::protocol::{article_by_msgid, body_by_msgid, head_by_msgid, stat_by_msgid};
use crate::session::backend::send_command;

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
    pub async fn fetch_body(
        &self,
        message_id: &crate::types::MessageId<'_>,
    ) -> Result<PooledBuffer> {
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
    pub async fn fetch_head(
        &self,
        message_id: &crate::types::MessageId<'_>,
    ) -> Result<PooledBuffer> {
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
    pub async fn fetch_article(
        &self,
        message_id: &crate::types::MessageId<'_>,
    ) -> Result<PooledBuffer> {
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
    pub async fn stat(&self, message_id: &crate::types::MessageId<'_>) -> Result<bool> {
        let command = stat_by_msgid(message_id);
        let mut conn = self.get_connection().await?;
        let mut buffer = self.buffer_pool.acquire().await;

        let response = send_command(&mut *conn, &command, &mut buffer).await?;

        Self::parse_stat_response(response.status_code())
    }

    /// Parse STAT response code into existence check
    #[inline]
    fn parse_stat_response(status_code: Option<crate::protocol::StatusCode>) -> Result<bool> {
        status_code
            .ok_or_else(|| anyhow::anyhow!("Invalid STAT response"))
            .and_then(|code| match code.as_u16() {
                223 => Ok(true),  // Article exists
                430 => Ok(false), // No such article
                _ => anyhow::bail!("Unexpected STAT response: {}", code),
            })
    }

    /// Get a connection from the pool
    #[inline]
    async fn get_connection(&self) -> Result<Object<TcpManager>> {
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

        if response.is_multiline {
            // Use a capture buffer as the accumulator: pooled, can grow beyond io_buffer
            // capacity without panicking, returned to pool on drop.
            let mut capture = self.buffer_pool.acquire_capture().await;
            if let Err(err) = Self::drain_multiline_into(
                &mut conn,
                &mut io_buffer,
                &mut capture,
                response.bytes_read,
            )
            .await
            {
                self.conn_pool.remove_with_cooldown(conn);
                return Err(err);
            }
            Ok(capture)
        } else {
            Ok(io_buffer)
        }
    }

    /// Stream remaining multiline response data into `capture`.
    ///
    /// `io_buffer` is the scratch buffer for socket reads (fixed size).
    /// `capture` is the accumulator returned to the caller (can grow).
    async fn drain_multiline_into(
        conn: &mut crate::stream::ConnectionStream,
        io_buffer: &mut PooledBuffer,
        capture: &mut PooledBuffer,
        first_chunk_size: usize,
    ) -> Result<()> {
        use crate::session::streaming::tail_buffer::{TailBuffer, TerminatorStatus};

        let first_chunk = &io_buffer.as_mut_slice()[..first_chunk_size];
        let mut tail = TailBuffer::default();

        match tail.detect_terminator(first_chunk) {
            TerminatorStatus::FoundAt(pos) => {
                // pos is after the terminator (terminator included in [..pos])
                capture.extend_from_slice(&first_chunk[..pos]);
                if pos < first_chunk.len() {
                    conn.stash_leftover(&first_chunk[pos..])?;
                }
                return Ok(());
            }
            TerminatorStatus::NotFound => {
                tail.update(first_chunk);
                capture.extend_from_slice(first_chunk);
            }
        }

        while let Ok(n) = io_buffer.read_from(conn).await {
            if n == 0 {
                break; // EOF
            }
            // NLL: chunk borrow ends before next read_from call
            let found_at = {
                let chunk = &io_buffer.as_mut_slice()[..n];
                tail.detect_terminator(chunk)
            };
            match found_at {
                TerminatorStatus::FoundAt(pos) => {
                    // pos is after the terminator (terminator included in [..pos])
                    capture.extend_from_slice(&io_buffer.as_mut_slice()[..pos]);
                    if pos < n {
                        conn.stash_leftover(&io_buffer.as_mut_slice()[pos..n])?;
                    }
                    break;
                }
                TerminatorStatus::NotFound => {
                    let chunk = &io_buffer.as_mut_slice()[..n];
                    tail.update(chunk);
                    capture.extend_from_slice(chunk);
                }
            }
        }

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
    fn test_parse_stat_response_success() {
        use crate::protocol::StatusCode;
        // Article exists (223)
        assert!(NntpClient::parse_stat_response(StatusCode::parse(b"223")).unwrap());

        // Article not found (430)
        assert!(!NntpClient::parse_stat_response(StatusCode::parse(b"430")).unwrap());
    }

    #[test]
    fn test_parse_stat_response_errors() {
        use crate::protocol::StatusCode;
        // No status code
        assert!(NntpClient::parse_stat_response(None).is_err());

        // Unexpected codes
        assert!(NntpClient::parse_stat_response(StatusCode::parse(b"500")).is_err());
        assert!(NntpClient::parse_stat_response(StatusCode::parse(b"200")).is_err());
        assert!(NntpClient::parse_stat_response(StatusCode::parse(b"400")).is_err());
    }

    /// Spawn a minimal NNTP server that sends a greeting, then waits for
    /// `notify` before sending `article_data`. Returns (addr, notify).
    ///
    /// The caller calls `pool.get()` first (which consumes only the greeting),
    /// then fires the notify so the server sends article data into the established
    /// connection. This prevents `consume_greeting` from inadvertently consuming
    /// article bytes (both writes arriving in the same TCP segment).
    async fn spawn_test_server(
        article_data: &'static [u8],
    ) -> (std::net::SocketAddr, std::sync::Arc<tokio::sync::Notify>) {
        use std::sync::Arc;
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;
        use tokio::sync::Notify;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let notify = Arc::new(Notify::new());
        let n = Arc::clone(&notify);

        tokio::spawn(async move {
            loop {
                if let Ok((mut stream, _)) = listener.accept().await {
                    let wake = Arc::clone(&n);
                    tokio::spawn(async move {
                        let _ = stream.write_all(b"200 mock\r\n").await;
                        // Block until the test signals that pool.get() has returned
                        // (greeting already consumed) before sending article data
                        wake.notified().await;
                        let _ = stream.write_all(article_data).await;
                        // Keep alive so recycle's try_read sees WouldBlock
                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    });
                }
            }
        });

        (addr, notify)
    }

    async fn make_test_pool(addr: std::net::SocketAddr) -> crate::pool::deadpool_connection::Pool {
        let manager = crate::pool::deadpool_connection::TcpManager::new(
            addr.ip().to_string(),
            addr.port(),
            "test".to_string(),
            None,
            None,
            None,
            Some(false), // disable compression — mock doesn't handle it
            None,
        )
        .unwrap();
        crate::pool::deadpool_connection::Pool::builder(manager)
            .max_size(2)
            .build()
            .unwrap()
    }

    /// Verify drain_multiline_into captures the complete response when it all
    /// arrives in the first pre-read chunk (first_chunk_size == article length).
    #[tokio::test]
    async fn test_drain_multiline_into_single_read() {
        use crate::pool::BufferPool;
        use crate::types::BufferSize;

        let article = b"220 body follows\r\nHello world\r\n.\r\n";
        let (addr, notify) = spawn_test_server(article).await;
        let pool = make_test_pool(addr).await;
        let buffer_pool = BufferPool::new(BufferSize::try_new(4096).unwrap(), 2);

        let mut conn = pool.get().await.unwrap();
        // Signal server to send article data now that the greeting is consumed
        notify.notify_one();

        let mut io_buffer = buffer_pool.acquire().await;
        let mut capture = buffer_pool.acquire_capture().await;

        // Simulate send_command pre-reading the full first response chunk
        let first_chunk_size = io_buffer.read_from(&mut *conn).await.unwrap();

        NntpClient::drain_multiline_into(&mut conn, &mut io_buffer, &mut capture, first_chunk_size)
            .await
            .unwrap();

        assert_eq!(&capture[..], article as &[u8]);
    }

    /// Verify drain_multiline_into accumulates correctly across multiple reads,
    /// including when the NNTP terminator spans a read boundary (3+ reads).
    ///
    /// Uses an 8-byte I/O buffer against a 36-byte article, forcing 5 reads.
    /// Read 4 ends with `\r` and read 5 starts with `\n.\r\n`, so the terminator
    /// `\r\n.\r\n` spans the boundary — exercising TailBuffer spanning detection.
    #[tokio::test]
    async fn test_drain_multiline_into_multi_read_spanning_terminator() {
        use crate::pool::BufferPool;
        use crate::types::BufferSize;

        // 36 bytes total: 5 × 8-byte reads with 8-byte io_buffer.
        // Terminator \r\n.\r\n spans read 4 ("\r") → read 5 ("\n.\r\n").
        let article = b"220 article\r\nLine one\r\nLine two\r\n.\r\n";
        let (addr, notify) = spawn_test_server(article).await;
        let pool = make_test_pool(addr).await;
        // Tiny I/O buffer forces multiple reads and exercises the streaming loop
        let buffer_pool = BufferPool::new(BufferSize::try_new(8).unwrap(), 4);

        let mut conn = pool.get().await.unwrap();
        notify.notify_one();

        let mut io_buffer = buffer_pool.acquire().await;
        let mut capture = buffer_pool.acquire_capture().await;

        // first_chunk_size = 0: no pre-loaded data, all bytes arrive via the loop
        NntpClient::drain_multiline_into(&mut conn, &mut io_buffer, &mut capture, 0)
            .await
            .unwrap();

        assert_eq!(&capture[..], article as &[u8]);
    }
}
