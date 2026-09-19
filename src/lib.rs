//! # NNTP Proxy Library
//!
//! An NNTP proxy application with connection pooling, multiple
//! backends, optional article caching, and a live terminal dashboard.
//!
//! Install the application with `cargo install nntp-proxy --locked`, then follow
//! the [getting-started guide](https://github.com/mjc/nntp-proxy/blob/main/docs/operator/getting-started.md)
//! to configure a backend and run `nntp-proxy --config config.toml`.
//!
//! The crate requires Rust 1.95 or newer. Use the application for normal
//! operation; these pages document the evolving Rust API for contributors and
//! experimental integrations. [`NntpProxy`] and
//! [`NntpProxyBuilder`] construct the server; [`client::NntpClient`] provides
//! pooled article fetching.
//!
//! ## Architecture
//!
//! - **auth**: Authentication handling for both client and backend connections
//! - **command**: Local NNTP command policy and intercept/reject actions
//! - **config**: Configuration loading and management
//! - **pool**: Connection and buffer pooling
//! - **protocol**: Typed NNTP request metadata, response constants, and status parsing
//! - **proxy**: Main proxy orchestration (`NntpProxy` struct)
//! - **router**: Backend selection and load balancing
//! - **types**: Core type definitions (`ClientId`, `RequestId`, `BackendId`)
//! - **client**: Standalone pooled article-fetching API
//!
//! ## Operating Modes
//!
//! - **Hybrid mode**: Starts with per-command routing and switches to a
//!   dedicated backend connection when stateful protocol context is needed.
//! - **Stateful mode**: Traditional mode where each client gets a dedicated
//!   backend connection.
//! - **Per-command routing mode**: Each supported stateless command can route to
//!   a backend independently.
//!
//! Response shape is request-scoped: callers must not infer multiline behavior
//! from a status code alone. The session layer owns response boundaries,
//! continuation, packed-response suffixes, and ordered delivery. Ordinary
//! forwarding borrows pooled bytes directly.
//!
//! ## Article response ownership
//!
//! [`client::NntpClient`] returns a [`client::FramedArticle`] after the response
//! has been framed. Calling [`client::FramedArticle::validate`]
//! or [`client::FramedArticle::validate_with_yenc`] consumes the framed response
//! and returns a [`client::ValidatedArticle`]. Its
//! [`client::ValidatedArticle::article`] method returns a zero-copy
//! [`protocol::ArticleView`] without repeating semantic validation.
//!
//! Framing checks response boundaries; article validation checks article
//! structure. Callers can also request yEnc validation. Forwarding, indexing,
//! and inspection can use these stages independently; decoding the body is a
//! separate operation.

#![allow(clippy::disallowed_methods)]

#[macro_use]
mod macros;

// Module declarations
pub mod args;
pub mod auth;
pub mod client;
mod compression;
pub mod connection_error;
pub mod formatting;
mod io_util;
pub mod logging;
pub mod metrics;
mod network;
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
pub use args::{CommonArgs, UiMode};
pub use config::{
    Cache, Config, ConfigSource, RoutingMode, Server, create_default_config, has_server_env_vars,
    load_config, load_config_from_env, load_config_with_fallback,
};
pub use proxy::{NntpProxy, NntpProxyBuilder};
pub use runtime::{RuntimeConfig, shutdown_signal};
