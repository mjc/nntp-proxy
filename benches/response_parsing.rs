//! Benchmarks for NNTP response parsing optimization

use divan::{Bencher, black_box};

fn main() {
    divan::main();
}

mod status_code_parsing {
    use super::*;
    use nntp_proxy::protocol::StatusCode;

    const RESPONSES: &[&[u8]] = &[
        b"200 Ready\r\n",
        b"220 0 12345 <msgid@example.com>\r\n",
        b"381 Password required\r\n",
    ];

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn optimized(bencher: Bencher) {
        bencher.bench(|| {
            for response in RESPONSES {
                black_box(StatusCode::parse(black_box(*response)));
            }
        });
    }
}

mod terminator_finding {
    use super::*;

    #[inline]
    fn find_terminator_old(data: &[u8]) -> Option<usize> {
        let n = data.len();
        if n < 5 {
            return None;
        }

        for i in 0..=(n - 5) {
            if &data[i..i + 5] == b"\r\n.\r\n" {
                return Some(i + 5);
            }
        }

        None
    }

    #[inline]
    fn find_terminator_new(data: &[u8]) -> Option<usize> {
        let n = data.len();
        if n < 5 {
            return None;
        }

        let mut pos = 0;
        while let Some(r_pos) = memchr::memchr(b'\r', &data[pos..]) {
            let abs_pos = pos + r_pos;

            if abs_pos + 5 > n {
                return None;
            }

            if &data[abs_pos..abs_pos + 5] == b"\r\n.\r\n" {
                return Some(abs_pos + 5);
            }

            pos = abs_pos + 1;
        }

        None
    }

    const SMALL_RESPONSE: &[u8] =
        b"220 0 12345 <msgid@example.com>\r\nArticle content here\r\nMore lines\r\n.\r\n";

    const MEDIUM_RESPONSE: &[u8] = b"220 0 12345 <msgid@example.com>\r\n\
        Header: value\r\n\
        Another-Header: another value\r\n\
        \r\n\
        Article body line 1\r\n\
        Article body line 2\r\n\
        Article body line 3\r\n\
        Article body line 4\r\n\
        Article body line 5\r\n\
        .\r\n";

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn old_small(bencher: Bencher) {
        bencher.bench(|| black_box(find_terminator_old(black_box(SMALL_RESPONSE))));
    }

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn new_small(bencher: Bencher) {
        bencher.bench(|| black_box(find_terminator_new(black_box(SMALL_RESPONSE))));
    }

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn old_medium(bencher: Bencher) {
        bencher.bench(|| black_box(find_terminator_old(black_box(MEDIUM_RESPONSE))));
    }

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn new_medium(bencher: Bencher) {
        bencher.bench(|| black_box(find_terminator_new(black_box(MEDIUM_RESPONSE))));
    }
}
