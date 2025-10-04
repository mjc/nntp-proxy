//! Authentication module
//!
//! This module handles authentication for both client-facing and
//! backend server authentication.

mod backend;
mod handler;

#[allow(unused_imports)]
pub use backend::BackendAuthenticator;
pub use handler::AuthHandler;
