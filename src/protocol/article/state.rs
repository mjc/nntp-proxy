//! Resource-bound article states used by the proxy's framed client surface.
//!
//! The session framer has a different physical owner from the standalone
//! client, but the state transitions have the same meaning in both projects:
//! receiving is tied to an active exchange, framing owns one exact response,
//! and validation owns immutable bytes plus its semantic view.

use super::{ArticleLayout, ParseError, YencValidation};
use crate::protocol::{RequestKind, StatusCode};

/// Storage whose bytes remain stable for the lifetime of a validated state.
///
/// This is deliberately crate-private: callers cannot manufacture a
/// validated article from an arbitrary `AsRef<[u8]>` implementation whose
/// result could change between accesses.
pub(crate) trait StableBytes {
    fn as_slice(&self) -> &[u8];
}

impl StableBytes for crate::pool::PooledBuffer {
    fn as_slice(&self) -> &[u8] {
        self.as_ref()
    }
}

/// Exclusive end of the request-scoped status line in a framed response.
/// The coordinate is relative to the bytes carried by the same `Framed` state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StatusLineEnd(usize);

impl StatusLineEnd {
    pub(crate) const fn new(value: usize) -> Self {
        Self(value)
    }

    pub(crate) const fn get(self) -> usize {
        self.0
    }
}

/// An article operation in one of the protocol-owned states.
#[derive(Debug)]
pub(crate) struct Article<State>(State);

impl<State> Article<State> {
    pub(crate) const fn new(state: State) -> Self {
        Self(state)
    }

    pub(crate) const fn as_inner(&self) -> &State {
        &self.0
    }

    pub(crate) fn into_inner(self) -> State {
        self.0
    }
}

impl<B> Article<Framed<B>> {
    pub(crate) const fn kind(&self) -> RequestKind {
        self.0.kind()
    }

    pub(crate) const fn status(&self) -> StatusCode {
        self.0.status()
    }

    pub(crate) const fn status_line_end(&self) -> StatusLineEnd {
        self.0.status_line_end()
    }

    pub(crate) fn into_bytes(self) -> B {
        self.0.into_bytes()
    }
}

/// A complete wire response retained by an owner selected by the adapter.
#[derive(Debug)]
pub(crate) struct Framed<B> {
    bytes: B,
    kind: RequestKind,
    status: StatusCode,
    status_line_end: StatusLineEnd,
}

impl<B> Framed<B> {
    pub(crate) const fn new(
        bytes: B,
        kind: RequestKind,
        status: StatusCode,
        status_line_end: StatusLineEnd,
    ) -> Self {
        Self {
            bytes,
            kind,
            status,
            status_line_end,
        }
    }

    pub(crate) fn bytes(&self) -> &B {
        &self.bytes
    }

    pub(crate) const fn kind(&self) -> RequestKind {
        self.kind
    }

    pub(crate) const fn status(&self) -> StatusCode {
        self.status
    }

    pub(crate) const fn status_line_end(&self) -> StatusLineEnd {
        self.status_line_end
    }

    pub(crate) fn into_bytes(self) -> B {
        self.bytes
    }
}

impl<B: StableBytes> Framed<B> {
    /// Consume the framed state at the semantic-validation boundary.
    ///
    /// Framing proves only the response boundary. This transition performs
    /// article validation while the bytes and their private layout remain in
    /// one state value, so callers cannot validate one allocation and later
    /// bind the result to another.
    pub(crate) fn validate(self, policy: YencValidation) -> Result<Validated<B>, ParseError> {
        let layout =
            ArticleLayout::parse_framed(self.bytes.as_slice(), self.status, self.status_line_end)?;
        match policy {
            YencValidation::Disabled => {}
            YencValidation::Enabled => layout.validate_yenc(self.bytes.as_slice())?,
        }
        Ok(Validated::new(self.bytes, layout))
    }
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

impl<B: StableBytes> Validated<B> {
    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.bytes.as_slice()
    }
}
