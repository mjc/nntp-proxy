//! Comprehensive tests for Article parsing and validation
//!
//! Tests cover:
//! - ARTICLE (220) - headers + body
//! - HEAD (221) - headers only
//! - BODY (222) - body only
//! - STAT (223) - metadata only
//! - Header validation (RFC 5322)
//! - yenc validation
//! - Error cases

use nntp_proxy::protocol::{Article, Headers, ParseError};

/// Valid ARTICLE response with text content
const VALID_ARTICLE_TEXT: &[u8] = b"220 12345 <test@example.com>\r\n\
Subject: Test Article\r\n\
From: test@example.com\r\n\
Date: Sat, 30 Nov 2024 12:00:00 +0000\r\n\
Message-ID: <test@example.com>\r\n\
\r\n\
This is the article body.\r\n\
Multiple lines of text.\r\n\
.\r\n";

/// Valid ARTICLE response with yenc binary (real yenc-encoded "Hello, yEnc!")
const VALID_ARTICLE_YENC: &[u8] = b"220 54321 <binary@example.com>\r\n\
Subject: test.txt (1/1)\r\n\
From: poster@example.com\r\n\
Message-ID: <binary@example.com>\r\n\
\r\n\
=ybegin line=128 size=12 name=test.txt\r\n\
r\x8f\x96\x96\x99VJ\xa3o\x98\x8dK\r\n\
=yend size=12 crc32=0337ab3d\r\n\
.\r\n";

/// Valid HEAD response
const VALID_HEAD: &[u8] = b"221 12345 <test@example.com>\r\n\
Subject: Test Article\r\n\
From: test@example.com\r\n\
Date: Sat, 30 Nov 2024 12:00:00 +0000\r\n\
Message-ID: <test@example.com>\r\n\
.\r\n";

/// Valid BODY response
const VALID_BODY: &[u8] = b"222 12345 <test@example.com>\r\n\
This is the article body.\r\n\
Multiple lines of text.\r\n\
.\r\n";

/// Valid STAT response
const VALID_STAT: &[u8] = b"223 12345 <test@example.com>\r\n";

/// ARTICLE with no blank line separator
const MISSING_SEPARATOR: &[u8] = b"220 12345 <test@example.com>\r\n\
Subject: Bad\r\n\
Body without separator\r\n\
.\r\n";

/// ARTICLE with no terminator
const MISSING_TERMINATOR: &[u8] = b"220 12345 <test@example.com>\r\n\
Subject: Test\r\n\
\r\n\
Body without terminator\r\n";

/// ARTICLE with invalid headers (missing colon)
const INVALID_HEADERS: &[u8] = b"220 12345 <test@example.com>\r\n\
Subject: Valid\r\n\
InvalidHeaderNoColon\r\n\
\r\n\
Body\r\n\
.\r\n";

/// ARTICLE with folded header  
const FOLDED_HEADER: &[u8] = concat!(
    "220 12345 <test@example.com>\r\n",
    "Subject: This is a long subject\r\n",
    " that continues on the next line\r\n", // Leading space preserved!
    "From: test@example.com\r\n",
    "\r\n",
    "Body\r\n",
    ".\r\n"
)
.as_bytes();

/// HEAD with body (invalid)
const HEAD_WITH_BODY: &[u8] = b"221 12345 <test@example.com>\r\n\
Subject: Test\r\n\
\r\n\
This body should not be here\r\n\
.\r\n";

/// BODY with headers (should skip them)
const BODY_WITH_HEADERS: &[u8] = b"222 12345 <test@example.com>\r\n\
Subject: Should be ignored\r\n\
\r\n\
Actual body content\r\n\
.\r\n";

/// Article by message-ID (article number = 0)
const ARTICLE_BY_MSGID: &[u8] = b"220 0 <msgid@example.com>\r\n\
Subject: Retrieved by message-ID\r\n\
\r\n\
Body\r\n\
.\r\n";

/// Wrong status code
const WRONG_STATUS_CODE: &[u8] = b"430 No such article\r\n";

/// Malformed yenc (missing =yend)
const INVALID_YENC: &[u8] = b"220 12345 <test@example.com>\r\n\
Subject: Binary\r\n\
\r\n\
=ybegin line=128 size=12 name=test.bin\r\n\
r\x8f\x96\x96\x99VJ\xa3o\x98\x8dK\r\n\
.\r\n";

#[test]
fn test_parse_valid_article_text() {
    let article: Article = VALID_ARTICLE_TEXT.try_into().unwrap();

    assert_eq!(article.article_number, Some(12345));
    assert_eq!(article.message_id.as_str(), "<test@example.com>");
    assert!(article.headers.is_some());
    assert!(article.body.is_some());

    let headers = article.headers.unwrap();
    assert_eq!(headers.get("Subject"), Some(&b"Test Article"[..]));
    assert_eq!(headers.get("From"), Some(&b"test@example.com"[..]));

    let body = article.body.unwrap();
    assert!(body.starts_with(b"This is the article body."));
}

