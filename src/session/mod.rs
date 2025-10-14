//! Session module
//!
//! This module is being refactored to separate concerns.
//! New submodules are being introduced while maintaining backward compatibility.

// New refactored modules
pub mod backend;

// Legacy module (will be gradually refactored)
mod legacy;

// Re-export everything from legacy for backward compatibility
pub use legacy::{ClientSession, SessionMode};

// Re-export new modules for convenience
pub use backend::{BackendResponse, fetch_backend_response};
