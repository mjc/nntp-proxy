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
//! - **proxy**: Main proxy orchestration (NntpProxy struct)
//! - **router**: Backend selection and load balancing
//! - **types**: Core type definitions (ClientId, RequestId, BackendId)
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
mod auth;
pub mod network;
pub mod protocol;
mod proxy;
mod streaming;

// Public modules for integration tests
pub mod cache;
pub mod command;
pub mod config;
pub mod constants;
pub mod health;
pub mod pool;
pub mod router;
pub mod session;
pub mod types;

// Public exports
pub use config::{CacheConfig, Config, ServerConfig, create_default_config, load_config};
pub use network::SocketOptimizer;
pub use proxy::NntpProxy;

