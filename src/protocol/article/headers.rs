//! RFC 5322 compliant header parsing with zero-copy slicing

use super::error::ParseError;
use std::borrow::Cow;

/// Validated NNTP article headers (zero-copy)
///
/// Per [RFC 5322](https://datatracker.ietf.org/doc/html/rfc5322):
/// - Each header line: `name: value CRLF`
/// - Header names: no spaces, ASCII printable except colon
/// - Folded headers: continuation lines start with space/tab
/// - Headers end with blank line (CRLF CRLF)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Headers<'a> {
    raw: &'a [u8],
    transformation: super::HeaderTransformation,
}

impl<'a> Headers<'a> {
    pub(crate) fn from_validated(
        data: &'a [u8],
        transformation: super::HeaderTransformation,
    ) -> Self {
        Self::from_transformation(data, transformation)
    }

    pub(crate) fn validate(data: &[u8]) -> Result<super::HeaderTransformation, ParseError> {
        let transformation = Self::validate_headers(data)?;
        Ok(if transformation {
            super::HeaderTransformation::Unfold
        } else {
            super::HeaderTransformation::None
        })
    }

    fn from_transformation(data: &'a [u8], transformation: super::HeaderTransformation) -> Self {
        Self {
            raw: data,
            transformation,
        }
    }

    /// Parse and validate header block
    ///
    /// # Arguments
    /// * `data` - Raw header bytes (should NOT include the trailing blank line)
    ///
    /// # Returns
    /// Validated Headers or `ParseError`
    ///
    /// # Errors
    /// Returns `ParseError` when any header line violates RFC 5322 formatting rules.
    pub fn parse(data: &'a [u8]) -> Result<Self, ParseError> {
        let transformation = Self::validate(data)?;
        Ok(Self::from_transformation(data, transformation))
    }

    /// Validate header format per RFC 5322
    fn validate_headers(data: &[u8]) -> Result<bool, ParseError> {
        let mut pos = 0;
        let len = data.len();
        let mut folded = false;

        while pos < len {
            // Find end of line
            let line_end = Self::find_line_end(data, pos)?;
            let line = &data[pos..line_end];

            // Skip empty lines (shouldn't happen but be lenient)
            if line.is_empty() {
                pos = line_end + 2; // Skip CRLF
                continue;
            }

            // Check for folded header (starts with space/tab)
            if line[0] == b' ' || line[0] == b'\t' {
                // Continuation line - valid only if not first line
                if pos == 0 {
                    return Err(ParseError::InvalidHeader(
                        "Header cannot start with folding whitespace".to_string(),
                    ));
                }
                folded = true;
                pos = line_end + 2;
                continue;
            }

            // Find colon separator
            let colon_pos = memchr::memchr(b':', line).ok_or_else(|| {
                ParseError::InvalidHeader(format!(
                    "Header missing colon: {}",
                    String::from_utf8_lossy(line)
                ))
            })?;

            // Validate header name
            let name = &line[..colon_pos];
            if name.is_empty() {
                return Err(ParseError::InvalidHeader("Empty header name".to_string()));
            }

            // Header name must not contain spaces or invalid characters
            for &byte in name {
                if byte == b' ' || byte == b'\t' || !(33..=126).contains(&byte) {
                    return Err(ParseError::InvalidHeader(format!(
                        "Invalid character in header name: {}",
                        String::from_utf8_lossy(name)
                    )));
                }
            }

            pos = line_end + 2; // Move past CRLF
        }

        Ok(folded)
    }

    /// Find end of line (position of \r in \r\n)
    fn find_line_end(data: &[u8], start: usize) -> Result<usize, ParseError> {
        for i in start..data.len() {
            if data[i] == b'\n' {
                // Check if preceded by \r
                if i > 0 && data[i - 1] == b'\r' {
                    return Ok(i - 1); // Return position of \r
                }
                return Err(ParseError::InvalidHeader(
                    "LF not preceded by CR".to_string(),
                ));
            }
            if data[i] == b'\r' {
                // Check for \n following \r
                if i + 1 < data.len() && data[i + 1] == b'\n' {
                    return Ok(i);
                } else if i + 1 >= data.len() {
                    // CR at end of buffer - might be incomplete
                    return Ok(i);
                }
                return Err(ParseError::InvalidHeader(
                    "CR not followed by LF".to_string(),
                ));
            }
        }

        // No line ending found - return end of buffer (last line)
        Ok(data.len())
    }

    /// Get header value by name (case-insensitive, zero-copy)
    ///
    /// # Arguments
    /// * `name` - Header name (case-insensitive)
    ///
    /// # Returns
    /// Header value slice (trimmed leading/trailing whitespace) or None
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&'a [u8]> {
        let lookup = name.as_bytes();
        let mut pos = 0;

        while pos < self.raw.len() {
            // Find line end
            let line_end = Self::find_line_end(self.raw, pos).ok()?;
            let line = &self.raw[pos..line_end];

            if line.is_empty() {
                pos = line_end + 2;
                continue;
            }

            // Skip folded lines (we'll handle them when we find the main header)
            if line[0] == b' ' || line[0] == b'\t' {
                pos = line_end + 2;
                continue;
            }

            // Find colon
            let colon_pos = memchr::memchr(b':', line)?;
            let header_name = &line[..colon_pos];

            // Case-insensitive comparison
            if header_name.eq_ignore_ascii_case(lookup) {
                // Found it! Get value
                let mut value_start = colon_pos + 1;

                // Skip leading whitespace in value
                while value_start < line.len()
                    && (line[value_start] == b' ' || line[value_start] == b'\t')
                {
                    value_start += 1;
                }

                let value = &line[value_start..];

                return Some(value);
            }

            pos = line_end + 2;
        }

