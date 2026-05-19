//! Benchmarks for NNTP response parsing optimization

use divan::{Bencher, black_box};

fn main() {
    divan::main();
}

mod status_code_parsing {
    use super::{Bencher, black_box};
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
