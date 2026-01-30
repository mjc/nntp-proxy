//! NNTP status code constants per RFC 3977 and RFC 4643
//!
//! Provides named constants for all standard NNTP response codes,
//! organized by category (informational, success, continuation, error).

// 1xx - Informational (RFC 3977 §3.2.1.1)

/// Help text follows (RFC 3977 §7.2)
pub const HELP_TEXT: u16 = 100;
/// Capability list follows (RFC 3977 §5.2)
pub const CAPABILITY_LIST: u16 = 101;
/// Server date and time (RFC 3977 §7.1)
pub const SERVER_DATE: u16 = 111;

// 2xx - Success (RFC 3977 §3.2.1.2)

/// Server ready, posting allowed (RFC 3977 §5.1.1)
pub const POSTING_ALLOWED: u16 = 200;
/// Server ready, no posting (RFC 3977 §5.1.1)
pub const NO_POSTING: u16 = 201;
/// Connection closing (RFC 3977 §5.4)
pub const CONNECTION_CLOSING: u16 = 205;
/// Group selected (RFC 3977 §6.1.1)
pub const GROUP_SELECTED: u16 = 211;
/// Information follows (RFC 3977 §7.6.1)
pub const INFORMATION_FOLLOWS: u16 = 215;
/// Article follows (RFC 3977 §6.2.1)
pub const ARTICLE_FOLLOWS: u16 = 220;
/// Head follows (RFC 3977 §6.2.2)
pub const HEAD_FOLLOWS: u16 = 221;
/// Body follows (RFC 3977 §6.2.3)
pub const BODY_FOLLOWS: u16 = 222;
/// Article exists (RFC 3977 §6.2.4)
pub const ARTICLE_EXISTS: u16 = 223;
/// Overview follows (RFC 3977 §8.3)
pub const OVERVIEW_FOLLOWS: u16 = 224;
/// Headers follow (RFC 3977 §8.5)
pub const HEADERS_FOLLOW: u16 = 225;
/// New articles follow (RFC 3977 §7.4)
pub const NEW_ARTICLES_FOLLOW: u16 = 230;
/// New groups follow (RFC 3977 §7.3)
pub const NEW_GROUPS_FOLLOW: u16 = 231;
/// Authentication accepted (RFC 4643 §2.3)
pub const AUTH_ACCEPTED: u16 = 281;
/// Compression active (RFC 8054 §2.2)
pub const COMPRESSION_ACTIVE: u16 = 206;

// 3xx - Continuation (RFC 3977 §3.2.1.3)

/// Send article to be transferred (RFC 3977 §6.3.1)
pub const SEND_ARTICLE_TRANSFER: u16 = 335;
/// Send article to be posted (RFC 3977 §6.3.2)
pub const SEND_ARTICLE_POST: u16 = 340;
/// Password required (RFC 4643 §2.3)
pub const PASSWORD_REQUIRED: u16 = 381;
/// SASL continue (RFC 4643 §2.4)
pub const SASL_CONTINUE: u16 = 383;

// 4xx - Temporary errors (RFC 3977 §3.2.1.4)

/// Service temporarily unavailable (RFC 3977 §3.2.1)
pub const SERVICE_UNAVAILABLE: u16 = 400;
/// Internal fault (RFC 3977 §3.2.1)
pub const INTERNAL_FAULT: u16 = 403;
/// No such newsgroup (RFC 3977 §6.1.1)
pub const NO_SUCH_GROUP: u16 = 411;
/// No newsgroup selected (RFC 3977 §6.1.1)
pub const NO_GROUP_SELECTED: u16 = 412;
/// No current article selected (RFC 3977 §6.2.4)
pub const NO_CURRENT_ARTICLE: u16 = 420;
/// No next article (RFC 3977 §6.3.1)
pub const NO_NEXT_ARTICLE: u16 = 421;
/// No previous article (RFC 3977 §6.3.2)
pub const NO_PREV_ARTICLE: u16 = 422;
/// No article with that number (RFC 3977 §6.2.1)
pub const NO_SUCH_ARTICLE_NUMBER: u16 = 423;
/// No article with that message-id (RFC 3977 §6.2.1)
pub const NO_SUCH_ARTICLE_ID: u16 = 430;
/// Authentication required (RFC 4643 §2.3)
pub const AUTH_REQUIRED: u16 = 480;
/// Authentication rejected (RFC 4643 §2.3)
pub const AUTH_REJECTED: u16 = 481;
/// Authentication out of sequence (RFC 4643 §2.3)
pub const AUTH_OUT_OF_SEQUENCE: u16 = 482;
/// Encryption required (RFC 4643 §2.3)
pub const ENCRYPTION_REQUIRED: u16 = 483;

