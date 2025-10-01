//! NNTP protocol handling module
//!
//! This module contains protocol-specific constants, response parsing,
//! and protocol utilities for NNTP communication.

mod constants;
mod response;

pub use constants::*;
pub use response::ResponseParser;

// Re-export for future use
#[allow(unused_imports)]
pub use response::NntpResponse;
