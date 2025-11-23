//! Cache module for NNTP caching
//!
//! This module provides caching functionality for:
//! - Article content caching (reduces backend load for repeated ARTICLE requests)
//! - Article location caching (tracks which backends have which articles)

mod article;
mod location;

pub use article::{ArticleCache, CachedArticle};
pub use location::{ArticleLocationCache, BackendAvailability};
