//! Session handlers for different routing modes
//!
//! This module contains the core session handling logic split by routing mode:
//! - `standard`: Standard 1:1 routing with dedicated backend connection
//! - `per_command`: Per-command routing where each command can go to a different backend
//! - `hybrid`: Hybrid mode that starts with per-command routing and switches to stateful
//!
//! Shared utilities are in the parent `session::common` module.
//!
//! All handler functions are implemented as methods on `ClientSession` in their
//! respective modules. No need to re-export since they're all impl blocks.

mod hybrid;
mod per_command;
mod standard;
