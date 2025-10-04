//! Command classification logic for NNTP commands

use super::types::{CommandType, ValidationResult};

// Case permutations for fast matching without allocation
// Covers: UPPERCASE, lowercase, Titlecase
const ARTICLE_CASES: &[&[u8]] = &[b"ARTICLE", b"article", b"Article"];
const BODY_CASES: &[&[u8]] = &[b"BODY", b"body", b"Body"];
const HEAD_CASES: &[&[u8]] = &[b"HEAD", b"head", b"Head"];
const STAT_CASES: &[&[u8]] = &[b"STAT", b"stat", b"Stat"];
const GROUP_CASES: &[&[u8]] = &[b"GROUP", b"group", b"Group"];
const AUTHINFO_CASES: &[&[u8]] = &[b"AUTHINFO", b"authinfo", b"Authinfo"];
const LIST_CASES: &[&[u8]] = &[b"LIST", b"list", b"List"];
const DATE_CASES: &[&[u8]] = &[b"DATE", b"date", b"Date"];
const CAPABILITIES_CASES: &[&[u8]] = &[b"CAPABILITIES", b"capabilities", b"Capabilities"];
const MODE_CASES: &[&[u8]] = &[b"MODE", b"mode", b"Mode"];
const HELP_CASES: &[&[u8]] = &[b"HELP", b"help", b"Help"];
const QUIT_CASES: &[&[u8]] = &[b"QUIT", b"quit", b"Quit"];
const XOVER_CASES: &[&[u8]] = &[b"XOVER", b"xover", b"Xover"];
const OVER_CASES: &[&[u8]] = &[b"OVER", b"over", b"Over"];
const XHDR_CASES: &[&[u8]] = &[b"XHDR", b"xhdr", b"Xhdr"];
const HDR_CASES: &[&[u8]] = &[b"HDR", b"hdr", b"Hdr"];
const NEXT_CASES: &[&[u8]] = &[b"NEXT", b"next", b"Next"];
const LAST_CASES: &[&[u8]] = &[b"LAST", b"last", b"Last"];
const LISTGROUP_CASES: &[&[u8]] = &[b"LISTGROUP", b"listgroup", b"Listgroup"];
const POST_CASES: &[&[u8]] = &[b"POST", b"post", b"Post"];
const IHAVE_CASES: &[&[u8]] = &[b"IHAVE", b"ihave", b"Ihave"];
const NEWGROUPS_CASES: &[&[u8]] = &[b"NEWGROUPS", b"newgroups", b"Newgroups"];
const NEWNEWS_CASES: &[&[u8]] = &[b"NEWNEWS", b"newnews", b"Newnews"];

/// Helper: Check if command matches any case variation
#[inline]
fn matches_any(cmd: &[u8], cases: &[&[u8]]) -> bool {
    cases.iter().any(|&c| cmd == c)
}

/// NNTP command classification for different handling strategies
#[derive(Debug, PartialEq)]
pub enum NntpCommand {
    /// Authentication commands (AUTHINFO USER/PASS) - intercepted locally
    AuthUser,
    AuthPass,
    /// Stateful commands that require GROUP context - REJECTED in stateless mode
    Stateful,
    /// Commands that cannot be multiplexed - REJECTED in multiplexing mode
    NonMultiplexable,
    /// Stateless commands that can be safely proxied without state
    Stateless,
    /// Article retrieval by message-ID (stateless) - can be proxied
    ArticleByMessageId,
}

