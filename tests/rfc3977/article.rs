//! Tests for Article parsing and validation.

use nntp_proxy::protocol::{Article, Headers, ParseError};

const VALID_ARTICLE_TEXT: &[u8] = b"220 12345 <test@example.com>\r\n\
Subject: Test Article\r\n\
From: test@example.com\r\n\
Date: Sat, 30 Nov 2024 12:00:00 +0000\r\n\
Message-ID: <test@example.com>\r\n\
\r\n\
This is the article body.\r\n\
Multiple lines of text.\r\n\
.\r\n";

const VALID_ARTICLE_YENC: &[u8] = b"220 54321 <binary@example.com>\r\n\
Subject: test.txt (1/1)\r\n\
From: poster@example.com\r\n\
Message-ID: <binary@example.com>\r\n\
\r\n\
=ybegin line=128 size=12 name=test.txt\r\n\
r\x8f\x96\x96\x99VJ\xa3o\x98\x8dK\r\n\
=yend size=12 crc32=0337ab3d\r\n\
.\r\n";

const VALID_HEAD: &[u8] = b"221 12345 <test@example.com>\r\n\
Subject: Test Article\r\n\
From: test@example.com\r\n\
Date: Sat, 30 Nov 2024 12:00:00 +0000\r\n\
Message-ID: <test@example.com>\r\n\
.\r\n";

const VALID_BODY: &[u8] = b"222 12345 <test@example.com>\r\n\
This is the article body.\r\n\
Multiple lines of text.\r\n\
.\r\n";

const VALID_STAT: &[u8] = b"223 12345 <test@example.com>\r\n";

const ARTICLE_BY_MSGID: &[u8] = b"220 0 <msgid@example.com>\r\n\
Subject: Retrieved by message-ID\r\n\
\r\n\
Body\r\n\
.\r\n";

const BODY_WITH_HEADERS: &[u8] = b"222 12345 <test@example.com>\r\n\
Subject: Should be ignored\r\n\
\r\n\
Actual body content\r\n\
.\r\n";

const FOLDED_HEADER: &[u8] = concat!(
    "220 12345 <test@example.com>\r\n",
    "Subject: This is a long subject\r\n",
    " that continues on the next line\r\n",
    "From: test@example.com\r\n",
    "\r\n",
    "Body\r\n",
    ".\r\n"
)
.as_bytes();

fn article(input: &[u8]) -> Article<'_> {
    input.try_into().unwrap()
}

fn assert_article_shape(
    input: &[u8],
    article_number: Option<u64>,
    message_id: &str,
    has_headers: bool,
    has_body: bool,
) {
    let article = article(input);
    assert_eq!(article.article_number, article_number);
    assert_eq!(article.message_id.as_str(), message_id);
    assert_eq!(article.headers.is_some(), has_headers);
    assert_eq!(article.body.is_some(), has_body);
}

#[test]
fn test_parse_valid_article_shapes() {
    for (input, article_number, message_id, has_headers, has_body) in [
        (
            VALID_ARTICLE_TEXT,
            Some(12345),
            "<test@example.com>",
            true,
            true,
        ),
        (
            VALID_ARTICLE_YENC,
            Some(54321),
            "<binary@example.com>",
            true,
            true,
        ),
        (VALID_HEAD, Some(12345), "<test@example.com>", true, false),
        (VALID_BODY, Some(12345), "<test@example.com>", false, true),
        (VALID_STAT, Some(12345), "<test@example.com>", false, false),
        (ARTICLE_BY_MSGID, Some(0), "<msgid@example.com>", true, true),
    ] {
        assert_article_shape(input, article_number, message_id, has_headers, has_body);
    }
}

#[test]
fn test_article_content_accessors() {
    let parsed = article(VALID_ARTICLE_TEXT);
    let headers = parsed.headers.unwrap();
    assert_eq!(headers.get("Subject"), Some(&b"Test Article"[..]));
    assert_eq!(headers.get("From"), Some(&b"test@example.com"[..]));
    assert_eq!(headers.get("subject"), headers.get("Subject"));
    assert_eq!(headers.get("FROM"), headers.get("From"));
    assert!(
        parsed
            .body
            .unwrap()
            .starts_with(b"This is the article body.")
    );

    let body_article = article(VALID_BODY);
    assert!(
        body_article
            .body
            .unwrap()
            .starts_with(b"This is the article body.")
    );

    let yenc = article(VALID_ARTICLE_YENC).body.unwrap();
    assert!(yenc.starts_with(b"=ybegin"));
    assert!(yenc.windows(5).any(|window| window == b"=yend"));
}

