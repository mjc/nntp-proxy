//! Typed NNTP request context.
//!
//! A `RequestContext` is created at the validated request boundary. It owns the
//! verb and argument bytes so it can move across backend worker queues without
//! carrying a redundant serialized command buffer.

use smallvec::SmallVec;

use super::{StatusCode, codes};
use crate::types::{BackendId, MessageId};

pub const MAX_COMMAND_LINE_OCTETS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    Article,
    Body,
    Head,
    Stat,
    Group,
    ListGroup,
    Last,
    Next,
    List,
    Date,
    Help,
    Capabilities,
    Mode,
    Quit,
    Over,
    Xover,
    Hdr,
    Xhdr,
    NewGroups,
    NewNews,
    Post,
    Ihave,
    Check,
    TakeThis,
    AuthInfo,
    StartTls,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestRouteClass {
    ArticleByMessageId,
    Stateless,
    Stateful,
    Local,
    Reject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestCacheStatus {
    Hit,
    PartialHit,
    Miss,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct RequestWireLen(usize);

impl RequestWireLen {
    #[must_use]
    pub const fn new(value: usize) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> usize {
        self.0
    }

    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0 as u64
    }
}

impl From<usize> for RequestWireLen {
    fn from(value: usize) -> Self {
        Self::new(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ResponseWireLen(usize);

impl ResponseWireLen {
    #[must_use]
    pub const fn new(value: usize) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> usize {
        self.0
    }
}

impl From<usize> for ResponseWireLen {
    fn from(value: usize) -> Self {
        Self::new(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ResponsePayloadLen(usize);

impl ResponsePayloadLen {
    #[must_use]
    pub(crate) const fn new(value: usize) -> Self {
        Self(value)
    }

    #[must_use]
    pub(crate) const fn get(self) -> usize {
        self.0
    }
}

impl From<usize> for ResponsePayloadLen {
    fn from(value: usize) -> Self {
        Self::new(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestResponseMetadata {
    status: StatusCode,
    wire_len: ResponseWireLen,
}

impl RequestResponseMetadata {
    #[must_use]
    pub(crate) const fn new(status: StatusCode, wire_len: ResponseWireLen) -> Self {
        Self { status, wire_len }
    }

    #[must_use]
    pub(crate) const fn status(self) -> StatusCode {
        self.status
    }

    #[must_use]
    pub(crate) const fn wire_len(self) -> ResponseWireLen {
        self.wire_len
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct RequestCacheAvailability {
    checked: u8,
    missing: u8,
}

impl RequestCacheAvailability {
    #[must_use]
    pub(crate) const fn from_bits(checked: u8, missing: u8) -> Self {
        Self { checked, missing }
    }

    #[must_use]
    pub(crate) const fn checked_bits(self) -> u8 {
        self.checked
    }

    #[must_use]
    pub(crate) const fn missing_bits(self) -> u8 {
        self.missing
    }

    #[must_use]
    pub(crate) fn backend_has_article(self, backend_id: BackendId) -> bool {
        let mask = backend_id.availability_bit();
        self.checked & mask != 0 && self.missing & mask == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct RequestCacheTier(u8);

impl RequestCacheTier {
    #[must_use]
    pub(crate) const fn new(value: u8) -> Self {
        Self(value)
    }
}

impl From<u8> for RequestCacheTier {
    fn from(value: u8) -> Self {
        Self::new(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct RequestCacheTimestampMillis(u64);

impl RequestCacheTimestampMillis {
    #[must_use]
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }
}

impl From<u64> for RequestCacheTimestampMillis {
    fn from(value: u64) -> Self {
        Self::new(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RequestCachePayloadKind {
    Missing,
    AvailabilityOnly,
    Article,
    Head,
    Body,
    Stat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestCacheArticleNumber(u64);

impl RequestCacheArticleNumber {
    #[must_use]
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }
}

impl From<u64> for RequestCacheArticleNumber {
    fn from(value: u64) -> Self {
        Self::new(value)
    }
}

#[derive(Debug)]
pub struct RequestContext {
    kind: RequestKind,
    verb: SmallVec<[u8; 16]>,
    args: SmallVec<[u8; 512]>,
    message_id: Option<(usize, usize)>,
    cache_status: Option<RequestCacheStatus>,
    cache_entry: Option<RequestCacheEntryMetadata>,
    backend_id: Option<BackendId>,
    response: Option<RequestResponseMetadata>,
    response_payload: Option<crate::pool::ChunkedResponse>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RequestLine<'a> {
    kind: RequestKind,
    verb: &'a [u8],
    args: &'a [u8],
    message_id: Option<(usize, usize)>,
}

impl<'a> RequestLine<'a> {
    #[must_use]
    pub fn parse(line: &'a [u8]) -> Self {
        let bytes = trim_line_end(line);
        let split = memchr::memchr(b' ', bytes).unwrap_or(bytes.len());
        let verb = &bytes[..split];
        let args = if split < bytes.len() {
            &bytes[split + 1..]
        } else {
            &[]
        };

        Self {
            kind: classify_verb(verb),
            verb,
            args,
            message_id: find_message_id(args),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> RequestKind {
        self.kind
    }

    #[must_use]
    pub const fn verb(&self) -> &'a [u8] {
        self.verb
    }

    #[must_use]
    pub const fn args(&self) -> &'a [u8] {
        self.args
    }

    #[must_use]
    #[cfg(test)]
    pub fn message_id(&self) -> Option<&str> {
        let (start, end) = self.message_id?;
        std::str::from_utf8(&self.args[start..end]).ok()
    }

    #[must_use]
    #[cfg(test)]
    pub fn message_id_value(&self) -> Option<MessageId<'a>> {
        let (start, end) = self.message_id?;
        MessageId::from_borrowed(std::str::from_utf8(&self.args[start..end]).ok()?).ok()
    }

    #[must_use]
    const fn message_id_span(&self) -> Option<(usize, usize)> {
        self.message_id
    }

    #[must_use]
    #[cfg(test)]
    pub const fn route_class(&self) -> RequestRouteClass {
        route_class(self.kind, self.message_id.is_some())
    }
}

impl Clone for RequestContext {
    fn clone(&self) -> Self {
        debug_assert!(
            self.response_payload.is_none(),
            "completed response payloads are not cloned"
        );
        Self {
            kind: self.kind,
            verb: self.verb.clone(),
            args: self.args.clone(),
            message_id: self.message_id,
            cache_status: self.cache_status,
            cache_entry: self.cache_entry,
            backend_id: self.backend_id,
            response: self.response,
            response_payload: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestCacheEntryMetadata {
    status: StatusCode,
    availability: RequestCacheAvailability,
    tier: RequestCacheTier,
    timestamp: RequestCacheTimestampMillis,
    payload_kind: RequestCachePayloadKind,
    article_number: Option<RequestCacheArticleNumber>,
}

impl RequestCacheEntryMetadata {
    #[must_use]
    pub(crate) const fn new(
        status: StatusCode,
        availability: RequestCacheAvailability,
        tier: RequestCacheTier,
        timestamp: RequestCacheTimestampMillis,
        payload_kind: RequestCachePayloadKind,
        article_number: Option<RequestCacheArticleNumber>,
    ) -> Self {
        Self {
            status,
            availability,
            tier,
            timestamp,
            payload_kind,
            article_number,
        }
    }

    #[must_use]
    pub(crate) const fn status(self) -> StatusCode {
        self.status
    }

    #[must_use]
    pub(crate) const fn availability(self) -> RequestCacheAvailability {
        self.availability
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) const fn tier(self) -> RequestCacheTier {
        self.tier
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) const fn timestamp(self) -> RequestCacheTimestampMillis {
        self.timestamp
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) const fn payload_kind(self) -> RequestCachePayloadKind {
        self.payload_kind
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) const fn article_number(self) -> Option<RequestCacheArticleNumber> {
        self.article_number
    }
}

impl RequestContext {
    #[must_use]
    pub fn parse(line: &[u8]) -> Option<Self> {
        if line.len() > MAX_COMMAND_LINE_OCTETS {
            return None;
        }

        let line = RequestLine::parse(line);
        (!line.verb().is_empty()).then(|| Self::from_request_line(line))
    }

    #[must_use]
    fn from_request_line(line: RequestLine<'_>) -> Self {
        Self::from_parts(
            line.kind(),
            SmallVec::from_slice(line.verb()),
            SmallVec::from_slice(line.args()),
            line.message_id_span(),
        )
    }

    #[must_use]
    pub(crate) fn from_verb_args(verb: &[u8], args: &[u8]) -> Self {
        let verb = SmallVec::from_slice(verb);
        let args = SmallVec::from_slice(args);
        let kind = classify_verb(&verb);
        let message_id = find_message_id(&args);

        Self::from_parts(kind, verb, args, message_id)
    }

    #[must_use]
    pub(crate) fn from_verb_arg_slices(verb: &[u8], args: &[&[u8]]) -> Self {
        let verb = SmallVec::from_slice(verb);
        let arg_len = args.iter().map(|part| part.len()).sum();
        let mut joined_args = SmallVec::<[u8; 512]>::with_capacity(arg_len);
        for part in args {
            joined_args.extend_from_slice(part);
        }
        let kind = classify_verb(&verb);
        let message_id = find_message_id(&joined_args);

        Self::from_parts(kind, verb, joined_args, message_id)
    }

    const fn from_parts(
        kind: RequestKind,
        verb: SmallVec<[u8; 16]>,
        args: SmallVec<[u8; 512]>,
        message_id: Option<(usize, usize)>,
    ) -> Self {
        Self {
            kind,
            verb,
            args,
            message_id,
            cache_status: None,
            cache_entry: None,
            backend_id: None,
            response: None,
            response_payload: None,
        }
    }

    #[inline]
    #[must_use]
    pub const fn kind(&self) -> RequestKind {
        self.kind
    }

    #[inline]
    #[must_use]
    pub const fn backend_id(&self) -> Option<BackendId> {
        self.backend_id
    }

    #[cfg(test)]
    #[inline]
    #[must_use]
    pub(crate) const fn cache_status(&self) -> Option<RequestCacheStatus> {
        self.cache_status
    }

    #[inline]
    #[must_use]
    pub(crate) const fn cache_availability(&self) -> Option<RequestCacheAvailability> {
        match self.cache_entry {
            Some(entry) => Some(entry.availability()),
            None => None,
        }
    }

    #[inline]
    #[must_use]
    pub(crate) fn cache_records_backend_has_article(&self, backend_id: BackendId) -> bool {
        self.cache_availability()
            .is_some_and(|availability| availability.backend_has_article(backend_id))
    }

    #[inline]
    #[must_use]
    pub const fn cache_entry_status(&self) -> Option<StatusCode> {
        match self.cache_entry {
            Some(entry) => Some(entry.status()),
            None => None,
        }
    }

    #[cfg(test)]
    #[inline]
    #[must_use]
    pub(crate) const fn cache_entry_tier(&self) -> Option<RequestCacheTier> {
        match self.cache_entry {
            Some(entry) => Some(entry.tier()),
            None => None,
        }
    }

    #[cfg(test)]
    #[inline]
    #[must_use]
    pub(crate) const fn cache_entry_timestamp(&self) -> Option<RequestCacheTimestampMillis> {
        match self.cache_entry {
            Some(entry) => Some(entry.timestamp()),
            None => None,
        }
    }

    #[cfg(test)]
    #[inline]
    #[must_use]
    pub(crate) const fn cache_payload_kind(&self) -> Option<RequestCachePayloadKind> {
        match self.cache_entry {
            Some(entry) => Some(entry.payload_kind()),
            None => None,
        }
    }

    #[cfg(test)]
    #[inline]
    #[must_use]
    pub(crate) const fn cache_article_number(&self) -> Option<RequestCacheArticleNumber> {
        match self.cache_entry {
            Some(entry) => entry.article_number(),
            None => None,
        }
    }

    #[cfg(test)]
    #[inline]
    #[must_use]
    pub(crate) const fn cache_entry_metadata(&self) -> Option<RequestCacheEntryMetadata> {
        self.cache_entry
    }

    #[inline]
    pub(crate) const fn record_cache_status(&mut self, status: RequestCacheStatus) {
        self.cache_status = Some(status);
    }

    #[inline]
    pub(crate) const fn record_cache_entry_metadata(
        &mut self,
        metadata: RequestCacheEntryMetadata,
    ) {
        self.cache_entry = Some(metadata);
    }

    #[inline]
    #[must_use]
    pub const fn response_status(&self) -> Option<StatusCode> {
        match self.response {
            Some(response) => Some(response.status),
            None => None,
        }
    }

    #[inline]
    #[must_use]
    pub const fn response_wire_len(&self) -> Option<ResponseWireLen> {
        match self.response {
            Some(response) => Some(response.wire_len),
            None => None,
        }
    }

    #[inline]
    #[must_use]
    pub(crate) const fn response_metadata(&self) -> Option<RequestResponseMetadata> {
        self.response
    }

    #[inline]
    #[must_use]
    pub(crate) const fn response_payload_len(&self) -> Option<ResponsePayloadLen> {
        match &self.response_payload {
            Some(response) => Some(ResponsePayloadLen::new(response.len())),
            None => None,
        }
    }

    #[inline]
    pub(crate) fn complete_backend_response(
        &mut self,
        backend_id: BackendId,
        status: StatusCode,
        response: crate::pool::ChunkedResponse,
    ) {
        self.backend_id = Some(backend_id);
        self.response = Some(RequestResponseMetadata::new(status, response.len().into()));
        self.response_payload = Some(response);
    }

    #[inline]
    #[must_use]
    pub(crate) fn response_payload(&self) -> Option<&crate::pool::ChunkedResponse> {
        self.response_payload.as_ref()
    }

    #[inline]
    #[cfg(test)]
    #[must_use]
    pub(crate) fn response_payload_eq(&self, expected: &[u8]) -> Option<bool> {
        let response = self.response_payload()?;
        if response.len() != expected.len() {
            return Some(false);
        }

        let mut offset = 0;
        Some(response.iter_chunks().all(|chunk| {
            let end = offset + chunk.len();
            let matches = expected
                .get(offset..end)
                .is_some_and(|expected_chunk| expected_chunk == chunk);
            offset = end;
            matches
        }))
    }

    #[inline]
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn response_payload_is_empty(&self) -> Option<bool> {
        match &self.response_payload {
            Some(response) => Some(response.is_empty()),
            None => None,
        }
    }

    #[inline]
    pub(crate) const fn record_backend_response(
        &mut self,
        backend_id: BackendId,
        response: RequestResponseMetadata,
    ) {
        self.backend_id = Some(backend_id);
        self.response = Some(response);
    }

    #[inline]
    pub(crate) const fn record_cache_response(&mut self, response: RequestResponseMetadata) {
        self.cache_status = Some(RequestCacheStatus::Hit);
        self.response = Some(response);
    }

    #[inline]
    pub(crate) const fn record_local_response(&mut self, response: RequestResponseMetadata) {
        self.response = Some(response);
    }

    #[inline]
    #[must_use]
    pub fn verb(&self) -> &[u8] {
        &self.verb
    }

    #[inline]
    #[must_use]
    pub fn args(&self) -> &[u8] {
        &self.args
    }

    #[must_use]
    pub fn message_id(&self) -> Option<&str> {
        let (start, end) = self.message_id?;
        std::str::from_utf8(&self.args[start..end]).ok()
    }

    #[must_use]
    pub fn message_id_value(&self) -> Option<MessageId<'_>> {
        let (start, end) = self.message_id?;
        MessageId::from_borrowed(std::str::from_utf8(&self.args[start..end]).ok()?).ok()
    }

    #[must_use]
    pub const fn has_message_id(&self) -> bool {
        self.message_id.is_some()
    }

    #[must_use]
    pub const fn is_stat(&self) -> bool {
        matches!(self.kind, RequestKind::Stat)
    }

    #[must_use]
    pub const fn is_head(&self) -> bool {
        matches!(self.kind, RequestKind::Head)
    }

    #[must_use]
    pub const fn is_unknown_extension(&self) -> bool {
        matches!(self.kind, RequestKind::Unknown)
    }

    #[must_use]
    pub const fn route_class(&self) -> RequestRouteClass {
        route_class(self.kind, self.message_id.is_some())
    }

    #[must_use]
    pub const fn is_pipelineable(&self) -> bool {
        matches!(self.route_class(), RequestRouteClass::ArticleByMessageId)
    }

    #[must_use]
    pub const fn is_large_transfer(&self) -> bool {
        matches!(self.kind, RequestKind::Article | RequestKind::Body) && self.message_id.is_some()
    }

    #[must_use]
    pub fn request_wire_len(&self) -> RequestWireLen {
        (self.verb.len() + usize::from(!self.args.is_empty()) + self.args.len() + 2).into()
    }

    /// Write the typed request as NNTP wire bytes without building a command buffer.
    ///
    /// # Errors
    /// Returns any I/O error from writing the request bytes to `writer`.
    pub async fn write_wire_to<W>(&self, writer: &mut W) -> std::io::Result<()>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        use std::io::IoSlice;

        if self.args().is_empty() {
            let mut slices = [IoSlice::new(self.verb()), IoSlice::new(b"\r\n")];
            crate::io_util::write_all_vectored(writer, &mut slices).await
        } else {
            let mut slices = [
                IoSlice::new(self.verb()),
                IoSlice::new(b" "),
                IoSlice::new(self.args()),
                IoSlice::new(b"\r\n"),
            ];
            crate::io_util::write_all_vectored(writer, &mut slices).await
        }
    }

    #[must_use]
    pub fn expects_multiline_response(&self, status: StatusCode) -> bool {
        let code = status.as_u16();
        if status.is_error() {
            return false;
        }

        matches!(
            (self.kind, code),
            (RequestKind::Article, 220)
                | (RequestKind::Head, 221)
                | (RequestKind::Body, 222)
                | (RequestKind::ListGroup, 211)
                | (RequestKind::Help, 100)
                | (RequestKind::Capabilities, 101)
                | (RequestKind::List, 215)
                | (RequestKind::Over | RequestKind::Xover, 224)
                | (RequestKind::Hdr | RequestKind::Xhdr, 225)
                | (RequestKind::NewNews, 230)
                | (RequestKind::NewGroups, 231)
        ) || matches!(self.kind, RequestKind::Unknown) && status_implies_multiline(code)
    }
}

fn trim_line_end(mut line: &[u8]) -> &[u8] {
    while matches!(line.last(), Some(b'\r' | b'\n')) {
        line = &line[..line.len() - 1];
    }
    line
}

const fn route_class(kind: RequestKind, has_message_id: bool) -> RequestRouteClass {
    match kind {
        RequestKind::Capabilities | RequestKind::Quit | RequestKind::AuthInfo => {
            RequestRouteClass::Local
        }
        RequestKind::Post
        | RequestKind::Ihave
        | RequestKind::Check
        | RequestKind::TakeThis
        | RequestKind::StartTls => RequestRouteClass::Reject,
        RequestKind::Article | RequestKind::Body | RequestKind::Head | RequestKind::Stat
            if has_message_id =>
        {
            RequestRouteClass::ArticleByMessageId
        }
        RequestKind::Article
        | RequestKind::Body
        | RequestKind::Head
        | RequestKind::Stat
        | RequestKind::Group
        | RequestKind::ListGroup
        | RequestKind::Last
        | RequestKind::Next
        | RequestKind::Over
        | RequestKind::Xover
        | RequestKind::Hdr
        | RequestKind::Xhdr
        | RequestKind::Unknown => RequestRouteClass::Stateful,
        RequestKind::List
        | RequestKind::Date
        | RequestKind::Help
        | RequestKind::Mode
        | RequestKind::NewGroups
        | RequestKind::NewNews => RequestRouteClass::Stateless,
    }
}

const fn classify_verb(verb: &[u8]) -> RequestKind {
    macro_rules! classify_verbs {
        ($verb:expr; $($len:literal => { $($lit:literal => $kind:expr),+ $(,)? }),+ $(,)?) => {{
            match $verb.len() {
                $(
                    $len => {
                        $(
                            const _: [(); $len] = [(); $lit.len()];
                        )+
                        $(
                            if $verb.eq_ignore_ascii_case($lit) {
                                $kind
                            } else
                        )+
                        {
                            RequestKind::Unknown
                        }
                    }
                )+
                _ => RequestKind::Unknown,
            }
        }};
    }

    classify_verbs!(verb;
        3 => {
            b"HDR" => RequestKind::Hdr,
        },
        4 => {
            b"BODY" => RequestKind::Body,
            b"DATE" => RequestKind::Date,
            b"HEAD" => RequestKind::Head,
            b"HELP" => RequestKind::Help,
            b"LAST" => RequestKind::Last,
            b"LIST" => RequestKind::List,
            b"MODE" => RequestKind::Mode,
            b"NEXT" => RequestKind::Next,
            b"OVER" => RequestKind::Over,
            b"POST" => RequestKind::Post,
            b"QUIT" => RequestKind::Quit,
            b"STAT" => RequestKind::Stat,
            b"XHDR" => RequestKind::Xhdr,
        },
        5 => {
            b"CHECK" => RequestKind::Check,
            b"GROUP" => RequestKind::Group,
            b"IHAVE" => RequestKind::Ihave,
            b"XOVER" => RequestKind::Xover,
        },
        7 => {
            b"ARTICLE" => RequestKind::Article,
            b"NEWNEWS" => RequestKind::NewNews,
        },
        8 => {
            b"AUTHINFO" => RequestKind::AuthInfo,
            b"STARTTLS" => RequestKind::StartTls,
            b"TAKETHIS" => RequestKind::TakeThis,
        },
        9 => {
            b"LISTGROUP" => RequestKind::ListGroup,
            b"NEWGROUPS" => RequestKind::NewGroups,
        },
        12 => {
            b"CAPABILITIES" => RequestKind::Capabilities,
        },
    )
}

fn find_message_id(args: &[u8]) -> Option<(usize, usize)> {
    let start = args.iter().position(|byte| !byte.is_ascii_whitespace())?;
    let end = args.iter().rposition(|byte| !byte.is_ascii_whitespace())? + 1;
    let trimmed = &args[start..end];

    if !trimmed.starts_with(b"<")
        || !trimmed.ends_with(b">")
        || trimmed[1..trimmed.len() - 1]
            .iter()
            .any(|byte| byte.is_ascii_whitespace())
    {
        return None;
    }

    MessageId::from_borrowed(std::str::from_utf8(trimmed).ok()?).ok()?;
    Some((start, end))
}

const fn status_implies_multiline(code: u16) -> bool {
    matches!(
        code,
        codes::HELP_TEXT
            | codes::CAPABILITY_LIST
            | codes::INFORMATION_FOLLOWS
            | codes::ARTICLE_FOLLOWS
            | codes::HEAD_FOLLOWS
            | codes::BODY_FOLLOWS
            | codes::OVERVIEW_FOLLOWS
            | codes::HEADERS_FOLLOW
            | codes::NEW_ARTICLES_FOLLOW
            | codes::NEW_GROUPS_FOLLOW
            | 282
            | 288
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use std::io::IoSlice;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::AsyncWrite;

    #[derive(Default)]
    struct CountingWriter {
        bytes: Vec<u8>,
        writes: usize,
        vectored_writes: usize,
    }

    impl AsyncWrite for CountingWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.writes += 1;
            self.bytes.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_write_vectored(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            bufs: &[IoSlice<'_>],
        ) -> Poll<std::io::Result<usize>> {
            self.vectored_writes += 1;
            let len = bufs.iter().map(|buf| buf.len()).sum();
            for buf in bufs {
                self.bytes.extend_from_slice(buf);
            }
            Poll::Ready(Ok(len))
        }

        fn is_write_vectored(&self) -> bool {
            true
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn wire(context: &RequestContext) -> Vec<u8> {
        let mut out = Vec::with_capacity(context.request_wire_len().get());
        block_on(context.write_wire_to(&mut out)).unwrap();
        out
    }

    fn request_context(line: &[u8]) -> RequestContext {
        RequestContext::parse(line).expect("valid request line")
    }

    #[test]
    fn typed_request_context_parses_message_id() {
        let ctx = request_context(b"ARTICLE <a@b>\r\n");
        assert_eq!(ctx.kind(), RequestKind::Article);
        assert_eq!(ctx.request_wire_len(), RequestWireLen::new(15));
        assert_eq!(ctx.message_id(), Some("<a@b>"));
        assert_eq!(ctx.cache_status(), None);
        assert_eq!(ctx.cache_availability(), None);
        assert_eq!(ctx.cache_entry_status(), None);
        assert_eq!(ctx.cache_entry_tier(), None);
        assert_eq!(ctx.cache_entry_timestamp(), None);
        assert_eq!(ctx.cache_payload_kind(), None);
        assert_eq!(ctx.cache_article_number(), None);
        assert_eq!(ctx.backend_id(), None);
        assert_eq!(ctx.response_status(), None);
        assert_eq!(ctx.response_wire_len(), None);
        assert!(ctx.is_pipelineable());
        assert_eq!(wire(&ctx), b"ARTICLE <a@b>\r\n");
    }

    #[test]
    fn typed_request_context_uses_vectored_write() {
        let ctx = request_context(b"ARTICLE <a@b>\r\n");
        let mut out = CountingWriter::default();

        block_on(ctx.write_wire_to(&mut out)).unwrap();

        assert_eq!(out.bytes, b"ARTICLE <a@b>\r\n");
        assert_eq!(out.writes, 0);
        assert_eq!(out.vectored_writes, 1);
    }

    #[test]
    fn borrowed_request_line_parses_without_owning_bytes() {
        let bytes = b"ARTICLE <a@b>\r\n";
        let parsed = RequestLine::parse(bytes);

        assert_eq!(parsed.kind(), RequestKind::Article);
        assert_eq!(parsed.verb(), b"ARTICLE");
        assert_eq!(parsed.args(), b"<a@b>");
        assert_eq!(parsed.message_id(), Some("<a@b>"));
        assert_eq!(
            parsed.message_id_value(),
            Some(MessageId::from_borrowed("<a@b>").unwrap())
        );
        assert_eq!(parsed.route_class(), RequestRouteClass::ArticleByMessageId);
    }

    #[test]
    fn borrowed_request_line_requires_exact_message_id_argument() {
        let extra_token = RequestLine::parse(b"ARTICLE 123 <a@b>\r\n");
        let spaced = RequestLine::parse(b"ARTICLE   <a@b>  \r\n");

        assert_eq!(extra_token.message_id(), None);
        assert_eq!(extra_token.route_class(), RequestRouteClass::Stateful);

        assert_eq!(spaced.message_id(), Some("<a@b>"));
        assert_eq!(spaced.route_class(), RequestRouteClass::ArticleByMessageId);
    }

    #[test]
    fn request_context_owns_borrowed_request_line() {
        let parsed = RequestLine::parse(b"BODY <a@b>\r\n");
        let ctx = RequestContext::from_request_line(parsed);

        assert_eq!(ctx.kind(), RequestKind::Body);
        assert_eq!(ctx.verb(), b"BODY");
        assert_eq!(ctx.args(), b"<a@b>");
        assert_eq!(ctx.message_id(), Some("<a@b>"));
        assert_eq!(ctx.request_wire_len(), RequestWireLen::new(12));
    }

    #[test]
    fn request_context_parses_wire_line_at_boundary() {
        let ctx = RequestContext::parse(b"BODY <a@b>\r\n").expect("valid request line");

        assert_eq!(ctx.kind(), RequestKind::Body);
        assert_eq!(ctx.verb(), b"BODY");
        assert_eq!(ctx.args(), b"<a@b>");
        assert_eq!(ctx.message_id(), Some("<a@b>"));
        assert_eq!(ctx.request_wire_len(), RequestWireLen::new(12));
    }

    #[test]
    fn request_context_parse_rejects_empty_request_lines() {
        assert!(RequestContext::parse(b"").is_none());
        assert!(RequestContext::parse(b"\r\n").is_none());
        assert!(RequestContext::parse(b"   \r\n").is_none());
    }

    #[test]
    fn request_context_parse_rejects_oversized_request_lines() {
        let mut line = b"ARTICLE <".to_vec();
        line.extend(std::iter::repeat_n(b'a', 496));
        line.extend_from_slice(b"@example.com>\r\n");

        assert_eq!(line.len(), 520);
        assert!(RequestContext::parse(&line).is_none());
    }

    #[tokio::test]
    async fn request_context_writes_wire_bytes_without_command_buffer() {
        let ctx = request_context(b"BODY <a@b>\r\n");
        let mut out = Vec::new();

        ctx.write_wire_to(&mut out).await.unwrap();

        assert_eq!(out, b"BODY <a@b>\r\n");
    }

    #[test]
    fn typed_request_context_parses_request_bytes() {
        let ctx = request_context(b"XFOO \xff\r\n");

        assert_eq!(ctx.kind(), RequestKind::Unknown);
        assert_eq!(ctx.verb(), b"XFOO");
        assert_eq!(ctx.args(), b"\xff");
        assert_eq!(ctx.route_class(), RequestRouteClass::Stateful);
        assert_eq!(wire(&ctx), b"XFOO \xff\r\n");
    }

    #[test]
    fn request_context_records_backend_response_when_known() {
        let mut ctx = request_context(b"STAT <a@b>\r\n");
        let backend_id = BackendId::from_index(2);
        let status = StatusCode::new(223);

        ctx.record_backend_response(
            backend_id,
            RequestResponseMetadata::new(status, ResponseWireLen::new(19)),
        );

        assert_eq!(ctx.backend_id(), Some(backend_id));
        assert_eq!(
            ctx.response_metadata(),
            Some(RequestResponseMetadata::new(
                status,
                ResponseWireLen::new(19)
            ))
        );
        assert_eq!(ctx.response_status(), Some(status));
        assert_eq!(ctx.response_wire_len(), Some(ResponseWireLen::new(19)));
        assert_eq!(wire(&ctx), b"STAT <a@b>\r\n");
    }

    #[test]
    fn request_context_compares_chunked_response_payload_without_flattening() {
        let mut ctx = request_context(b"STAT <a@b>\r\n");
        let backend_id = BackendId::from_index(2);
        let pool = crate::pool::BufferPool::for_tests();
        let mut response = crate::pool::ChunkedResponse::default();
        response.extend_from_slice(&pool, b"223 0 ");
        response.extend_from_slice(&pool, b"<a@b>\r\n");

        ctx.complete_backend_response(backend_id, StatusCode::new(223), response);

        assert_eq!(
            ctx.response_payload_len(),
            Some(ResponsePayloadLen::new(13))
        );
        assert_eq!(ctx.response_payload_eq(b"223 0 <a@b>\r\n"), Some(true));
        assert_eq!(ctx.response_payload_eq(b"223 0 <other>\r\n"), Some(false));
        assert_eq!(
            ctx.response_payload_eq(b"223 0 <a@b>\r\nextra"),
            Some(false)
        );
    }

    #[test]
    fn request_context_records_cache_status_when_known() {
        let mut ctx = request_context(b"BODY <a@b>\r\n");

        ctx.record_cache_status(RequestCacheStatus::PartialHit);

        assert_eq!(ctx.cache_status(), Some(RequestCacheStatus::PartialHit));
        assert_eq!(wire(&ctx), b"BODY <a@b>\r\n");
    }

    #[test]
    fn request_context_records_cache_entry_metadata_together() {
        let mut ctx = request_context(b"ARTICLE <a@b>\r\n");
        let status = StatusCode::new(430);
        let availability = RequestCacheAvailability::from_bits(0b0000_0010, 0b0000_0010);
        let tier = RequestCacheTier::new(2);
        let timestamp = RequestCacheTimestampMillis::new(123_456);
        let payload_kind = RequestCachePayloadKind::Missing;
        let article_number = Some(RequestCacheArticleNumber::new(42));

        ctx.record_cache_entry_metadata(RequestCacheEntryMetadata::new(
            status,
            availability,
            tier,
            timestamp,
            payload_kind,
            article_number,
        ));

        assert_eq!(
            ctx.cache_entry_metadata(),
            Some(RequestCacheEntryMetadata::new(
                status,
                availability,
                tier,
                timestamp,
                payload_kind,
                article_number
            ))
        );
        assert_eq!(ctx.cache_entry_status(), Some(status));
        assert_eq!(ctx.cache_availability(), Some(availability));
        assert_eq!(ctx.cache_entry_tier(), Some(tier));
        assert_eq!(ctx.cache_entry_timestamp(), Some(timestamp));
        assert_eq!(ctx.cache_payload_kind(), Some(payload_kind));
        assert_eq!(ctx.cache_article_number(), article_number);
        assert_eq!(ctx.response_status(), None);
        assert_eq!(wire(&ctx), b"ARTICLE <a@b>\r\n");
    }

    #[test]
    fn request_context_detects_cached_backend_has_article() {
        let mut ctx = request_context(b"ARTICLE <a@b>\r\n");

        ctx.record_cache_entry_metadata(RequestCacheEntryMetadata::new(
            StatusCode::new(220),
            RequestCacheAvailability::from_bits(0b0000_0110, 0b0000_0010),
            RequestCacheTier::new(0),
            RequestCacheTimestampMillis::new(1),
            RequestCachePayloadKind::AvailabilityOnly,
            None,
        ));

        assert!(!ctx.cache_records_backend_has_article(BackendId::from_index(0)));
        assert!(!ctx.cache_records_backend_has_article(BackendId::from_index(1)));
        assert!(ctx.cache_records_backend_has_article(BackendId::from_index(2)));
    }

    #[test]
    fn request_cache_availability_detects_highest_backend_bit() {
        let availability = RequestCacheAvailability::from_bits(0b1000_0000, 0);

        assert!(availability.backend_has_article(BackendId::from_index(7)));
        assert!(!availability.backend_has_article(BackendId::from_index(6)));
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "Backend index 8 exceeds MAX_BACKENDS")]
    fn request_cache_availability_panics_for_out_of_range_backend() {
        let availability = RequestCacheAvailability::from_bits(0, 0);

        let _ = availability.backend_has_article(BackendId::from_index(8));
    }

    #[test]
    fn request_context_records_cache_response_when_served() {
        let mut ctx = request_context(b"HEAD <a@b>\r\n");
        let status = StatusCode::new(221);

        ctx.record_cache_response(RequestResponseMetadata::new(
            status,
            ResponseWireLen::new(24),
        ));

        assert_eq!(ctx.cache_status(), Some(RequestCacheStatus::Hit));
        assert_eq!(ctx.backend_id(), None);
        assert_eq!(ctx.response_status(), Some(status));
        assert_eq!(ctx.response_wire_len(), Some(ResponseWireLen::new(24)));
        assert_eq!(wire(&ctx), b"HEAD <a@b>\r\n");
    }

    #[test]
    fn request_context_records_local_response_without_backend() {
        let mut ctx = request_context(b"QUIT\r\n");
        let status = StatusCode::new(205);

        ctx.record_local_response(RequestResponseMetadata::new(
            status,
            ResponseWireLen::new(24),
        ));

        assert_eq!(ctx.backend_id(), None);
        assert_eq!(ctx.response_status(), Some(status));
        assert_eq!(ctx.response_wire_len(), Some(ResponseWireLen::new(24)));
        assert_eq!(wire(&ctx), b"QUIT\r\n");
    }

    #[test]
    fn unknown_extensions_are_stateful() {
        let ctx = request_context(b"XFOO arg\r\n");
        assert_eq!(ctx.kind(), RequestKind::Unknown);
        assert_eq!(ctx.route_class(), RequestRouteClass::Stateful);
    }

    #[test]
    fn all_rfc_command_verbs_classify_to_request_kinds() {
        let cases = [
            ("ARTICLE <a@b>\r\n", RequestKind::Article),
            ("BODY <a@b>\r\n", RequestKind::Body),
            ("HEAD <a@b>\r\n", RequestKind::Head),
            ("STAT <a@b>\r\n", RequestKind::Stat),
            ("GROUP alt.test\r\n", RequestKind::Group),
            ("LISTGROUP alt.test\r\n", RequestKind::ListGroup),
            ("LAST\r\n", RequestKind::Last),
            ("NEXT\r\n", RequestKind::Next),
            ("LIST\r\n", RequestKind::List),
            ("DATE\r\n", RequestKind::Date),
            ("HELP\r\n", RequestKind::Help),
            ("CAPABILITIES\r\n", RequestKind::Capabilities),
            ("MODE READER\r\n", RequestKind::Mode),
            ("QUIT\r\n", RequestKind::Quit),
            ("OVER 1-10\r\n", RequestKind::Over),
            ("XOVER 1-10\r\n", RequestKind::Xover),
            ("HDR Subject 1-10\r\n", RequestKind::Hdr),
            ("XHDR Subject 1-10\r\n", RequestKind::Xhdr),
            ("NEWGROUPS 20260101 000000 GMT\r\n", RequestKind::NewGroups),
            ("NEWNEWS * 20260101 000000 GMT\r\n", RequestKind::NewNews),
            ("POST\r\n", RequestKind::Post),
            ("IHAVE <a@b>\r\n", RequestKind::Ihave),
            ("CHECK <a@b>\r\n", RequestKind::Check),
            ("TAKETHIS <a@b>\r\n", RequestKind::TakeThis),
            ("AUTHINFO USER test\r\n", RequestKind::AuthInfo),
            ("STARTTLS\r\n", RequestKind::StartTls),
        ];

        for (line, expected) in cases {
            assert_eq!(
                RequestLine::parse(line.as_bytes()).kind(),
                expected,
                "{line}"
            );
        }
    }

    #[test]
    fn request_route_classes_match_typed_command_behavior() {
        let cases = [
            ("ARTICLE <a@b>\r\n", RequestRouteClass::ArticleByMessageId),
            ("BODY 123\r\n", RequestRouteClass::Stateful),
            ("GROUP alt.test\r\n", RequestRouteClass::Stateful),
            ("LIST\r\n", RequestRouteClass::Stateless),
            ("MODE READER\r\n", RequestRouteClass::Stateless),
            ("QUIT\r\n", RequestRouteClass::Local),
            ("AUTHINFO PASS secret\r\n", RequestRouteClass::Local),
            ("POST\r\n", RequestRouteClass::Reject),
            ("IHAVE <a@b>\r\n", RequestRouteClass::Reject),
            ("CHECK <a@b>\r\n", RequestRouteClass::Reject),
            ("TAKETHIS <a@b>\r\n", RequestRouteClass::Reject),
            ("STARTTLS\r\n", RequestRouteClass::Reject),
            ("XFOO arg\r\n", RequestRouteClass::Stateful),
        ];

        for (line, expected) in cases {
            assert_eq!(
                RequestLine::parse(line.as_bytes()).route_class(),
                expected,
                "{line}"
            );
        }
    }

    #[test]
    fn request_context_derives_multiline_response_expectation() {
        let group = request_context(b"GROUP alt.test\r\n");
        let listgroup = request_context(b"LISTGROUP alt.test\r\n");
        let unknown = request_context(b"XFEATURE TEST\r\n");
        assert!(!group.expects_multiline_response(StatusCode::new(211)));
        assert!(listgroup.expects_multiline_response(StatusCode::new(211)));
        assert!(
            !request_context(b"ARTICLE <x@y>\r\n").expects_multiline_response(StatusCode::new(430))
        );
        assert!(unknown.expects_multiline_response(StatusCode::new(282)));
        assert!(unknown.expects_multiline_response(StatusCode::new(288)));
        assert!(!unknown.expects_multiline_response(StatusCode::new(281)));
    }
}
