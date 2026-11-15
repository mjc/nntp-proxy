//! Bounded command grouping for exported metrics.
//!
//! Exported series are labelled by command class rather than by verb: a client
//! can send any verb, and a label per distinct verb would let one client grow
//! the series count without limit.

use crate::protocol::RequestKind;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

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

/// Stripes the counters are spread over; a scrape sums all of them.
const STRIPES: usize = 16;

/// One stripe of per-class counters, alone on its cache lines.
#[derive(Debug, Default)]
#[repr(align(128))]
struct Stripe([AtomicU64; CommandClass::COUNT]);

/// Relaxed per-class counters, striped across threads.
///
/// Every client command adds here from whatever worker handles it. A single
/// shared array made every core fight over the same cache line; with its own
/// stripe per recording thread the add stays an uncontended relaxed
/// increment, and the cost moves to the scrape, which sums the stripes.
#[derive(Debug)]
pub(crate) struct CommandClassCounters(Box<[Stripe; STRIPES]>);

impl Default for CommandClassCounters {
    fn default() -> Self {
        Self(Box::new(std::array::from_fn(|_| Stripe::default())))
    }
}

/// Stripes currently owned by a live thread, one bit per stripe.
static CLAIMED_STRIPES: AtomicU32 = AtomicU32::new(0);
/// Round-robin fallback once every stripe is owned.
static OVERFLOW_STRIPE: AtomicUsize = AtomicUsize::new(0);

/// A thread's stripe. An owned stripe is released when the thread exits, so a
/// replacement worker can take it.
struct StripeClaim {
    index: usize,
    ownership: StripeOwnership,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StripeOwnership {
    /// This thread alone adds to the stripe.
    Exclusive,
    /// All stripes were owned; this thread shares one round-robin.
    Shared,
}

impl StripeClaim {
    /// Take the lowest free stripe, or share one when all are owned.
    ///
    /// Stripes are claimed on a thread's first recorded command rather than
    /// keyed on a tokio worker index, which tokio does not expose on stable.
    /// Only threads that record take a stripe, and client commands are
    /// recorded from session tasks on the runtime's workers, so with up to
    /// [`STRIPES`] workers no two of them share one; blocking-pool threads
    /// never record and never take a stripe.
    fn claim() -> Self {
        let mut claimed = CLAIMED_STRIPES.load(Ordering::Relaxed);
        loop {
            let free = !claimed & ((1u32 << STRIPES) - 1);
            if free == 0 {
                return Self {
                    index: OVERFLOW_STRIPE.fetch_add(1, Ordering::Relaxed) % STRIPES,
                    ownership: StripeOwnership::Shared,
                };
            }
            let index = free.trailing_zeros() as usize;
            match CLAIMED_STRIPES.compare_exchange_weak(
                claimed,
                claimed | (1 << index),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Self {
                        index,
                        ownership: StripeOwnership::Exclusive,
                    };
                }
                Err(current) => claimed = current,
            }
        }
    }
}

impl Drop for StripeClaim {
    fn drop(&mut self) {
        if self.ownership == StripeOwnership::Exclusive {
            CLAIMED_STRIPES.fetch_and(!(1 << self.index), Ordering::Relaxed);
        }
    }
}

thread_local! {
    static THREAD_STRIPE: StripeClaim = StripeClaim::claim();
}

/// This thread's stripe, claimed on first use.
#[inline]
fn thread_stripe() -> usize {
    // A thread recording during its own teardown, after the claim was
    // dropped, falls back to stripe 0; the count is still kept.
    THREAD_STRIPE.try_with(|claim| claim.index).unwrap_or(0)
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
    fn live_threads_get_distinct_stripes_and_release_them_on_exit() {
        use std::sync::{Arc, Barrier};
        // Other tests may hold stripes; only ask for as many as are free now.
        let free = STRIPES - CLAIMED_STRIPES.load(Ordering::Relaxed).count_ones() as usize;
        let threads = free.min(8);
        let barrier = Arc::new(Barrier::new(threads));
        // Every thread must be running before any is joined, or the barrier
        // would never release.
        let mut handles = Vec::with_capacity(threads);
        for _ in 0..threads {
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                let stripe = thread_stripe();
                // Hold every claim until all threads have one.
                barrier.wait();
                stripe
            }));
        }
        let mut stripes = Vec::with_capacity(threads);
        for handle in handles {
            stripes.push(handle.join().unwrap());
        }
        stripes.sort_unstable();
        stripes.dedup();
        assert_eq!(stripes.len(), threads, "concurrent threads shared a stripe");
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
