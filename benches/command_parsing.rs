//! Benchmarks for NNTP command classification
//!
//! This benchmark suite measures command classification performance across:
//! - Different command types (ARTICLE, BODY, HEAD, STAT, etc.)
//! - Message-ID vs numeric article specifications
//! - Real-world command distribution (70% article retrieval, 10% GROUP, etc.)
//!
//! Optimized for 40Gbit line rate with 1000 samples × 100 iterations per benchmark.
//!
//! Run with: cargo bench --bench command_parsing

use divan::{Bencher, black_box};
use nntp_proxy::command::classifier::NntpCommand;

fn main() {
    divan::main();
}

/// Macro to generate benchmark modules for command classification
/// Uses 1000 samples × 100 iterations for stable results on noisy hardware
macro_rules! bench_command {
    ($mod_name:ident, $command:expr) => {
        mod $mod_name {
            use super::*;

            #[divan::bench(name = "classifier", sample_count = 1000, sample_size = 100)]
            fn classifier(bencher: Bencher) {
                bencher.bench(|| black_box(NntpCommand::classify(black_box($command))));
            }
        }
    };
}

// =============================================================================
// Article Retrieval Commands (70% of traffic)
// =============================================================================

bench_command!(article_by_msgid, "ARTICLE <msg123@example.com>");
bench_command!(article_by_number, "ARTICLE 12345");
bench_command!(article_current, "ARTICLE");
bench_command!(body_by_msgid, "BODY <test@news.example.org>");
bench_command!(head_by_msgid, "HEAD <complex.id.12345@server.domain.com>");
bench_command!(stat_by_msgid, "STAT <msg@example.com>");

// =============================================================================
// Navigation Commands (10% of traffic)
// =============================================================================

bench_command!(group_command, "GROUP alt.binaries.test");
bench_command!(next_command, "NEXT");
bench_command!(last_command, "LAST");
bench_command!(listgroup_command, "LISTGROUP alt.test");

// =============================================================================
// Header Retrieval Commands (5% of traffic)
// =============================================================================

bench_command!(over_command, "OVER 100-200");
bench_command!(xover_command, "XOVER 1000-2000");
bench_command!(hdr_command, "HDR Subject 100-200");

// =============================================================================
// Stateless Information Commands (5-10% of traffic)
// =============================================================================

bench_command!(list_command, "LIST ACTIVE");
bench_command!(date_command, "DATE");
bench_command!(capabilities_command, "CAPABILITIES");
bench_command!(help_command, "HELP");
bench_command!(quit_command, "QUIT");

// =============================================================================
// Authentication Commands (once per connection)
// =============================================================================

bench_command!(authinfo_user, "AUTHINFO USER testuser");
bench_command!(authinfo_pass, "AUTHINFO PASS testpass");

// =============================================================================
// Mode Commands
// =============================================================================

bench_command!(mode_reader, "MODE READER");

// =============================================================================
// Posting Commands (rare)
// =============================================================================

bench_command!(post_command, "POST");
bench_command!(ihave_command, "IHAVE <unique@msgid.com>");

// =============================================================================
// Complex Commands
// =============================================================================

bench_command!(
    complex_msgid,
    "ARTICLE <very.long.complex.message.id.with.multiple.segments.12345678@news.server.example.com>"
);
bench_command!(
    long_groupname,
    "GROUP alt.binaries.multimedia.anime.highspeed.repost"
);

// =============================================================================
// Realistic Mixed Workload
// =============================================================================

mod realistic_workload {
    use super::*;

    /// Simulates a realistic distribution of NNTP commands:
    /// - 70% article retrieval (ARTICLE/BODY/HEAD/STAT)
    /// - 10% GROUP navigation
    /// - 10% header retrieval (OVER/XOVER)
    /// - 5% LIST/stateless commands
    /// - 5% other commands
    const COMMANDS: &[&str] = &[
        // 70% article retrieval
        "ARTICLE <msg1@test.com>",
        "BODY <msg2@test.com>",
        "HEAD <msg3@test.com>",
        "STAT <msg4@test.com>",
        "ARTICLE 12345",
        "BODY 12346",
        "HEAD 12347",
        // 10% GROUP
        "GROUP alt.test",
        // 10% header retrieval
        "OVER 100-200",
        // 5% LIST/stateless
        "LIST ACTIVE",
        // 5% other
        "DATE",
        "CAPABILITIES",
        "QUIT",
    ];

    #[divan::bench(name = "classifier", sample_count = 1000, sample_size = 100)]
    fn classifier(bencher: Bencher) {
        bencher
            .counter(divan::counter::ItemsCount::new(COMMANDS.len()))
            .bench(|| {
                for cmd in COMMANDS {
                    black_box(NntpCommand::classify(black_box(cmd)));
                }
            });
    }
}
