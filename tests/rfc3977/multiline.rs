//! RFC 3977 Section 3.1.1 - Multi-line Response and Byte-Stuffing Tests.

use nntp_proxy::protocol::CRLF;
use nntp_proxy::session::streaming::tail_buffer::TailBuffer;

fn unstuff_line(line: &str) -> &str {
    if line.starts_with("..") {
        &line[1..]
    } else {
        line
    }
}

fn parse_multiline_body(lines: &[&str]) -> Vec<String> {
    lines
        .iter()
        .take_while(|line| **line != ".")
        .map(|line| unstuff_line(line).to_string())
        .collect()
}

fn parse_raw_multiline_response(raw: &str) -> Vec<String> {
    let lines = raw
        .split("\r\n")
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    parse_multiline_body(&lines)
}

#[test]
fn test_dot_stuffing_rules() {
    [
        (".", "."),
        ("..", "."),
        ("...", ".."),
        ("....", "..."),
        ("..Hello", ".Hello"),
        ("..test..", ".test.."),
        ("Hello World", "Hello World"),
        ("", ""),
        ("Normal line", "Normal line"),
        ("Hello.World.Test", "Hello.World.Test"),
        ("End with dot.", "End with dot."),
        (" ..test", " ..test"),
        ("Hello\0World", "Hello\0World"),
    ]
    .into_iter()
    .for_each(|(line, expected)| assert_eq!(unstuff_line(line), expected));
}

#[test]
fn test_multiline_body_parsing() {
    [
        (
            ["Line 1", "Line 2", "Line 3", "."].as_slice(),
            vec!["Line 1", "Line 2", "Line 3"],
        ),
        (
            ["Normal line", "..This started with a dot", "...", "."].as_slice(),
            vec!["Normal line", ".This started with a dot", ".."],
        ),
        (
            ["First", "", "Third", "", "."].as_slice(),
            vec!["First", "", "Third", ""],
        ),
        (["."].as_slice(), vec![]),
        (["Content", "..", "."].as_slice(), vec!["Content", "."]),
        (
            ["..", "...", "....", "."].as_slice(),
            vec![".", "..", "..."],
        ),
        (["Data", "."].as_slice(), vec!["Data"]),
    ]
    .into_iter()
    .for_each(|(lines, expected)| assert_eq!(parse_multiline_body(lines), expected));
}

#[test]
fn test_multiline_article_and_xover_shapes() {
    let article = parse_multiline_body(&[
        "From: user@example.com",
        "Subject: Test Article",
        "Date: Mon, 1 Jan 2024 00:00:00 +0000",
        "Message-ID: <test@example.com>",
        "",
        "This is the body of the article.",
        "It has multiple lines.",
        "",
        "..Hidden dot line",
        ".",
    ]);
    assert_eq!(article.len(), 9);
    assert_eq!(article[0], "From: user@example.com");
    assert_eq!(article[4], "");
    assert_eq!(article[8], ".Hidden dot line");

    let xover = parse_multiline_body(&[
        "12345\tSubject\tauthor@example.com\tDate\t<msgid>\trefs\t1000\t50",
        "12346\tAnother\tother@example.com\tDate2\t<msgid2>\trefs2\t2000\t100",
        ".",
    ]);
    assert_eq!(xover.len(), 2);
    assert!(xover[0].starts_with("12345\t"));
    assert!(xover[1].starts_with("12346\t"));
}

#[test]
fn test_raw_crlf_multiline_parsing() {
    [
        ("Line 1\r\nLine 2\r\n.\r\n", vec!["Line 1", "Line 2"]),
        (
            "Normal\r\n..Dot-stuffed\r\n.\r\n",
            vec!["Normal", ".Dot-stuffed"],
        ),
        (".\r\n", vec![]),
    ]
    .into_iter()
    .for_each(|(raw, expected)| assert_eq!(parse_raw_multiline_response(raw), expected));
}

#[test]
fn test_crlf_article_separator_and_terminator_format() {
    let raw = "From: user@example.com\r\nSubject: Test\r\n\r\nBody text.\r\n.\r\n";
    let lines = raw.split("\r\n").collect::<Vec<_>>();

    assert_eq!(lines[0], "From: user@example.com");
    assert_eq!(lines[1], "Subject: Test");
    assert_eq!(lines[2], "");
    assert_eq!(lines[3], "Body text.");
    assert_eq!(lines[4], ".");
    assert!("Content\r\n.\r\n".ends_with(".\r\n"));
}

#[test]
fn test_bare_and_mixed_line_endings_are_not_crlf_sequences() {
    [("Line1\nLine2\n.\n", '\n'), ("Line1\rLine2\r.\r", '\r')]
        .into_iter()
        .for_each(|(raw, separator)| {
            assert_eq!(raw.split("\r\n").filter(|s| !s.is_empty()).count(), 1);
            assert_eq!(raw.split(separator).filter(|s| !s.is_empty()).count(), 3);
        });

    let mixed = "Line1\r\nLine2\nLine3\r\n.\r\n";
    let crlf_lines = mixed
        .split("\r\n")
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    assert!(crlf_lines.iter().any(|line| line.contains("Line2\nLine3")));
}

#[test]
fn test_protocol_constants() {
    assert_eq!(b"\r\n.\r\n".len(), 5);
    assert_eq!(CRLF, b"\r\n");
    assert_eq!(CRLF.len(), 2);
}

#[test]
fn test_tail_buffer_terminator_detection() {
    [
        (
            b"220 Article follows\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".as_slice(),
            true,
        ),
        (
            b"220 Article follows\r\nSubject: Test\r\n".as_slice(),
            false,
        ),
        (
            b"222 Body\r\n..Content starting with dot\r\n.\r\n".as_slice(),
            true,
        ),
    ]
    .into_iter()
    .for_each(|(response, found)| {
        assert_eq!(
            TailBuffer::default().detect_terminator(response).is_found(),
            found
        );
    });

    let response = b"222 Body\r\n..Content starting with dot\r\n.\r\n";
    let body_start = b"222 Body\r\n".len();
    assert_eq!(&response[body_start..body_start + 2], b"..");
}
