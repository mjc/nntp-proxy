//! # NNTP Proxy Library
//!
//! A high-performance NNTP proxy server implementation with two operating modes:
//! 1:1 mode (one backend per client) and per-command routing mode.
//!
//! ## Architecture
//!
//! The proxy is organized into several modules for clean separation of concerns:
//!
//! - **auth**: Authentication handling for both client and backend connections
//! - **command**: NNTP command parsing and classification
//! - **config**: Configuration loading and management
//! - **pool**: Connection and buffer pooling for high performance
//! - **protocol**: NNTP protocol constants and response parsing
//! - **proxy**: Main proxy orchestration (`NntpProxy` struct)
//! - **router**: Backend selection and load balancing
//! - **types**: Core type definitions (`ClientId`, `RequestId`, `BackendId`)
//!
//! ## Design Philosophy
//!
//! This proxy operates in **stateless mode**, rejecting commands that require
//! maintaining session state (like GROUP, NEXT, LAST). This design enables:
//!
//! - Simpler architecture with clear separation of concerns
//! - Per-command routing mode where each command can use a different backend
//! - Easy testing and maintenance of individual components
//!
//! ## Operating Modes
//!
//! - **1:1 mode**: Traditional mode where each client gets a dedicated backend connection
//! - **Per-command routing mode**: Each command is routed to a backend (round-robin),
//!   but commands are still processed serially (NNTP is synchronous)

// Nursery lint that often produces less readable code with multi-line closures.
#![allow(clippy::option_if_let_else)]
// Doc coverage requirements — adding Errors/Panics sections to 63+31 functions is
// mechanical noise; the codebase uses anyhow::Error pervasively and panics are rare.
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
// f64 comparisons in chart/metrics code — comparing against 0.0 and similar
// exact values is intentional here; using approx comparisons would be wrong.
#![allow(clippy::float_cmp)]
// Casting u64/usize to f64 for chart coordinates is intentional: precision loss
// at large values is acceptable for display purposes (52-bit mantissa covers
// all realistic throughput values accurately).
#![allow(clippy::cast_precision_loss)]
// `use` items declared inside function bodies are idiomatic for localizing imports;
// this lint treats them as confusing, but they improve locality in long functions.
#![allow(clippy::items_after_statements)]
// Numeric cast warnings: all conversions in this codebase are intentional.
// u128→u64 timestamps: safe (u64 holds ~584 years of nanoseconds).
// u64/usize→usize/u32: safe at realistic article/connection counts.
// f64→u64: display-only, values are bounded and non-negative.
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_sign_loss)]
// Long functions in complex handlers are necessary; splitting would obscure the
// control flow. Document with comments instead of splitting arbitrarily.
#![allow(clippy::too_many_lines)]

// Module declarations
pub mod args;
pub mod auth;
pub mod client;
pub mod compression;
pub mod connection_error;
pub mod formatting;
pub mod logging;
pub mod metrics;
pub mod network;
pub mod protocol;
mod proxy;
pub mod stream;
pub mod tui;

// Public modules for integration tests
pub mod cache;
pub mod command;
pub mod config;
pub mod constants;
pub mod pool;
pub mod router;
pub mod runtime;
pub mod session;
pub mod tls;
pub mod types;

// Public exports
pub use args::CommonArgs;
pub use config::{
    Cache, Config, ConfigSource, RoutingMode, Server, create_default_config, has_server_env_vars,
    load_config, load_config_from_env, load_config_with_fallback,
};
pub use proxy::{NntpProxy, NntpProxyBuilder};
pub use runtime::{RuntimeConfig, shutdown_signal};

// Re-export backend command utilities for standalone client use
pub mod backend {
    //! Backend communication utilities for NNTP client operations
    //!
    //! Use these when building standalone NNTP clients.
    pub use crate::session::backend::{BackendResponse, send_request};
}
