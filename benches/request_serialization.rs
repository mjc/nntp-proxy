//! Benchmarks for serializing typed request contexts without building command strings.
//!
//! These model the backend write hot path by copying the verb/args/CRLF slices into
//! a fixed sink. The benchmark intentionally avoids `Vec` so regressions show up as
//! extra instruction work in request construction or slice emission, not allocator noise.
//!
//! Run with: cargo bench --bench request_serialization

use divan::{Bencher, black_box};
use nntp_proxy::protocol::{RequestContext, StatusCode};

fn main() {
    divan::main();
}

struct FixedSink {
    data: [u8; 1024],
    len: usize,
}

impl Default for FixedSink {
    fn default() -> Self {
        Self {
            data: [0; 1024],
            len: 0,
        }
    }
}

impl FixedSink {
    #[inline]
    fn clear(&mut self) {
        self.len = 0;
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let end = self.len + bytes.len();
        self.data[self.len..end].copy_from_slice(bytes);
        self.len = end;
    }

    #[inline]
    fn len(&self) -> usize {
        self.len
    }
}

#[inline]
fn write_request_slices(sink: &mut FixedSink, request: &RequestContext) -> usize {
    sink.clear();
    sink.write(request.verb());
    if !request.args().is_empty() {
        sink.write(b" ");
        sink.write(request.args());
    }
    sink.write(b"\r\n");
    sink.len()
}

mod single_request {
    use super::{Bencher, FixedSink, RequestContext, black_box, write_request_slices};

    macro_rules! bench_request {
        ($name:ident, $line:literal) => {
            #[divan::bench(sample_count = 1000, sample_size = 1000)]
            fn $name(bencher: Bencher) {
                let request = RequestContext::from_request_line($line);
                bencher.bench(|| {
                    let mut sink = FixedSink::default();
                    black_box(write_request_slices(
                        black_box(&mut sink),
                        black_box(&request),
                    ))
                });
            }
        };
    }

    bench_request!(date_no_args, "DATE\r\n");
    bench_request!(article_message_id, "ARTICLE <bench@example.com>\r\n");
    bench_request!(
        group_long_args,
        "GROUP alt.binaries.multimedia.highspeed.repost\r\n"
    );
    bench_request!(
        very_long_message_id,
        "ARTICLE <very.long.message.id.with.segments.1234567890@news.example.org>\r\n"
    );
}

mod mixed_batch {
    use super::{Bencher, FixedSink, RequestContext, black_box, write_request_slices};

    const COMMANDS: &[&str] = &[
        "ARTICLE <a@example.com>\r\n",
        "BODY <b@example.com>\r\n",
        "HEAD <c@example.com>\r\n",
        "STAT <d@example.com>\r\n",
        "DATE\r\n",
        "CAPABILITIES\r\n",
        "GROUP alt.test\r\n",
        "LIST ACTIVE\r\n",
    ];

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn realistic_request_stream(bencher: Bencher) {
        let requests = COMMANDS
            .iter()
            .map(|line| RequestContext::from_request_line(line))
            .collect::<Vec<_>>();

        bencher
            .counter(divan::counter::ItemsCount::new(requests.len()))
            .bench(|| {
                let mut sink = FixedSink::default();
                requests
                    .iter()
                    .map(|request| write_request_slices(black_box(&mut sink), black_box(request)))
                    .sum::<usize>()
            });
    }
}

mod response_shape {
    use super::{Bencher, RequestContext, StatusCode, black_box};

    const CASES: &[(&str, u16)] = &[
        ("GROUP alt.test\r\n", 211),
        ("LISTGROUP alt.test\r\n", 211),
        ("ARTICLE <a@example.com>\r\n", 220),
        ("HEAD <a@example.com>\r\n", 221),
        ("BODY <a@example.com>\r\n", 222),
        ("ARTICLE <missing@example.com>\r\n", 430),
        ("HELP\r\n", 100),
        ("CAPABILITIES\r\n", 101),
    ];

    #[divan::bench(sample_count = 1000, sample_size = 1000)]
    fn request_aware_shape(bencher: Bencher) {
        let cases = CASES
            .iter()
            .map(|(line, status)| {
                (
                    RequestContext::from_request_line(line),
                    StatusCode::new(*status),
                )
            })
            .collect::<Vec<_>>();

        bencher
            .counter(divan::counter::ItemsCount::new(cases.len()))
            .bench(|| {
                cases.iter().fold(0usize, |count, (request, status)| {
                    black_box(request.response_shape(black_box(*status)));
                    count + 1
                })
            });
    }
}
