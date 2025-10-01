//! Command processing module
//!
//! This module handles NNTP command classification and processing.
//! It provides a clean abstraction for parsing and validating commands
//! without coupling to the proxy implementation.

mod classifier;
mod handler;
mod types;

pub use handler::{AuthAction, CommandAction, CommandHandler};

// Re-export types for future multiplexing use
#[allow(unused_imports)]
pub use classifier::{CommandClassifier, NntpCommand};
#[allow(unused_imports)]
pub use types::{Command, CommandType, ValidationResult};
