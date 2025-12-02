//! Router integration tests
//!
//! These tests verify the BackendSelector behavior including:
//! - Basic routing and backend management
//! - Load tracking across backends
//! - Round-robin distribution
//! - Stateful connection reservation
//! - Weighted distribution based on pool sizes

mod router;