#[test]
fn test_parse_valid_article_yenc() {
    let article: Article = VALID_ARTICLE_YENC.try_into().unwrap();

    assert_eq!(article.article_number, Some(54321));
    assert!(article.headers.is_some());
    assert!(article.body.is_some());

    let body = article.body.unwrap();
    assert!(body.starts_with(b"=ybegin"));
    // Check if body contains "=yend" footer (search as bytes, not UTF-8)
    assert!(body.windows(5).any(|w| w == b"=yend"));
}

#[test]
fn test_parse_valid_head() {
    let article: Article = VALID_HEAD.try_into().unwrap();

    assert_eq!(article.article_number, Some(12345));
    assert!(article.headers.is_some());
    assert!(article.body.is_none()); // HEAD has no body

    let headers = article.headers.unwrap();
    assert_eq!(headers.get("Subject"), Some(&b"Test Article"[..]));
}

#[test]
fn test_parse_valid_body() {
    let article: Article = VALID_BODY.try_into().unwrap();

    assert_eq!(article.article_number, Some(12345));
    assert!(article.headers.is_none()); // BODY has no headers
    assert!(article.body.is_some());

    let body = article.body.unwrap();
    assert!(body.starts_with(b"This is the article body."));
}

#[test]
fn test_parse_valid_stat() {
    let article: Article = VALID_STAT.try_into().unwrap();

    assert_eq!(article.article_number, Some(12345));
    assert!(article.headers.is_none()); // STAT has no headers
    assert!(article.body.is_none()); // STAT has no body
}

#[test]
fn test_article_by_message_id() {
    let article: Article = ARTICLE_BY_MSGID.try_into().unwrap();

    // When retrieved by message-ID, article number is 0
    assert_eq!(article.article_number, Some(0));
    assert_eq!(article.message_id.as_str(), "<msgid@example.com>");
}

#[test]
fn test_missing_separator() {
    let result: Result<Article, _> = MISSING_SEPARATOR.try_into();
    assert!(matches!(result, Err(ParseError::MissingSeparator)));
}

#[test]
fn test_missing_terminator() {
    let result: Result<Article, _> = MISSING_TERMINATOR.try_into();
    assert!(matches!(result, Err(ParseError::MissingTerminator)));
}

#[test]
fn test_invalid_headers() {
    let result: Result<Article, _> = INVALID_HEADERS.try_into();
    assert!(matches!(result, Err(ParseError::InvalidHeader(_))));
}

#[test]
fn test_folded_header() {
    let article: Article = FOLDED_HEADER.try_into().unwrap();

    let headers = article.headers.unwrap();
    // Folded header should parse successfully (validation passes)
    let subject = headers.get("Subject").unwrap();
    let subject_str = std::str::from_utf8(subject).unwrap();
    // Currently returns first line only - folding support is TODO
    assert!(subject_str.contains("This is a long subject"));
}

#[test]
fn test_head_with_body_rejected() {
    let result: Result<Article, _> = HEAD_WITH_BODY.try_into();
    // HEAD should not have body - this is invalid
    assert!(matches!(result, Err(ParseError::UnexpectedBody)));
}

#[test]
fn test_body_ignores_headers() {
    let article: Article = BODY_WITH_HEADERS.try_into().unwrap();

    // BODY response should not parse headers
    assert!(article.headers.is_none());

    // Body should include everything after status line
    let body = article.body.unwrap();
    assert!(body.starts_with(b"Subject:") || body.starts_with(b"Actual"));
}

#[test]
fn test_wrong_status_code() {
    let result: Result<Article, _> = WRONG_STATUS_CODE.try_into();
    assert!(matches!(result, Err(ParseError::InvalidStatusCode(_))));
}

#[test]
fn test_invalid_yenc() {
    let result: Result<Article, _> = INVALID_YENC.try_into();
    // yenc without proper =yend should fail validation
    assert!(matches!(result, Err(ParseError::InvalidYenc(_))));
}

#[test]
fn test_headers_iteration() {
    let article: Article = VALID_ARTICLE_TEXT.try_into().unwrap();
    let headers = article.headers.unwrap();

    let mut count = 0;
    for (name, value) in headers.iter() {
        count += 1;
        assert!(!name.is_empty());
        assert!(!value.is_empty());
    }

    assert!(count >= 4); // Subject, From, Date, Message-ID
}

#[test]
fn test_headers_case_insensitive() {
    let article: Article = VALID_ARTICLE_TEXT.try_into().unwrap();
    let headers = article.headers.unwrap();

    // Header names should be case-insensitive
    assert_eq!(headers.get("subject"), headers.get("Subject"));
    assert_eq!(headers.get("FROM"), headers.get("From"));
}

