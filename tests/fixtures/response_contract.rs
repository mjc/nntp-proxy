// Shared response-contract fixtures.
//
// This file is intentionally duplicated at the same path in the other
// repository. It
// contains protocol examples only; each repository maps the neutral request
// shape to its own adapter and runs the same cases through production framing.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestShape {
    Article,
    Body,
    Head,
    Stat,
    Group,
    ListGroup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// The complete response must be returned at the exact frame boundary.
    Complete,
    /// The status line is deliberately malformed. nntpbench rejects it;
    /// nntp-proxy retains its existing transparent-forwarding behavior.
    MalformedStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Case {
    pub name: &'static str,
    pub request: RequestShape,
    pub response: &'static [u8],
    pub suffix_request: Option<RequestShape>,
    pub suffix: &'static [u8],
    pub disposition: Disposition,
}

pub const CASES: &[Case] = &[
    Case {
        name: "stat-success-single-line",
        request: RequestShape::Stat,
        response: b"223 1 <stat@test> article exists\r\n",
        suffix_request: None,
        suffix: b"",
        disposition: Disposition::Complete,
    },
    Case {
        name: "article-error-single-line",
        request: RequestShape::Article,
        response: b"430 no article with that message-id\r\n",
        suffix_request: None,
        suffix: b"",
        disposition: Disposition::Complete,
    },
    Case {
        name: "body-empty-multiline",
        request: RequestShape::Body,
        response: b"222 1 <body@test> follows\r\n.\r\n",
        suffix_request: None,
        suffix: b"",
        disposition: Disposition::Complete,
    },
    Case {
        name: "body-dot-stuffed-multiline",
        request: RequestShape::Body,
        response: b"222 1 <body@test> follows\r\n..dot\r\nbody\r\n.\r\n",
        suffix_request: None,
        suffix: b"",
        disposition: Disposition::Complete,
    },
    Case {
        name: "head-folded-multiline",
        request: RequestShape::Head,
        response: b"221 1 <head@test> follows\r\nSubject: first\r\n second\r\n.\r\n",
        suffix_request: None,
        suffix: b"",
        disposition: Disposition::Complete,
    },
    Case {
        name: "group-211-single-line",
        request: RequestShape::Group,
        response: b"211 1 1 1 alt.test\r\n",
        suffix_request: None,
        suffix: b"",
        disposition: Disposition::Complete,
    },
    Case {
        name: "listgroup-211-multiline",
        request: RequestShape::ListGroup,
        response: b"211 1 1 1 alt.test\r\n1\r\n.\r\n",
        suffix_request: None,
        suffix: b"",
        disposition: Disposition::Complete,
    },
    Case {
        name: "packed-body-then-stat",
        request: RequestShape::Body,
        response: b"222 1 <body@test> follows\r\nbody\r\n.\r\n",
        suffix_request: Some(RequestShape::Stat),
        suffix: b"223 2 <stat@test> article exists\r\n",
        disposition: Disposition::Complete,
    },
    Case {
        name: "bare-lf-status",
        request: RequestShape::Body,
        response: b"430 no article with that message-id\n",
        suffix_request: None,
        suffix: b"",
        disposition: Disposition::MalformedStatus,
    },
];
