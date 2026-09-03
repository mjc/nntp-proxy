//! Command processing module
//!
//! This module handles NNTP command classification and processing.
//! It provides a clean abstraction for parsing and validating commands
//! without coupling to the proxy implementation.

mod handler;

pub(crate) use handler::StatefulHandoff;
pub use handler::{
    ArticleLookupRequest, AuthAction, AuthenticationAccess, CommandAction, CommandHandler,
    CommandPlan, RejectResponse, StatefulRequest,
};
