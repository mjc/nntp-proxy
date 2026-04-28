//! Command handling with action types
//!
//! This module provides a `CommandHandler` that processes NNTP commands
//! and returns actions to be taken, separating command interpretation
//! from command execution.
//!
//! # NNTP Response Codes
//!
//! Response codes follow RFC 3977 Section 3.2:
//! <https://www.rfc-editor.org/rfc/rfc3977.html#section-3.2>
//!
//! ## Codes Used
//!
//! - `480` Authentication required
//!   <https://www.rfc-editor.org/rfc/rfc4643.html#section-2.4.1>
//! - `502` Command not implemented\
//!   <https://www.rfc-editor.org/rfc/rfc3977.html#section-3.2.1>
//!   Used when a command is recognized but not supported by this server

use super::classifier::NntpCommand;

/// Action to take in response to a command
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum CommandAction<'a> {
    /// Intercept and send authentication response to client
    InterceptAuth(AuthAction<'a>),
    /// Reject the command with an error message (NNTP response format with CRLF)
    Reject(&'static str),
    /// Forward the command to backend (stateless)
    ForwardStateless,
    /// Intercept CAPABILITIES and return a synthetic proxy-accurate capability list
    InterceptCapabilities,
}

/// Specific authentication action
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum AuthAction<'a> {
    /// Send password required response (username provided)
    RequestPassword(&'a str),
    /// Validate credentials and send appropriate response
    ValidateAndRespond { password: &'a str },
    /// AUTHINFO with an unrecognized subcommand — reject with 501 per RFC 4643 §2.3.1
    UnknownSubcommand,
}

/// Handler for processing commands and determining actions
pub struct CommandHandler;

impl CommandHandler {
    /// Classify a command and return the action to take
    #[must_use]
    pub fn classify(command: &str) -> CommandAction<'_> {
        match NntpCommand::parse(command) {
            NntpCommand::AuthUser => {
                // Extract username from "AUTHINFO USER <username>" — case-insensitive per RFC 3977 §3.1
                let trimmed = command.trim();
                const USER_PREFIX: &str = "AUTHINFO USER";
                let username = if trimmed.len() >= USER_PREFIX.len()
                    && trimmed[..USER_PREFIX.len()].eq_ignore_ascii_case(USER_PREFIX)
                {
                    trimmed[USER_PREFIX.len()..].trim()
                } else {
                    ""
                };
                CommandAction::InterceptAuth(AuthAction::RequestPassword(username))
            }
            NntpCommand::AuthPass => {
                // Extract password from "AUTHINFO PASS <password>" — case-insensitive per RFC 3977 §3.1
                let trimmed = command.trim();
                const PASS_PREFIX: &str = "AUTHINFO PASS";
                let password = if trimmed.len() >= PASS_PREFIX.len()
                    && trimmed[..PASS_PREFIX.len()].eq_ignore_ascii_case(PASS_PREFIX)
                {
                    trimmed[PASS_PREFIX.len()..].trim()
                } else {
                    ""
                };
                CommandAction::InterceptAuth(AuthAction::ValidateAndRespond { password })
            }
            NntpCommand::AuthUnknown => {
                // RFC 4643 §2.3.1: route through InterceptAuth so auth state is checked first.
                // common::handle_auth_command returns 502 if already authenticated, else 501.
                CommandAction::InterceptAuth(AuthAction::UnknownSubcommand)
            }
            NntpCommand::Stateful => {
                // RFC 3977 Section 3.2.1: 502 Command not implemented
                // https://www.rfc-editor.org/rfc/rfc3977.html#section-3.2.1
                CommandAction::Reject("502 Command not implemented in stateless proxy mode\r\n")
            }
            NntpCommand::Post => {
                // RFC 3977 §6.3.1: servers that do not permit posting MUST return 440
                CommandAction::Reject("440 Posting not permitted\r\n")
            }
            NntpCommand::NonRoutable => {
                // RFC 3977 Section 3.2.1: 502 Command not implemented
                // https://www.rfc-editor.org/rfc/rfc3977.html#section-3.2.1
                CommandAction::Reject("502 Command not implemented in per-command routing mode\r\n")
            }
            NntpCommand::Capabilities => {
                // RFC 3977 §5.2 + RFC 4643 §3.1: proxy must return its own capability list,
                // not forward the backend's. Accessible before and after authentication.
                CommandAction::InterceptCapabilities
            }
            NntpCommand::ArticleByMessageId | NntpCommand::Stateless => {
                CommandAction::ForwardStateless
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auth_user_command() {
        let action = CommandHandler::classify("AUTHINFO USER test");
        assert!(matches!(
            action,
            CommandAction::InterceptAuth(AuthAction::RequestPassword(username)) if username == "test"
        ));
    }

    #[test]
    fn test_auth_pass_command() {
        let action = CommandHandler::classify("AUTHINFO PASS secret");
        assert!(matches!(
            action,
            CommandAction::InterceptAuth(AuthAction::ValidateAndRespond { password }) if password == "secret"
        ));
    }

    #[test]
    fn test_stateful_command_rejected() {
        let action = CommandHandler::classify("GROUP alt.test");
        assert!(
            matches!(action, CommandAction::Reject(msg) if msg.contains("stateless")),
            "Expected Reject with 'stateless' in message"
        );
    }

    #[test]
    fn test_article_by_message_id() {
        let action = CommandHandler::classify("ARTICLE <test@example.com>");
        assert_eq!(action, CommandAction::ForwardStateless);
    }

    #[test]
    fn test_stateless_command() {
        let action = CommandHandler::classify("LIST");
        assert_eq!(action, CommandAction::ForwardStateless);

        let action = CommandHandler::classify("HELP");
        assert_eq!(action, CommandAction::ForwardStateless);
    }

    #[test]
    fn test_all_stateful_commands_rejected() {
        // Test various stateful commands
        let stateful_commands = vec![
            "GROUP alt.test",
            "NEXT",
            "LAST",
            "LISTGROUP alt.test",
            "ARTICLE 123",
            "HEAD 456",
            "BODY 789",
            "STAT",
            "XOVER 1-100",
        ];

        for cmd in stateful_commands {
            match CommandHandler::classify(cmd) {
                CommandAction::Reject(msg) => {
                    assert!(msg.contains("stateless") || msg.contains("not supported"));
                }
                other => panic!("Expected Reject for '{cmd}', got {other:?}"),
            }
        }
    }

    #[test]
    fn test_all_article_by_msgid_forwarded() {
        // All message-ID based article commands should be forwarded as stateless
        let msgid_commands = vec![
            "ARTICLE <test@example.com>",
            "BODY <msg@server.org>",
            "HEAD <id@host.net>",
            "STAT <unique@domain.com>",
        ];

        for cmd in msgid_commands {
            assert_eq!(
                CommandHandler::classify(cmd),
                CommandAction::ForwardStateless,
                "Command '{cmd}' should be forwarded as stateless"
            );
        }
    }

    #[test]
    fn test_various_stateless_commands() {
        let stateless_commands = vec![
            "HELP",
            "LIST",
            "LIST ACTIVE",
            "LIST NEWSGROUPS",
            "DATE",
            "QUIT",
        ];

        for cmd in stateless_commands {
            assert_eq!(
                CommandHandler::classify(cmd),
                CommandAction::ForwardStateless,
                "Command '{cmd}' should be stateless"
            );
        }
    }

    #[test]
    fn test_capabilities_intercepted_not_forwarded() {
        // RFC 3977 §5.2 + RFC 4643 §3.1: CAPABILITIES must be intercepted by the proxy
        // to return an accurate capability list, not forwarded to the backend.
        assert_eq!(
            CommandHandler::classify("CAPABILITIES"),
            CommandAction::InterceptCapabilities,
        );
        assert_eq!(
            CommandHandler::classify("capabilities"),
            CommandAction::InterceptCapabilities,
        );
        assert_eq!(
            CommandHandler::classify("Capabilities"),
            CommandAction::InterceptCapabilities,
        );
    }

    /// Bug 2 regression test: RFC 4643 §2.3.1 — AUTHINFO is case-insensitive.
    ///
    /// Before fix: the username/password extractor only stripped exact "AUTHINFO USER" or
    /// "authinfo user" prefixes, so mixed-case commands (e.g. "Authinfo User foo") classified
    /// correctly but returned an empty username.
    #[test]
    fn test_mixed_case_authinfo_extraction() {
        // "Authinfo User" — Titlecase keyword, Titlecase subcommand
        let action = CommandHandler::classify("Authinfo User testuser");
        assert!(
            matches!(
                action,
                CommandAction::InterceptAuth(AuthAction::RequestPassword(u)) if u == "testuser"
            ),
            "Expected username 'testuser', got: {action:?}"
        );

        // "AUTHINFO user" — uppercase keyword, lowercase subcommand
        let action = CommandHandler::classify("AUTHINFO user anotheruser");
        assert!(
            matches!(
                action,
                CommandAction::InterceptAuth(AuthAction::RequestPassword(u)) if u == "anotheruser"
            ),
            "Expected username 'anotheruser', got: {action:?}"
        );

        // "aUtHiNfO pAsS" — fully mixed case
        let action = CommandHandler::classify("aUtHiNfO pAsS mypassword");
        assert!(
            matches!(
                action,
                CommandAction::InterceptAuth(AuthAction::ValidateAndRespond { password: p }) if p == "mypassword"
            ),
            "Expected password 'mypassword', got: {action:?}"
        );

        // "Authinfo Pass" — Titlecase keyword + Titlecase subcommand
        let action = CommandHandler::classify("Authinfo Pass s3cr3t");
        assert!(
            matches!(
                action,
                CommandAction::InterceptAuth(AuthAction::ValidateAndRespond { password: p }) if p == "s3cr3t"
            ),
            "Expected password 's3cr3t', got: {action:?}"
        );
    }

    #[test]
    fn test_case_insensitive_handling() {
        // Test that command handling is case-insensitive
        assert_eq!(
            CommandHandler::classify("list"),
            CommandAction::ForwardStateless
        );
        assert_eq!(
            CommandHandler::classify("LiSt"),
            CommandAction::ForwardStateless
        );
        assert_eq!(
            CommandHandler::classify("QUIT"),
            CommandAction::ForwardStateless
        );
        assert_eq!(
            CommandHandler::classify("quit"),
            CommandAction::ForwardStateless
        );
    }

    #[test]
    fn test_empty_command() {
        // Empty command should be treated as stateless (unknown)
        let action = CommandHandler::classify("");
        assert_eq!(action, CommandAction::ForwardStateless);
    }

    #[test]
    fn test_whitespace_handling() {
        // Command with leading/trailing whitespace
        let action = CommandHandler::classify("  LIST  ");
        assert_eq!(action, CommandAction::ForwardStateless);

        // Auth command with extra whitespace
        let action = CommandHandler::classify("  AUTHINFO USER test  ");
        assert!(matches!(
            action,
            CommandAction::InterceptAuth(AuthAction::RequestPassword(username)) if username == "test"
        ));
    }

    #[test]
    fn test_malformed_auth_commands() {
        // AUTHINFO without subcommand
        let action = CommandHandler::classify("AUTHINFO");
        assert_eq!(action, CommandAction::ForwardStateless);

        // AUTHINFO with unknown subcommand — intercepted so auth state can be checked first.
        // Returns 501 if not authenticated, 502 if already authenticated (RFC 4643 §2.3.1/§2.2).
        let action = CommandHandler::classify("AUTHINFO INVALID");
        assert!(
            matches!(
                action,
                CommandAction::InterceptAuth(AuthAction::UnknownSubcommand)
            ),
            "Unknown AUTHINFO subcommand must produce InterceptAuth(UnknownSubcommand), got: {action:?}"
        );
    }

    #[test]
    fn test_auth_commands_without_arguments() {
        // AUTHINFO USER without username (still intercept, empty username)
        let action = CommandHandler::classify("AUTHINFO USER");
        assert!(matches!(
            action,
            CommandAction::InterceptAuth(AuthAction::RequestPassword(username)) if username.is_empty()
        ));

        // AUTHINFO PASS without password (still intercept, empty password)
        let action = CommandHandler::classify("AUTHINFO PASS");
        assert!(matches!(
            action,
            CommandAction::InterceptAuth(AuthAction::ValidateAndRespond { password }) if password.is_empty()
        ));
    }

    #[test]
    fn test_article_commands_with_newlines() {
        // Command with CRLF
        let action = CommandHandler::classify("ARTICLE <msg@test.com>\r\n");
        assert_eq!(action, CommandAction::ForwardStateless);

        // Command with just LF
        let action = CommandHandler::classify("LIST\n");
        assert_eq!(action, CommandAction::ForwardStateless);
    }

    #[test]
    fn test_very_long_commands() {
        // Very long stateless command
        let long_cmd = format!("LIST {}", "A".repeat(10000));
        let action = CommandHandler::classify(&long_cmd);
        assert_eq!(action, CommandAction::ForwardStateless);

        // Very long GROUP name (stateful)
        let long_group = format!("GROUP {}", "alt.".repeat(1000));
        match CommandHandler::classify(&long_group) {
            CommandAction::Reject(_) => {} // Expected
            other => panic!("Expected Reject for long GROUP, got {other:?}"),
        }
    }

    #[test]
    fn test_command_action_equality() {
        // Test that CommandAction implements PartialEq correctly
        assert_eq!(
            CommandAction::ForwardStateless,
            CommandAction::ForwardStateless
        );
        assert_eq!(
            CommandAction::InterceptAuth(AuthAction::RequestPassword("test")),
            CommandAction::InterceptAuth(AuthAction::RequestPassword("test"))
        );

        // Test inequality
        assert_ne!(
            CommandAction::InterceptAuth(AuthAction::RequestPassword("user1")),
            CommandAction::InterceptAuth(AuthAction::ValidateAndRespond { password: "pass1" })
        );
    }

    #[test]
    fn test_reject_messages() {
        // Verify reject messages are informative
        assert!(
            matches!(
                CommandHandler::classify("GROUP alt.test"),
                CommandAction::Reject(msg) if !msg.is_empty() && msg.len() > 10
            ),
            "Expected Reject with meaningful message"
        );
    }

    #[test]
    fn test_unknown_commands_forwarded() {
        // Unknown commands should be forwarded as stateless
        // The backend server will handle the error
        let unknown_commands = ["INVALIDCOMMAND", "XYZABC", "RANDOM DATA", "12345"];

        assert!(
            unknown_commands
                .iter()
                .all(|cmd| { CommandHandler::classify(cmd) == CommandAction::ForwardStateless }),
            "All unknown commands should be forwarded as stateless"
        );
    }

    #[test]
    fn test_non_routable_commands_rejected() {
        // POST must return 440 per RFC 3977 §6.3.1 (posting not permitted)
        assert!(
            matches!(
                CommandHandler::classify("POST"),
                CommandAction::Reject(msg) if msg.starts_with("440")
            ),
            "POST must return 440 (Posting not permitted), got: {:?}",
            CommandHandler::classify("POST")
        );

        // IHAVE should be rejected with 502 (not implemented)
        assert!(
            matches!(
                CommandHandler::classify("IHAVE <test@example.com>"),
                CommandAction::Reject(msg) if msg.contains("routing")
            ),
            "Expected Reject for IHAVE"
        );

        // NEWGROUPS/NEWNEWS are stateless (RFC 3977 §7.3-7.4) — forwarded, not rejected
        assert_eq!(
            CommandHandler::classify("NEWGROUPS 20240101 000000 GMT"),
            CommandAction::ForwardStateless,
            "NEWGROUPS should be forwarded as stateless"
        );
        assert_eq!(
            CommandHandler::classify("NEWNEWS * 20240101 000000 GMT"),
            CommandAction::ForwardStateless,
            "NEWNEWS should be forwarded as stateless"
        );
    }

    #[test]
    fn test_reject_message_content() {
        // Verify different reject messages for different command types
        let CommandAction::Reject(stateful_reject) = CommandHandler::classify("GROUP alt.test")
        else {
            panic!("Expected Reject")
        };

        let CommandAction::Reject(post_reject) = CommandHandler::classify("POST") else {
            panic!("Expected Reject")
        };

        let CommandAction::Reject(ihave_reject) = CommandHandler::classify("IHAVE <x@y>") else {
            panic!("Expected Reject")
        };

        // Stateful commands rejected with stateless-mode message
        assert!(stateful_reject.contains("stateless"));
        // POST rejected with RFC 3977 §6.3.1 440 response
        assert!(post_reject.starts_with("440"));
        assert!(
            post_reject.to_lowercase().contains("posting")
                || post_reject.to_lowercase().contains("permitted")
        );
        // IHAVE rejected with routing-mode message
        assert!(ihave_reject.contains("routing"));
        // All rejections are distinct
        assert_ne!(stateful_reject, post_reject);
        assert_ne!(stateful_reject, ihave_reject);
        assert_ne!(post_reject, ihave_reject);
    }

    #[test]
    fn test_reject_response_format() {
        // RFC 3977 Section 3.1: Response format is "xyz text\r\n"
        // https://www.rfc-editor.org/rfc/rfc3977.html#section-3.1

        let CommandAction::Reject(response) = CommandHandler::classify("GROUP alt.test") else {
            panic!("Expected Reject")
        };

        // Must start with 3-digit status code
        assert!(response.len() >= 3, "Response too short");
        assert!(
            response[0..3].chars().all(|c| c.is_ascii_digit()),
            "First 3 chars must be digits, got: {}",
            &response[0..3]
        );

        // Must have space after status code
        assert_eq!(&response[3..4], " ", "Must have space after status code");

        // Must end with CRLF
        assert!(response.ends_with("\r\n"), "Response must end with CRLF");

        // Status code must be 503 (Feature not supported)
        // RFC 3977 §3.2.1: 503 = "Feature not supported" is correct for commands the
        // proxy structurally cannot support (e.g. stateful commands in per-command mode)
        assert!(
            response.starts_with("503 "),
            "Expected 503 status code, got: {response}"
        );
    }

    #[test]
    fn test_all_reject_responses_are_valid_nntp() {
        // Test all commands that produce Reject responses
        let reject_commands = vec![
            "GROUP alt.test",
            "NEXT",
            "LAST",
            "POST",
            "IHAVE <test@example.com>",
        ];

        for cmd in reject_commands {
            let CommandAction::Reject(response) = CommandHandler::classify(cmd) else {
                panic!("Expected Reject for command: {cmd}");
            };

            // All must be valid NNTP format
            assert!(
                response.len() >= 5,
                "Response too short for {cmd}: {response}"
            );
            assert!(
                response.starts_with(|c: char| c.is_ascii_digit()),
                "Must start with digit for {cmd}: {response}"
            );
            assert!(
                response.ends_with("\r\n"),
                "Must end with CRLF for {cmd}: {response}"
            );
            assert!(
                response.contains(' '),
                "Must have space separator for {cmd}: {response}"
            );
        }
    }

    #[test]
    fn test_502_status_code_usage() {
        // RFC 3977 Section 3.2.1: 502 is "Command not implemented"
        // https://www.rfc-editor.org/rfc/rfc3977.html#section-3.2.1
        // "The command is not presently implemented by the server, although
        //  it may be implemented in the future."

        // Stateful commands in stateless mode use 502
        let CommandAction::Reject(response) = CommandHandler::classify("GROUP alt.test") else {
            panic!("Expected Reject");
        };
        assert!(
            response.starts_with("502 "),
            "Stateful commands should return 502, got: {response}"
        );

        // POST uses 440 per RFC 3977 §6.3.1 (posting not permitted)
        let CommandAction::Reject(response) = CommandHandler::classify("POST") else {
            panic!("Expected Reject");
        };
        assert!(
            response.starts_with("440 "),
            "POST must return 440 (posting not permitted), got: {response}"
        );

        // IHAVE uses 502 (transit command not implemented)
        let CommandAction::Reject(response) = CommandHandler::classify("IHAVE <x@y>") else {
            panic!("Expected Reject");
        };
        assert!(
            response.starts_with("502 "),
            "IHAVE should return 502, got: {response}"
        );
    }

    #[test]
    fn test_response_messages_are_descriptive() {
        // Responses should explain why the command is rejected
        let CommandAction::Reject(stateful) = CommandHandler::classify("GROUP alt.test") else {
            panic!("Expected Reject");
        };
        assert!(
            stateful.to_lowercase().contains("stateless")
                || stateful.to_lowercase().contains("mode"),
            "Should explain stateless mode restriction: {stateful}"
        );

        let CommandAction::Reject(post) = CommandHandler::classify("POST") else {
            panic!("Expected Reject");
        };
        assert!(
            post.to_lowercase().contains("posting") || post.to_lowercase().contains("permitted"),
            "POST rejection should mention posting or permitted: {post}"
        );
    }
}
