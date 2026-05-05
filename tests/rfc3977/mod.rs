//! RFC 3977: Core NNTP protocol compliance tests
//!
//! This module contains tests for the Network News Transfer Protocol (NNTP)
//! as defined in RFC 3977. These tests verify compliance with the NNTP
//! protocol specification.

pub mod article;
pub mod commands;
pub mod errors;
pub mod multiline;
pub mod response;
pub mod session;
pub mod session_state;
pub mod stateful_workflow;
pub mod unsupported;
pub mod xover_edgecases;
