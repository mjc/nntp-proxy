//! Bounded command grouping for exported metrics.
//!
//! Exported series are labelled by command class rather than by verb: a client
//! can send any verb, and a label per distinct verb would let one client grow
//! the series count without limit.

use crate::protocol::RequestKind;
use std::sync::atomic::{AtomicU64, Ordering};

/// Command group used as the `command` label on exported metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandClass {
    Article,
    Body,
    Head,
    Stat,
    /// Group selection and article-number navigation.
    Group,
    /// Overview and header listings (OVER/XOVER/HDR/XHDR).
    Overview,
    /// Group and article listings (LIST/NEWGROUPS/NEWNEWS).
    List,
    /// Posting and transit (POST/IHAVE/CHECK/TAKETHIS).
    Post,
    /// Authentication and TLS negotiation.
    Auth,
    /// Session housekeeping (DATE/HELP/CAPABILITIES/MODE/QUIT/COMPRESS).
    Session,
    /// Verbs the parser does not recognise.
    Other,
    /// Lines the proxy refused before classifying them: over the RFC 3977
    /// length limit or not a valid command line.
    Rejected,
}

impl CommandClass {
    pub const ALL: [Self; 12] = [
        Self::Article,
        Self::Body,
        Self::Head,
        Self::Stat,
        Self::Group,
        Self::Overview,
        Self::List,
        Self::Post,
        Self::Auth,
        Self::Session,
        Self::Other,
        Self::Rejected,
    ];
    pub const COUNT: usize = Self::ALL.len();

    #[must_use]
    pub const fn from_kind(kind: RequestKind) -> Self {
        match kind {
            RequestKind::Article => Self::Article,
            RequestKind::Body => Self::Body,
            RequestKind::Head => Self::Head,
            RequestKind::Stat => Self::Stat,
            RequestKind::Group | RequestKind::ListGroup | RequestKind::Last | RequestKind::Next => {
                Self::Group
            }
            RequestKind::Over | RequestKind::Xover | RequestKind::Hdr | RequestKind::Xhdr => {
                Self::Overview
            }
            RequestKind::List | RequestKind::NewGroups | RequestKind::NewNews => Self::List,
            RequestKind::Post | RequestKind::Ihave | RequestKind::Check | RequestKind::TakeThis => {
                Self::Post
            }
            RequestKind::AuthInfo | RequestKind::StartTls => Self::Auth,
            RequestKind::Date
            | RequestKind::Help
            | RequestKind::Capabilities
            | RequestKind::Mode
            | RequestKind::Quit
            | RequestKind::Compress
            | RequestKind::XfeatureCompressGzip => Self::Session,
            RequestKind::Unknown => Self::Other,
        }
    }

    /// Label value used in the exposition format.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Article => "article",
            Self::Body => "body",
            Self::Head => "head",
            Self::Stat => "stat",
            Self::Group => "group",
            Self::Overview => "overview",
            Self::List => "list",
            Self::Post => "post",
            Self::Auth => "auth",
            Self::Session => "session",
            Self::Other => "other",
            Self::Rejected => "rejected",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

/// One relaxed counter per command class.
///
/// A fixed array indexed by the enum keeps recording to a single atomic add
/// with no hashing or allocation.
#[derive(Debug, Default)]
pub(crate) struct CommandClassCounters([AtomicU64; CommandClass::COUNT]);

impl CommandClassCounters {
    #[inline]
    pub(crate) fn record(&self, class: CommandClass) {
        self.0[class.index()].fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> [u64; CommandClass::COUNT] {
        std::array::from_fn(|i| self.0[i].load(Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_lists_every_class_at_its_own_index() {
        for (i, class) in CommandClass::ALL.iter().enumerate() {
            assert_eq!(class.index(), i, "{class:?}");
        }
    }

    #[test]
    fn article_family_kinds_keep_their_own_class() {
        assert_eq!(
            CommandClass::from_kind(RequestKind::Article),
            CommandClass::Article
        );
        assert_eq!(
            CommandClass::from_kind(RequestKind::Body),
            CommandClass::Body
        );
        assert_eq!(
            CommandClass::from_kind(RequestKind::Head),
            CommandClass::Head
        );
        assert_eq!(
            CommandClass::from_kind(RequestKind::Stat),
            CommandClass::Stat
        );
        assert_eq!(
            CommandClass::from_kind(RequestKind::Xover),
            CommandClass::Overview
        );
        assert_eq!(
            CommandClass::from_kind(RequestKind::Unknown),
            CommandClass::Other
        );
    }

    #[test]
    fn counters_record_by_class() {
        let counters = CommandClassCounters::default();
        counters.record(CommandClass::Body);
        counters.record(CommandClass::Body);
        counters.record(CommandClass::Stat);
        let snapshot = counters.snapshot();
        assert_eq!(snapshot[CommandClass::Body.index()], 2);
        assert_eq!(snapshot[CommandClass::Stat.index()], 1);
        assert_eq!(snapshot[CommandClass::Article.index()], 0);
    }
}
