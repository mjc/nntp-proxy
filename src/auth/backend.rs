//! Backend server authentication

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::debug;

use crate::pool::BufferPool;
use crate::protocol::{ResponseParser, authinfo_pass, authinfo_user};

/// Handles authentication to backend NNTP servers
pub struct BackendAuthenticator;

impl BackendAuthenticator {
    /// Authenticate to backend server - generic over stream types to support both TCP and future TLS
    #[allow(dead_code)]
    async fn authenticate<S>(
        backend_stream: &mut S,
        username: &str,
        password: &str,
        buffer_pool: &BufferPool,
    ) -> Result<()>
    where
        S: AsyncReadExt + AsyncWriteExt + Unpin,
    {
        let mut buffer = buffer_pool.get_buffer().await;

        // Send AUTHINFO USER command
        backend_stream
            .write_all(authinfo_user(username).as_bytes())
            .await?;

        // Read response
        let n = backend_stream.read(&mut buffer).await?;
        let response = String::from_utf8_lossy(&buffer[..n]);
        debug!("AUTHINFO USER response: {}", response.trim());

        // Should get 381 (password required) or 281 (authenticated)
        if ResponseParser::is_auth_success(&buffer[..n]) {
            // Already authenticated with just username
            return Ok(());
        } else if !ResponseParser::is_auth_required(&buffer[..n]) {
            let error = format!("Unexpected response to AUTHINFO USER: {}", response.trim());
            return Err(anyhow::anyhow!(error));
        }

        // Send AUTHINFO PASS command
        backend_stream
            .write_all(authinfo_pass(password).as_bytes())
            .await?;

        // Read final response
        let n = backend_stream.read(&mut buffer).await?;
        let response = String::from_utf8_lossy(&buffer[..n]);
        debug!("AUTHINFO PASS response: {}", response.trim());

        // Should get 281 (authenticated)
        if ResponseParser::is_auth_success(&buffer[..n]) {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "Authentication failed: {}",
                response.trim()
            ))
        }
    }

    /// Read and forward the backend server's greeting to the client - generic over stream types
    #[allow(dead_code)]
    pub async fn forward_greeting<B, C>(
        backend_stream: &mut B,
        client_stream: &mut C,
        buffer_pool: &BufferPool,
    ) -> Result<()>
    where
        B: AsyncReadExt + AsyncWriteExt + Unpin,
        C: AsyncReadExt + AsyncWriteExt + Unpin,
    {
        let mut buffer = buffer_pool.get_buffer().await;

        // Read the server greeting
        let n = backend_stream.read(&mut buffer).await?;
        let greeting = &buffer[..n];
        let greeting_str = String::from_utf8_lossy(greeting);
        debug!("Backend greeting: {}", greeting_str.trim());

        if !ResponseParser::is_greeting(greeting) {
            let error = format!(
                "Server returned non-success greeting: {}",
                greeting_str.trim()
            );
            return Err(anyhow::anyhow!(error));
        }

        // Forward greeting to client
        client_stream.write_all(greeting).await?;

        Ok(())
    }

    /// Perform authentication and forward the greeting to the client - generic over stream types
    #[allow(dead_code)]
    pub async fn authenticate_and_forward_greeting<B, C>(
        backend_stream: &mut B,
        client_stream: &mut C,
        username: &str,
        password: &str,
        buffer_pool: &BufferPool,
    ) -> Result<()>
    where
        B: AsyncReadExt + AsyncWriteExt + Unpin,
        C: AsyncReadExt + AsyncWriteExt + Unpin,
    {
        let mut buffer = buffer_pool.get_buffer().await;

        // Read the server greeting first and forward it
        let n = backend_stream.read(&mut buffer).await?;
        let greeting = &buffer[..n];
        let greeting_str = String::from_utf8_lossy(greeting);
        debug!("Backend greeting: {}", greeting_str.trim());

        if !ResponseParser::is_greeting(greeting) {
            let error = format!(
                "Server returned non-success greeting: {}",
                greeting_str.trim()
            );
            return Err(anyhow::anyhow!(error));
        }

        // Forward greeting to client immediately
        client_stream.write_all(greeting).await?;

        // Return buffer before calling authenticate

        // Now perform authentication on backend
        Self::authenticate(backend_stream, username, password, buffer_pool).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::BufferSize;

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
        let buffer_pool = BufferPool::new(BufferSize::new(8192).unwrap(), 2);

        // Verify we can get and return buffers
        let buffer1 = buffer_pool.get_buffer().await;
        let buffer2 = buffer_pool.get_buffer().await;

        assert_eq!(buffer1.len(), 8192);
        assert_eq!(buffer2.len(), 8192);

        // Should be able to get them again
        let buffer3 = buffer_pool.get_buffer().await;
        assert_eq!(buffer3.len(), 8192);
    }

    /// Test authentication command formatting
    #[test]
    fn test_authentication_command_format() {
        // Verify the commands we construct are correct
        let username = "testuser";
        let password = "testpass";

        let user_command = authinfo_user(username);
        assert_eq!(user_command, "AUTHINFO USER testuser\r\n");

        let pass_command = authinfo_pass(password);
        assert_eq!(pass_command, "AUTHINFO PASS testpass\r\n");
    }

    /// Test edge cases in command formatting
    #[test]
    fn test_authentication_command_edge_cases() {
        // Username with spaces (invalid but test the format)
        let username = "user with spaces";
        let user_command = authinfo_user(username);
        assert!(user_command.contains("user with spaces"));

        // Empty username
        let empty_command = authinfo_user("");
        assert_eq!(empty_command, "AUTHINFO USER \r\n");

        // Password with special characters
        let password = "p@ssw0rd!#$";
        let pass_command = authinfo_pass(password);
        assert_eq!(pass_command, "AUTHINFO PASS p@ssw0rd!#$\r\n");
    }
}
