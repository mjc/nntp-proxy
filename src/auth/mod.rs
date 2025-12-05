//! Authentication module
//!
//! This module handles authentication for both client-facing and
//! backend server authentication.

mod backend;
mod handler;

pub use handler::AuthHandler;
