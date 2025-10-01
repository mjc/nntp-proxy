//! Command types and structures

/// Represents a validated and classified NNTP command
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    pub command_type: CommandType,
    pub raw: String,
}

/// Classification of NNTP commands for routing decisions
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub enum CommandType {
    /// Authentication commands (AUTHINFO USER/PASS) - intercepted locally
    AuthUser(String),
    AuthPass(String),
    /// Stateful commands that require GROUP context - REJECTED in stateless mode
    Stateful,
    /// Commands that cannot be multiplexed - REJECTED in multiplexing mode
    NonMultiplexable,
    /// Stateless commands that can be safely proxied without state
    Stateless,
    /// Article retrieval by message-ID (stateless) - can be proxied
    ArticleByMessageId,
}

/// Result of command validation
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub enum ValidationResult {
    /// Command is allowed and should be forwarded
    Allowed,
    /// Command is rejected with error message
    Rejected(&'static str),
    /// Command is intercepted and handled locally (auth)
    Intercepted,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_creation() {
        let cmd = Command {
            command_type: CommandType::Stateless,
            raw: "LIST\r\n".to_string(),
        };
        assert_eq!(cmd.raw, "LIST\r\n");
    }

    #[test]
    fn test_command_type_equality() {
        let auth_user1 = CommandType::AuthUser("user1".to_string());
        let auth_user2 = CommandType::AuthUser("user1".to_string());
        let auth_user3 = CommandType::AuthUser("user2".to_string());

        assert_eq!(auth_user1, auth_user2);
        assert_ne!(auth_user1, auth_user3);
    }

    #[test]
    fn test_command_type_auth_user() {
        let cmd_type = CommandType::AuthUser("testuser".to_string());
        match cmd_type {
            CommandType::AuthUser(username) => {
                assert_eq!(username, "testuser");
            }
            _ => panic!("Expected AuthUser variant"),
        }
    }

    #[test]
    fn test_command_type_auth_pass() {
        let cmd_type = CommandType::AuthPass("secret".to_string());
        match cmd_type {
            CommandType::AuthPass(password) => {
                assert_eq!(password, "secret");
            }
            _ => panic!("Expected AuthPass variant"),
        }
    }

    #[test]
    fn test_command_type_variants() {
        let stateful = CommandType::Stateful;
        let stateless = CommandType::Stateless;
        let article = CommandType::ArticleByMessageId;

        assert_ne!(stateful, stateless);
        assert_ne!(stateless, article);
        assert_ne!(stateful, article);
    }

    #[test]
    fn test_validation_result_allowed() {
        let result = ValidationResult::Allowed;
        assert_eq!(result, ValidationResult::Allowed);
    }

    #[test]
    fn test_validation_result_rejected() {
        let result = ValidationResult::Rejected("Command not allowed");
        match result {
            ValidationResult::Rejected(msg) => {
                assert_eq!(msg, "Command not allowed");
            }
            _ => panic!("Expected Rejected variant"),
        }
    }

    #[test]
    fn test_validation_result_intercepted() {
        let result = ValidationResult::Intercepted;
        assert_eq!(result, ValidationResult::Intercepted);
    }

    #[test]
    fn test_command_clone() {
        let cmd1 = Command {
            command_type: CommandType::Stateless,
            raw: "CAPABILITIES\r\n".to_string(),
        };
        let cmd2 = cmd1.clone();

        assert_eq!(cmd1, cmd2);
        assert_eq!(cmd1.raw, cmd2.raw);
    }

    #[test]
    fn test_command_type_debug() {
        let cmd_type = CommandType::AuthUser("user".to_string());
        let debug_str = format!("{:?}", cmd_type);
        assert!(debug_str.contains("AuthUser"));
        assert!(debug_str.contains("user"));
    }

    #[test]
    fn test_validation_result_equality() {
        assert_eq!(ValidationResult::Allowed, ValidationResult::Allowed);
        assert_eq!(ValidationResult::Intercepted, ValidationResult::Intercepted);
        assert_eq!(
            ValidationResult::Rejected("test"),
            ValidationResult::Rejected("test")
        );
        assert_ne!(
            ValidationResult::Rejected("test1"),
            ValidationResult::Rejected("test2")
        );
    }

    #[test]
    fn test_command_with_different_types() {
        let commands = vec![
            Command {
                command_type: CommandType::AuthUser("user".to_string()),
                raw: "AUTHINFO USER user\r\n".to_string(),
            },
            Command {
                command_type: CommandType::AuthPass("pass".to_string()),
                raw: "AUTHINFO PASS pass\r\n".to_string(),
            },
            Command {
                command_type: CommandType::Stateless,
                raw: "DATE\r\n".to_string(),
            },
            Command {
                command_type: CommandType::Stateful,
                raw: "LAST\r\n".to_string(),
            },
            Command {
                command_type: CommandType::ArticleByMessageId,
                raw: "ARTICLE <123@example.com>\r\n".to_string(),
            },
        ];

        assert_eq!(commands.len(), 5);

        // Each command should be different
        for i in 0..commands.len() {
            for j in i + 1..commands.len() {
                assert_ne!(commands[i], commands[j]);
            }
        }
    }
}
