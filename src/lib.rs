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
pub use args::{CacheArgs, CommonArgs};
pub use config::{
    Cache, Config, ConfigSource, RoutingMode, Server, create_default_config, has_server_env_vars,
    load_config, load_config_from_env, load_config_with_fallback,
};
pub use proxy::{NntpProxy, NntpProxyBuilder, is_client_disconnect_error};
pub use runtime::{RuntimeConfig, shutdown_signal};

// Re-export streaming utilities for standalone client use
pub mod streaming {
    //! Streaming utilities for reading NNTP multiline responses
    //!
    //! These are useful when building standalone NNTP clients that need to
    //! fetch articles directly from servers.
    pub use crate::session::streaming::{
        stream_and_capture_multiline_response, stream_multiline_response,
    };
}

// Re-export backend command utilities for standalone client use
pub mod backend {
    //! Backend communication utilities for NNTP client operations
    //!
    //! Use these when building standalone NNTP clients.
    pub use crate::session::backend::{CommandResponse, send_command};
}
