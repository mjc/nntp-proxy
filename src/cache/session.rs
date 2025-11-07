//! Caching session handler that wraps ClientSession with caching logic

use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info};

use crate::auth::AuthHandler;
use crate::cache::article::{ArticleCache, CachedArticle};
use crate::command::{CommandHandler, NntpCommand};
use crate::constants::buffer;
use crate::protocol::{NntpResponse, ResponseCode};
use crate::types::BytesTransferred;

/// Caching session that wraps standard session with article cache
pub struct CachingSession {
    client_addr: SocketAddr,
    cache: Arc<ArticleCache>,
    auth_handler: Arc<AuthHandler>,
}

impl CachingSession {
    /// Create a new caching session
    pub fn new(
        client_addr: SocketAddr,
        cache: Arc<ArticleCache>,
        auth_handler: Arc<AuthHandler>,
    ) -> Self {
        Self {
            client_addr,
            cache,
            auth_handler,
        }
    }

    /// Handle client connection with caching support
    pub async fn handle_with_pooled_backend<T>(
        &self,
        mut client_stream: TcpStream,
        backend_conn: T,
    ) -> Result<(u64, u64)>
    where
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        use tokio::io::BufReader;

        let (client_read, mut client_write) = client_stream.split();
        let (backend_read, mut backend_write) = tokio::io::split(backend_conn);
        let mut client_reader = BufReader::new(client_read);
        let mut backend_reader = BufReader::with_capacity(buffer::POOL, backend_read);

        let mut client_to_backend_bytes = BytesTransferred::zero();
        let mut backend_to_client_bytes = BytesTransferred::zero();
        let mut line = String::with_capacity(buffer::COMMAND);
        // Pre-allocate with typical NNTP response line size (most are < 512 bytes)
        // Reduces reallocations during line reading
        // `first_line` is a Vec<u8> because it is used for reading raw bytes from the backend,
        // which may not always be valid UTF-8, whereas `line` is a String for text-based client input.
        let mut first_line = Vec::with_capacity(buffer::COMMAND);

        debug!("Caching session for client {} starting", self.client_addr);

        loop {
            line.clear();

            tokio::select! {
                result = client_reader.read_line(&mut line) => {
                    match result {
                        Ok(0) => {
                            debug!("Client {} disconnected", self.client_addr);
                            break;
                        }
                        Ok(_n) => {
                            debug!("Client {} sent command: {}", self.client_addr, line.trim());

                            // Check if this is a cacheable command (article by message-ID)
                            if matches!(NntpCommand::classify(&line), NntpCommand::ArticleByMessageId) {
                                if let Some(message_id) = NntpResponse::extract_message_id(&line) {
                                    // Check cache first
                                    if let Some(cached) = self.cache.get(&message_id).await {
                                        info!("Cache HIT for message-ID: {} (size: {} bytes)", message_id, cached.response.len());
                                        client_write.write_all(&cached.response).await?;
                                        backend_to_client_bytes.add(cached.response.len());
                                        continue;
                                    }
                                    info!("Cache MISS for message-ID: {}", message_id);
                                } else {
                                    debug!("No message-ID extracted from command: {}", line.trim());
                                }
                            }

                            // Handle command using centralized auth/reject logic
                            let action = CommandHandler::handle_command(&line);
                            match crate::session::forwarding::handle_intercepted_command(
                                action,
                                &line,
                                &mut client_write,
                                &self.auth_handler,
                                &self.client_addr,
                            )
                            .await?
                            {
                                Some(bytes) => {
                                    // Command was intercepted (auth or reject)
                                    backend_to_client_bytes.add(bytes);
                                }
                                None => {
                                    // Forward to backend and cache response
                                    // Forward commands to backend
                                    backend_write.write_all(line.as_bytes()).await?;
                                    client_to_backend_bytes.add(line.len());

                                    // Read first line of response using read_until for efficiency
                                    first_line.clear();
                                    backend_reader.read_until(b'\n', &mut first_line).await?;

                                    if first_line.is_empty() {
                                        debug!("Backend {} closed connection", self.client_addr);
                                        break;
                                    }

                                    // Transfer ownership using mem::take (leaves first_line as empty Vec)
                                    let mut response_buffer = std::mem::take(&mut first_line);

                                    // Check for backend disconnect (205 status)
                                    if NntpResponse::is_disconnect(&response_buffer) {
                                        debug!("Backend {} sent disconnect: {}", self.client_addr, String::from_utf8_lossy(&response_buffer));
                                        client_write.write_all(&response_buffer).await?;
                                        backend_to_client_bytes.add(response_buffer.len());
                                        break;
                                    }

                                    // Parse response code once and reuse it (avoid redundant parsing)
                                    let response_code = ResponseCode::parse(&response_buffer);
                                    let is_multiline = response_code.is_multiline();

                                    // Read multiline data if needed (as raw bytes)
                                    if is_multiline {
                                        loop {
                                            let start_pos = response_buffer.len();
                                            backend_reader.read_until(b'\n', &mut response_buffer).await?;

                                            if response_buffer.len() == start_pos {
                                                break;
                                            }

                                            // Check for end marker by examining just the new data
                                            let new_data = &response_buffer[start_pos..];
                                            if new_data == b".\r\n" || new_data == b".\n" {
                                                break;
                                            }
                                        }
                                    }

                                    // Cache if it was a cacheable command (article by message-ID)
                                    if matches!(NntpCommand::classify(&line), NntpCommand::ArticleByMessageId)
                                        && let Some(message_id) = NntpResponse::extract_message_id(&line) {
                                            // Only cache successful responses (2xx) - reuse already-parsed response_code
                                            if response_code.is_success() {
                                                info!("Caching response for message-ID: {}", message_id);
                                                self.cache.insert(
                                                    message_id,
                                                    CachedArticle {
                                                        response: Arc::new(response_buffer.clone()),
                                                    }
                                                ).await;
                                            }
                                    }

                                    // Forward response to client
                                    client_write.write_all(&response_buffer).await?;
                                    backend_to_client_bytes.add(response_buffer.len());
                                }
                            }
                        }
                        Err(e) => {
                            debug!("Error reading from client {}: {}", self.client_addr, e);
                            break;
                        }
                    }
                }
            }
        }

        Ok((
            client_to_backend_bytes.as_u64(),
            backend_to_client_bytes.as_u64(),
        ))
    }
}
