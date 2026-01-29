//! Article parsing errors

/// Errors that can occur when parsing article responses
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("Invalid status code: {0}")]
    InvalidStatusCode(u16),

    #[error("Missing blank line separator between headers and body")]
    MissingSeparator,

    #[error("Missing multiline terminator (CRLF.CRLF)")]
    MissingTerminator,

    #[error("Invalid header: {0}")]
    InvalidHeader(String),

    #[error("HEAD response should not contain body")]
    UnexpectedBody,

    #[error("Invalid yenc: {0}")]
    InvalidYenc(String),

    #[error("Buffer too short to contain valid response")]
    BufferTooShort,

    #[error("Invalid message-ID: {0}")]
    InvalidMessageId(String),
}

// Allow conversion from ValidationError to ParseError
impl From<crate::types::validated::ValidationError> for ParseError {
    fn from(err: crate::types::validated::ValidationError) -> Self {
        ParseError::InvalidMessageId(err.to_string())
    }
}
