//! NNTP response parsing and handling

/// Represents a parsed NNTP response
#[derive(Debug, Clone, PartialEq)]
pub struct NntpResponse {
    /// Status code (e.g., 200, 381, 500)
    pub status_code: u16,
    /// Whether this is a multiline response
    pub is_multiline: bool,
    /// Complete response data including status line
    pub data: Vec<u8>,
}

impl NntpResponse {
    /// Parse a status code from response data
    pub fn parse_status_code(data: &[u8]) -> Option<u16> {
        if data.len() < 3 {
            return None;
        }
        
        let code_str = std::str::from_utf8(&data[0..3]).ok()?;
        code_str.parse().ok()
    }
    
    /// Check if a response indicates a multiline response
    #[allow(dead_code)]
    pub fn is_multiline_response(status_code: u16) -> bool {
        // Multiline responses in NNTP typically have codes like:
        // 1xx informational (multiline)
        // 2xx success with data (some multiline: 215, 220, 221, 222, 224, 225, 230, 231, 282)
        // 4xx/5xx errors are single line
        match status_code {
            100..=199 => true,  // Informational multiline
            215 | 220 | 221 | 222 | 224 | 225 | 230 | 231 | 282 => true, // Article/list data
            _ => false,
        }
    }
    
    /// Check if data contains the end-of-multiline marker
    #[allow(dead_code)]
    pub fn has_multiline_terminator(data: &[u8]) -> bool {
        // NNTP multiline responses end with "\r\n.\r\n"
        if data.len() < 5 {
            return false;
        }
        
        // Look for the terminator at the end
        data.ends_with(b"\r\n.\r\n") || data.ends_with(b"\n.\r\n")
    }
}

/// Response parser for NNTP protocol
pub struct ResponseParser;

impl ResponseParser {
    /// Check if a response starts with a success code
    #[allow(dead_code)]
    pub fn is_success_response(data: &[u8]) -> bool {
        if let Some(code) = NntpResponse::parse_status_code(data) {
            code >= 200 && code < 400
        } else {
            false
        }
    }
    
    /// Check if response is a greeting (200 or 201)
    pub fn is_greeting(data: &[u8]) -> bool {
        let response_str = String::from_utf8_lossy(data);
        response_str.starts_with("200") || response_str.starts_with("201")
    }
    
    /// Check if response indicates authentication is required
    pub fn is_auth_required(data: &[u8]) -> bool {
        if let Some(code) = NntpResponse::parse_status_code(data) {
            code == 381 || code == 480
        } else {
            false
        }
    }
    
    /// Check if response indicates successful authentication
    pub fn is_auth_success(data: &[u8]) -> bool {
        if let Some(code) = NntpResponse::parse_status_code(data) {
            code == 281
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_status_code() {
        assert_eq!(NntpResponse::parse_status_code(b"200 Ready\r\n"), Some(200));
        assert_eq!(NntpResponse::parse_status_code(b"381 Password required\r\n"), Some(381));
        assert_eq!(NntpResponse::parse_status_code(b"500 Error\r\n"), Some(500));
        assert_eq!(NntpResponse::parse_status_code(b"XX"), None);
        assert_eq!(NntpResponse::parse_status_code(b""), None);
    }

    #[test]
    fn test_is_multiline_response() {
        assert!(NntpResponse::is_multiline_response(100));
        assert!(NntpResponse::is_multiline_response(215)); // LIST
        assert!(NntpResponse::is_multiline_response(220)); // ARTICLE
        assert!(NntpResponse::is_multiline_response(221)); // HEAD
        assert!(NntpResponse::is_multiline_response(222)); // BODY
        assert!(!NntpResponse::is_multiline_response(200)); // Greeting
        assert!(!NntpResponse::is_multiline_response(381)); // Auth required
        assert!(!NntpResponse::is_multiline_response(500)); // Error
    }

    #[test]
    fn test_has_multiline_terminator() {
        assert!(NntpResponse::has_multiline_terminator(b"data\r\nmore data\r\n.\r\n"));
        assert!(NntpResponse::has_multiline_terminator(b"single line\n.\r\n"));
        assert!(!NntpResponse::has_multiline_terminator(b"incomplete\r\n"));
        assert!(!NntpResponse::has_multiline_terminator(b".\r\n")); // Too short
    }

    #[test]
    fn test_is_success_response() {
        assert!(ResponseParser::is_success_response(b"200 Ready\r\n"));
        assert!(ResponseParser::is_success_response(b"281 Auth OK\r\n"));
        assert!(!ResponseParser::is_success_response(b"400 Error\r\n"));
        assert!(!ResponseParser::is_success_response(b"500 Error\r\n"));
    }

    #[test]
    fn test_is_greeting() {
        assert!(ResponseParser::is_greeting(b"200 news.example.com ready\r\n"));
        assert!(ResponseParser::is_greeting(b"201 read-only\r\n"));
        assert!(!ResponseParser::is_greeting(b"381 Password required\r\n"));
    }

    #[test]
    fn test_auth_responses() {
        assert!(ResponseParser::is_auth_required(b"381 Password required\r\n"));
        assert!(ResponseParser::is_auth_required(b"480 Auth required\r\n"));
        assert!(!ResponseParser::is_auth_required(b"200 Ready\r\n"));
        
        assert!(ResponseParser::is_auth_success(b"281 Auth accepted\r\n"));
        assert!(!ResponseParser::is_auth_success(b"381 Password required\r\n"));
    }
}
