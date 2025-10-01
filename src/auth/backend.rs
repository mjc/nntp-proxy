//! Backend server authentication

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::debug;

use crate::pool::BufferPool;
use crate::protocol::{ResponseParser};

/// Handles authentication to backend NNTP servers
pub struct BackendAuthenticator;

impl BackendAuthenticator {
    /// Perform NNTP authentication on a backend connection
    pub async fn authenticate(
        backend_stream: &mut TcpStream,
        username: &str,
        password: &str,
        buffer_pool: &BufferPool,
    ) -> Result<()> {
        let mut buffer = buffer_pool.get_buffer().await;

        // Send AUTHINFO USER command
        let user_command = format!("AUTHINFO USER {}\r\n", username);
        backend_stream.write_all(user_command.as_bytes()).await?;

        // Read response
        let n = backend_stream.read(&mut buffer).await?;
        let response = String::from_utf8_lossy(&buffer[..n]);
        debug!("AUTHINFO USER response: {}", response.trim());

        // Should get 381 (password required) or 281 (authenticated)
        if ResponseParser::is_auth_success(&buffer[..n]) {
            // Already authenticated with just username
            buffer_pool.return_buffer(buffer).await;
            return Ok(());
        } else if !ResponseParser::is_auth_required(&buffer[..n]) {
            let error_msg = response.trim().to_string();
            buffer_pool.return_buffer(buffer).await;
            return Err(anyhow::anyhow!(
                "Unexpected response to AUTHINFO USER: {}",
                error_msg
            ));
        }

        // Send AUTHINFO PASS command
        let pass_command = format!("AUTHINFO PASS {}\r\n", password);
        backend_stream.write_all(pass_command.as_bytes()).await?;

        // Read final response
        let n = backend_stream.read(&mut buffer).await?;
        let response = String::from_utf8_lossy(&buffer[..n]);
        debug!("AUTHINFO PASS response: {}", response.trim());

        // Should get 281 (authenticated)
        let result = if ResponseParser::is_auth_success(&buffer[..n]) {
            Ok(())
        } else {
            let error_msg = response.trim().to_string();
            Err(anyhow::anyhow!("Authentication failed: {}", error_msg))
        };

        // Return buffer to pool
        buffer_pool.return_buffer(buffer).await;
        result
    }

    /// Read and forward the backend server's greeting to the client
    pub async fn forward_greeting(
        backend_stream: &mut TcpStream,
        client_stream: &mut TcpStream,
        buffer_pool: &BufferPool,
    ) -> Result<()> {
        let mut buffer = buffer_pool.get_buffer().await;

        // Read the server greeting
        let n = backend_stream.read(&mut buffer).await?;
        let greeting = &buffer[..n];
        let greeting_str = String::from_utf8_lossy(greeting);
        debug!("Backend greeting: {}", greeting_str.trim());

        if !ResponseParser::is_greeting(greeting) {
            let error_msg = greeting_str.trim().to_string();
            buffer_pool.return_buffer(buffer).await;
            return Err(anyhow::anyhow!(
                "Server returned non-success greeting: {}",
                error_msg
            ));
        }

        // Forward greeting to client
        client_stream.write_all(greeting).await?;

        buffer_pool.return_buffer(buffer).await;
        Ok(())
    }

    /// Perform authentication and forward the greeting to the client
    pub async fn authenticate_and_forward_greeting(
        backend_stream: &mut TcpStream,
        client_stream: &mut TcpStream,
        username: &str,
        password: &str,
        buffer_pool: &BufferPool,
    ) -> Result<()> {
        let mut buffer = buffer_pool.get_buffer().await;

        // Read the server greeting first and forward it
        let n = backend_stream.read(&mut buffer).await?;
        let greeting = &buffer[..n];
        let greeting_str = String::from_utf8_lossy(greeting);
        debug!("Backend greeting: {}", greeting_str.trim());

        if !ResponseParser::is_greeting(greeting) {
            let error_msg = greeting_str.trim().to_string();
            buffer_pool.return_buffer(buffer).await;
            return Err(anyhow::anyhow!(
                "Server returned non-success greeting: {}",
                error_msg
            ));
        }

        // Forward greeting to client immediately
        client_stream.write_all(greeting).await?;
        
        // Return buffer before calling authenticate
        buffer_pool.return_buffer(buffer).await;

        // Now perform authentication on backend
        Self::authenticate(backend_stream, username, password, buffer_pool).await
    }
}
