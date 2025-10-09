//! Caching session handler that wraps ClientSession with caching logic

use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info};

use crate::cache::article::{ArticleCache, CachedArticle};
use crate::command::{CommandAction, CommandHandler};
use crate::constants::buffer;
use crate::constants::stateless_proxy::NNTP_COMMAND_NOT_SUPPORTED;

/// Extract message-ID from command arguments using fast byte searching
fn extract_message_id(command: &str) -> Option<String> {
    let trimmed = command.trim();
    let bytes = trimmed.as_bytes();

    memchr::memchr(b'<', bytes).and_then(|start| {
        memchr::memchr(b'>', &bytes[start..]).map(|end| trimmed[start..start + end + 1].to_string())
    })
}

/// Check if command is cacheable (ARTICLE/BODY/HEAD/STAT with message-ID)
fn is_cacheable_command(command: &str) -> bool {
    let upper = command.trim().to_uppercase();
    (upper.starts_with("ARTICLE ")
        || upper.starts_with("BODY ")
        || upper.starts_with("HEAD ")
        || upper.starts_with("STAT "))
        && extract_message_id(command).is_some()
}

/// Parse status code from binary data and determine if it's a multiline response
fn parse_multiline_status(data: &[u8]) -> bool {
    std::str::from_utf8(data)
        .ok()
        .and_then(parse_status_code)
        .map(is_multiline_status)
        .unwrap_or(false)
}

/// Caching session that wraps standard session with article cache
pub struct CachingSession {
    client_addr: SocketAddr,
    cache: Arc<ArticleCache>,
}

impl CachingSession {
    /// Create a new caching session
    pub fn new(client_addr: SocketAddr, cache: Arc<ArticleCache>) -> Self {
        Self { client_addr, cache }
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
        let mut backend_reader = BufReader::with_capacity(buffer::MEDIUM_BUFFER_SIZE, backend_read);

        let mut client_to_backend_bytes = 0u64;
        let mut backend_to_client_bytes = 0u64;
        let mut line = String::with_capacity(buffer::COMMAND_SIZE);
        // Pre-allocate with typical NNTP response line size (most are < 512 bytes)
        // Reduces reallocations during line reading
        let mut first_line = Vec::with_capacity(512);

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

                            // Check if this is a cacheable command
                            if is_cacheable_command(&line) {
                                if let Some(message_id) = extract_message_id(&line) {
                                    // Check cache first
                                    if let Some(cached) = self.cache.get(&message_id).await {
                                        info!("Cache HIT for message-ID: {} (size: {} bytes)", message_id, cached.response.len());
                                        client_write.write_all(&cached.response).await?;
                                        client_write.flush().await?;
                                        backend_to_client_bytes += cached.response.len() as u64;
                                        continue;
                                    } else {
                                        info!("Cache MISS for message-ID: {}", message_id);
                                    }
                                } else {
                                    debug!("No message-ID extracted from command: {}", line.trim());
                                }
                            }

                            // Handle command using standard handler
                            match CommandHandler::handle_command(&line) {
                                CommandAction::InterceptAuth(auth_action) => {
                                    use crate::auth::AuthHandler;
                                    use crate::command::AuthAction;
                                    let response = match auth_action {
                                        AuthAction::RequestPassword => AuthHandler::user_response(),
                                        AuthAction::AcceptAuth => AuthHandler::pass_response(),
                                    };
                                    client_write.write_all(response).await?;
                                    backend_to_client_bytes += response.len() as u64;
                                }
                                CommandAction::Reject(_reason) => {
                                    client_write.write_all(NNTP_COMMAND_NOT_SUPPORTED).await?;
                                    backend_to_client_bytes += NNTP_COMMAND_NOT_SUPPORTED.len() as u64;
                                }
                                CommandAction::ForwardStateless => {
                                    // Forward stateless commands to backend
                                    backend_write.write_all(line.as_bytes()).await?;
                                    backend_write.flush().await?;
                                    client_to_backend_bytes += line.len() as u64;

                                    // Read response using read_until for efficiency
                                    first_line.clear();
                                    backend_reader.read_until(b'\n', &mut first_line).await?;

                                    if first_line.is_empty() {
                                        debug!("Backend {} closed connection", self.client_addr);
                                        break;
                                    }

                                    // Use mem::take to transfer ownership from first_line to response_buffer
                                    // More idiomatic than swap - explicitly shows we're taking the value and leaving default
                                    let mut response_buffer = std::mem::take(&mut first_line);

                                    // Check for backend disconnect (205 status)
                                    if response_buffer.len() >= 3 && &response_buffer[0..3] == b"205" {
                                        debug!("Backend {} sent disconnect: {}", self.client_addr, String::from_utf8_lossy(&response_buffer));
                                        client_write.write_all(&response_buffer).await?;
                                        client_write.flush().await?;
                                        backend_to_client_bytes += response_buffer.len() as u64;
                                        break;
                                    }

                                    let is_multiline = parse_multiline_status(&response_buffer);

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

                                    client_write.write_all(&response_buffer).await?;
                                    client_write.flush().await?;
                                    backend_to_client_bytes += response_buffer.len() as u64;
                                }
                                CommandAction::ForwardHighThroughput => {
                                    // Forward to backend
                                    backend_write.write_all(line.as_bytes()).await?;
                                    backend_write.flush().await?;
                                    client_to_backend_bytes += line.len() as u64;

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
                                    if response_buffer.len() >= 3 && &response_buffer[0..3] == b"205" {
                                        debug!("Backend {} sent disconnect: {}", self.client_addr, String::from_utf8_lossy(&response_buffer));
                                        client_write.write_all(&response_buffer).await?;
                                        client_write.flush().await?;
                                        backend_to_client_bytes += response_buffer.len() as u64;
                                        break;
                                    }

                                    // Check if this is a multiline response by parsing status code
                                    let is_multiline = parse_multiline_status(&response_buffer);

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

                                    // Cache if it was a cacheable command
                                    if is_cacheable_command(&line)
                                        && let Some(message_id) = extract_message_id(&line) {
                                            // Only cache successful responses (2xx)
                                            if !response_buffer.is_empty() && response_buffer[0] == b'2' {
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
                                    client_write.flush().await?;
                                    backend_to_client_bytes += response_buffer.len() as u64;
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

        Ok((client_to_backend_bytes, backend_to_client_bytes))
    }
}

/// Parse status code from NNTP response line
fn parse_status_code(line: &str) -> Option<u16> {
    let trimmed = line.trim();
    if trimmed.len() < 3 {
        return None;
    }
    trimmed[0..3].parse().ok()
}

/// Check if a status code indicates a multiline response
fn is_multiline_status(status_code: u16) -> bool {
    // Multiline responses: 1xx informational, and specific 2xx codes
    match status_code {
        100..=199 => true, // Informational multiline
        215 | 220 | 221 | 222 | 224 | 225 | 230 | 231 | 282 => true, // Article/list data
        _ => false,
    }
}
