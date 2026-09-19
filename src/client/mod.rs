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
//! use nntp_proxy::protocol::{Article, YencValidation};
//! use nntp_proxy::types::{BufferSize, MessageId};
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
//! # let message_ids: Vec<MessageId<'static>> = vec![];
//! for msg_id in message_ids {
//!     let framed = client.fetch_body(&msg_id).await?;
//!     let validated = framed.validate_with_yenc(YencValidation::Enabled)?;
//!     let article = validated.article();
//!     if let Some(decoded) = article.decode() {
//!         process(&decoded);
//!     }
//!     // Buffer returns to shared pool when dropped
//! }
//! # Ok(())
//! # }
//! # fn process(_: &[u8]) {}
//! ```

use crate::pool::{BufferPool, DeadpoolConnectionProvider, PooledBuffer};
use crate::protocol::{
    ArticleView, RequestContext, StatusCode, YencValidation, article_request, body_request,
    head_request, stat_request,
};
use crate::session::backend::execute_request_exchange;
use anyhow::{Context, Result};

/// Standalone NNTP client for fetching articles
///
/// Zero-allocation design using caller-provided buffer pool.
/// Share one pool across multiple clients for minimal allocations.
/// Returns a framer-owned [`FramedArticle`]. Its bytes remain associated with
/// the boundary established by the response framer; callers obtain a reusable
/// [`ArticleView`] through the consuming [`FramedArticle::validate`] transition.
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
    pub const fn new(conn_pool: DeadpoolConnectionProvider, buffer_pool: BufferPool) -> Self {
        Self {
            conn_pool,
            buffer_pool,
        }
    }

    /// Fetch article body (BODY command)
    ///
    /// Returns a framed owner with the status line and payload bytes. The
    /// multiline terminator has already been consumed by the framer.
    ///
    /// # Arguments
    /// * `message_id` - Message-ID including angle brackets, e.g. `<abc@example.com>`
    ///
    /// # Errors
    /// Returns any connection, write, or backend-response error encountered while
    /// fetching the BODY response.
    #[inline]
    pub fn fetch_body(
        &self,
        message_id: &crate::types::MessageId<'_>,
    ) -> impl std::future::Future<Output = Result<FramedArticle>> + '_ {
        self.fetch_response(body_request(message_id))
    }

    /// Fetch article headers (HEAD command)
    ///
    /// Returns a framed owner with the status line and payload bytes. The
    /// multiline terminator has already been consumed by the framer.
    ///
    /// # Arguments
    /// * `message_id` - Message-ID including angle brackets
    ///
    /// # Errors
    /// Returns any connection, write, or backend-response error encountered while
    /// fetching the HEAD response.
    #[inline]
    pub fn fetch_head(
        &self,
        message_id: &crate::types::MessageId<'_>,
    ) -> impl std::future::Future<Output = Result<FramedArticle>> + '_ {
        self.fetch_response(head_request(message_id))
    }

    /// Fetch full article (ARTICLE command)
    ///
    /// Returns a framed owner with the status line and payload bytes. The
    /// multiline terminator has already been consumed by the framer.
    ///
    /// # Arguments
    /// * `message_id` - Message-ID including angle brackets
    ///
    /// # Errors
    /// Returns any connection, write, or backend-response error encountered while
    /// fetching the ARTICLE response.
    #[inline]
    pub fn fetch_article(
        &self,
        message_id: &crate::types::MessageId<'_>,
    ) -> impl std::future::Future<Output = Result<FramedArticle>> + '_ {
        self.fetch_response(article_request(message_id))
    }

    /// Check if article exists (STAT command)
    ///
    /// # Arguments
    /// * `message_id` - Message-ID including angle brackets
    ///
    /// # Returns
    /// `true` if article exists, `false` if 430 (not found)
    ///
    /// # Errors
    /// Returns any connection or protocol error while issuing `STAT`, including
    /// malformed or unexpected backend status codes.
    pub async fn stat(&self, message_id: &crate::types::MessageId<'_>) -> Result<bool> {
        let request = stat_request(message_id);
        let conn = self
            .conn_pool
            .checkout_connection_guard()
            .await
            .context("Failed to get connection from pool")?
            .activate();
        let buffer = self.buffer_pool.acquire();

        let mut exchange = execute_request_exchange(
            conn,
            &request,
            buffer,
            &self.buffer_pool,
            crate::types::BackendId::from_index(0),
        )
        .await?;
        let Some(status_code) = exchange.status_code() else {
            exchange.fail_backend();
            anyhow::bail!("Invalid STAT response");
        };

        let result = Self::parse_stat_response(status_code);
        if result.is_ok() {
            let captured = exchange.receiving()?.capture_isolated().await;
            let conn = exchange.into_connection();
            match captured {
                Ok(_captured) => {
                    let _reusable = conn.complete_success();
                }
                Err(error) => {
                    conn.fail_backend();
                    return Err(error);
                }
            }
        } else {
            exchange.fail_backend();
        }
        result
    }

    /// Parse STAT response code into existence check
    #[inline]
    fn parse_stat_response(status_code: crate::protocol::StatusCode) -> Result<bool> {
        match status_code.as_u16() {
            223 => Ok(true),  // Article exists
            430 => Ok(false), // No such article
            code => anyhow::bail!("Unexpected STAT response: {code}"),
        }
    }

    /// Internal: fetch response into `PooledBuffer`
    ///
    /// # Errors
    /// Returns any connection, write, read, or backend-status validation error
    /// encountered while fetching the NNTP response.
    async fn fetch_response(&self, request: RequestContext) -> Result<FramedArticle> {
        let conn = self
            .conn_pool
            .checkout_connection_guard()
            .await
            .context("Failed to get connection from pool")?
            .activate();
        let io_buffer = self.buffer_pool.acquire();

        let mut exchange = execute_request_exchange(
            conn,
            &request,
            io_buffer,
            &self.buffer_pool,
            crate::types::BackendId::from_index(0),
        )
        .await?;
        let Some(status_code) = exchange.status_code() else {
            exchange.fail_backend();
            anyhow::bail!("Invalid response from server");
        };

        Self::validate_response(status_code)?;

        let captured = match exchange.receiving()?.capture_isolated().await {
            Ok(result) => result,
            Err(error) => {
                exchange.into_connection_after_failure().fail_backend();
                return Err(error);
            }
        };
        let conn = exchange.into_connection();
        let _reusable = conn.complete_success();
        Ok(FramedArticle::from_framed(captured))
    }

    /// Validate NNTP response status code
    #[inline]
    fn validate_response(status_code: crate::protocol::StatusCode) -> Result<()> {
        match status_code.as_u16() {
            430 => anyhow::bail!("Article not found (430)"),
            code if code >= 400 => anyhow::bail!("Server error: {code}"),
            _ => Ok(()),
        }
    }
}

