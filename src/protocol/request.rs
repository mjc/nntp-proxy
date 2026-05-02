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
    AuthInfo,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestContext {
    kind: RequestKind,
    verb: SmallVec<[u8; 16]>,
    args: SmallVec<[u8; 512]>,
    message_id: Option<(usize, usize)>,
    cache_status: Option<RequestCacheStatus>,
    cache_availability: Option<RequestCacheAvailability>,
    cache_entry_status: Option<StatusCode>,
    backend_id: Option<BackendId>,
    response_status: Option<StatusCode>,
    response_wire_len: Option<ResponseWireLen>,
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
            cache_availability: None,
            cache_entry_status: None,
            backend_id: None,
            response_status: None,
            response_wire_len: None,
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
            cache_availability: None,
            cache_entry_status: None,
            backend_id: None,
            response_status: None,
            response_wire_len: None,
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
        self.cache_availability
    }

    #[inline]
    #[must_use]
    pub const fn cache_entry_status(&self) -> Option<StatusCode> {
        self.cache_entry_status
    }

    #[inline]
    pub const fn record_cache_status(&mut self, status: RequestCacheStatus) {
        self.cache_status = Some(status);
    }

    #[inline]
    pub const fn record_cache_availability(&mut self, availability: RequestCacheAvailability) {
        self.cache_availability = Some(availability);
    }

    #[inline]
    pub const fn record_cache_entry_status(&mut self, status: StatusCode) {
        self.cache_entry_status = Some(status);
    }

    #[inline]
    pub const fn record_cache_entry_metadata(
        &mut self,
        status: StatusCode,
        availability: RequestCacheAvailability,
    ) {
        self.cache_entry_status = Some(status);
        self.cache_availability = Some(availability);
    }

    #[inline]
    #[must_use]
    pub const fn response_status(&self) -> Option<StatusCode> {
        self.response_status
    }

    #[inline]
    #[must_use]
    pub const fn response_wire_len(&self) -> Option<ResponseWireLen> {
        self.response_wire_len
    }

    #[inline]
    pub const fn record_backend_response(
        &mut self,
        backend_id: BackendId,
        status: StatusCode,
        wire_len: ResponseWireLen,
    ) {
        self.backend_id = Some(backend_id);
        self.response_status = Some(status);
        self.response_wire_len = Some(wire_len);
    }

    #[inline]
    pub const fn record_cache_response(
        &mut self,
        backend_id: BackendId,
        status: StatusCode,
        wire_len: ResponseWireLen,
    ) {
        self.cache_status = Some(RequestCacheStatus::Hit);
        self.cache_entry_status = Some(status);
        self.backend_id = Some(backend_id);
        self.response_status = Some(status);
        self.response_wire_len = Some(wire_len);
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
            RequestKind::Post | RequestKind::Ihave => RequestRouteClass::Reject,
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
    macro_rules! is {
        ($lit:literal) => {
            verb.eq_ignore_ascii_case($lit)
        };
    }
    if is!(b"ARTICLE") {
        RequestKind::Article
    } else if is!(b"BODY") {
        RequestKind::Body
    } else if is!(b"HEAD") {
        RequestKind::Head
    } else if is!(b"STAT") {
        RequestKind::Stat
    } else if is!(b"GROUP") {
        RequestKind::Group
    } else if is!(b"LISTGROUP") {
        RequestKind::ListGroup
    } else if is!(b"LAST") {
        RequestKind::Last
    } else if is!(b"NEXT") {
        RequestKind::Next
    } else if is!(b"LIST") {
        RequestKind::List
    } else if is!(b"DATE") {
        RequestKind::Date
    } else if is!(b"HELP") {
        RequestKind::Help
    } else if is!(b"CAPABILITIES") {
        RequestKind::Capabilities
    } else if is!(b"MODE") {
        RequestKind::Mode
    } else if is!(b"QUIT") {
        RequestKind::Quit
    } else if is!(b"OVER") {
        RequestKind::Over
    } else if is!(b"XOVER") {
        RequestKind::Xover
    } else if is!(b"HDR") {
        RequestKind::Hdr
    } else if is!(b"XHDR") {
        RequestKind::Xhdr
    } else if is!(b"NEWGROUPS") {
        RequestKind::NewGroups
    } else if is!(b"NEWNEWS") {
        RequestKind::NewNews
    } else if is!(b"POST") {
        RequestKind::Post
    } else if is!(b"IHAVE") {
        RequestKind::Ihave
    } else if is!(b"AUTHINFO") {
        RequestKind::AuthInfo
    } else {
        RequestKind::Unknown
    }
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

        ctx.record_backend_response(backend_id, status, ResponseWireLen::new(19));

        assert_eq!(ctx.backend_id(), Some(backend_id));
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
    fn request_context_records_cache_availability_when_known() {
        let mut ctx = RequestContext::from_request_line("ARTICLE <a@b>\r\n");
        let availability = RequestCacheAvailability::from_bits(0b0000_0011, 0b0000_0001);

        ctx.record_cache_availability(availability);

        assert_eq!(ctx.cache_availability(), Some(availability));
        assert_eq!(availability.checked_bits(), 0b0000_0011);
        assert_eq!(availability.missing_bits(), 0b0000_0001);
        assert_eq!(wire(&ctx), b"ARTICLE <a@b>\r\n");
    }

    #[test]
    fn request_context_records_cache_entry_status_when_known() {
        let mut ctx = RequestContext::from_request_line("ARTICLE <a@b>\r\n");
        let status = StatusCode::new(430);

        ctx.record_cache_entry_status(status);

        assert_eq!(ctx.cache_entry_status(), Some(status));
        assert_eq!(ctx.response_status(), None);
        assert_eq!(wire(&ctx), b"ARTICLE <a@b>\r\n");
    }

    #[test]
    fn request_context_records_cache_entry_metadata_together() {
        let mut ctx = RequestContext::from_request_line("ARTICLE <a@b>\r\n");
        let status = StatusCode::new(430);
        let availability = RequestCacheAvailability::from_bits(0b0000_0010, 0b0000_0010);

        ctx.record_cache_entry_metadata(status, availability);

        assert_eq!(ctx.cache_entry_status(), Some(status));
        assert_eq!(ctx.cache_availability(), Some(availability));
        assert_eq!(ctx.response_status(), None);
        assert_eq!(wire(&ctx), b"ARTICLE <a@b>\r\n");
    }

    #[test]
    fn request_context_records_cache_response_when_served() {
        let mut ctx = RequestContext::from_request_line("HEAD <a@b>\r\n");
        let backend_id = BackendId::from_index(1);
        let status = StatusCode::new(221);

        ctx.record_cache_response(backend_id, status, ResponseWireLen::new(24));

        assert_eq!(ctx.cache_status(), Some(RequestCacheStatus::Hit));
        assert_eq!(ctx.backend_id(), Some(backend_id));
        assert_eq!(ctx.response_status(), Some(status));
        assert_eq!(ctx.response_wire_len(), Some(ResponseWireLen::new(24)));
        assert_eq!(wire(&ctx), b"HEAD <a@b>\r\n");
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
            ("AUTHINFO USER test\r\n", RequestKind::AuthInfo),
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
