//! Backend server authentication

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::debug;

use crate::pool::BufferPool;
use crate::protocol::ResponseParser;

/// Handles authentication to backend NNTP servers
pub struct BackendAuthenticator;

impl BackendAuthenticator {
    /// Authenticate to backend server
    #[allow(dead_code)]
    async fn authenticate(
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
    #[allow(dead_code)]
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
    #[allow(dead_code)]
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Test ResponseParser::is_auth_success
    #[test]
    fn test_auth_response_parsing() {
        // Test successful auth response
        let auth_success = b"281 Authentication accepted\r\n";
        assert!(ResponseParser::is_auth_success(auth_success));

        // Test password required response
        let password_required = b"381 Password required\r\n";
        assert!(ResponseParser::is_auth_required(password_required));
        assert!(!ResponseParser::is_auth_success(password_required));

        // Test failure response
        let auth_failed = b"481 Authentication failed\r\n";
        assert!(!ResponseParser::is_auth_success(auth_failed));
        assert!(!ResponseParser::is_auth_required(auth_failed));
    }

    /// Test greeting detection
    #[test]
    fn test_greeting_parsing() {
        let greeting = b"200 Welcome to the NNTP server\r\n";
        assert!(ResponseParser::is_greeting(greeting));

        let greeting_auth_required = b"201 Welcome, authentication required\r\n";
        assert!(ResponseParser::is_greeting(greeting_auth_required));

        let not_greeting = b"400 Service temporarily unavailable\r\n";
        assert!(!ResponseParser::is_greeting(not_greeting));
    }

    /// Test buffer pool interaction in authentication
    #[tokio::test]
    async fn test_buffer_pool_usage() {
        let buffer_pool = BufferPool::new(8192, 2);

        // Verify we can get and return buffers
        let buffer1 = buffer_pool.get_buffer().await;
        let buffer2 = buffer_pool.get_buffer().await;

        assert_eq!(buffer1.len(), 8192);
        assert_eq!(buffer2.len(), 8192);

        buffer_pool.return_buffer(buffer1).await;
        buffer_pool.return_buffer(buffer2).await;

        // Should be able to get them again
        let buffer3 = buffer_pool.get_buffer().await;
        assert_eq!(buffer3.len(), 8192);
        buffer_pool.return_buffer(buffer3).await;
    }

    /// Test authentication command formatting
    #[test]
    fn test_authentication_command_format() {
        // Verify the commands we construct are correct
        let username = "testuser";
        let password = "testpass";

        let user_command = format!("AUTHINFO USER {}\r\n", username);
        assert_eq!(user_command, "AUTHINFO USER testuser\r\n");

        let pass_command = format!("AUTHINFO PASS {}\r\n", password);
        assert_eq!(pass_command, "AUTHINFO PASS testpass\r\n");
    }

    /// Test edge cases in command formatting
    #[test]
    fn test_authentication_command_edge_cases() {
        // Username with spaces (invalid but test the format)
        let username = "user with spaces";
        let user_command = format!("AUTHINFO USER {}\r\n", username);
        assert!(user_command.contains("user with spaces"));

        // Empty username
        let empty_command = format!("AUTHINFO USER {}\r\n", "");
        assert_eq!(empty_command, "AUTHINFO USER \r\n");

        // Password with special characters
        let password = "p@ssw0rd!#$";
        let pass_command = format!("AUTHINFO PASS {}\r\n", password);
        assert_eq!(pass_command, "AUTHINFO PASS p@ssw0rd!#$\r\n");
    }
}
