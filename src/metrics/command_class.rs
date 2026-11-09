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

/// Stripes the counters are spread over. Each thread adds to one stripe, so
/// worker threads rarely share a cache line; a scrape sums all of them.
const STRIPES: usize = 16;

/// One stripe of per-class counters, alone on its cache lines.
#[derive(Debug, Default)]
#[repr(align(128))]
struct Stripe([AtomicU64; CommandClass::COUNT]);

/// Relaxed per-class counters, striped across threads.
///
/// Every client command adds here from whatever worker handles it. A single
/// shared array made every core fight over the same cache line; with one
/// stripe per thread (up to [`STRIPES`]) the add stays an uncontended relaxed
/// increment, and the cost moves to the scrape, which sums the stripes.
#[derive(Debug)]
pub(crate) struct CommandClassCounters(Box<[Stripe; STRIPES]>);

impl Default for CommandClassCounters {
    fn default() -> Self {
        Self(Box::new(std::array::from_fn(|_| Stripe::default())))
    }
}

/// This thread's stripe, assigned round-robin on first use.
fn thread_stripe() -> usize {
    use std::cell::Cell;
    use std::sync::atomic::AtomicUsize;
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    thread_local! {
        static STRIPE: Cell<usize> = const { Cell::new(usize::MAX) };
    }
    STRIPE.with(|stripe| {
        let current = stripe.get();
        if current != usize::MAX {
            return current;
        }
        let assigned = NEXT.fetch_add(1, Ordering::Relaxed) % STRIPES;
        stripe.set(assigned);
        assigned
    })
}

impl CommandClassCounters {
    #[inline]
    pub(crate) fn record(&self, class: CommandClass) {
        self.0[thread_stripe()].0[class.index()].fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> [u64; CommandClass::COUNT] {
        std::array::from_fn(|i| {
            self.0
                .iter()
                .map(|stripe| stripe.0[i].load(Ordering::Relaxed))
                .sum()
        })
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
    fn counts_from_many_threads_sum_across_stripes() {
        let counters = std::sync::Arc::new(CommandClassCounters::default());
        let threads: Vec<_> = (0..STRIPES + 3)
            .map(|_| {
                let counters = std::sync::Arc::clone(&counters);
                std::thread::spawn(move || {
                    for _ in 0..100 {
                        counters.record(CommandClass::Article);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let expected = 100 * (STRIPES as u64 + 3);
        assert_eq!(counters.snapshot()[CommandClass::Article.index()], expected);
    }

    #[test]
    fn stripes_do_not_share_cache_lines() {
        assert!(std::mem::align_of::<Stripe>() >= 128);
        assert!(std::mem::size_of::<Stripe>().is_multiple_of(128));
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
