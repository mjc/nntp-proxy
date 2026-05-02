//! Benchmarks for turning client read buffers into typed request contexts.
//!
//! The `typed_contexts` benches model the current request-boundary behavior:
//! each complete line becomes a `RequestContext`. The baseline modules model
//! older/raw approaches that either hand around command byte slices or allocate
//! command strings before later classification.
//!
//! Run with: cargo bench --bench request_batch_parse

use divan::{Bencher, black_box};
use nntp_proxy::protocol::RequestContext;

fn main() {
    divan::main();
}

#[inline]
fn parse_typed_contexts(buffer: &[u8]) -> usize {
    let mut start = 0;
    let mut count = 0;

    while let Some(relative_lf) = buffer[start..].iter().position(|byte| *byte == b'\n') {
        let end = start + relative_lf + 1;
        if end >= 2
            && buffer[end - 2] == b'\r'
            && let Ok(line) = std::str::from_utf8(&buffer[start..end])
        {
            black_box(RequestContext::from_request_bytes(line.as_bytes()));
            count += 1;
        }
        start = end;
    }

    count
}

#[inline]
fn parse_raw_command_slices(buffer: &[u8]) -> usize {
    let mut start = 0;
    let mut count = 0;

    while let Some(relative_lf) = buffer[start..].iter().position(|byte| *byte == b'\n') {
        let end = start + relative_lf + 1;
        if end >= 2 && buffer[end - 2] == b'\r' {
            black_box(&buffer[start..end]);
            count += 1;
        }
        start = end;
    }

    count
}

#[inline]
fn parse_allocating_command_strings(buffer: &[u8]) -> usize {
    let mut start = 0;
    let mut commands = Vec::new();

    while let Some(relative_lf) = buffer[start..].iter().position(|byte| *byte == b'\n') {
        let end = start + relative_lf + 1;
        if end >= 2
            && buffer[end - 2] == b'\r'
            && let Ok(line) = std::str::from_utf8(&buffer[start..end])
        {
            commands.push(line.to_string());
        }
        start = end;
    }

    black_box(commands).len()
}

fn repeated_article_batch(count: usize) -> Vec<u8> {
    (0..count)
        .map(|i| format!("ARTICLE <bench-{i}@example.com>\r\n"))
        .collect::<String>()
        .into_bytes()
}

mod typed_contexts {
    use super::{Bencher, black_box, parse_typed_contexts, repeated_article_batch};

    macro_rules! bench_batch {
        ($name:ident, $count:expr) => {
            #[divan::bench(sample_count = 1000, sample_size = 100)]
            fn $name(bencher: Bencher) {
                let buffer = repeated_article_batch($count);
                bencher
                    .counter(divan::counter::ItemsCount::new($count as usize))
                    .bench(|| black_box(parse_typed_contexts(black_box(&buffer))));
            }
        };
    }

    bench_batch!(one_article, 1);
    bench_batch!(four_articles, 4);
    bench_batch!(thirty_two_articles, 32);
}

mod raw_slice_baseline {
    use super::{Bencher, black_box, parse_raw_command_slices, repeated_article_batch};

    macro_rules! bench_batch {
        ($name:ident, $count:expr) => {
            #[divan::bench(sample_count = 1000, sample_size = 100)]
            fn $name(bencher: Bencher) {
                let buffer = repeated_article_batch($count);
                bencher
                    .counter(divan::counter::ItemsCount::new($count as usize))
                    .bench(|| black_box(parse_raw_command_slices(black_box(&buffer))));
            }
        };
    }

    bench_batch!(one_article, 1);
    bench_batch!(four_articles, 4);
    bench_batch!(thirty_two_articles, 32);
}

mod allocating_string_baseline {
    use super::{Bencher, black_box, parse_allocating_command_strings, repeated_article_batch};

    macro_rules! bench_batch {
        ($name:ident, $count:expr) => {
            #[divan::bench(sample_count = 500, sample_size = 100)]
            fn $name(bencher: Bencher) {
                let buffer = repeated_article_batch($count);
                bencher
                    .counter(divan::counter::ItemsCount::new($count as usize))
                    .bench(|| black_box(parse_allocating_command_strings(black_box(&buffer))));
            }
        };
    }

    bench_batch!(one_article, 1);
    bench_batch!(four_articles, 4);
    bench_batch!(thirty_two_articles, 32);
}

mod mixed_buffers {
    use super::{Bencher, black_box, parse_typed_contexts};

    const MIXED: &[u8] = b"ARTICLE <a@example.com>\r\n\
BODY <b@example.com>\r\n\
HEAD <c@example.com>\r\n\
STAT <d@example.com>\r\n\
GROUP alt.test\r\n\
LIST ACTIVE\r\n\
DATE\r\n\
CAPABILITIES\r\n";

    const WITH_TRAILING_PARTIAL: &[u8] = b"ARTICLE <a@example.com>\r\n\
BODY <b@example.com>\r\n\
HEAD <partial@example.com>";

    const WITH_INVALID_UTF8: &[u8] = b"ARTICLE <a@example.com>\r\n\
not utf8 \xff\r\n\
STAT <d@example.com>\r\n";

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn mixed_complete(bencher: Bencher) {
        bencher.bench(|| black_box(parse_typed_contexts(black_box(MIXED))));
    }

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn mixed_with_trailing_partial(bencher: Bencher) {
        bencher.bench(|| black_box(parse_typed_contexts(black_box(WITH_TRAILING_PARTIAL))));
    }

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn mixed_with_invalid_utf8_line(bencher: Bencher) {
        bencher.bench(|| black_box(parse_typed_contexts(black_box(WITH_INVALID_UTF8))));
    }
}
