//! Benchmarks comparing old string-based command classification vs new type-safe parser
//!
//! This benchmark suite measures:
//! - Parsing performance across different command types
//! - Memory allocation patterns
//! - Real-world command distribution (70% article retrieval, 10% GROUP, etc.)
//!
//! Run with: cargo bench --bench command_parsing

use divan::{black_box, Bencher};
use nntp_proxy::command::classifier::NntpCommand;
use nntp_proxy::protocol::parser::parse_command;

fn main() {
    divan::main();
}

// =============================================================================
// Article Retrieval Commands (70% of traffic)
// =============================================================================

mod article_by_msgid {
    use super::*;

    #[divan::bench]
    fn old(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(NntpCommand::classify(black_box(
                "ARTICLE <msg123@example.com>",
            )))
        });
    }

    #[divan::bench]
    fn new(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(parse_command(black_box("ARTICLE <msg123@example.com>")))
        });
    }
}

mod article_by_number {
    use super::*;

    #[divan::bench]
    fn old(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(NntpCommand::classify(black_box("ARTICLE 12345")))
        });
    }

    #[divan::bench]
    fn new(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(parse_command(black_box("ARTICLE 12345")))
        });
    }
}

mod article_current {
    use super::*;

    #[divan::bench]
    fn old(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(NntpCommand::classify(black_box("ARTICLE")))
        });
    }

    #[divan::bench]
    fn new(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(parse_command(black_box("ARTICLE")))
        });
    }
}

mod body_by_msgid {
    use super::*;

    #[divan::bench]
    fn old(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(NntpCommand::classify(black_box(
                "BODY <test@news.example.org>",
            )))
        });
    }

    #[divan::bench]
    fn new(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(parse_command(black_box("BODY <test@news.example.org>")))
        });
    }
}

mod head_by_msgid {
    use super::*;

    #[divan::bench]
    fn old(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(NntpCommand::classify(black_box(
                "HEAD <complex.id.12345@server.domain.com>",
            )))
        });
    }

    #[divan::bench]
    fn new(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(parse_command(black_box(
                "HEAD <complex.id.12345@server.domain.com>",
            )))
        });
    }
}

mod stat_by_msgid {
    use super::*;

    #[divan::bench]
    fn old(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(NntpCommand::classify(black_box(
                "STAT <msg@example.com>",
            )))
        });
    }

    #[divan::bench]
    fn new(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(parse_command(black_box("STAT <msg@example.com>")))
        });
    }
}

// =============================================================================
// Navigation Commands (10% of traffic)
// =============================================================================

mod group_command {
    use super::*;

    #[divan::bench]
    fn old(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(NntpCommand::classify(black_box("GROUP alt.binaries.test")))
        });
    }

    #[divan::bench]
    fn new(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(parse_command(black_box("GROUP alt.binaries.test")))
        });
    }
}

mod next_command {
    use super::*;

    #[divan::bench]
    fn old(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(NntpCommand::classify(black_box("NEXT")))
        });
    }

    #[divan::bench]
    fn new(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(parse_command(black_box("NEXT")))
        });
    }
}

mod last_command {
    use super::*;

    #[divan::bench]
    fn old(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(NntpCommand::classify(black_box("LAST")))
        });
    }

    #[divan::bench]
    fn new(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(parse_command(black_box("LAST")))
        });
    }
}

mod listgroup_command {
    use super::*;

    #[divan::bench]
    fn old(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(NntpCommand::classify(black_box("LISTGROUP alt.test")))
        });
    }

    #[divan::bench]
    fn new(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(parse_command(black_box("LISTGROUP alt.test")))
        });
    }
}

// =============================================================================
// Header Retrieval Commands (5% of traffic)
// =============================================================================

mod over_command {
    use super::*;

    #[divan::bench]
    fn old(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(NntpCommand::classify(black_box("OVER 100-200")))
        });
    }

    #[divan::bench]
    fn new(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(parse_command(black_box("OVER 100-200")))
        });
    }
}

mod xover_command {
    use super::*;

    #[divan::bench]
    fn old(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(NntpCommand::classify(black_box("XOVER 1000-2000")))
        });
    }

    #[divan::bench]
    fn new(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(parse_command(black_box("XOVER 1000-2000")))
        });
    }
}

mod hdr_command {
    use super::*;

    #[divan::bench]
    fn old(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(NntpCommand::classify(black_box("HDR Subject 100-200")))
        });
    }

    #[divan::bench]
    fn new(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(parse_command(black_box("HDR Subject 100-200")))
        });
    }
}

