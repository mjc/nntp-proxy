//! Command processing module
//!
//! This module handles NNTP command classification and processing.
//! It provides a clean abstraction for parsing and validating commands
//! without coupling to the proxy implementation.

mod classifier;
mod handler;

pub use handler::{AuthAction, CommandAction, CommandHandler};
pub use classifier::NntpCommand;

