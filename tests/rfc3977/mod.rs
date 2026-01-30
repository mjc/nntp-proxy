//! RFC 3977: Core NNTP protocol compliance tests
//!
//! This module contains tests for the Network News Transfer Protocol (NNTP)
//! as defined in RFC 3977. These tests verify compliance with the NNTP
//! protocol specification.

pub mod commands;
pub mod errors;
pub mod multiline;
pub mod response;

// TODO: article.rs uses types (Article, Headers, ParseError) not yet in the public API
// TODO: session.rs uses `mod test_helpers` which requires a different wiring approach
// pub mod article;
// pub mod session;
