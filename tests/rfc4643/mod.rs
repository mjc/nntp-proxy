//! RFC 4643: NNTP Authentication protocol compliance tests
//!
//! This module contains tests for NNTP Authentication as defined in RFC 4643.
//! These tests verify authentication mechanisms, user credentials, and security.

pub mod authentication;
pub mod backend;
pub mod bypass_prevention;
pub mod capabilities;
pub mod integration;
pub mod review_claims;
pub mod rfc_compliance;
pub mod security;
