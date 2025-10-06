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
use crate::protocol::NNTP_COMMAND_NOT_SUPPORTED;

/// Extract message-ID from command arguments
fn extract_message_id(command: &str) -> Option<String> {
    let trimmed = command.trim();
    if let Some(start) = trimmed.find('<')
        && let Some(end) = trimmed[start..].find('>') {
            return Some(trimmed[start..start + end + 1].to_string());
        }
    None
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

/// Caching session that wraps standard session with article cache
pub struct CachingSession {
    client_addr: SocketAddr,
    cache: Arc<ArticleCache>,
}

impl CachingSession {
    /// Create a new caching session
    pub fn new(
        client_addr: SocketAddr,
        cache: Arc<ArticleCache>,
    ) -> Self {
        Self {
            client_addr,
            cache,
        }
    }

    /// Handle client connection with caching support
    pub async fn handle_with_pooled_backend<T>(
        &self,
        mut client_stream: TcpStream,
        mut backend_conn: T,
    ) -> Result<(u64, u64)>
    where
        T: std::ops::DerefMut<Target = TcpStream>,
    {
        use tokio::io::BufReader;

        let (client_read, mut client_write) = client_stream.split();
        let (backend_read, mut backend_write) = backend_conn.split();
        let mut client_reader = BufReader::new(client_read);
        let mut backend_reader = BufReader::new(backend_read);

        let mut client_to_backend_bytes = 0u64;
        let mut backend_to_client_bytes = 0u64;
        let mut line = String::with_capacity(buffer::COMMAND_SIZE);

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
                                    // Intercept DATE command for health checks
                                    let upper = line.trim().to_uppercase();
                                    if upper.starts_with("DATE") {
                                        // Generate DATE response: 111 YYYYMMDDhhmmss
                                        // Fixed timestamp for health checks (actual time doesn't matter)
                                        let date_response = "111 20251006190000\r\n";
                                        client_write.write_all(date_response.as_bytes()).await?;
                                        client_write.flush().await?;
                                        backend_to_client_bytes += date_response.len() as u64;
                                        debug!("Intercepted DATE command for client {}", self.client_addr);
                                        continue;
                                    }
                                    
                                    // Forward other stateless commands to backend
                                    backend_write.write_all(line.as_bytes()).await?;
                                    backend_write.flush().await?;
                                    client_to_backend_bytes += line.len() as u64;

                                    // Read response using read_until for efficiency
                                    let mut first_line = Vec::new();
                                    backend_reader.read_until(b'\n', &mut first_line).await?;

                                    if first_line.is_empty() {
                                        debug!("Backend {} closed connection", self.client_addr);
                                        break;
                                    }

                                    let mut response_buffer = first_line.clone();
                                    
                                    let is_multiline = if let Ok(status_line) = String::from_utf8(first_line.clone()) {
                                        parse_status_code(&status_line)
                                            .map(is_multiline_status)
                                            .unwrap_or(false)
                                    } else {
                                        false
                                    };

                                    if is_multiline {
                                        loop {
                                            let mut line_buf = Vec::new();
                                            backend_reader.read_until(b'\n', &mut line_buf).await?;
                                            
                                            if line_buf.is_empty() {
                                                break;
                                            }
                                            
                                            response_buffer.extend_from_slice(&line_buf);
                                            
                                            if line_buf == b".\r\n" || line_buf == b".\n" {
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
                                    let mut first_line = Vec::new();
                                    backend_reader.read_until(b'\n', &mut first_line).await?;

                                    if first_line.is_empty() {
                                        debug!("Backend {} closed connection", self.client_addr);
                                        break;
                                    }

                                    let mut response_buffer = first_line.clone();
                                    
                                    // Check if this is a multiline response by parsing status code
                                    let is_multiline = if let Ok(status_line) = String::from_utf8(first_line.clone()) {
                                        parse_status_code(&status_line)
                                            .map(is_multiline_status)
                                            .unwrap_or(false)
                                    } else {
                                        false
                                    };

                                    // Read multiline data if needed (as raw bytes)
                                    if is_multiline {
                                        loop {
                                            let mut line_buf = Vec::new();
                                            backend_reader.read_until(b'\n', &mut line_buf).await?;
                                            
                                            if line_buf.is_empty() {
                                                break;
                                            }
                                            
                                            response_buffer.extend_from_slice(&line_buf);
                                            
                                            // Check for end-of-multiline marker
                                            if line_buf == b".\r\n" || line_buf == b".\n" {
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
                                                        response: response_buffer.clone(),
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
