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
            NntpCommand::AuthUser => {
                CommandAction::InterceptAuth(AuthAction::RequestPassword)
            }
            NntpCommand::AuthPass => {
                CommandAction::InterceptAuth(AuthAction::AcceptAuth)
            }
            NntpCommand::Stateful => {
                CommandAction::Reject("Command not supported by this proxy (stateless proxy mode)")
            }
            NntpCommand::ArticleByMessageId => {
                CommandAction::ForwardHighThroughput
            }
            NntpCommand::Stateless => {
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
        let action = CommandHandler::handle_command("AUTHINFO USER test");
        assert_eq!(
            action,
            CommandAction::InterceptAuth(AuthAction::RequestPassword)
        );
    }

    #[test]
    fn test_auth_pass_command() {
        let action = CommandHandler::handle_command("AUTHINFO PASS secret");
        assert_eq!(
            action,
            CommandAction::InterceptAuth(AuthAction::AcceptAuth)
        );
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
}
