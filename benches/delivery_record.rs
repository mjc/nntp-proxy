//! Cost of recording one delivered article into a backend's delivery
//! estimate, alone and with several threads completing at once (each backend
//! sees a few hundred completions a second in production).
//!
//! Run with: cargo bench --bench `delivery_record`

use divan::{Bencher, black_box};
use nntp_proxy::pool::delivery::DeliveryStats;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() {
    divan::main();
}

#[divan::bench(sample_count = 1000, sample_size = 100, threads = [1, 4, 8])]
fn record_article(bencher: Bencher) {
    let stats = Arc::new(DeliveryStats::default());
    let now = Instant::now();
    bencher.bench(|| {
        stats.record_article(
            black_box(740_000),
            black_box(Duration::from_millis(480)),
            black_box(Duration::ZERO),
            black_box(now),
        );
    });
}

#[divan::bench(sample_count = 1000, sample_size = 100)]
fn record_ttfb(bencher: Bencher) {
    let stats = DeliveryStats::default();
    bencher.bench(|| stats.record_ttfb(black_box(Duration::from_millis(370))));
}
