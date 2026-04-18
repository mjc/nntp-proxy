//! Validation for raw client command lines.

use thiserror::Error;

/// RFC 3977 command line length limit, including trailing CRLF.
pub const MAX_COMMAND_LINE_OCTETS: usize = 512;

/// A raw client command line that has passed framing and size validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidatedCommandLine<'a> {
    raw: &'a str,
    content: &'a str,
}

impl<'a> ValidatedCommandLine<'a> {
    /// Validate a raw command line read from the client socket.
    pub fn new(raw: &'a str) -> Result<Self, CommandLineError> {
        let bytes = raw.as_bytes();

        if bytes.len() > MAX_COMMAND_LINE_OCTETS {
            return Err(CommandLineError::TooLong);
        }

        if !bytes.ends_with(b"\r\n") {
            if bytes[..bytes.len().saturating_sub(1)].contains(&b'\n')
                || bytes[..bytes.len().saturating_sub(1)].contains(&b'\r')
            {
                return Err(CommandLineError::MixedLineEnding);
            }

            return Err(match bytes.last().copied() {
                Some(b'\n') => CommandLineError::BareLf,
                Some(b'\r') => CommandLineError::BareCr,
                _ => CommandLineError::MissingCrLf,
            });
        }

        let content = &raw[..raw.len() - 2];
        let content_bytes = content.as_bytes();

        if content_bytes.is_empty() {
            return Err(CommandLineError::Empty);
        }

        if content_bytes.contains(&b'\r') || content_bytes.contains(&b'\n') {
            return Err(CommandLineError::MixedLineEnding);
        }

        if content_bytes
            .iter()
            .all(|byte| matches!(byte, b' ' | b'\t'))
        {
            return Err(CommandLineError::WhitespaceOnly);
        }

        if matches!(content_bytes.first(), Some(b' ' | b'\t'))
            || matches!(content_bytes.last(), Some(b' ' | b'\t'))
        {
            return Err(CommandLineError::SurroundingWhitespace);
        }

        Ok(Self { raw, content })
    }

    #[inline]
    #[must_use]
    pub const fn raw(self) -> &'a str {
        self.raw
    }

    #[inline]
    #[must_use]
    pub const fn content(self) -> &'a str {
        self.content
    }
}

/// Errors from validating a raw client command line.
#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
pub enum CommandLineError {
    #[error("command line exceeds 512 octets")]
    TooLong,
    #[error("command line must end with CRLF")]
    MissingCrLf,
    #[error("command line used bare LF")]
    BareLf,
    #[error("command line used bare CR")]
    BareCr,
    #[error("command line used mixed or embedded line endings")]
    MixedLineEnding,
    #[error("command line was empty")]
    Empty,
    #[error("command line was whitespace only")]
    WhitespaceOnly,
    #[error("command line had leading or trailing whitespace")]
    SurroundingWhitespace,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_canonical_crlf_line() {
        let line = ValidatedCommandLine::new("LIST\r\n").unwrap();
        assert_eq!(line.raw(), "LIST\r\n");
        assert_eq!(line.content(), "LIST");
    }

    #[test]
    fn rejects_non_canonical_line_endings() {
        assert_eq!(
            ValidatedCommandLine::new("LIST\n"),
            Err(CommandLineError::BareLf)
        );
        assert_eq!(
            ValidatedCommandLine::new("LIST\r"),
            Err(CommandLineError::BareCr)
        );
        assert_eq!(
            ValidatedCommandLine::new("LIST\rX\n"),
            Err(CommandLineError::MixedLineEnding)
        );
    }

    #[test]
    fn rejects_empty_and_whitespace_only_lines() {
        assert_eq!(
            ValidatedCommandLine::new("\r\n"),
            Err(CommandLineError::Empty)
        );
        assert_eq!(
            ValidatedCommandLine::new(" \t\r\n"),
            Err(CommandLineError::WhitespaceOnly)
        );
    }

    #[test]
    fn rejects_surrounding_whitespace() {
        assert_eq!(
            ValidatedCommandLine::new(" LIST\r\n"),
            Err(CommandLineError::SurroundingWhitespace)
        );
        assert_eq!(
            ValidatedCommandLine::new("LIST \r\n"),
            Err(CommandLineError::SurroundingWhitespace)
        );
    }

    #[test]
    fn rejects_oversized_line() {
        let oversized = format!("LIST {}\r\n", "A".repeat(MAX_COMMAND_LINE_OCTETS));
        assert_eq!(
            ValidatedCommandLine::new(&oversized),
            Err(CommandLineError::TooLong)
        );
    }
}
