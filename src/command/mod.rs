//! Command processing module
//!
//! This module handles NNTP command classification and processing.
//! It provides a clean abstraction for parsing and validating commands
//! without coupling to the proxy implementation.

pub mod classifier;
mod handler;

pub use classifier::NntpCommand;
pub use handler::{AuthAction, CommandAction, CommandHandler};
