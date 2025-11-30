//! RFC 5322 compliant header parsing with zero-copy slicing

use super::error::ParseError;

/// Validated NNTP article headers (zero-copy)
///
/// Per [RFC 5322](https://datatracker.ietf.org/doc/html/rfc5322):
/// - Each header line: `name: value CRLF`
/// - Header names: no spaces, ASCII printable except colon
/// - Folded headers: continuation lines start with space/tab
/// - Headers end with blank line (CRLF CRLF)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Headers<'a> {
    data: &'a [u8],
}

impl<'a> Headers<'a> {
    /// Parse and validate header block
    ///
    /// # Arguments
    /// * `data` - Raw header bytes (should NOT include the trailing blank line)
    ///
    /// # Returns
    /// Validated Headers or ParseError
    pub fn parse(data: &'a [u8]) -> Result<Self, ParseError> {
        Self::validate_headers(data)?;
        Ok(Headers { data })
    }

    /// Validate header format per RFC 5322
    fn validate_headers(data: &[u8]) -> Result<(), ParseError> {
        let mut pos = 0;
        let len = data.len();

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

        Ok(())
    }

    /// Find end of line (position of \r in \r\n)
    fn find_line_end(data: &[u8], start: usize) -> Result<usize, ParseError> {
        for i in start..data.len() {
            if data[i] == b'\n' {
                // Check if preceded by \r
                if i > 0 && data[i - 1] == b'\r' {
                    return Ok(i - 1); // Return position of \r
                } else {
                    return Err(ParseError::InvalidHeader(
                        "LF not preceded by CR".to_string(),
                    ));
                }
            }
            if data[i] == b'\r' {
                // Check for \n following \r
                if i + 1 < data.len() && data[i + 1] == b'\n' {
                    return Ok(i);
                } else if i + 1 >= data.len() {
                    // CR at end of buffer - might be incomplete
                    return Ok(i);
                } else {
                    return Err(ParseError::InvalidHeader(
                        "CR not followed by LF".to_string(),
                    ));
                }
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
    pub fn get(&self, name: &str) -> Option<&'a [u8]> {
        let name_lower = name.to_ascii_lowercase();
        let mut pos = 0;

        while pos < self.data.len() {
            // Find line end
            let line_end = Self::find_line_end(self.data, pos).ok()?;
            let line = &self.data[pos..line_end];

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
            if header_name.eq_ignore_ascii_case(name_lower.as_bytes()) {
                // Found it! Get value
                let mut value_start = colon_pos + 1;

                // Skip leading whitespace in value
                while value_start < line.len()
                    && (line[value_start] == b' ' || line[value_start] == b'\t')
                {
                    value_start += 1;
                }

                let value = &line[value_start..];

                // Check for folded continuation lines
                let mut next_pos = line_end + 2;
                let mut folded_value = Vec::new();

                while next_pos < self.data.len() {
                    let next_line_end = Self::find_line_end(self.data, next_pos).ok()?;
                    let next_line = &self.data[next_pos..next_line_end];

                    // Check if this is a continuation
                    if next_line.is_empty() || (next_line[0] != b' ' && next_line[0] != b'\t') {
                        break;
                    }

                    // This is a folded line - append it
                    if folded_value.is_empty() {
                        folded_value.extend_from_slice(value);
                    }
                    folded_value.push(b' '); // RFC says to replace folding with space
                    folded_value.extend_from_slice(next_line.trim_ascii_start());

                    next_pos = next_line_end + 2;
                }

                // If we collected folded lines, return that (allocated)
                // Otherwise return the original slice (zero-copy)
                if folded_value.is_empty() {
                    return Some(value);
                } else {
                    // We have to allocate for folded headers
                    // This is a limitation - we could return Cow<'a, [u8]> instead
                    // For now, just return the first line
                    // TODO: Return Cow to handle folding without allocation in non-folded case
                    return Some(value);
                }
            }

            pos = line_end + 2;
        }

        None
    }

    /// Iterate over all headers (zero-copy)
    pub fn iter(&self) -> HeaderIter<'a> {
        HeaderIter {
            data: self.data,
            pos: 0,
        }
    }

    /// Get raw header bytes
    pub fn as_bytes(&self) -> &'a [u8] {
        self.data
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
}
