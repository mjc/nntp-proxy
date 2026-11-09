//! Prometheus scrape endpoint.
//!
//! Disabled unless `[metrics]` (or `--metrics-listen`) is configured. Values
//! are read from the proxy's existing lock-free collectors when a scrape
//! arrives, so the endpoint adds no work to request handling between scrapes.
//! See `docs/operator/metrics.md` for the metric reference.

pub mod process;
pub mod scrape;
pub mod server;
pub mod source;
pub mod text;

pub use scrape::{Scrape, render};
pub use server::{CONTENT_TYPE, ServerLimits, bind, serve};
pub use source::{ScrapeSource, gather};

use std::net::SocketAddr;
use std::sync::Arc;

/// Bind `addr` and serve scrapes of `proxy` in a background task.
///
/// Returns the bound address (useful when `addr` used port 0) and the task
/// handle; abort the handle to stop serving.
///
/// # Errors
/// Returns the bind error when the address is unavailable.
pub async fn spawn_endpoint(
    addr: SocketAddr,
    proxy: Arc<crate::NntpProxy>,
) -> std::io::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = bind(addr).await?;
    let local = listener.local_addr()?;
    let handle = tokio::spawn(serve(listener, proxy, ServerLimits::default()));
    Ok((local, handle))
}