#[test]
fn test_parse_error_cases() {
    for (input, expected) in [
        (
            b"220 12345 <test@example.com>\r\nSubject: Bad\r\nBody without separator\r\n.\r\n"
                .as_slice(),
            "missing separator",
        ),
        (
            b"220 12345 <test@example.com>\r\nSubject: Test\r\n\r\nBody without terminator\r\n"
                .as_slice(),
            "missing terminator",
        ),
        (
            b"220 12345 <test@example.com>\r\nSubject: Valid\r\nInvalidHeaderNoColon\r\n\r\nBody\r\n.\r\n"
                .as_slice(),
            "invalid header",
        ),
        (
            b"221 12345 <test@example.com>\r\nSubject: Test\r\n\r\nThis body should not be here\r\n.\r\n"
                .as_slice(),
            "unexpected body",
        ),
        (b"430 No such article\r\n".as_slice(), "invalid status"),
        (
            b"220 12345 <test@example.com>\r\nSubject: Binary\r\n\r\n=ybegin line=128 size=12 name=test.bin\r\nr\x8f\x96\x96\x99VJ\xa3o\x98\x8dK\r\n.\r\n"
                .as_slice(),
            "invalid yenc",
        ),
    ] {
        let result: Result<Article<'_>, _> = input.try_into();
        match (result, expected) {
            (Err(ParseError::MissingSeparator), "missing separator")
            | (Err(ParseError::MissingTerminator), "missing terminator")
            | (Err(ParseError::InvalidHeader(_)), "invalid header")
            | (Err(ParseError::UnexpectedBody), "unexpected body")
            | (Err(ParseError::InvalidStatusCode(_)), "invalid status")
            | (Err(ParseError::InvalidYenc(_)), "invalid yenc") => {}
            (other, expected) => panic!("expected {expected}, got {other:?}"),
        }
    }
}

#[test]
fn test_folded_header_and_body_with_headers() {
    let folded = article(FOLDED_HEADER);
    let subject = folded.headers.unwrap().get("Subject").unwrap();
    assert!(
        std::str::from_utf8(subject)
            .unwrap()
            .contains("This is a long subject")
    );

    let body_with_headers = article(BODY_WITH_HEADERS);
    assert!(body_with_headers.headers.is_none());
    let body = body_with_headers.body.unwrap();
    assert!(body.starts_with(b"Subject:") || body.starts_with(b"Actual"));
}

#[test]
fn test_headers_iteration_and_message_id_header() {
    let article = article(VALID_ARTICLE_TEXT);
    let headers = article.headers.unwrap();
    assert!(
        headers
            .iter()
            .inspect(|(name, value)| {
                assert!(!name.is_empty());
                assert!(!value.is_empty());
            })
            .count()
            >= 4
    );
    assert!(
        std::str::from_utf8(headers.get("Message-ID").unwrap())
            .unwrap()
            .contains("test@example.com")
    );
}

#[test]
fn test_body_edge_cases() {
    let empty_body = article(b"222 123 <test@example.com>\r\n.\r\n");
    let body = empty_body.body.unwrap();
    assert!(body.is_empty() || body == b"\r\n");

    let mut binary_article = b"222 123 <test@example.com>\r\n".to_vec();
    binary_article.extend_from_slice(&[0xFF, 0xFE, 0xFD, 0xFC, 0xFB]);
    binary_article.extend_from_slice(b"\r\n.\r\n");
    assert!(article(&binary_article).body.unwrap().contains(&0xFF));

    let dotted = b"222 123 <test@example.com>\r\n\
Line 1\r\n\
..Line starting with dot\r\n\
Line 3\r\n\
.\r\n";
    let body = article(dotted).body.unwrap();
    assert!(
        std::str::from_utf8(body)
            .unwrap()
            .contains(".Line starting with dot")
    );
}

#[test]
fn test_very_long_header() {
    let long_header = format!(
        "220 123 <test@example.com>\r\nSubject: {}\r\n\r\nBody\r\n.\r\n",
        "A".repeat(10000)
    );

    let article = article(long_header.as_bytes());
    assert_eq!(
        article.headers.unwrap().get("Subject").unwrap().len(),
        10000
    );
}

#[test]
fn test_zero_copy_slices() {
    let article = article(VALID_ARTICLE_TEXT);
    let headers_ptr = article.headers.unwrap().as_bytes().as_ptr();
    let body_ptr = article.body.unwrap().as_ptr();
    let original_ptr = VALID_ARTICLE_TEXT.as_ptr();
    let original_end = unsafe { original_ptr.add(VALID_ARTICLE_TEXT.len()) };

    assert!(headers_ptr >= original_ptr);
    assert!(headers_ptr < original_end);
    assert!(body_ptr >= original_ptr);
    assert!(body_ptr < original_end);
}

mod headers_tests {
    use super::*;

    #[test]
    fn test_headers_basic_parsing() {
        let headers = Headers::parse(b"Subject: Test\r\nFrom: test@example.com\r\n").unwrap();
        assert_eq!(headers.get("Subject"), Some(&b"Test"[..]));
        assert_eq!(headers.get("From"), Some(&b"test@example.com"[..]));
    }

    #[test]
    fn test_headers_invalid_inputs() {
        for input in [
            b"Subject: Test\nFrom: Bad".as_slice(),
            b"Subject Test\r\n".as_slice(),
            b": Value\r\n".as_slice(),
            b"Bad Name: Value\r\n".as_slice(),
        ] {
            assert!(matches!(
                Headers::parse(input),
                Err(ParseError::InvalidHeader(_))
            ));
        }
    }

    #[test]
    fn test_headers_value_edge_cases() {
        let empty = Headers::parse(b"Subject:\r\n").unwrap();
        assert_eq!(empty.get("Subject"), Some(&b""[..]));

        let spaced = Headers::parse(b"Subject:   Test with spaces\r\n").unwrap();
        let subject = spaced.get("Subject").unwrap();
        assert!(subject.starts_with(b" ") || subject.starts_with(b"Test"));
    }
}
