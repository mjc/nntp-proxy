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

impl StableBytes for bytes::Bytes {
    fn as_slice(&self) -> &[u8] {
        self.as_ref()
    }
}

impl StableBytes for &[u8] {
    fn as_slice(&self) -> &[u8] {
        self
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

/// Exclusive end of the framed wire content, relative to the bytes carried
/// by the same `Framed` state. This is distinct from the status-line end so a
/// validator cannot accidentally inspect a packed-response suffix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContentEnd(usize);

impl ContentEnd {
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

// These projections are consumed by the public `client::FramedArticle` facade.
// The generic state is crate-private, so dead-code analysis cannot see those
// external callers.
#[allow(dead_code)]
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

    pub(crate) const fn content_end(&self) -> ContentEnd {
        self.0.content_end()
    }

    pub(crate) fn as_bytes(&self) -> &[u8]
    where
        B: StableBytes,
    {
        self.0.bytes().as_slice()
    }

    pub(crate) fn into_bytes(self) -> B {
        self.0.into_bytes()
    }
}

impl<B: StableBytes> Article<Framed<B>> {
    /// Consume the framed article at the semantic-validation boundary while
    /// retaining the same owner in the stronger state.
    pub(crate) fn validate(
        self,
        policy: YencValidation,
    ) -> Result<Article<Validated<B>>, ParseError> {
        self.into_inner().validate(policy).map(Article::new)
    }
}

/// A complete wire response retained by an owner selected by the adapter.
#[derive(Debug)]
pub(crate) struct Framed<B> {
    bytes: B,
    kind: RequestKind,
    status: StatusCode,
    status_line_end: StatusLineEnd,
    content_end: ContentEnd,
}

impl<B> Framed<B> {
    pub(crate) const fn new(
        bytes: B,
        kind: RequestKind,
        status: StatusCode,
        status_line_end: StatusLineEnd,
        content_end: ContentEnd,
    ) -> Self {
        Self {
            bytes,
            kind,
            status,
            status_line_end,
            content_end,
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

    pub(crate) const fn content_end(&self) -> ContentEnd {
        self.content_end
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
        let Self {
            bytes,
            kind,
            status,
            status_line_end,
            content_end,
        } = self;
        let layout =
            ArticleLayout::parse_framed(bytes.as_slice(), status, status_line_end, content_end)?;
        match policy {
            YencValidation::Disabled => {}
            YencValidation::Enabled => layout.validate_yenc(bytes.as_slice())?,
        }
        Ok(Validated::new(bytes, kind, status, layout))
    }
}

/// Semantic article proof tied to the immutable storage it describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Validated<B> {
    bytes: B,
    kind: RequestKind,
    status: StatusCode,
    layout: ArticleLayout,
}

impl<B> Validated<B> {
    pub(crate) const fn new(
        bytes: B,
        kind: RequestKind,
        status: StatusCode,
        layout: ArticleLayout,
    ) -> Self {
        Self {
            bytes,
            kind,
            status,
            layout,
        }
    }

    pub(crate) const fn kind(&self) -> RequestKind {
        self.kind
    }

    pub(crate) const fn status(&self) -> StatusCode {
        self.status
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

impl<B: StableBytes> Article<Validated<B>> {
    pub(crate) const fn kind(&self) -> RequestKind {
        self.0.kind()
    }

    pub(crate) const fn status(&self) -> StatusCode {
        self.0.status()
    }

    pub(crate) fn article(&self) -> super::Article<'_> {
        self.0.layout().view(self.0.as_bytes())
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    pub(crate) fn into_bytes(self) -> B {
        self.0.into_bytes()
    }
}

// Compile-contract controls exercise the actual owner/view relationship. The
// negative variants must fail at the borrow boundary before a validated view
// can outlive or mutate the bytes that establish its layout.
#[cfg(response_contract)]
#[allow(dead_code)]
mod contracts {
    use super::*;

    fn valid_body() -> (bytes::Bytes, ArticleLayout) {
        let bytes = bytes::Bytes::from_static(b"222 1 <body@test> follows\r\nbody\r\n");
        let layout = ArticleLayout::parse(bytes.as_ref()).expect("fixture is valid");
        (bytes, layout)
    }

    fn validated_borrow_is_reusable() {
        let (bytes, layout) = valid_body();
        let article = Article::new(Validated::new(
            bytes.as_ref(),
            RequestKind::Body,
            StatusCode::new(222),
            layout,
        ));
        let _first = article.article();
        let _second = article.article();
    }

    #[cfg(response_contract = "validated_view_mutation")]
    fn validated_view_mutation() {
        let mut bytes = b"222 1 <body@test> follows\r\nbody\r\n".to_vec();
        let layout = ArticleLayout::parse(&bytes).expect("fixture is valid");
        let article = Article::new(Validated::new(
            &bytes[..],
            RequestKind::Body,
            StatusCode::new(222),
            layout,
        ));
        let view = article.article();
        bytes.clear();
        std::hint::black_box(view);
    }

    #[cfg(response_contract = "validated_storage_reuse")]
    fn validated_storage_reuse() {
        let (bytes, layout) = valid_body();
        let article = Article::new(Validated::new(
            bytes,
            RequestKind::Body,
            StatusCode::new(222),
            layout,
        ));
        let view = article.article();
        let _reused_storage = article.into_bytes();
        std::hint::black_box(view);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn article_state_wrapper_adds_no_storage_to_framed_or_validated_owner() {
        assert_eq!(
            std::mem::size_of::<Article<Framed<crate::pool::PooledBuffer>>>(),
            std::mem::size_of::<Framed<crate::pool::PooledBuffer>>()
        );
        assert_eq!(
            std::mem::size_of::<Article<Validated<crate::pool::PooledBuffer>>>(),
            std::mem::size_of::<Validated<crate::pool::PooledBuffer>>()
        );
        assert_eq!(
            std::mem::size_of::<Article<Framed<crate::pool::ChunkedResponse>>>(),
            std::mem::size_of::<Framed<crate::pool::ChunkedResponse>>()
        );
    }

    #[test]
    fn borrowed_bytes_can_carry_a_validated_article_view() {
        let bytes: &[u8] = b"222 1 <body@test> follows\r\nbody\r\n";
        let layout = ArticleLayout::parse(bytes).expect("fixture is valid");
        let article = Article::new(Validated::new(
            bytes,
            RequestKind::Body,
            StatusCode::new(222),
            layout,
        ));

        assert_eq!(article.article().body, Some(&b"body\r\n"[..]));
    }

    #[test]
    fn borrowed_framed_owner_validates_without_detaching_its_layout() {
        let bytes: &[u8] = b"222 1 <body@test> follows\r\nbody\r\n";
        let status_line_end = StatusLineEnd::new(b"222 1 <body@test> follows\r\n".len());
        let framed = Article::new(Framed::new(
            bytes,
            RequestKind::Body,
            StatusCode::new(222),
            status_line_end,
            ContentEnd::new(bytes.len()),
        ));

        let validated = framed
            .validate(YencValidation::Disabled)
            .expect("valid article");
        assert_eq!(validated.article().body, Some(&b"body\r\n"[..]));
        assert_eq!(validated.as_bytes(), bytes);
    }
}