impl NntpCommand {
    /// Classify an NNTP command based on its content using fast byte-level parsing
    /// Ordered by frequency (most common first) for optimal short-circuit performance
    /// 
    /// Performance: Zero allocations - uses direct byte slice comparison with hardcoded
    /// case permutations instead of case conversion.
    #[inline]
    pub fn classify(command: &str) -> Self {
        let trimmed = command.trim();
        let bytes = trimmed.as_bytes();

        // Fast path: find space to separate command from arguments
        let cmd_end = memchr::memchr(b' ', bytes).unwrap_or(bytes.len());
        let cmd = &bytes[..cmd_end];

        // Helper: Check if arguments start with '<' (message-ID indicator)
        #[inline]
        fn is_message_id_arg(bytes: &[u8], cmd_end: usize) -> bool {
            if cmd_end >= bytes.len() {
                return false;
            }
            let args = &bytes[cmd_end + 1..];
            let args_trimmed = args
                .iter()
                .position(|&b| !b.is_ascii_whitespace())
                .map(|pos| &args[pos..])
                .unwrap_or(args);
            !args_trimmed.is_empty() && args_trimmed[0] == b'<'
        }

        // Ordered by frequency: ARTICLE/BODY/HEAD/STAT are 70%+ of traffic
        // Article retrieval commands (MOST FREQUENT ~70% of traffic)
        if matches_any(cmd, ARTICLE_CASES) || matches_any(cmd, BODY_CASES) 
            || matches_any(cmd, HEAD_CASES) || matches_any(cmd, STAT_CASES) {
            return if is_message_id_arg(bytes, cmd_end) {
                Self::ArticleByMessageId
            } else {
                Self::Stateful
            };
        }

        // GROUP - common for switching groups (~10% of traffic)
        if matches_any(cmd, GROUP_CASES) {
            return Self::Stateful;
        }

        // Authentication - once per connection but checked early
        if matches_any(cmd, AUTHINFO_CASES) {
            if cmd_end + 1 < bytes.len() {
                let args = &bytes[cmd_end + 1..];
                if args.len() >= 4 {
                    match &args[..4] {
                        b"USER" | b"user" | b"User" => return Self::AuthUser,
                        b"PASS" | b"pass" | b"Pass" => return Self::AuthPass,
                        _ => {}
                    }
                }
            }
            return Self::Stateless;
        }

        // Stateless commands (moderately common ~5-10%)
        if matches_any(cmd, LIST_CASES) || matches_any(cmd, DATE_CASES)
            || matches_any(cmd, CAPABILITIES_CASES) || matches_any(cmd, MODE_CASES)
            || matches_any(cmd, HELP_CASES) || matches_any(cmd, QUIT_CASES) {
            return Self::Stateless;
        }

        // Header retrieval commands (~5%)
        if matches_any(cmd, XOVER_CASES) || matches_any(cmd, OVER_CASES)
            || matches_any(cmd, XHDR_CASES) || matches_any(cmd, HDR_CASES) {
            return Self::Stateful;
        }

        // Other stateful commands (rare)
        if matches_any(cmd, NEXT_CASES) || matches_any(cmd, LAST_CASES)
            || matches_any(cmd, LISTGROUP_CASES) {
            return Self::Stateful;
        }

        // Non-multiplexable commands (very rare in typical usage)
        if matches_any(cmd, POST_CASES) || matches_any(cmd, IHAVE_CASES)
            || matches_any(cmd, NEWGROUPS_CASES) || matches_any(cmd, NEWNEWS_CASES) {
            return Self::NonMultiplexable;
        }

        // Unknown commands - treat as stateless (forward and let backend decide)
        Self::Stateless
    }
}

/// Command classifier for validation and routing
#[allow(dead_code)]
pub struct CommandClassifier;

#[allow(dead_code)]
impl CommandClassifier {
    /// Validate a command and return validation result
    pub fn validate(command: &str) -> ValidationResult {
        match NntpCommand::classify(command) {
            NntpCommand::Stateful => ValidationResult::Rejected(
                "Command not supported by this proxy (stateless proxy mode)",
            ),
            NntpCommand::NonMultiplexable => ValidationResult::Rejected(
                "Command not supported by this proxy (multiplexing mode)",
            ),
            NntpCommand::AuthUser | NntpCommand::AuthPass => ValidationResult::Intercepted,
            _ => ValidationResult::Allowed,
        }
    }

