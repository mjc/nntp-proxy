//! Cache module for NNTP article caching
//!
//! This module provides caching functionality for NNTP articles,
//! allowing the proxy to cache article content and reduce backend load.
//!
//! The `ArticleAvailability` type serves dual purposes:
//! 1. Cache persistence - track which backends have which articles across requests
//! 2. Retry tracking - track which backends tried during 430 retry loops (transient)

mod article;

pub use article::{ArticleAvailability, ArticleCache, ArticleEntry};
