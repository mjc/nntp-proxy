//! Article parsing errors

use std::fmt;

/// Errors that can occur when parsing article responses
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Invalid or unexpected status code
    InvalidStatusCode(u16),

    /// Missing blank line separator between headers and body
    MissingSeparator,

    /// Missing multiline terminator (.\r\n)
    MissingTerminator,

    /// Invalid header format
    InvalidHeader(String),

    /// HEAD response contains body (invalid)
    UnexpectedBody,

    /// Invalid yenc structure
    InvalidYenc(String),

    /// Buffer too short
    BufferTooShort,

    /// Invalid message-ID format
    InvalidMessageId(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidStatusCode(code) => write!(f, "Invalid status code: {}", code),
            Self::MissingSeparator => {
                write!(f, "Missing blank line separator between headers and body")
            }
            Self::MissingTerminator => write!(f, "Missing multiline terminator (CRLF.CRLF)"),
            Self::InvalidHeader(msg) => write!(f, "Invalid header: {}", msg),
            Self::UnexpectedBody => write!(f, "HEAD response should not contain body"),
            Self::InvalidYenc(msg) => write!(f, "Invalid yenc: {}", msg),
            Self::BufferTooShort => write!(f, "Buffer too short to contain valid response"),
            Self::InvalidMessageId(msg) => write!(f, "Invalid message-ID: {}", msg),
        }
    }
}

impl std::error::Error for ParseError {}

// Allow conversion from ValidationError to ParseError
impl From<crate::types::validated::ValidationError> for ParseError {
    fn from(err: crate::types::validated::ValidationError) -> Self {
        ParseError::InvalidMessageId(err.to_string())
    }
}