/// An article-family response whose wire boundary has already been established.
///
/// The owner is the pooled allocation returned by the framing operation. The
/// multiline terminator is not part of the stored bytes, while the status line
/// and payload bytes are retained exactly as received. Framing and semantic
/// article validity are separate guarantees.
#[derive(Debug)]
pub struct FramedArticle {
    state: crate::protocol::ArticleState<crate::protocol::FramedArticleState<PooledBuffer>>,
}

impl FramedArticle {
    fn from_framed(
        state: crate::protocol::ArticleState<crate::protocol::FramedArticleState<PooledBuffer>>,
    ) -> Self {
        Self { state }
    }

    /// Request kind that produced this response.
    #[must_use]
    pub const fn kind(&self) -> crate::protocol::RequestKind {
        self.state.as_inner().kind()
    }

    /// Parsed status code established by the response framer.
    #[must_use]
    pub const fn status(&self) -> StatusCode {
        self.state.as_inner().status()
    }

    /// Exact framed bytes, including the status line and excluding the
    /// multiline terminator.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.state.as_bytes()
    }

    /// Validate NNTP article semantics and transition to reusable typed access.
    /// yEnc validation is explicit policy, not part of NNTP framing.
    pub fn validate(self, validate_yenc: bool) -> Result<ValidatedArticle> {
        let policy = match validate_yenc {
            true => YencValidation::Enabled,
            false => YencValidation::Disabled,
        };
        self.validate_with_yenc(policy)
    }

    /// Validate NNTP semantics and apply the selected optional yEnc policy.
    pub fn validate_with_yenc(self, policy: YencValidation) -> Result<ValidatedArticle> {
        let state = self.state.into_inner().validate(policy)?;
        Ok(ValidatedArticle {
            state: crate::protocol::ArticleState::new(state),
        })
    }

    /// Consume the framed owner and return its pooled storage.
    #[must_use]
    pub fn into_bytes(self) -> PooledBuffer {
        self.state.into_inner().into_bytes()
    }
}

