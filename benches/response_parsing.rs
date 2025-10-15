//! Benchmarks for NNTP response parsing optimization
//!
//! Compares optimized direct byte-to-digit conversion vs UTF-8-based parsing.
//!
//! Run with: cargo bench --bench response_parsing

use divan::{black_box, Bencher};

fn main() {
    divan::main();
}

/// Old UTF-8 based status code parsing (baseline)
#[inline]
fn parse_status_code_old(data: &[u8]) -> Option<u16> {
    if data.len() < 3 {
        return None;
    }
    let code_str = std::str::from_utf8(&data[0..3]).ok()?;
    code_str.parse().ok()
}

/// New optimized status code parsing (direct byte-to-digit)
#[inline]
fn parse_status_code_new(data: &[u8]) -> Option<u16> {
    if data.len() < 3 {
        return None;
    }

    // Direct ASCII digit conversion without UTF-8 overhead
    let d0 = data[0].wrapping_sub(b'0');
    let d1 = data[1].wrapping_sub(b'0');
    let d2 = data[2].wrapping_sub(b'0');

    // Validate all three are digits (0-9)
    if d0 > 9 || d1 > 9 || d2 > 9 {
        return None;
    }

    // Combine into u16: d0*100 + d1*10 + d2
    Some((d0 as u16) * 100 + (d1 as u16) * 10 + (d2 as u16))
}

mod status_code_parsing {
    use super::*;

    // Common response codes for realistic benchmarking
    const RESPONSES: &[&[u8]] = &[
        b"200 Ready\r\n",
        b"220 0 12345 <msgid@example.com>\r\n",
        b"222 0 <msgid@example.com>\r\n",
        b"381 Password required\r\n",
        b"480 Authentication required\r\n",
        b"500 Command not recognized\r\n",
        b"111 20231014123456\r\n",
        b"215 List of newsgroups follows\r\n",
        b"281 Authentication accepted\r\n",
        b"205 Goodbye\r\n",
    ];

    #[divan::bench(name = "old_utf8", sample_count = 1000, sample_size = 100)]
    fn old_utf8(bencher: Bencher) {
        bencher.bench_local(|| {
            for response in RESPONSES {
                black_box(parse_status_code_old(black_box(*response)));
            }
        });
    }

    #[divan::bench(name = "new_optimized", sample_count = 1000, sample_size = 100)]
    fn new_optimized(bencher: Bencher) {
        bencher.bench_local(|| {
            for response in RESPONSES {
                black_box(parse_status_code_new(black_box(*response)));
            }
        });
    }
}

mod single_response {
    use super::*;

    #[divan::bench(name = "old_200", sample_count = 1000, sample_size = 100)]
    fn old_200(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(parse_status_code_old(black_box(b"200 Ready\r\n")))
        });
    }

    #[divan::bench(name = "new_200", sample_count = 1000, sample_size = 100)]
    fn new_200(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(parse_status_code_new(black_box(b"200 Ready\r\n")))
        });
    }

    #[divan::bench(name = "old_220_article", sample_count = 1000, sample_size = 100)]
    fn old_220_article(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(parse_status_code_old(black_box(b"220 0 12345 <msgid@example.com>\r\n")))
        });
    }

    #[divan::bench(name = "new_220_article", sample_count = 1000, sample_size = 100)]
    fn new_220_article(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(parse_status_code_new(black_box(b"220 0 12345 <msgid@example.com>\r\n")))
        });
    }

    #[divan::bench(name = "old_381_auth", sample_count = 1000, sample_size = 100)]
    fn old_381_auth(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(parse_status_code_old(black_box(b"381 Password required\r\n")))
        });
    }

    #[divan::bench(name = "new_381_auth", sample_count = 1000, sample_size = 100)]
    fn new_381_auth(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(parse_status_code_new(black_box(b"381 Password required\r\n")))
        });
    }
}

mod terminator_finding {
    use super::*;

    /// Old naive byte-by-byte search
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

    /// New memchr-optimized search
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

    // Small response (most common case)
    const SMALL_RESPONSE: &[u8] = b"220 0 12345 <msgid@example.com>\r\nArticle content here\r\nMore lines\r\n.\r\n";

    // Medium response
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

    #[divan::bench(name = "old_small", sample_count = 1000, sample_size = 100)]
    fn old_small(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(find_terminator_old(black_box(SMALL_RESPONSE)))
        });
    }

    #[divan::bench(name = "new_small", sample_count = 1000, sample_size = 100)]
    fn new_small(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(find_terminator_new(black_box(SMALL_RESPONSE)))
        });
    }

    #[divan::bench(name = "old_medium", sample_count = 1000, sample_size = 100)]
    fn old_medium(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(find_terminator_old(black_box(MEDIUM_RESPONSE)))
        });
    }

    #[divan::bench(name = "new_medium", sample_count = 1000, sample_size = 100)]
    fn new_medium(bencher: Bencher) {
        bencher.bench_local(|| {
            black_box(find_terminator_new(black_box(MEDIUM_RESPONSE)))
        });
    }
}
