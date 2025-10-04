//! Response demultiplexing for multiplexed backend connections
//!
//! This module handles reading responses from backend servers and
//! routing them back to the correct clients based on request tracking.

use anyhow::Result;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::mpsc;
use tracing::{debug, error};

use crate::constants::buffer;
use crate::protocol::NntpResponse;
use crate::types::{BackendId, ClientId, RequestId};

/// Message containing a response to be sent to a client
#[derive(Debug)]
#[allow(dead_code)]
pub struct ClientResponse {
    /// The client that should receive this response
    pub client_id: ClientId,
    /// The complete response data
    pub data: Vec<u8>,
    /// Whether this response is complete (no more data expected)
    pub complete: bool,
}

/// Demultiplexer for routing backend responses to clients
#[allow(dead_code)]
pub struct ResponseDemultiplexer {
    /// Backend ID this demultiplexer is handling
    backend_id: BackendId,
    /// Channel to send responses back to clients
    response_tx: mpsc::UnboundedSender<ClientResponse>,
}

#[allow(dead_code)]
impl ResponseDemultiplexer {
    /// Create a new response demultiplexer
    pub fn new(backend_id: BackendId, response_tx: mpsc::UnboundedSender<ClientResponse>) -> Self {
        Self {
            backend_id,
            response_tx,
        }
    }

    /// Read a single NNTP response from the backend stream
    /// Returns the response data and whether it's multiline
    async fn read_response<R: AsyncRead + Unpin>(
        &self,
        stream: &mut R,
        buffer: &mut Vec<u8>,
    ) -> Result<(Vec<u8>, bool)> {
        buffer.clear();
        // Pre-allocate typical response size to reduce reallocations
        let mut response_data = Vec::with_capacity(buffer::RESPONSE_INITIAL_CAPACITY);
        let mut temp_buf = [0u8; 8192];

        // Read the status line
        loop {
            let n = stream.read(&mut temp_buf).await?;
            if n == 0 {
                anyhow::bail!("Connection closed while reading response");
            }

            response_data.extend_from_slice(&temp_buf[..n]);

            // Check if we have a complete line (ending with \r\n)
            if response_data.ends_with(b"\r\n") {
                break;
            }
        }

        // Parse the status line to determine if multiline
        let status_code = NntpResponse::parse_status_code(&response_data)
            .ok_or_else(|| anyhow::anyhow!("Invalid NNTP response"))?;
        let is_multiline = NntpResponse::is_multiline_response(status_code);

        // If multiline, read until we see ".\r\n"
        if is_multiline {
            // Check if we already have the complete multiline response
            if !NntpResponse::has_multiline_terminator(&response_data) {
                loop {
                    let n = stream.read(&mut temp_buf).await?;
                    if n == 0 {
                        // Check one more time if we have the terminator
                        if NntpResponse::has_multiline_terminator(&response_data) {
                            break;
                        }
                        anyhow::bail!("Connection closed while reading multiline response");
                    }

                    response_data.extend_from_slice(&temp_buf[..n]);

                    // Check for multiline terminator
                    if NntpResponse::has_multiline_terminator(&response_data) {
                        break;
                    }
                }
            }
        }

        Ok((response_data, is_multiline))
    }

    /// Route a response to the appropriate client
    pub fn route_response(
        &self,
        client_id: ClientId,
        request_id: RequestId,
        data: Vec<u8>,
        complete: bool,
    ) -> Result<()> {
        debug!(
            "Routing response for request {:?} to client {:?} ({} bytes, complete={})",
            request_id,
            client_id,
            data.len(),
            complete
        );

        let response = ClientResponse {
            client_id,
            data,
            complete,
        };

        self.response_tx.send(response).map_err(|e| {
            error!("Failed to send response to client: {}", e);
            anyhow::anyhow!("Failed to route response to client")
        })?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_demux_creation() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let backend_id = BackendId::from_index(0);
        let demux = ResponseDemultiplexer::new(backend_id, tx);
        assert_eq!(demux.backend_id, backend_id);
    }

    #[tokio::test]
    async fn test_route_simple_response() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let backend_id = BackendId::from_index(0);
        let demux = ResponseDemultiplexer::new(backend_id, tx);

        let client_id = ClientId::new();
        let request_id = RequestId::new();
        let data = b"200 Server ready\r\n".to_vec();

        demux
            .route_response(client_id, request_id, data.clone(), true)
            .unwrap();

        let response = rx.recv().await.unwrap();
        assert_eq!(response.client_id, client_id);
        assert_eq!(response.data, data);
        assert!(response.complete);
    }

    #[tokio::test]
    async fn test_read_single_line_response() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let backend_id = BackendId::from_index(0);
        let demux = ResponseDemultiplexer::new(backend_id, tx);

        let response_data = b"211 1234 3000 3002 alt.test\r\n";
        let mut cursor = std::io::Cursor::new(response_data);
        let mut buffer = Vec::new();

        let (data, is_multiline) = demux.read_response(&mut cursor, &mut buffer).await.unwrap();
        assert_eq!(data, response_data);
        assert!(!is_multiline);
    }

    #[tokio::test]
    async fn test_read_multiline_response() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let backend_id = BackendId::from_index(0);
        let demux = ResponseDemultiplexer::new(backend_id, tx);

        let response_data = b"215 List of newsgroups follows\r\nalt.test A test group\r\nalt.test2 Another\r\n.\r\n";
        let mut cursor = std::io::Cursor::new(response_data);
        let mut buffer = Vec::new();

        let (data, is_multiline) = demux.read_response(&mut cursor, &mut buffer).await.unwrap();
        assert_eq!(data, response_data);
        assert!(is_multiline);
    }

    #[tokio::test]
    async fn test_multiline_with_embedded_dots() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let backend_id = BackendId::from_index(0);
        let demux = ResponseDemultiplexer::new(backend_id, tx);

        // Response with a line starting with a dot (should be dot-stuffed)
        let response_data = b"220 0 <test@example.com> article\r\n..This line starts with a dot\r\nNormal line\r\n.\r\n";
        let mut cursor = std::io::Cursor::new(response_data);
        let mut buffer = Vec::new();

        let (data, is_multiline) = demux.read_response(&mut cursor, &mut buffer).await.unwrap();
        assert_eq!(data, response_data);
        assert!(is_multiline);
    }

    #[tokio::test]
    async fn test_route_multiple_responses() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let backend_id = BackendId::from_index(0);
        let demux = ResponseDemultiplexer::new(backend_id, tx);

        let client1 = ClientId::new();
        let client2 = ClientId::new();
        let req1 = RequestId::new();
        let req2 = RequestId::new();

        demux
            .route_response(client1, req1, b"200 OK\r\n".to_vec(), true)
            .unwrap();
        demux
            .route_response(client2, req2, b"215 List\r\n".to_vec(), false)
            .unwrap();

        let resp1 = rx.recv().await.unwrap();
        assert_eq!(resp1.client_id, client1);
        assert!(resp1.complete);

        let resp2 = rx.recv().await.unwrap();
        assert_eq!(resp2.client_id, client2);
        assert!(!resp2.complete);
    }
}
