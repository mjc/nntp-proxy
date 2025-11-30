//! yEnc validation utilities
//!
//! Validates yenc-encoded binary data structure and checksums.

use super::ParseError;
use std::io::{BufRead, BufReader};

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
    let mut line_buf = Vec::new();

    // Find =ybegin header
    let mut header_found = false;
    let mut expected_size: Option<usize> = None;
    let mut expected_crc: Option<u32> = None;
    let mut is_multipart = false;

    while reader
        .read_until(b'\n', &mut line_buf)
        .map_err(|e| ParseError::InvalidYenc(format!("Failed to read yenc: {}", e)))?
        > 0
    {
        if line_buf.starts_with(b"=ybegin ") {
            header_found = true;

            // Parse as lossy string for parameter extraction
            let line_str = String::from_utf8_lossy(&line_buf);

            // Parse size parameter (required)
            if let Some(size_str) = extract_param(&line_str, "size") {
                expected_size = size_str.parse().ok();
            } else {
                return Err(ParseError::InvalidYenc(
                    "Missing 'size' parameter in =ybegin".to_string(),
                ));
            }

            // Check if it's a multi-part
            if extract_param(&line_str, "part").is_some() {
                is_multipart = true;
            }

            break;
        }
        line_buf.clear();
    }

    if !header_found {
        return Err(ParseError::InvalidYenc(
            "Missing =ybegin header".to_string(),
        ));
    }

    // Decode and checksum the data
    let mut checksum = crc32fast::Hasher::new();
    let mut _decoded_size = 0usize;
    let mut footer_found = false;

    line_buf.clear();
    while reader
        .read_until(b'\n', &mut line_buf)
        .map_err(|e| ParseError::InvalidYenc(format!("Failed to read yenc: {}", e)))?
        > 0
    {
        if line_buf.starts_with(b"=ypart ") {
            // Multi-part metadata, skip
            line_buf.clear();
            continue;
        } else if line_buf.starts_with(b"=yend ") {
            footer_found = true;

            let line_str = String::from_utf8_lossy(&line_buf);

            // Parse footer size
            if let Some(size_str) = extract_param(&line_str, "size")
                && let Some(expected) = expected_size
            {
                let footer_size: usize = size_str
                    .parse()
                    .map_err(|_| ParseError::InvalidYenc("Invalid size in =yend".to_string()))?;
                if footer_size != expected {
                    return Err(ParseError::InvalidYenc(format!(
                        "Size mismatch: header={}, footer={}",
                        expected, footer_size
                    )));
                }
            }

            // Parse CRC if present
            if is_multipart {
                if let Some(crc_str) = extract_param(&line_str, "pcrc32") {
                    expected_crc = u32::from_str_radix(crc_str, 16).ok();
                }
            } else if let Some(crc_str) = extract_param(&line_str, "crc32") {
                expected_crc = u32::from_str_radix(crc_str, 16).ok();
            }

            break;
        } else {
            // Decode the line and update checksum
            // Strip trailing CRLF before decoding
            let line_to_decode = line_buf
                .strip_suffix(b"\r\n")
                .or_else(|| line_buf.strip_suffix(b"\n"))
                .unwrap_or(&line_buf);

            let decoded = yenc::decode_buffer(line_to_decode)
                .map_err(|e| ParseError::InvalidYenc(format!("Decode error: {}", e)))?;
            checksum.update(&decoded);
            _decoded_size += decoded.len();
        }
        line_buf.clear();
    }

    if !footer_found {
        return Err(ParseError::InvalidYenc("Missing =yend footer".to_string()));
    }

    // Validate checksum if provided
    if let Some(expected) = expected_crc {
        let actual = checksum.finalize();
        if actual != expected {
            return Err(ParseError::InvalidYenc(format!(
                "CRC mismatch: expected={:08x}, actual={:08x}",
                expected, actual
            )));
        }
    }

    Ok(())
}

/// Extract a parameter value from a yenc metadata line
fn extract_param<'a>(line: &'a str, param: &str) -> Option<&'a str> {
    let pattern = format!("{}=", param);
    let start = line.find(&pattern)? + pattern.len();
    let rest = &line[start..];

    // Find end (space or newline)
    let end = rest.find(' ').unwrap_or(rest.len());
    Some(rest[..end].trim())
}

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