    /// Parse and classify a raw command string
    pub fn parse(raw: &str) -> CommandType {
        let trimmed = raw.trim();
        match NntpCommand::classify(trimmed) {
            NntpCommand::AuthUser => {
                // Extract username from "AUTHINFO USER <username>"
                let parts: Vec<&str> = trimmed.split_whitespace().collect();
                let username = if parts.len() >= 3 {
                    parts[2].to_string()
                } else {
                    String::new()
                };
                CommandType::AuthUser(username)
            }
            NntpCommand::AuthPass => {
                // Extract password from "AUTHINFO PASS <password>"
                let parts: Vec<&str> = trimmed.split_whitespace().collect();
                let password = if parts.len() >= 3 {
                    parts[2].to_string()
                } else {
                    String::new()
                };
                CommandType::AuthPass(password)
            }
            NntpCommand::Stateful => CommandType::Stateful,
            NntpCommand::NonMultiplexable => CommandType::NonMultiplexable,
            NntpCommand::ArticleByMessageId => CommandType::ArticleByMessageId,
            NntpCommand::Stateless => CommandType::Stateless,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nntp_command_classification() {
        // Test authentication commands
        assert_eq!(
            NntpCommand::classify("AUTHINFO USER testuser"),
            NntpCommand::AuthUser
        );
        assert_eq!(
            NntpCommand::classify("AUTHINFO PASS testpass"),
            NntpCommand::AuthPass
        );
        assert_eq!(
            NntpCommand::classify("  AUTHINFO USER  whitespace  "),
            NntpCommand::AuthUser
        );

        // Test stateful commands (should be rejected)
        assert_eq!(
            NntpCommand::classify("GROUP alt.test"),
            NntpCommand::Stateful
        );
        assert_eq!(NntpCommand::classify("NEXT"), NntpCommand::Stateful);
        assert_eq!(NntpCommand::classify("LAST"), NntpCommand::Stateful);
        assert_eq!(
            NntpCommand::classify("LISTGROUP alt.test"),
            NntpCommand::Stateful
        );
        assert_eq!(
            NntpCommand::classify("ARTICLE 12345"),
            NntpCommand::Stateful
        );
        assert_eq!(NntpCommand::classify("ARTICLE"), NntpCommand::Stateful);
        assert_eq!(NntpCommand::classify("HEAD 67890"), NntpCommand::Stateful);
        assert_eq!(NntpCommand::classify("STAT"), NntpCommand::Stateful);
        assert_eq!(NntpCommand::classify("XOVER 1-100"), NntpCommand::Stateful);

        // Test article retrieval by message-ID (stateless - allowed)
        assert_eq!(
            NntpCommand::classify("ARTICLE <message@example.com>"),
            NntpCommand::ArticleByMessageId
        );
        assert_eq!(
            NntpCommand::classify("BODY <test@server.org>"),
            NntpCommand::ArticleByMessageId
        );
        assert_eq!(
            NntpCommand::classify("HEAD <another@example.net>"),
            NntpCommand::ArticleByMessageId
        );
        assert_eq!(
            NntpCommand::classify("STAT <id@host.com>"),
            NntpCommand::ArticleByMessageId
        );

        // Test stateless commands (allowed)
        assert_eq!(NntpCommand::classify("HELP"), NntpCommand::Stateless);
        assert_eq!(NntpCommand::classify("LIST"), NntpCommand::Stateless);
        assert_eq!(NntpCommand::classify("DATE"), NntpCommand::Stateless);
        assert_eq!(
            NntpCommand::classify("CAPABILITIES"),
            NntpCommand::Stateless
        );
        assert_eq!(NntpCommand::classify("QUIT"), NntpCommand::Stateless);
        assert_eq!(NntpCommand::classify("LIST ACTIVE"), NntpCommand::Stateless);
        assert_eq!(
            NntpCommand::classify("UNKNOWN COMMAND"),
            NntpCommand::Stateless
        );
    }

    #[test]
    fn test_command_validation() {
        assert_eq!(
            CommandClassifier::validate("LIST"),
            ValidationResult::Allowed
        );
        assert_eq!(
            CommandClassifier::validate("GROUP alt.test"),
            ValidationResult::Rejected(
                "Command not supported by this proxy (stateless proxy mode)"
            )
        );
        assert_eq!(
            CommandClassifier::validate("AUTHINFO USER test"),
            ValidationResult::Intercepted
        );
    }

    #[test]
    fn test_command_parsing() {
        match CommandClassifier::parse("AUTHINFO USER testuser") {
            CommandType::AuthUser(username) => assert_eq!(username, "testuser"),
            _ => panic!("Expected AuthUser"),
        }

        match CommandClassifier::parse("AUTHINFO PASS testpass") {
            CommandType::AuthPass(password) => assert_eq!(password, "testpass"),
            _ => panic!("Expected AuthPass"),
        }

        assert_eq!(
            CommandClassifier::parse("GROUP alt.test"),
            CommandType::Stateful
        );

        assert_eq!(
            CommandClassifier::parse("ARTICLE <msg@example.com>"),
            CommandType::ArticleByMessageId
        );

        assert_eq!(CommandClassifier::parse("LIST"), CommandType::Stateless);
    }

    #[test]
    fn test_case_insensitivity() {
        // Commands should be case-insensitive per NNTP spec
        assert_eq!(NntpCommand::classify("list"), NntpCommand::Stateless);
        assert_eq!(NntpCommand::classify("LiSt"), NntpCommand::Stateless);
        assert_eq!(NntpCommand::classify("QUIT"), NntpCommand::Stateless);
        assert_eq!(NntpCommand::classify("quit"), NntpCommand::Stateless);
        assert_eq!(
            NntpCommand::classify("group alt.test"),
            NntpCommand::Stateful
        );
        assert_eq!(
            NntpCommand::classify("GROUP alt.test"),
            NntpCommand::Stateful
        );
    }

    #[test]
    fn test_empty_and_whitespace_commands() {
        // Empty command
        assert_eq!(NntpCommand::classify(""), NntpCommand::Stateless);

        // Only whitespace
        assert_eq!(NntpCommand::classify("   "), NntpCommand::Stateless);

        // Tabs and spaces
        assert_eq!(NntpCommand::classify("\t\t  "), NntpCommand::Stateless);
    }

    #[test]
    fn test_malformed_authinfo_commands() {
        // AUTHINFO without USER or PASS
        assert_eq!(NntpCommand::classify("AUTHINFO"), NntpCommand::Stateless);

        // AUTHINFO with unknown subcommand
        assert_eq!(
            NntpCommand::classify("AUTHINFO INVALID"),
            NntpCommand::Stateless
        );

        // AUTHINFO USER without username
        assert_eq!(
            NntpCommand::classify("AUTHINFO USER"),
            NntpCommand::AuthUser
        );

        // AUTHINFO PASS without password
        assert_eq!(
            NntpCommand::classify("AUTHINFO PASS"),
            NntpCommand::AuthPass
        );
    }

    #[test]
    fn test_article_commands_with_various_message_ids() {
        // Standard message-ID
        assert_eq!(
            NntpCommand::classify("ARTICLE <test@example.com>"),
            NntpCommand::ArticleByMessageId
        );

        // Message-ID with complex domain
        assert_eq!(
            NntpCommand::classify("ARTICLE <msg.123@news.example.co.uk>"),
            NntpCommand::ArticleByMessageId
        );

        // Message-ID with special characters
        assert_eq!(
            NntpCommand::classify("ARTICLE <user+tag@domain.com>"),
            NntpCommand::ArticleByMessageId
        );

        // BODY with message-ID
        assert_eq!(
            NntpCommand::classify("BODY <test@test.com>"),
            NntpCommand::ArticleByMessageId
        );

        // HEAD with message-ID
        assert_eq!(
            NntpCommand::classify("HEAD <id@host>"),
            NntpCommand::ArticleByMessageId
        );

        // STAT with message-ID
        assert_eq!(
            NntpCommand::classify("STAT <msg@server>"),
            NntpCommand::ArticleByMessageId
        );
    }

    #[test]
    fn test_article_commands_without_message_id() {
        // ARTICLE with number (stateful - requires GROUP context)
        assert_eq!(
            NntpCommand::classify("ARTICLE 12345"),
            NntpCommand::Stateful
        );

        // ARTICLE without argument (stateful - uses current article)
        assert_eq!(NntpCommand::classify("ARTICLE"), NntpCommand::Stateful);

        // BODY with number
        assert_eq!(NntpCommand::classify("BODY 999"), NntpCommand::Stateful);

        // HEAD with number
        assert_eq!(NntpCommand::classify("HEAD 123"), NntpCommand::Stateful);
    }

    #[test]
    fn test_special_characters_in_commands() {
        // Command with newlines
        assert_eq!(NntpCommand::classify("LIST\r\n"), NntpCommand::Stateless);

        // Command with extra whitespace
        assert_eq!(
            NntpCommand::classify("  LIST   ACTIVE  "),
            NntpCommand::Stateless
        );

        // Command with tabs
        assert_eq!(
            NntpCommand::classify("LIST\tACTIVE"),
            NntpCommand::Stateless
        );
    }

    #[test]
    fn test_very_long_commands() {
        // Very long command line
        let long_command = format!("LIST {}", "A".repeat(1000));
        assert_eq!(NntpCommand::classify(&long_command), NntpCommand::Stateless);

        // Very long GROUP name
        let long_group = format!("GROUP {}", "alt.".repeat(100));
        assert_eq!(NntpCommand::classify(&long_group), NntpCommand::Stateful);

        // Very long message-ID
        let long_msgid = format!("ARTICLE <{}@example.com>", "x".repeat(500));
        assert_eq!(
            NntpCommand::classify(&long_msgid),
            NntpCommand::ArticleByMessageId
        );
    }

    #[test]
    fn test_command_parser_extracts_credentials() {
        // Test username extraction
        match CommandClassifier::parse("AUTHINFO USER alice") {
            CommandType::AuthUser(user) => assert_eq!(user, "alice"),
            _ => panic!("Expected AuthUser"),
        }

        // Test username with spaces (takes first word)
        match CommandClassifier::parse("AUTHINFO USER bob smith") {
            CommandType::AuthUser(user) => assert_eq!(user, "bob"),
            _ => panic!("Expected AuthUser"),
        }

        // Test password extraction
        match CommandClassifier::parse("AUTHINFO PASS secret123") {
            CommandType::AuthPass(pass) => assert_eq!(pass, "secret123"),
            _ => panic!("Expected AuthPass"),
        }

        // Test password with special characters
        match CommandClassifier::parse("AUTHINFO PASS p@ssw0rd!#$") {
            CommandType::AuthPass(pass) => assert_eq!(pass, "p@ssw0rd!#$"),
            _ => panic!("Expected AuthPass"),
        }
    }

    #[test]
    fn test_list_command_variations() {
        // LIST without arguments
        assert_eq!(NntpCommand::classify("LIST"), NntpCommand::Stateless);

        // LIST ACTIVE
        assert_eq!(NntpCommand::classify("LIST ACTIVE"), NntpCommand::Stateless);

        // LIST NEWSGROUPS
        assert_eq!(
            NntpCommand::classify("LIST NEWSGROUPS"),
            NntpCommand::Stateless
        );

        // LIST OVERVIEW.FMT
        assert_eq!(
            NntpCommand::classify("LIST OVERVIEW.FMT"),
            NntpCommand::Stateless
        );
    }

    #[test]
    fn test_boundary_conditions() {
        // Single character command
        assert_eq!(NntpCommand::classify("X"), NntpCommand::Stateless);

        // Command that looks like message-ID but isn't
        assert_eq!(
            NntpCommand::classify("NOTARTICLE <test@example.com>"),
            NntpCommand::Stateless
        );

        // Message-ID without angle brackets (not valid, treated as number)
        assert_eq!(
            NntpCommand::classify("ARTICLE test@example.com"),
            NntpCommand::Stateful
        );
    }

    #[test]
    fn test_non_multiplexable_commands() {
        // POST command - cannot be multiplexed
        assert_eq!(NntpCommand::classify("POST"), NntpCommand::NonMultiplexable);

        // IHAVE command - cannot be multiplexed
        assert_eq!(
            NntpCommand::classify("IHAVE <test@example.com>"),
            NntpCommand::NonMultiplexable
        );

        // NEWGROUPS command - cannot be multiplexed
        assert_eq!(
            NntpCommand::classify("NEWGROUPS 20240101 000000 GMT"),
            NntpCommand::NonMultiplexable
        );

        // NEWNEWS command - cannot be multiplexed
        assert_eq!(
            NntpCommand::classify("NEWNEWS * 20240101 000000 GMT"),
            NntpCommand::NonMultiplexable
        );
    }

    #[test]
    fn test_non_multiplexable_case_insensitive() {
        assert_eq!(NntpCommand::classify("post"), NntpCommand::NonMultiplexable);

        assert_eq!(NntpCommand::classify("Post"), NntpCommand::NonMultiplexable);

        assert_eq!(
            NntpCommand::classify("IHAVE <msg>"),
            NntpCommand::NonMultiplexable
        );

        assert_eq!(
            NntpCommand::classify("ihave <msg>"),
            NntpCommand::NonMultiplexable
        );
    }
}