// =============================================================================
// Stateless Information Commands (5-10% of traffic)
// =============================================================================

mod list_command {
    use super::*;

    #[divan::bench]
    fn old(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(NntpCommand::classify(black_box("LIST ACTIVE")))
        });
    }

    #[divan::bench]
    fn new(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(parse_command(black_box("LIST ACTIVE")))
        });
    }
}

mod date_command {
    use super::*;

    #[divan::bench]
    fn old(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(NntpCommand::classify(black_box("DATE")))
        });
    }

    #[divan::bench]
    fn new(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(parse_command(black_box("DATE")))
        });
    }
}

mod capabilities_command {
    use super::*;

    #[divan::bench]
    fn old(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(NntpCommand::classify(black_box("CAPABILITIES")))
        });
    }

    #[divan::bench]
    fn new(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(parse_command(black_box("CAPABILITIES")))
        });
    }
}

mod help_command {
    use super::*;

    #[divan::bench]
    fn old(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(NntpCommand::classify(black_box("HELP")))
        });
    }

    #[divan::bench]
    fn new(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(parse_command(black_box("HELP")))
        });
    }
}

mod quit_command {
    use super::*;

    #[divan::bench]
    fn old(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(NntpCommand::classify(black_box("QUIT")))
        });
    }

    #[divan::bench]
    fn new(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(parse_command(black_box("QUIT")))
        });
    }
}

// =============================================================================
// Authentication Commands (once per connection)
// =============================================================================

mod authinfo_user {
    use super::*;

    #[divan::bench]
    fn old(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(NntpCommand::classify(black_box("AUTHINFO USER testuser")))
        });
    }

    #[divan::bench]
    fn new(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(parse_command(black_box("AUTHINFO USER testuser")))
        });
    }
}

mod authinfo_pass {
    use super::*;

    #[divan::bench]
    fn old(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(NntpCommand::classify(black_box("AUTHINFO PASS testpass")))
        });
    }

    #[divan::bench]
    fn new(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(parse_command(black_box("AUTHINFO PASS testpass")))
        });
    }
}

// =============================================================================
// Mode Commands
// =============================================================================

mod mode_reader {
    use super::*;

    #[divan::bench]
    fn old(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(NntpCommand::classify(black_box("MODE READER")))
        });
    }

    #[divan::bench]
    fn new(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(parse_command(black_box("MODE READER")))
        });
    }
}

// =============================================================================
// Posting Commands (rare)
// =============================================================================

mod post_command {
    use super::*;

    #[divan::bench]
    fn old(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(NntpCommand::classify(black_box("POST")))
        });
    }

    #[divan::bench]
    fn new(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(parse_command(black_box("POST")))
        });
    }
}

mod ihave_command {
    use super::*;

    #[divan::bench]
    fn old(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(NntpCommand::classify(black_box(
                "IHAVE <unique@msgid.com>",
            )))
        });
    }

    #[divan::bench]
    fn new(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(parse_command(black_box("IHAVE <unique@msgid.com>")))
        });
    }
}

// =============================================================================
// Complex Commands
// =============================================================================

mod complex_msgid {
    use super::*;

    #[divan::bench]
    fn old(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(NntpCommand::classify(black_box(
                "ARTICLE <very.long.complex.message.id.with.multiple.segments.12345678@news.server.example.com>",
            )))
        });
    }

    #[divan::bench]
    fn new(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(parse_command(black_box(
                "ARTICLE <very.long.complex.message.id.with.multiple.segments.12345678@news.server.example.com>",
            )))
        });
    }
}

mod long_groupname {
    use super::*;

    #[divan::bench]
    fn old(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(NntpCommand::classify(black_box(
                "GROUP alt.binaries.multimedia.anime.highspeed.repost",
            )))
        });
    }

    #[divan::bench]
    fn new(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(parse_command(black_box(
                "GROUP alt.binaries.multimedia.anime.highspeed.repost",
            )))
        });
    }
}

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
    #[divan::bench]
    fn old(bencher: Bencher) {
        let commands = [
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

        bencher
            .counter(divan::counter::ItemsCount::new(commands.len()))
            .bench_local(|| {
                for cmd in &commands {
                    black_box(NntpCommand::classify(black_box(cmd)));
                }
            });
    }

    #[divan::bench]
    fn new(bencher: Bencher) {
        let commands = [
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

        bencher
            .counter(divan::counter::ItemsCount::new(commands.len()))
            .bench_local(|| {
                for cmd in &commands {
                    let _ = black_box(parse_command(black_box(cmd)));
                }
            });
    }
}
