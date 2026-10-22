//! yEnc validation utilities
//!
//! Validates yenc-encoded binary data structure and checksums.
//! Decodes wire dot stuffing and yEnc escapes in a single pass.

use super::ParseError;
use std::io::{BufRead, BufReader};

/// Decode a wire yEnc data line, removing one leading stuffed dot.
#[inline]
#[must_use]
pub fn decode_yenc_line(input: &[u8]) -> Vec<u8> {
    let input = if input.get(..2) == Some(b"..") {
        &input[1..]
    } else {
        input
    };
    let mut bytes = input.iter().copied();
    let mut output = Vec::with_capacity(input.len());
    while let Some(byte) = bytes.next() {
        let encoded = match byte {
            b'\r' | b'\n' | 0 => continue,
            b'=' => match bytes.next() {
                Some(escaped) => escaped.wrapping_sub(64),
                None => break,
            },
            byte => byte,
        };
        output.push(encoded.wrapping_sub(42));
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
}

/// Inclusive, one-based byte range within the complete file.
fn parse_part_size(line: &[u8], file_size: usize) -> Result<usize, ParseError> {
    let line = String::from_utf8_lossy(line);
    let begin = extract_param(&line, "begin").and_then(|v| v.parse::<usize>().ok());
    let end = extract_param(&line, "end").and_then(|v| v.parse::<usize>().ok());
    match (begin, end) {
        (Some(begin), Some(end)) if begin > 0 && end >= begin && end <= file_size => {
            Ok(end - begin + 1)
        }
        _ => Err(ParseError::InvalidYenc(
            "Invalid =ypart byte range".to_string(),
        )),
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
    fn parse(line: &[u8], is_multipart: bool) -> Self {
        let line_str = String::from_utf8_lossy(line);

        let size = extract_param(&line_str, "size").and_then(|s| s.parse().ok());

        let crc_param = if is_multipart { "pcrc32" } else { "crc32" };
        let crc32 =
            extract_param(&line_str, crc_param).and_then(|s| u32::from_str_radix(s, 16).ok());

        Self { size, crc32 }
    }

    /// Validate CRC32 checksum if provided
    #[inline]
    fn validate_checksum(&self, checksum: &crc32fast::Hasher) -> Result<(), ParseError> {
        self.crc32
            .filter(|&expected| checksum.clone().finalize() != expected)
            .map_or(Ok(()), |expected| {
                Err(ParseError::InvalidYenc(format!(
                    "CRC mismatch: expected={:08x}, actual={:08x}",
                    expected,
                    checksum.clone().finalize()
                )))
            })
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

/// Iterator adapter for reading lines from `BufReader`
struct YencLines<'a> {
    reader: &'a mut BufReader<&'a [u8]>,
    line_buf: Vec<u8>,
}

impl<'a> YencLines<'a> {
    #[inline]
    const fn new(reader: &'a mut BufReader<&'a [u8]>) -> Self {
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
            .map_err(|e| ParseError::InvalidYenc(format!("Failed to read yenc: {e}")))
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
/// - Decoded size matches the part range (multipart) or file size (single part)
/// - Proper line encoding (validates actual yenc data)
///
/// Does NOT write to disk - validates in-memory only.
///
/// # Errors
/// Returns `ParseError` when the body is missing required yEnc structure, has
/// invalid footer/header metadata, or fails checksum/size validation.
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

    let mut expected_size = if header.is_multipart {
        None
    } else {
        Some(header.expected_size)
    };
    let mut checksum = crc32fast::Hasher::new();
    let mut decoded_size = 0usize;
    let mut footer = None;
    for line in lines {
        let line = line?;
        match YencLine::classify(&line) {
            YencLine::Part if header.is_multipart && expected_size.is_none() => {
                expected_size = Some(parse_part_size(&line, header.expected_size)?);
            }
            YencLine::Part => {
                return Err(ParseError::InvalidYenc(
                    "Unexpected =ypart header".to_string(),
                ));
            }
            YencLine::End => {
                footer = Some(YencFooter::parse(&line, header.is_multipart));
                break;
            }
            YencLine::Data => {
                if expected_size.is_none() {
                    return Err(ParseError::InvalidYenc("Missing =ypart header".to_string()));
                }
                decoded_size = decoded_size
                    .checked_add(decode_and_checksum(&line, &mut checksum))
                    .ok_or_else(|| ParseError::InvalidYenc("Decoded size overflow".to_string()))?;
            }
        }
    }
    let footer =
        footer.ok_or_else(|| ParseError::InvalidYenc("Missing =yend footer".to_string()))?;
    let expected_size = expected_size
        .ok_or_else(|| ParseError::InvalidYenc("Missing =ypart header".to_string()))?;
    if footer.size != Some(expected_size) || decoded_size != expected_size {
        return Err(ParseError::InvalidYenc(format!(
            "Size mismatch: expected={expected_size}, footer={:?}, decoded={decoded_size}",
            footer.size
        )));
    }
    footer.validate_checksum(&checksum)?;

    Ok(())
}

/// Decode a yenc data line and update checksum
#[inline]
fn decode_and_checksum(line: &[u8], checksum: &mut crc32fast::Hasher) -> usize {
    let decoded = line
        .strip_suffix(b"\r\n")
        .or_else(|| line.strip_suffix(b"\n"))
        .unwrap_or(line)
        .pipe(decode_yenc_line);

    checksum.update(&decoded);
    decoded.len()
}

/// Extract a parameter value from a yenc metadata line
#[inline]
fn extract_param<'a>(line: &'a str, param: &str) -> Option<&'a str> {
    let prefix = format!("{param}=");
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(&prefix))
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
    use proptest::prelude::*;

    fn encode_wire_yenc_line(data: &[u8]) -> Vec<u8> {
        let mut wire = Vec::new();
        for byte in data {
            let encoded = byte.wrapping_add(42);
            if [0, 10, 13, b'='].contains(&encoded) {
                wire.extend_from_slice(&[b'=', encoded.wrapping_add(64)]);
            } else {
                wire.push(encoded);
            }
        }
        if wire.first() == Some(&b'.') {
            wire.insert(0, b'.');
        }
        wire
    }

    fn encode_wire_yenc_payload(data: &[u8], line_width: usize) -> Vec<u8> {
        let mut body = Vec::new();
        for line in data.chunks(line_width) {
            body.extend(encode_wire_yenc_line(line));
            body.extend_from_slice(b"\r\n");
        }
        body
    }

    proptest! {
        #[test]
        fn prop_singlepart_yenc_checks_decoded_sizes_without_checksums(
            payload in prop_oneof![Just(Vec::new()), prop::collection::vec(any::<u8>(), 1..256)],
            line_width in 1usize..=64,
        ) {
            let encoded = encode_wire_yenc_payload(&payload, line_width);
            let size = payload.len();
            let mut valid = format!("=ybegin xsize={} xpart=9 line=128 size={size} name=property.bin\r\n", size + 1).into_bytes();
            valid.extend_from_slice(&encoded);
            valid.extend_from_slice(format!("=yend size={size}\r\n").as_bytes());
            prop_assert!(validate_yenc_structure(&valid).is_ok());
            let mut response = b"222 0 <property@example.com> body\r\n".to_vec();
            response.extend(valid);
            let article = super::super::Article::parse(&response, true).unwrap();
            let mut decoded = vec![0xff; payload.len() + 17];
            prop_assert!(article.decode_into(&mut decoded));
            prop_assert_eq!(&decoded, &payload);

            // Matching metadata must still fail when it disagrees with the payload.
            for (header_size, footer_size) in [(size + 1, size), (size, size + 1), (size + 1, size + 1)] {
                let mut body = format!("=ybegin line=128 size={header_size} name=property.bin\r\n").into_bytes();
                body.extend_from_slice(&encoded);
                body.extend_from_slice(format!("=yend size={footer_size}\r\n").as_bytes());
                prop_assert!(validate_yenc_structure(&body).is_err());
            }
        }

        #[test]
        fn prop_multipart_yenc_rejects_invalid_and_overflowing_ranges(
            payload in prop::collection::vec(any::<u8>(), 1..128),
            prefix_len in 1usize..64,
            suffix_len in 1usize..64,
        ) {
            let file_size = prefix_len + payload.len() + suffix_len;
            let begin = prefix_len + 1;
            let end = prefix_len + payload.len();
            let encoded = encode_wire_yenc_payload(&payload, 32);
            let part_lines = [
                format!("=ypart begin=0 end={end}\r\n"),
                format!("=ypart begin={} end={begin}\r\n", end + 1),
                format!("=ypart begin={begin} end={}\r\n", file_size + 1),
                format!("=ypart end={end}\r\n"),
                format!("=ypart begin={begin}\r\n"),
                format!("=ypart begin={}0 end={end}\r\n", usize::MAX),
                format!("=ypart begin={begin} end={}0\r\n", usize::MAX),
            ];
            for part_line in part_lines {
                let mut body = format!("=ybegin part=2 total=3 line=128 size={file_size} name=property.bin\r\n{part_line}").into_bytes();
                body.extend_from_slice(&encoded);
                body.extend_from_slice(format!("=yend size={} part=2\r\n", payload.len()).as_bytes());
                prop_assert!(validate_yenc_structure(&body).is_err());
            }
        }

        #[test]
        fn prop_multipart_yenc_requires_exactly_one_part_header_before_data(
            payload in prop::collection::vec(any::<u8>(), 1..128),
            prefix_len in 1usize..64,
        ) {
            let header = format!("=ybegin part=2 total=3 line=128 size={} name=property.bin\r\n", prefix_len + payload.len() + 1);
            let part = format!("=ypart begin={} end={}\r\n", prefix_len + 1, prefix_len + payload.len());
            let data = encode_wire_yenc_payload(&payload, 32);
            let footer = format!("=yend size={} part=2\r\n", payload.len());
            let valid = [header.as_bytes(), part.as_bytes(), &data, footer.as_bytes()].concat();
            prop_assert!(validate_yenc_structure(&valid).is_ok());
            for body in [
                [header.as_bytes(), &data, footer.as_bytes()].concat(),
                [header.as_bytes(), part.as_bytes(), part.as_bytes(), &data, footer.as_bytes()].concat(),
                [header.as_bytes(), &data, part.as_bytes(), footer.as_bytes()].concat(),
            ] {
                prop_assert!(validate_yenc_structure(&body).is_err());
            }
        }

        #[test]
        fn prop_yenc_checksum_parameters_are_scoped_to_part_kind(
            payload in prop::collection::vec(any::<u8>(), 1..256),
            line_width in 1usize..=64,
        ) {
            let data = encode_wire_yenc_payload(&payload, line_width);
            let size = payload.len();
            let crc = crc32fast::hash(&payload);
            let wrong = crc ^ 1;
            let single = format!("=ybegin line=128 size={size} name=property.bin\r\n");
            let multipart = format!("=ybegin part=1 total=2 line=128 size={} name=property.bin\r\n=ypart begin=1 end={size}\r\n", size + 1);
            for (header, valid_footer, invalid_footer) in [
                (single, format!("=yend size={size} pcrc32={wrong:08x} crc32={crc:08x}\r\n"), format!("=yend size={size} pcrc32={crc:08x} crc32={wrong:08x}\r\n")),
                (multipart, format!("=yend size={size} part=1 crc32={wrong:08x} pcrc32={crc:08x}\r\n"), format!("=yend size={size} part=1 crc32={crc:08x} pcrc32={wrong:08x}\r\n")),
            ] {
                let valid = [header.as_bytes(), &data, valid_footer.as_bytes()].concat();
                let invalid = [header.as_bytes(), &data, invalid_footer.as_bytes()].concat();
                prop_assert!(validate_yenc_structure(&valid).is_ok());
                prop_assert!(validate_yenc_structure(&invalid).is_err());
            }
        }

        #[test]
        fn prop_yenc_roundtrip_preserves_arbitrary_bytes(
            leading_dots in 0usize..5,
            payload in prop::collection::vec(any::<u8>(), 0..1024),
        ) {
            let mut plain = vec![4; leading_dots];
            plain.extend(payload);
            prop_assert_eq!(decode_yenc_line(&encode_wire_yenc_line(&plain)), plain);
        }

        #[test]
        fn prop_multipart_yenc_roundtrip_validates_part_checksum(
            payload in prop::collection::vec(any::<u8>(), 1..512),
            prefix_len in 0usize..1024,
            suffix_len in 0usize..1024,
        ) {
            let suffix_len = suffix_len.max(usize::from(prefix_len == 0));
            let part = if prefix_len == 0 { 1 } else { 2 };
            let total = part + usize::from(suffix_len != 0);
            let mut body = format!(
                "=ybegin part={part} total={total} line=128 size={} name=property.bin\r\n=ypart begin={} end={}\r\n",
                prefix_len + payload.len() + suffix_len,
                prefix_len + 1, prefix_len + payload.len(),
            ).into_bytes();
            // Escaping can double a line's length, so 64 decoded bytes fit line=128.
            for line in payload.chunks(64) {
                body.extend(encode_wire_yenc_line(line));
                body.extend_from_slice(b"\r\n");
            }
            let crc = crc32fast::hash(&payload);
            let footer = format!("=yend size={} part={part} pcrc32={crc:08x}\r\n", payload.len());
            let mut valid = body.clone();
            valid.extend_from_slice(footer.as_bytes());
            prop_assert!(validate_yenc_structure(&valid).is_ok());
            let mut response = b"222 0 <property@example.com> body\r\n".to_vec();
            response.extend(valid);
            let article = super::super::Article::parse(&response, true).unwrap();
            let decoded = article.decode().unwrap();
            prop_assert_eq!(&decoded, &payload);

            let wrong_crc = crc ^ 1;
            body.extend_from_slice(format!("=yend size={} part={part} pcrc32={wrong_crc:08x}\r\n", payload.len()).as_bytes());
            prop_assert!(validate_yenc_structure(&body).is_err());
        }
    }

    #[test]
    fn multipart_yenc_validates_part_sizes_and_checksums() {
        let crc = crc32fast::hash(b"ABCDEF");
        for (part, begin, end) in [(1, 1, 6), (2, 7, 12), (3, 13, 18)] {
            let body = format!(
                "=ybegin part={part} total=3 line=128 size=18 name=sample.bin\r\n=ypart begin={begin} end={end}\r\nklmnop\r\n=yend size=6 part={part} pcrc32={crc:08x} crc32=00000000\r\n"
            );
            validate_yenc_structure(body.as_bytes())
                .expect("part size differs from whole file size");
        }
    }

    #[test]
    fn yenc_rejects_payload_size_mismatches_without_checksums() {
        for body in [
            "=ybegin size=12 name=file\r\nklmnop\r\n=yend size=12\r\n",
            "=ybegin part=1 size=12 name=file\r\n=ypart begin=1 end=12\r\nklmnop\r\n=yend size=12\r\n",
            "=ybegin part=1 size=12 name=file\r\n=ypart begin=1 end=6\r\nklmnop\r\n=yend size=5\r\n",
            "=ybegin part=1 size=12 name=file\r\n=ypart begin=0 end=5\r\nklmnop\r\n=yend size=6\r\n",
            "=ybegin part=1 size=12 name=file\r\n=ypart begin=8 end=13\r\nklmnop\r\n=yend size=6\r\n",
            "=ybegin part=1 size=6 name=file\r\nklmnop\r\n=yend size=6\r\n",
        ] {
            assert!(
                validate_yenc_structure(body.as_bytes()).is_err(),
                "accepted {body:?}"
            );
        }
    }

    #[test]
    fn test_valid_yenc() {
        let data = b"=ybegin line=128 size=12 name=test.txt\r\nr\x8f\x96\x96\x99VJ\xa3o\x98\x8dK\r\n=yend size=12 crc32=0337ab3d\r\n";

        let result = validate_yenc_structure(data);
        assert!(result.is_ok());
    }

    #[test]
    fn decode_yenc_line_removes_one_wire_stuffed_leading_dot() {
        let once_stuffed = decode_yenc_line(b"..encoded");
        let twice_stuffed = decode_yenc_line(b"...encoded");

        assert_eq!(
            once_stuffed,
            [
                vec![4],
                b"encoded".iter().map(|b| b.wrapping_sub(42)).collect()
            ]
            .concat()
        );
        assert_eq!(
            twice_stuffed,
            [
                vec![4, 4],
                b"encoded".iter().map(|b| b.wrapping_sub(42)).collect()
            ]
            .concat()
        );
    }

    #[test]
    fn stuffed_leading_dots_contribute_to_the_yenc_checksum_once() {
        let mut actual = crc32fast::Hasher::new();
        decode_and_checksum(b"..\r\n", &mut actual);

        let mut expected = crc32fast::Hasher::new();
        expected.update(&[4]);
        assert_eq!(actual.finalize(), expected.finalize());

        let mut two_bytes = crc32fast::Hasher::new();
        decode_and_checksum(b"...\r\n", &mut two_bytes);
        let mut expected_two_bytes = crc32fast::Hasher::new();
        expected_two_bytes.update(&[4, 4]);
        assert_eq!(two_bytes.finalize(), expected_two_bytes.finalize());
    }

    #[test]
    fn validate_yenc_preserves_two_and_three_wire_leading_dots() {
        for wire_line in [b"..".as_slice(), b"...".as_slice()] {
            let decoded = decode_yenc_line(wire_line);
            let mut checksum = crc32fast::Hasher::new();
            checksum.update(&decoded);
            let crc = checksum.finalize();
            let body = format!(
                "=ybegin line=128 size={} name=dots.bin\r\n{}\r\n=yend size={} crc32={crc:08x}\r\n",
                decoded.len(),
                String::from_utf8_lossy(wire_line),
                decoded.len(),
            );

            validate_yenc_structure(body.as_bytes())
                .expect("wire dot stuffing must be removed exactly once");
        }
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
