//! Cache module for NNTP article caching
//!
//! This module provides caching functionality for NNTP articles,
//! allowing the proxy to cache article content and reduce backend load.

mod article;
mod session;

pub use article::ArticleCache;
pub use session::CachingSession;
