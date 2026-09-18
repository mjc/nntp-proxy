//! Resource-bound article states used by the proxy's framed client surface.
//!
//! The session framer has a different physical owner from the standalone
//! client, but the state transitions have the same meaning in both projects:
//! receiving is tied to an active exchange, framing owns one exact response,
//! and validation owns immutable bytes plus its semantic view.

use super::ArticleLayout;
use crate::protocol::{RequestKind, StatusCode};

/// An article operation in one of the protocol-owned states.
#[derive(Debug)]
pub(crate) struct Article<State>(pub(crate) State);

/// A complete wire response retained by an owner selected by the adapter.
#[derive(Debug)]
pub(crate) struct Framed<B> {
    pub(crate) bytes: B,
    pub(crate) kind: RequestKind,
    pub(crate) status: StatusCode,
}

/// Semantic article proof tied to the immutable storage it describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Validated<B> {
    bytes: B,
    layout: ArticleLayout,
}

impl<B> Validated<B> {
    pub(crate) const fn new(bytes: B, layout: ArticleLayout) -> Self {
        Self { bytes, layout }
    }

    pub(crate) fn bytes(&self) -> &B {
        &self.bytes
    }

    pub(crate) fn layout(&self) -> &ArticleLayout {
        &self.layout
    }

    pub(crate) fn into_bytes(self) -> B {
        self.bytes
    }
}
