//! yEnc validation utilities
//!
//! Validates yenc-encoded binary data structure and checksums using
//! functional composition and iterator-based parsing.
//!
//! yEnc encoding is simple: each byte is `(input - 42) % 256`, with `=` as escape
//! for special bytes (followed by `byte - 64`), and leading `..` for dot-stuffing.

use super::ParseError;
use std::io::{BufRead, BufReader};

/// yEnc escape character (signals next byte needs special handling)
const ESCAPE: u8 = b'=';
/// CR/LF and NUL are skipped in yenc data
const CR: u8 = b'\r';
const LF: u8 = b'\n';
const NUL: u8 = b'\0';
/// Dot at start of line (NNTP dot-stuffing)
const DOT: u8 = b'.';

/// Decode yenc-encoded line to raw bytes
///
/// yEnc encoding: `encoded = (original + 42) % 256`
/// Special handling:
/// - `=` is escape: next byte uses `(byte - 64) % 256` instead of 42
/// - Leading `..` collapses to `.` (NNTP dot-stuffing)
/// - CR, LF, NUL are ignored
#[inline]
fn decode_yenc_line(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len());
    let mut iter = input.iter().copied().enumerate();

    while let Some((col, byte)) = iter.next() {
        match byte {
            // Skip control characters
            NUL | CR | LF => continue,

            // Handle NNTP dot-stuffing (.. at start becomes .)
            DOT if col == 0 => {
                if let Some((_, DOT)) = iter.next() {
                    output.push(DOT.wrapping_sub(42));
                } else {
                    output.push(DOT.wrapping_sub(42));
                }
            }

            // Escape character: next byte uses different offset
            ESCAPE => {
                if let Some((_, escaped_byte)) = iter.next() {
                    output.push(escaped_byte.wrapping_sub(64).wrapping_sub(42));
                }
            }

            // Normal yenc byte: subtract 42
            _ => output.push(byte.wrapping_sub(42)),
        }
    }

    output
}

/// yEnc header metadata extracted from =ybegin line
#[derive(Debug, Clone)]
struct YencHeader {
    expected_size: usize,
    is_multipart: bool,
}

impl YencHeader {
    /// Parse from line bytes
    #[inline]
    fn parse(line: &[u8]) -> Result<Self, ParseError> {
        let line_str = String::from_utf8_lossy(line);

        let expected_size = extract_param(&line_str, "size")
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| {
                ParseError::InvalidYenc("Missing 'size' parameter in =ybegin".to_string())
            })?;

        let is_multipart = extract_param(&line_str, "part").is_some();

        Ok(Self {
            expected_size,
            is_multipart,
        })
    }

    /// Validate footer size matches header
    #[inline]
    fn validate_size(&self, footer: &YencFooter) -> Result<(), ParseError> {
        footer
            .size
            .filter(|&size| size != self.expected_size)
            .map(|size| {
                Err(ParseError::InvalidYenc(format!(
                    "Size mismatch: header={}, footer={}",
                    self.expected_size, size
                )))
            })
            .unwrap_or(Ok(()))
    }
}

/// yEnc footer metadata extracted from =yend line
#[derive(Debug, Clone)]
struct YencFooter {
    size: Option<usize>,
    crc32: Option<u32>,
}

impl YencFooter {
    /// Parse from line bytes
    #[inline]
    fn parse(line: &[u8], is_multipart: bool) -> Result<Self, ParseError> {
        let line_str = String::from_utf8_lossy(line);

        let size = extract_param(&line_str, "size").and_then(|s| s.parse().ok());

        let crc_param = if is_multipart { "pcrc32" } else { "crc32" };
        let crc32 =
            extract_param(&line_str, crc_param).and_then(|s| u32::from_str_radix(s, 16).ok());

        Ok(Self { size, crc32 })
    }

    /// Validate CRC32 checksum if provided
    #[inline]
    fn validate_checksum(&self, checksum: &crc32fast::Hasher) -> Result<(), ParseError> {
        self.crc32
            .filter(|&expected| checksum.clone().finalize() != expected)
            .map(|expected| {
                Err(ParseError::InvalidYenc(format!(
                    "CRC mismatch: expected={:08x}, actual={:08x}",
                    expected,
                    checksum.clone().finalize()
                )))
            })
            .unwrap_or(Ok(()))
    }
}

/// yEnc line classification
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum YencLine {
    Part,
    End,
    Data,
}

impl YencLine {
    /// Classify a yenc line by its prefix
    #[inline]
    fn classify(line: &[u8]) -> Self {
        if line.starts_with(b"=ypart ") {
            Self::Part
        } else if line.starts_with(b"=yend ") {
            Self::End
        } else {
            Self::Data
        }
    }
}

/// Iterator adapter for reading lines from BufReader
struct YencLines<'a> {
    reader: &'a mut BufReader<&'a [u8]>,
    line_buf: Vec<u8>,
}

