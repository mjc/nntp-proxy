//! # NNTP Proxy Library
//!
//! A high-performance, stateless NNTP proxy server implementation designed
//! for future connection multiplexing capabilities.
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
//! - **router**: Request routing and multiplexing coordination
//! - **types**: Core type definitions (ClientId, RequestId, BackendId)
//!
//! ## Design Philosophy
//!
//! This proxy operates in **stateless mode**, rejecting commands that require
//! maintaining session state (like GROUP, NEXT, LAST). This design enables:
//!
//! - Simpler architecture with clear separation of concerns
//! - Future multiplexing where multiple clients share backend connections
//! - Easy testing and maintenance of individual components
//!
//! ## Current Implementation
//!
//! The current implementation uses a 1:1 client-to-backend connection model
//! with connection pooling. The modular architecture is designed to support
//! future conversion to a true multiplexing proxy.

// Module declarations
mod auth;
mod network;
mod protocol;
mod proxy;
mod streaming;

// Public modules for integration tests
pub mod command;
pub mod config;
pub mod constants;
pub mod health;
pub mod pool;
pub mod router;
pub mod session;
pub mod types;

// Public exports
pub use config::{Config, ServerConfig, create_default_config, load_config};
pub use proxy::NntpProxy;