// 5xx - Permanent errors (RFC 3977 §3.2.1.5)

/// Command not recognized (RFC 3977 §3.2.1)
pub const COMMAND_NOT_RECOGNIZED: u16 = 500;
/// Command syntax error (RFC 3977 §3.2.1)
pub const COMMAND_SYNTAX_ERROR: u16 = 501;
/// Access denied / no permission (RFC 3977 §3.2.1)
pub const ACCESS_DENIED: u16 = 502;
/// Feature not supported (RFC 3977 §3.2.1)
pub const FEATURE_NOT_SUPPORTED: u16 = 503;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_1xx_informational_range() {
        assert!((100..200).contains(&HELP_TEXT));
        assert!((100..200).contains(&CAPABILITY_LIST));
        assert!((100..200).contains(&SERVER_DATE));
    }

    #[test]
    fn test_2xx_success_range() {
        let codes = [
            POSTING_ALLOWED,
            NO_POSTING,
            CONNECTION_CLOSING,
            GROUP_SELECTED,
            INFORMATION_FOLLOWS,
            ARTICLE_FOLLOWS,
            HEAD_FOLLOWS,
            BODY_FOLLOWS,
            ARTICLE_EXISTS,
            OVERVIEW_FOLLOWS,
            HEADERS_FOLLOW,
            NEW_ARTICLES_FOLLOW,
            NEW_GROUPS_FOLLOW,
            AUTH_ACCEPTED,
            COMPRESSION_ACTIVE,
        ];
        for code in codes {
            assert!((200..300).contains(&code), "Code {} should be 2xx", code);
        }
    }

    #[test]
    fn test_3xx_continuation_range() {
        let codes = [
            SEND_ARTICLE_TRANSFER,
            SEND_ARTICLE_POST,
            PASSWORD_REQUIRED,
            SASL_CONTINUE,
        ];
        for code in codes {
            assert!((300..400).contains(&code), "Code {} should be 3xx", code);
        }
    }

    #[test]
    fn test_4xx_temporary_error_range() {
        let codes = [
            SERVICE_UNAVAILABLE,
            INTERNAL_FAULT,
            NO_SUCH_GROUP,
            NO_GROUP_SELECTED,
            NO_CURRENT_ARTICLE,
            NO_NEXT_ARTICLE,
            NO_PREV_ARTICLE,
            NO_SUCH_ARTICLE_NUMBER,
            NO_SUCH_ARTICLE_ID,
            AUTH_REQUIRED,
            AUTH_REJECTED,
            AUTH_OUT_OF_SEQUENCE,
            ENCRYPTION_REQUIRED,
        ];
        for code in codes {
            assert!((400..500).contains(&code), "Code {} should be 4xx", code);
        }
    }

    #[test]
    fn test_5xx_permanent_error_range() {
        let codes = [
            COMMAND_NOT_RECOGNIZED,
            COMMAND_SYNTAX_ERROR,
            ACCESS_DENIED,
            FEATURE_NOT_SUPPORTED,
        ];
        for code in codes {
            assert!((500..600).contains(&code), "Code {} should be 5xx", code);
        }
    }

    #[test]
    fn test_specific_code_values() {
        // 4xx codes
        assert_eq!(SERVICE_UNAVAILABLE, 400);
        assert_eq!(INTERNAL_FAULT, 403);
        assert_eq!(NO_SUCH_GROUP, 411);
        assert_eq!(NO_GROUP_SELECTED, 412);
        assert_eq!(NO_CURRENT_ARTICLE, 420);
        assert_eq!(NO_NEXT_ARTICLE, 421);
        assert_eq!(NO_PREV_ARTICLE, 422);
        assert_eq!(NO_SUCH_ARTICLE_NUMBER, 423);
        assert_eq!(NO_SUCH_ARTICLE_ID, 430);
        assert_eq!(AUTH_REQUIRED, 480);
        assert_eq!(AUTH_REJECTED, 481);
        assert_eq!(AUTH_OUT_OF_SEQUENCE, 482);
        assert_eq!(ENCRYPTION_REQUIRED, 483);

        // 5xx codes
        assert_eq!(COMMAND_NOT_RECOGNIZED, 500);
        assert_eq!(COMMAND_SYNTAX_ERROR, 501);
        assert_eq!(ACCESS_DENIED, 502);
        assert_eq!(FEATURE_NOT_SUPPORTED, 503);
    }
}