/// A semantically validated article whose layout remains bound to its bytes.
#[derive(Debug)]
pub struct ValidatedArticle {
    state: crate::protocol::ArticleState<crate::protocol::ValidatedArticleState<PooledBuffer>>,
}

impl ValidatedArticle {
    /// Request kind that established this validated article state.
    #[must_use]
    pub const fn kind(&self) -> crate::protocol::RequestKind {
        self.state.kind()
    }

    /// Status code that established this validated article state.
    #[must_use]
    pub const fn status(&self) -> StatusCode {
        self.state.status()
    }

    /// Return a reusable zero-copy article view without repeating validation.
    #[must_use]
    pub fn article(&self) -> ArticleView<'_> {
        self.state.article()
    }

    /// Return the validated wire bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.state.as_bytes()
    }

    /// Consume the validated state and return its pooled storage.
    #[must_use]
    pub fn into_bytes(self) -> PooledBuffer {
        self.state.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_stat_response_success() {
        use crate::protocol::StatusCode;
        // Article exists (223)
        assert!(NntpClient::parse_stat_response(StatusCode::parse(b"223").unwrap()).unwrap());

        // Article not found (430)
        assert!(!NntpClient::parse_stat_response(StatusCode::parse(b"430").unwrap()).unwrap());
    }

    #[test]
    fn test_parse_stat_response_errors() {
        use crate::protocol::StatusCode;
        // Unexpected codes
        assert!(NntpClient::parse_stat_response(StatusCode::parse(b"500").unwrap()).is_err());
        assert!(NntpClient::parse_stat_response(StatusCode::parse(b"200").unwrap()).is_err());
        assert!(NntpClient::parse_stat_response(StatusCode::parse(b"400").unwrap()).is_err());
    }

    async fn spawn_fetch_test_server(
        expected_command: &'static str,
        response: &'static [u8],
    ) -> std::net::SocketAddr {
        spawn_fetch_test_server_with_chunk_size(expected_command, response, response.len()).await
    }

    async fn spawn_fetch_test_server_with_chunk_size(
        expected_command: &'static str,
        response: &'static [u8],
        chunk_size: usize,
    ) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            loop {
                if let Ok((mut stream, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        let _ = stream.write_all(b"200 mock\r\n").await;
                        let mut cmd_buf = [0u8; 1024];
                        loop {
                            let Ok(n) = stream.read(&mut cmd_buf).await else {
                                return;
                            };
                            if n == 0 {
                                return;
                            }

                            let command = std::str::from_utf8(&cmd_buf[..n]).unwrap();
                            if command.starts_with(expected_command) {
                                for chunk in response.chunks(chunk_size.max(1)) {
                                    let _ = stream.write_all(chunk).await;
                                    tokio::task::yield_now().await;
                                }
                                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                                return;
                            }

                            let _ = stream.write_all(b"200 OK\r\n").await;
                        }
                    });
                }
            }
        });

        addr
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
                        use tokio::io::AsyncReadExt;
                        let _ = stream.write_all(b"200 mock\r\n").await;
                        // Respond to negotiation commands (MODE READER, etc.) while
                        // waiting for the test to signal that pool.get() has returned.
                        let mut cmd_buf = vec![0u8; 256];
                        loop {
                            tokio::select! {
                                () = wake.notified() => break,
                                result = stream.read(&mut cmd_buf) => {
                                    match result {
                                        Ok(n) if n > 0 => { let _ = stream.write_all(b"200 OK\r\n").await; }
                                        _ => break,
                                    }
                                }
                            }
                        }
                        let _ = stream.write_all(article_data).await;
                        // Keep alive so recycle's try_read sees WouldBlock
                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    });
                }
            }
        });

        (addr, notify)
    }

    async fn spawn_truncated_test_server(
        article_prefix: &'static [u8],
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
                        use tokio::io::AsyncReadExt;
                        let _ = stream.write_all(b"200 mock\r\n").await;
                        // Respond to negotiation commands (MODE READER, etc.) while
                        // waiting for the test to signal that pool.get() has returned.
                        let mut cmd_buf = vec![0u8; 256];
                        loop {
                            tokio::select! {
                                () = wake.notified() => break,
                                result = stream.read(&mut cmd_buf) => {
                                    match result {
                                        Ok(n) if n > 0 => { let _ = stream.write_all(b"200 OK\r\n").await; }
                                        _ => break,
                                    }
                                }
                            }
                        }
                        let _ = stream.write_all(article_prefix).await;
                        let _ = stream.shutdown().await;
                    });
                }
            }
        });

        (addr, notify)
    }

    fn make_test_pool(addr: std::net::SocketAddr) -> crate::pool::deadpool_connection::Pool {
        let manager = crate::pool::deadpool_connection::TcpManager::new(
            addr.ip().to_string(),
            addr.port(),
            "test".to_string(),
            crate::pool::deadpool_connection::TcpManagerOptions {
                compress: Some(false), // disable compression — mock doesn't handle it
                ..crate::pool::deadpool_connection::TcpManagerOptions::default()
            },
        )
        .unwrap();
        crate::pool::deadpool_connection::Pool::builder(manager)
            .max_size(2)
            .build()
            .unwrap()
    }

    fn make_test_client_with_buffer_size(
        addr: std::net::SocketAddr,
        buffer_size: usize,
    ) -> NntpClient {
        use crate::pool::BufferPool;
        use crate::types::BufferSize;

        let provider = DeadpoolConnectionProvider::builder(addr.ip().to_string(), addr.port())
            .name("test")
            .max_connections(2)
            .build()
            .unwrap();
        let buffer_pool = BufferPool::new(BufferSize::try_new(buffer_size).unwrap(), 2);
        NntpClient::new(provider, buffer_pool)
    }

    fn make_test_client(addr: std::net::SocketAddr) -> NntpClient {
        make_test_client_with_buffer_size(addr, 4096)
    }

    async fn capture_multiline_response_for_test(
        conn: &mut crate::stream::ConnectionStream,
        io_buffer: &mut PooledBuffer,
        capture: &mut PooledBuffer,
    ) -> Result<()> {
        crate::session::multiline_framing::capture_isolated_multiline_response(
            conn, io_buffer, capture,
        )
        .await
    }

    /// Verify the session response reader captures the complete response when it all
    /// arrives in the first pre-read buffer.
    #[tokio::test]
    async fn test_multiline_response_capture_single_read() {
        use crate::pool::BufferPool;
        use crate::types::BufferSize;

        let article = b"220 body follows\r\nHello world\r\n.\r\n";
        let (addr, notify) = spawn_test_server(article).await;
        let pool = make_test_pool(addr);
        let buffer_pool = BufferPool::new(BufferSize::try_new(4096).unwrap(), 2);

        let mut conn = pool.get().await.unwrap();
        // Signal server to send article data now that the greeting is consumed
        notify.notify_one();

        let mut io_buffer = buffer_pool.acquire();
        let mut capture = buffer_pool.acquire_capture();

        // Simulate send_request reading a complete response into the buffer.
        io_buffer.read_from(&mut *conn).await.unwrap();

        capture_multiline_response_for_test(&mut conn, &mut io_buffer, &mut capture)
            .await
            .unwrap();

        assert_eq!(&capture[..], article as &[u8]);
    }

    /// Verify response capture accumulates correctly across multiple reads,
    /// including when the response body completes across multiple reads.
    ///
    /// Uses an 8-byte I/O buffer against a 36-byte article, forcing 5 reads.
    /// Read 4 ends in the middle of the response body end, exercising response capture
    /// response-reader state.
    #[tokio::test]
    async fn test_multiline_response_capture_multi_read_spanning_body_end() {
        use crate::pool::BufferPool;
        use crate::types::BufferSize;

        // 36 bytes total: 5 × 8-byte reads with 8-byte io_buffer.
        // The response body end spans two reads.
        let article = b"220 article\r\nLine one\r\nLine two\r\n.\r\n";
        let (addr, notify) = spawn_test_server(article).await;
        let pool = make_test_pool(addr);
        // Tiny I/O buffer forces multiple reads while capturing the complete response.
        let buffer_pool = BufferPool::new(BufferSize::try_new(8).unwrap(), 4);

        let mut conn = pool.get().await.unwrap();
        notify.notify_one();

        let mut io_buffer = buffer_pool.acquire();
        let mut capture = buffer_pool.acquire_capture();

        // No response bytes have been read into the buffer yet.
        capture_multiline_response_for_test(&mut conn, &mut io_buffer, &mut capture)
            .await
            .unwrap();

        assert_eq!(&capture[..], article as &[u8]);
    }

    #[tokio::test]
    async fn test_multiline_response_capture_errors_on_truncated_response() {
        use crate::pool::BufferPool;
        use crate::types::BufferSize;

        let article_prefix = b"220 body follows\r\npartial article";
        let (addr, notify) = spawn_truncated_test_server(article_prefix).await;
        let pool = make_test_pool(addr);
        let buffer_pool = BufferPool::new(BufferSize::try_new(8).unwrap(), 4);

        let mut conn = pool.get().await.unwrap();
        notify.notify_one();

        let mut io_buffer = buffer_pool.acquire();
        let mut capture = buffer_pool.acquire_capture();

        let err = capture_multiline_response_for_test(&mut conn, &mut io_buffer, &mut capture)
            .await
            .unwrap_err();

        assert!(
            err.to_string()
                .contains("Backend closed connection before complete"),
            "unexpected error: {err:#}"
        );
    }

    #[tokio::test]
    async fn test_multiline_response_capture_errors_on_extra_response_bytes() {
        use crate::pool::BufferPool;
        use crate::types::BufferSize;

        let article = b"220 body follows\r\nHello world\r\n.\r\n";
        let extra_response = [article.as_slice(), b"430 No such article\r\n"].concat();
        let extra_response: &'static [u8] = Box::leak(extra_response.into_boxed_slice());
        let (addr, notify) = spawn_test_server(extra_response).await;
        let pool = make_test_pool(addr);
        let buffer_pool = BufferPool::new(BufferSize::try_new(4096).unwrap(), 2);

        let mut conn = pool.get().await.unwrap();
        notify.notify_one();

        let mut io_buffer = buffer_pool.acquire();
        let mut capture = buffer_pool.acquire_capture();
        io_buffer.read_from(&mut *conn).await.unwrap();

        let err = capture_multiline_response_for_test(&mut conn, &mut io_buffer, &mut capture)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("unexpected"));
    }

    #[tokio::test]
    async fn fetch_head_reads_multiline_response() {
        let response = b"221 0 <test@example.com>\r\nSubject: test\r\nFrom: tester\r\n\r\n.\r\n";
        let addr = spawn_fetch_test_server("HEAD <test@example.com>", response).await;
        let client = make_test_client(addr);
        let msg_id = crate::types::MessageId::new("<test@example.com>".to_string()).unwrap();

        let framed = client.fetch_head(&msg_id).await.unwrap();

        assert_eq!(framed.as_bytes(), &response[..response.len() - 5]);
    }

    #[tokio::test]
    async fn fetch_body_reads_multiline_response() {
        let response = b"222 0 <test@example.com>\r\nhello world\r\n.\r\n";
        let addr = spawn_fetch_test_server("BODY <test@example.com>", response).await;
        let client = make_test_client(addr);
        let msg_id = crate::types::MessageId::new("<test@example.com>".to_string()).unwrap();

        let framed = client.fetch_body(&msg_id).await.unwrap();

        assert_eq!(framed.as_bytes(), &response[..response.len() - 5]);
        let validated = framed.validate_with_yenc(YencValidation::Disabled).unwrap();
        assert_eq!(validated.kind(), crate::protocol::RequestKind::Body);
        assert_eq!(validated.status(), StatusCode::new(222));
        let article = validated.article();
        assert_eq!(article.body, Some(&b"hello world"[..]));
    }

    #[tokio::test]
    async fn fetch_body_validates_after_fragmented_production_capture() {
        let response = b"222 0 <fragmented@example.com>\r\nfragmented body\r\n.\r\n";
        let addr =
            spawn_fetch_test_server_with_chunk_size("BODY <fragmented@example.com>", response, 1)
                .await;
        let client = make_test_client_with_buffer_size(addr, 4096);
        let msg_id = crate::types::MessageId::new("<fragmented@example.com>".to_string()).unwrap();

        let validated = client
            .fetch_body(&msg_id)
            .await
            .unwrap()
            .validate_with_yenc(YencValidation::Disabled)
            .unwrap();

        assert_eq!(validated.article().body, Some(&b"fragmented body"[..]));
    }

    #[tokio::test]
    async fn fetch_article_returns_a_validated_article_view() {
        let response = b"220 42 <article@example.com> article follows\r\n\
            Subject: test\r\n\
            From: tester\r\n\
            \r\n\
            article body\r\n\
            .\r\n";
        let addr = spawn_fetch_test_server("ARTICLE <article@example.com>", response).await;
        let client = make_test_client(addr);
        let msg_id = crate::types::MessageId::new("<article@example.com>".to_string()).unwrap();

        let validated = client
            .fetch_article(&msg_id)
            .await
            .unwrap()
            .validate_with_yenc(YencValidation::Disabled)
            .unwrap();
        assert_eq!(validated.kind(), crate::protocol::RequestKind::Article);
        assert_eq!(validated.status(), StatusCode::new(220));
        let article = validated.article();

        assert_eq!(article.article_number, Some(42));
        assert_eq!(article.message_id.as_str(), "<article@example.com>");
        assert_eq!(article.headers.unwrap().get("Subject"), Some(&b"test"[..]));
        assert_eq!(article.body, Some(&b"article body"[..]));
    }

    #[tokio::test]
    async fn stat_consumes_a_complete_single_line_response() {
        let response = b"223 42 <stat@example.com> article exists\r\n";
        let addr = spawn_fetch_test_server("STAT <stat@example.com>", response).await;
        let client = make_test_client(addr);
        let msg_id = crate::types::MessageId::new("<stat@example.com>".to_string()).unwrap();

        assert!(client.stat(&msg_id).await.unwrap());
    }

    #[tokio::test]
    async fn fetch_head_can_be_parsed_by_the_documented_article_api() {
        let response = b"221 0 <test@example.com>\r\nSubject: test\r\nFrom: tester\r\n.\r\n";
        let addr = spawn_fetch_test_server("HEAD <test@example.com>", response).await;
        let client = make_test_client(addr);
        let msg_id = crate::types::MessageId::new("<test@example.com>".to_string()).unwrap();

        let framed = client.fetch_head(&msg_id).await.unwrap();
        let validated = framed.validate_with_yenc(YencValidation::Disabled).unwrap();
        let article = validated.article();
        assert_eq!(article.body, None);
        assert_eq!(article.headers.unwrap().get("Subject"), Some(&b"test"[..]));
    }

    #[tokio::test]
    async fn fetch_body_preserves_wire_dot_stuffing_for_the_article_decoder() {
        let response = b"222 0 <dotted@example.com>\r\n..wire-dot\r\n.\r\n";
        let addr = spawn_fetch_test_server("BODY <dotted@example.com>", response).await;
        let client = make_test_client(addr);
        let msg_id = crate::types::MessageId::new("<dotted@example.com>".to_string()).unwrap();

        let framed = client.fetch_body(&msg_id).await.unwrap();
        assert_eq!(framed.as_bytes(), &response[..response.len() - 5]);
        let validated = framed.validate_with_yenc(YencValidation::Disabled).unwrap();
        let article = validated.article();
        assert_eq!(article.body, Some(&b"..wire-dot"[..]));
    }

    #[tokio::test]
    async fn validated_article_view_is_reusable_without_revalidation() {
        let response = b"222 0 <repeat@example.com>\r\nhello world\r\n.\r\n";
        let addr = spawn_fetch_test_server("BODY <repeat@example.com>", response).await;
        let client = make_test_client(addr);
        let msg_id = crate::types::MessageId::new("<repeat@example.com>".to_string()).unwrap();

        let validated = client
            .fetch_body(&msg_id)
            .await
            .unwrap()
            .validate_with_yenc(YencValidation::Disabled)
            .unwrap();
        let first = validated.article();
        let second = validated.article();

        assert_eq!(first, second);
        assert_eq!(first.body, Some(&b"hello world"[..]));
    }

    #[tokio::test]
    async fn fetch_body_reads_multiline_response_above_retention_limit() {
        let mut response = Vec::with_capacity((4 * 1024 * 1024) + 64);
        response.extend_from_slice(b"222 0 <large@example.com>\r\n");
        response.extend(std::iter::repeat_n(b'x', 4 * 1024 * 1024));
        response.extend_from_slice(b"\r\n.\r\n");
        let response: &'static [u8] = Box::leak(response.into_boxed_slice());
        let addr = spawn_fetch_test_server("BODY <large@example.com>", response).await;
        let client = make_test_client(addr);
        let msg_id = crate::types::MessageId::new("<large@example.com>".to_string()).unwrap();

        let framed = client.fetch_body(&msg_id).await.unwrap();

        assert_eq!(framed.as_bytes(), &response[..response.len() - 5]);
    }
}
