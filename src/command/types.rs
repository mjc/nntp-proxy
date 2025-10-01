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