        None
    }

    /// Iterate over all headers (zero-copy)
    #[must_use]
    pub const fn iter(&self) -> HeaderIter<'a> {
        HeaderIter {
            data: self.raw,
            pos: 0,
        }
    }

    /// Get raw header bytes
    #[must_use]
    pub const fn as_bytes(&self) -> &'a [u8] {
        self.raw
    }

    /// Materialize RFC 5322 unfolding for the logical header block.
    ///
    /// Validated article views retain the wire bytes and transformation
    /// requirement without allocating. The allocation for folded headers is
    /// deferred until a caller explicitly asks for the unfolded block.
    #[must_use]
    pub fn unfolded_bytes(&self) -> Cow<'a, [u8]> {
        match self.transformation {
            super::HeaderTransformation::None => Cow::Borrowed(self.raw),
            super::HeaderTransformation::Unfold => unfold_continuations(self.raw),
        }
    }
}

fn unfold_continuations(data: &[u8]) -> Cow<'_, [u8]> {
    let mut unfolded = Vec::with_capacity(data.len());
    let mut pos = 0;
    while pos < data.len() {
        if pos + 2 < data.len()
            && data[pos] == b'\r'
            && data[pos + 1] == b'\n'
            && matches!(data[pos + 2], b' ' | b'\t')
        {
            // RFC 5322 unfolding removes only CRLF; preserve the WSP that
            // begins the continuation line.
            pos += 2;
        } else {
            unfolded.push(data[pos]);
            pos += 1;
        }
    }
    Cow::Owned(unfolded)
}

impl<'a> IntoIterator for &Headers<'a> {
    type Item = (&'a [u8], &'a [u8]);
    type IntoIter = HeaderIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Iterator over headers
pub struct HeaderIter<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for HeaderIter<'a> {
    type Item = (&'a [u8], &'a [u8]); // (name, value)

    fn next(&mut self) -> Option<Self::Item> {
        while self.pos < self.data.len() {
            // Find line end
            let line_end = Headers::find_line_end(self.data, self.pos).ok()?;
            let line = &self.data[self.pos..line_end];

            if line.is_empty() {
                self.pos = line_end + 2;
                continue;
            }

            // Skip folded lines (they're part of previous header)
            if line[0] == b' ' || line[0] == b'\t' {
                self.pos = line_end + 2;
                continue;
            }

            // Find colon
            let colon_pos = memchr::memchr(b':', line)?;
            let name = &line[..colon_pos];
            let mut value_start = colon_pos + 1;

            // Skip leading whitespace
            while value_start < line.len()
                && (line[value_start] == b' ' || line[value_start] == b'\t')
            {
                value_start += 1;
            }

            let value = &line[value_start..];

            self.pos = line_end + 2;
            return Some((name, value));
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_headers() {
        let data = b"Subject: Test\r\nFrom: test@example.com\r\n";
        let headers = Headers::parse(data).unwrap();
        assert_eq!(headers.get("Subject"), Some(&b"Test"[..]));
        assert_eq!(headers.get("From"), Some(&b"test@example.com"[..]));
    }

    #[test]
    fn test_case_insensitive() {
        let data = b"Subject: Test\r\n";
        let headers = Headers::parse(data).unwrap();
        assert_eq!(headers.get("subject"), headers.get("Subject"));
        assert_eq!(headers.get("SUBJECT"), headers.get("Subject"));
    }

    #[test]
    fn test_missing_colon() {
        let data = b"Invalid Header\r\n";
        assert!(matches!(
            Headers::parse(data),
            Err(ParseError::InvalidHeader(_))
        ));
    }

    #[test]
    fn test_empty_name() {
        let data = b": Value\r\n";
        assert!(matches!(
            Headers::parse(data),
            Err(ParseError::InvalidHeader(_))
        ));
    }

    #[test]
    fn test_iteration() {
        let data = b"Subject: Test\r\nFrom: user@example.com\r\n";
        let headers = Headers::parse(data).unwrap();

        let items: Vec<_> = headers.iter().collect();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].0, b"Subject");
        assert_eq!(items[0].1, b"Test");
    }

    #[test]
    fn raw_and_unfolded_header_bytes_have_distinct_contracts() {
        let raw = b"Subject: first\r\n \t second\r\n";
        let headers = Headers::parse(raw).unwrap();
        let copied = headers;

        assert_eq!(headers.as_bytes(), raw);
        assert_eq!(copied.as_bytes(), raw);
        assert_eq!(
            headers.unfolded_bytes().as_ref(),
            b"Subject: first \t second\r\n"
        );
    }

    #[test]
    fn plain_unfolded_headers_borrow_the_wire_bytes() {
        let raw = b"Subject: first\r\n";
        let headers = Headers::parse(raw).unwrap();

        assert!(matches!(headers.unfolded_bytes(), Cow::Borrowed(bytes) if bytes == raw));
    }
}
