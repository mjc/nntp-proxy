//! Command classification logic for NNTP commands

use super::types::{CommandType, ValidationResult};

/// NNTP command classification for different handling strategies
#[derive(Debug, PartialEq)]
pub enum NntpCommand {
    /// Authentication commands (AUTHINFO USER/PASS) - intercepted locally
    AuthUser,
    AuthPass,
    /// Stateful commands that require GROUP context - REJECTED in stateless mode
    Stateful,
    /// Stateless commands that can be safely proxied without state
    Stateless,
    /// Article retrieval by message-ID (stateless) - can be proxied
    ArticleByMessageId,
}

impl NntpCommand {
    /// Classify an NNTP command based on its content using fast byte-level parsing
    pub fn classify(command: &str) -> Self {
        let trimmed = command.trim();
        let bytes = trimmed.as_bytes();
        
        // Fast path: find space to separate command from arguments
        let cmd_end = memchr::memchr(b' ', bytes).unwrap_or(bytes.len());
        let cmd = &bytes[..cmd_end];
        
        // Convert to uppercase for case-insensitive comparison
        let cmd_upper = cmd.to_ascii_uppercase();
        
        match cmd_upper.as_slice() {
            // Authentication commands (intercepted locally)
            b"AUTHINFO" => {
                if trimmed.to_uppercase().starts_with("AUTHINFO USER") {
                    Self::AuthUser
                } else if trimmed.to_uppercase().starts_with("AUTHINFO PASS") {
                    Self::AuthPass
                } else {
                    Self::Stateless
                }
            }
            
            // Stateful commands - REJECTED (require GROUP context)
            b"GROUP" | b"NEXT" | b"LAST" | b"LISTGROUP" => Self::Stateful,
            
            // XOVER/OVER and HDR/XHDR require group context
            b"XOVER" | b"OVER" | b"XHDR" | b"HDR" => Self::Stateful,
            
            // Article commands - check if by message-ID or number
            b"ARTICLE" | b"BODY" | b"HEAD" | b"STAT" => {
                if cmd_end < bytes.len() {
                    let args = &bytes[cmd_end + 1..];
                    let args_trimmed = args.iter().position(|&b| !b.is_ascii_whitespace())
                        .map(|pos| &args[pos..])
                        .unwrap_or(args);
                    
                    // Message-IDs start with '<' and end with '>'
                    if !args_trimmed.is_empty() && args_trimmed[0] == b'<' {
                        Self::ArticleByMessageId
                    } else {
                        // Article by number or current - needs GROUP context
                        Self::Stateful
                    }
                } else {
                    // No argument = current article - needs GROUP context
                    Self::Stateful
                }
            }
            
            // Stateless commands that don't need GROUP context
            b"LIST" | b"HELP" | b"DATE" | b"CAPABILITIES" | b"MODE" 
            | b"NEWGROUPS" | b"NEWNEWS" | b"POST" | b"QUIT" => Self::Stateless,
            
            // Unknown commands - treat as stateless (forward and let backend decide)
            _ => Self::Stateless,
        }
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
            NntpCommand::Stateful => {
                ValidationResult::Rejected("Command not supported by this proxy (stateless proxy mode)")
            }
            NntpCommand::AuthUser | NntpCommand::AuthPass => {
                ValidationResult::Intercepted
            }
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
        assert_eq!(
            NntpCommand::classify("ARTICLE"),
            NntpCommand::Stateful
        );
        assert_eq!(
            NntpCommand::classify("HEAD 67890"),
            NntpCommand::Stateful
        );
        assert_eq!(NntpCommand::classify("STAT"), NntpCommand::Stateful);
        assert_eq!(
            NntpCommand::classify("XOVER 1-100"),
            NntpCommand::Stateful
        );

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
        assert_eq!(
            NntpCommand::classify("LIST ACTIVE"),
            NntpCommand::Stateless
        );
        assert_eq!(
            NntpCommand::classify("NEWGROUPS 20231201 000000"),
            NntpCommand::Stateless
        );
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
            ValidationResult::Rejected("Command not supported by this proxy (stateless proxy mode)")
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

        assert_eq!(
            CommandClassifier::parse("LIST"),
            CommandType::Stateless
        );
    }
}
