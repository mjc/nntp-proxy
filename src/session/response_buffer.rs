//! Buffered NNTP response readers for pooled backend connections.
//!
//! This module owns the request-aware response buffering logic used by
//! per-command and pipeline paths.

use std::borrow::Cow;
use std::collections::VecDeque;

pub(crate) use crate::session::multiline_framing::{
    CAPABILITIES_WITH_AUTHINFO_RESPONSE, CAPABILITIES_WITHOUT_AUTHINFO_RESPONSE,
};

#[derive(Debug)]
enum OrderedResponse {
    Backend,
    Local(Vec<u8>),
}

#[derive(Default, Debug)]
pub(crate) struct BackendResponseOrder {
    inner: crate::session::multiline_framing::BackendReplyTracker,
    ordered: VecDeque<OrderedResponse>,
}

impl BackendResponseOrder {
    pub(crate) fn push_request(&mut self, kind: crate::protocol::RequestKind) {
        self.inner.push_request(kind);
        self.ordered.push_back(OrderedResponse::Backend);
    }

    #[inline]
    pub(crate) fn has_pending_backend_replies(&self) -> bool {
        self.ordered
            .iter()
            .any(|reply| matches!(reply, OrderedResponse::Backend))
    }

    pub(crate) fn push_deferred_reply(&mut self, reply: impl Into<Vec<u8>>) {
        self.ordered.push_back(OrderedResponse::Local(reply.into()));
    }

    #[inline]
    pub(crate) fn has_deferred_replies(&self) -> bool {
        self.ordered
            .iter()
            .any(|reply| matches!(reply, OrderedResponse::Local(_)))
    }

    pub(crate) fn should_drain_backend_replies(&self) -> bool {
        let has_deferred_reply_behind_front = self
            .ordered
            .iter()
            .skip(1)
            .any(|reply| matches!(reply, OrderedResponse::Local(_)));

        matches!(self.ordered.front(), Some(OrderedResponse::Backend))
            && has_deferred_reply_behind_front
    }

    pub(crate) fn take_ready_deferred_replies(&mut self) -> Vec<Vec<u8>> {
        let mut replies = Vec::new();
        while let Some(OrderedResponse::Local(_)) = self.ordered.front() {
            let Some(OrderedResponse::Local(reply)) = self.ordered.pop_front() else {
                break;
            };
            replies.push(reply);
        }
        replies
    }

    pub(crate) fn client_writes_for_backend_read<'a>(
        &mut self,
        backend_read: &'a [u8],
    ) -> Vec<Cow<'a, [u8]>> {
        let mut writes = Vec::new();

        for reply in self.take_ready_deferred_replies() {
            writes.push(Cow::Owned(reply));
        }

        for reply in self.inner.accept_backend_bytes(backend_read) {
            match reply {
                crate::session::multiline_framing::BackendReplyBytes::CompletedTrackedReply(
                    bytes,
                ) => {
                    writes.push(Cow::Borrowed(bytes));
                    if matches!(self.ordered.front(), Some(OrderedResponse::Backend)) {
                        self.ordered.pop_front();
                    }
                    for reply in self.take_ready_deferred_replies() {
                        writes.push(Cow::Owned(reply));
                    }
                }
                crate::session::multiline_framing::BackendReplyBytes::ForwardUntracked(bytes) => {
                    writes.push(Cow::Borrowed(bytes));
                }
            }
        }

        writes
    }
}

#[must_use]
pub(crate) fn cached_response_completion() -> std::io::IoSlice<'static> {
    crate::session::multiline_framing::cached_response_completion()
}

#[must_use]
pub(crate) fn complete_response_body(payload: &[u8]) -> Option<&[u8]> {
    crate::session::multiline_framing::complete_response_body(payload)
}

/// Outcome of buffering a backend response.
#[derive(Debug)]
pub enum ResponseTransferError {
    /// Client disconnected after the proxy had already buffered a complete
    /// backend response and started writing it locally.
    ClientDisconnect(std::io::Error),

    /// Client disconnected while backend response capture was still in progress.
    ///
    /// The client-facing error is still a normal disconnect, but the backend
    /// connection cannot be reused because unread response bytes may still be in
    /// flight.
    ClientDisconnectBeforeResponseComplete(std::io::Error),

    /// Backend closed connection before sending a complete multiline response.
    BackendEof {
        backend_id: crate::types::BackendId,
        bytes_received: u64,
    },

    /// Other I/O / protocol error.
    Io(anyhow::Error),
}

impl ResponseTransferError {
    pub(crate) const fn must_remove_connection(&self) -> bool {
        !matches!(self, Self::ClientDisconnect(_))
    }