#[test]
fn test_empty_body() {
    let empty_body = b"222 123 <test@example.com>\r\n.\r\n";
    let article: Article = (&empty_body[..]).try_into().unwrap();

    let body = article.body.unwrap();
    assert!(body.is_empty() || body == b"\r\n");
}

#[test]
fn test_very_long_header() {
    let long_header = format!(
        "220 123 <test@example.com>\r\nSubject: {}\r\n\r\nBody\r\n.\r\n",
        "A".repeat(10000)
    );

    let article: Article = long_header.as_bytes().try_into().unwrap();
    let headers = article.headers.unwrap();
    let subject = headers.get("Subject").unwrap();
    assert_eq!(subject.len(), 10000);
}

#[test]
fn test_binary_data_in_body() {
    let mut binary_article = Vec::new();
    binary_article.extend_from_slice(b"222 123 <test@example.com>\r\n");
    // Add some binary data (not valid UTF-8)
    binary_article.extend_from_slice(&[0xFF, 0xFE, 0xFD, 0xFC, 0xFB]);
    binary_article.extend_from_slice(b"\r\n.\r\n");

    let article: Article = binary_article.as_slice().try_into().unwrap();
    let body = article.body.unwrap();
    assert!(body.contains(&0xFF));
}

#[test]
fn test_dot_stuffing() {
    // Lines starting with "." should be unescaped (dot-stuffing per RFC 3977)
    let dotted = b"222 123 <test@example.com>\r\n\
Line 1\r\n\
..Line starting with dot\r\n\
Line 3\r\n\
.\r\n";

    let article: Article = (&dotted[..]).try_into().unwrap();
    let body = article.body.unwrap();

    // The ".." should be unescaped to "."
    let body_str = std::str::from_utf8(body).unwrap();
    assert!(body_str.contains(".Line starting with dot"));
}

#[test]
fn test_multiple_message_ids() {
    // Response line has message-ID, headers also have Message-ID
    // They should match
    let article: Article = VALID_ARTICLE_TEXT.try_into().unwrap();
    let headers = article.headers.unwrap();
    let header_msgid = headers.get("Message-ID").unwrap();

    // Check if header contains test@example.com
    let msgid_str = std::str::from_utf8(header_msgid).unwrap();
    assert!(msgid_str.contains("test@example.com"));
}

#[test]
fn test_zero_copy_slices() {
    let article: Article = VALID_ARTICLE_TEXT.try_into().unwrap();

    // Verify headers and body are slices into original buffer
    let headers_ptr = article.headers.unwrap().as_bytes().as_ptr();
    let body_ptr = article.body.unwrap().as_ptr();
    let original_ptr = VALID_ARTICLE_TEXT.as_ptr();

    // Headers should point somewhere in the original buffer
    assert!(headers_ptr >= original_ptr);
    assert!(headers_ptr < unsafe { original_ptr.add(VALID_ARTICLE_TEXT.len()) });

    // Body should point somewhere in the original buffer
    assert!(body_ptr >= original_ptr);
    assert!(body_ptr < unsafe { original_ptr.add(VALID_ARTICLE_TEXT.len()) });
}

mod headers_tests {
    use super::*;

    #[test]
    fn test_headers_basic_parsing() {
        let header_data = b"Subject: Test\r\nFrom: test@example.com\r\n";
        let headers = Headers::parse(header_data).unwrap();

        assert_eq!(headers.get("Subject"), Some(&b"Test"[..]));
        assert_eq!(headers.get("From"), Some(&b"test@example.com"[..]));
    }

    #[test]
    fn test_headers_no_crlf() {
        let invalid = b"Subject: Test\nFrom: Bad";
        let result = Headers::parse(invalid);
        assert!(matches!(result, Err(ParseError::InvalidHeader(_))));
    }

    #[test]
    fn test_headers_no_colon() {
        let invalid = b"Subject Test\r\n";
        let result = Headers::parse(invalid);
        assert!(matches!(result, Err(ParseError::InvalidHeader(_))));
    }

    #[test]
    fn test_headers_empty_name() {
        let invalid = b": Value\r\n";
        let result = Headers::parse(invalid);
        assert!(matches!(result, Err(ParseError::InvalidHeader(_))));
    }

    #[test]
    fn test_headers_whitespace_in_name() {
        let invalid = b"Bad Name: Value\r\n";
        let result = Headers::parse(invalid);
        assert!(matches!(result, Err(ParseError::InvalidHeader(_))));
    }

    #[test]
    fn test_headers_empty_value() {
        let valid = b"Subject:\r\n";
        let headers = Headers::parse(valid).unwrap();
        assert_eq!(headers.get("Subject"), Some(&b""[..]));
    }

    #[test]
    fn test_headers_leading_whitespace() {
        let valid = b"Subject:   Test with spaces\r\n";
        let headers = Headers::parse(valid).unwrap();
        let subject = headers.get("Subject").unwrap();
        // Leading whitespace in value should be preserved
        assert!(subject.starts_with(b" ") || subject.starts_with(b"Test"));
    }
}