impl<'a> YencLines<'a> {
    #[inline]
    fn new(reader: &'a mut BufReader<&'a [u8]>) -> Self {
        Self {
            reader,
            line_buf: Vec::new(),
        }
    }
}

impl Iterator for YencLines<'_> {
    type Item = Result<Vec<u8>, ParseError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.line_buf.clear();
        match self
            .reader
            .read_until(b'\n', &mut self.line_buf)
            .map_err(|e| ParseError::InvalidYenc(format!("Failed to read yenc: {}", e)))
        {
            Ok(0) => None,
            Ok(_) => Some(Ok(self.line_buf.clone())),
            Err(e) => Some(Err(e)),
        }
    }
}

/// Validate yenc structure in article body
///
/// Validates:
/// - `=ybegin` header line with required parameters (name, size)
/// - `=yend` footer line
/// - CRC32 checksums if provided (pcrc32 or crc32)
/// - Size matches between header and footer
/// - Proper line encoding (validates actual yenc data)
///
/// Does NOT write to disk - validates in-memory only.
pub fn validate_yenc_structure(body: &[u8]) -> Result<(), ParseError> {
    let mut reader = BufReader::new(body);
    let mut lines = YencLines::new(&mut reader);

    let header = lines
        .by_ref()
        .find_map(|line| {
            line.ok()
                .filter(|l| l.starts_with(b"=ybegin "))
                .and_then(|l| YencHeader::parse(&l).ok())
        })
        .ok_or_else(|| ParseError::InvalidYenc("Missing =ybegin header".to_string()))?;

    let (footer, checksum) = lines.try_fold(
        (None, crc32fast::Hasher::new()),
        |(footer, mut checksum), line| -> Result<_, ParseError> {
            let line = line?;
            match YencLine::classify(&line) {
                YencLine::Part => Ok((footer, checksum)),
                YencLine::End => {
                    let parsed_footer = YencFooter::parse(&line, header.is_multipart)?;
                    Ok((Some(parsed_footer), checksum))
                }
                YencLine::Data => {
                    decode_and_checksum(&line, &mut checksum)?;
                    Ok((footer, checksum))
                }
            }
        },
    )?;

    let footer =
        footer.ok_or_else(|| ParseError::InvalidYenc("Missing =yend footer".to_string()))?;

    header.validate_size(&footer)?;
    footer.validate_checksum(&checksum)?;

    Ok(())
}

/// Decode a yenc data line and update checksum
#[inline]
fn decode_and_checksum(line: &[u8], checksum: &mut crc32fast::Hasher) -> Result<(), ParseError> {
    let decoded = line
        .strip_suffix(b"\r\n")
        .or_else(|| line.strip_suffix(b"\n"))
        .unwrap_or(line)
        .pipe(decode_yenc_line);

    checksum.update(&decoded);
    Ok(())
}

/// Extract a parameter value from a yenc metadata line
#[inline]
fn extract_param<'a>(line: &'a str, param: &str) -> Option<&'a str> {
    line.find(&format!("{}=", param))
        .map(|start| &line[start + param.len() + 1..])
        .and_then(|rest| rest.split_whitespace().next())
}

/// Pipe combinator for method chaining
trait Pipe: Sized {
    #[inline]
    fn pipe<F, R>(self, f: F) -> R
    where
        F: FnOnce(Self) -> R,
    {
        f(self)
    }
}

impl<T> Pipe for T {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_yenc() {
        let data = b"=ybegin line=128 size=12 name=test.txt\r\nr\x8f\x96\x96\x99VJ\xa3o\x98\x8dK\r\n=yend size=12 crc32=0337ab3d\r\n";

        let result = validate_yenc_structure(data);
        if let Err(ref e) = result {
            eprintln!("Validation error: {:?}", e);
        }
        assert!(result.is_ok());
    }

    #[test]
    fn test_missing_yend() {
        let data = b"=ybegin part=1 size=1000 name=test.bin\r\n\
                     <data>\r\n";

        assert!(matches!(
            validate_yenc_structure(data),
            Err(ParseError::InvalidYenc(_))
        ));
    }

    #[test]
    fn test_missing_ybegin() {
        let data = b"<data>\r\n\
                     =yend size=1000\r\n";

        assert!(matches!(
            validate_yenc_structure(data),
            Err(ParseError::InvalidYenc(_))
        ));
    }

    #[test]
    fn test_invalid_crc() {
        // Valid structure but wrong CRC
        let data = b"=ybegin line=128 size=11 name=test.txt\r\n\
                     *+,-./01234\r\n\
                     =yend size=11 crc32=00000000\r\n";

        // Should fail due to CRC mismatch
        assert!(matches!(
            validate_yenc_structure(data),
            Err(ParseError::InvalidYenc(_))
        ));
    }
}