    pub(crate) fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::ClientDisconnect(io_err)
            | Self::ClientDisconnectBeforeResponseComplete(io_err) => anyhow::Error::from(io_err),
            Self::Io(e) => e,
            Self::BackendEof {
                backend_id,
                bytes_received,
            } => anyhow::anyhow!(
                "Backend {backend_id:?} closed connection before complete multiline response \
                 ({bytes_received} bytes received)"
            ),
        }
    }
}

impl std::fmt::Display for ResponseTransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClientDisconnect(e) | Self::ClientDisconnectBeforeResponseComplete(e) => {
                write!(f, "client disconnected: {e}")
            }
            Self::BackendEof {
                backend_id,
                bytes_received,
            } => write!(
                f,
                "backend {backend_id:?} closed connection before complete multiline response \
                 ({bytes_received} bytes received)"
            ),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ResponseTransferError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ClientDisconnect(e) | Self::ClientDisconnectBeforeResponseComplete(e) => Some(e),
            Self::BackendEof { .. } => None,
            Self::Io(e) => e.source(),
        }
    }
}

pub(crate) enum ResponseConnectionReuse {
    Reusable,
    QueuedBytes { len: usize },
}

#[must_use]
pub(crate) fn connection_reuse_after_response(
    conn: &crate::pool::ConnectionGuard,
) -> ResponseConnectionReuse {
    if conn.has_pending_bytes() {
        ResponseConnectionReuse::QueuedBytes {
            len: conn.pending_bytes_len(),
        }
    } else {
        ResponseConnectionReuse::Reusable
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::BufferSize;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    fn make_pool() -> crate::pool::BufferPool {
        crate::pool::BufferPool::new(BufferSize::try_new(65536).unwrap(), 2)
    }

    async fn buffer_initial_read_for_test(
        conn: &mut crate::stream::ConnectionStream,
        first_chunk: &[u8],
        request_line: &[u8],
        status_code: u16,
        pool: &crate::pool::BufferPool,
    ) -> Result<crate::pool::ChunkedResponse, ResponseTransferError> {
        let request = crate::protocol::RequestContext::parse(request_line)
            .expect("test request should parse");
        let mut io_buffer = pool.acquire();
        io_buffer.copy_from_slice(first_chunk);
        let mut captured = crate::pool::ChunkedResponse::default();
        crate::session::multiline_framing::InitialResponseCapture::from_buffer(
            &request,
            crate::protocol::StatusCode::new(status_code),
            &mut io_buffer,
            conn,
            &mut captured,
            pool,
            crate::types::BackendId::from_index(1),
        )
        .capture()
        .await?;
        Ok(captured)
    }

    async fn mock_backend_conn(chunks: Vec<Vec<u8>>) -> crate::stream::ConnectionStream {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            for chunk in chunks {
                stream.write_all(&chunk).await.unwrap();
            }
            stream.shutdown().await.unwrap();
        });

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        crate::stream::ConnectionStream::plain(stream)
    }

    #[tokio::test]
    async fn buffered_multiline_response_returns_complete_response() {
        let response = b"220 Article follows\r\nLine 1\r\nLine 2\r\n.\r\n";
        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![]).await;

        let captured = buffer_initial_read_for_test(
            &mut conn,
            response,
            b"ARTICLE <test@example>\r\n",
            220,
            &pool,
        )
        .await
        .unwrap();

        assert_eq!(captured.to_vec(), response);
    }

    #[tokio::test]
    async fn buffered_multiline_response_handles_empty_body() {
        let response = b"220 0 Article follows\r\n.\r\n";
        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![]).await;

        let captured = buffer_initial_read_for_test(
            &mut conn,
            response,
            b"ARTICLE <test@example>\r\n",
            220,
            &pool,
        )
        .await
        .unwrap();

        assert_eq!(captured.to_vec(), response);
    }

    #[tokio::test]
    async fn buffered_multiline_response_preserves_next_reply_from_initial_read() {
        let first_response = b"220 Article follows\r\nLine 1\r\n.\r\n";
        let next_reply = b"223 0 <next@example>\r\n";
        let mut combined = Vec::from(first_response.as_slice());
        combined.extend_from_slice(next_reply);

        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![]).await;

        let captured = buffer_initial_read_for_test(
            &mut conn,
            &combined,
            b"ARTICLE <test@example>\r\n",
            220,
            &pool,
        )
        .await
        .unwrap();

        assert_eq!(captured.to_vec(), first_response);
        assert!(conn.has_pending_bytes());
    }

    #[tokio::test]
    async fn buffered_multiline_response_preserves_next_reply_from_later_read() {
        let first_chunk = b"220 Article follows\r\nLine 1\r\n";
        let tail_chunk = b".\r\n223 0 <next@example>\r\n".to_vec();

        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![tail_chunk]).await;

        let captured = buffer_initial_read_for_test(
            &mut conn,
            first_chunk,
            b"ARTICLE <test@example>\r\n",
            220,
            &pool,
        )
        .await
        .unwrap();

        assert_eq!(captured.to_vec(), b"220 Article follows\r\nLine 1\r\n.\r\n");
        assert!(conn.has_pending_bytes());
    }

    #[tokio::test]
    async fn buffered_multiline_response_handles_large_article_across_chunks() {
        let header = b"220 Article follows\r\n";
        let mut body = Vec::new();
        for i in 0..1000 {
            body.extend_from_slice(format!("Line {i}\r\n").as_bytes());
        }

        let mut expected_response = Vec::new();
        expected_response.extend_from_slice(header);
        expected_response.extend_from_slice(&body);
        expected_response.extend_from_slice(b".\r\n");

        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![expected_response[header.len()..].to_vec()]).await;

        let captured = buffer_initial_read_for_test(
            &mut conn,
            header,
            b"ARTICLE <test@example>\r\n",
            220,
            &pool,
        )
        .await
        .expect("large multiline response should buffer successfully");

        assert_eq!(captured.to_vec(), expected_response);
    }

    #[tokio::test]
    async fn buffered_multiline_response_moves_scratch_buffers_across_chunks() {
        let header = b"220 Article follows\r\n";
        let mut body_chunk = vec![b'x'; 94];
        body_chunk.extend_from_slice(b"\r\n");
        let mut expected_response = Vec::new();
        expected_response.extend_from_slice(header);
        expected_response.extend_from_slice(&body_chunk);
        expected_response.extend_from_slice(b".\r\n");

        let pool = crate::pool::BufferPool::new(BufferSize::try_new(64).unwrap(), 2)
            .with_capture_pool(64, 4);
        let mut io_buffer = pool.acquire();
        io_buffer.copy_from_slice(header);
        let mut captured = crate::pool::ChunkedResponse::default();
        let mut conn = mock_backend_conn(vec![body_chunk.clone(), b".\r\n".to_vec()]).await;
        let request = crate::protocol::RequestContext::parse(b"ARTICLE <test@example>\r\n")
            .expect("valid request");

        crate::session::multiline_framing::InitialResponseCapture::from_buffer(
            &request,
            crate::protocol::StatusCode::new(220),
            &mut io_buffer,
            &mut conn,
            &mut captured,
            &pool,
            crate::types::BackendId::from_index(1),
        )
        .capture()
        .await
        .expect("multiline buffering should succeed");

        assert_eq!(captured.to_vec(), expected_response);
        assert_eq!(
            pool.available_buffers(),
            0,
            "buffered multiline response should hold pooled read buffers instead of copying into capture buffers"
        );
    }

    #[tokio::test]
    async fn buffered_multiline_response_errors_on_full_buffer_mid_response() {
        let first_chunk = b"220 Article follows\r\nLong body content here";
        let expected_tail = b" more body\r\n.\r\n";
        let mut second_chunk = expected_tail.to_vec();
        second_chunk.resize(4096, b'X');

        let pool =
            crate::pool::BufferPool::new(crate::types::BufferSize::try_new(4096).unwrap(), 2);
        let mut conn = mock_backend_conn(vec![second_chunk]).await;

        let err = buffer_initial_read_for_test(
            &mut conn,
            first_chunk,
            b"ARTICLE <test@example>\r\n",
            220,
            &pool,
        )
        .await
        .unwrap_err();

        assert!(matches!(err, ResponseTransferError::BackendEof { .. }));
        assert!(!conn.has_pending_bytes());
    }

    #[tokio::test]
    async fn buffered_response_reader_completes_request_context() {
        let response = b"223 0 <test@example>\r\n";
        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![]).await;
        let backend_id = crate::types::BackendId::from_index(1);
        let mut request = crate::protocol::RequestContext::parse(b"STAT <test@example>\r\n")
            .expect("valid request line");

        let captured = buffer_initial_read_for_test(
            &mut conn,
            response,
            b"STAT <test@example>\r\n",
            223,
            &pool,
        )
        .await
        .unwrap();
        request.complete_backend_response(
            backend_id,
            crate::protocol::StatusCode::new(223),
            captured,
        );

        assert_eq!(
            request.response_status(),
            Some(crate::protocol::StatusCode::new(223))
        );
        assert_eq!(request.backend_id(), Some(backend_id));
        assert_eq!(request.response_payload_eq(response), Some(true));
    }

    #[tokio::test]
    async fn buffered_multiline_response_errors_on_truncated_backend() {
        let partial = b"220 Article follows\r\nIncomplete body\r\n";
        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![]).await;

        let result = buffer_initial_read_for_test(
            &mut conn,
            partial,
            b"ARTICLE <test@example>\r\n",
            220,
            &pool,
        )
        .await;
        assert!(
            matches!(result, Err(ResponseTransferError::BackendEof { .. })),
            "EOF before complete response must be BackendEof"
        );
    }

    #[tokio::test]
    async fn article_430_response_is_buffered_as_single_line() {
        let pool = make_pool();
        let combined = b"430 No article with that message-id\r\n223 0 <next@example>\r\n";
        let mut conn = mock_backend_conn(vec![]).await;

        let buffered = buffer_initial_read_for_test(
            &mut conn,
            combined,
            b"ARTICLE <test@example>\r\n",
            430,
            &pool,
        )
        .await
        .expect("430 article miss should stay single-line");

        assert_eq!(
            buffered.to_vec(),
            b"430 No article with that message-id\r\n"
        );
        assert!(conn.has_pending_bytes());
        assert_eq!(conn.pending_bytes_len(), b"223 0 <next@example>\r\n".len());
    }

    #[tokio::test]
    async fn buffered_response_handles_prefilled_pending_bytes_article_with_queued_pending_bytes() {
        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![]).await;

        let result = buffer_initial_read_for_test(
            &mut conn,
            b"220 Article follows\r\nbody\r\n.\r\n223 0 <next@example>\r\n",
            b"ARTICLE <test@example>\r\n",
            220,
            &pool,
        )
        .await;

        assert!(result.is_ok());
        assert!(conn.has_pending_bytes());
    }

    #[tokio::test]
    async fn single_line_buffering_preserves_next_reply() {
        let pool = make_pool();
        let combined = b"223 0 <test@example>\r\n430 No article with that message-id\r\n";
        let mut conn = mock_backend_conn(vec![]).await;

        let buffered = buffer_initial_read_for_test(
            &mut conn,
            combined,
            b"STAT <test@example>\r\n",
            223,
            &pool,
        )
        .await
        .expect("single-line response should buffer cleanly");

        assert_eq!(buffered.to_vec(), b"223 0 <test@example>\r\n");
        assert!(conn.has_pending_bytes());
        assert_eq!(
            conn.pending_bytes_len(),
            b"430 No article with that message-id\r\n".len()
        );
    }

    #[tokio::test]
    async fn single_line_buffering_keeps_pending_bytes_out_of_pool_buffers() {
        let pool = crate::pool::BufferPool::new(crate::types::BufferSize::try_new(64).unwrap(), 2);
        assert_eq!(pool.available_buffers(), 2);

        {
            let combined = b"223 0 <test@example>\r\n430 No article with that message-id\r\n";
            let mut conn = mock_backend_conn(vec![]).await;

            let buffered = buffer_initial_read_for_test(
                &mut conn,
                combined,
                b"STAT <test@example>\r\n",
                223,
                &pool,
            )
            .await
            .expect("single-line response should buffer cleanly");

            assert_eq!(buffered.to_vec(), b"223 0 <test@example>\r\n");
        }

        assert_eq!(
            pool.available_buffers(),
            2,
            "pending bytes should not retain the backing pool buffer"
        );
    }

    #[tokio::test]
    async fn buffered_multiline_response_preserves_queued_next_reply() {
        let first_chunk = b"220 Article follows\r\nLine 1\r\n.";
        let second_response = b"220 Next follows\r\nLine 2\r\n.\r\n";
        let mut later_chunk = b"\r\n".to_vec();
        later_chunk.extend_from_slice(second_response);

        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![later_chunk]).await;

        let captured = buffer_initial_read_for_test(
            &mut conn,
            first_chunk,
            b"ARTICLE <test@example>\r\n",
            220,
            &pool,
        )
        .await
        .unwrap();

        assert_eq!(captured.to_vec(), b"220 Article follows\r\nLine 1\r\n.\r\n");
        assert!(conn.has_pending_bytes());
    }

    #[test]
    fn response_transfer_error_pool_fate_matches_disconnect_semantics() {
        let disconnect = ResponseTransferError::ClientDisconnect(std::io::Error::from(
            std::io::ErrorKind::BrokenPipe,
        ));
        let dirty_disconnect = ResponseTransferError::ClientDisconnectBeforeResponseComplete(
            std::io::Error::from(std::io::ErrorKind::BrokenPipe),
        );
        let eof = ResponseTransferError::BackendEof {
            backend_id: crate::types::BackendId::from_index(0),
            bytes_received: 12,
        };
        let io = ResponseTransferError::Io(anyhow::anyhow!("backend dirty"));

        assert!(!disconnect.must_remove_connection());
        assert!(dirty_disconnect.must_remove_connection());
        assert!(eof.must_remove_connection());
        assert!(io.must_remove_connection());
    }
}
