//! Backend response validation
//!
//! This module provides pure functions for validating backend NNTP responses.

mod validation;

pub use validation::{ResponseWarning, ValidatedResponse, validate_backend_response};
