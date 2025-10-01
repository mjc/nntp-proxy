//! Command handling with action types
//!
//! This module provides a CommandHandler that processes NNTP commands
//! and returns actions to be taken, separating command interpretation
//! from command execution.

use super::classifier::NntpCommand;

/// Action to take in response to a command
#[derive(Debug, Clone, PartialEq)]
pub enum CommandAction {
    /// Intercept and send authentication response to client
    InterceptAuth(AuthAction),
    /// Reject the command with an error message
    Reject(&'static str),
    /// Forward the command to backend (stateless)
    ForwardStateless,
    /// Forward the command and switch to high-throughput mode (article by message-ID)
    ForwardHighThroughput,
}

/// Specific authentication action
#[derive(Debug, Clone, PartialEq)]
pub enum AuthAction {
    /// Send password required response
    RequestPassword,
    /// Send authentication accepted response
    AcceptAuth,
}

/// Handler for processing commands and determining actions
pub struct CommandHandler;

impl CommandHandler {
    /// Process a command and return the action to take
    pub fn handle_command(command: &str) -> CommandAction {
        match NntpCommand::classify(command) {
            NntpCommand::AuthUser => CommandAction::InterceptAuth(AuthAction::RequestPassword),
            NntpCommand::AuthPass => CommandAction::InterceptAuth(AuthAction::AcceptAuth),
            NntpCommand::Stateful => {
                CommandAction::Reject("Command not supported by this proxy (stateless proxy mode)")
            }
            NntpCommand::NonMultiplexable => {
                CommandAction::Reject("Command not supported by this proxy (multiplexing mode)")
            }
            NntpCommand::ArticleByMessageId => CommandAction::ForwardHighThroughput,
            NntpCommand::Stateless => CommandAction::ForwardStateless,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auth_user_command() {
        let action = CommandHandler::handle_command("AUTHINFO USER test");
        assert_eq!(
            action,
            CommandAction::InterceptAuth(AuthAction::RequestPassword)
        );
    }

    #[test]
    fn test_auth_pass_command() {
        let action = CommandHandler::handle_command("AUTHINFO PASS secret");
        assert_eq!(action, CommandAction::InterceptAuth(AuthAction::AcceptAuth));
    }

    #[test]
    fn test_stateful_command_rejected() {
        let action = CommandHandler::handle_command("GROUP alt.test");
        match action {
            CommandAction::Reject(msg) => {
                assert!(msg.contains("stateless"));
            }
            _ => panic!("Expected Reject action"),
        }
    }

    #[test]
    fn test_article_by_message_id() {
        let action = CommandHandler::handle_command("ARTICLE <test@example.com>");
        assert_eq!(action, CommandAction::ForwardHighThroughput);
    }

    #[test]
    fn test_stateless_command() {
        let action = CommandHandler::handle_command("LIST");
        assert_eq!(action, CommandAction::ForwardStateless);

        let action = CommandHandler::handle_command("HELP");
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
            match CommandHandler::handle_command(cmd) {
                CommandAction::Reject(msg) => {
                    assert!(msg.contains("stateless") || msg.contains("not supported"));
                }
                other => panic!("Expected Reject for '{}', got {:?}", cmd, other),
            }
        }
    }

    #[test]
    fn test_all_article_by_msgid_as_high_throughput() {
        // All message-ID based article commands should be high-throughput
        let msgid_commands = vec![
            "ARTICLE <test@example.com>",
            "BODY <msg@server.org>",
            "HEAD <id@host.net>",
            "STAT <unique@domain.com>",
        ];

        for cmd in msgid_commands {
            assert_eq!(
                CommandHandler::handle_command(cmd),
                CommandAction::ForwardHighThroughput,
                "Command '{}' should be high-throughput",
                cmd
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
            "CAPABILITIES",
            "QUIT",
        ];

        for cmd in stateless_commands {
            assert_eq!(
                CommandHandler::handle_command(cmd),
                CommandAction::ForwardStateless,
                "Command '{}' should be stateless",
                cmd
            );
        }
    }

    #[test]
    fn test_case_insensitive_handling() {
        // Test that command handling is case-insensitive
        assert_eq!(
            CommandHandler::handle_command("list"),
            CommandAction::ForwardStateless
        );
        assert_eq!(
            CommandHandler::handle_command("LiSt"),
            CommandAction::ForwardStateless
        );
        assert_eq!(
            CommandHandler::handle_command("QUIT"),
            CommandAction::ForwardStateless
        );
        assert_eq!(
            CommandHandler::handle_command("quit"),
            CommandAction::ForwardStateless
        );
    }

    #[test]
    fn test_empty_command() {
        // Empty command should be treated as stateless (unknown)
        let action = CommandHandler::handle_command("");
        assert_eq!(action, CommandAction::ForwardStateless);
    }

    #[test]
    fn test_whitespace_handling() {
        // Command with leading/trailing whitespace
        let action = CommandHandler::handle_command("  LIST  ");
        assert_eq!(action, CommandAction::ForwardStateless);

        // Auth command with extra whitespace
        let action = CommandHandler::handle_command("  AUTHINFO USER test  ");
        assert_eq!(
            action,
            CommandAction::InterceptAuth(AuthAction::RequestPassword)
        );
    }

    #[test]
    fn test_malformed_auth_commands() {
        // AUTHINFO without subcommand
        let action = CommandHandler::handle_command("AUTHINFO");
        assert_eq!(action, CommandAction::ForwardStateless);

        // AUTHINFO with unknown subcommand
        let action = CommandHandler::handle_command("AUTHINFO INVALID");
        assert_eq!(action, CommandAction::ForwardStateless);
    }

    #[test]
    fn test_auth_commands_without_arguments() {
        // AUTHINFO USER without username (still intercept)
        let action = CommandHandler::handle_command("AUTHINFO USER");
        assert_eq!(
            action,
            CommandAction::InterceptAuth(AuthAction::RequestPassword)
        );

        // AUTHINFO PASS without password (still intercept)
        let action = CommandHandler::handle_command("AUTHINFO PASS");
        assert_eq!(action, CommandAction::InterceptAuth(AuthAction::AcceptAuth));
    }

    #[test]
    fn test_article_commands_with_newlines() {
        // Command with CRLF
        let action = CommandHandler::handle_command("ARTICLE <msg@test.com>\r\n");
        assert_eq!(action, CommandAction::ForwardHighThroughput);

        // Command with just LF
        let action = CommandHandler::handle_command("LIST\n");
        assert_eq!(action, CommandAction::ForwardStateless);
    }

    #[test]
    fn test_very_long_commands() {
        // Very long stateless command
        let long_cmd = format!("LIST {}", "A".repeat(10000));
        let action = CommandHandler::handle_command(&long_cmd);
        assert_eq!(action, CommandAction::ForwardStateless);

        // Very long GROUP name (stateful)
        let long_group = format!("GROUP {}", "alt.".repeat(1000));
        match CommandHandler::handle_command(&long_group) {
            CommandAction::Reject(_) => {} // Expected
            other => panic!("Expected Reject for long GROUP, got {:?}", other),
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
            CommandAction::ForwardHighThroughput,
            CommandAction::ForwardHighThroughput
        );
        assert_eq!(
            CommandAction::InterceptAuth(AuthAction::RequestPassword),
            CommandAction::InterceptAuth(AuthAction::RequestPassword)
        );

        // Test inequality
        assert_ne!(
            CommandAction::ForwardStateless,
            CommandAction::ForwardHighThroughput
        );
        assert_ne!(
            CommandAction::InterceptAuth(AuthAction::RequestPassword),
            CommandAction::InterceptAuth(AuthAction::AcceptAuth)
        );
    }

    #[test]
    fn test_reject_messages() {
        // Verify reject messages are informative
        match CommandHandler::handle_command("GROUP alt.test") {
            CommandAction::Reject(msg) => {
                assert!(!msg.is_empty());
                assert!(msg.len() > 10); // Should have a meaningful message
            }
            _ => panic!("Expected Reject"),
        }
    }

    #[test]
    fn test_unknown_commands_forwarded() {
        // Unknown commands should be forwarded as stateless
        // The backend server will handle the error
        let unknown_commands = vec!["INVALIDCOMMAND", "XYZABC", "RANDOM DATA", "12345"];

        for cmd in unknown_commands {
            assert_eq!(
                CommandHandler::handle_command(cmd),
                CommandAction::ForwardStateless,
                "Unknown command '{}' should be forwarded",
                cmd
            );
        }
    }

    #[test]
    fn test_non_multiplexable_commands_rejected() {
        // POST should be rejected
        match CommandHandler::handle_command("POST") {
            CommandAction::Reject(msg) => {
                assert!(msg.contains("multiplexing"));
            }
            _ => panic!("Expected Reject for POST"),
        }

        // IHAVE should be rejected
        match CommandHandler::handle_command("IHAVE <test@example.com>") {
            CommandAction::Reject(msg) => {
                assert!(msg.contains("multiplexing"));
            }
            _ => panic!("Expected Reject for IHAVE"),
        }

        // NEWGROUPS should be rejected
        match CommandHandler::handle_command("NEWGROUPS 20240101 000000 GMT") {
            CommandAction::Reject(msg) => {
                assert!(msg.contains("multiplexing"));
            }
            _ => panic!("Expected Reject for NEWGROUPS"),
        }

        // NEWNEWS should be rejected
        match CommandHandler::handle_command("NEWNEWS * 20240101 000000 GMT") {
            CommandAction::Reject(msg) => {
                assert!(msg.contains("multiplexing"));
            }
            _ => panic!("Expected Reject for NEWNEWS"),
        }
    }

    #[test]
    fn test_reject_message_content() {
        // Verify different reject messages for different command types
        let stateful_reject = match CommandHandler::handle_command("GROUP alt.test") {
            CommandAction::Reject(msg) => msg,
            _ => panic!("Expected Reject"),
        };

        let multiplexing_reject = match CommandHandler::handle_command("POST") {
            CommandAction::Reject(msg) => msg,
            _ => panic!("Expected Reject"),
        };

        // They should have different messages
        assert!(stateful_reject.contains("stateless"));
        assert!(multiplexing_reject.contains("multiplexing"));
        assert_ne!(stateful_reject, multiplexing_reject);
    }
}
