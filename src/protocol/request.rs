//! Typed NNTP request context.
//!
//! A `RequestContext` is created after the command line has passed the RFC
//! length/read boundary checks. It owns the verb and argument bytes so it can
//! move across backend worker queues without carrying a redundant full wire
//! command buffer.

use smallvec::SmallVec;

use super::StatusCode;
use crate::types::{BackendId, MessageId};

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
pub enum ResponseShape {
    SingleLine,
    Multiline,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestResponseMetadata {
    status: StatusCode,
    wire_len: ResponseWireLen,
}

impl RequestResponseMetadata {
    #[must_use]
    pub const fn new(status: StatusCode, wire_len: ResponseWireLen) -> Self {
        Self { status, wire_len }
    }

    #[must_use]
    pub fn from_wire_response(response: &[u8]) -> Option<Self> {
        let status = StatusCode::parse(response)?;
        Some(Self::new(status, response.len().into()))
    }

    #[must_use]
    pub const fn status(self) -> StatusCode {
        self.status
    }

    #[must_use]
    pub const fn wire_len(self) -> ResponseWireLen {
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
    pub const fn from_bits(checked: u8, missing: u8) -> Self {
        Self { checked, missing }
    }

    #[must_use]
    pub const fn checked_bits(self) -> u8 {
        self.checked
    }

    #[must_use]
    pub const fn missing_bits(self) -> u8 {
        self.missing
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct RequestCacheTier(u8);

impl RequestCacheTier {
    #[must_use]
    pub const fn new(value: u8) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
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
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
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
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
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
    pub const fn new(
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
    pub const fn status(self) -> StatusCode {
        self.status
    }

    #[must_use]
    pub const fn availability(self) -> RequestCacheAvailability {
        self.availability
    }

    #[must_use]
    pub const fn tier(self) -> RequestCacheTier {
        self.tier
    }

    #[must_use]
    pub const fn timestamp(self) -> RequestCacheTimestampMillis {
        self.timestamp
    }

    #[must_use]
    pub const fn payload_kind(self) -> RequestCachePayloadKind {
        self.payload_kind
    }

    #[must_use]
    pub const fn article_number(self) -> Option<RequestCacheArticleNumber> {
        self.article_number
    }
}

impl RequestContext {
    #[must_use]
    pub fn from_request_line(line: &str) -> Self {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        let bytes = trimmed.as_bytes();
        let split = memchr::memchr(b' ', bytes).unwrap_or(bytes.len());
        let verb = SmallVec::from_slice(&bytes[..split]);
        let args = if split < bytes.len() {
            SmallVec::from_slice(&bytes[split + 1..])
        } else {
            SmallVec::new()
        };
        let kind = classify_verb(&verb);
        let message_id = find_message_id(&args);

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

    #[must_use]
    pub fn from_verb_args(verb: &[u8], args: &[u8]) -> Self {
        let verb = SmallVec::from_slice(verb);
        let args = SmallVec::from_slice(args);
        let kind = classify_verb(&verb);
        let message_id = find_message_id(&args);

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

    #[inline]
    #[must_use]
    pub const fn cache_status(&self) -> Option<RequestCacheStatus> {
        self.cache_status
    }

    #[inline]
    #[must_use]
    pub const fn cache_availability(&self) -> Option<RequestCacheAvailability> {
        match self.cache_entry {
            Some(entry) => Some(entry.availability()),
            None => None,
        }
    }

    #[inline]
    #[must_use]
    pub const fn cache_entry_status(&self) -> Option<StatusCode> {
        match self.cache_entry {
            Some(entry) => Some(entry.status()),
            None => None,
        }
    }

    #[inline]
    #[must_use]
    pub const fn cache_entry_tier(&self) -> Option<RequestCacheTier> {
        match self.cache_entry {
            Some(entry) => Some(entry.tier()),
            None => None,
        }
    }

    #[inline]
    #[must_use]
    pub const fn cache_entry_timestamp(&self) -> Option<RequestCacheTimestampMillis> {
        match self.cache_entry {
            Some(entry) => Some(entry.timestamp()),
            None => None,
        }
    }

    #[inline]
    #[must_use]
    pub const fn cache_payload_kind(&self) -> Option<RequestCachePayloadKind> {
        match self.cache_entry {
            Some(entry) => Some(entry.payload_kind()),
            None => None,
        }
    }

    #[inline]
    #[must_use]
    pub const fn cache_article_number(&self) -> Option<RequestCacheArticleNumber> {
        match self.cache_entry {
            Some(entry) => entry.article_number(),
            None => None,
        }
    }

    #[inline]
    #[must_use]
    pub const fn cache_entry_metadata(&self) -> Option<RequestCacheEntryMetadata> {
        self.cache_entry
    }

    #[inline]
    pub const fn record_cache_status(&mut self, status: RequestCacheStatus) {
        self.cache_status = Some(status);
    }

    #[inline]
    pub const fn record_cache_entry_metadata(&mut self, metadata: RequestCacheEntryMetadata) {
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
    pub const fn response_metadata(&self) -> Option<RequestResponseMetadata> {
        self.response
    }

    #[inline]
    #[must_use]
    pub const fn response_payload_len(&self) -> Option<usize> {
        match &self.response_payload {
            Some(response) => Some(response.len()),
            None => None,
        }
    }

    #[inline]
    pub fn complete_backend_response(
        &mut self,
        backend_id: BackendId,
        status: StatusCode,
        response: crate::pool::ChunkedResponse,
    ) {
        self.backend_id = Some(backend_id);
        self.response = Some(RequestResponseMetadata::new(status, response.len().into()));
        self.response_payload = Some(response);
    }

    pub async fn write_response_payload_to<W>(&self, writer: &mut W) -> std::io::Result<()>
    where
        W: tokio::io::AsyncWriteExt + Unpin,
    {
        self.response_payload
            .as_ref()
            .expect("completed request context carries response payload")
            .write_all_to(writer)
            .await
    }

    #[inline]
    #[must_use]
    pub fn response_payload_to_vec(&self) -> Option<Vec<u8>> {
        self.response_payload
            .as_ref()
            .map(crate::pool::ChunkedResponse::to_vec)
    }

    #[inline]
    #[must_use]
    pub const fn response_payload_is_empty(&self) -> Option<bool> {
        match &self.response_payload {
            Some(response) => Some(response.is_empty()),
            None => None,
        }
    }

    #[inline]
    pub const fn record_backend_response(
        &mut self,
        backend_id: BackendId,
        response: RequestResponseMetadata,
    ) {
        self.backend_id = Some(backend_id);
        self.response = Some(response);
    }

    #[inline]
    pub const fn record_cache_response(
        &mut self,
        backend_id: BackendId,
        response: RequestResponseMetadata,
    ) {
        self.cache_status = Some(RequestCacheStatus::Hit);
        self.backend_id = Some(backend_id);
        self.response = Some(response);
    }

    #[inline]
    pub const fn record_local_response(&mut self, response: RequestResponseMetadata) {
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
    pub fn args_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.args).ok()
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
    pub fn route_class(&self) -> RequestRouteClass {
        match self.kind {
            RequestKind::Capabilities | RequestKind::Quit | RequestKind::AuthInfo => {
                RequestRouteClass::Local
            }
            RequestKind::Post
            | RequestKind::Ihave
            | RequestKind::Check
            | RequestKind::TakeThis
            | RequestKind::StartTls => RequestRouteClass::Reject,
            RequestKind::Article | RequestKind::Body | RequestKind::Head | RequestKind::Stat
                if self.message_id.is_some() =>
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

    #[must_use]
    pub fn is_pipelineable(&self) -> bool {
        matches!(self.route_class(), RequestRouteClass::ArticleByMessageId)
    }

    #[must_use]
    pub fn is_large_transfer(&self) -> bool {
        matches!(self.kind, RequestKind::Article | RequestKind::Body) && self.message_id.is_some()
    }

    #[must_use]
    pub fn wire_len(&self) -> usize {
        self.verb.len() + usize::from(!self.args.is_empty()) + self.args.len() + 2
    }

    #[must_use]
    pub fn response_shape(&self, status: StatusCode) -> ResponseShape {
        let code = status.as_u16();
        if status.is_error() {
            return ResponseShape::SingleLine;
        }

        let multiline = matches!(
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
        );
        if multiline {
            ResponseShape::Multiline
        } else {
            ResponseShape::SingleLine
        }
    }
}

fn classify_verb(verb: &[u8]) -> RequestKind {
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
    let start = args.iter().position(|b| *b == b'<')?;
    let end = args[start..].iter().position(|b| *b == b'>')? + start + 1;
    Some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(context: &RequestContext) -> Vec<u8> {
        let mut out = Vec::with_capacity(context.wire_len());
        out.extend_from_slice(context.verb());
        if !context.args().is_empty() {
            out.push(b' ');
            out.extend_from_slice(context.args());
        }
        out.extend_from_slice(b"\r\n");
        out
    }

    #[test]
    fn typed_request_context_parses_message_id() {
        let ctx = RequestContext::from_request_line("ARTICLE <a@b>\r\n");
        assert_eq!(ctx.kind(), RequestKind::Article);
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
    fn request_context_records_backend_response_when_known() {
        let mut ctx = RequestContext::from_request_line("STAT <a@b>\r\n");
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
    fn request_context_records_cache_status_when_known() {
        let mut ctx = RequestContext::from_request_line("BODY <a@b>\r\n");

        ctx.record_cache_status(RequestCacheStatus::PartialHit);

        assert_eq!(ctx.cache_status(), Some(RequestCacheStatus::PartialHit));
        assert_eq!(wire(&ctx), b"BODY <a@b>\r\n");
    }

    #[test]
    fn request_context_records_cache_entry_metadata_together() {
        let mut ctx = RequestContext::from_request_line("ARTICLE <a@b>\r\n");
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
    fn request_context_records_cache_response_when_served() {
        let mut ctx = RequestContext::from_request_line("HEAD <a@b>\r\n");
        let backend_id = BackendId::from_index(1);
        let status = StatusCode::new(221);

        ctx.record_cache_response(
            backend_id,
            RequestResponseMetadata::new(status, ResponseWireLen::new(24)),
        );

        assert_eq!(ctx.cache_status(), Some(RequestCacheStatus::Hit));
        assert_eq!(ctx.backend_id(), Some(backend_id));
        assert_eq!(ctx.response_status(), Some(status));
        assert_eq!(ctx.response_wire_len(), Some(ResponseWireLen::new(24)));
        assert_eq!(wire(&ctx), b"HEAD <a@b>\r\n");
    }

    #[test]
    fn request_context_records_local_response_without_backend() {
        let mut ctx = RequestContext::from_request_line("QUIT\r\n");
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
        let ctx = RequestContext::from_request_line("XFOO arg\r\n");
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
                RequestContext::from_request_line(line).kind(),
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
                RequestContext::from_request_line(line).route_class(),
                expected,
                "{line}"
            );
        }
    }

    #[test]
    fn request_aware_response_shape() {
        let group = RequestContext::from_request_line("GROUP alt.test\r\n");
        let listgroup = RequestContext::from_request_line("LISTGROUP alt.test\r\n");
        assert_eq!(
            group.response_shape(StatusCode::new(211)),
            ResponseShape::SingleLine
        );
        assert_eq!(
            listgroup.response_shape(StatusCode::new(211)),
            ResponseShape::Multiline
        );
        assert_eq!(
            RequestContext::from_request_line("ARTICLE <x@y>\r\n")
                .response_shape(StatusCode::new(430)),
            ResponseShape::SingleLine
        );
    }
}
